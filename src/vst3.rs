//! Runtime hosting for already-installed VST3 plug-ins.
//!
//! The project parser decodes VST identity metadata from recognized FLP envelopes,
//! while preserving their complete raw bytes. This runtime does not translate
//! Image-Line's FLP state envelope into the state stream expected by every VST3.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::FlpDocument;
use crate::audio::StreamingAudioWriter;
use crate::sample_render::{channel_gain_pan, schedule_playlist_pattern_notes};
use vst3_host::audio::AudioBuffers;
use vst3_host::midi::{MidiChannel, MidiEvent};
use vst3_host::{Plugin, PluginInfo, PluginWindow, Vst3Host};

use crate::PatternNote;

static NEXT_RENDER_FILE_ID: AtomicU64 = AtomicU64::new(1);
const MAX_STEREO_BUFFER_BYTES: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScheduledMidiEvent {
    frame: u64,
    priority: u8,
    event: MidiEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3RenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub output_channels: usize,
    pub notes_rendered: usize,
}

struct PreparedPatternRender {
    events: Vec<ScheduledMidiEvent>,
    note_count: usize,
    sample_rate: f64,
    sample_rate_u32: u32,
    output_channels: usize,
    block_size: usize,
    total_frames: u64,
}

/// A prepared VST3 pattern render that can stream processed blocks to a live audio device.
pub struct Vst3PatternStream {
    plugin: Arc<Mutex<Plugin>>,
    render: PreparedPatternRender,
}

/// A running pattern stream with completion and worker-lifecycle handles.
pub struct Vst3PatternStreamHandle {
    receiver: Receiver<Result<Vst3RenderSummary, String>>,
    worker: thread::JoinHandle<()>,
}

impl Vst3PatternStreamHandle {
    /// Poll for the VST3 block-render worker's final result without waiting.
    pub fn try_recv(&self) -> Result<Result<Vst3RenderSummary, String>, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Whether the worker has exited and can be joined without waiting for plug-in processing.
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    /// Join the worker after playback has been cancelled or completed.
    pub fn join(self) -> thread::Result<()> {
        self.worker.join()
    }
}

impl Vst3PatternStream {
    /// Start rendering on a worker and return a receiver for its completion result.
    ///
    /// The existing loaded VST3 instance is used, so its current editor state remains the
    /// playback state. Processing locks the instance only for each block; ring-buffer backpressure
    /// happens after releasing the plug-in lock.
    pub fn start(
        self,
        writer: StreamingAudioWriter,
        output_sample_rate: u32,
    ) -> Result<Vst3PatternStreamHandle, String> {
        if output_sample_rate == 0 {
            return Err("audio output sample rate must be positive".to_owned());
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("vst3-pattern-stream".to_owned())
            .spawn(move || {
                let result =
                    stream_pattern_render(self.plugin, self.render, &writer, output_sample_rate);
                writer.finish();
                let _ = sender.send(result);
            })
            .map_err(|error| format!("could not start VST3 render worker: {error}"))?;
        Ok(Vst3PatternStreamHandle { receiver, worker })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3PatternRenderOptions {
    pub channel_id: u16,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub tail_seconds: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3PlaylistRenderSummary {
    pub plugin_channels_rendered: usize,
    pub notes_scheduled: usize,
    pub unloaded_plugin_channels: Vec<u16>,
    pub frames: u64,
    pub sample_rate: u32,
}

#[derive(Clone, Copy)]
struct PlaylistMidiNote {
    start_tick: u64,
    stop_tick: u64,
    key: u16,
    velocity: u8,
    midi_channel: u8,
}

struct PlaylistPluginStream {
    channel_id: u16,
    plugin: Arc<Mutex<Plugin>>,
    render: PreparedPatternRender,
    resampler: StereoStreamResampler,
    pending_output: VecDeque<[f32; 2]>,
    source_frame: u64,
    next_event: usize,
    resampler_finished: bool,
    gain: f32,
    left_pan_gain: f32,
    right_pan_gain: f32,
}

/// Prepared VST3 instrument channels for blockwise Playlist Song transport.
/// The instances are the same objects used by the open editors.
pub struct Vst3PlaylistStreamProcessor {
    streams: Vec<PlaylistPluginStream>,
    output_sample_rate: u32,
    output_frames: u64,
    notes_scheduled: usize,
    unloaded_plugin_channels: Vec<u16>,
    started_count: usize,
}

impl Vst3PlaylistStreamProcessor {
    pub fn summary(&self) -> Vst3PlaylistRenderSummary {
        Vst3PlaylistRenderSummary {
            plugin_channels_rendered: self.streams.len(),
            notes_scheduled: self.notes_scheduled,
            unloaded_plugin_channels: self.unloaded_plugin_channels.clone(),
            frames: self.output_frames,
            sample_rate: self.output_sample_rate,
        }
    }

    pub fn extend_to_output_frames(&mut self, frames: u64) -> Result<(), String> {
        self.output_frames = self.output_frames.max(frames);
        for stream in &mut self.streams {
            let source_frames = (self.output_frames as f64 * stream.render.sample_rate
                / f64::from(self.output_sample_rate))
            .ceil();
            if !source_frames.is_finite() || source_frames > u64::MAX as f64 {
                return Err(
                    "VST3 Playlist stream length is outside the renderable range".to_owned(),
                );
            }
            stream.render.total_frames = source_frames as u64;
        }
        Ok(())
    }

    pub fn start_processing(&mut self) -> Result<(), String> {
        for index in 0..self.streams.len() {
            let channel_id = self.streams[index].channel_id;
            let result = self.streams[index]
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())
                .and_then(|mut plugin| {
                    plugin.start_processing().map_err(|error| error.to_string())
                });
            if let Err(error) = result {
                let _ = self.stop_processing();
                return Err(format!(
                    "could not start VST3 channel {} processing: {error}",
                    channel_id
                ));
            }
            self.started_count += 1;
        }
        Ok(())
    }

    pub fn mix_next_block(&mut self, output: &mut [f32]) -> Result<(), String> {
        if !output.len().is_multiple_of(2) {
            return Err("VST3 Playlist mix buffer must contain stereo frames".to_owned());
        }
        let frame_count = output.len() / 2;
        for stream in &mut self.streams {
            let mut plugin_output = Vec::new();
            plugin_output
                .try_reserve_exact(output.len())
                .map_err(|error| format!("could not allocate VST3 Playlist block: {error}"))?;
            stream.render_output_frames(frame_count, &mut plugin_output)?;
            for (frame_index, frame) in plugin_output.as_chunks::<2>().0.iter().enumerate() {
                let left = frame[0] * stream.gain;
                let right = frame[1] * stream.gain;
                output[frame_index * 2] += left * stream.left_pan_gain;
                output[frame_index * 2 + 1] += right * stream.right_pan_gain;
            }
        }
        Ok(())
    }

    pub fn stop_processing(&mut self) -> Result<(), String> {
        let mut first_error = None;
        while self.started_count > 0 {
            self.started_count -= 1;
            let stream = &self.streams[self.started_count];
            let result = stream
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())
                .and_then(|mut plugin| plugin.stop_processing().map_err(|error| error.to_string()));
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(format!(
                    "could not stop VST3 channel {} processing: {error}",
                    stream.channel_id
                ));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl PlaylistPluginStream {
    fn render_output_frames(
        &mut self,
        frame_count: usize,
        output: &mut Vec<f32>,
    ) -> Result<(), String> {
        output.clear();
        let output_samples = frame_count
            .checked_mul(2)
            .ok_or_else(|| "VST3 Playlist output block size overflow".to_owned())?;
        output
            .try_reserve(output_samples)
            .map_err(|error| format!("could not allocate VST3 Playlist output: {error}"))?;
        while self.pending_output.len() < frame_count && !self.resampler_finished {
            if self.source_frame < self.render.total_frames {
                let source_count = (self.render.total_frames - self.source_frame)
                    .min(self.render.block_size as u64) as usize;
                let block_end = self.source_frame + source_count as u64;
                let mut buffers = AudioBuffers::new(
                    0,
                    self.render.output_channels,
                    source_count,
                    self.render.sample_rate,
                );
                let output_block = {
                    let mut plugin = self
                        .plugin
                        .lock()
                        .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                    while let Some(event) = self.render.events.get(self.next_event)
                        && event.frame < block_end
                    {
                        let offset = event.frame.saturating_sub(self.source_frame) as i32;
                        plugin
                            .send_midi_event_at(event.event, offset)
                            .map_err(|error| error.to_string())?;
                        self.next_event += 1;
                    }
                    plugin
                        .process_audio(&mut buffers)
                        .map_err(|error| error.to_string())?;
                    if buffers.outputs.len() != self.render.output_channels
                        || buffers
                            .outputs
                            .iter()
                            .any(|channel| channel.len() < source_count)
                    {
                        return Err(format!(
                            "VST3 channel {} returned audio buffers with an unexpected shape",
                            self.channel_id
                        ));
                    }
                    self.resampler.push_outputs(
                        &buffers.outputs,
                        source_count,
                        self.render.output_channels,
                    )?
                };
                self.push_interleaved(&output_block)?;
                self.source_frame = block_end;
            } else {
                let output_block = self.resampler.finish()?;
                self.push_interleaved(&output_block)?;
                self.resampler_finished = true;
            }
        }
        for _ in 0..frame_count {
            let frame = self.pending_output.pop_front().unwrap_or([0.0, 0.0]);
            output.extend_from_slice(&frame);
        }
        Ok(())
    }

    fn push_interleaved(&mut self, samples: &[f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(2) {
            return Err("VST3 Playlist resampler returned a partial stereo frame".to_owned());
        }
        self.pending_output.extend(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|frame| [finite_sample(frame[0]), finite_sample(frame[1])]),
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedPluginInfo {
    pub id: u64,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub category: String,
    pub uid: String,
    pub path: PathBuf,
    pub has_editor: bool,
}

struct HostedPlugin {
    info: HostedPluginInfo,
    plugin: Arc<Mutex<Plugin>>,
    editor: Option<PluginWindow>,
}

/// Owns active VST3 instances and their native editor windows.
///
/// Plug-ins are loaded on demand. The host does not scan or execute every installed
/// plug-in during startup.
pub struct Vst3HostRuntime {
    host: Vst3Host,
    loaded: Vec<HostedPlugin>,
    next_id: u64,
}

impl Vst3HostRuntime {
    pub fn new(sample_rate: f64, block_size: usize) -> Result<Self, String> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err("sample rate must be finite and positive".to_owned());
        }
        if block_size == 0 {
            return Err("block size must be greater than zero".to_owned());
        }
        let host = Vst3Host::builder()
            .sample_rate(sample_rate)
            .block_size(block_size)
            .with_process_isolation(false)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            host,
            loaded: Vec::new(),
            next_id: 1,
        })
    }

    /// Load one audio class from an installed VST3 bundle.
    ///
    /// `class_id` may be omitted for single-class bundles. Multi-class bundles should
    /// pass the 32-character class UID stored in the project's wrapper metadata.
    pub fn load(
        &mut self,
        path: impl AsRef<Path>,
        class_id: Option<&str>,
    ) -> Result<HostedPluginInfo, String> {
        let plugin = match class_id {
            Some(class_id) => self.host.load_plugin_class(path, class_id),
            None => self.host.load_plugin(path),
        }
        .map_err(|error| error.to_string())?;

        let plugin_info = plugin.info().clone();
        let info = hosted_info(self.next_id, &plugin_info, plugin.has_editor());
        self.next_id = self.next_id.saturating_add(1);
        self.loaded.push(HostedPlugin {
            info: info.clone(),
            plugin: Arc::new(Mutex::new(plugin)),
            editor: None,
        });
        Ok(info)
    }

    /// Pass a state blob to the plug-in's VST3 state loader.
    ///
    /// The caller must provide bytes compatible with that plug-in. This method does
    /// not convert an Image-Line FLP `0xD5` envelope into a VST3 component state.
    pub fn restore_state(&mut self, id: u64, state: &[u8]) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .load_state(state)
            .map_err(|error| error.to_string())
    }

    /// Return the opaque VST3 host snapshot for an instance.
    ///
    /// This is not written directly into an FLP `0xD5` event; Image-Line's wrapper
    /// encoding must be applied first.
    pub fn save_state(&self, id: u64) -> Result<Vec<u8>, String> {
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .save_state()
            .map_err(|error| error.to_string())
    }

    pub fn open_editor(&mut self, id: u64) -> Result<(), String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if !loaded.info.has_editor {
            return Err(format!(
                "{} does not provide a custom editor",
                loaded.info.name
            ));
        }
        if loaded.editor.is_none() {
            loaded.editor = Some(PluginWindow::new(loaded.plugin.clone()));
        }
        let editor = loaded.editor.as_mut().expect("editor was just created");
        // `PluginWindow` does not expose a raise/focus operation. Reopening an existing
        // native editor recreates its window and brings it above the main DAW window.
        editor.open().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn close_editor(&mut self, id: u64) -> Result<(), String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if let Some(mut editor) = loaded.editor.take() {
            editor.close();
        }
        Ok(())
    }

    pub fn set_parameter(&self, id: u64, parameter_id: u32, value: f64) -> Result<(), String> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("VST3 parameter values must be finite and normalized to 0..1".to_owned());
        }
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .set_parameter(parameter_id, value)
            .map_err(|error| error.to_string())
    }

    pub fn parameter_snapshot(
        &self,
        id: u64,
    ) -> Result<Vec<vst3_host::parameters::Parameter>, String> {
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .get_parameters()
            .map_err(|error| error.to_string())
    }

    /// Render notes from one FLP pattern channel through a loaded instrument to a float WAV.
    ///
    /// This schedules the decoded score notes with the project's PPQ and tempo. It does not
    /// restore the FLP plug-in wrapper state, apply Mixer routing/effects, or expand Playlist
    /// clips. `tail_seconds` is appended after the final note to capture instrument release.
    pub fn render_pattern_channel_to_wav(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        path: impl AsRef<Path>,
    ) -> Result<Vst3RenderSummary, String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render = prepare_pattern_render(&plugin, notes, options)?;
        let data_bytes = render
            .total_frames
            .checked_mul(render.output_channels as u64)
            .and_then(|frames| frames.checked_mul(4))
            .ok_or_else(|| "rendered WAV size overflow".to_owned())?;
        if data_bytes > u64::from(u32::MAX - 36) {
            return Err("render is too long for a standard RIFF/WAVE file".to_owned());
        }

        let output_path = path.as_ref();
        let mut temporary = TemporaryWaveFile::create(output_path)?;
        write_float_wave_header(
            temporary.file.as_mut().expect("temporary WAV is open"),
            render.total_frames as u32,
            render.sample_rate_u32,
            render.output_channels as u16,
        )?;

        let mut interleaved_bytes =
            Vec::with_capacity(render.block_size * render.output_channels * 4);
        process_pattern_render(&mut plugin, &render, |interleaved| {
            interleaved_bytes.clear();
            for sample in interleaved {
                interleaved_bytes.extend_from_slice(&sample.to_le_bytes());
            }
            temporary
                .file
                .as_mut()
                .expect("temporary WAV is open")
                .write_all(&interleaved_bytes)
                .map_err(|error| format!("could not write WAV audio data: {error}"))
        })?;

        temporary.commit(output_path)?;
        Ok(Vst3RenderSummary {
            frames: render.total_frames,
            sample_rate: render.sample_rate_u32,
            output_channels: render.output_channels,
            notes_rendered: render.note_count,
        })
    }

    /// Render one pattern channel through an installed VST3 into interleaved stereo samples.
    /// The result is suitable for device playback after resampling to the device rate.
    /// This is an offline render; it does not expand Playlist clips or apply Mixer processing.
    pub fn render_pattern_channel_to_stereo_buffer(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
    ) -> Result<(Vec<f32>, Vst3RenderSummary), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render = prepare_pattern_render(&plugin, notes, options)?;
        if !(1..=2).contains(&render.output_channels) {
            return Err(format!(
                "VST3 preview supports mono or stereo output buses; this instrument has {} channels",
                render.output_channels
            ));
        }
        let sample_count = usize::try_from(render.total_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(2))
            .ok_or_else(|| "preview buffer size overflow".to_owned())?;
        let buffer_bytes = sample_count
            .checked_mul(std::mem::size_of::<f32>())
            .filter(|bytes| *bytes <= MAX_STEREO_BUFFER_BYTES)
            .ok_or_else(|| {
                format!(
                    "preview exceeds the {} MiB in-memory render limit",
                    MAX_STEREO_BUFFER_BYTES / (1024 * 1024)
                )
            })?;
        let mut stereo = Vec::with_capacity(buffer_bytes / std::mem::size_of::<f32>());
        process_pattern_render(&mut plugin, &render, |interleaved| {
            append_vst_output_block_to_stereo(&mut stereo, interleaved, render.output_channels)
        })?;

        Ok((
            stereo,
            Vst3RenderSummary {
                frames: render.total_frames,
                sample_rate: render.sample_rate_u32,
                output_channels: render.output_channels,
                notes_rendered: render.note_count,
            },
        ))
    }

    /// Prepare a single pattern channel for blockwise streaming through its loaded VST3.
    pub fn prepare_pattern_channel_stream(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
    ) -> Result<Vst3PatternStream, String> {
        let plugin = self.plugin(id)?.clone();
        let render = {
            let plugin_guard = plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            prepare_pattern_render(&plugin_guard, notes, options)?
        };
        if !(1..=2).contains(&render.output_channels) {
            return Err(format!(
                "VST3 streaming supports mono or stereo output buses; this instrument has {} channels",
                render.output_channels
            ));
        }
        Ok(Vst3PatternStream { plugin, render })
    }

    /// Prepare all loaded VST3 instrument channels used by the selected Playlist arrangement.
    ///
    /// Pattern Clips are expanded into absolute project ticks, then each mapped channel is
    /// processed independently on the playback worker and resampled into the shared song mix.
    /// Unloaded instrument channels are reported so the caller can surface missing plug-ins.
    pub fn prepare_playlist_stream(
        &self,
        document: &FlpDocument,
        arrangement_id: u16,
        channel_instances: &BTreeMap<u16, u64>,
        output_sample_rate: u32,
        tail_seconds: f64,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        if !(8_000..=384_000).contains(&output_sample_rate) {
            return Err("Playlist audio rate must be between 8000 and 384000 Hz".to_owned());
        }
        if !tail_seconds.is_finite() || !(0.0..=60.0).contains(&tail_seconds) {
            return Err("VST3 Playlist tail must be between 0 and 60 seconds".to_owned());
        }
        let ppq = document.header().ppq();
        if ppq == 0 {
            return Err("project PPQ must be greater than zero".to_owned());
        }
        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err("project tempo must be finite and positive".to_owned());
        }
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
        let patterns = document.patterns().map_err(|error| error.to_string())?;
        let channels = document.channels();
        let channels_by_id: BTreeMap<_, _> = channels
            .iter()
            .map(|channel| (channel.id(), channel))
            .collect();
        let mut plugin_channels: BTreeSet<_> = document
            .channel_plugin_states()
            .into_iter()
            .map(|state| state.channel_id())
            .collect();
        plugin_channels.extend(
            channels
                .iter()
                .filter(|channel| {
                    channel.plugin_identifier().is_some() && channel.enabled() != Some(false)
                })
                .map(|channel| channel.id()),
        );
        let schedule = schedule_playlist_pattern_notes(&patterns, &arrangement, |channel_id| {
            plugin_channels.contains(&channel_id)
                && channels_by_id
                    .get(&channel_id)
                    .is_none_or(|channel| channel.enabled() != Some(false))
        })?;

        let mut notes_by_channel = BTreeMap::<u16, Vec<PlaylistMidiNote>>::new();
        for placed in &schedule.notes {
            let note = placed.note;
            let stop_tick = match placed.clipped_stop_tick {
                Some(stop_tick) => stop_tick,
                None => placed
                    .start_tick
                    .checked_add(1)
                    .ok_or_else(|| "zero-length VST3 note end overflow".to_owned())?,
            };
            notes_by_channel
                .entry(note.channel_id)
                .or_default()
                .push(PlaylistMidiNote {
                    start_tick: placed.start_tick,
                    stop_tick,
                    key: note.key,
                    velocity: note.velocity,
                    midi_channel: note.midi_channel,
                });
        }

        let ppq = u64::from(ppq);
        let max_tick = arrangement.clips.iter().fold(0u64, |end, clip| {
            end.max(u64::from(clip.position_ticks) + u64::from(clip.length_ticks))
        });
        let duration_seconds = max_tick as f64 * 60.0 / (ppq as f64 * tempo_bpm);
        let base_output_frames = duration_seconds * f64::from(output_sample_rate);
        let tail_output_frames = tail_seconds * f64::from(output_sample_rate);
        let total_output_frames = base_output_frames + tail_output_frames;
        if !total_output_frames.is_finite() || total_output_frames > u64::MAX as f64 {
            return Err("VST3 Playlist length is outside the renderable range".to_owned());
        }
        let mut output_frames = if notes_by_channel.is_empty() {
            0
        } else {
            (total_output_frames.ceil() as u64).max(1)
        };

        let mut streams = Vec::new();
        let mut unloaded_plugin_channels = BTreeSet::new();
        let mut notes_scheduled = 0usize;
        for (channel_id, notes) in notes_by_channel {
            let Some(instance_id) = channel_instances.get(&channel_id).copied() else {
                unloaded_plugin_channels.insert(channel_id);
                continue;
            };
            let Ok(plugin) = self.plugin(instance_id).cloned() else {
                unloaded_plugin_channels.insert(channel_id);
                continue;
            };
            let channel = channels_by_id.get(&channel_id);
            let (gain, pan) = channel_gain_pan(
                channel.and_then(|channel| channel.volume()),
                channel.and_then(|channel| channel.pan()),
            );
            let render = {
                let plugin_guard = plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                let source_frames = (output_frames as f64 * plugin_guard.sample_rate()
                    / f64::from(output_sample_rate))
                .ceil();
                if !source_frames.is_finite() || source_frames > u64::MAX as f64 {
                    return Err(format!(
                        "VST3 channel {channel_id} Playlist length is outside the renderable range"
                    ));
                }
                prepare_playlist_pattern_render(
                    &plugin_guard,
                    &notes,
                    u16::try_from(ppq).map_err(|_| "project PPQ is out of range".to_owned())?,
                    tempo_bpm,
                    source_frames as u64,
                )?
            };
            let (left_pan_gain, right_pan_gain) =
                playlist_plugin_pan_gains(pan, render.output_channels == 1);
            let resampler = StereoStreamResampler::new(render.sample_rate_u32, output_sample_rate)?;
            notes_scheduled += render.note_count;
            streams.push(PlaylistPluginStream {
                channel_id,
                plugin,
                render,
                resampler,
                pending_output: VecDeque::new(),
                source_frame: 0,
                next_event: 0,
                resampler_finished: false,
                gain,
                left_pan_gain,
                right_pan_gain,
            });
        }
        if streams.is_empty() {
            output_frames = 0;
        }
        Ok(Vst3PlaylistStreamProcessor {
            streams,
            output_sample_rate,
            output_frames,
            notes_scheduled,
            unloaded_plugin_channels: unloaded_plugin_channels.into_iter().collect(),
            started_count: 0,
        })
    }

    /// Service native editor close/resize requests and the VST3 UI run loop where needed.
    pub fn service_editors(&mut self) -> Result<(), String> {
        for loaded in &mut self.loaded {
            if let Some(editor) = loaded.editor.as_ref() {
                editor
                    .service_platform_events()
                    .map_err(|error| error.to_string())?;
            }
            loaded
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?
                .service_run_loop();
        }
        Ok(())
    }

    pub fn loaded_plugins(&self) -> Vec<HostedPluginInfo> {
        self.loaded
            .iter()
            .map(|loaded| loaded.info.clone())
            .collect()
    }

    fn plugin(&self, id: u64) -> Result<&Arc<Mutex<Plugin>>, String> {
        self.loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map(|loaded| &loaded.plugin)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))
    }
}

fn prepare_playlist_pattern_render(
    plugin: &Plugin,
    notes: &[PlaylistMidiNote],
    ppq: u16,
    tempo_bpm: f64,
    total_frames: u64,
) -> Result<PreparedPatternRender, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    if notes.is_empty() {
        return Err("VST3 Playlist channel has no notes to render".to_owned());
    }
    let sample_rate = plugin.sample_rate();
    if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
        return Err("VST3 host sample rate is invalid".to_owned());
    }
    let sample_rate_u32 = sample_rate.round() as u32;
    let block_size = plugin.block_size();
    if block_size == 0 || block_size > 65_536 {
        return Err("VST3 host block size is not renderable".to_owned());
    }
    let output_channels = plugin.output_channel_count();
    if !(1..=2).contains(&output_channels) {
        return Err(format!(
            "VST3 Playlist supports mono or stereo output buses; the instrument has {output_channels} channels"
        ));
    }
    let (events, note_count) = scheduled_playlist_midi_events(notes, ppq, tempo_bpm, sample_rate)?;
    let last_event_frame = events
        .iter()
        .map(|event| event.frame)
        .max()
        .ok_or_else(|| "VST3 Playlist channel has no renderable notes".to_owned())?;
    Ok(PreparedPatternRender {
        events,
        note_count,
        sample_rate,
        sample_rate_u32,
        output_channels,
        block_size,
        total_frames: total_frames.max(last_event_frame.saturating_add(1)),
    })
}

fn scheduled_playlist_midi_events(
    notes: &[PlaylistMidiNote],
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: f64,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Err("VST3 host sample rate must be finite and positive".to_owned());
    }
    if notes.len() > 2_000_000 {
        return Err("VST3 Playlist expands to more than 2000000 note events".to_owned());
    }
    let mut events = Vec::with_capacity(notes.len().saturating_mul(2));
    let mut note_count = 0usize;
    for note in notes {
        if note.key > 127 {
            return Err(format!(
                "Playlist pattern contains key {} outside the MIDI note range",
                note.key
            ));
        }
        if note.velocity > 127 {
            return Err(format!(
                "Playlist pattern contains velocity {} outside the MIDI range",
                note.velocity
            ));
        }
        let channel = MidiChannel::from_index(note.midi_channel).ok_or_else(|| {
            format!(
                "Playlist pattern contains MIDI channel {} outside 0..15",
                note.midi_channel
            )
        })?;
        let frame_for_tick =
            |tick: u64| ((tick as f64 / f64::from(ppq)) * (60.0 / tempo_bpm) * sample_rate).round();
        let start_frame = frame_for_tick(note.start_tick);
        let end_frame = frame_for_tick(note.stop_tick.max(note.start_tick.saturating_add(1)));
        if !start_frame.is_finite()
            || !end_frame.is_finite()
            || start_frame < 0.0
            || end_frame < start_frame
            || end_frame > u64::MAX as f64
        {
            return Err("Playlist note timing is outside the renderable range".to_owned());
        }
        let start_frame = start_frame as u64;
        let end_frame = (end_frame as u64).max(start_frame.saturating_add(1));
        events.push(ScheduledMidiEvent {
            frame: start_frame,
            priority: 1,
            event: MidiEvent::NoteOn {
                channel,
                note: note.key as u8,
                velocity: note.velocity,
            },
        });
        events.push(ScheduledMidiEvent {
            frame: end_frame,
            priority: 0,
            event: MidiEvent::NoteOff {
                channel,
                note: note.key as u8,
                velocity: 0,
            },
        });
        note_count += 1;
    }
    events.sort_by_key(|event| (event.frame, event.priority));
    Ok((events, note_count))
}

fn playlist_plugin_pan_gains(pan: f32, mono: bool) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if mono {
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        (angle.cos(), angle.sin())
    } else if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

fn prepare_pattern_render(
    plugin: &Plugin,
    notes: &[PatternNote],
    options: Vst3PatternRenderOptions,
) -> Result<PreparedPatternRender, String> {
    let Vst3PatternRenderOptions {
        channel_id,
        ppq,
        tempo_bpm,
        tail_seconds,
    } = options;
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    if !tail_seconds.is_finite() || !(0.0..=60.0).contains(&tail_seconds) {
        return Err("render tail must be between 0 and 60 seconds".to_owned());
    }

    let sample_rate = plugin.sample_rate();
    if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
        return Err("VST3 host sample rate is invalid".to_owned());
    }
    let sample_rate_u32 = sample_rate.round() as u32;
    let block_size = plugin.block_size();
    if block_size == 0 || block_size > 65_536 {
        return Err("VST3 host block size is not renderable".to_owned());
    }
    let output_channels = plugin.output_channel_count();
    if output_channels == 0 || output_channels > 64 {
        return Err("VST3 instrument has no supported audio output buses".to_owned());
    }

    let (events, note_count) =
        scheduled_pattern_events(notes, channel_id, ppq, tempo_bpm, sample_rate)?;
    if note_count == 0 {
        return Err(format!(
            "pattern channel {channel_id} has no notes to render"
        ));
    }
    let final_note_frame = events
        .iter()
        .map(|event| event.frame)
        .max()
        .ok_or_else(|| "pattern channel has no renderable notes".to_owned())?;
    let tail_frames = (tail_seconds * sample_rate).round();
    if !tail_frames.is_finite() || tail_frames < 0.0 || tail_frames > u64::MAX as f64 {
        return Err("render tail is too long".to_owned());
    }
    let total_frames = final_note_frame
        .checked_add((tail_frames as u64).max(1))
        .ok_or_else(|| "render length overflow".to_owned())?;

    Ok(PreparedPatternRender {
        events,
        note_count,
        sample_rate,
        sample_rate_u32,
        output_channels,
        block_size,
        total_frames,
    })
}

fn process_pattern_render(
    plugin: &mut Plugin,
    render: &PreparedPatternRender,
    mut consume: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<(), String> {
    plugin
        .start_processing()
        .map_err(|error| error.to_string())?;
    let render_result = (|| {
        let mut rendered_frames = 0u64;
        let mut event_index = 0usize;
        let buffer_samples = render
            .block_size
            .checked_mul(render.output_channels)
            .ok_or_else(|| "VST3 block buffer size overflow".to_owned())?;
        let mut interleaved = Vec::<f32>::with_capacity(buffer_samples);
        while rendered_frames < render.total_frames {
            let frame_count =
                (render.total_frames - rendered_frames).min(render.block_size as u64) as usize;
            let block_end = rendered_frames + frame_count as u64;
            while let Some(event) = render.events.get(event_index)
                && event.frame < block_end
            {
                let offset = event.frame.saturating_sub(rendered_frames) as i32;
                plugin
                    .send_midi_event_at(event.event, offset)
                    .map_err(|error| error.to_string())?;
                event_index += 1;
            }

            let mut buffers =
                AudioBuffers::new(0, render.output_channels, frame_count, render.sample_rate);
            plugin
                .process_audio(&mut buffers)
                .map_err(|error| error.to_string())?;
            if buffers.outputs.len() != render.output_channels
                || buffers
                    .outputs
                    .iter()
                    .any(|channel| channel.len() < frame_count)
            {
                return Err("VST3 returned audio buffers with an unexpected shape".to_owned());
            }

            interleaved.clear();
            for frame in 0..frame_count {
                for channel in &buffers.outputs {
                    interleaved.push(channel[frame]);
                }
            }
            consume(&interleaved)?;
            rendered_frames = block_end;
        }
        Ok(())
    })();
    let stop_result = plugin.stop_processing().map_err(|error| error.to_string());
    render_result?;
    stop_result
}

fn stream_pattern_render(
    plugin: Arc<Mutex<Plugin>>,
    render: PreparedPatternRender,
    writer: &StreamingAudioWriter,
    output_sample_rate: u32,
) -> Result<Vst3RenderSummary, String> {
    let mut resampler = StereoStreamResampler::new(render.sample_rate_u32, output_sample_rate)?;
    {
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        plugin
            .start_processing()
            .map_err(|error| error.to_string())?;
    }

    let render_result = (|| {
        let mut rendered_frames = 0_u64;
        let mut event_index = 0;
        while rendered_frames < render.total_frames {
            if writer.is_cancelled() {
                return Err("VST3 pattern stream was stopped".to_owned());
            }
            let frame_count =
                (render.total_frames - rendered_frames).min(render.block_size as u64) as usize;
            let block_end = rendered_frames + frame_count as u64;
            let mut buffers =
                AudioBuffers::new(0, render.output_channels, frame_count, render.sample_rate);
            let output = {
                let mut plugin = plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                while let Some(event) = render.events.get(event_index)
                    && event.frame < block_end
                {
                    let offset = event.frame.saturating_sub(rendered_frames) as i32;
                    plugin
                        .send_midi_event_at(event.event, offset)
                        .map_err(|error| error.to_string())?;
                    event_index += 1;
                }
                plugin
                    .process_audio(&mut buffers)
                    .map_err(|error| error.to_string())?;
                if buffers.outputs.len() != render.output_channels
                    || buffers
                        .outputs
                        .iter()
                        .any(|channel| channel.len() < frame_count)
                {
                    return Err("VST3 returned audio buffers with an unexpected shape".to_owned());
                }
                resampler.push_outputs(&buffers.outputs, frame_count, render.output_channels)?
            };
            writer.write_stereo_samples(&output)?;
            rendered_frames = block_end;
        }
        let output = resampler.finish()?;
        writer.write_stereo_samples(&output)?;
        Ok(Vst3RenderSummary {
            frames: render.total_frames,
            sample_rate: render.sample_rate_u32,
            output_channels: render.output_channels,
            notes_rendered: render.note_count,
        })
    })();
    let stop_result = plugin
        .lock()
        .map_err(|_| "plug-in state lock was poisoned".to_owned())?
        .stop_processing()
        .map_err(|error| error.to_string());
    match (render_result, stop_result) {
        (Err(render_error), _) => Err(render_error),
        (Ok(_), Err(stop_error)) => Err(stop_error),
        (Ok(summary), Ok(())) => Ok(summary),
    }
}

struct StereoStreamResampler {
    source_rate: f64,
    output_rate: f64,
    source_frames: u64,
    output_frames: u64,
    buffer_start: u64,
    buffer: VecDeque<[f32; 2]>,
}

impl StereoStreamResampler {
    fn new(source_rate: u32, output_rate: u32) -> Result<Self, String> {
        if source_rate == 0 || output_rate == 0 {
            return Err("stream sample rates must be positive".to_owned());
        }
        Ok(Self {
            source_rate: f64::from(source_rate),
            output_rate: f64::from(output_rate),
            source_frames: 0,
            output_frames: 0,
            buffer_start: 0,
            buffer: VecDeque::new(),
        })
    }

    fn push_outputs(
        &mut self,
        outputs: &[Vec<f32>],
        frame_count: usize,
        output_channels: usize,
    ) -> Result<Vec<f32>, String> {
        if !(1..=2).contains(&output_channels)
            || outputs.len() != output_channels
            || outputs.iter().any(|channel| channel.len() < frame_count)
        {
            return Err("VST3 output block is not mono or stereo".to_owned());
        }
        self.source_frames = self
            .source_frames
            .checked_add(frame_count as u64)
            .ok_or_else(|| "VST3 stream frame count overflow".to_owned())?;
        if output_channels == 1 {
            for &sample in outputs[0].iter().take(frame_count) {
                let sample = finite_sample(sample);
                self.buffer.push_back([sample, sample]);
            }
        } else {
            for (&left, &right) in outputs[0].iter().zip(&outputs[1]).take(frame_count) {
                self.buffer
                    .push_back([finite_sample(left), finite_sample(right)]);
            }
        }
        self.emit_ready(false)
    }

    fn finish(&mut self) -> Result<Vec<f32>, String> {
        self.emit_ready(true)
    }

    fn emit_ready(&mut self, final_block: bool) -> Result<Vec<f32>, String> {
        if self.source_frames == 0 {
            return Ok(Vec::new());
        }
        let target_frames = if final_block {
            let count = self.source_frames as f64 * self.output_rate / self.source_rate;
            if !count.is_finite() || count.ceil() > u64::MAX as f64 {
                return Err("resampled VST3 stream is too long".to_owned());
            }
            count.ceil() as u64
        } else {
            u64::MAX
        };
        let mut output = Vec::new();
        loop {
            if self.output_frames >= target_frames {
                break;
            }
            let source_position = self.output_frames as f64 * self.source_rate / self.output_rate;
            if !source_position.is_finite() || source_position > u64::MAX as f64 {
                return Err("VST3 stream resampling position overflow".to_owned());
            }
            let last_source_frame = self.source_frames - 1;
            let source_position = if final_block {
                source_position.min(last_source_frame as f64)
            } else {
                source_position
            };
            let first_frame = source_position.floor() as u64;
            let second_frame = first_frame.saturating_add(1);
            if !final_block && second_frame >= self.source_frames {
                break;
            }
            let second_frame = second_frame.min(last_source_frame);
            let first = self.sample_at(first_frame)?;
            let second = self.sample_at(second_frame)?;
            let fraction = (source_position - first_frame as f64) as f32;
            output.push(first[0] + (second[0] - first[0]) * fraction);
            output.push(first[1] + (second[1] - first[1]) * fraction);
            self.output_frames = self
                .output_frames
                .checked_add(1)
                .ok_or_else(|| "VST3 stream output frame count overflow".to_owned())?;

            let next_source_position =
                self.output_frames as f64 * self.source_rate / self.output_rate;
            if !next_source_position.is_finite() || next_source_position > u64::MAX as f64 {
                return Err("VST3 stream resampling position overflow".to_owned());
            }
            let discard_before =
                (next_source_position.floor() as u64).min(self.source_frames.saturating_sub(1));
            while self.buffer_start < discard_before {
                self.buffer.pop_front();
                self.buffer_start += 1;
            }
        }
        Ok(output)
    }

    fn sample_at(&self, source_frame: u64) -> Result<[f32; 2], String> {
        let buffer_index = source_frame
            .checked_sub(self.buffer_start)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| "VST3 resampler lost a required source frame".to_owned())?;
        self.buffer
            .get(buffer_index)
            .copied()
            .ok_or_else(|| "VST3 resampler source frame is not buffered".to_owned())
    }
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn append_vst_output_block_to_stereo(
    stereo: &mut Vec<f32>,
    interleaved: &[f32],
    output_channels: usize,
) -> Result<(), String> {
    if !(1..=2).contains(&output_channels) {
        return Err(format!(
            "VST3 preview supports mono or stereo output buses; this instrument has {output_channels} channels"
        ));
    }
    if !interleaved.len().is_multiple_of(output_channels) {
        return Err("VST3 output block is not a whole number of frames".to_owned());
    }
    if output_channels == 1 {
        for sample in interleaved {
            stereo.extend_from_slice(&[*sample, *sample]);
        }
    } else {
        stereo.extend_from_slice(interleaved);
    }
    Ok(())
}

fn scheduled_pattern_events(
    notes: &[PatternNote],
    channel_id: u16,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: f64,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Err("VST3 host sample rate must be finite and positive".to_owned());
    }
    let mut events = Vec::new();
    let mut note_count = 0usize;
    for note in notes.iter().filter(|note| note.channel_id == channel_id) {
        if note.key > 127 {
            return Err(format!(
                "pattern channel {channel_id} contains key {} outside the MIDI note range",
                note.key
            ));
        }
        if note.length == 0 {
            return Err(format!(
                "pattern channel {channel_id} contains a zero-length note"
            ));
        }
        if note.velocity > 127 {
            return Err(format!(
                "pattern channel {channel_id} contains velocity {} outside the MIDI range",
                note.velocity
            ));
        }
        let channel = MidiChannel::from_index(note.midi_channel).ok_or_else(|| {
            format!(
                "pattern channel {channel_id} contains MIDI channel {} outside 0..15",
                note.midi_channel
            )
        })?;
        let start_tick = u64::from(note.position);
        let end_tick = start_tick
            .checked_add(u64::from(note.length))
            .ok_or_else(|| "note end position overflow".to_owned())?;
        let frame_for_tick =
            |tick: u64| ((tick as f64 / f64::from(ppq)) * (60.0 / tempo_bpm) * sample_rate).round();
        let start_frame = frame_for_tick(start_tick);
        let end_frame = frame_for_tick(end_tick);
        if !start_frame.is_finite()
            || !end_frame.is_finite()
            || start_frame < 0.0
            || end_frame < start_frame
            || end_frame > u64::MAX as f64
        {
            return Err("note timing is outside the renderable range".to_owned());
        }
        events.push(ScheduledMidiEvent {
            frame: start_frame as u64,
            priority: 1,
            event: MidiEvent::NoteOn {
                channel,
                note: note.key as u8,
                velocity: note.velocity,
            },
        });
        events.push(ScheduledMidiEvent {
            frame: end_frame as u64,
            priority: 0,
            event: MidiEvent::NoteOff {
                channel,
                note: note.key as u8,
                velocity: 0,
            },
        });
        note_count += 1;
    }
    events.sort_by_key(|event| (event.frame, event.priority));
    Ok((events, note_count))
}

fn write_float_wave_header(
    file: &mut std::fs::File,
    frames: u32,
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    use std::io::Write;

    let data_bytes = frames
        .checked_mul(u32::from(channels))
        .and_then(|samples| samples.checked_mul(4))
        .ok_or_else(|| "rendered WAV size overflow".to_owned())?;
    let block_align = channels
        .checked_mul(4)
        .ok_or_else(|| "rendered WAV channel count overflow".to_owned())?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "rendered WAV byte rate overflow".to_owned())?;
    file.write_all(b"RIFF")
        .and_then(|()| file.write_all(&(36u32 + data_bytes).to_le_bytes()))
        .and_then(|()| file.write_all(b"WAVEfmt "))
        .and_then(|()| file.write_all(&16u32.to_le_bytes()))
        .and_then(|()| file.write_all(&3u16.to_le_bytes()))
        .and_then(|()| file.write_all(&channels.to_le_bytes()))
        .and_then(|()| file.write_all(&sample_rate.to_le_bytes()))
        .and_then(|()| file.write_all(&byte_rate.to_le_bytes()))
        .and_then(|()| file.write_all(&block_align.to_le_bytes()))
        .and_then(|()| file.write_all(&32u16.to_le_bytes()))
        .and_then(|()| file.write_all(b"data"))
        .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
        .map_err(|error| format!("could not write WAV header: {error}"))
}

struct TemporaryWaveFile {
    path: PathBuf,
    file: Option<std::fs::File>,
    committed: bool,
}

impl TemporaryWaveFile {
    fn create(output_path: &Path) -> Result<Self, String> {
        use std::fs::OpenOptions;

        let parent = output_path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = output_path
            .file_name()
            .ok_or_else(|| "output WAV path must include a file name".to_owned())?;
        for _ in 0..100 {
            let id = NEXT_RENDER_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = file_name.to_os_string();
            temporary_name.push(format!(".{}.{}.tmp", std::process::id(), id));
            let path = parent.join(temporary_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "could not create temporary WAV beside {}: {error}",
                        output_path.display()
                    ));
                }
            }
        }
        Err("could not allocate a unique temporary WAV file".to_owned())
    }

    fn commit(mut self, output_path: &Path) -> Result<(), String> {
        use std::io::Write;

        let mut file = self.file.take().expect("temporary WAV is open");
        file.flush()
            .map_err(|error| format!("could not flush rendered WAV: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync rendered WAV: {error}"))?;
        drop(file);
        if output_path.exists() {
            std::fs::remove_file(output_path)
                .map_err(|error| format!("could not replace {}: {error}", output_path.display()))?;
        }
        std::fs::rename(&self.path, output_path)
            .map_err(|error| format!("could not finalize {}: {error}", output_path.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TemporaryWaveFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn hosted_info(id: u64, info: &PluginInfo, has_editor: bool) -> HostedPluginInfo {
    HostedPluginInfo {
        id,
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        category: info.category.clone(),
        uid: info.uid.clone(),
        path: info.path.clone(),
        has_editor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn schedules_pattern_note_on_project_timeline() {
        let note = PatternNote {
            position: 480,
            length: 240,
            channel_id: 7,
            key: 60,
            midi_channel: 1,
            velocity: 99,
            ..PatternNote::default()
        };
        let (events, note_count) =
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0).unwrap();
        assert_eq!(note_count, 1);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].frame, 24_000);
        assert_eq!(events[1].frame, 36_000);
        assert!(matches!(
            events[0].event,
            MidiEvent::NoteOn {
                channel: MidiChannel::Ch2,
                note: 60,
                velocity: 99
            }
        ));
        assert!(matches!(
            events[1].event,
            MidiEvent::NoteOff {
                channel: MidiChannel::Ch2,
                note: 60,
                velocity: 0
            }
        ));
    }

    #[test]
    fn note_off_precedes_note_on_at_a_shared_frame() {
        let notes = [
            PatternNote {
                position: 0,
                length: 120,
                channel_id: 3,
                key: 60,
                velocity: 100,
                ..PatternNote::default()
            },
            PatternNote {
                position: 120,
                length: 120,
                channel_id: 3,
                key: 60,
                velocity: 100,
                ..PatternNote::default()
            },
        ];
        let (events, _) = scheduled_pattern_events(&notes, 3, 480, 120.0, 48_000.0).unwrap();
        assert_eq!(events[1].frame, events[2].frame);
        assert!(matches!(events[1].event, MidiEvent::NoteOff { .. }));
        assert!(matches!(events[2].event, MidiEvent::NoteOn { .. }));
    }

    #[test]
    fn playlist_notes_keep_absolute_arrangement_ticks_and_order_tied_note_offs_first() {
        let notes = [
            PlaylistMidiNote {
                start_tick: 480,
                stop_tick: 720,
                key: 64,
                velocity: 96,
                midi_channel: 0,
            },
            PlaylistMidiNote {
                start_tick: 720,
                stop_tick: 960,
                key: 64,
                velocity: 80,
                midi_channel: 0,
            },
        ];

        let (events, note_count) =
            scheduled_playlist_midi_events(&notes, 480, 120.0, 48_000.0).unwrap();

        assert_eq!(note_count, 2);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].frame, 24_000);
        assert!(matches!(
            events[0].event,
            MidiEvent::NoteOn { note: 64, .. }
        ));
        assert_eq!(events[1].frame, 36_000);
        assert!(matches!(
            events[1].event,
            MidiEvent::NoteOff { note: 64, .. }
        ));
        assert_eq!(events[2].frame, 36_000);
        assert!(matches!(
            events[2].event,
            MidiEvent::NoteOn { note: 64, .. }
        ));
        assert_eq!(events[3].frame, 48_000);
        assert!(matches!(
            events[3].event,
            MidiEvent::NoteOff { note: 64, .. }
        ));
    }

    #[test]
    fn playlist_midi_scheduler_rejects_invalid_channel_and_timing() {
        let note = PlaylistMidiNote {
            start_tick: 0,
            stop_tick: 1,
            key: 60,
            velocity: 100,
            midi_channel: 16,
        };
        assert!(
            scheduled_playlist_midi_events(&[note], 480, 120.0, 48_000.0)
                .unwrap_err()
                .contains("outside 0..15")
        );

        let note = PlaylistMidiNote {
            midi_channel: 0,
            ..note
        };
        assert!(
            scheduled_playlist_midi_events(&[note], 0, 120.0, 48_000.0)
                .unwrap_err()
                .contains("PPQ")
        );
    }

    #[test]
    fn preview_blocks_duplicate_mono_and_preserve_stereo_channel_order() {
        let mut stereo = Vec::new();
        append_vst_output_block_to_stereo(&mut stereo, &[0.25, -0.5], 1).unwrap();
        append_vst_output_block_to_stereo(&mut stereo, &[0.75, -1.0], 2).unwrap();
        assert_eq!(stereo, [0.25, 0.25, -0.5, -0.5, 0.75, -1.0]);
    }

    #[test]
    fn preview_blocks_reject_unsupported_channel_shapes() {
        let mut stereo = Vec::new();
        assert!(append_vst_output_block_to_stereo(&mut stereo, &[0.0], 0).is_err());
        assert!(append_vst_output_block_to_stereo(&mut stereo, &[0.0], 2).is_err());
        assert!(append_vst_output_block_to_stereo(&mut stereo, &[0.0; 6], 3).is_err());
        assert!(stereo.is_empty());
    }

    #[test]
    fn streaming_resampler_preserves_stereo_across_block_boundaries() {
        let mut resampler = StereoStreamResampler::new(2, 2).unwrap();
        assert_eq!(
            resampler
                .push_outputs(&[vec![0.0, 0.5], vec![1.0, 1.5]], 2, 2)
                .unwrap(),
            [0.0, 1.0]
        );
        assert_eq!(
            resampler
                .push_outputs(&[vec![1.0], vec![2.0]], 1, 2)
                .unwrap(),
            [0.5, 1.5]
        );
        assert_eq!(resampler.finish().unwrap(), [1.0, 2.0]);
    }

    #[test]
    fn streaming_resampler_changes_rate_and_duplicates_mono() {
        let mut upsample = StereoStreamResampler::new(2, 4).unwrap();
        assert_eq!(
            upsample.push_outputs(&[vec![0.0, 1.0, 2.0]], 3, 1).unwrap(),
            [0.0, 0.0, 0.5, 0.5, 1.0, 1.0, 1.5, 1.5]
        );
        assert_eq!(upsample.finish().unwrap(), [2.0, 2.0, 2.0, 2.0]);

        let mut downsample = StereoStreamResampler::new(48_000, 1).unwrap();
        assert_eq!(
            downsample
                .push_outputs(&[vec![0.25, 0.5], vec![-0.25, -0.5]], 2, 2)
                .unwrap(),
            [0.25, -0.25]
        );
        assert!(downsample.finish().unwrap().is_empty());
    }

    #[test]
    fn streaming_resampler_rejects_invalid_rates_and_output_shapes() {
        assert!(StereoStreamResampler::new(0, 48_000).is_err());
        let mut resampler = StereoStreamResampler::new(44_100, 48_000).unwrap();
        assert!(resampler.push_outputs(&[], 4, 2).is_err());
        assert!(resampler.push_outputs(&[vec![0.0; 4]], 4, 2).is_err());
    }

    #[test]
    fn rejects_notes_that_cannot_be_represented_as_midi() {
        let note = PatternNote {
            channel_id: 2,
            key: 128,
            length: 60,
            velocity: 100,
            ..PatternNote::default()
        };
        let error = scheduled_pattern_events(&[note], 2, 480, 120.0, 48_000.0).unwrap_err();
        assert!(error.contains("outside the MIDI note range"));
    }

    #[test]
    fn writes_and_commits_float_wav_header_and_audio_data() {
        let output_path = std::env::temp_dir().join(format!(
            "flp-rebuild-render-test-{}-{}.wav",
            std::process::id(),
            NEXT_RENDER_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&output_path, b"previous output").unwrap();
        let mut temporary = TemporaryWaveFile::create(&output_path).unwrap();
        write_float_wave_header(temporary.file.as_mut().unwrap(), 2, 48_000, 2).unwrap();
        temporary
            .file
            .as_mut()
            .unwrap()
            .write_all(&[0u8; 16])
            .unwrap();
        temporary.commit(&output_path).unwrap();

        let bytes = std::fs::read(&output_path).unwrap();
        assert_eq!(bytes.len(), 60);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 52);
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        std::fs::remove_file(output_path).unwrap();
    }
}

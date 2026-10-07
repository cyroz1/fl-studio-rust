//! Runtime hosting for already-installed VST3 plug-ins.
//!
//! The project parser decodes VST identity metadata from recognized FLP envelopes,
//! while preserving their complete raw bytes. This runtime does not translate
//! Image-Line's FLP state envelope into the state stream expected by every VST3.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3PatternRenderOptions {
    pub channel_id: u16,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub tail_seconds: f64,
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

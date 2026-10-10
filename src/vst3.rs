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

use crate::audio::StreamingAudioWriter;
use crate::sample_render::{
    channel_gain_pan, schedule_playlist_pattern_notes, swing_note_start_tick,
};
use crate::{ChannelNoteRouter, ChannelPluginState, FlpDocument};
use vst3_host::audio::AudioBuffers;
use vst3_host::midi::{MidiChannel, MidiEvent};
use vst3_host::{Plugin, PluginWindow, Vst3Host};

use crate::PatternNote;

static NEXT_RENDER_FILE_ID: AtomicU64 = AtomicU64::new(1);
const MAX_STEREO_BUFFER_BYTES: usize = 512 * 1024 * 1024;
const MAX_VST3_AUTOMATION_POINTS: usize = 1_000_000;
// Leave headroom below vst3-host's 4096-change processing queue for concurrent editor edits.
const MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK: usize = 2_048;
const FLP_VST3_STATE_MARKER: u32 = 1;
const VST3_HOST_STATE_MAGIC: &[u8; 16] = b"VST3HOST_STATE\0\0";
const VST3_HOST_STATE_VERSION: u32 = 1;
const VST3_HOST_NO_CONTROLLER_STATE: u32 = u32::MAX;
const MAX_VST3_STATE_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REPORTED_TAIL_SECONDS: f64 = 60.0;
const VST3_HOST_STATE_HEADER_SIZE: usize = 28;

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
    parameter_automation: Vec<PreparedParameterAutomation>,
    note_count: usize,
    sample_rate: f64,
    sample_rate_u32: u32,
    output_channels: usize,
    block_size: usize,
    total_frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScheduledParameterPoint {
    frame: u64,
    value: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedParameterAutomation {
    parameter_id: u32,
    points: Vec<ScheduledParameterPoint>,
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
    pub global_swing_mix_raw: u8,
    pub channel_swing_mix_raw: u16,
}

/// One normalized VST3 parameter value at a pattern-relative beat position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3AutomationPoint {
    pub position_beats: f64,
    pub value: f64,
}

/// Explicit VST3 parameter automation for a pattern render.
///
/// `parameter_id` must be an ID reported by the loaded VST3, and point positions are
/// measured from the start of the rendered pattern. Values use the VST3 normalized
/// 0..=1 range. Points are scheduled at their sample offsets; when a segment crosses a
/// processing-block boundary, its value is evaluated linearly at the next block start.
/// Automation is limited to 2,048 scheduled changes per processing block.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst3ParameterAutomation {
    pub parameter_id: u32,
    pub points: Vec<Vst3AutomationPoint>,
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
        crate::audio::enable_denormal_protection();
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
                    set_parameter_automation_for_block(
                        &mut plugin,
                        &self.render.parameter_automation,
                        self.source_frame,
                        source_count,
                    )?;
                    while let Some(event) = self.render.events.get(self.next_event)
                        && event.frame < block_end
                    {
                        let offset = event.frame.saturating_sub(self.source_frame) as i32;
                        plugin
                            .send_midi_event_at(event.event, offset)
                            .map_err(|error| error.to_string())?;
                        self.next_event += 1;
                    }
                    crate::audio::enable_denormal_protection();
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

#[derive(Clone, Debug, PartialEq)]
pub struct HostedPluginInfo {
    pub id: u64,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub category: String,
    pub uid: String,
    pub path: PathBuf,
    pub has_editor: bool,
    /// Latency most recently reported by `IAudioProcessor::getLatencySamples`.
    pub latency_samples: u32,
    /// Sample rate configured for this plug-in instance when the report was read.
    pub sample_rate_hz: f64,
}

impl HostedPluginInfo {
    /// Convert the reported latency to milliseconds using the plug-in's configured rate.
    pub fn latency_milliseconds(&self) -> Option<f64> {
        (self.sample_rate_hz.is_finite() && self.sample_rate_hz > 0.0)
            .then(|| f64::from(self.latency_samples) * 1000.0 / self.sample_rate_hz)
    }
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

fn encode_flp_vst3_state_snapshot(nested: &[u8]) -> Result<Vec<u8>, String> {
    let marker = read_u32(nested, 0)?;
    if marker != FLP_VST3_STATE_MARKER {
        return Err(format!("unsupported FLP nested VST3 state marker {marker}"));
    }

    let mut cursor = 4usize;
    let mut component = None;
    let mut controller = None;
    while cursor < nested.len() {
        let header_end = cursor
            .checked_add(12)
            .ok_or_else(|| "FLP VST3 state record header offset overflow".to_owned())?;
        if header_end > nested.len() {
            return Err("truncated FLP VST3 state record header".to_owned());
        }
        let id = read_u32(nested, cursor)?;
        let encoded_length = u64::from_le_bytes(
            nested[cursor + 4..header_end]
                .try_into()
                .map_err(|_| "truncated FLP VST3 state record length".to_owned())?,
        );
        let length = usize::try_from(encoded_length)
            .map_err(|_| "FLP VST3 state record is too large for this platform".to_owned())?;
        let data_end = header_end
            .checked_add(length)
            .ok_or_else(|| "FLP VST3 state record length overflow".to_owned())?;
        let data = nested
            .get(header_end..data_end)
            .ok_or_else(|| "truncated FLP VST3 state record data".to_owned())?;
        match id {
            3 if component.is_some() => {
                return Err("FLP VST3 state contains duplicate component records".to_owned());
            }
            3 => component = Some(data),
            4 if controller.is_some() => {
                return Err("FLP VST3 state contains duplicate controller records".to_owned());
            }
            4 => controller = Some(data),
            _ => {}
        }
        cursor = data_end;
    }

    let component =
        component.ok_or_else(|| "FLP VST3 state has no component record (field 3)".to_owned())?;
    let payload_length = component
        .len()
        .checked_add(controller.map_or(0, <[u8]>::len))
        .ok_or_else(|| "FLP VST3 state payload length overflow".to_owned())?;
    if payload_length > MAX_VST3_STATE_PAYLOAD_BYTES {
        return Err(format!(
            "FLP VST3 state payload is too large ({} bytes, maximum {})",
            payload_length, MAX_VST3_STATE_PAYLOAD_BYTES
        ));
    }
    let component_length = u32::try_from(component.len())
        .map_err(|_| "FLP VST3 component state is too large".to_owned())?;
    let controller_length = match controller {
        Some(bytes) => u32::try_from(bytes.len())
            .map_err(|_| "FLP VST3 controller state is too large".to_owned())?,
        None => VST3_HOST_NO_CONTROLLER_STATE,
    };
    let total_length = VST3_HOST_STATE_HEADER_SIZE
        .checked_add(payload_length)
        .ok_or_else(|| "VST3 host state snapshot length overflow".to_owned())?;
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(total_length)
        .map_err(|error| format!("could not allocate VST3 host state snapshot: {error}"))?;
    snapshot.extend_from_slice(VST3_HOST_STATE_MAGIC);
    snapshot.extend_from_slice(&VST3_HOST_STATE_VERSION.to_le_bytes());
    snapshot.extend_from_slice(&component_length.to_le_bytes());
    snapshot.extend_from_slice(&controller_length.to_le_bytes());
    snapshot.extend_from_slice(component);
    if let Some(controller) = controller {
        snapshot.extend_from_slice(controller);
    }
    Ok(snapshot)
}

fn decode_vst3_host_state_snapshot(snapshot: &[u8]) -> Result<(&[u8], Option<&[u8]>), String> {
    if snapshot.get(..16) != Some(VST3_HOST_STATE_MAGIC.as_slice()) {
        return Err("unsupported VST3 host state snapshot header".to_owned());
    }
    let version = read_u32(snapshot, 16)?;
    if version != VST3_HOST_STATE_VERSION {
        return Err(format!("unsupported VST3 host state version {version}"));
    }
    let component_length = usize::try_from(read_u32(snapshot, 20)?)
        .map_err(|_| "VST3 component state is too large for this platform".to_owned())?;
    let controller_length = read_u32(snapshot, 24)?;
    let component_end = VST3_HOST_STATE_HEADER_SIZE
        .checked_add(component_length)
        .ok_or_else(|| "VST3 component state length overflow".to_owned())?;
    let component = snapshot
        .get(VST3_HOST_STATE_HEADER_SIZE..component_end)
        .ok_or_else(|| "truncated VST3 component state snapshot".to_owned())?;
    let controller = if controller_length == VST3_HOST_NO_CONTROLLER_STATE {
        if component_end != snapshot.len() {
            return Err("VST3 snapshot has unexpected bytes after component state".to_owned());
        }
        None
    } else {
        let controller_length = usize::try_from(controller_length)
            .map_err(|_| "VST3 controller state is too large for this platform".to_owned())?;
        let controller_end = component_end
            .checked_add(controller_length)
            .ok_or_else(|| "VST3 controller state length overflow".to_owned())?;
        let controller = snapshot
            .get(component_end..controller_end)
            .ok_or_else(|| "truncated VST3 controller state snapshot".to_owned())?;
        if controller_end != snapshot.len() {
            return Err("VST3 snapshot has unexpected bytes after controller state".to_owned());
        }
        Some(controller)
    };
    let payload_length = component
        .len()
        .checked_add(controller.map_or(0, <[u8]>::len))
        .ok_or_else(|| "VST3 state payload length overflow".to_owned())?;
    if payload_length > MAX_VST3_STATE_PAYLOAD_BYTES {
        return Err(format!(
            "VST3 state payload is too large ({} bytes, maximum {})",
            payload_length, MAX_VST3_STATE_PAYLOAD_BYTES
        ));
    }
    Ok((component, controller))
}

fn replace_flp_vst3_state_snapshot(nested: &[u8], snapshot: &[u8]) -> Result<Vec<u8>, String> {
    let marker = read_u32(nested, 0)?;
    if marker != FLP_VST3_STATE_MARKER {
        return Err(format!("unsupported FLP nested VST3 state marker {marker}"));
    }
    if nested.len() > MAX_VST3_STATE_PAYLOAD_BYTES {
        return Err("FLP VST3 nested state is too large to update safely".to_owned());
    }
    let (component_state, controller_state) = decode_vst3_host_state_snapshot(snapshot)?;

    let mut output = Vec::new();
    output
        .try_reserve(nested.len())
        .map_err(|error| format!("could not allocate FLP VST3 state: {error}"))?;
    output.extend_from_slice(&nested[..4]);
    let mut cursor = 4usize;
    let mut component_seen = false;
    let mut controller_seen = false;
    while cursor < nested.len() {
        let header_end = cursor
            .checked_add(12)
            .ok_or_else(|| "FLP VST3 state record header offset overflow".to_owned())?;
        if header_end > nested.len() {
            return Err("truncated FLP VST3 state record header".to_owned());
        }
        let id = read_u32(nested, cursor)?;
        let encoded_length = u64::from_le_bytes(
            nested[cursor + 4..header_end]
                .try_into()
                .map_err(|_| "truncated FLP VST3 state record length".to_owned())?,
        );
        let length = usize::try_from(encoded_length)
            .map_err(|_| "FLP VST3 state record is too large for this platform".to_owned())?;
        let data_end = header_end
            .checked_add(length)
            .ok_or_else(|| "FLP VST3 state record length overflow".to_owned())?;
        nested
            .get(header_end..data_end)
            .ok_or_else(|| "truncated FLP VST3 state record data".to_owned())?;
        match id {
            3 if component_seen => {
                return Err("FLP VST3 state contains duplicate component records".to_owned());
            }
            3 => {
                component_seen = true;
                append_flp_vst3_state_record(&mut output, id, component_state)?;
            }
            4 if controller_seen => {
                return Err("FLP VST3 state contains duplicate controller records".to_owned());
            }
            4 => {
                controller_seen = true;
                if let Some(controller) = controller_state {
                    append_flp_vst3_state_record(&mut output, id, controller)?;
                }
            }
            _ => output.extend_from_slice(&nested[cursor..data_end]),
        }
        cursor = data_end;
    }
    if !component_seen {
        return Err("FLP VST3 state has no component record (field 3)".to_owned());
    }
    if let Some(controller) = controller_state
        && !controller_seen
    {
        append_flp_vst3_state_record(&mut output, 4, controller)?;
    }
    if output.len() > MAX_VST3_STATE_PAYLOAD_BYTES {
        return Err("updated FLP VST3 nested state exceeds the size limit".to_owned());
    }
    Ok(output)
}

fn append_flp_vst3_state_record(payload: &mut Vec<u8>, id: u32, data: &[u8]) -> Result<(), String> {
    let length =
        u64::try_from(data.len()).map_err(|_| "FLP VST3 state record is too large".to_owned())?;
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(data);
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "VST3 state integer offset overflow".to_owned())?;
    let bytes = bytes
        .get(offset..end)
        .ok_or_else(|| "truncated VST3 state integer".to_owned())?;
    Ok(u32::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| "truncated VST3 state integer".to_owned())?,
    ))
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

        let info = hosted_info(self.next_id, &plugin);
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
        let plugin = Arc::clone(self.plugin(id)?);
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        plugin
            .load_state(state)
            .map_err(|error| error.to_string())?;
        let latency_samples = plugin.latency_samples();
        let sample_rate_hz = plugin.sample_rate();
        drop(plugin);
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        loaded.info.latency_samples = latency_samples;
        loaded.info.sample_rate_hz = sample_rate_hz;
        Ok(())
    }

    /// Restore both VST3 state streams stored in an FLP channel's `0xD5` record.
    ///
    /// FLP field 53 contains Image-Line's record stream, not a raw VST3 component stream. The
    /// supported marker-12 VST3 layout stores the component stream in nested record 3 and the
    /// controller stream in nested record 4. These are converted to the snapshot envelope
    /// expected by `vst3-host` before calling the plug-in.
    pub fn restore_flp_channel_state(
        &mut self,
        id: u64,
        state: &ChannelPluginState,
    ) -> Result<(), String> {
        let metadata = state
            .vst_metadata()
            .ok_or_else(|| "FLP channel has no recognized VST state envelope".to_owned())?;
        if metadata.format_marker() != 12 {
            return Err(format!(
                "unsupported FLP VST wrapper marker {}; VST3 state restore requires marker 12",
                metadata.format_marker()
            ));
        }
        if metadata.fourcc().is_some() {
            return Err("FLP channel state identifies a VST2 plug-in, not VST3".to_owned());
        }
        let expected_uid = metadata
            .class_uid()
            .ok_or_else(|| "FLP VST3 state has no valid 16-byte class UID".to_owned())?;
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if !loaded.info.uid.eq_ignore_ascii_case(&expected_uid) {
            return Err(format!(
                "FLP state class UID {expected_uid} does not match loaded plug-in UID {}",
                loaded.info.uid
            ));
        }
        let nested_state = state
            .vst_state_bytes()
            .ok_or_else(|| "FLP VST3 state has no nested field 53".to_owned())?;
        let host_snapshot = encode_flp_vst3_state_snapshot(nested_state)?;
        self.restore_state(id, &host_snapshot)
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

    /// Save an FLP VST3 channel's current component and controller state into its
    /// marker-12 nested state stream, retaining unrelated FLP records byte-for-byte.
    pub fn save_flp_channel_state(
        &self,
        id: u64,
        state: &ChannelPluginState,
    ) -> Result<Vec<u8>, String> {
        let metadata = state
            .vst_metadata()
            .ok_or_else(|| "FLP channel has no recognized VST state envelope".to_owned())?;
        if metadata.format_marker() != 12 {
            return Err(format!(
                "unsupported FLP VST wrapper marker {}; state write-back requires marker 12",
                metadata.format_marker()
            ));
        }
        if metadata.fourcc().is_some() {
            return Err("FLP channel state identifies a VST2 plug-in, not VST3".to_owned());
        }
        let expected_uid = metadata
            .class_uid()
            .ok_or_else(|| "FLP VST3 state has no valid 16-byte class UID".to_owned())?;
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if !loaded.info.uid.eq_ignore_ascii_case(&expected_uid) {
            return Err(format!(
                "FLP state class UID {expected_uid} does not match loaded plug-in UID {}",
                loaded.info.uid
            ));
        }
        let nested_state = state
            .vst_state_bytes()
            .ok_or_else(|| "FLP VST3 state has no nested field 53".to_owned())?;
        let snapshot = self.save_state(id)?;
        replace_flp_vst3_state_snapshot(nested_state, &snapshot)
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
        self.render_pattern_channel_to_wav_with_automation(id, notes, options, &[], path)
    }

    /// Render a pattern channel with explicitly mapped VST3 parameter automation lanes.
    ///
    /// The automation uses pattern-relative beats and normalized values. It does not decode
    /// FL Studio automation target links; callers must supply the VST3 parameter IDs.
    pub fn render_pattern_channel_to_wav_with_automation(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
        path: impl AsRef<Path>,
    ) -> Result<Vst3RenderSummary, String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render =
            prepare_pattern_render_with_automation(&plugin, notes, options, parameter_automation)?;
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
        self.render_pattern_channel_to_stereo_buffer_with_automation(id, notes, options, &[])
    }

    /// Render a pattern channel to stereo samples with explicit parameter automation lanes.
    pub fn render_pattern_channel_to_stereo_buffer_with_automation(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
    ) -> Result<(Vec<f32>, Vst3RenderSummary), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render =
            prepare_pattern_render_with_automation(&plugin, notes, options, parameter_automation)?;
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
        self.prepare_pattern_channel_stream_with_automation(id, notes, options, &[])
    }

    /// Prepare a pattern stream with explicit VST3 parameter automation lanes.
    pub fn prepare_pattern_channel_stream_with_automation(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
    ) -> Result<Vst3PatternStream, String> {
        let plugin = self.plugin(id)?.clone();
        let render = {
            let plugin_guard = plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            prepare_pattern_render_with_automation(
                &plugin_guard,
                notes,
                options,
                parameter_automation,
            )?
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
        include_plugin_reported_tails: bool,
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
        let channel_router = ChannelNoteRouter::new(channels.clone());
        let mut plugin_channels: BTreeSet<_> = document
            .channel_plugin_states()
            .into_iter()
            .map(|state| state.channel_id())
            .collect();
        let disabled_track_ids = document
            .playlist_tracks()
            .into_iter()
            .filter(|track| track.enabled == Some(false))
            .map(|track| track.id)
            .collect::<BTreeSet<_>>();
        plugin_channels.extend(
            channels
                .iter()
                .filter(|channel| {
                    channel.plugin_identifier().is_some() && channel.enabled() != Some(false)
                })
                .map(|channel| channel.id()),
        );
        let schedule = schedule_playlist_pattern_notes(
            &patterns,
            &arrangement,
            &disabled_track_ids,
            ppq,
            document.metadata().global_swing_mix(),
            |channel_id| {
                channels_by_id
                    .get(&channel_id)
                    .map_or(128, |channel| channel.swing_mix())
            },
            |channel_id, seed| {
                channel_router
                    .targets(channel_id, seed)
                    .into_iter()
                    .filter(|target_channel_id| plugin_channels.contains(target_channel_id))
                    .collect()
            },
        )?;

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
                .entry(placed.target_channel_id)
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
        let mut longest_plugin_tail_seconds = 0.0f64;
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
                if include_plugin_reported_tails
                    && let Some(tail_seconds) = reported_tail_seconds(
                        plugin_guard.tail_samples(),
                        plugin_guard.sample_rate(),
                    )
                {
                    longest_plugin_tail_seconds = longest_plugin_tail_seconds.max(tail_seconds);
                }
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
        } else if include_plugin_reported_tails {
            let effective_tail_seconds = tail_seconds.max(longest_plugin_tail_seconds);
            let total_output_frames =
                base_output_frames + effective_tail_seconds * f64::from(output_sample_rate);
            if !total_output_frames.is_finite() || total_output_frames > u64::MAX as f64 {
                return Err("VST3 Playlist length is outside the renderable range".to_owned());
            }
            output_frames = (total_output_frames.ceil() as u64).max(1);
        }
        let mut processor = Vst3PlaylistStreamProcessor {
            streams,
            output_sample_rate,
            output_frames,
            notes_scheduled,
            unloaded_plugin_channels: unloaded_plugin_channels.into_iter().collect(),
            started_count: 0,
        };
        processor.extend_to_output_frames(output_frames)?;
        Ok(processor)
    }

    /// Service editor/UI requests and VST3 restart requests on the control thread.
    ///
    /// Returns `true` when a plug-in changes its audio bus layout so callers can rebuild routing.
    pub fn service_editors(&mut self) -> Result<bool, String> {
        let mut io_changed = false;
        for loaded in &mut self.loaded {
            if let Some(editor) = loaded.editor.as_ref() {
                editor
                    .service_platform_events()
                    .map_err(|error| error.to_string())?;
            }
            let mut plugin = loaded
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            plugin.service_run_loop();
            let restart_flags = plugin
                .service_host_requests()
                .map_err(|error| error.to_string())?;
            if restart_flags.latency_changed() {
                loaded.info.latency_samples = plugin.latency_samples();
                loaded.info.sample_rate_hz = plugin.sample_rate();
            }
            io_changed |= restart_flags.io_changed();
        }
        Ok(io_changed)
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
    prepare_pattern_render_with_automation(plugin, notes, options, &[])
}

fn prepare_pattern_render_with_automation(
    plugin: &Plugin,
    notes: &[PatternNote],
    options: Vst3PatternRenderOptions,
    parameter_automation: &[Vst3ParameterAutomation],
) -> Result<PreparedPatternRender, String> {
    let Vst3PatternRenderOptions {
        channel_id,
        ppq,
        tempo_bpm,
        tail_seconds,
        global_swing_mix_raw,
        channel_swing_mix_raw,
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
    let parameter_automation =
        prepare_parameter_automation(parameter_automation, sample_rate, tempo_bpm)?;

    let (events, note_count) = scheduled_pattern_events(
        notes,
        channel_id,
        ppq,
        tempo_bpm,
        sample_rate,
        global_swing_mix_raw,
        channel_swing_mix_raw,
    )?;
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
    validate_parameter_automation_block_density(&parameter_automation, block_size, total_frames)?;

    Ok(PreparedPatternRender {
        events,
        parameter_automation,
        note_count,
        sample_rate,
        sample_rate_u32,
        output_channels,
        block_size,
        total_frames,
    })
}

fn prepare_parameter_automation(
    automation: &[Vst3ParameterAutomation],
    sample_rate: f64,
    tempo_bpm: f64,
) -> Result<Vec<PreparedParameterAutomation>, String> {
    let mut point_count = 0usize;
    let mut parameter_ids = BTreeSet::new();
    let mut prepared = Vec::with_capacity(automation.len());
    for lane in automation {
        if lane.points.is_empty() {
            continue;
        }
        if !parameter_ids.insert(lane.parameter_id) {
            return Err(format!(
                "VST3 parameter {} has more than one automation lane",
                lane.parameter_id
            ));
        }
        point_count = point_count
            .checked_add(lane.points.len())
            .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
            .ok_or_else(|| {
                format!("VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points")
            })?;

        let mut points = Vec::with_capacity(lane.points.len());
        for point in &lane.points {
            if !point.position_beats.is_finite() || point.position_beats < 0.0 {
                return Err("VST3 automation positions must be finite and non-negative".to_owned());
            }
            if !point.value.is_finite() || !(0.0..=1.0).contains(&point.value) {
                return Err(
                    "VST3 automation values must be finite and normalized to 0..=1".to_owned(),
                );
            }
            let frame = point.position_beats * 60.0 / tempo_bpm * sample_rate;
            if !frame.is_finite() || frame < 0.0 || frame >= u64::MAX as f64 {
                return Err("VST3 automation position is outside the renderable range".to_owned());
            }
            points.push(ScheduledParameterPoint {
                frame: frame.round() as u64,
                value: point.value,
            });
        }
        points.sort_by_key(|point| point.frame);
        let mut unique_points: Vec<ScheduledParameterPoint> = Vec::with_capacity(points.len());
        for point in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.frame == point.frame
            {
                previous.value = point.value;
            } else {
                unique_points.push(point);
            }
        }
        prepared.push(PreparedParameterAutomation {
            parameter_id: lane.parameter_id,
            points: unique_points,
        });
    }
    Ok(prepared)
}

fn parameter_automation_value_at_frame(
    points: &[ScheduledParameterPoint],
    frame: u64,
) -> Option<f64> {
    let next_index = points.partition_point(|point| point.frame <= frame);
    if next_index == 0 {
        return None;
    }
    let from = points[next_index - 1];
    let Some(to) = points.get(next_index) else {
        return Some(from.value);
    };
    let span = to.frame.saturating_sub(from.frame);
    if span == 0 {
        return Some(from.value);
    }
    let fraction = frame.saturating_sub(from.frame) as f64 / span as f64;
    Some(from.value + (to.value - from.value) * fraction.clamp(0.0, 1.0))
}

fn validate_parameter_automation_block_density(
    automation: &[PreparedParameterAutomation],
    block_size: usize,
    total_frames: u64,
) -> Result<(), String> {
    if block_size == 0 || total_frames == 0 {
        return Ok(());
    }
    let block_size =
        u64::try_from(block_size).map_err(|_| "VST3 automation block size overflow".to_owned())?;
    let mut active_from_blocks = Vec::with_capacity(automation.len());
    let mut in_block_point_counts = BTreeMap::<u64, usize>::new();

    for lane in automation {
        let first_point = lane.points.iter().find(|point| point.frame < total_frames);
        let Some(first_point) = first_point else {
            continue;
        };
        let first_block = first_point.frame / block_size;
        let first_offset = first_point.frame % block_size;
        active_from_blocks.push(first_block + (first_offset > 0) as u64);

        for point in lane
            .points
            .iter()
            .filter(|point| point.frame < total_frames)
        {
            let offset = point.frame % block_size;
            if offset > 0 {
                let count = in_block_point_counts
                    .entry(point.frame / block_size)
                    .or_default();
                *count = count.saturating_add(1);
            }
        }
    }

    active_from_blocks.sort_unstable();
    let mut candidate_blocks = active_from_blocks.iter().copied().collect::<BTreeSet<_>>();
    candidate_blocks.extend(in_block_point_counts.keys().copied());
    for block in candidate_blocks {
        let active_lanes = active_from_blocks.partition_point(|active| *active <= block);
        let in_block_points = in_block_point_counts
            .get(&block)
            .copied()
            .unwrap_or_default();
        let scheduled_changes = active_lanes.saturating_add(in_block_points);
        if scheduled_changes > MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK {
            return Err(format!(
                "VST3 automation schedules {scheduled_changes} parameter changes in one block; the limit is {MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK}"
            ));
        }
    }
    Ok(())
}

fn set_parameter_automation_for_block(
    plugin: &mut Plugin,
    automation: &[PreparedParameterAutomation],
    block_start_frame: u64,
    frame_count: usize,
) -> Result<(), String> {
    if frame_count == 0 {
        return Ok(());
    }
    for lane in automation {
        for_each_parameter_change_for_block(
            lane,
            block_start_frame,
            frame_count,
            |offset, value| {
                plugin
                    .set_parameter_at(lane.parameter_id, value, offset)
                    .map_err(|error| {
                        format!(
                            "could not automate VST3 parameter {}: {error}",
                            lane.parameter_id
                        )
                    })
            },
        )?;
    }
    Ok(())
}

fn for_each_parameter_change_for_block(
    lane: &PreparedParameterAutomation,
    block_start_frame: u64,
    frame_count: usize,
    mut visit: impl FnMut(i32, f64) -> Result<(), String>,
) -> Result<(), String> {
    if frame_count == 0 {
        return Ok(());
    }
    let block_end_frame = block_start_frame
        .checked_add(
            u64::try_from(frame_count)
                .map_err(|_| "VST3 automation block size overflow".to_owned())?,
        )
        .ok_or_else(|| "VST3 automation block end overflow".to_owned())?;
    if let Some(value) = parameter_automation_value_at_frame(&lane.points, block_start_frame) {
        visit(0, value)?;
    }
    let first_in_block = lane
        .points
        .partition_point(|point| point.frame <= block_start_frame);
    for point in &lane.points[first_in_block..] {
        if point.frame >= block_end_frame {
            break;
        }
        let offset = i32::try_from(point.frame - block_start_frame)
            .map_err(|_| "VST3 automation offset exceeds the host range".to_owned())?;
        visit(offset, point.value)?;
    }
    Ok(())
}

fn process_pattern_render(
    plugin: &mut Plugin,
    render: &PreparedPatternRender,
    mut consume: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<(), String> {
    crate::audio::enable_denormal_protection();
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
            set_parameter_automation_for_block(
                plugin,
                &render.parameter_automation,
                rendered_frames,
                frame_count,
            )?;
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
            crate::audio::enable_denormal_protection();
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
    crate::audio::enable_denormal_protection();
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
                set_parameter_automation_for_block(
                    &mut plugin,
                    &render.parameter_automation,
                    rendered_frames,
                    frame_count,
                )?;
                while let Some(event) = render.events.get(event_index)
                    && event.frame < block_end
                {
                    let offset = event.frame.saturating_sub(rendered_frames) as i32;
                    plugin
                        .send_midi_event_at(event.event, offset)
                        .map_err(|error| error.to_string())?;
                    event_index += 1;
                }
                crate::audio::enable_denormal_protection();
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
    global_swing_mix_raw: u8,
    channel_swing_mix_raw: u16,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
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
        let nominal_start_tick = u64::from(note.position);
        let start_tick = swing_note_start_tick(
            nominal_start_tick,
            ppq,
            global_swing_mix_raw,
            channel_swing_mix_raw,
        )?;
        let swing_offset = start_tick.saturating_sub(nominal_start_tick);
        let end_tick = nominal_start_tick
            .checked_add(u64::from(note.length))
            .and_then(|tick| tick.checked_add(swing_offset))
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

fn hosted_info(id: u64, plugin: &Plugin) -> HostedPluginInfo {
    let info = plugin.info();
    HostedPluginInfo {
        id,
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        category: info.category.clone(),
        uid: info.uid.clone(),
        path: info.path.clone(),
        has_editor: plugin.has_editor(),
        latency_samples: plugin.latency_samples(),
        sample_rate_hz: plugin.sample_rate(),
    }
}

fn reported_tail_seconds(tail_samples: u32, sample_rate_hz: f64) -> Option<f64> {
    if tail_samples == u32::MAX || !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
        return None;
    }
    Some((f64::from(tail_samples) / sample_rate_hz).min(MAX_REPORTED_TAIL_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parameter_automation_uses_pattern_beats_and_emits_block_offsets() {
        let lanes = [Vst3ParameterAutomation {
            parameter_id: 17,
            points: vec![
                Vst3AutomationPoint {
                    position_beats: 0.0,
                    value: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 0.02,
                    value: 1.0,
                },
                Vst3AutomationPoint {
                    position_beats: 0.04,
                    value: 0.0,
                },
            ],
        }];
        let prepared = prepare_parameter_automation(&lanes, 48_000.0, 120.0)
            .expect("normalized automation should prepare");
        assert_eq!(
            prepared[0].points,
            vec![
                ScheduledParameterPoint {
                    frame: 0,
                    value: 0.0,
                },
                ScheduledParameterPoint {
                    frame: 480,
                    value: 1.0,
                },
                ScheduledParameterPoint {
                    frame: 960,
                    value: 0.0,
                },
            ]
        );

        let collect_changes = |block_start_frame, frame_count| {
            let mut changes = Vec::new();
            for_each_parameter_change_for_block(
                &prepared[0],
                block_start_frame,
                frame_count,
                |offset, value| {
                    changes.push((offset, value));
                    Ok(())
                },
            )
            .expect("block automation should be schedulable");
            changes
        };
        assert_eq!(collect_changes(0, 512), vec![(0, 0.0), (480, 1.0)]);
        assert_eq!(
            collect_changes(512, 512),
            vec![(0, 56.0 / 60.0), (448, 0.0)]
        );
        assert_eq!(collect_changes(1024, 512), vec![(0, 0.0)]);
    }

    #[test]
    fn parameter_automation_preserves_future_points_and_collapses_same_frame_points() {
        let future = [Vst3ParameterAutomation {
            parameter_id: 18,
            points: vec![Vst3AutomationPoint {
                position_beats: 0.02,
                value: 0.75,
            }],
        }];
        let prepared = prepare_parameter_automation(&future, 48_000.0, 120.0)
            .expect("future automation should prepare");
        let mut changes = Vec::new();
        for_each_parameter_change_for_block(&prepared[0], 0, 256, |offset, value| {
            changes.push((offset, value));
            Ok(())
        })
        .unwrap();
        assert!(changes.is_empty());

        for_each_parameter_change_for_block(&prepared[0], 256, 256, |offset, value| {
            changes.push((offset, value));
            Ok(())
        })
        .unwrap();
        assert_eq!(changes, vec![(224, 0.75)]);

        let same_frame = [Vst3ParameterAutomation {
            parameter_id: 19,
            points: vec![
                Vst3AutomationPoint {
                    position_beats: 0.0,
                    value: 0.1,
                },
                Vst3AutomationPoint {
                    position_beats: 0.000001,
                    value: 0.9,
                },
            ],
        }];
        let prepared = prepare_parameter_automation(&same_frame, 48_000.0, 120.0)
            .expect("same-frame values should prepare");
        assert_eq!(
            prepared[0].points,
            vec![ScheduledParameterPoint {
                frame: 0,
                value: 0.9,
            }]
        );
    }

    #[test]
    fn parameter_automation_rejects_invalid_values_and_ambiguous_duplicate_lanes() {
        let invalid = [Vst3ParameterAutomation {
            parameter_id: 17,
            points: vec![Vst3AutomationPoint {
                position_beats: 0.0,
                value: f64::NAN,
            }],
        }];
        assert!(prepare_parameter_automation(&invalid, 48_000.0, 120.0).is_err());

        let duplicate_ids = [
            Vst3ParameterAutomation {
                parameter_id: 17,
                points: vec![Vst3AutomationPoint {
                    position_beats: 0.0,
                    value: 0.0,
                }],
            },
            Vst3ParameterAutomation {
                parameter_id: 17,
                points: vec![Vst3AutomationPoint {
                    position_beats: 1.0,
                    value: 1.0,
                }],
            },
        ];
        assert!(prepare_parameter_automation(&duplicate_ids, 48_000.0, 120.0).is_err());
    }

    #[test]
    fn parameter_automation_rejects_dense_blocks_before_the_host_drops_points() {
        let automation = vec![
            PreparedParameterAutomation {
                parameter_id: 1,
                points: vec![ScheduledParameterPoint {
                    frame: 0,
                    value: 0.0,
                }],
            },
            PreparedParameterAutomation {
                parameter_id: 2,
                points: (1..=MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK)
                    .map(|frame| ScheduledParameterPoint {
                        frame: frame as u64,
                        value: 0.5,
                    })
                    .collect(),
            },
        ];
        let error = validate_parameter_automation_block_density(&automation, 4096, 4097)
            .expect_err("dense block should be rejected");
        assert!(error.contains("parameter changes in one block"));
    }

    #[test]
    fn reported_latency_uses_the_plugin_sample_rate() {
        let info = HostedPluginInfo {
            id: 1,
            name: "Test instrument".to_owned(),
            vendor: "Test vendor".to_owned(),
            version: "1.0".to_owned(),
            category: "Instrument".to_owned(),
            uid: "00000000000000000000000000000000".to_owned(),
            path: PathBuf::from("test.vst3"),
            has_editor: false,
            latency_samples: 480,
            sample_rate_hz: 48_000.0,
        };

        assert_eq!(info.latency_milliseconds(), Some(10.0));
    }

    #[test]
    fn reported_latency_has_no_millisecond_value_for_invalid_sample_rates() {
        let mut info = HostedPluginInfo {
            id: 1,
            name: "Test instrument".to_owned(),
            vendor: "Test vendor".to_owned(),
            version: "1.0".to_owned(),
            category: "Instrument".to_owned(),
            uid: "00000000000000000000000000000000".to_owned(),
            path: PathBuf::from("test.vst3"),
            has_editor: false,
            latency_samples: 480,
            sample_rate_hz: 0.0,
        };

        assert_eq!(info.latency_milliseconds(), None);
        info.sample_rate_hz = f64::NAN;
        assert_eq!(info.latency_milliseconds(), None);
    }

    #[test]
    fn finite_plugin_tail_uses_its_sample_rate_and_obeys_the_render_cap() {
        assert_eq!(reported_tail_seconds(96_000, 48_000.0), Some(2.0));
        assert_eq!(reported_tail_seconds(600_000_000, 48_000.0), Some(60.0));
    }

    #[test]
    fn infinite_or_invalid_plugin_tails_do_not_extend_the_selected_tail() {
        assert_eq!(reported_tail_seconds(u32::MAX, 48_000.0), None);
        assert_eq!(reported_tail_seconds(48_000, 0.0), None);
        assert_eq!(reported_tail_seconds(48_000, f64::NAN), None);
    }

    fn append_flp_vst3_state_record(payload: &mut Vec<u8>, id: u32, data: &[u8]) {
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(data);
    }

    #[test]
    fn converts_flp_component_and_controller_records_to_host_snapshot() {
        let component = [0x10, 0x20, 0x30];
        let controller = [0xA0, 0xB0];
        let mut nested = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut nested, 1, &[0; 64]);
        append_flp_vst3_state_record(&mut nested, 3, &component);
        append_flp_vst3_state_record(&mut nested, 2, &[0x55]);
        append_flp_vst3_state_record(&mut nested, 4, &controller);

        let snapshot = encode_flp_vst3_state_snapshot(&nested).expect("snapshot should convert");
        assert_eq!(&snapshot[..16], VST3_HOST_STATE_MAGIC);
        assert_eq!(read_u32(&snapshot, 16).unwrap(), VST3_HOST_STATE_VERSION);
        assert_eq!(read_u32(&snapshot, 20).unwrap(), component.len() as u32);
        assert_eq!(read_u32(&snapshot, 24).unwrap(), controller.len() as u32);
        assert_eq!(
            &snapshot[VST3_HOST_STATE_HEADER_SIZE..],
            [&component[..], &controller[..]].concat()
        );
    }

    #[test]
    fn converts_missing_controller_to_the_host_no_controller_sentinel() {
        let component = [0x11, 0x22];
        let mut nested = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut nested, 3, &component);

        let snapshot = encode_flp_vst3_state_snapshot(&nested).expect("component should convert");
        assert_eq!(read_u32(&snapshot, 20).unwrap(), component.len() as u32);
        assert_eq!(
            read_u32(&snapshot, 24).unwrap(),
            VST3_HOST_NO_CONTROLLER_STATE
        );
        assert_eq!(&snapshot[VST3_HOST_STATE_HEADER_SIZE..], component);
    }

    fn host_snapshot(component: &[u8], controller: Option<&[u8]>) -> Vec<u8> {
        let mut snapshot = VST3_HOST_STATE_MAGIC.to_vec();
        snapshot.extend_from_slice(&VST3_HOST_STATE_VERSION.to_le_bytes());
        snapshot.extend_from_slice(&(component.len() as u32).to_le_bytes());
        snapshot.extend_from_slice(
            &controller
                .map_or(VST3_HOST_NO_CONTROLLER_STATE, |state| state.len() as u32)
                .to_le_bytes(),
        );
        snapshot.extend_from_slice(component);
        if let Some(controller) = controller {
            snapshot.extend_from_slice(controller);
        }
        snapshot
    }

    #[test]
    fn writes_host_state_back_and_preserves_unknown_flp_records() {
        let mut nested = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut nested, 1, &[0xA1, 0xA2]);
        append_flp_vst3_state_record(&mut nested, 3, &[0x10]);
        append_flp_vst3_state_record(&mut nested, 2, &[0xB1, 0xB2, 0xB3]);
        append_flp_vst3_state_record(&mut nested, 4, &[0x20]);
        append_flp_vst3_state_record(&mut nested, 999, &[0xC1]);
        let snapshot = host_snapshot(&[0x31, 0x32], Some(&[0x41, 0x42]));

        let updated = replace_flp_vst3_state_snapshot(&nested, &snapshot)
            .expect("host state should convert back to FLP records");
        let mut expected = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut expected, 1, &[0xA1, 0xA2]);
        append_flp_vst3_state_record(&mut expected, 3, &[0x31, 0x32]);
        append_flp_vst3_state_record(&mut expected, 2, &[0xB1, 0xB2, 0xB3]);
        append_flp_vst3_state_record(&mut expected, 4, &[0x41, 0x42]);
        append_flp_vst3_state_record(&mut expected, 999, &[0xC1]);
        assert_eq!(updated, expected);
    }

    #[test]
    fn host_state_writeback_adds_or_removes_the_controller_record() {
        let component_only = host_snapshot(&[0x31], None);
        let mut nested = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut nested, 3, &[0x10]);
        let with_controller =
            replace_flp_vst3_state_snapshot(&nested, &host_snapshot(&[0x31], Some(&[0x41])))
                .expect("controller state should be added");
        let mut expected = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut expected, 3, &[0x31]);
        append_flp_vst3_state_record(&mut expected, 4, &[0x41]);
        assert_eq!(with_controller, expected);

        let mut nested_with_controller = nested.clone();
        append_flp_vst3_state_record(&mut nested_with_controller, 4, &[0x20]);
        let without_controller =
            replace_flp_vst3_state_snapshot(&nested_with_controller, &component_only)
                .expect("absent controller state should remove its old FLP record");
        let mut expected_without_controller = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut expected_without_controller, 3, &[0x31]);
        assert_eq!(without_controller, expected_without_controller);
    }

    #[test]
    fn host_state_writeback_rejects_truncated_or_ambiguous_snapshots() {
        let nested = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        assert!(replace_flp_vst3_state_snapshot(&nested, &[]).is_err());

        let mut no_component = nested.clone();
        append_flp_vst3_state_record(&mut no_component, 4, &[0x20]);
        assert!(
            replace_flp_vst3_state_snapshot(&no_component, &host_snapshot(&[1], None)).is_err()
        );

        let mut duplicate = nested;
        append_flp_vst3_state_record(&mut duplicate, 3, &[0x10]);
        append_flp_vst3_state_record(&mut duplicate, 3, &[0x11]);
        assert!(replace_flp_vst3_state_snapshot(&duplicate, &host_snapshot(&[1], None)).is_err());

        let mut truncated_snapshot = host_snapshot(&[1], None);
        truncated_snapshot.pop();
        let valid_nested = {
            let mut bytes = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
            append_flp_vst3_state_record(&mut bytes, 3, &[0x10]);
            bytes
        };
        assert!(replace_flp_vst3_state_snapshot(&valid_nested, &truncated_snapshot).is_err());
    }

    #[test]
    fn rejects_unsupported_truncated_and_ambiguous_flp_vst3_state() {
        assert!(encode_flp_vst3_state_snapshot(&[]).is_err());

        let unsupported = 2u32.to_le_bytes().to_vec();
        assert!(encode_flp_vst3_state_snapshot(&unsupported).is_err());

        let mut missing_component = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut missing_component, 4, &[1]);
        assert!(encode_flp_vst3_state_snapshot(&missing_component).is_err());

        let mut truncated = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        truncated.extend_from_slice(&3u32.to_le_bytes());
        truncated.extend_from_slice(&4u64.to_le_bytes());
        truncated.extend_from_slice(&[1, 2]);
        assert!(encode_flp_vst3_state_snapshot(&truncated).is_err());

        let mut duplicate = FLP_VST3_STATE_MARKER.to_le_bytes().to_vec();
        append_flp_vst3_state_record(&mut duplicate, 3, &[1]);
        append_flp_vst3_state_record(&mut duplicate, 3, &[2]);
        assert!(encode_flp_vst3_state_snapshot(&duplicate).is_err());
    }

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
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0, 0, 128).unwrap();
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
    fn pattern_scheduler_applies_channel_rack_swing_to_note_on_and_off() {
        let note = PatternNote {
            position: 120,
            length: 120,
            channel_id: 3,
            key: 60,
            velocity: 100,
            ..PatternNote::default()
        };
        let (events, note_count) =
            scheduled_pattern_events(&[note], 3, 480, 120.0, 48_000.0, 128, 128).unwrap();

        assert_eq!(note_count, 1);
        assert_eq!(events[0].frame, 8_000);
        assert_eq!(events[1].frame, 14_000);
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
        let (events, _) =
            scheduled_pattern_events(&notes, 3, 480, 120.0, 48_000.0, 0, 128).unwrap();
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
        let error = scheduled_pattern_events(&[note], 2, 480, 120.0, 48_000.0, 0, 128).unwrap_err();
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

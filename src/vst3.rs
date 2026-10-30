//! Runtime hosting for already-installed VST3 plug-ins.
//!
//! The project parser decodes VST identity metadata from recognized FLP envelopes,
//! while preserving their complete raw bytes. This runtime does not translate
//! Image-Line's FLP state envelope into the state stream expected by every VST3.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio::{
    LiveMixerSourceControl, LiveMixerSourceFrameControls, LiveMixerSourceTargets,
    MixerTrackPeakMeters, StereoFrameDelay, StreamingAudioWriter, mixer_effect_wet_mix,
};
use crate::media::DecodedAudio;
use crate::sample_render::{
    AudioFileRenderOptions, PlaylistPatternScheduleOptions, PlaylistRenderOptions,
    PlaylistTempoMap, PlaylistTrackFilter, apply_mixer_master_output_to_stereo, channel_gain_pan,
    channel_pan_gains, project_pan_law, render_stereo_audio_to_file,
    schedule_playlist_pattern_notes_with_adjustments, swing_note_start_tick,
};
use crate::{
    ChannelNoteRouter, ChannelPluginState, FlpDocument, MixerEffectSlotSummary,
    MixerRouteAudibility, MixerSourceRouteTransform, PlaylistClipTarget,
    automation_segment_value_with_tension,
};
use vst3_host::audio::{AudioBuffers, AudioBusLayout, BusAudioBuffers, BusDirection, MediaType};
use vst3_host::midi::{
    MidiChannel, MidiEvent, NoteExpressionInfo, NoteExpressionType, NoteId, PluginEvent,
    PluginEventData,
};
use vst3_host::{Plugin, Vst3Host};

use crate::PatternNote;

static NEXT_RENDER_FILE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_VST3_NOTE_EXPRESSION_ID: AtomicU64 = AtomicU64::new(0);
const VST3_NOTE_EXPRESSION_ID_START: u64 = 1 << 30;
const VST3_NOTE_EXPRESSION_ID_COUNT: u64 = 1 << 30;
const LIVE_VST3_INPUT_TRACK_ID: u32 = u32::MAX;
// A newly attached live session restores controller state on all 16 channels.
const PLAYLIST_LIVE_MIDI_QUEUE_CAPACITY: usize = 1024;
const MAX_STEREO_BUFFER_BYTES: usize = 512 * 1024 * 1024;
const VST3_EFFECT_RENDER_BLOCK_FRAMES: usize = 512;
const MIXER_EFFECT_MIX_SMOOTHING_SECONDS: f32 = 0.02;
const MAX_VST3_EFFECT_LATENCY_SECONDS: f64 = 60.0;
const MAX_VST3_AUTOMATION_POINTS: usize = 1_000_000;
pub(crate) const MAX_PDC_DELAY_BYTES: u64 = 128 * 1024 * 1024;
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
    fine_pitch_offset_cents: i16,
    note_id: i32,
    note_pan: u8,
    note_mod_x: u8,
    note_mod_y: u8,
    note_release: u8,
    layer_gain: f32,
    layer_pan_raw: i32,
    performance_track_id: Option<u32>,
}

#[derive(Clone, Copy)]
struct ScheduledVst3Note {
    note_id: i32,
    note_pan: u8,
    note_mod_x: u8,
    note_mod_y: u8,
    note_release: u8,
    supports_pan: bool,
    supports_volume: bool,
    supports_brightness: bool,
    mod_y_expression: Option<Vst3ModYExpression>,
    layer_gain: f32,
    layer_pan_raw: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Vst3ModYExpression {
    kind: NoteExpressionType,
    min: f64,
    max: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3RenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub output_channels: usize,
    pub notes_rendered: usize,
}

/// Requested effect tail for an offline audio-file render. A finite plug-in-reported tail is
/// included even when this value is shorter; an infinite tail uses this selected duration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3AudioEffectRenderOptions {
    pub tail_seconds: f64,
}

impl Default for Vst3AudioEffectRenderOptions {
    fn default() -> Self {
        Self { tail_seconds: 2.0 }
    }
}

/// Summary for audio processed through one loaded VST3 effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3AudioEffectRenderSummary {
    pub plugin_name: String,
    pub input_frames: u64,
    pub output_frames: u64,
    pub sample_rate: u32,
    pub latency_compensated_frames: u64,
    pub tail_frames: u64,
}

/// One loaded VST3 instance and its target project channel for a summed offline render.
pub struct Vst3PatternChannelRenderRequest {
    pub instance_id: u64,
    pub options: Vst3PatternRenderOptions,
    pub parameter_automation: Vec<Vst3ParameterAutomation>,
}

/// Summary for one summed render of multiple loaded VST3 Pattern channels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vst3PatternMixRenderSummary {
    pub frames: u64,
    pub sample_rate: u32,
    pub channels_rendered: usize,
    pub notes_rendered: usize,
}

struct PreparedPatternRender {
    events: Vec<ScheduledMidiEvent>,
    parameter_automation: Vec<PreparedParameterAutomation>,
    pitch_offset_cents: i16,
    supports_note_pan: bool,
    supports_note_volume: bool,
    supports_note_brightness: bool,
    mod_y_expression: Option<Vst3ModYExpression>,
    note_count: usize,
    sample_rate: f64,
    sample_rate_u32: u32,
    output_channels: usize,
    output_bus_channels: Vec<usize>,
    output_mixer_offsets: Option<Vec<i32>>,
    block_size: usize,
    total_frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScheduledParameterPoint {
    frame: u64,
    value: f64,
    tension: f32,
    performance_track_id: Option<u32>,
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

/// A running VST3 instrument stream that accepts live MIDI events.
pub struct Vst3LiveInputStreamHandle {
    sender: SyncSender<MidiEvent>,
    receiver: Receiver<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl Vst3LiveInputStreamHandle {
    /// Queue one live MIDI event for the next plug-in processing block.
    pub fn try_send(&self, event: MidiEvent) -> Result<(), String> {
        self.sender.try_send(event).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "VST3 live MIDI queue is full".to_owned(),
            mpsc::TrySendError::Disconnected(_) => {
                "VST3 live MIDI worker is no longer available".to_owned()
            }
        })
    }

    /// Poll the live stream worker for a processing error without waiting.
    pub fn try_recv(&self) -> Result<Result<(), String>, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Cancel live processing and send Note Off for any held notes before stopping the plug-in.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether the plug-in processing worker has exited and can be joined without waiting.
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    /// Join the worker after playback has been cancelled or ended.
    pub fn join(self) -> thread::Result<()> {
        self.worker.join()
    }
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
    /// Effective continuous project Master pitch and channel Fine Tune for this channel, in cents.
    pub pitch_offset_cents: i16,
    /// Effective project Master pitch as a note transposition when Add to Key is enabled.
    pub note_pitch_offset_semitones: i16,
}

/// One normalized VST3 parameter value at a pattern-relative beat position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3AutomationPoint {
    pub position_beats: f64,
    pub value: f64,
    /// Approximate single-curve tension for the segment ending at this point.
    pub tension: f32,
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

/// One normalized VST3 parameter value at an absolute Playlist tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3PlaylistAutomationPoint {
    pub tick: u64,
    pub value: f64,
    /// Approximate single-curve tension for the segment ending at this point.
    pub tension: f32,
}

/// Explicit VST3 parameter automation expanded from Pattern events into Playlist time.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst3PlaylistParameterAutomation {
    pub parameter_id: u32,
    pub points: Vec<Vst3PlaylistAutomationPoint>,
}

/// App-linked VST3 automation sources expanded for Playlist playback and render.
#[derive(Clone, Debug, Default)]
pub struct Vst3PlaylistAutomationSources {
    /// Pattern-relative automation keyed by `(pattern_id, VST3 channel_id)`.
    pub pattern_clips: BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    /// Pattern Event automation keyed by `(pattern_id, Mixer insert, effect slot)`.
    pub mixer_effect_pattern_clips:
        BTreeMap<(u16, usize, u16), Vec<Vst3PlaylistParameterAutomation>>,
    /// Automation Clip curves keyed by `(automation_channel_id, VST3 channel_id)`.
    pub automation_clips: BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    /// Automation Clip curves keyed by `(automation_channel_id, Mixer insert, effect slot)`.
    pub mixer_effect_automation_clips: BTreeMap<(u16, usize, u16), Vec<Vst3ParameterAutomation>>,
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
pub(crate) struct PlaylistMidiNote {
    pub(crate) start_tick: u64,
    pub(crate) stop_tick: u64,
    pub(crate) key: u16,
    pub(crate) velocity: u8,
    pub(crate) midi_channel: u8,
    pub(crate) fine_pitch_offset_cents: i16,
    pub(crate) pan: u8,
    pub(crate) mod_x: u8,
    pub(crate) mod_y: u8,
    pub(crate) release: u8,
    pub(crate) layer_gain: f32,
    pub(crate) layer_pan_raw: i32,
}

struct PlaylistPluginStream {
    channel_id: u16,
    plugin_instance_id: u64,
    plugin: Arc<Mutex<Plugin>>,
    live_midi_receiver: Receiver<PlaylistLiveMidiEvent>,
    live_midi_stopped_generation: Arc<AtomicU64>,
    live_midi_active_notes: BTreeMap<(u64, u8, u8), VecDeque<ScheduledVst3Note>>,
    live_midi_sustain_channels: BTreeMap<u64, BTreeSet<u8>>,
    render: PreparedPatternRender,
    output_buses: Vec<PlaylistPluginOutputBus>,
    source_frame: u64,
    next_event: usize,
    resampler_finished: bool,
    latency_frames: u64,
    manual_latency_offset_samples: i32,
    live_performance: bool,
    active_performance_notes: HashMap<i32, ScheduledMidiEvent>,
    active_pattern_note_ids: BTreeSet<i32>,
    note_pitch_offset_semitones: i16,
    pattern_loop: Option<PatternLoopEventQueue>,
}

struct PlaylistPluginOutputBus {
    resampler: StereoStreamResampler,
    pending_output: VecDeque<[f32; 2]>,
    delay_line: StereoFrameDelay,
    gain: f32,
    left_pan_gain: f32,
    right_pan_gain: f32,
    mixer_transform: MixerSourceRouteTransform,
    mixer_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    mixer_effect_input_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    live_mixer_source: Option<PlaylistLiveMixerSource>,
}

impl PlaylistPluginOutputBus {
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

struct PlaylistLiveMixerSource {
    control: LiveMixerSourceControl,
    targets: Option<LiveMixerSourceTargets>,
}

#[derive(Clone, Copy)]
struct PlaylistLiveMidiEvent {
    generation: u64,
    event: MidiEvent,
    note_id: i32,
    layer_gain: f32,
    layer_pan_raw: i32,
}

#[derive(Clone, Copy)]
struct PlaylistLiveNoteContext {
    note_id: i32,
    layer_gain: f32,
    layer_pan_raw: i32,
}

type PlaylistLiveNoteState = HashMap<(u8, u8), VecDeque<PlaylistLiveNoteContext>>;

/// Routes live MIDI into a VST3 instrument already processing Playlist audio.
#[derive(Clone)]
pub struct Vst3PlaylistLiveInputSender {
    sender: SyncSender<PlaylistLiveMidiEvent>,
    next_generation: Arc<AtomicU64>,
    stopped_generation: Arc<AtomicU64>,
}

/// One live MIDI audition session routed through a Playlist VST3 processor.
pub struct Vst3PlaylistLiveInputSession {
    sender: Vst3PlaylistLiveInputSender,
    generation: u64,
    active_notes: Mutex<PlaylistLiveNoteState>,
}

impl Vst3PlaylistLiveInputSender {
    pub fn begin_session(&self) -> Vst3PlaylistLiveInputSession {
        let generation = self
            .next_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        Vst3PlaylistLiveInputSession {
            sender: self.clone(),
            generation,
            active_notes: Mutex::new(HashMap::new()),
        }
    }
}

impl Vst3PlaylistLiveInputSession {
    /// Queue one live MIDI event for the next Playlist plug-in processing block.
    pub fn try_send(&self, event: MidiEvent) -> Result<(), String> {
        self.try_send_with_layer_adjustments(event, 1.0, 0)
    }

    /// Queue a live MIDI event with Layer gain and pan for the next processing block.
    pub fn try_send_with_layer_adjustments(
        &self,
        mut event: MidiEvent,
        layer_gain: f32,
        layer_pan_raw: i32,
    ) -> Result<(), String> {
        if let MidiEvent::NoteOn {
            channel,
            note,
            velocity: 0,
        } = event
        {
            event = MidiEvent::NoteOff {
                channel,
                note,
                velocity: 0,
            };
        }

        let mut note_id = 0;
        let mut event_layer_gain = 1.0;
        let mut event_layer_pan_raw = 0;
        let note_key = match event {
            MidiEvent::NoteOn {
                channel,
                note,
                velocity,
            } if velocity > 0 => {
                note_id = next_vst3_note_expression_id();
                event_layer_gain = layer_gain;
                event_layer_pan_raw = layer_pan_raw;
                Some((channel.as_index(), note, true))
            }
            MidiEvent::NoteOff { channel, note, .. } => Some((channel.as_index(), note, false)),
            _ => None,
        };
        let paired_note = if let Some((channel, note, false)) = note_key {
            self.active_notes
                .lock()
                .map_err(|_| "VST3 live MIDI note state lock was poisoned".to_owned())?
                .get_mut(&(channel, note))
                .and_then(VecDeque::pop_front)
        } else {
            None
        };
        if let Some(paired_note) = paired_note {
            note_id = paired_note.note_id;
            event_layer_gain = paired_note.layer_gain;
            event_layer_pan_raw = paired_note.layer_pan_raw;
        }
        self.sender
            .sender
            .try_send(PlaylistLiveMidiEvent {
                generation: self.generation,
                event,
                note_id,
                layer_gain: event_layer_gain,
                layer_pan_raw: event_layer_pan_raw,
            })
            .map_err(|error| {
                if let Some((channel, note, false)) = note_key
                    && let Some(paired_note) = paired_note
                    && let Ok(mut active_notes) = self.active_notes.lock()
                {
                    active_notes
                        .entry((channel, note))
                        .or_default()
                        .push_front(paired_note);
                }
                match error {
                    mpsc::TrySendError::Full(_) => "VST3 live MIDI queue is full".to_owned(),
                    mpsc::TrySendError::Disconnected(_) => {
                        "Playlist VST3 processor is no longer available".to_owned()
                    }
                }
            })?;

        if let Some((channel, note, true)) = note_key {
            self.active_notes
                .lock()
                .map_err(|_| "VST3 live MIDI note state lock was poisoned".to_owned())?
                .entry((channel, note))
                .or_default()
                .push_back(PlaylistLiveNoteContext {
                    note_id,
                    layer_gain,
                    layer_pan_raw,
                });
        }
        if let MidiEvent::ControlChange {
            channel,
            controller: 120 | 123..=127,
            ..
        } = event
        {
            let channel = channel.as_index();
            self.active_notes
                .lock()
                .map_err(|_| "VST3 live MIDI note state lock was poisoned".to_owned())?
                .retain(|(active_channel, _), _| *active_channel != channel);
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.sender
            .stopped_generation
            .fetch_max(self.generation, Ordering::AcqRel);
    }
}

impl Drop for Vst3PlaylistLiveInputSession {
    fn drop(&mut self) {
        self.stop();
    }
}

struct PatternLoopEventQueue {
    loop_start_frame: u64,
    loop_frames: u64,
    first_event_frame: u64,
    template_events: Vec<ScheduledMidiEvent>,
    note_ids: HashMap<i32, i32>,
    repeated_events: Vec<ScheduledMidiEvent>,
    next_cycle: u64,
}

impl PatternLoopEventQueue {
    fn new(
        loop_start_frame: u64,
        loop_frames: u64,
        template_events: Vec<ScheduledMidiEvent>,
    ) -> Self {
        let first_event_frame = template_events
            .iter()
            .map(|event| event.frame)
            .min()
            .unwrap_or(loop_start_frame);
        let mut note_ids = HashMap::with_capacity(template_events.len());
        for event in &template_events {
            note_ids.entry(event.note_id).or_insert(0);
        }
        Self {
            loop_start_frame,
            loop_frames,
            first_event_frame,
            repeated_events: Vec::with_capacity(template_events.len()),
            template_events,
            note_ids,
            next_cycle: 1,
        }
    }

    fn repeat_cycle(&mut self, cycle_offset: u64) -> Result<(), String> {
        self.repeated_events.clear();
        for note_id in self.note_ids.values_mut() {
            *note_id = next_vst3_note_expression_id();
        }
        for template_event in &self.template_events {
            let mut event = *template_event;
            event.frame = event
                .frame
                .checked_add(cycle_offset)
                .ok_or_else(|| "VST3 Pattern event position overflow".to_owned())?;
            event.note_id = *self
                .note_ids
                .get(&template_event.note_id)
                .ok_or_else(|| "VST3 Pattern event note ID was not indexed".to_owned())?;
            self.repeated_events.push(event);
        }
        Ok(())
    }

    fn replace_template_events(
        &mut self,
        template_events: Vec<ScheduledMidiEvent>,
        next_cycle: u64,
    ) {
        self.first_event_frame = template_events
            .iter()
            .map(|event| event.frame)
            .min()
            .unwrap_or(self.loop_start_frame);
        self.note_ids.clear();
        self.note_ids.reserve(template_events.len());
        for event in &template_events {
            self.note_ids.entry(event.note_id).or_insert(0);
        }
        self.repeated_events.clear();
        self.template_events = template_events;
        self.next_cycle = next_cycle;
    }
}

/// One prepared Mixer VST3 effect slot.
/// The plug-in instance is shared with its native editor.
struct LiveWetMixRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining_frames: u32,
    smoothing_frames: u32,
}

impl LiveWetMixRamp {
    fn new(initial_mix: f32, smoothing_frames: u32) -> Self {
        Self {
            current: initial_mix,
            target: initial_mix,
            step: 0.0,
            remaining_frames: 0,
            smoothing_frames: smoothing_frames.max(1),
        }
    }

    fn set_target(&mut self, target: f32) {
        let target = if target.is_finite() {
            target.clamp(0.0, 1.0)
        } else {
            self.target
        };
        if target == self.target {
            return;
        }
        self.target = target;
        self.remaining_frames = self.smoothing_frames;
        self.step = (target - self.current) / self.remaining_frames as f32;
    }

    fn next(&mut self) -> f32 {
        if self.remaining_frames > 0 {
            self.current += self.step;
            self.remaining_frames -= 1;
            if self.remaining_frames == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}

struct PlaylistMixerEffectStream {
    id: u64,
    insert_ordinal: usize,
    slot_index: u16,
    plugin: Option<Arc<Mutex<Plugin>>>,
    external_processor: Option<Box<dyn MixerEffectProcessor>>,
    buffers: Option<BusAudioBuffers>,
    block_size: usize,
    input_channels: usize,
    output_channels: usize,
    external_auxiliary_input_buses: Vec<(usize, usize)>,
    external_auxiliary_input_source_map: Option<Vec<(usize, usize)>>,
    external_auxiliary_scratch: Vec<f32>,
    sidechain_inputs: Vec<PlaylistMixerEffectSidechainInput>,
    parameter_automation: Vec<PreparedParameterAutomation>,
    parameter_automation_loop_frames: Option<u64>,
    parameter_automation_loop_start_frame: u64,
    wet_mix: f32,
    saved_enabled: bool,
    saved_insert_effects_enabled: bool,
    live_wet_mix_target: Option<Arc<AtomicU32>>,
    live_enabled_target: Option<Arc<AtomicI32>>,
    live_insert_effects_enabled_target: Option<Arc<AtomicI32>>,
    wet_mix_ramp: LiveWetMixRamp,
    latency_frames: u64,
    tail_frames: u64,
    dry_delay_required: bool,
    manual_latency_offset_samples: i32,
    dry_delay_line: StereoFrameDelay,
    dry_delay_scratch: Vec<f32>,
    wet_scratch: Vec<f32>,
}

struct PlaylistMixerEffectSidechainInput {
    bus_index: usize,
    source_track: usize,
    channel_count: usize,
    saved_enabled: bool,
    live_control: bool,
    live_enabled_target: Option<Arc<AtomicI32>>,
    enabled_ramp: LiveWetMixRamp,
    delay: StereoFrameDelay,
    stereo_scratch: Vec<f32>,
}

impl PlaylistMixerEffectStream {
    fn begin_wet_mix_block(&mut self) {
        if self.live_wet_mix_target.is_none()
            && self.live_enabled_target.is_none()
            && self.live_insert_effects_enabled_target.is_none()
        {
            return;
        }
        let requested_mix = self
            .live_wet_mix_target
            .as_ref()
            .map(|target| f32::from_bits(target.load(Ordering::Acquire)))
            .filter(|mix| mix.is_finite())
            .map_or(self.wet_mix, |mix| mix.clamp(0.0, 1.0));
        let slot_enabled = self
            .live_enabled_target
            .as_ref()
            .map_or(self.saved_enabled, |target| {
                target.load(Ordering::Acquire) != 0
            });
        let insert_effects_enabled = self
            .live_insert_effects_enabled_target
            .as_ref()
            .map_or(self.saved_insert_effects_enabled, |target| {
                target.load(Ordering::Acquire) != 0
            });
        self.wet_mix_ramp
            .set_target(if slot_enabled && insert_effects_enabled {
                requested_mix
            } else {
                0.0
            });
    }
}

struct PlaylistMixerEffectChain {
    insert_ordinal: usize,
    route_plan: crate::MixerSourceRoutePlan,
    live_source: Option<PlaylistMixerEffectLiveSource>,
    post_effect_meter_transform: MixerSourceRouteTransform,
    first_effect_index: usize,
    end_effect_index: usize,
    latency_frames: u64,
    output_alignment_delay: StereoFrameDelay,
    dry_reference_delay: StereoFrameDelay,
    downstream_delays: Vec<PlaylistMixerEffectBranchDelay>,
}

struct PlaylistMixerEffectLiveSource {
    control: LiveMixerSourceControl,
    targets: Option<LiveMixerSourceTargets>,
}

impl PlaylistMixerEffectLiveSource {
    fn next_frame_controls(&self) -> Option<LiveMixerSourceFrameControls> {
        self.targets
            .as_ref()
            .map(|targets| self.control.next_frame_controls(targets))
    }

    fn insert_meter_transform(
        &self,
        track: usize,
        fallback: MixerSourceRouteTransform,
    ) -> MixerSourceRouteTransform {
        self.control
            .insert_meter_transforms()
            .iter()
            .find(|(meter_track, _)| *meter_track == track)
            .map_or(fallback, |(_, transform)| *transform)
    }

    fn insert_effect_input_transform(
        &self,
        track: usize,
        fallback: MixerSourceRouteTransform,
    ) -> MixerSourceRouteTransform {
        self.control
            .insert_effect_input_transforms()
            .iter()
            .find(|(input_track, _)| *input_track == track)
            .map_or(fallback, |(_, transform)| *transform)
    }

    fn apply_meter_frame(
        &self,
        controls: LiveMixerSourceFrameControls,
        track: usize,
        fallback: MixerSourceRouteTransform,
        frame: &mut [f32; 2],
    ) {
        let route_transform = self.insert_meter_transform(track, fallback);
        self.control
            .apply_meter_frame(controls, track, route_transform, frame);
    }

    fn apply_effect_input_frame(
        &self,
        controls: LiveMixerSourceFrameControls,
        track: usize,
        fallback: MixerSourceRouteTransform,
        frame: &mut [f32; 2],
    ) {
        let route_transform = self.insert_effect_input_transform(track, fallback);
        self.control
            .apply_effect_input_frame(controls, track, route_transform, frame);
    }
}

impl PlaylistMixerEffectChain {
    fn next_live_frame_controls(&self) -> Option<LiveMixerSourceFrameControls> {
        self.live_source
            .as_ref()
            .and_then(PlaylistMixerEffectLiveSource::next_frame_controls)
    }
}

fn route_playlist_mixer_effect_output_delta(
    output_delay: &mut StereoFrameDelay,
    route_plan: &crate::MixerSourceRoutePlan,
    live_source: Option<&PlaylistMixerEffectLiveSource>,
    mut frame: [f32; 2],
    live_controls: Option<LiveMixerSourceFrameControls>,
) -> [f32; 2] {
    output_delay.process_frame(&mut frame);
    if let (Some(source), Some(controls)) = (live_source, live_controls) {
        source.control.apply_output_frame(controls, &mut frame);
    } else {
        route_plan.output_transform.apply_frame(&mut frame);
    }
    frame
}

struct PlaylistMixerEffectBranchDelay {
    track: usize,
    meter_transform: MixerSourceRouteTransform,
    effect_input_transform: MixerSourceRouteTransform,
    meter_delay: StereoFrameDelay,
    effect_input_delay: StereoFrameDelay,
}

/// Prepared VST3 instrument channels and Mixer effects for blockwise Playlist transport.
/// The instances are the same objects used by the open editors.
pub struct Vst3PlaylistStreamProcessor {
    streams: Vec<PlaylistPluginStream>,
    mixer_effects: Vec<PlaylistMixerEffectStream>,
    mixer_effect_chains: Vec<PlaylistMixerEffectChain>,
    mixer_effect_scratch: Vec<f32>,
    mixer_effect_dry_scratch: Vec<f32>,
    mixer_effect_post_effect_bus_scratch: Vec<f32>,
    mixer_effect_tail_frames: u64,
    mixer_effect_dry_delay_frames: u64,
    mixer_effect_route_delay_frames: u64,
    mixer_effect_pdc_latency_frames: u64,
    mixer_effect_output_delay: StereoFrameDelay,
    mixer_effect_frame: u64,
    mixer_effect_meter_input_delays: Vec<StereoFrameDelay>,
    mixer_effect_input_delays: Vec<StereoFrameDelay>,
    mixer_post_effect_stem_delays: Vec<StereoFrameDelay>,
    mixer_routing: MixerRouteAudibility,
    include_effect_tails: bool,
    live_input_senders: BTreeMap<u16, Vst3PlaylistLiveInputSender>,
    volume_expression_channel_ids: BTreeSet<u16>,
    performance_pattern_automation: BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    tempo_map: PlaylistTempoMap,
    output_sample_rate: u32,
    content_frames: u64,
    output_frames: u64,
    maximum_latency_frames: u64,
    automatic_pdc_enabled: bool,
    notes_scheduled: usize,
    unloaded_plugin_channels: Vec<u16>,
    started_count: usize,
    started_mixer_effect_count: usize,
}

impl Vst3PlaylistStreamProcessor {
    pub fn live_midi_input_sender(&self, channel_id: u16) -> Option<Vst3PlaylistLiveInputSender> {
        self.live_input_senders.get(&channel_id).cloned()
    }

    pub fn live_midi_input_senders(&self) -> BTreeMap<u16, Vst3PlaylistLiveInputSender> {
        self.live_input_senders.clone()
    }

    pub(crate) fn update_live_pattern_notes(
        &mut self,
        document: &FlpDocument,
        pattern_notes: &[PatternNote],
        clap_audio_channels: &BTreeSet<u16>,
        output_frame: u64,
    ) -> Result<(), String> {
        let prepared_channel_bus_counts = self
            .streams
            .iter()
            .filter(|stream| stream.pattern_loop.is_some())
            .map(|stream| (stream.channel_id, stream.render.output_bus_channels.len()))
            .collect::<BTreeMap<_, _>>();
        if prepared_channel_bus_counts.is_empty() {
            return Ok(());
        }
        let ppq = document.header().ppq();
        if ppq == 0 {
            return Err("project PPQ must be greater than zero".to_owned());
        }
        let global_swing_mix_raw = document.metadata().global_swing_mix();
        let channels = document.channels();
        let channels_by_id = channels
            .iter()
            .map(|channel| (channel.id(), channel))
            .collect::<BTreeMap<_, _>>();
        let mut output_mixer_offsets_by_channel =
            flp_vst3_output_mixer_offsets_by_channel(document);
        output_mixer_offsets_by_channel.retain(|channel_id, offsets| {
            prepared_channel_bus_counts.get(channel_id) == Some(&offsets.len())
        });
        let router = ChannelNoteRouter::new(channels.clone());
        let mixer_route_audibility = MixerRouteAudibility::from_document(document);
        let mut notes_by_channel = BTreeMap::<u16, Vec<PlaylistMidiNote>>::new();

        for (note_index, note) in pattern_notes.iter().enumerate() {
            if note.is_muted() {
                continue;
            }
            let nominal_start_tick = u64::from(note.position);
            let source_swing_mix = channels_by_id
                .get(&note.channel_id)
                .map_or(128, |channel| channel.swing_mix());
            let start_tick = swing_note_start_tick(
                nominal_start_tick,
                ppq,
                global_swing_mix_raw,
                source_swing_mix,
            )?;
            let swing_offset = start_tick.saturating_sub(nominal_start_tick);
            let stop_tick = nominal_start_tick
                .checked_add(u64::from(note.length.max(1)))
                .and_then(|tick| tick.checked_add(swing_offset))
                .ok_or_else(|| "Pattern VST3 note end position overflow".to_owned())?;
            for (target_channel_id, layer_gain, layer_pan_raw) in router
                .targets_for_note_with_clap_and_mixed_crossfade(
                    note.channel_id,
                    note_index as u64,
                    note.key,
                    &self.volume_expression_channel_ids,
                    clap_audio_channels,
                )
            {
                if !prepared_channel_bus_counts.contains_key(&target_channel_id)
                    || !channels_by_id
                        .get(&target_channel_id)
                        .is_some_and(|channel| {
                            channel.enabled() != Some(false)
                                && vst3_channel_has_audible_output_route(
                                    &mixer_route_audibility,
                                    channel.mixer_track(),
                                    output_mixer_offsets_by_channel
                                        .get(&target_channel_id)
                                        .map(Vec::as_slice),
                                    true,
                                )
                        })
                {
                    continue;
                }
                notes_by_channel
                    .entry(target_channel_id)
                    .or_default()
                    .push(PlaylistMidiNote {
                        mod_y: note.mod_y,
                        start_tick,
                        stop_tick,
                        key: note.key,
                        velocity: note.velocity,
                        midi_channel: note.midi_channel,
                        fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
                        pan: note.pan,
                        mod_x: note.mod_x,
                        release: note.release,
                        layer_gain,
                        layer_pan_raw,
                    });
            }
        }

        let mut replacement_events_by_channel = BTreeMap::new();
        let mut notes_scheduled = 0usize;
        for stream in &self.streams {
            if stream.pattern_loop.is_none() {
                continue;
            }
            let notes = notes_by_channel
                .get(&stream.channel_id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let (mut events, note_count) = scheduled_playlist_midi_events_with_tempo_map(
                notes,
                &self.tempo_map,
                stream.render.sample_rate,
            )?;
            transpose_scheduled_note_events(&mut events, stream.note_pitch_offset_semitones);
            notes_scheduled = notes_scheduled.saturating_add(note_count);
            replacement_events_by_channel.insert(stream.channel_id, events);
        }

        for stream in &mut self.streams {
            let Some(queue) = stream.pattern_loop.as_mut() else {
                continue;
            };
            let Some(template_events) = replacement_events_by_channel.remove(&stream.channel_id)
            else {
                continue;
            };
            let current_frame = playlist_output_frame_to_plugin_frame(
                output_frame,
                stream.render.sample_rate,
                self.output_sample_rate,
            )?
            .max(stream.source_frame);
            let loop_frames = queue.loop_frames;
            if loop_frames == 0 {
                continue;
            }
            let cycle_offset =
                current_frame.saturating_sub(queue.loop_start_frame) / loop_frames * loop_frames;
            let current_loop_start = queue
                .loop_start_frame
                .checked_add(cycle_offset)
                .ok_or_else(|| "VST3 Pattern loop position overflow".to_owned())?;
            let note_on_ids = template_events
                .iter()
                .filter(|event| {
                    event.frame >= current_frame.saturating_sub(current_loop_start)
                        && matches!(event.event, MidiEvent::NoteOn { velocity, .. } if velocity > 0)
                })
                .map(|event| event.note_id)
                .collect::<BTreeSet<_>>();
            let current_events = template_events
                .iter()
                .filter(|event| note_on_ids.contains(&event.note_id))
                .map(|event| {
                    let mut event = *event;
                    event.frame = event
                        .frame
                        .checked_add(current_loop_start)
                        .ok_or_else(|| "VST3 Pattern event position overflow".to_owned())?;
                    Ok(event)
                })
                .collect::<Result<Vec<_>, String>>()?;

            let pending_non_note_on_events = stream
                .render
                .events
                .drain(stream.next_event..)
                .filter(|event| match event.event {
                    MidiEvent::NoteOn { .. } => false,
                    MidiEvent::NoteOff { .. } => {
                        stream.active_pattern_note_ids.contains(&event.note_id)
                    }
                    _ => true,
                })
                .collect::<Vec<_>>();
            stream.render.events.extend(pending_non_note_on_events);
            stream.render.events.extend(current_events);
            stream.render.events[stream.next_event..]
                .sort_unstable_by_key(|event| (event.frame, event.priority));
            let current_cycle = cycle_offset / loop_frames;
            queue.replace_template_events(template_events, current_cycle.saturating_add(1));
            stream.render.note_count = notes_by_channel.get(&stream.channel_id).map_or(0, Vec::len);
        }
        self.notes_scheduled = notes_scheduled;
        Ok(())
    }

    pub(crate) fn set_playlist_loop_range(
        &mut self,
        start_frame: u64,
        end_frame: u64,
    ) -> Result<(), String> {
        if end_frame <= start_frame {
            return Err("Playlist VST3 loop end must be after its start".to_owned());
        }
        if self.started_count != 0 || self.started_mixer_effect_count != 0 {
            return Err("Playlist VST3 loop range must be set before processing starts".to_owned());
        }
        let output_sample_rate = self.output_sample_rate;
        for stream in &mut self.streams {
            if stream.pattern_loop.is_some() {
                return Err(
                    "Playlist time selection looping cannot be combined with a Pattern loop"
                        .to_owned(),
                );
            }
            if stream.next_event != 0 || stream.source_frame != 0 {
                return Err("Playlist VST3 loop range must be set before rendering".to_owned());
            }
            let source_start = playlist_output_frame_to_plugin_frame(
                start_frame,
                stream.render.sample_rate,
                output_sample_rate,
            )?;
            let source_end = playlist_output_frame_to_plugin_frame(
                end_frame,
                stream.render.sample_rate,
                output_sample_rate,
            )?;
            if source_end <= source_start {
                return Err("Playlist VST3 loop is shorter than one plug-in frame".to_owned());
            }
            validate_looped_parameter_automation_block_density(
                &stream.render.parameter_automation,
                stream.render.block_size,
                source_end - source_start,
            )?;
            let (initial_events, template_events) =
                playlist_loop_midi_events(&stream.render.events, source_start, source_end);
            stream.render.events = initial_events;
            stream.pattern_loop = Some(PatternLoopEventQueue::new(
                source_start,
                source_end - source_start,
                template_events,
            ));
            stream.live_performance = true;
        }
        for effect in &mut self.mixer_effects {
            validate_looped_parameter_automation_block_density(
                &effect.parameter_automation,
                effect.block_size,
                end_frame - start_frame,
            )?;
            effect.parameter_automation_loop_start_frame = start_frame;
            effect.parameter_automation_loop_frames = Some(end_frame - start_frame);
        }
        Ok(())
    }

    pub(crate) fn set_live_mixer_source_targets(&mut self, meters: &MixerTrackPeakMeters) {
        for stream in &mut self.streams {
            for bus in &mut stream.output_buses {
                if let Some(source) = bus.live_mixer_source.as_mut() {
                    source.targets = source.control.targets(meters);
                }
            }
        }
        for chain in &mut self.mixer_effect_chains {
            if let Some(source) = chain.live_source.as_mut() {
                source.targets = source.control.targets(meters);
            }
        }
        for effect in &mut self.mixer_effects {
            effect.live_wet_mix_target = meters.live_mixer_effect_mix_target(
                effect.insert_ordinal,
                effect.slot_index,
                effect.wet_mix,
            );
            effect.live_enabled_target = meters.live_mixer_effect_slot_enabled_target(
                effect.insert_ordinal,
                effect.slot_index,
                effect.saved_enabled,
            );
            effect.live_insert_effects_enabled_target = meters
                .live_mixer_insert_effects_enabled_target(
                    effect.insert_ordinal,
                    effect.saved_insert_effects_enabled,
                );
            for sidechain in &mut effect.sidechain_inputs {
                sidechain.live_enabled_target = sidechain
                    .live_control
                    .then(|| {
                        meters
                            .live_mixer_send_targets(sidechain.source_track, effect.insert_ordinal)
                            .map(|targets| Arc::clone(&targets.enabled))
                    })
                    .flatten();
            }
        }
    }

    pub(crate) fn apply_live_mixer_routing(
        &mut self,
        document: &FlpDocument,
        meters: Option<&MixerTrackPeakMeters>,
    ) -> Result<(), String> {
        self.automatic_pdc_enabled = document.mixer_automatic_pdc_enabled().unwrap_or(true);
        let routing = MixerRouteAudibility::from_document(document);
        self.mixer_routing = routing.clone();
        let channels_by_id = document
            .channels()
            .into_iter()
            .map(|channel| (channel.id(), channel))
            .collect::<HashMap<_, _>>();
        for stream in &mut self.streams {
            let Some(channel) = channels_by_id.get(&stream.channel_id) else {
                continue;
            };
            for (bus_index, bus) in stream.output_buses.iter_mut().enumerate() {
                let mixer_route = vst3_output_mixer_route(
                    channel.mixer_track(),
                    stream
                        .render
                        .output_mixer_offsets
                        .as_ref()
                        .and_then(|offsets| offsets.get(bus_index).copied()),
                );
                let route_plan = routing.source_route_plan_for_channel(mixer_route);
                bus.mixer_transform = route_plan.output_transform;
                bus.mixer_meter_transforms = route_plan.insert_meter_transforms;
                bus.mixer_effect_input_transforms = route_plan.insert_effect_input_transforms;
                bus.live_mixer_source = routing
                    .live_source_route_plan_for_channel(mixer_route)
                    .map(|route_plan| PlaylistLiveMixerSource {
                        control: LiveMixerSourceControl::new(route_plan, self.output_sample_rate),
                        targets: None,
                    });
            }
        }
        for effect in &mut self.mixer_effects {
            let sources = routing.mixer_sidechain_sources_for_insert(effect.insert_ordinal);
            update_playlist_mixer_effect_sidechain_inputs(
                effect,
                &sources,
                f64::from(self.output_sample_rate),
            )?;
        }
        for chain in &mut self.mixer_effect_chains {
            if chain.insert_ordinal == 0 {
                continue;
            }
            let track = i8::try_from(chain.insert_ordinal).map_err(|_| {
                format!(
                    "Mixer insert ordinal {} exceeds the supported range",
                    chain.insert_ordinal
                )
            })?;
            let route_plan = routing.source_route_plan_for_channel(Some(track));
            chain.post_effect_meter_transform = route_plan
                .insert_meter_transforms
                .iter()
                .find(|(meter_track, _)| *meter_track == chain.insert_ordinal)
                .map_or_else(MixerSourceRouteTransform::default, |(_, transform)| {
                    *transform
                });
            chain.live_source =
                routing
                    .live_source_route_plan_for_channel(Some(track))
                    .map(|route_plan| PlaylistMixerEffectLiveSource {
                        control: LiveMixerSourceControl::new(route_plan, self.output_sample_rate),
                        targets: None,
                    });
            chain.route_plan = route_plan;
        }
        let instrument_delay_frames = if self.automatic_pdc_enabled {
            self.streams
                .iter()
                .try_fold(self.maximum_latency_frames, |total, stream| {
                    total
                        .checked_add(
                            self.maximum_latency_frames
                                .saturating_sub(stream.latency_frames),
                        )
                        .ok_or_else(|| "PDC delay storage size overflow".to_owned())
                })?
        } else {
            0
        };
        configure_mixer_effect_pdc(self, &routing, instrument_delay_frames)?;
        self.extend_to_output_frames(self.content_frames)?;
        if let Some(meters) = meters {
            self.set_live_mixer_source_targets(meters);
        }
        Ok(())
    }

    fn refresh_external_mixer_effect_reports(&mut self) -> Result<bool, String> {
        let mut latency_changed = false;
        let mut tail_changed = false;
        let max_total_tail_frames = u64::from(self.output_sample_rate)
            .saturating_mul(MAX_REPORTED_TAIL_SECONDS.ceil() as u64);
        let mut total_tail_frames = 0_u64;

        for effect in &mut self.mixer_effects {
            let Some(external_processor) = effect.external_processor.as_ref() else {
                total_tail_frames = total_tail_frames.saturating_add(effect.tail_frames);
                continue;
            };
            let sample_rate = external_processor.sample_rate();
            if !sample_rate.is_finite() || sample_rate <= 0.0 {
                return Err(format!(
                    "CLAP Mixer insert {} effect slot {} reported an invalid sample rate",
                    effect.insert_ordinal,
                    effect.slot_index + 1
                ));
            }
            let latency_frames = u64::from(external_processor.latency_samples());
            let maximum_latency_frames =
                (sample_rate * MAX_VST3_EFFECT_LATENCY_SECONDS).ceil() as u64;
            if latency_frames > maximum_latency_frames {
                return Err(format!(
                    "CLAP Mixer insert {} effect slot {} reports {latency_frames} frames of latency, exceeding the {MAX_VST3_EFFECT_LATENCY_SECONDS}-second processing limit",
                    effect.insert_ordinal,
                    effect.slot_index + 1
                ));
            }
            if latency_frames != effect.latency_frames {
                effect.latency_frames = latency_frames;
                latency_changed = true;
            }

            let tail_frames = if self.include_effect_tails {
                external_processor
                    .tail_samples()
                    .map(|tail_samples| {
                        ((tail_samples as f64 / sample_rate).min(MAX_REPORTED_TAIL_SECONDS)
                            * f64::from(self.output_sample_rate))
                        .ceil() as u64
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            if tail_frames != effect.tail_frames {
                effect.tail_frames = tail_frames;
                tail_changed = true;
            }
            total_tail_frames = total_tail_frames.saturating_add(effect.tail_frames);
        }

        if !latency_changed && !tail_changed {
            return Ok(false);
        }

        self.mixer_effect_tail_frames = total_tail_frames.min(max_total_tail_frames);
        if latency_changed {
            let chain_latencies = self
                .mixer_effect_chains
                .iter()
                .map(|chain| {
                    self.mixer_effects[chain.first_effect_index..chain.end_effect_index]
                        .iter()
                        .try_fold(0_u64, |total, effect| {
                            total
                                .checked_add(effect.latency_frames)
                                .ok_or_else(|| "Mixer effect chain latency overflow".to_owned())
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            for (chain, latency_frames) in self.mixer_effect_chains.iter_mut().zip(chain_latencies)
            {
                chain.latency_frames = latency_frames;
            }

            let mut dry_delay_frames = 0_u64;
            for effect in &mut self.mixer_effects {
                let delay_frames = if effect.dry_delay_required {
                    effect.latency_frames
                } else {
                    0
                };
                dry_delay_frames = dry_delay_frames
                    .checked_add(delay_frames)
                    .ok_or_else(|| "Mixer effect PDC delay size overflow".to_owned())?;
                effect.dry_delay_line.set_delay_frames(delay_frames)?;
            }
            self.mixer_effect_dry_delay_frames = dry_delay_frames;

            let instrument_delay_frames = if self.automatic_pdc_enabled {
                self.streams
                    .iter()
                    .try_fold(self.maximum_latency_frames, |total, stream| {
                        total
                            .checked_add(
                                self.maximum_latency_frames
                                    .saturating_sub(stream.latency_frames),
                            )
                            .ok_or_else(|| "PDC delay storage size overflow".to_owned())
                    })?
            } else {
                0
            };
            let routing = self.mixer_routing.clone();
            configure_mixer_effect_pdc(self, &routing, instrument_delay_frames)?;
        }
        self.extend_to_output_frames(self.content_frames)?;
        Ok(true)
    }

    /// Refresh plug-in latency reports at a stream block boundary and rebuild PDC while
    /// preserving the active streams and queued delay-line history.
    pub(crate) fn refresh_live_latency_reports(
        &mut self,
        plugin_ids: &BTreeSet<u64>,
        document: &FlpDocument,
        meters: Option<&MixerTrackPeakMeters>,
    ) -> Result<Vec<Vst3LatencyReport>, String> {
        let mut reports = Vec::with_capacity(self.streams.len() + self.mixer_effects.len());
        for stream in &mut self.streams {
            if !plugin_ids.contains(&stream.plugin_instance_id) {
                continue;
            }
            let report = refresh_plugin_latency_report(stream.plugin_instance_id, &stream.plugin)?;
            let pdc_latency_samples = apply_manual_latency_offset(
                report.latency_samples,
                stream.manual_latency_offset_samples,
            );
            stream.latency_frames = plugin_latency_in_output_frames(
                stream.channel_id,
                pdc_latency_samples,
                report.sample_rate_hz,
                self.output_sample_rate,
            )?;
            let stream_frames = self
                .content_frames
                .checked_add(stream.latency_frames)
                .ok_or_else(|| "VST3 PDC stream length overflow".to_owned())?;
            let source_frames = (stream_frames as f64 * stream.render.sample_rate
                / f64::from(self.output_sample_rate))
            .ceil();
            if !source_frames.is_finite() || source_frames > u64::MAX as f64 {
                return Err(
                    "VST3 Playlist stream length is outside the renderable range".to_owned(),
                );
            }
            stream.render.total_frames = source_frames as u64;
            reports.push(report);
        }
        for effect in &mut self.mixer_effects {
            let Some(plugin) = &effect.plugin else {
                continue;
            };
            if !plugin_ids.contains(&effect.id) {
                continue;
            }
            let report = refresh_plugin_latency_report(effect.id, plugin)?;
            if !report.sample_rate_hz.is_finite() || report.sample_rate_hz <= 0.0 {
                return Err(format!(
                    "VST3 Mixer insert {} effect slot {} reports an invalid sample rate",
                    effect.insert_ordinal,
                    effect.slot_index + 1
                ));
            }
            let pdc_latency_samples = apply_manual_latency_offset(
                report.latency_samples,
                effect.manual_latency_offset_samples,
            );
            let maximum_latency_frames =
                (report.sample_rate_hz * MAX_VST3_EFFECT_LATENCY_SECONDS).ceil() as u64;
            if u64::from(pdc_latency_samples) > maximum_latency_frames {
                return Err(format!(
                    "VST3 Mixer insert {} effect slot {} reports {} frames of latency, exceeding the {MAX_VST3_EFFECT_LATENCY_SECONDS}-second processing limit",
                    effect.insert_ordinal,
                    effect.slot_index + 1,
                    pdc_latency_samples
                ));
            }
            effect.latency_frames = u64::from(pdc_latency_samples);
            reports.push(report);
        }
        if reports.is_empty() {
            return Ok(reports);
        }
        let stream_latencies = self
            .streams
            .iter()
            .map(|stream| stream.latency_frames)
            .collect::<Vec<_>>();
        self.maximum_latency_frames = plan_pdc_delays(&stream_latencies).0;
        self.apply_live_mixer_routing(document, meters)?;
        Ok(reports)
    }

    pub(crate) fn performance_channel_ids(&self) -> BTreeSet<u16> {
        self.streams
            .iter()
            .filter(|stream| stream.live_performance)
            .map(|stream| stream.channel_id)
            .collect()
    }

    pub(crate) fn performance_volume_expression_channel_ids(&self) -> BTreeSet<u16> {
        self.streams
            .iter()
            .filter(|stream| stream.live_performance && stream.render.supports_note_volume)
            .map(|stream| stream.channel_id)
            .collect()
    }

    /// Channels whose prepared VST3 instances support per-note volume expression.
    pub fn volume_expression_channel_ids(&self) -> &BTreeSet<u16> {
        &self.volume_expression_channel_ids
    }

    pub(crate) fn performance_pattern_automation(
        &self,
    ) -> &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>> {
        &self.performance_pattern_automation
    }

    /// Remove the prepared arrangement notes from one live performance stream before input starts.
    pub fn clear_live_input_events(&mut self, channel_id: u16) -> Result<(), String> {
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| format!("VST3 channel {channel_id} is not available for live input"))?;
        self.notes_scheduled = self
            .notes_scheduled
            .saturating_sub(stream.render.note_count);
        stream.render.note_count = 0;
        stream.render.events.clear();
        stream.next_event = 0;
        stream.active_performance_notes.clear();
        Ok(())
    }

    pub(crate) fn schedule_live_input_note_on(
        &mut self,
        channel_id: u16,
        midi_channel: u8,
        key: u8,
        velocity: u8,
        layer_gain: f32,
        layer_pan_raw: i32,
    ) -> Result<(i32, u8), String> {
        let midi_channel = MidiChannel::from_index(midi_channel & 0x0F)
            .ok_or_else(|| "MIDI channel is outside 0..15".to_owned())?;
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| format!("VST3 channel {channel_id} is not available for live input"))?;
        let note =
            (i16::from(key.min(127)) + stream.note_pitch_offset_semitones).clamp(0, 127) as u8;
        let note_id = next_vst3_note_expression_id();
        let frame = stream.source_frame;
        queue_live_vst3_input_event(
            stream,
            ScheduledMidiEvent {
                frame,
                priority: 1,
                event: MidiEvent::NoteOn {
                    channel: midi_channel,
                    note,
                    velocity: velocity.min(127),
                },
                fine_pitch_offset_cents: 0,
                note_id,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: 64,
                layer_gain,
                layer_pan_raw,
                performance_track_id: Some(LIVE_VST3_INPUT_TRACK_ID),
            },
        );
        self.notes_scheduled = self.notes_scheduled.saturating_add(1);
        Ok((note_id, note))
    }

    pub(crate) fn schedule_live_input_note_off(
        &mut self,
        channel_id: u16,
        midi_channel: u8,
        key: u8,
        note_id: i32,
    ) -> Result<(), String> {
        let midi_channel = MidiChannel::from_index(midi_channel & 0x0F)
            .ok_or_else(|| "MIDI channel is outside 0..15".to_owned())?;
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| format!("VST3 channel {channel_id} is not available for live input"))?;
        let frame = stream.source_frame.saturating_add(1);
        queue_live_vst3_input_event(
            stream,
            ScheduledMidiEvent {
                frame,
                priority: 0,
                event: MidiEvent::NoteOff {
                    channel: midi_channel,
                    note: key.min(127),
                    velocity: 0,
                },
                fine_pitch_offset_cents: 0,
                note_id,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: 64,
                layer_gain: 1.0,
                layer_pan_raw: 0,
                performance_track_id: Some(LIVE_VST3_INPUT_TRACK_ID),
            },
        );
        Ok(())
    }

    pub(crate) fn schedule_live_input_midi_event(
        &mut self,
        channel_id: u16,
        event: MidiEvent,
    ) -> Result<(), String> {
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| format!("VST3 channel {channel_id} is not available for live input"))?;
        let event = match event {
            MidiEvent::ControlChange { .. }
            | MidiEvent::ProgramChange { .. }
            | MidiEvent::PitchBend { .. }
            | MidiEvent::ChannelAftertouch { .. } => event,
            MidiEvent::PolyAftertouch {
                channel,
                note,
                pressure,
            } => MidiEvent::PolyAftertouch {
                channel,
                note: (i16::from(note) + stream.note_pitch_offset_semitones).clamp(0, 127) as u8,
                pressure,
            },
            _ => return Err("only live VST3 controller messages can use this path".to_owned()),
        };
        let frame = stream.source_frame;
        queue_live_vst3_input_event(
            stream,
            ScheduledMidiEvent {
                frame,
                priority: 1,
                event,
                fine_pitch_offset_cents: 0,
                note_id: -1,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: 64,
                layer_gain: 1.0,
                layer_pan_raw: 0,
                performance_track_id: Some(LIVE_VST3_INPUT_TRACK_ID),
            },
        );
        Ok(())
    }

    pub(crate) fn stop_live_input_notes(&mut self) -> Result<(), String> {
        for stream in &mut self.streams {
            if !stream.live_performance {
                continue;
            }
            let stop_frame = stream.source_frame;
            let note_offs = stream
                .active_performance_notes
                .iter()
                .filter(|(_, event)| event.performance_track_id == Some(LIVE_VST3_INPUT_TRACK_ID))
                .filter_map(|(_, event)| {
                    let MidiEvent::NoteOn { channel, note, .. } = event.event else {
                        return None;
                    };
                    Some(ScheduledMidiEvent {
                        frame: stop_frame,
                        priority: 0,
                        event: MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        },
                        ..*event
                    })
                })
                .collect::<Vec<_>>();
            let mut pending_events = stream.render.events.split_off(stream.next_event);
            pending_events
                .retain(|event| event.performance_track_id != Some(LIVE_VST3_INPUT_TRACK_ID));
            pending_events.extend(note_offs.iter().copied());
            pending_events.sort_by_key(|event| (event.frame, event.priority));
            stream.render.events.extend(pending_events);
            if !note_offs.is_empty() {
                let output_frames = stream
                    .output_buses
                    .iter()
                    .map(|bus| bus.pending_output.len())
                    .max()
                    .unwrap_or_default()
                    .saturating_add(1);
                let mut discarded_output = Vec::new();
                stream.render_output_frames(output_frames, &mut discarded_output)?;
            }
        }
        Ok(())
    }

    pub(crate) fn schedule_performance_notes(
        &mut self,
        track_id: u32,
        channel_id: u16,
        notes: &[PlaylistMidiNote],
    ) -> Result<usize, String> {
        if notes.is_empty() {
            return Ok(0);
        }
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| {
                format!("VST3 channel {channel_id} is not available for Performance Mode launches")
            })?;
        let (mut events, note_count) = scheduled_playlist_midi_events_with_tempo_map(
            notes,
            &self.tempo_map,
            stream.render.sample_rate,
        )?;
        transpose_scheduled_note_events(&mut events, stream.note_pitch_offset_semitones);
        for event in &mut events {
            event.performance_track_id = Some(track_id);
        }
        let mut pending_events = stream.render.events.split_off(stream.next_event);
        pending_events.extend(events);
        pending_events.sort_by_key(|event| (event.frame, event.priority));
        stream.render.events.extend(pending_events);
        self.notes_scheduled = self.notes_scheduled.saturating_add(note_count);
        Ok(note_count)
    }

    pub(crate) fn schedule_performance_parameter_automation(
        &mut self,
        track_id: u32,
        channel_id: u16,
        automation: &[Vst3PlaylistParameterAutomation],
    ) -> Result<(), String> {
        if automation.is_empty() {
            return Ok(());
        }
        let tempo_map = self.tempo_map.clone();
        let stream = self
            .streams
            .iter_mut()
            .find(|stream| stream.channel_id == channel_id && stream.live_performance)
            .ok_or_else(|| {
                format!(
                    "VST3 channel {channel_id} is not available for Performance Mode automation"
                )
            })?;
        let mut prepared = prepare_playlist_parameter_automation(
            automation,
            &tempo_map,
            stream.render.sample_rate_u32,
            u64::MAX,
        )?;
        let incoming_point_count = prepared.iter().map(|lane| lane.points.len()).sum::<usize>();
        let existing_point_count = stream
            .render
            .parameter_automation
            .iter()
            .map(|lane| lane.points.len())
            .sum::<usize>();
        if existing_point_count.saturating_add(incoming_point_count) > MAX_VST3_AUTOMATION_POINTS {
            return Err(format!(
                "Performance VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
            ));
        }
        for lane in &mut prepared {
            for point in &mut lane.points {
                point.performance_track_id = Some(track_id);
            }
            if let Some(existing) = stream
                .render
                .parameter_automation
                .iter_mut()
                .find(|existing| existing.parameter_id == lane.parameter_id)
            {
                existing.points.extend(lane.points.iter().copied());
                existing.points.sort_by_key(|point| point.frame);
            } else {
                stream.render.parameter_automation.push(lane.clone());
                stream
                    .render
                    .parameter_automation
                    .sort_by_key(|lane| lane.parameter_id);
            }
        }
        let validation_frames = stream
            .render
            .parameter_automation
            .iter()
            .flat_map(|lane| lane.points.iter())
            .map(|point| point.frame.saturating_add(1))
            .max()
            .unwrap_or(stream.render.total_frames)
            .max(stream.render.total_frames);
        validate_parameter_automation_block_density(
            &stream.render.parameter_automation,
            stream.render.block_size,
            validation_frames,
        )
    }

    pub(crate) fn stop_performance_track(
        &mut self,
        track_id: u32,
        stop_frame: u64,
    ) -> Result<(), String> {
        for stream in &mut self.streams {
            if !stream.live_performance {
                continue;
            }
            let stop_source_frame = (stop_frame as f64 * stream.render.sample_rate
                / f64::from(self.output_sample_rate))
            .round();
            if !stop_source_frame.is_finite() || stop_source_frame > u64::MAX as f64 {
                return Err("VST3 Performance Mode stop is outside the renderable range".into());
            }
            let stop_source_frame = stop_source_frame as u64;
            let next_event = stream.next_event;
            let pending_events = &stream.render.events[next_event..];
            let mut notes_to_stop = stream
                .active_performance_notes
                .iter()
                .filter(|(_, event)| event.performance_track_id == Some(track_id))
                .map(|(&note_id, &event)| (note_id, event))
                .collect::<HashMap<_, _>>();
            for event in pending_events.iter().filter(|event| {
                event.performance_track_id == Some(track_id)
                    && event.frame < stop_source_frame
                    && matches!(event.event, MidiEvent::NoteOn { .. })
            }) {
                notes_to_stop.entry(event.note_id).or_insert(*event);
            }
            stop_performance_parameter_automation(
                &mut stream.render.parameter_automation,
                track_id,
                stop_source_frame,
            );
            let note_offs = notes_to_stop
                .into_iter()
                .filter(|(note_id, _)| {
                    !pending_events.iter().any(|event| {
                        event.performance_track_id == Some(track_id)
                            && event.note_id == *note_id
                            && event.frame < stop_source_frame
                            && matches!(event.event, MidiEvent::NoteOff { .. })
                    })
                })
                .filter_map(|(_, event)| {
                    let MidiEvent::NoteOn { channel, note, .. } = event.event else {
                        return None;
                    };
                    Some(ScheduledMidiEvent {
                        frame: stop_source_frame,
                        priority: 0,
                        event: MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        },
                        performance_track_id: Some(track_id),
                        ..event
                    })
                })
                .collect::<Vec<_>>();
            let mut pending_events = stream.render.events.split_off(next_event);
            pending_events.retain(|event| {
                event.performance_track_id != Some(track_id) || event.frame < stop_source_frame
            });
            pending_events.extend(note_offs);
            pending_events.sort_by_key(|event| (event.frame, event.priority));
            stream.render.events.extend(pending_events);
        }
        Ok(())
    }

    pub fn summary(&self) -> Vst3PlaylistRenderSummary {
        Vst3PlaylistRenderSummary {
            plugin_channels_rendered: self.streams.len(),
            notes_scheduled: self.notes_scheduled,
            unloaded_plugin_channels: self.unloaded_plugin_channels.clone(),
            frames: self.output_frames,
            sample_rate: self.output_sample_rate,
        }
    }

    /// Content length before Master effect tails are appended.
    pub fn content_frames(&self) -> u64 {
        self.content_frames
    }

    pub(crate) fn stop_pattern_repetition_after_first_pass(&mut self) {
        for stream in &mut self.streams {
            stream.pattern_loop = None;
        }
    }

    pub fn extend_to_output_frames(&mut self, frames: u64) -> Result<(), String> {
        self.content_frames = self.content_frames.max(frames);
        let content_with_effect_tail = self
            .content_frames
            .checked_add(self.mixer_effect_tail_frames)
            .ok_or_else(|| "Playlist Mixer effect tail length overflow".to_owned())?;
        let total_pdc_latency_frames = self
            .maximum_latency_frames
            .checked_add(self.mixer_effect_pdc_latency_frames)
            .ok_or_else(|| "Playlist Mixer PDC latency overflow".to_owned())?;
        self.output_frames =
            pdc_aligned_output_frames(content_with_effect_tail, total_pdc_latency_frames)?;

        let mut delay_frames = if self.automatic_pdc_enabled {
            self.maximum_latency_frames
        } else {
            0
        };
        for stream in &self.streams {
            let compensation_frames = if self.automatic_pdc_enabled {
                self.maximum_latency_frames
                    .saturating_sub(stream.latency_frames)
            } else {
                0
            };
            let bus_compensation_frames = compensation_frames
                .checked_mul(u64::try_from(stream.output_buses.len()).map_err(|_| {
                    "VST3 output bus count exceeds the PDC storage range".to_owned()
                })?)
                .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
            delay_frames = delay_frames
                .checked_add(bus_compensation_frames)
                .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        }
        delay_frames = delay_frames
            .checked_add(self.mixer_effect_dry_delay_frames)
            .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        delay_frames = delay_frames
            .checked_add(self.mixer_effect_route_delay_frames)
            .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        let delay_bytes = delay_frames
            .checked_mul(std::mem::size_of::<[f32; 2]>() as u64)
            .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        if delay_bytes > MAX_PDC_DELAY_BYTES {
            return Err(format!(
                "automatic PDC needs {delay_bytes} bytes of delay storage, exceeding the 128 MiB limit"
            ));
        }

        for stream in &mut self.streams {
            let compensation_frames = if self.automatic_pdc_enabled {
                self.maximum_latency_frames
                    .saturating_sub(stream.latency_frames)
            } else {
                0
            };
            for bus in &mut stream.output_buses {
                bus.delay_line.set_delay_frames(compensation_frames)?;
            }
            let stream_frames = self
                .content_frames
                .checked_add(stream.latency_frames)
                .ok_or_else(|| "VST3 PDC stream length overflow".to_owned())?;
            let source_frames = (stream_frames as f64 * stream.render.sample_rate
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

    pub(crate) fn pdc_direct_source_delay_frames(&self) -> u64 {
        if self.automatic_pdc_enabled {
            self.maximum_latency_frames
        } else {
            0
        }
    }

    /// Initial delay added to the final mix by supported automatic instrument and Mixer PDC.
    pub(crate) fn pdc_render_start_trim_frames(&self) -> u64 {
        pdc_render_start_trim_frames(
            self.automatic_pdc_enabled,
            self.maximum_latency_frames,
            self.mixer_effect_pdc_latency_frames,
        )
    }

    /// Initial PDC delay on one Mixer stem, including post-effect alignment when present.
    pub(crate) fn pdc_mixer_stem_start_trim_frames(
        &self,
        track: usize,
        post_effect_bus: bool,
    ) -> u64 {
        if track == 0 {
            return self.pdc_render_start_trim_frames();
        }
        if !self.automatic_pdc_enabled {
            return 0;
        }
        let meter_input_delay = self
            .mixer_effect_meter_input_delays
            .get(track)
            .map_or(0, crate::audio::StereoFrameDelay::delay_frames);
        let post_effect_delay = if post_effect_bus && self.needs_mixer_insert_buses() {
            let chain_latency = self
                .mixer_effect_chains
                .iter()
                .filter(|chain| chain.insert_ordinal == track)
                .fold(0_u64, |total, chain| {
                    total.saturating_add(chain.latency_frames)
                });
            let output_alignment = self
                .mixer_post_effect_stem_delays
                .get(track)
                .map_or(0, crate::audio::StereoFrameDelay::delay_frames);
            chain_latency.saturating_add(output_alignment)
        } else {
            0
        };
        pdc_mixer_stem_start_trim_frames(
            self.automatic_pdc_enabled,
            self.maximum_latency_frames,
            meter_input_delay,
            post_effect_delay,
        )
    }

    pub(crate) fn needs_mixer_insert_buses(&self) -> bool {
        self.mixer_effect_chains
            .iter()
            .any(|chain| chain.insert_ordinal != 0)
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
        for index in 0..self.mixer_effects.len() {
            let slot_index = self.mixer_effects[index].slot_index;
            let result = if let Some(plugin) = &self.mixer_effects[index].plugin {
                plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())
                    .and_then(|mut plugin| {
                        plugin.start_processing().map_err(|error| error.to_string())
                    })
            } else if self.mixer_effects[index].external_processor.is_some() {
                Ok(())
            } else {
                Err("Mixer effect has no audio processor".to_owned())
            };
            if let Err(error) = result {
                let _ = self.stop_processing();
                return Err(format!(
                    "could not start VST3 Mixer effect slot {} processing: {error}",
                    slot_index + 1
                ));
            }
            self.started_mixer_effect_count += 1;
        }
        Ok(())
    }

    pub fn mix_next_block(&mut self, output: &mut [f32]) -> Result<(), String> {
        self.mix_next_block_with_optional_meter_and_post_effect_buses(output, None, None)
    }

    pub(crate) fn mix_next_block_with_meter_buses(
        &mut self,
        output: &mut [f32],
        meter_buses: &mut [f32],
    ) -> Result<(), String> {
        self.mix_next_block_with_optional_meter_and_post_effect_buses(
            output,
            Some(meter_buses),
            None,
        )
    }

    pub(crate) fn mix_next_block_with_post_effect_buses(
        &mut self,
        output: &mut [f32],
        meter_buses: &mut [f32],
        post_effect_buses: &mut [f32],
    ) -> Result<(), String> {
        self.mix_next_block_with_optional_meter_and_post_effect_buses(
            output,
            Some(meter_buses),
            Some(post_effect_buses),
        )
    }

    fn mix_next_block_with_optional_meter_and_post_effect_buses(
        &mut self,
        output: &mut [f32],
        mut meter_buses: Option<&mut [f32]>,
        post_effect_buses: Option<&mut [f32]>,
    ) -> Result<(), String> {
        crate::audio::enable_denormal_protection();
        if !output.len().is_multiple_of(2) {
            return Err("VST3 Playlist mix buffer must contain stereo frames".to_owned());
        }
        let (mut meter_buses, mut effect_input_buses) = if let Some(buses) = meter_buses.take() {
            let (meter_buses, effect_input_buses) =
                crate::audio::split_mixer_track_buses_mut(buses, output.len())?;
            (Some(meter_buses), effect_input_buses)
        } else {
            (None, None)
        };
        if let (Some(meter_buses), Some(post_effect_buses)) =
            (meter_buses.as_deref(), post_effect_buses.as_deref())
            && meter_buses.len() != post_effect_buses.len()
        {
            return Err("post-effect Mixer buses do not match the meter buses".to_owned());
        }
        let frame_count = output.len() / 2;
        for stream in &mut self.streams {
            let mut plugin_outputs = Vec::new();
            plugin_outputs
                .try_reserve_exact(stream.output_buses.len())
                .map_err(|error| format!("could not allocate VST3 output bus blocks: {error}"))?;
            stream.render_output_frames(frame_count, &mut plugin_outputs)?;
            for (bus_index, plugin_output) in plugin_outputs.iter_mut().enumerate() {
                let bus = &mut stream.output_buses[bus_index];
                bus.delay_line.process_interleaved(plugin_output)?;
                if let Some(meter_buses) = meter_buses.as_deref() {
                    for (track, _) in &bus.mixer_meter_transforms {
                        if *track >= crate::audio::MIXER_TRACK_METER_COUNT {
                            return Err("VST3 Mixer meter route exceeds the supported track range"
                                .to_owned());
                        }
                        let meter_end = (*track + 1)
                            .checked_mul(output.len())
                            .ok_or_else(|| "Mixer meter buffer size overflow".to_owned())?;
                        if meter_end > meter_buses.len() {
                            return Err(
                                "Mixer meter buffer is too small for the VST3 route".to_owned()
                            );
                        }
                    }
                    for (track, _) in &bus.mixer_effect_input_transforms {
                        if *track >= crate::audio::MIXER_TRACK_METER_COUNT {
                            return Err(
                                "VST3 Mixer effect input route exceeds the supported track range"
                                    .to_owned(),
                            );
                        }
                    }
                }
                for (frame_index, frame) in plugin_output.as_chunks::<2>().0.iter().enumerate() {
                    let source_frame = [
                        frame[0] * bus.gain * bus.left_pan_gain,
                        frame[1] * bus.gain * bus.right_pan_gain,
                    ];
                    let mut routed_frame = source_frame;
                    let live_controls = bus.live_mixer_source.as_ref().and_then(|source| {
                        source
                            .targets
                            .as_ref()
                            .map(|targets| source.control.next_frame_controls(targets))
                    });
                    if let (Some(source), Some(controls)) =
                        (bus.live_mixer_source.as_ref(), live_controls)
                    {
                        source
                            .control
                            .apply_output_frame(controls, &mut routed_frame);
                    } else {
                        bus.mixer_transform.apply_frame(&mut routed_frame);
                    }
                    output[frame_index * 2] += routed_frame[0];
                    output[frame_index * 2 + 1] += routed_frame[1];
                    if let Some(meter_buses) = meter_buses.as_deref_mut() {
                        let meter_transforms = match (bus.live_mixer_source.as_ref(), live_controls)
                        {
                            (Some(source), Some(_)) => source.control.insert_meter_transforms(),
                            _ => bus.mixer_meter_transforms.as_slice(),
                        };
                        for (track, transform) in meter_transforms {
                            let mut meter_frame = source_frame;
                            if let (Some(source), Some(controls)) =
                                (bus.live_mixer_source.as_ref(), live_controls)
                            {
                                source.control.apply_meter_frame(
                                    controls,
                                    *track,
                                    *transform,
                                    &mut meter_frame,
                                );
                            } else {
                                transform.apply_frame(&mut meter_frame);
                            }
                            let meter_offset = *track * output.len() + frame_index * 2;
                            meter_buses[meter_offset] += meter_frame[0];
                            meter_buses[meter_offset + 1] += meter_frame[1];
                        }
                    }
                    if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
                        let effect_input_transforms =
                            match (bus.live_mixer_source.as_ref(), live_controls) {
                                (Some(source), Some(_)) => {
                                    source.control.insert_effect_input_transforms()
                                }
                                _ => bus.mixer_effect_input_transforms.as_slice(),
                            };
                        for (track, transform) in effect_input_transforms {
                            if *track >= crate::audio::MIXER_TRACK_METER_COUNT {
                                return Err(
                                    "VST3 Mixer effect input route exceeds the supported track range"
                                        .to_owned()
                                );
                            }
                            let mut effect_input_frame = source_frame;
                            if let (Some(source), Some(controls)) =
                                (bus.live_mixer_source.as_ref(), live_controls)
                            {
                                source.control.apply_effect_input_frame(
                                    controls,
                                    *track,
                                    *transform,
                                    &mut effect_input_frame,
                                );
                            } else {
                                transform.apply_frame(&mut effect_input_frame);
                            }
                            let effect_input_offset = *track * output.len() + frame_index * 2;
                            effect_input_buses[effect_input_offset] += effect_input_frame[0];
                            effect_input_buses[effect_input_offset + 1] += effect_input_frame[1];
                        }
                    }
                }
            }
        }
        let block_start_frame = self.mixer_effect_frame;
        self.process_mixer_effects(
            output,
            meter_buses,
            effect_input_buses,
            post_effect_buses,
            block_start_frame,
        )?;
        self.mixer_effect_frame = self
            .mixer_effect_frame
            .checked_add(
                u64::try_from(frame_count)
                    .map_err(|_| "Mixer effect automation frame count overflow".to_owned())?,
            )
            .ok_or_else(|| "Mixer effect automation frame position overflow".to_owned())?;
        self.refresh_external_mixer_effect_reports()?;
        Ok(())
    }

    fn process_mixer_effects<'a>(
        &'a mut self,
        output: &mut [f32],
        mut meter_buses: Option<&mut [f32]>,
        mut effect_input_buses: Option<&mut [f32]>,
        mut post_effect_buses: Option<&'a mut [f32]>,
        block_start_frame: u64,
    ) -> Result<(), String> {
        if !output.len().is_multiple_of(2) {
            return Err("VST3 Mixer effect buffer must contain stereo frames".to_owned());
        }

        self.mixer_effect_output_delay.process_interleaved(output)?;
        let samples_per_track = output.len();
        if post_effect_buses.is_none()
            && self
                .mixer_effects
                .iter()
                .any(|effect| !effect.sidechain_inputs.is_empty())
        {
            if meter_buses.is_none() {
                return Err("VST3 Mixer sidechain inputs require Mixer source buses".to_owned());
            }
            let bus_samples = crate::audio::MIXER_TRACK_METER_COUNT
                .checked_mul(samples_per_track)
                .ok_or_else(|| "post-effect Mixer bus size overflow".to_owned())?;
            self.mixer_effect_post_effect_bus_scratch
                .resize(bus_samples, 0.0);
            self.mixer_effect_post_effect_bus_scratch.fill(0.0);
            post_effect_buses = Some(&mut self.mixer_effect_post_effect_bus_scratch);
        }
        if let Some(meter_buses) = meter_buses.as_deref_mut() {
            for (track, delay) in self.mixer_effect_meter_input_delays.iter_mut().enumerate() {
                let start = track
                    .checked_mul(samples_per_track)
                    .ok_or_else(|| "Mixer meter PDC offset overflow".to_owned())?;
                let end = start
                    .checked_add(samples_per_track)
                    .ok_or_else(|| "Mixer meter PDC size overflow".to_owned())?;
                if end > meter_buses.len() {
                    return Err("Mixer meter buffer is too small for PDC".to_owned());
                }
                delay.process_interleaved(&mut meter_buses[start..end])?;
            }
        }
        if let Some(effect_input_buses) = effect_input_buses.as_deref_mut() {
            for (track, delay) in self.mixer_effect_input_delays.iter_mut().enumerate() {
                let start = track
                    .checked_mul(samples_per_track)
                    .ok_or_else(|| "Mixer effect input PDC offset overflow".to_owned())?;
                let end = start
                    .checked_add(samples_per_track)
                    .ok_or_else(|| "Mixer effect input PDC size overflow".to_owned())?;
                if end > effect_input_buses.len() {
                    return Err("Mixer effect input buffer is too small for PDC".to_owned());
                }
                delay.process_interleaved(&mut effect_input_buses[start..end])?;
            }
        }
        if let (Some(meter_buses), Some(post_effect_buses)) =
            (meter_buses.as_deref(), post_effect_buses.as_deref_mut())
        {
            if meter_buses.len() != post_effect_buses.len() {
                return Err("post-effect Mixer buses do not match the meter buses".to_owned());
            }
            post_effect_buses.copy_from_slice(meter_buses);
        }

        for chain in &mut self.mixer_effect_chains {
            let first_effect_index = chain.first_effect_index;
            let end_effect_index = chain.end_effect_index;
            if chain.insert_ordinal == 0 {
                process_playlist_mixer_effect_chain(
                    &mut self.mixer_effects[first_effect_index..end_effect_index],
                    output,
                    post_effect_buses.as_deref(),
                    samples_per_track,
                    block_start_frame,
                )?;
                continue;
            }

            let Some(meter_buses) = meter_buses.as_deref_mut() else {
                return Err("VST3 Mixer insert effects require Mixer input buses".to_owned());
            };
            let Some(effect_input_buses) = effect_input_buses.as_deref_mut() else {
                return Err("VST3 Mixer insert effects require pre-effect input buses".to_owned());
            };
            let bus_start = chain
                .insert_ordinal
                .checked_mul(samples_per_track)
                .ok_or_else(|| "Mixer effect bus offset overflow".to_owned())?;
            let bus_end = bus_start
                .checked_add(samples_per_track)
                .ok_or_else(|| "Mixer effect bus size overflow".to_owned())?;
            if bus_end > meter_buses.len() {
                return Err(format!(
                    "Mixer effect insert {} exceeds the available bus buffer",
                    chain.insert_ordinal
                ));
            }
            if bus_end > effect_input_buses.len() {
                return Err(format!(
                    "Mixer effect insert {} exceeds the available pre-effect bus buffer",
                    chain.insert_ordinal
                ));
            }
            self.mixer_effect_scratch.resize(samples_per_track, 0.0);
            self.mixer_effect_dry_scratch.resize(samples_per_track, 0.0);
            self.mixer_effect_scratch
                .copy_from_slice(&effect_input_buses[bus_start..bus_end]);
            self.mixer_effect_dry_scratch
                .copy_from_slice(&self.mixer_effect_scratch);
            process_playlist_mixer_effect_chain(
                &mut self.mixer_effects[first_effect_index..end_effect_index],
                &mut self.mixer_effect_scratch,
                post_effect_buses.as_deref(),
                samples_per_track,
                block_start_frame,
            )?;
            chain
                .dry_reference_delay
                .process_interleaved(&mut self.mixer_effect_dry_scratch)?;

            for (frame_index, (processed, dry)) in self
                .mixer_effect_scratch
                .as_chunks::<2>()
                .0
                .iter()
                .zip(self.mixer_effect_dry_scratch.as_chunks::<2>().0.iter())
                .enumerate()
            {
                let raw_delta = [processed[0] - dry[0], processed[1] - dry[1]];
                let live_controls = chain.next_live_frame_controls();
                let output_delta = route_playlist_mixer_effect_output_delta(
                    &mut chain.output_alignment_delay,
                    &chain.route_plan,
                    chain.live_source.as_ref(),
                    raw_delta,
                    live_controls,
                );
                let live_source = chain.live_source.as_ref();
                if let Some(post_effect_buses) = post_effect_buses.as_deref_mut() {
                    let post_effect_offset = chain
                        .insert_ordinal
                        .checked_mul(samples_per_track)
                        .ok_or_else(|| "post-effect Mixer bus offset overflow".to_owned())?;
                    let post_effect_sample = post_effect_offset
                        .checked_add(frame_index * 2)
                        .ok_or_else(|| "post-effect Mixer bus offset overflow".to_owned())?;
                    let post_effect_end = post_effect_sample
                        .checked_add(2)
                        .filter(|end| *end <= post_effect_buses.len())
                        .ok_or_else(|| "post-effect Mixer bus is too small".to_owned())?;
                    let mut post_effect_frame = [processed[0], processed[1]];
                    chain
                        .post_effect_meter_transform
                        .apply_frame(&mut post_effect_frame);
                    post_effect_buses[post_effect_sample] = finite_sample(post_effect_frame[0]);
                    post_effect_buses[post_effect_sample + 1] = finite_sample(post_effect_frame[1]);
                    debug_assert_eq!(post_effect_end, post_effect_sample + 2);
                }
                output[frame_index * 2] += finite_sample(output_delta[0]);
                output[frame_index * 2 + 1] += finite_sample(output_delta[1]);

                for branch in &mut chain.downstream_delays {
                    if branch.track >= crate::audio::MIXER_TRACK_METER_COUNT {
                        return Err(
                            "VST3 Mixer meter route exceeds the supported track range".to_owned()
                        );
                    }
                    let meter_offset = branch.track * samples_per_track + frame_index * 2;
                    let meter_end = meter_offset
                        .checked_add(2)
                        .ok_or_else(|| "Mixer meter buffer offset overflow".to_owned())?;
                    if meter_end > meter_buses.len() {
                        return Err("Mixer meter buffer is too small for the VST3 route".to_owned());
                    }
                    let mut meter_delta = raw_delta;
                    if let (Some(source), Some(controls)) = (live_source, live_controls) {
                        source.apply_meter_frame(
                            controls,
                            branch.track,
                            branch.meter_transform,
                            &mut meter_delta,
                        );
                    } else {
                        branch.meter_transform.apply_frame(&mut meter_delta);
                    }
                    branch.meter_delay.process_frame(&mut meter_delta);
                    meter_buses[meter_offset] += finite_sample(meter_delta[0]);
                    meter_buses[meter_offset + 1] += finite_sample(meter_delta[1]);
                    if let Some(post_effect_buses) = post_effect_buses.as_deref_mut() {
                        post_effect_buses[meter_offset] += finite_sample(meter_delta[0]);
                        post_effect_buses[meter_offset + 1] += finite_sample(meter_delta[1]);
                    }
                    if branch.track >= crate::audio::MIXER_TRACK_METER_COUNT {
                        return Err(
                            "VST3 Mixer effect input route exceeds the supported track range"
                                .to_owned(),
                        );
                    }
                    let input_offset = branch.track * samples_per_track + frame_index * 2;
                    let input_end = input_offset
                        .checked_add(2)
                        .ok_or_else(|| "Mixer effect input buffer offset overflow".to_owned())?;
                    if input_end > effect_input_buses.len() {
                        return Err(
                            "Mixer effect input buffer is too small for the VST3 route".to_owned()
                        );
                    }
                    let mut effect_input_delta = raw_delta;
                    if let (Some(source), Some(controls)) = (live_source, live_controls) {
                        source.apply_effect_input_frame(
                            controls,
                            branch.track,
                            branch.effect_input_transform,
                            &mut effect_input_delta,
                        );
                    } else {
                        branch
                            .effect_input_transform
                            .apply_frame(&mut effect_input_delta);
                    }
                    branch
                        .effect_input_delay
                        .process_frame(&mut effect_input_delta);
                    effect_input_buses[input_offset] += finite_sample(effect_input_delta[0]);
                    effect_input_buses[input_offset + 1] += finite_sample(effect_input_delta[1]);
                }
            }
        }
        if let Some(post_effect_buses) = post_effect_buses {
            let expected_samples = crate::audio::MIXER_TRACK_METER_COUNT
                .checked_mul(samples_per_track)
                .ok_or_else(|| "post-effect Mixer bus size overflow".to_owned())?;
            if post_effect_buses.len() != expected_samples {
                return Err("post-effect Mixer buses do not match the render block".to_owned());
            }
            if self.mixer_post_effect_stem_delays.len() != crate::audio::MIXER_TRACK_METER_COUNT {
                return Err("post-effect Mixer PDC delays were not prepared".to_owned());
            }
            for (track, delay) in self.mixer_post_effect_stem_delays.iter_mut().enumerate() {
                let start = track
                    .checked_mul(samples_per_track)
                    .ok_or_else(|| "post-effect Mixer bus offset overflow".to_owned())?;
                let end = start
                    .checked_add(samples_per_track)
                    .ok_or_else(|| "post-effect Mixer bus size overflow".to_owned())?;
                delay.process_interleaved(&mut post_effect_buses[start..end])?;
            }
        }
        Ok(())
    }

    pub fn stop_processing(&mut self) -> Result<(), String> {
        let mut first_error = None;
        while self.started_mixer_effect_count > 0 {
            self.started_mixer_effect_count -= 1;
            let effect = &mut self.mixer_effects[self.started_mixer_effect_count];
            let result = if let Some(plugin) = &effect.plugin {
                plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())
                    .and_then(|mut plugin| {
                        plugin.stop_processing().map_err(|error| error.to_string())
                    })
            } else if let Some(processor) = effect.external_processor.as_mut() {
                processor.stop_processing()
            } else {
                Err("Mixer effect has no audio processor".to_owned())
            };
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(format!(
                    "could not stop VST3 Mixer effect slot {} processing: {error}",
                    effect.slot_index + 1
                ));
            }
        }
        while self.started_count > 0 {
            self.started_count -= 1;
            let stream = &mut self.streams[self.started_count];
            let plugin_cell = Arc::clone(&stream.plugin);
            let result = plugin_cell
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())
                .and_then(|mut plugin| {
                    let cleanup_result = stream.finish_live_midi(&mut plugin);
                    let stop_result = plugin.stop_processing().map_err(|error| error.to_string());
                    cleanup_result.and(stop_result)
                });
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

fn process_playlist_mixer_effect_block(
    effect: &mut PlaylistMixerEffectStream,
    post_effect_buses: Option<&[f32]>,
    samples_per_track: usize,
    block_offset: usize,
    block_start_frame: u64,
    frame_count: usize,
) -> Result<(), String> {
    let sample_count = frame_count
        .checked_mul(2)
        .ok_or_else(|| "Mixer effect stereo block size overflow".to_owned())?;
    if let Some(plugin_cell) = effect.plugin.as_ref().map(Arc::clone) {
        let buffers = effect
            .buffers
            .as_mut()
            .ok_or_else(|| "VST3 Mixer effect audio buffers are unavailable".to_owned())?;
        resize_vst3_bus_audio_buffers(buffers, frame_count);
        let input = buffers
            .inputs
            .first_mut()
            .ok_or_else(|| "VST3 Mixer effect main input bus is unavailable".to_owned())?;
        for frame_index in 0..frame_count {
            let left = effect.dry_delay_scratch[frame_index * 2];
            let right = effect.dry_delay_scratch[frame_index * 2 + 1];
            if effect.input_channels == 1 {
                input.channels[0][frame_index] = (left + right) * 0.5;
            } else {
                input.channels[0][frame_index] = left;
                input.channels[1][frame_index] = right;
            }
        }
        fill_playlist_mixer_sidechain_inputs(
            &mut effect.sidechain_inputs,
            &mut buffers.inputs,
            post_effect_buses,
            samples_per_track,
            block_offset,
            frame_count,
        )?;
        {
            let mut plugin = plugin_cell
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            validate_vst3_audio_block(buffers, plugin.sample_rate(), plugin.block_size())?;
            if let Some(loop_frames) = effect.parameter_automation_loop_frames {
                set_looped_parameter_automation_for_block_at_range(
                    &mut plugin,
                    &effect.parameter_automation,
                    block_start_frame,
                    frame_count,
                    effect.parameter_automation_loop_start_frame,
                    loop_frames,
                )?;
            } else {
                set_parameter_automation_for_block(
                    &mut plugin,
                    &effect.parameter_automation,
                    block_start_frame,
                    frame_count,
                )?;
            }
            plugin
                .process_bus_audio(buffers)
                .map_err(|error| error.to_string())
                .map_err(|error| {
                    format!(
                        "VST3 Mixer insert {} effect slot {} (instance {}) failed: {error}",
                        effect.insert_ordinal,
                        effect.slot_index + 1,
                        effect.id
                    )
                })?;
        }
        let wet = buffers
            .outputs
            .first()
            .ok_or_else(|| "VST3 Mixer effect main output bus is unavailable".to_owned())?;
        let wet_left = wet
            .channels
            .first()
            .ok_or_else(|| "VST3 Mixer effect main output has no channels".to_owned())?;
        let wet_right = if effect.output_channels == 1 {
            wet_left
        } else {
            wet.channels
                .get(1)
                .ok_or_else(|| "VST3 Mixer effect main output has no right channel".to_owned())?
        };
        for frame_index in 0..frame_count {
            effect.wet_scratch[frame_index * 2] = finite_sample(wet_left[frame_index]);
            effect.wet_scratch[frame_index * 2 + 1] = finite_sample(wet_right[frame_index]);
        }
        return Ok(());
    }

    effect.wet_scratch[..sample_count].copy_from_slice(&effect.dry_delay_scratch[..sample_count]);
    fill_external_playlist_mixer_sidechain_inputs(
        effect,
        post_effect_buses,
        samples_per_track,
        block_offset,
        frame_count,
    )?;
    let parameter_changes = mixer_effect_parameter_changes_for_block(
        &effect.parameter_automation,
        block_start_frame,
        frame_count,
        effect
            .parameter_automation_loop_frames
            .map(|loop_frames| (effect.parameter_automation_loop_start_frame, loop_frames)),
    )?;
    effect
        .external_processor
        .as_mut()
        .ok_or_else(|| "Mixer effect has no audio processor".to_owned())?
        .process_block(
            &mut effect.wet_scratch[..sample_count],
            &effect.external_auxiliary_scratch,
            &parameter_changes,
        )
        .map_err(|error| {
            format!(
                "Mixer insert {} external effect slot {} failed: {error}",
                effect.insert_ordinal,
                effect.slot_index + 1
            )
        })
}

fn process_playlist_mixer_effect_chain(
    effects: &mut [PlaylistMixerEffectStream],
    samples: &mut [f32],
    post_effect_buses: Option<&[f32]>,
    samples_per_track: usize,
    block_start_frame: u64,
) -> Result<(), String> {
    if effects.is_empty() {
        return Ok(());
    }
    if !samples.len().is_multiple_of(2) {
        return Err("VST3 Mixer effect buffer must contain stereo frames".to_owned());
    }
    let chain_block_frames = effects[0].block_size.max(1);
    for (chain_block_index, chain_block) in samples.chunks_mut(chain_block_frames * 2).enumerate() {
        let chain_block_offset = chain_block_index * chain_block_frames;
        let chain_block_start_frame = block_start_frame
            .checked_add(
                u64::try_from(chain_block_offset)
                    .map_err(|_| "Mixer effect automation frame offset overflow".to_owned())?,
            )
            .ok_or_else(|| "Mixer effect automation frame position overflow".to_owned())?;
        for effect in effects.iter_mut() {
            let max_frames = effect.block_size.max(1);
            let mut effect_block_start_frame = chain_block_start_frame;
            let mut effect_block_offset = chain_block_offset;
            for block in chain_block.chunks_mut(max_frames * 2) {
                effect.begin_wet_mix_block();
                let frame_count = block.len() / 2;
                for (frame_index, frame) in block.as_chunks::<2>().0.iter().enumerate() {
                    effect.dry_delay_scratch[frame_index * 2] = finite_sample(frame[0]);
                    effect.dry_delay_scratch[frame_index * 2 + 1] = finite_sample(frame[1]);
                }
                process_playlist_mixer_effect_block(
                    effect,
                    post_effect_buses,
                    samples_per_track,
                    effect_block_offset,
                    effect_block_start_frame,
                    frame_count,
                )?;
                effect
                    .dry_delay_line
                    .process_interleaved(&mut effect.dry_delay_scratch[..frame_count * 2])?;
                let wet_mix_ramp = &mut effect.wet_mix_ramp;
                for (frame_index, frame) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let wet_mix = wet_mix_ramp.next();
                    let dry_gain = 1.0 - wet_mix;
                    let dry_left = effect.dry_delay_scratch[frame_index * 2];
                    let dry_right = effect.dry_delay_scratch[frame_index * 2 + 1];
                    let wet_left = effect.wet_scratch[frame_index * 2];
                    let wet_right = effect.wet_scratch[frame_index * 2 + 1];
                    frame[0] = dry_left * dry_gain + wet_left * wet_mix;
                    frame[1] = dry_right * dry_gain + wet_right * wet_mix;
                }
                effect_block_start_frame = effect_block_start_frame
                    .checked_add(
                        u64::try_from(frame_count).map_err(|_| {
                            "Mixer effect automation frame count overflow".to_owned()
                        })?,
                    )
                    .ok_or_else(|| "Mixer effect automation frame position overflow".to_owned())?;
                effect_block_offset += frame_count;
            }
        }
    }
    Ok(())
}

fn fill_playlist_mixer_sidechain_inputs(
    sidechain_inputs: &mut [PlaylistMixerEffectSidechainInput],
    input_buses: &mut [vst3_host::audio::AudioBusBuffer],
    post_effect_buses: Option<&[f32]>,
    samples_per_track: usize,
    frame_offset: usize,
    frame_count: usize,
) -> Result<(), String> {
    fill_playlist_mixer_sidechain_audio_scratch(
        sidechain_inputs,
        post_effect_buses,
        samples_per_track,
        frame_offset,
        frame_count,
    )?;
    for sidechain in sidechain_inputs {
        let input_bus = input_buses
            .get_mut(sidechain.bus_index)
            .ok_or_else(|| "VST3 Mixer sidechain input bus is unavailable".to_owned())?;
        if !input_bus.active
            || input_bus.channels.len() != sidechain.channel_count
            || !matches!(sidechain.channel_count, 1 | 2)
        {
            return Err("VST3 Mixer sidechain input bus has an unsupported layout".to_owned());
        }
        if input_bus
            .channels
            .iter()
            .any(|channel| channel.len() != frame_count)
        {
            return Err("VST3 Mixer sidechain input bus has an unexpected block size".to_owned());
        }
        for frame_index in 0..frame_count {
            let left = sidechain.stereo_scratch[frame_index * 2];
            let right = sidechain.stereo_scratch[frame_index * 2 + 1];
            if sidechain.channel_count == 1 {
                input_bus.channels[0][frame_index] = (left + right) * 0.5;
            } else {
                input_bus.channels[0][frame_index] = left;
                input_bus.channels[1][frame_index] = right;
            }
        }
    }
    Ok(())
}

fn fill_playlist_mixer_sidechain_audio_scratch(
    sidechain_inputs: &mut [PlaylistMixerEffectSidechainInput],
    post_effect_buses: Option<&[f32]>,
    samples_per_track: usize,
    frame_offset: usize,
    frame_count: usize,
) -> Result<(), String> {
    if sidechain_inputs.is_empty() {
        return Ok(());
    }
    let post_effect_buses = post_effect_buses
        .ok_or_else(|| "Mixer sidechain inputs require post-effect Mixer buses".to_owned())?;
    if !samples_per_track.is_multiple_of(2) {
        return Err("post-effect Mixer bus stride must contain stereo frames".to_owned());
    }
    let expected_samples = crate::audio::MIXER_TRACK_METER_COUNT
        .checked_mul(samples_per_track)
        .ok_or_else(|| "post-effect Mixer bus size overflow".to_owned())?;
    if post_effect_buses.len() != expected_samples {
        return Err("post-effect Mixer buses do not match the render block".to_owned());
    }

    for sidechain in sidechain_inputs {
        if sidechain.source_track >= crate::audio::MIXER_TRACK_METER_COUNT {
            return Err("VST3 Mixer sidechain source exceeds the supported track range".to_owned());
        }
        let source_start = sidechain
            .source_track
            .checked_mul(samples_per_track)
            .and_then(|start| start.checked_add(frame_offset.checked_mul(2)?))
            .ok_or_else(|| "VST3 Mixer sidechain source offset overflow".to_owned())?;
        let source_end = source_start
            .checked_add(
                frame_count
                    .checked_mul(2)
                    .ok_or_else(|| "VST3 Mixer sidechain block size overflow".to_owned())?,
            )
            .filter(|end| *end <= post_effect_buses.len())
            .ok_or_else(|| "post-effect Mixer bus is too small for a sidechain".to_owned())?;
        let source = &post_effect_buses[source_start..source_end];
        let sample_count = frame_count
            .checked_mul(2)
            .ok_or_else(|| "Mixer sidechain block size overflow".to_owned())?;
        if sidechain.stereo_scratch.len() < sample_count {
            return Err("Mixer sidechain scratch buffer is too small".to_owned());
        }
        if let Some(target) = &sidechain.live_enabled_target {
            let enabled = match target.load(Ordering::Acquire) {
                0 => false,
                1 => true,
                _ => sidechain.saved_enabled,
            };
            sidechain
                .enabled_ramp
                .set_target(if enabled { 1.0 } else { 0.0 });
        }
        for frame_index in 0..frame_count {
            let source_offset = frame_index * 2;
            let mut frame = [
                finite_sample(source[source_offset]),
                finite_sample(source[source_offset + 1]),
            ];
            sidechain.delay.process_frame(&mut frame);
            let enabled_gain = sidechain.enabled_ramp.next();
            frame[0] *= enabled_gain;
            frame[1] *= enabled_gain;
            sidechain.stereo_scratch[source_offset] = frame[0];
            sidechain.stereo_scratch[source_offset + 1] = frame[1];
        }
    }
    Ok(())
}

fn fill_external_playlist_mixer_sidechain_inputs(
    effect: &mut PlaylistMixerEffectStream,
    post_effect_buses: Option<&[f32]>,
    samples_per_track: usize,
    frame_offset: usize,
    frame_count: usize,
) -> Result<(), String> {
    fill_playlist_mixer_sidechain_audio_scratch(
        &mut effect.sidechain_inputs,
        post_effect_buses,
        samples_per_track,
        frame_offset,
        frame_count,
    )?;
    let sample_count = frame_count
        .checked_mul(2)
        .ok_or_else(|| "CLAP Mixer sidechain block size overflow".to_owned())?;
    let total_sample_count = effect
        .external_auxiliary_input_buses
        .len()
        .checked_mul(sample_count)
        .ok_or_else(|| "CLAP Mixer sidechain scratch size overflow".to_owned())?;
    if total_sample_count > effect.external_auxiliary_scratch.capacity() {
        return Err("CLAP Mixer sidechain scratch capacity is too small".to_owned());
    }
    effect
        .external_auxiliary_scratch
        .resize(total_sample_count, 0.0);
    effect.external_auxiliary_scratch.fill(0.0);
    for sidechain in &effect.sidechain_inputs {
        let bus_position = effect
            .external_auxiliary_input_buses
            .iter()
            .position(|(bus_index, channel_count)| {
                *bus_index == sidechain.bus_index && *channel_count == sidechain.channel_count
            })
            .ok_or_else(|| "CLAP Mixer sidechain bus is unavailable".to_owned())?;
        let source = &sidechain.stereo_scratch[..sample_count];
        let destination_start = bus_position
            .checked_mul(sample_count)
            .ok_or_else(|| "CLAP Mixer sidechain offset overflow".to_owned())?;
        let destination_end = destination_start
            .checked_add(sample_count)
            .filter(|end| *end <= effect.external_auxiliary_scratch.len())
            .ok_or_else(|| "CLAP Mixer sidechain scratch is too small".to_owned())?;
        for (destination, source) in effect.external_auxiliary_scratch
            [destination_start..destination_end]
            .iter_mut()
            .zip(source)
        {
            if source.is_finite() {
                *destination += *source;
            }
        }
    }
    Ok(())
}

fn mixer_effect_parameter_changes_for_block(
    automation: &[PreparedParameterAutomation],
    block_start_frame: u64,
    frame_count: usize,
    loop_range: Option<(u64, u64)>,
) -> Result<Vec<(u32, u32, f64)>, String> {
    let mut changes = Vec::new();
    let mut append_segment =
        |pattern_frame: u64, output_offset: usize, segment_count: usize| -> Result<(), String> {
            for lane in automation {
                for_each_parameter_change_for_block(
                    lane,
                    pattern_frame,
                    segment_count,
                    |offset, value| {
                        let frame_offset = usize::try_from(offset)
                            .ok()
                            .and_then(|offset| offset.checked_add(output_offset))
                            .and_then(|offset| u32::try_from(offset).ok())
                            .ok_or_else(|| {
                                "Mixer effect automation offset exceeds the host range".to_owned()
                            })?;
                        changes.push((frame_offset, lane.parameter_id, value));
                        Ok(())
                    },
                )?;
            }
            Ok(())
        };
    if let Some((loop_start_frame, loop_frames)) = loop_range {
        for (pattern_frame, output_offset, segment_count) in
            looped_parameter_automation_segments_at_range(
                block_start_frame,
                frame_count,
                loop_start_frame,
                loop_frames,
            )?
        {
            append_segment(pattern_frame, output_offset, segment_count)?;
        }
    } else {
        append_segment(block_start_frame, 0, frame_count)?;
    }
    if changes.len() > MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK {
        return Err(format!(
            "Mixer effect automation exceeds {} changes in one processing block",
            MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK
        ));
    }
    Ok(changes)
}

fn update_playlist_mixer_effect_sidechain_inputs(
    effect: &mut PlaylistMixerEffectStream,
    sources: &[(usize, bool, bool)],
    sample_rate: f64,
) -> Result<(), String> {
    let auxiliary_buses = if let Some(buffers) = effect.buffers.as_ref() {
        buffers
            .inputs
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, bus)| bus.active && matches!(bus.channels.len(), 1 | 2))
            .map(|(bus_index, bus)| (bus_index, bus.channels.len()))
            .collect::<Vec<_>>()
    } else {
        effect.external_auxiliary_input_buses.clone()
    };
    if effect.external_processor.is_some() && effect.external_auxiliary_input_source_map.is_none() {
        let active_source_count = sources
            .iter()
            .filter(|(_, saved_enabled, live_control)| *saved_enabled || *live_control)
            .count();
        if active_source_count > auxiliary_buses.len() {
            return Err(format!(
                "Mixer insert {} CLAP effect slot {} has {active_source_count} active sidechain routes but only {} supported auxiliary inputs",
                effect.insert_ordinal,
                effect.slot_index + 1,
                auxiliary_buses.len()
            ));
        }
    }
    let sidechain_pairs = if let Some(mapping) = &effect.external_auxiliary_input_source_map {
        let mut mapped_sources = Vec::new();
        mapped_sources
            .try_reserve_exact(mapping.len())
            .map_err(|error| format!("could not allocate Mixer sidechain mappings: {error}"))?;
        for (source_track, bus_index) in mapping {
            let Some((source_track, saved_enabled, live_control)) = sources
                .iter()
                .copied()
                .find(|source| source.0 == *source_track)
            else {
                continue;
            };
            let Some((bus_index, channel_count)) = auxiliary_buses
                .iter()
                .copied()
                .find(|(available_bus, _)| available_bus == bus_index)
            else {
                return Err(format!(
                    "CLAP Mixer insert {} effect slot {} maps source insert {source_track} to unavailable auxiliary input {bus_index}",
                    effect.insert_ordinal,
                    effect.slot_index + 1
                ));
            };
            mapped_sources.push((
                (bus_index, channel_count),
                (source_track, saved_enabled, live_control),
            ));
        }
        mapped_sources
    } else {
        auxiliary_buses
            .into_iter()
            .zip(sources.iter().copied())
            .collect()
    };
    let smoothing_frames = (sample_rate * f64::from(MIXER_EFFECT_MIX_SMOOTHING_SECONDS))
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    let scratch_sample_count = effect
        .block_size
        .checked_mul(2)
        .ok_or_else(|| "Mixer sidechain scratch size overflow".to_owned())?;
    let previous_inputs = std::mem::take(&mut effect.sidechain_inputs);
    let mut previous_inputs = previous_inputs
        .into_iter()
        .map(|input| ((input.bus_index, input.source_track), input))
        .collect::<BTreeMap<_, _>>();
    let mut sidechain_inputs = Vec::new();
    sidechain_inputs
        .try_reserve_exact(sidechain_pairs.len())
        .map_err(|error| format!("could not allocate Mixer sidechain inputs: {error}"))?;
    // VST3 buses and unmapped CLAP ports retain stable source-order pairing. Explicit
    // CLAP maps may route several source inserts into one auxiliary port.
    for ((bus_index, channel_count), (source_track, saved_enabled, live_control)) in sidechain_pairs
    {
        let mut input = previous_inputs
            .remove(&(bus_index, source_track))
            .unwrap_or_else(|| PlaylistMixerEffectSidechainInput {
                bus_index,
                source_track,
                channel_count,
                saved_enabled,
                live_control,
                live_enabled_target: None,
                enabled_ramp: LiveWetMixRamp::new(
                    if saved_enabled { 1.0 } else { 0.0 },
                    smoothing_frames,
                ),
                delay: StereoFrameDelay::default(),
                stereo_scratch: vec![0.0; scratch_sample_count],
            });
        input.channel_count = channel_count;
        input.stereo_scratch.resize(scratch_sample_count, 0.0);
        input.saved_enabled = saved_enabled;
        input.live_control = live_control;
        input.live_enabled_target = None;
        if !live_control {
            input
                .enabled_ramp
                .set_target(if saved_enabled { 1.0 } else { 0.0 });
        }
        sidechain_inputs.push(input);
    }
    effect.sidechain_inputs = sidechain_inputs;
    Ok(())
}

fn queue_live_vst3_input_event(stream: &mut PlaylistPluginStream, event: ScheduledMidiEvent) {
    let mut pending_events = stream.render.events.split_off(stream.next_event);
    pending_events.push(event);
    pending_events.sort_by_key(|event| (event.frame, event.priority));
    stream.render.events.extend(pending_events);
}

fn plugin_latency_in_output_frames(
    channel_id: u16,
    latency_samples: u32,
    plugin_sample_rate: f64,
    output_sample_rate: u32,
) -> Result<u64, String> {
    if latency_samples == 0 {
        return Ok(0);
    }
    if !plugin_sample_rate.is_finite() || plugin_sample_rate <= 0.0 {
        return Err(format!(
            "VST3 channel {channel_id} reported latency with an invalid sample rate"
        ));
    }
    let output_frames =
        (f64::from(latency_samples) * f64::from(output_sample_rate) / plugin_sample_rate).ceil();
    if !output_frames.is_finite() || output_frames > u64::MAX as f64 {
        return Err(format!(
            "VST3 channel {channel_id} reported latency outside the renderable range"
        ));
    }
    Ok(output_frames as u64)
}

fn refresh_plugin_latency_report(
    plugin_id: u64,
    plugin: &Arc<Mutex<Plugin>>,
) -> Result<Vst3LatencyReport, String> {
    let mut plugin = plugin
        .lock()
        .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
    let was_processing = plugin.is_processing();
    if was_processing {
        plugin.stop_processing().map_err(|error| {
            format!("could not stop VST3 instance {plugin_id} for a latency refresh: {error}")
        })?;
    }
    let report = Vst3LatencyReport {
        plugin_id,
        latency_samples: plugin.latency_samples(),
        sample_rate_hz: plugin.sample_rate(),
    };
    if was_processing {
        plugin.start_processing().map_err(|error| {
            format!("could not resume VST3 instance {plugin_id} after a latency refresh: {error}")
        })?;
    }
    Ok(report)
}

fn pdc_aligned_output_frames(
    content_frames: u64,
    maximum_latency_frames: u64,
) -> Result<u64, String> {
    content_frames
        .checked_add(maximum_latency_frames)
        .ok_or_else(|| "PDC-aligned Playlist length overflow".to_owned())
}

fn pdc_render_start_trim_frames(
    automatic_pdc_enabled: bool,
    instrument_latency_frames: u64,
    mixer_effect_latency_frames: u64,
) -> u64 {
    if automatic_pdc_enabled {
        instrument_latency_frames.saturating_add(mixer_effect_latency_frames)
    } else {
        0
    }
}

fn pdc_mixer_stem_start_trim_frames(
    automatic_pdc_enabled: bool,
    instrument_latency_frames: u64,
    meter_input_latency_frames: u64,
    post_effect_latency_frames: u64,
) -> u64 {
    if automatic_pdc_enabled {
        instrument_latency_frames
            .saturating_add(meter_input_latency_frames)
            .saturating_add(post_effect_latency_frames)
    } else {
        0
    }
}

fn plan_pdc_delays(latencies: &[u64]) -> (u64, Vec<u64>) {
    let maximum = latencies.iter().copied().max().unwrap_or(0);
    let delays = latencies
        .iter()
        .map(|latency| maximum.saturating_sub(*latency))
        .collect();
    (maximum, delays)
}

fn update_stereo_delay_lines(
    lines: &mut Vec<StereoFrameDelay>,
    delay_frames: impl IntoIterator<Item = u64>,
) -> Result<(), String> {
    let previous = std::mem::take(lines);
    let mut previous = previous.into_iter();
    let delays = delay_frames.into_iter();
    let (minimum_count, _) = delays.size_hint();
    let mut updated = Vec::new();
    updated
        .try_reserve_exact(minimum_count)
        .map_err(|error| format!("could not allocate Mixer PDC delays: {error}"))?;
    for frames in delays {
        let mut line = previous.next().unwrap_or_default();
        line.set_delay_frames(frames)?;
        updated.push(line);
    }
    *lines = updated;
    Ok(())
}

fn configure_mixer_effect_pdc(
    processor: &mut Vst3PlaylistStreamProcessor,
    routing: &MixerRouteAudibility,
    instrument_delay_frames: u64,
) -> Result<(), String> {
    if processor.mixer_effect_chains.is_empty() {
        return Ok(());
    }

    fn add_delay_frames(total: &mut u64, frames: u64, copies: u64) -> Result<(), String> {
        let addition = frames
            .checked_mul(copies)
            .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        *total = total
            .checked_add(addition)
            .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
        Ok(())
    }

    let mut effect_latency_by_track = HashMap::new();
    let mut master_effect_latency_frames = 0_u64;
    for chain in &processor.mixer_effect_chains {
        if chain.insert_ordinal == 0 {
            master_effect_latency_frames = master_effect_latency_frames
                .checked_add(chain.latency_frames)
                .ok_or_else(|| "Mixer Master effect latency overflow".to_owned())?;
        } else {
            effect_latency_by_track.insert(chain.insert_ordinal, chain.latency_frames);
        }
    }
    let sidechain_routes = processor
        .mixer_effects
        .iter()
        .flat_map(|effect| {
            effect
                .sidechain_inputs
                .iter()
                .map(move |sidechain| (sidechain.source_track, effect.insert_ordinal))
        })
        .collect::<BTreeSet<_>>();
    let pdc_plan = routing.mixer_insert_pdc_plan(&effect_latency_by_track, &sidechain_routes)?;
    let master_input_latency_frames = pdc_plan.master_input_latency_frames;
    let total_effect_latency_frames = master_input_latency_frames
        .checked_add(master_effect_latency_frames)
        .ok_or_else(|| "Mixer effect PDC latency overflow".to_owned())?;

    let mut route_delay_frames = if processor.automatic_pdc_enabled {
        master_input_latency_frames
    } else {
        0
    };
    if processor.automatic_pdc_enabled {
        for input_latency_frames in pdc_plan.input_latency_frames.values() {
            add_delay_frames(&mut route_delay_frames, *input_latency_frames, 2)?;
        }
    }
    for chain in processor
        .mixer_effect_chains
        .iter()
        .filter(|chain| chain.insert_ordinal != 0)
    {
        let output_latency_frames = pdc_plan
            .output_latency_frames
            .get(&chain.insert_ordinal)
            .copied()
            .unwrap_or_default();
        let output_alignment_frames = if processor.automatic_pdc_enabled
            && pdc_plan
                .tracks_reaching_master
                .contains(&chain.insert_ordinal)
        {
            master_input_latency_frames
                .checked_sub(output_latency_frames)
                .ok_or_else(|| "Mixer output path exceeds its planned PDC latency".to_owned())?
        } else {
            0
        };
        if processor.automatic_pdc_enabled {
            add_delay_frames(&mut route_delay_frames, chain.latency_frames, 1)?;
            add_delay_frames(&mut route_delay_frames, output_alignment_frames, 1)?;
        }
        for (track, _) in &chain.route_plan.insert_effect_input_transforms {
            if *track == chain.insert_ordinal {
                continue;
            }
            let input_latency_frames = pdc_plan
                .input_latency_frames
                .get(track)
                .copied()
                .ok_or_else(|| {
                    format!("Mixer insert {track} has no latency entry in the route plan")
                })?;
            let branch_delay_frames = if processor.automatic_pdc_enabled {
                input_latency_frames
                    .checked_sub(output_latency_frames)
                    .ok_or_else(|| "Mixer send path exceeds its planned PDC latency".to_owned())?
            } else {
                0
            };
            add_delay_frames(&mut route_delay_frames, branch_delay_frames, 2)?;
        }
    }
    if processor.automatic_pdc_enabled {
        for effect in &processor.mixer_effects {
            let destination_input_latency = if effect.insert_ordinal == 0 {
                master_input_latency_frames
            } else {
                pdc_plan
                    .input_latency_frames
                    .get(&effect.insert_ordinal)
                    .copied()
                    .ok_or_else(|| {
                        format!(
                            "Mixer insert {} has no latency entry for its sidechain inputs",
                            effect.insert_ordinal
                        )
                    })?
            };
            for sidechain in &effect.sidechain_inputs {
                let source_output_latency = pdc_plan
                    .output_latency_frames
                    .get(&sidechain.source_track)
                    .copied()
                    .ok_or_else(|| {
                        format!(
                            "Mixer insert {} has no latency entry for sidechain source {}",
                            effect.insert_ordinal, sidechain.source_track
                        )
                    })?;
                let branch_delay = destination_input_latency
                    .checked_sub(source_output_latency)
                    .ok_or_else(|| {
                        "Mixer sidechain path exceeds its planned PDC latency".to_owned()
                    })?;
                add_delay_frames(&mut route_delay_frames, branch_delay, 1)?;
            }
        }
    }

    let total_delay_frames = instrument_delay_frames
        .checked_add(processor.mixer_effect_dry_delay_frames)
        .and_then(|frames| frames.checked_add(route_delay_frames))
        .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
    let delay_bytes = total_delay_frames
        .checked_mul(std::mem::size_of::<[f32; 2]>() as u64)
        .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
    if delay_bytes > MAX_PDC_DELAY_BYTES {
        return Err(format!(
            "Mixer route PDC needs {delay_bytes} bytes of delay storage, exceeding the 128 MiB limit"
        ));
    }

    processor
        .mixer_effect_output_delay
        .set_delay_frames(if processor.automatic_pdc_enabled {
            master_input_latency_frames
        } else {
            0
        })?;
    let meter_input_delays = (0..crate::audio::MIXER_TRACK_METER_COUNT).map(|track| {
        if processor.automatic_pdc_enabled {
            pdc_plan
                .input_latency_frames
                .get(&track)
                .copied()
                .unwrap_or_default()
        } else {
            0
        }
    });
    update_stereo_delay_lines(
        &mut processor.mixer_effect_meter_input_delays,
        meter_input_delays,
    )?;
    let effect_input_delays = (0..crate::audio::MIXER_TRACK_METER_COUNT).map(|track| {
        if processor.automatic_pdc_enabled {
            pdc_plan
                .input_latency_frames
                .get(&track)
                .copied()
                .unwrap_or_default()
        } else {
            0
        }
    });
    update_stereo_delay_lines(
        &mut processor.mixer_effect_input_delays,
        effect_input_delays,
    )?;
    for effect in &mut processor.mixer_effects {
        let destination_input_latency = if effect.insert_ordinal == 0 {
            master_input_latency_frames
        } else {
            pdc_plan
                .input_latency_frames
                .get(&effect.insert_ordinal)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "Mixer insert {} has no latency entry for its sidechain inputs",
                        effect.insert_ordinal
                    )
                })?
        };
        for sidechain in &mut effect.sidechain_inputs {
            let source_output_latency = pdc_plan
                .output_latency_frames
                .get(&sidechain.source_track)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "Mixer insert {} has no latency entry for sidechain source {}",
                        effect.insert_ordinal, sidechain.source_track
                    )
                })?;
            let branch_delay = if processor.automatic_pdc_enabled {
                destination_input_latency
                    .checked_sub(source_output_latency)
                    .ok_or_else(|| {
                        "Mixer sidechain path exceeds its planned PDC latency".to_owned()
                    })?
            } else {
                0
            };
            sidechain.delay.set_delay_frames(branch_delay)?;
        }
    }
    let post_effect_stem_delays = (0..crate::audio::MIXER_TRACK_METER_COUNT)
        .map(|track| {
            let output_latency_frames = pdc_plan
                .output_latency_frames
                .get(&track)
                .copied()
                .unwrap_or_default();
            if processor.automatic_pdc_enabled && pdc_plan.tracks_reaching_master.contains(&track) {
                master_input_latency_frames
                    .checked_sub(output_latency_frames)
                    .ok_or_else(|| "post-effect Mixer output exceeds planned PDC".to_owned())
            } else {
                Ok(0)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    update_stereo_delay_lines(
        &mut processor.mixer_post_effect_stem_delays,
        post_effect_stem_delays,
    )?;

    for chain in &mut processor.mixer_effect_chains {
        if chain.insert_ordinal == 0 {
            continue;
        }
        let meter_transforms = chain
            .route_plan
            .insert_meter_transforms
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>();
        let output_latency_frames = pdc_plan
            .output_latency_frames
            .get(&chain.insert_ordinal)
            .copied()
            .unwrap_or_default();
        let output_alignment_frames = if processor.automatic_pdc_enabled
            && pdc_plan
                .tracks_reaching_master
                .contains(&chain.insert_ordinal)
        {
            master_input_latency_frames
                .checked_sub(output_latency_frames)
                .ok_or_else(|| "Mixer output path exceeds its planned PDC latency".to_owned())?
        } else {
            0
        };
        chain
            .output_alignment_delay
            .set_delay_frames(output_alignment_frames)?;
        chain
            .dry_reference_delay
            .set_delay_frames(chain.latency_frames)?;
        let previous_downstream_delays = std::mem::take(&mut chain.downstream_delays);
        let mut previous_downstream_delays = previous_downstream_delays
            .into_iter()
            .map(|delay| (delay.track, delay))
            .collect::<HashMap<_, _>>();
        let mut downstream_delays = Vec::new();
        downstream_delays
            .try_reserve_exact(chain.route_plan.insert_effect_input_transforms.len())
            .map_err(|error| format!("could not allocate Mixer send PDC routes: {error}"))?;
        for (track, transform) in &chain.route_plan.insert_effect_input_transforms {
            if *track == chain.insert_ordinal {
                continue;
            }
            let input_latency_frames = pdc_plan
                .input_latency_frames
                .get(track)
                .copied()
                .ok_or_else(|| {
                    format!("Mixer insert {track} has no latency entry in the route plan")
                })?;
            let branch_delay_frames = if processor.automatic_pdc_enabled {
                input_latency_frames
                    .checked_sub(output_latency_frames)
                    .ok_or_else(|| "Mixer send path exceeds its planned PDC latency".to_owned())?
            } else {
                0
            };
            let meter_transform = meter_transforms.get(track).copied().ok_or_else(|| {
                format!("Mixer insert {track} has no meter transform for its send route")
            })?;
            let mut delays = previous_downstream_delays.remove(track).unwrap_or_else(|| {
                PlaylistMixerEffectBranchDelay {
                    track: *track,
                    meter_transform,
                    effect_input_transform: *transform,
                    meter_delay: StereoFrameDelay::default(),
                    effect_input_delay: StereoFrameDelay::default(),
                }
            });
            delays.meter_transform = meter_transform;
            delays.effect_input_transform = *transform;
            delays.meter_delay.set_delay_frames(branch_delay_frames)?;
            delays
                .effect_input_delay
                .set_delay_frames(branch_delay_frames)?;
            downstream_delays.push(delays);
        }
        chain.downstream_delays = downstream_delays;
    }
    processor.mixer_effect_route_delay_frames = route_delay_frames;
    processor.mixer_effect_pdc_latency_frames = total_effect_latency_frames;
    Ok(())
}

impl PlaylistPluginStream {
    fn render_output_frames(
        &mut self,
        frame_count: usize,
        output: &mut Vec<Vec<f32>>,
    ) -> Result<(), String> {
        let output_samples = frame_count
            .checked_mul(2)
            .ok_or_else(|| "VST3 Playlist output block size overflow".to_owned())?;
        if output.len() != self.output_buses.len() {
            output.clear();
            output
                .try_reserve(self.output_buses.len())
                .map_err(|error| format!("could not allocate VST3 output bus blocks: {error}"))?;
            output.resize_with(self.output_buses.len(), Vec::new);
        }
        for bus_output in output.iter_mut() {
            bus_output.clear();
            bus_output
                .try_reserve(output_samples)
                .map_err(|error| format!("could not allocate VST3 Playlist output: {error}"))?;
        }
        while self
            .output_buses
            .iter()
            .any(|bus| bus.pending_output.len() < frame_count)
            && !self.resampler_finished
        {
            if self.live_performance || self.source_frame < self.render.total_frames {
                let source_count = if self.live_performance {
                    self.render.block_size
                } else {
                    (self.render.total_frames - self.source_frame)
                        .min(self.render.block_size as u64) as usize
                };
                let block_end = self.source_frame + source_count as u64;
                self.queue_pattern_loop_events_before(block_end)?;
                let mut buffers = AudioBuffers::new(
                    0,
                    self.render.output_channels,
                    source_count,
                    self.render.sample_rate,
                );
                {
                    let plugin_cell = Arc::clone(&self.plugin);
                    let mut plugin = plugin_cell
                        .lock()
                        .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                    if let Some(pattern_loop) = self.pattern_loop.as_ref() {
                        if pattern_loop.loop_start_frame == 0 {
                            set_looped_parameter_automation_for_block(
                                &mut plugin,
                                &self.render.parameter_automation,
                                self.source_frame,
                                source_count,
                                pattern_loop.loop_frames,
                            )?;
                        } else {
                            set_looped_parameter_automation_for_block_at_range(
                                &mut plugin,
                                &self.render.parameter_automation,
                                self.source_frame,
                                source_count,
                                pattern_loop.loop_start_frame,
                                pattern_loop.loop_frames,
                            )?;
                        }
                    } else {
                        set_parameter_automation_for_block(
                            &mut plugin,
                            &self.render.parameter_automation,
                            self.source_frame,
                            source_count,
                        )?;
                    }
                    while let Some(event) = self.render.events.get(self.next_event)
                        && event.frame < block_end
                    {
                        let event = *event;
                        let offset = event.frame.saturating_sub(self.source_frame) as i32;
                        send_scheduled_midi_event(
                            &mut plugin,
                            event.event,
                            offset,
                            self.render.pitch_offset_cents,
                            event.fine_pitch_offset_cents,
                            ScheduledVst3Note {
                                note_id: event.note_id,
                                note_pan: event.note_pan,
                                note_mod_x: event.note_mod_x,
                                note_mod_y: event.note_mod_y,
                                note_release: event.note_release,
                                supports_pan: self.render.supports_note_pan,
                                supports_volume: self.render.supports_note_volume,
                                supports_brightness: self.render.supports_note_brightness,
                                mod_y_expression: self.render.mod_y_expression,
                                layer_gain: event.layer_gain,
                                layer_pan_raw: event.layer_pan_raw,
                            },
                        )?;
                        if event.performance_track_id.is_some() {
                            match event.event {
                                MidiEvent::NoteOn { .. } => {
                                    self.active_performance_notes.insert(event.note_id, event);
                                }
                                MidiEvent::NoteOff { .. } => {
                                    self.active_performance_notes.remove(&event.note_id);
                                }
                                _ => {}
                            }
                        }
                        match event.event {
                            MidiEvent::NoteOn { velocity, .. } if velocity > 0 => {
                                self.active_pattern_note_ids.insert(event.note_id);
                            }
                            MidiEvent::NoteOn { .. } | MidiEvent::NoteOff { .. } => {
                                self.active_pattern_note_ids.remove(&event.note_id);
                            }
                            _ => {}
                        }
                        self.next_event += 1;
                    }
                    self.process_live_midi_events(&mut plugin)?;
                    if self.next_event > 4096 {
                        self.render.events.drain(..self.next_event);
                        self.next_event = 0;
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
                    let mut first_channel = 0usize;
                    for (bus_index, channel_count) in
                        self.render.output_bus_channels.iter().copied().enumerate()
                    {
                        let end_channel = first_channel
                            .checked_add(channel_count)
                            .ok_or_else(|| "VST3 output bus channel range overflow".to_owned())?;
                        let bus_channels = buffers
                            .outputs
                            .get(first_channel..end_channel)
                            .ok_or_else(|| {
                                "VST3 output bus layout exceeds the buffers".to_owned()
                            })?;
                        let output_block = self.output_buses[bus_index]
                            .resampler
                            .push_outputs_with_bus_channels(
                                bus_channels,
                                source_count,
                                &[channel_count],
                            )?;
                        self.output_buses[bus_index].push_interleaved(&output_block)?;
                        first_channel = end_channel;
                    }
                    if first_channel != buffers.outputs.len() {
                        return Err("VST3 output bus layout left unassigned channels".to_owned());
                    }
                }
                self.source_frame = block_end;
                if self.pattern_loop.is_none() {
                    prune_parameter_automation_before(
                        &mut self.render.parameter_automation,
                        self.source_frame,
                    );
                }
            } else {
                for bus in &mut self.output_buses {
                    let output_block = bus.resampler.finish()?;
                    bus.push_interleaved(&output_block)?;
                }
                self.resampler_finished = true;
            }
        }
        for (bus_index, bus) in self.output_buses.iter_mut().enumerate() {
            for _ in 0..frame_count {
                let frame = bus.pending_output.pop_front().unwrap_or([0.0, 0.0]);
                output[bus_index].extend_from_slice(&frame);
            }
        }
        Ok(())
    }

    fn process_live_midi_events(&mut self, plugin: &mut Plugin) -> Result<(), String> {
        let stopped_generation = self.live_midi_stopped_generation.load(Ordering::Acquire);
        self.release_stopped_live_midi(plugin, stopped_generation)?;
        while let Ok(queued) = self.live_midi_receiver.try_recv() {
            if queued.generation <= stopped_generation {
                continue;
            }
            let mut event = queued.event;
            if let MidiEvent::NoteOn {
                channel,
                note,
                velocity: 0,
            } = event
            {
                event = MidiEvent::NoteOff {
                    channel,
                    note,
                    velocity: 0,
                };
            }
            if self.note_pitch_offset_semitones != 0 {
                match &mut event {
                    MidiEvent::NoteOn { note, .. }
                    | MidiEvent::NoteOff { note, .. }
                    | MidiEvent::PolyAftertouch { note, .. } => {
                        *note = (i16::from(*note) + self.note_pitch_offset_semitones).clamp(0, 127)
                            as u8;
                    }
                    _ => {}
                }
            }
            let release = match event {
                MidiEvent::NoteOff { velocity, .. } => velocity,
                _ => 0,
            };
            let live_note = ScheduledVst3Note {
                note_id: queued.note_id,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: release,
                supports_pan: self.render.supports_note_pan,
                supports_volume: self.render.supports_note_volume,
                supports_brightness: self.render.supports_note_brightness,
                mod_y_expression: self.render.mod_y_expression,
                layer_gain: queued.layer_gain,
                layer_pan_raw: queued.layer_pan_raw,
            };
            let note_off_context = match event {
                MidiEvent::NoteOff { channel, note, .. } => self
                    .live_midi_active_notes
                    .get_mut(&(queued.generation, channel.as_index(), note))
                    .and_then(VecDeque::pop_front),
                _ => None,
            };
            let mut event_note = note_off_context.unwrap_or(live_note);
            if matches!(event, MidiEvent::NoteOff { .. }) {
                event_note.note_release = release;
            }
            match event {
                MidiEvent::NoteOn { .. } | MidiEvent::NoteOff { .. } => {
                    send_scheduled_midi_event(
                        plugin,
                        event,
                        0,
                        self.render.pitch_offset_cents,
                        0,
                        event_note,
                    )?;
                }
                _ => plugin
                    .send_midi_event_at(event, 0)
                    .map_err(|error| error.to_string())?,
            }
            match event {
                MidiEvent::NoteOn {
                    channel,
                    note,
                    velocity,
                } if velocity > 0 => {
                    self.live_midi_active_notes
                        .entry((queued.generation, channel.as_index(), note))
                        .or_default()
                        .push_back(live_note);
                }
                MidiEvent::NoteOff { channel, note, .. } => {
                    let key = (queued.generation, channel.as_index(), note);
                    if self
                        .live_midi_active_notes
                        .get(&key)
                        .is_some_and(VecDeque::is_empty)
                    {
                        self.live_midi_active_notes.remove(&key);
                    }
                }
                MidiEvent::ControlChange {
                    channel,
                    controller: 64,
                    value,
                } if value >= 64 => {
                    self.live_midi_sustain_channels
                        .entry(queued.generation)
                        .or_default()
                        .insert(channel.as_index());
                }
                MidiEvent::ControlChange {
                    channel,
                    controller: 64 | 121,
                    ..
                } => {
                    if let Some(channels) =
                        self.live_midi_sustain_channels.get_mut(&queued.generation)
                    {
                        channels.remove(&channel.as_index());
                    }
                }
                MidiEvent::ControlChange {
                    channel,
                    controller,
                    ..
                } if controller == 120 || (123..=127).contains(&controller) => {
                    self.live_midi_active_notes
                        .retain(|(generation, active_channel, _), _| {
                            *generation != queued.generation
                                || *active_channel != channel.as_index()
                        });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn release_stopped_live_midi(
        &mut self,
        plugin: &mut Plugin,
        stopped_generation: u64,
    ) -> Result<(), String> {
        let sustain_channels = self
            .live_midi_sustain_channels
            .iter()
            .filter(|(generation, _)| **generation <= stopped_generation)
            .flat_map(|(generation, channels)| {
                channels.iter().map(|channel| (*generation, *channel))
            })
            .collect::<Vec<_>>();
        for (generation, channel) in sustain_channels {
            if let Some(channel) = MidiChannel::from_index(channel) {
                plugin
                    .send_midi_event_at(
                        MidiEvent::ControlChange {
                            channel,
                            controller: 64,
                            value: 0,
                        },
                        0,
                    )
                    .map_err(|error| error.to_string())?;
            }
            if let Some(channels) = self.live_midi_sustain_channels.get_mut(&generation) {
                channels.remove(&channel);
                if channels.is_empty() {
                    self.live_midi_sustain_channels.remove(&generation);
                }
            }
        }
        let notes = self
            .live_midi_active_notes
            .iter()
            .filter(|((generation, _, _), _)| *generation <= stopped_generation)
            .map(|(&(generation, channel, note), active)| {
                (
                    generation,
                    channel,
                    note,
                    active.iter().copied().collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        for (generation, channel, note, active_notes) in notes {
            if let Some(channel) = MidiChannel::from_index(channel) {
                for mut active_note in active_notes {
                    active_note.note_release = 0;
                    send_scheduled_midi_event(
                        plugin,
                        MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        },
                        0,
                        self.render.pitch_offset_cents,
                        0,
                        active_note,
                    )?;
                }
            }
            self.live_midi_active_notes
                .remove(&(generation, channel, note));
        }
        self.live_midi_sustain_channels
            .retain(|generation, _| *generation > stopped_generation);
        self.live_midi_active_notes
            .retain(|(generation, _, _), _| *generation > stopped_generation);
        Ok(())
    }

    fn finish_live_midi(&mut self, plugin: &mut Plugin) -> Result<(), String> {
        self.live_midi_stopped_generation
            .store(u64::MAX, Ordering::Release);
        while self.live_midi_receiver.try_recv().is_ok() {}
        self.release_stopped_live_midi(plugin, u64::MAX)
    }

    fn queue_pattern_loop_events_before(&mut self, block_end: u64) -> Result<(), String> {
        let Some(queue) = &mut self.pattern_loop else {
            return Ok(());
        };
        if queue.loop_frames == 0 || queue.template_events.is_empty() {
            return Ok(());
        }
        loop {
            let cycle_start = queue
                .loop_frames
                .checked_mul(queue.next_cycle.saturating_sub(1))
                .and_then(|offset| {
                    queue
                        .loop_start_frame
                        .checked_add(queue.loop_frames)?
                        .checked_add(offset)
                })
                .ok_or_else(|| "VST3 Playlist loop position overflow".to_owned())?;
            let first_frame = cycle_start
                .checked_add(
                    queue
                        .first_event_frame
                        .saturating_sub(queue.loop_start_frame),
                )
                .ok_or_else(|| "VST3 Playlist loop event position overflow".to_owned())?;
            if first_frame >= block_end {
                break;
            }
            let cycle_offset = cycle_start
                .checked_sub(queue.loop_start_frame)
                .ok_or_else(|| "VST3 Playlist loop offset underflow".to_owned())?;
            queue.repeat_cycle(cycle_offset)?;
            self.render
                .events
                .extend(queue.repeated_events.iter().copied());
            self.render.events[self.next_event..]
                .sort_unstable_by_key(|event| (event.frame, event.priority));
            queue.next_cycle = queue
                .next_cycle
                .checked_add(1)
                .ok_or_else(|| "VST3 Pattern loop count overflow".to_owned())?;
        }
        Ok(())
    }
}

fn playlist_output_frame_to_plugin_frame(
    output_frame: u64,
    plugin_sample_rate: f64,
    output_sample_rate: u32,
) -> Result<u64, String> {
    if !plugin_sample_rate.is_finite() || plugin_sample_rate <= 0.0 || output_sample_rate == 0 {
        return Err("Playlist VST3 loop requires valid plug-in and output sample rates".to_owned());
    }
    let plugin_frame = output_frame as f64 * plugin_sample_rate / f64::from(output_sample_rate);
    if !plugin_frame.is_finite() || plugin_frame < 0.0 || plugin_frame > u64::MAX as f64 {
        return Err("Playlist VST3 loop position is outside the renderable range".to_owned());
    }
    Ok(plugin_frame.round() as u64)
}

fn playlist_loop_midi_events(
    events: &[ScheduledMidiEvent],
    loop_start_frame: u64,
    loop_end_frame: u64,
) -> (Vec<ScheduledMidiEvent>, Vec<ScheduledMidiEvent>) {
    let active_before = |end_frame: u64| {
        let mut active = HashMap::new();
        for event in events.iter().take_while(|event| event.frame < end_frame) {
            match event.event {
                MidiEvent::NoteOn { velocity, .. } if velocity > 0 => {
                    active.insert(event.note_id, *event);
                }
                MidiEvent::NoteOn { .. } | MidiEvent::NoteOff { .. } => {
                    active.remove(&event.note_id);
                }
                _ => {}
            }
        }
        active
    };
    let active_at_start = active_before(loop_start_frame);
    let active_at_end = active_before(loop_end_frame);
    let mut initial_events = events
        .iter()
        .copied()
        .filter(|event| event.frame < loop_end_frame)
        .collect::<Vec<_>>();
    let mut template_events = events
        .iter()
        .copied()
        .filter(|event| loop_start_frame <= event.frame && event.frame < loop_end_frame)
        .collect::<Vec<_>>();
    for note_on in active_at_start.values() {
        if let MidiEvent::NoteOn { velocity, .. } = note_on.event
            && velocity > 0
        {
            template_events.push(ScheduledMidiEvent {
                frame: loop_start_frame,
                ..*note_on
            });
        }
    }
    for note_on in active_at_end.values() {
        if let Some(note_off) = playlist_loop_note_off(note_on, loop_end_frame) {
            initial_events.push(note_off);
            template_events.push(note_off);
        }
    }
    initial_events.sort_unstable_by_key(|event| (event.frame, event.priority));
    template_events.sort_unstable_by_key(|event| (event.frame, event.priority));
    (initial_events, template_events)
}

fn playlist_loop_note_off(note_on: &ScheduledMidiEvent, frame: u64) -> Option<ScheduledMidiEvent> {
    let MidiEvent::NoteOn { channel, note, .. } = note_on.event else {
        return None;
    };
    Some(ScheduledMidiEvent {
        frame,
        priority: 0,
        event: MidiEvent::NoteOff {
            channel,
            note,
            velocity: 0,
        },
        ..*note_on
    })
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
    /// Number of audio input buses advertised by this VST3 class.
    pub audio_inputs: u32,
    /// Number of audio output buses advertised by this VST3 class.
    pub audio_outputs: u32,
    /// Latency most recently reported by `IAudioProcessor::getLatencySamples`.
    pub latency_samples: u32,
    /// User-configured manual latency offset applied only to PDC planning.
    pub manual_latency_offset_samples: i32,
    /// Sample rate configured for this plug-in instance when the report was read.
    pub sample_rate_hz: f64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Vst3HostServiceChanges {
    pub io_changed: bool,
    pub latency_changed: bool,
    pub latency_changed_while_processing: bool,
    pub latency_changed_plugin_ids: BTreeSet<u64>,
}

/// A latency report refreshed by an active Playlist processor at a safe processing boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst3LatencyReport {
    pub plugin_id: u64,
    pub latency_samples: u32,
    pub sample_rate_hz: f64,
}

impl HostedPluginInfo {
    /// Convert the reported latency to milliseconds using the plug-in's configured rate.
    pub fn latency_milliseconds(&self) -> Option<f64> {
        (self.sample_rate_hz.is_finite() && self.sample_rate_hz > 0.0)
            .then(|| f64::from(self.latency_samples) * 1000.0 / self.sample_rate_hz)
    }

    /// Effective latency used by PDC after the saved per-plugin offset is applied.
    pub fn pdc_latency_samples(&self) -> u32 {
        apply_manual_latency_offset(self.latency_samples, self.manual_latency_offset_samples)
    }
}

struct HostedPlugin {
    info: HostedPluginInfo,
    plugin: Arc<Mutex<Plugin>>,
    editor_open: bool,
}

fn read_32bit_x86_binary(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 64];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if header.starts_with(b"\x7FELF") {
        return header[4] == 1 && header[5] == 1 && header[18] == 3 && header[19] == 0;
    }
    if header.starts_with(b"MZ") {
        let Some(offset_bytes) = header.get(0x3C..0x40) else {
            return false;
        };
        let offset = u64::from(u32::from_le_bytes([
            offset_bytes[0],
            offset_bytes[1],
            offset_bytes[2],
            offset_bytes[3],
        ]));
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut pe_header = [0_u8; 6];
        return file.read_exact(&mut pe_header).is_ok()
            && pe_header[..4] == *b"PE\0\0"
            && pe_header[4] == 0x4C
            && pe_header[5] == 0x01;
    }
    // Thin 32-bit Mach-O. Universal binaries are intentionally left to the native loader,
    // which can select an architecture slice appropriate to this process.
    let magic = &header[..4];
    let is_32bit_macho = matches!(magic, [0xFE, 0xED, 0xFA, 0xCE] | [0xCE, 0xFA, 0xED, 0xFE]);
    if is_32bit_macho {
        let cpu_type_bytes = [header[4], header[5], header[6], header[7]];
        let cpu_type = if magic == [0xCE, 0xFA, 0xED, 0xFE] {
            u32::from_le_bytes(cpu_type_bytes)
        } else {
            u32::from_be_bytes(cpu_type_bytes)
        };
        return cpu_type == 7;
    }
    false
}

#[cfg(target_os = "windows")]
fn find_x86_bundle_binary(path: &Path) -> Option<PathBuf> {
    std::fs::read_dir(path.join("Contents").join("x86-win"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .extension()
                .is_some_and(|extension| extension == "vst3")
                && read_32bit_x86_binary(candidate)
        })
}

#[cfg(target_os = "linux")]
fn find_x86_bundle_binary(path: &Path) -> Option<PathBuf> {
    std::fs::read_dir(path.join("Contents").join("i386-linux"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .extension()
                .is_some_and(|extension| extension == "so")
                && read_32bit_x86_binary(candidate)
        })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn find_x86_bundle_binary(_path: &Path) -> Option<PathBuf> {
    None
}

fn x86_vst3_binary(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return read_32bit_x86_binary(path).then(|| path.to_path_buf());
    }
    if let Ok(native_binary) = vst3_host::discovery::get_vst3_binary_path(path) {
        return read_32bit_x86_binary(&native_binary).then_some(native_binary);
    }
    find_x86_bundle_binary(path)
}

fn x86_helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VST3_HOST_HELPER_X86_PATH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let helper_name = if cfg!(target_os = "windows") {
        "vst3-host-helper-x86.exe"
    } else {
        "vst3-host-helper-x86"
    };
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    [
        Some(directory.to_path_buf()),
        directory.parent().map(Path::to_path_buf),
    ]
    .into_iter()
    .flatten()
    .map(|directory| directory.join(helper_name))
    .find(|path| path.is_file())
}

fn build_isolated_vst3_host(
    sample_rate: f64,
    block_size: usize,
    helper_path: Option<&Path>,
) -> Result<Vst3Host, String> {
    let mut builder = Vst3Host::builder()
        .sample_rate(sample_rate)
        .block_size(block_size)
        .with_process_isolation(true);
    if let Some(helper_path) = helper_path {
        builder = builder.helper_path(helper_path);
    }
    builder.build().map_err(|error| error.to_string())
}

/// Owns active VST3 instances and their native editor windows.
///
/// Plug-ins are loaded on demand. The host does not scan or execute every installed
/// plug-in during startup.
pub struct Vst3HostRuntime {
    host: Vst3Host,
    loaded: Vec<HostedPlugin>,
    mixer_effect_processor_factory: Option<Arc<dyn MixerEffectProcessorFactory>>,
    pending_latency_refresh: BTreeSet<u64>,
    manual_latency_offsets_by_uid: BTreeMap<String, i32>,
    manual_latency_offset_overrides: BTreeSet<u64>,
    next_id: u64,
}

/// Processes a Mixer effect block through a non-VST3 plug-in host.
///
/// External processors receive the interleaved stereo main bus in place. Auxiliary bus layouts
/// are (bus index, channel count) pairs. Auxiliary audio contains one interleaved stereo block per
/// layout entry, in the same order. Parameter changes contain (frame_offset, parameter_id, value)
/// tuples for the current block.
pub trait MixerEffectProcessor: Send {
    fn sample_rate(&self) -> f64;
    fn block_size(&self) -> usize;
    fn input_channels(&self) -> usize;
    fn output_channels(&self) -> usize;
    fn auxiliary_input_buses(&self) -> &[(usize, usize)] {
        &[]
    }
    /// Optional source-insert to auxiliary-port mapping for external processors.
    fn auxiliary_input_source_map(&self) -> Option<&[(usize, usize)]> {
        None
    }
    fn latency_samples(&self) -> u32;
    fn tail_samples(&self) -> Option<u64> {
        None
    }
    fn process_block(
        &mut self,
        samples: &mut [f32],
        auxiliary_input_samples: &[f32],
        parameter_changes: &[(u32, u32, f64)],
    ) -> Result<(), String>;
    fn stop_processing(&mut self) -> Result<(), String>;
}

/// Creates one independent external processor for each prepared Mixer stream.
pub trait MixerEffectProcessorFactory: Send + Sync {
    fn contains(&self, insert_ordinal: usize, slot_index: u16) -> bool;
    fn create(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        sample_rate: u32,
    ) -> Result<Option<Box<dyn MixerEffectProcessor>>, String>;
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
        let host = build_isolated_vst3_host(sample_rate, block_size, None)?;
        Ok(Self {
            host,
            loaded: Vec::new(),
            mixer_effect_processor_factory: None,
            pending_latency_refresh: BTreeSet::new(),
            manual_latency_offsets_by_uid: BTreeMap::new(),
            manual_latency_offset_overrides: BTreeSet::new(),
            next_id: 1,
        })
    }

    /// Install the external processor factory used for CLAP or other non-VST3 Mixer effects.
    pub fn set_mixer_effect_processor_factory(
        &mut self,
        factory: Option<Arc<dyn MixerEffectProcessorFactory>>,
    ) {
        self.mixer_effect_processor_factory = factory;
    }

    /// Install per-class manual latency offsets before loading project plug-ins.
    pub fn set_manual_latency_offsets(&mut self, offsets: &BTreeMap<String, i32>) {
        self.manual_latency_offsets_by_uid = offsets
            .iter()
            .map(|(uid, offset)| (uid.to_ascii_lowercase(), *offset))
            .collect();
        self.manual_latency_offset_overrides.clear();
        for loaded in &mut self.loaded {
            loaded.info.manual_latency_offset_samples = self
                .manual_latency_offsets_by_uid
                .get(&loaded.info.uid.to_ascii_lowercase())
                .copied()
                .unwrap_or_default();
        }
    }

    /// Update every loaded instance of one VST3 class and retain the offset for future loads.
    pub fn set_manual_latency_offset(
        &mut self,
        id: u64,
        offset_samples: i32,
    ) -> Result<bool, String> {
        let uid = self
            .loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map(|loaded| loaded.info.uid.to_ascii_lowercase())
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        let changed = self
            .manual_latency_offsets_by_uid
            .insert(uid.clone(), offset_samples)
            != Some(offset_samples);
        for loaded in &mut self.loaded {
            if loaded.info.uid.eq_ignore_ascii_case(&uid)
                && !self
                    .manual_latency_offset_overrides
                    .contains(&loaded.info.id)
            {
                loaded.info.manual_latency_offset_samples = offset_samples;
            }
        }
        Ok(changed)
    }

    /// Update one loaded instance without changing the class-wide default or its peers.
    pub fn set_manual_latency_offset_for_instance(
        &mut self,
        id: u64,
        offset_samples: i32,
    ) -> Result<bool, String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        let changed = loaded.info.manual_latency_offset_samples != offset_samples;
        loaded.info.manual_latency_offset_samples = offset_samples;
        self.manual_latency_offset_overrides.insert(id);
        Ok(changed)
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
        let path = path.as_ref();
        let plugin = if x86_vst3_binary(path).is_some() {
            let helper_path = x86_helper_path().ok_or_else(|| {
                format!(
                    "32-bit x86 VST3 plugin {} requires the bundled x86 helper, which is unavailable on this platform",
                    path.display()
                )
            })?;
            let config = self.host.config();
            let mut helper_host = build_isolated_vst3_host(
                config.sample_rate,
                config.block_size,
                Some(&helper_path),
            )?;
            match class_id {
                Some(class_id) => helper_host.load_plugin_class(path, class_id),
                None => helper_host.load_plugin(path),
            }
        } else {
            match class_id {
                Some(class_id) => self.host.load_plugin_class(path, class_id),
                None => self.host.load_plugin(path),
            }
        }
        .map_err(|error| error.to_string())?;

        let mut info = hosted_info(self.next_id, &plugin);
        info.manual_latency_offset_samples = self
            .manual_latency_offsets_by_uid
            .get(&info.uid.to_ascii_lowercase())
            .copied()
            .unwrap_or_default();
        self.next_id = self.next_id.saturating_add(1);
        self.loaded.push(HostedPlugin {
            info: info.clone(),
            plugin: Arc::new(Mutex::new(plugin)),
            editor_open: false,
        });
        Ok(info)
    }

    /// Load a VST3 class that accepts audio and produces audio, for effect processing.
    ///
    /// Instrument-only classes are removed from the runtime and rejected. The instance remains
    /// stopped until [`Self::start_audio_processing`] is called.
    pub fn load_audio_effect(
        &mut self,
        path: impl AsRef<Path>,
        class_id: Option<&str>,
    ) -> Result<HostedPluginInfo, String> {
        let info = self.load(path, class_id)?;
        if info.audio_inputs == 0 || info.audio_outputs == 0 {
            self.loaded.retain(|loaded| loaded.info.id != info.id);
            return Err(format!(
                "{} does not expose both audio input and output channels",
                info.name
            ));
        }
        Ok(info)
    }

    /// Read all audio buses for a loaded VST3 instance in bus-index order.
    pub fn audio_bus_layout(&self, id: u64) -> Result<AudioBusLayout, String> {
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .audio_bus_layout()
            .map_err(|error| error.to_string())
    }

    /// Activate every advertised audio output bus on a loaded instrument before processing.
    ///
    /// VST3 secondary outputs commonly begin inactive, so output counts and block buffers omit
    /// them until the host activates them. Mixer effects retain their saved bus activation and
    /// should not use this instrument-specific helper.
    pub fn activate_all_audio_output_buses(&self, id: u64) -> Result<usize, String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        if plugin.is_processing() {
            return Err("VST3 output buses cannot be activated during processing".to_owned());
        }
        let layout = plugin
            .audio_bus_layout()
            .map_err(|error| error.to_string())?;
        let inactive_bus_indices = layout
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(index, bus)| (!bus.active).then_some(index))
            .collect::<Vec<_>>();
        let mut activated = Vec::with_capacity(inactive_bus_indices.len());
        for bus_index in inactive_bus_indices {
            let bus_index_i32 = i32::try_from(bus_index)
                .map_err(|_| "VST3 output bus index exceeds the host range".to_owned())?;
            if let Err(error) =
                plugin.set_bus_active(MediaType::Audio, BusDirection::Output, bus_index_i32, true)
            {
                for activated_bus_index in activated.into_iter().rev() {
                    let _ = plugin.set_bus_active(
                        MediaType::Audio,
                        BusDirection::Output,
                        activated_bus_index,
                        false,
                    );
                }
                return Err(format!(
                    "could not activate VST3 audio output bus {bus_index}: {error}"
                ));
            }
            activated.push(bus_index_i32);
        }
        Ok(activated.len())
    }

    /// Allocate reusable buffers for every configured audio bus.
    pub fn create_audio_bus_buffers(
        &self,
        id: u64,
        block_size: usize,
    ) -> Result<BusAudioBuffers, String> {
        if block_size == 0 {
            return Err("audio block size must be greater than zero".to_owned());
        }
        self.plugin(id)?
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .create_bus_audio_buffers(block_size)
            .map_err(|error| error.to_string())
    }

    /// Start one loaded VST3 instance for repeated audio-block processing.
    pub fn start_audio_processing(&self, id: u64) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        if !plugin.is_processing() {
            plugin
                .start_processing()
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Process one bus-aware audio block on a started VST3 instance.
    ///
    /// Reuse buffers returned by [`Self::create_audio_bus_buffers`] and call this method from
    /// one processing thread per instance. Every bus and channel must contain exactly
    /// `buffers.block_size` frames; the plug-in's VST3 host validates bus counts and activation.
    pub fn process_audio_block(
        &self,
        id: u64,
        buffers: &mut BusAudioBuffers,
    ) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        validate_vst3_audio_block(buffers, plugin.sample_rate(), plugin.block_size())?;
        if !plugin.is_processing() {
            return Err("VST3 audio processing has not been started".to_owned());
        }
        plugin
            .process_bus_audio(buffers)
            .map_err(|error| error.to_string())
    }

    /// Stop audio processing for one loaded VST3 instance.
    pub fn stop_audio_processing(&self, id: u64) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        plugin.stop_processing().map_err(|error| error.to_string())
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

    /// Restore marker-12 VST3 state from one Mixer effect slot's unique `0xD5` event.
    ///
    /// Slots with multiple state events, VST2 envelopes, or a different VST wrapper marker are
    /// rejected rather than guessing how their bytes should be combined.
    pub fn restore_flp_mixer_effect_state(
        &mut self,
        id: u64,
        slot: &MixerEffectSlotSummary,
    ) -> Result<(), String> {
        let metadata = slot.vst_metadata().ok_or_else(|| {
            "Mixer effect slot has no unique recognized VST state envelope".to_owned()
        })?;
        if metadata.format_marker() != 12 {
            return Err(format!(
                "unsupported FLP VST wrapper marker {}; VST3 state restore requires marker 12",
                metadata.format_marker()
            ));
        }
        if metadata.fourcc().is_some() {
            return Err("Mixer effect slot state identifies a VST2 plug-in, not VST3".to_owned());
        }
        let expected_uid = metadata
            .class_uid()
            .ok_or_else(|| "FLP Mixer effect state has no valid 16-byte class UID".to_owned())?;
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if loaded.info.audio_inputs == 0 || loaded.info.audio_outputs == 0 {
            return Err(format!(
                "{} is not a VST3 audio effect with both audio inputs and outputs",
                loaded.info.name
            ));
        }
        if !loaded.info.uid.eq_ignore_ascii_case(&expected_uid) {
            return Err(format!(
                "FLP Mixer effect class UID {expected_uid} does not match loaded plug-in UID {}",
                loaded.info.uid
            ));
        }
        let nested_state = slot
            .vst_state_bytes()
            .ok_or_else(|| "FLP Mixer effect state has no nested field 53".to_owned())?;
        let host_snapshot = encode_flp_vst3_state_snapshot(nested_state)?;
        self.restore_state(id, &host_snapshot)
    }

    /// Save one loaded VST3 audio effect into a Mixer slot's marker-12 nested state stream.
    ///
    /// The returned bytes can be written with
    /// [`FlpDocument::replace_vst3_mixer_effect_slot_state_bytes`]. Unknown FLP records in the
    /// nested stream are retained.
    pub fn save_flp_mixer_effect_state(
        &self,
        id: u64,
        slot: &MixerEffectSlotSummary,
    ) -> Result<Vec<u8>, String> {
        let metadata = slot.vst_metadata().ok_or_else(|| {
            "Mixer effect slot has no unique recognized VST state envelope".to_owned()
        })?;
        if metadata.format_marker() != 12 {
            return Err(format!(
                "unsupported FLP VST wrapper marker {}; VST3 state write-back requires marker 12",
                metadata.format_marker()
            ));
        }
        if metadata.fourcc().is_some() {
            return Err("Mixer effect slot state identifies a VST2 plug-in, not VST3".to_owned());
        }
        let expected_uid = metadata
            .class_uid()
            .ok_or_else(|| "FLP Mixer effect state has no valid 16-byte class UID".to_owned())?;
        let loaded = self
            .loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if loaded.info.audio_inputs == 0 || loaded.info.audio_outputs == 0 {
            return Err(format!(
                "{} is not a VST3 audio effect with both audio inputs and outputs",
                loaded.info.name
            ));
        }
        if !loaded.info.uid.eq_ignore_ascii_case(&expected_uid) {
            return Err(format!(
                "FLP Mixer effect class UID {expected_uid} does not match loaded plug-in UID {}",
                loaded.info.uid
            ));
        }
        let nested_state = slot
            .vst_state_bytes()
            .ok_or_else(|| "FLP Mixer effect state has no nested field 53".to_owned())?;
        let snapshot = self.save_state(id)?;
        replace_flp_vst3_state_snapshot(nested_state, &snapshot)
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
        loaded
            .plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?
            .open_isolated_editor()
            .map_err(|error| error.to_string())?;
        loaded.editor_open = true;
        Ok(())
    }

    pub fn close_editor(&mut self, id: u64) -> Result<(), String> {
        let loaded = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == id)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))?;
        if loaded.editor_open {
            loaded
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?
                .close_editor()
                .map_err(|error| error.to_string())?;
            loaded.editor_open = false;
        }
        Ok(())
    }

    pub fn close_all_editors(&mut self) {
        for loaded in &mut self.loaded {
            if loaded.editor_open {
                if let Ok(mut plugin) = loaded.plugin.lock() {
                    let _ = plugin.close_editor();
                }
                loaded.editor_open = false;
            }
        }
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

    /// Render a pattern channel as raw 32-bit float WAV with every active output channel retained
    /// in bus/channel order. This does not restore FLP plug-in state, apply Mixer routing/effects,
    /// or expand Playlist clips.
    pub fn render_pattern_channel_to_file(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        path: impl AsRef<Path>,
        file_options: AudioFileRenderOptions,
    ) -> Result<Vst3RenderSummary, String> {
        self.render_pattern_channel_to_file_with_automation(
            id,
            notes,
            options,
            &[],
            path,
            file_options,
        )
    }

    /// Render a pattern channel with explicit VST3 parameter automation into an encoded audio file.
    /// Active mono/stereo output buses are summed to stereo before applying the selected channel
    /// mode and encoding. Separate per-bus Mixer connections are not applied here.
    pub fn render_pattern_channel_to_file_with_automation(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
        path: impl AsRef<Path>,
        file_options: AudioFileRenderOptions,
    ) -> Result<Vst3RenderSummary, String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render =
            prepare_pattern_render_with_automation(&plugin, notes, options, parameter_automation)?;
        render_stereo_audio_to_file(
            path.as_ref(),
            render.total_frames,
            render.sample_rate_u32,
            file_options.encoding,
            file_options.output_format,
            |write_block| {
                let mut stereo_block = Vec::with_capacity(render.block_size * 2);
                process_pattern_render(&mut plugin, &render, |interleaved| {
                    stereo_block.clear();
                    append_vst_output_buses_to_stereo(
                        &mut stereo_block,
                        interleaved,
                        &render.output_bus_channels,
                    )?;
                    write_block(&stereo_block)
                })?;
                Ok(Vst3RenderSummary {
                    frames: render.total_frames,
                    sample_rate: render.sample_rate_u32,
                    output_channels: render.output_channels,
                    notes_rendered: render.note_count,
                })
            },
        )
    }

    /// Process a decoded mono or stereo source through one loaded VST3 audio effect and encode
    /// the result to an audio file. Effective PDC latency, including any configured manual
    /// offset, is removed from the output and the output duration includes the longer of the
    /// selected tail and a finite reported tail.
    ///
    /// This is a standalone effect render; it does not apply FLP Mixer slot state, Mix controls,
    /// or project routing.
    pub fn render_audio_through_effect_to_file(
        &self,
        id: u64,
        audio: &DecodedAudio,
        options: Vst3AudioEffectRenderOptions,
        path: impl AsRef<Path>,
        file_options: AudioFileRenderOptions,
    ) -> Result<Vst3AudioEffectRenderSummary, String> {
        if !(8_000..=384_000).contains(&audio.sample_rate) {
            return Err("audio sample rate must be between 8000 and 384000 Hz".to_owned());
        }
        if audio.channels.is_empty() || audio.channels.len() > 2 {
            return Err(
                "VST3 effect file rendering requires mono or stereo input audio".to_owned(),
            );
        }
        let input_frames = audio.frame_count();
        if input_frames == 0 {
            return Err("input audio contains no frames".to_owned());
        }
        if audio
            .channels
            .iter()
            .any(|channel| channel.len() != input_frames)
        {
            return Err("input audio channels have different frame counts".to_owned());
        }
        if !options.tail_seconds.is_finite()
            || !(0.0..=MAX_REPORTED_TAIL_SECONDS).contains(&options.tail_seconds)
        {
            return Err(format!(
                "effect render tail must be between 0 and {MAX_REPORTED_TAIL_SECONDS} seconds"
            ));
        }

        let (plugin_name, sample_rate, latency_frames, tail_seconds, layout, block_size) = {
            let plugin = self
                .plugin(id)?
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            if plugin.info().audio_inputs == 0 || plugin.info().audio_outputs == 0 {
                return Err(format!(
                    "{} does not expose both audio input and output channels",
                    plugin.info().name
                ));
            }
            let sample_rate = plugin.sample_rate();
            if (sample_rate - f64::from(audio.sample_rate)).abs() > 0.000_001 {
                return Err(format!(
                    "audio sample rate {} does not match the VST3 instance rate {sample_rate}",
                    audio.sample_rate
                ));
            }
            if plugin.is_processing() {
                return Err("VST3 effect is already processing audio".to_owned());
            }
            let layout = plugin
                .audio_bus_layout()
                .map_err(|error| error.to_string())?;
            let input_bus = layout
                .inputs
                .first()
                .filter(|bus| bus.active && (1..=2).contains(&bus.channel_count))
                .ok_or_else(|| {
                    "VST3 effect must have an active mono or stereo main input bus".to_owned()
                })?;
            let output_bus = layout
                .outputs
                .first()
                .filter(|bus| bus.active && (1..=2).contains(&bus.channel_count))
                .ok_or_else(|| {
                    "VST3 effect must have an active mono or stereo main output bus".to_owned()
                })?;
            let _ = (input_bus, output_bus);
            let latency_frames =
                u64::from(self.pdc_latency_samples_for_plugin(id, plugin.latency_samples()));
            let maximum_compensated_latency =
                (sample_rate * MAX_VST3_EFFECT_LATENCY_SECONDS).ceil() as u64;
            if latency_frames > maximum_compensated_latency {
                return Err(format!(
                    "VST3 effect latency {latency_frames} frames exceeds the {MAX_VST3_EFFECT_LATENCY_SECONDS}-second offline processing limit"
                ));
            }
            let reported_tail = reported_tail_seconds(plugin.tail_samples(), sample_rate);
            let tail_seconds = options.tail_seconds.max(reported_tail.unwrap_or_default());
            (
                plugin.info().name.clone(),
                audio.sample_rate,
                latency_frames,
                tail_seconds,
                layout,
                plugin
                    .block_size()
                    .clamp(1, VST3_EFFECT_RENDER_BLOCK_FRAMES),
            )
        };
        let input_frames = u64::try_from(input_frames)
            .map_err(|_| "input frame count exceeds the renderable range".to_owned())?;
        let tail_frames = (tail_seconds * f64::from(sample_rate)).ceil() as u64;
        let output_frames = input_frames
            .checked_add(tail_frames)
            .ok_or_else(|| "VST3 effect render length overflow".to_owned())?;
        let process_frames = output_frames
            .checked_add(latency_frames)
            .ok_or_else(|| "VST3 effect processing length overflow".to_owned())?;
        let mut buffers = self.create_audio_bus_buffers(id, block_size)?;
        if buffers.inputs.len() != layout.inputs.len()
            || buffers.outputs.len() != layout.outputs.len()
        {
            return Err("VST3 audio bus layout changed while preparing the render".to_owned());
        }

        self.start_audio_processing(id)?;
        let render_result = render_stereo_audio_to_file(
            path.as_ref(),
            output_frames,
            sample_rate,
            file_options.encoding,
            file_options.output_format,
            |write_block| {
                let mut output_block = Vec::with_capacity(VST3_EFFECT_RENDER_BLOCK_FRAMES * 2);
                let mut processed_frames = 0_u64;
                let mut frames_to_skip = latency_frames;
                let mut written_frames = 0_u64;
                while processed_frames < process_frames {
                    let frame_count =
                        usize::try_from((process_frames - processed_frames).min(block_size as u64))
                            .map_err(|_| {
                                "VST3 effect block size exceeds this platform".to_owned()
                            })?;
                    resize_vst3_bus_audio_buffers(&mut buffers, frame_count);
                    let input_bus = &mut buffers.inputs[0];
                    for frame in 0..frame_count {
                        let source_frame = processed_frames.saturating_add(frame as u64);
                        let (left, right) = if source_frame < input_frames {
                            let source_frame = source_frame as usize;
                            let left = audio.channels[0][source_frame];
                            let right = audio
                                .channels
                                .get(1)
                                .map_or(left, |channel| channel[source_frame]);
                            (finite_sample(left), finite_sample(right))
                        } else {
                            (0.0, 0.0)
                        };
                        if input_bus.channels.len() == 1 {
                            input_bus.channels[0][frame] = (left + right) * 0.5;
                        } else {
                            input_bus.channels[0][frame] = left;
                            input_bus.channels[1][frame] = right;
                        }
                    }
                    buffers.block_size = frame_count;
                    self.process_audio_block(id, &mut buffers)?;
                    let output_bus = &buffers.outputs[0];
                    for frame in 0..frame_count {
                        if frames_to_skip > 0 {
                            frames_to_skip -= 1;
                            continue;
                        }
                        if written_frames >= output_frames {
                            break;
                        }
                        let left = finite_sample(output_bus.channels[0][frame]);
                        let right = if output_bus.channels.len() == 1 {
                            left
                        } else {
                            finite_sample(output_bus.channels[1][frame])
                        };
                        output_block.extend_from_slice(&[left, right]);
                        written_frames += 1;
                    }
                    if output_block.len() >= VST3_EFFECT_RENDER_BLOCK_FRAMES * 2 {
                        write_block(&output_block)?;
                        output_block.clear();
                    }
                    processed_frames = processed_frames
                        .checked_add(frame_count as u64)
                        .ok_or_else(|| "VST3 effect processing position overflow".to_owned())?;
                }
                if written_frames != output_frames {
                    return Err(format!(
                        "VST3 effect produced {written_frames} frames after latency compensation; expected {output_frames}"
                    ));
                }
                if !output_block.is_empty() {
                    write_block(&output_block)?;
                }
                Ok(Vst3AudioEffectRenderSummary {
                    plugin_name,
                    input_frames,
                    output_frames,
                    sample_rate,
                    latency_compensated_frames: latency_frames,
                    tail_frames,
                })
            },
        );
        let stop_result = self.stop_audio_processing(id);
        match (render_result, stop_result) {
            (Ok(summary), Ok(())) => Ok(summary),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
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
        self.render_pattern_channel_to_stereo_buffer_with_layer_adjustments(
            id,
            notes,
            options,
            parameter_automation,
            None,
        )
    }

    fn render_pattern_channel_to_stereo_buffer_with_layer_adjustments(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
        layer_adjustments: Option<&[(f32, i32)]>,
    ) -> Result<(Vec<f32>, Vst3RenderSummary), String> {
        self.render_pattern_channel_to_stereo_buffer_with_layer_adjustments_and_mixer_transforms(
            id,
            notes,
            options,
            parameter_automation,
            layer_adjustments,
            None,
        )
    }

    fn render_pattern_channel_to_stereo_buffer_with_layer_adjustments_and_mixer_transforms(
        &self,
        id: u64,
        notes: &[PatternNote],
        options: Vst3PatternRenderOptions,
        parameter_automation: &[Vst3ParameterAutomation],
        layer_adjustments: Option<&[(f32, i32)]>,
        mixer_transforms: Option<&[crate::MixerSourceRouteTransform]>,
    ) -> Result<(Vec<f32>, Vst3RenderSummary), String> {
        let plugin = self.plugin(id)?;
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        let render = prepare_pattern_render_with_layer_adjustments(
            &plugin,
            notes,
            options,
            parameter_automation,
            layer_adjustments,
        )?;
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
            append_vst_output_buses_to_stereo_with_mixer_transforms(
                &mut stereo,
                interleaved,
                &render.output_bus_channels,
                mixer_transforms,
            )
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

    /// Render several loaded VST3 channels from the same Pattern into one stereo audio file.
    ///
    /// Notes are routed through supported Layer children and Mixer paths before the resulting
    /// stereo buffers are summed and encoded using the shared audio output settings. Mixer
    /// effects are not applied.
    pub fn render_pattern_channels_to_file(
        &self,
        document: &FlpDocument,
        notes: &[PatternNote],
        channels: &[Vst3PatternChannelRenderRequest],
        path: impl AsRef<Path>,
        file_options: AudioFileRenderOptions,
    ) -> Result<Vst3PatternMixRenderSummary, String> {
        if channels.is_empty() {
            return Err("no loaded VST3 Pattern channels have notes to render".to_owned());
        }

        let project_channels = document.channels();
        let channels_by_id = project_channels
            .iter()
            .map(|channel| (channel.id(), channel))
            .collect::<BTreeMap<_, _>>();
        let note_router = ChannelNoteRouter::new(project_channels.clone());
        let channel_instances = channels
            .iter()
            .map(|channel| (channel.options.channel_id, channel.instance_id))
            .collect::<BTreeMap<_, _>>();
        let output_mixer_offsets_by_channel =
            flp_vst3_output_mixer_offsets_for_loaded_channels(self, document, &channel_instances);
        let vst3_volume_expression_channels = self.volume_expression_channels(&channel_instances);
        let mixer_route_audibility = MixerRouteAudibility::from_document(document);
        let global_swing_mix_raw = document.metadata().global_swing_mix();
        let mut routed_notes_by_channel = BTreeMap::<u16, Vec<(PatternNote, f32, i32)>>::new();
        for channel in channels {
            let channel_id = channel.options.channel_id;
            if !channels_by_id.contains_key(&channel_id) {
                return Err(format!(
                    "VST3 Pattern channel {channel_id} was not found in the project"
                ));
            }
            if routed_notes_by_channel
                .insert(channel_id, Vec::new())
                .is_some()
            {
                return Err(format!(
                    "VST3 Pattern channel {channel_id} was requested more than once"
                ));
            }
        }
        for (note_index, note) in notes.iter().enumerate() {
            if note.is_muted() {
                continue;
            }
            let targets = note_router.targets_for_note_with_vst3_crossfade(
                note.channel_id,
                note_index as u64,
                note.key,
                &vst3_volume_expression_channels,
            );
            if !targets
                .iter()
                .any(|(channel_id, _, _)| routed_notes_by_channel.contains_key(channel_id))
            {
                continue;
            }
            let source_swing_mix_raw = channels_by_id
                .get(&note.channel_id)
                .map_or(128, |source| source.swing_mix());
            let swung_position = swing_note_start_tick(
                u64::from(note.position),
                document.header().ppq(),
                global_swing_mix_raw,
                source_swing_mix_raw,
            )?;
            let position = u32::try_from(swung_position)
                .map_err(|_| "swing-adjusted VST3 note position exceeds 32-bit ticks")?;
            for (channel_id, layer_gain, layer_pan_raw) in targets {
                let Some(routed_notes) = routed_notes_by_channel.get_mut(&channel_id) else {
                    continue;
                };
                let mut routed_note = note.clone();
                routed_note.position = position;
                routed_note.channel_id = channel_id;
                routed_notes.push((routed_note, layer_gain, layer_pan_raw));
            }
        }

        let mut mixed_stereo = Vec::new();
        let mut mixed_frames = 0_u64;
        let mut sample_rate = None;
        let mut notes_rendered = 0usize;
        let mut channels_rendered = 0usize;
        for channel in channels {
            let channel_id = channel.options.channel_id;
            let target_channel = channels_by_id
                .get(&channel_id)
                .expect("Pattern target channels were validated above");
            if target_channel.enabled() == Some(false) {
                continue;
            }
            let routed_note_data = routed_notes_by_channel
                .get(&channel_id)
                .expect("Pattern target channels were validated above");
            if routed_note_data.is_empty() {
                continue;
            }
            let plugin = self.plugin(channel.instance_id)?;
            let plugin_guard = plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            let output_bus_channels = plugin_output_bus_channel_counts(&plugin_guard)?;
            drop(plugin_guard);
            let output_offsets = output_mixer_offsets_by_channel
                .get(&channel_id)
                .filter(|offsets| offsets.len() == output_bus_channels.len())
                .map(Vec::as_slice);
            if !vst3_channel_has_audible_output_route(
                &mixer_route_audibility,
                target_channel.mixer_track(),
                output_offsets,
                false,
            ) {
                continue;
            }
            let routed_notes = routed_note_data
                .iter()
                .map(|(note, _, _)| note.clone())
                .collect::<Vec<_>>();
            let layer_adjustments = routed_note_data
                .iter()
                .map(|(_, gain, pan_raw)| (*gain, *pan_raw))
                .collect::<Vec<_>>();
            let mixer_transforms = (0..output_bus_channels.len())
                .map(|bus_index| {
                    let route = vst3_output_mixer_route(
                        target_channel.mixer_track(),
                        output_offsets.and_then(|offsets| offsets.get(bus_index).copied()),
                    );
                    mixer_route_audibility
                        .source_route_plan_for_channel(route)
                        .output_transform
                })
                .collect::<Vec<_>>();

            let mut options = channel.options;
            options.global_swing_mix_raw = 0;
            options.channel_swing_mix_raw = 0;
            let (stereo, summary) = self
                .render_pattern_channel_to_stereo_buffer_with_layer_adjustments_and_mixer_transforms(
                    channel.instance_id,
                    &routed_notes,
                    options,
                    &channel.parameter_automation,
                    Some(&layer_adjustments),
                    Some(&mixer_transforms),
                )?;
            if let Some(expected_rate) = sample_rate
                && summary.sample_rate != expected_rate
            {
                return Err(format!(
                    "VST3 Pattern channels use different render rates ({expected_rate} and {})",
                    summary.sample_rate
                ));
            }
            sample_rate.get_or_insert(summary.sample_rate);
            mix_stereo_pattern_render_buffer(
                &mut mixed_stereo,
                &mut mixed_frames,
                &stereo,
                summary.frames,
            )?;
            notes_rendered = notes_rendered
                .checked_add(summary.notes_rendered)
                .ok_or_else(|| "rendered VST3 note count overflow".to_owned())?;
            channels_rendered = channels_rendered
                .checked_add(1)
                .ok_or_else(|| "rendered VST3 channel count overflow".to_owned())?;
        }

        let sample_rate = sample_rate.ok_or_else(|| {
            "no loaded VST3 Pattern channels have audible notes to render".to_owned()
        })?;
        apply_mixer_master_output_to_stereo(document, &mut mixed_stereo)?;
        let summary = Vst3PatternMixRenderSummary {
            frames: mixed_frames,
            sample_rate,
            channels_rendered,
            notes_rendered,
        };
        render_stereo_audio_to_file(
            path.as_ref(),
            summary.frames,
            summary.sample_rate,
            file_options.encoding,
            file_options.output_format,
            |write_block| {
                for block in mixed_stereo.chunks(8_192) {
                    write_block(block)?;
                }
                Ok(summary)
            },
        )
    }

    /// Prepare one Pattern's loaded Mixer VST3 effects for a bounded offline render.
    ///
    /// This processor has no instrument streams. Callers can render Sampler or audio sources into
    /// its Mixer buses, then pass those buses through the Pattern's loaded insert and Master FX.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pattern_mixer_effects_processor(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        mixer_effect_parameter_automation: &BTreeMap<
            (usize, u16),
            Vec<Vst3PlaylistParameterAutomation>,
        >,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let mut processor = self.prepare_pattern_loop_stream_with_automation(
            document,
            pattern_id,
            loop_length_ticks,
            output_sample_rate,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )?;
        processor.stop_pattern_repetition_after_first_pass();
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            true,
            false,
            mixer_effect_parameter_automation,
            None,
        )?;
        Ok(processor)
    }

    /// Prepare loaded Mixer effects for an offline Audio Clip render.
    ///
    /// Audio Clip callers render the source into the processor's Mixer buses, then the processor
    /// applies the active insert and Master chains, reported tails, and automatic PDC.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_audio_clip_mixer_effects_processor(
        &self,
        document: &FlpDocument,
        arrangement_id: u16,
        output_sample_rate: u32,
        selected_clip_indices: &[usize],
        soloed_playlist_track_range: Option<(u32, u32)>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<(Vst3PlaylistStreamProcessor, bool), String> {
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
        if selected_clip_indices.is_empty() {
            return Err("Mixer-effect Audio Clip renders require a selected clip".to_owned());
        }
        let content_end_tick = selected_clip_indices
            .iter()
            .map(|clip_index| {
                let clip = arrangement
                    .clips
                    .get(*clip_index)
                    .ok_or_else(|| format!("Playlist clip {clip_index} does not exist"))?;
                u64::from(clip.position_ticks)
                    .checked_add(u64::from(clip.length_ticks))
                    .ok_or_else(|| "selected Audio Clip endpoint overflow".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .ok_or_else(|| "Mixer-effect Audio Clip renders require a selected clip".to_owned())?;
        let options = PlaylistRenderOptions {
            arrangement_id,
            sample_rate: output_sample_rate,
            soloed_playlist_track_range,
            ..PlaylistRenderOptions::default()
        };
        let effect_automation = expand_playlist_mixer_effect_parameter_automation(
            document,
            options.clone(),
            &automation_sources.mixer_effect_pattern_clips,
            &automation_sources.mixer_effect_automation_clips,
        )?;
        let render_from_song_start = effect_automation.values().flatten().any(|lane| {
            lane.points
                .iter()
                .any(|point| point.tick < content_end_tick)
        });
        let mut processor = self.prepare_playlist_stream_with_automation_sources(
            document,
            options,
            &BTreeMap::new(),
            false,
            &Vst3PlaylistAutomationSources::default(),
        )?;
        if render_from_song_start {
            let content_frames = processor
                .tempo_map
                .frame_at_tick(content_end_tick, output_sample_rate)?;
            processor.extend_to_output_frames(content_frames)?;
        }
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            true,
            false,
            &effect_automation,
            None,
        )?;
        Ok((processor, render_from_song_start))
    }

    /// Render one Pattern through its loaded VST3 instruments and the project's loaded Mixer FX.
    ///
    /// The prepared Pattern stream applies the same channel routing, effect automation, and PDC
    /// used by Pattern playback. Pattern repetition is stopped after its first pass; the render
    /// then includes the standard two-second instrument tail plus any reported effect tails.
    #[allow(clippy::too_many_arguments)]
    pub fn render_pattern_channels_with_mixer_effects_to_file(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        channels: &[Vst3PatternChannelRenderRequest],
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        mixer_effect_parameter_automation: &BTreeMap<
            (usize, u16),
            Vec<Vst3PlaylistParameterAutomation>,
        >,
        path: impl AsRef<Path>,
        file_options: AudioFileRenderOptions,
    ) -> Result<Vst3PatternMixRenderSummary, String> {
        if channels.is_empty() {
            return Err("no loaded VST3 Pattern channels have notes to render".to_owned());
        }

        let mut channel_instances = BTreeMap::new();
        let mut parameter_automation = BTreeMap::new();
        for channel in channels {
            let channel_id = channel.options.channel_id;
            if channel_instances
                .insert(channel_id, channel.instance_id)
                .is_some()
            {
                return Err(format!(
                    "VST3 Pattern channel {channel_id} was requested more than once"
                ));
            }
            if !channel.parameter_automation.is_empty() {
                parameter_automation.insert(channel_id, channel.parameter_automation.clone());
            }
        }

        let sample_rate = {
            let first_channel = channels
                .first()
                .expect("nonempty VST3 Pattern channels were checked above");
            let plugin = self.plugin(first_channel.instance_id)?;
            let plugin = plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            let sample_rate = plugin.sample_rate();
            if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
                return Err("VST3 Pattern render rate is invalid".to_owned());
            }
            sample_rate.round() as u32
        };

        let mut processor = self.prepare_pattern_loop_stream_with_automation(
            document,
            pattern_id,
            loop_length_ticks,
            sample_rate,
            &channel_instances,
            &parameter_automation,
        )?;
        processor.stop_pattern_repetition_after_first_pass();
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            true,
            false,
            mixer_effect_parameter_automation,
            None,
        )?;

        let pattern_frames = processor.content_frames();
        let instrument_tail_frames = u64::from(sample_rate)
            .checked_mul(2)
            .ok_or_else(|| "VST3 Pattern tail length overflow".to_owned())?;
        let content_with_instrument_tail = pattern_frames
            .checked_add(instrument_tail_frames)
            .ok_or_else(|| "VST3 Pattern render length overflow".to_owned())?;
        processor.extend_to_output_frames(content_with_instrument_tail)?;

        let processor_summary = processor.summary();
        if processor_summary.plugin_channels_rendered == 0 || processor_summary.notes_scheduled == 0
        {
            return Err("no loaded VST3 Pattern channels have audible notes to render".to_owned());
        }
        let trim_frames = processor
            .pdc_render_start_trim_frames()
            .min(processor_summary.frames);
        let summary = Vst3PatternMixRenderSummary {
            frames: processor_summary.frames.saturating_sub(trim_frames),
            sample_rate: processor_summary.sample_rate,
            channels_rendered: processor_summary.plugin_channels_rendered,
            notes_rendered: processor_summary.notes_scheduled,
        };

        processor.start_processing()?;
        let render_result = render_stereo_audio_to_file(
            path.as_ref(),
            summary.frames,
            summary.sample_rate,
            file_options.encoding,
            file_options.output_format,
            |write_block| {
                let mut rendered_frame = 0_u64;
                let mut block = Vec::new();
                let mut mixer_buses = Vec::new();
                while rendered_frame < processor_summary.frames {
                    let frame_count = (processor_summary.frames - rendered_frame).min(4_096);
                    let sample_count = usize::try_from(frame_count)
                        .ok()
                        .and_then(|frames| frames.checked_mul(2))
                        .ok_or_else(|| "VST3 Pattern block size overflow".to_owned())?;
                    let bus_sample_count = sample_count
                        .checked_mul(crate::audio::MIXER_TRACK_METER_COUNT)
                        .and_then(|count| count.checked_mul(2))
                        .ok_or_else(|| "VST3 Pattern Mixer bus size overflow".to_owned())?;
                    block.resize(sample_count, 0.0);
                    block.fill(0.0);
                    mixer_buses.resize(bus_sample_count, 0.0);
                    mixer_buses.fill(0.0);
                    processor.mix_next_block_with_meter_buses(&mut block, &mut mixer_buses)?;
                    apply_mixer_master_output_to_stereo(document, &mut block)?;

                    let skipped_frames =
                        trim_frames.saturating_sub(rendered_frame).min(frame_count);
                    if skipped_frames < frame_count {
                        let skip_samples = usize::try_from(skipped_frames)
                            .ok()
                            .and_then(|frames| frames.checked_mul(2))
                            .ok_or_else(|| "VST3 Pattern PDC trim overflow".to_owned())?;
                        write_block(&block[skip_samples..])?;
                    }
                    rendered_frame = rendered_frame
                        .checked_add(frame_count)
                        .ok_or_else(|| "VST3 Pattern render frame overflow".to_owned())?;
                }
                Ok(summary)
            },
        );
        let stop_result = processor.stop_processing();
        let summary = render_result?;
        stop_result?;
        Ok(summary)
    }

    /// Start a low-latency stream for live MIDI audition through an already loaded instrument.
    pub fn start_live_midi_input_stream(
        &self,
        id: u64,
        writer: StreamingAudioWriter,
        output_sample_rate: u32,
        pitch_offset_cents: i16,
        note_pitch_offset_semitones: i16,
    ) -> Result<Vst3LiveInputStreamHandle, String> {
        if output_sample_rate == 0 {
            return Err("audio output sample rate must be positive".to_owned());
        }
        if !(-1200..=1200).contains(&pitch_offset_cents) {
            return Err("VST3 pitch offset must be between -1200 and 1200 cents".to_owned());
        }
        if !(-12..=12).contains(&note_pitch_offset_semitones) {
            return Err("VST3 note pitch offset must be between -12 and 12 semitones".to_owned());
        }
        let plugin = self.plugin(id)?.clone();
        let (sample_rate, block_size) = {
            let plugin = plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            plugin_output_bus_channel_counts(&plugin)?;
            (plugin.sample_rate(), plugin.block_size())
        };
        if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
            return Err("VST3 host sample rate is invalid".to_owned());
        }
        if block_size == 0 || block_size > 65_536 {
            return Err("VST3 host block size is not renderable".to_owned());
        }
        let (sender, receiver) = mpsc::sync_channel(256);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("vst3-live-midi-input".to_owned())
            .spawn(move || {
                let result = stream_live_midi_input(
                    plugin,
                    receiver,
                    &writer,
                    output_sample_rate,
                    pitch_offset_cents,
                    note_pitch_offset_semitones,
                    worker_cancelled,
                );
                writer.finish();
                let _ = result_sender.send(result);
            })
            .map_err(|error| format!("could not start VST3 live input worker: {error}"))?;
        Ok(Vst3LiveInputStreamHandle {
            sender,
            receiver: result_receiver,
            cancelled,
            worker,
        })
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
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        include_plugin_reported_tails: bool,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_playlist_stream_with_pattern_automation(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            &BTreeMap::new(),
        )
    }

    /// Prepare loaded VST3 channels with app-linked Pattern event automation expanded across
    /// every matching Pattern Clip and repetition in the selected arrangement.
    pub fn prepare_playlist_stream_with_pattern_automation(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        include_plugin_reported_tails: bool,
        pattern_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let playlist_automation = expand_playlist_parameter_automation(
            document,
            options.clone(),
            pattern_automation,
            &BTreeMap::new(),
        )?;
        self.prepare_playlist_stream_inner(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            &playlist_automation,
            &BTreeSet::new(),
        )
    }

    /// Prepare loaded VST3 channels with app-linked Pattern and Playlist Automation Clip curves.
    pub fn prepare_playlist_stream_with_automation_sources(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let playlist_automation = expand_playlist_parameter_automation(
            document,
            options.clone(),
            &automation_sources.pattern_clips,
            &automation_sources.automation_clips,
        )?;
        self.prepare_playlist_stream_inner(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            &playlist_automation,
            &BTreeSet::new(),
        )
    }

    /// Prepare Playlist playback and attach the loaded VST3 effects on Mixer inserts.
    ///
    /// Effect instances are matched by project insert and slot coordinates, keeping duplicate
    /// plug-in classes distinct and preserving the slot order from the FLP.
    pub fn prepare_playlist_stream_with_mixer_effects_and_automation_sources(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_playlist_stream_with_mixer_effects_mode(
            document,
            options,
            channel_instances,
            mixer_effect_instances,
            include_plugin_reported_tails,
            automation_sources,
            false,
        )
    }

    /// Prepare a live Playlist stream whose loaded Mixer effect Mix controls can change while
    /// playback continues, with latency and dry alignment prepared for the full Mix range.
    pub fn prepare_live_playlist_stream_with_mixer_effects_and_automation_sources(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_playlist_stream_with_mixer_effects_mode(
            document,
            options,
            channel_instances,
            mixer_effect_instances,
            include_plugin_reported_tails,
            automation_sources,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_playlist_stream_with_mixer_effects_mode(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
        live_mix_changes: bool,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let effect_automation = expand_playlist_mixer_effect_parameter_automation(
            document,
            options.clone(),
            &automation_sources.mixer_effect_pattern_clips,
            &automation_sources.mixer_effect_automation_clips,
        )?;
        let mut processor = self.prepare_playlist_stream_with_automation_sources(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            automation_sources,
        )?;
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            true,
            live_mix_changes,
            &effect_automation,
            None,
        )?;
        Ok(processor)
    }

    /// Prepare loaded VST3 channels for both static Playlist notes and live Performance Mode
    /// Pattern Clip launches.
    pub fn prepare_playlist_performance_stream(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        live_performance_channels: &BTreeSet<u16>,
        include_plugin_reported_tails: bool,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_playlist_performance_stream_with_pattern_automation(
            document,
            options,
            channel_instances,
            live_performance_channels,
            include_plugin_reported_tails,
            &BTreeMap::new(),
        )
    }

    /// Prepare Playlist and Performance playback with Pattern event automation.
    pub fn prepare_playlist_performance_stream_with_pattern_automation(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        live_performance_channels: &BTreeSet<u16>,
        include_plugin_reported_tails: bool,
        pattern_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let playlist_automation = expand_playlist_parameter_automation(
            document,
            options.clone(),
            pattern_automation,
            &BTreeMap::new(),
        )?;
        let mut processor = self.prepare_playlist_stream_inner(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            &playlist_automation,
            live_performance_channels,
        )?;
        processor.performance_pattern_automation = pattern_automation.clone();
        Ok(processor)
    }

    /// Prepare Playlist and Performance playback with Pattern and Automation Clip curves.
    pub fn prepare_playlist_performance_stream_with_automation_sources(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        live_performance_channels: &BTreeSet<u16>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let playlist_automation = expand_playlist_parameter_automation(
            document,
            options.clone(),
            &automation_sources.pattern_clips,
            &automation_sources.automation_clips,
        )?;
        let mut processor = self.prepare_playlist_stream_inner(
            document,
            options,
            channel_instances,
            include_plugin_reported_tails,
            &playlist_automation,
            live_performance_channels,
        )?;
        processor.performance_pattern_automation = automation_sources.pattern_clips.clone();
        Ok(processor)
    }

    /// Prepare Playlist and Performance playback with loaded Mixer VST3 effects.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_playlist_performance_stream_with_mixer_effects_and_automation_sources(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        live_performance_channels: &BTreeSet<u16>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        include_plugin_reported_tails: bool,
        automation_sources: &Vst3PlaylistAutomationSources,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let effect_automation = expand_playlist_mixer_effect_parameter_automation(
            document,
            options.clone(),
            &automation_sources.mixer_effect_pattern_clips,
            &automation_sources.mixer_effect_automation_clips,
        )?;
        let mut processor = self.prepare_playlist_performance_stream_with_automation_sources(
            document,
            options,
            channel_instances,
            live_performance_channels,
            include_plugin_reported_tails,
            automation_sources,
        )?;
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            false,
            true,
            &effect_automation,
            None,
        )?;
        Ok(processor)
    }

    /// Prepare loaded VST3 channels for repeated playback of one Pattern.
    ///
    /// The returned streams keep processing on the playback worker and enqueue each Pattern
    /// iteration at its loop frame, so plug-in state and notes crossing the loop boundary remain
    /// continuous.
    pub fn prepare_pattern_loop_stream(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        channel_instances: &BTreeMap<u16, u64>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_pattern_loop_stream_with_automation(
            document,
            pattern_id,
            loop_length_ticks,
            output_sample_rate,
            channel_instances,
            &BTreeMap::new(),
        )
    }

    /// Prepare a repeating Pattern loop with per-project-channel parameter automation.
    pub fn prepare_pattern_loop_stream_with_automation(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        channel_instances: &BTreeMap<u16, u64>,
        parameter_automation: &BTreeMap<u16, Vec<Vst3ParameterAutomation>>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_pattern_loop_stream_with_automation_and_clap_audio_channels(
            document,
            pattern_id,
            loop_length_ticks,
            output_sample_rate,
            channel_instances,
            parameter_automation,
            &BTreeSet::new(),
        )
    }

    /// Prepare a repeating Pattern loop while accounting for assigned CLAP
    /// channels when deciding whether mixed Layer Crossfade is supported.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pattern_loop_stream_with_automation_and_clap_audio_channels(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        channel_instances: &BTreeMap<u16, u64>,
        parameter_automation: &BTreeMap<u16, Vec<Vst3ParameterAutomation>>,
        clap_audio_channels: &BTreeSet<u16>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        if !(8_000..=384_000).contains(&output_sample_rate) {
            return Err("Pattern audio rate must be between 8000 and 384000 Hz".to_owned());
        }
        if loop_length_ticks == 0 {
            return Err("Pattern VST3 loop length must be greater than zero".to_owned());
        }
        let ppq = document.header().ppq();
        if ppq == 0 {
            return Err("project PPQ must be greater than zero".to_owned());
        }
        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
        let tempo_map = PlaylistTempoMap::constant(ppq, tempo_bpm)?;
        let pattern = document
            .patterns()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or_else(|| format!("Pattern {pattern_id} was not found"))?;
        let channels = document.channels();
        let channels_by_id = channels
            .iter()
            .map(|channel| (channel.id(), channel))
            .collect::<BTreeMap<_, _>>();
        let output_mixer_offsets_by_channel =
            flp_vst3_output_mixer_offsets_for_loaded_channels(self, document, channel_instances);
        let mut plugin_channels = document
            .channel_plugin_states()
            .into_iter()
            .map(|state| state.channel_id())
            .collect::<BTreeSet<_>>();
        plugin_channels.extend(
            channels
                .iter()
                .filter(|channel| channel.plugin_identifier().is_some())
                .map(|channel| channel.id()),
        );
        let vst3_volume_expression_channels = self.volume_expression_channels(channel_instances);
        let router = ChannelNoteRouter::new(channels.clone());
        let mixer_route_audibility = MixerRouteAudibility::from_document(document);
        let global_swing_mix_raw = document.metadata().global_swing_mix();
        let mut notes_by_channel = BTreeMap::<u16, Vec<PlaylistMidiNote>>::new();
        for (note_index, note) in pattern.notes.iter().enumerate() {
            if note.is_muted() {
                continue;
            }
            let nominal_start_tick = u64::from(note.position);
            let source_swing_mix = channels_by_id
                .get(&note.channel_id)
                .map_or(128, |channel| channel.swing_mix());
            let start_tick = swing_note_start_tick(
                nominal_start_tick,
                ppq,
                global_swing_mix_raw,
                source_swing_mix,
            )?;
            let swing_offset = start_tick.saturating_sub(nominal_start_tick);
            let stop_tick = nominal_start_tick
                .checked_add(u64::from(note.length.max(1)))
                .and_then(|tick| tick.checked_add(swing_offset))
                .ok_or_else(|| "Pattern VST3 note end position overflow".to_owned())?;
            for (target_channel_id, layer_gain, layer_pan_raw) in router
                .targets_for_note_with_clap_and_mixed_crossfade(
                    note.channel_id,
                    note_index as u64,
                    note.key,
                    &vst3_volume_expression_channels,
                    clap_audio_channels,
                )
            {
                if !plugin_channels.contains(&target_channel_id)
                    || !channels_by_id
                        .get(&target_channel_id)
                        .is_some_and(|channel| {
                            channel.enabled() != Some(false)
                                && vst3_channel_has_audible_output_route(
                                    &mixer_route_audibility,
                                    channel.mixer_track(),
                                    output_mixer_offsets_by_channel
                                        .get(&target_channel_id)
                                        .map(Vec::as_slice),
                                    true,
                                )
                        })
                {
                    continue;
                }
                notes_by_channel
                    .entry(target_channel_id)
                    .or_default()
                    .push(PlaylistMidiNote {
                        mod_y: note.mod_y,
                        start_tick,
                        stop_tick,
                        key: note.key,
                        velocity: note.velocity,
                        midi_channel: note.midi_channel,
                        fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
                        pan: note.pan,
                        mod_x: note.mod_x,
                        release: note.release,
                        layer_gain,
                        layer_pan_raw,
                    });
            }
        }

        // Keep loaded, enabled Pattern channels available for live note edits even if the
        // selected Pattern is silent on them when playback starts.
        for channel_id in channel_instances.keys().copied() {
            if plugin_channels.contains(&channel_id)
                && channels_by_id.get(&channel_id).is_some_and(|channel| {
                    channel.enabled() != Some(false)
                        && vst3_channel_has_audible_output_route(
                            &mixer_route_audibility,
                            channel.mixer_track(),
                            output_mixer_offsets_by_channel
                                .get(&channel_id)
                                .map(Vec::as_slice),
                            true,
                        )
                })
            {
                notes_by_channel.entry(channel_id).or_default();
            }
        }

        let loop_output_frames = tempo_map
            .frame_at_tick(loop_length_ticks, output_sample_rate)?
            .max(1);
        let pan_law = project_pan_law(document);
        let project_master_pitch_cents = document.metadata().master_pitch_cents();
        let mut streams = Vec::new();
        let mut live_input_senders = BTreeMap::new();
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
            let channel = channels_by_id.get(&channel_id).copied();
            let pitch_offset_cents = channel.map_or(0, |channel| {
                channel_pitch_offset_cents(channel, project_master_pitch_cents)
            });
            let note_pitch_offset_semitones = channel.map_or(0, |channel| {
                channel.master_pitch_note_offset_semitones(project_master_pitch_cents)
            });
            let (gain, pan) = channel_gain_pan(
                channel.and_then(|channel| channel.volume()),
                channel.and_then(|channel| channel.pan()),
            );
            let (render, latency_frames, loop_frames) = {
                let plugin_guard = plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                let plugin_sample_rate = plugin_guard.sample_rate();
                if !plugin_sample_rate.is_finite()
                    || !(8_000.0..=384_000.0).contains(&plugin_sample_rate)
                {
                    return Err(format!(
                        "VST3 channel {channel_id} has an invalid sample rate"
                    ));
                }
                let plugin_sample_rate_u32 = plugin_sample_rate.round() as u32;
                let loop_frames = tempo_map
                    .frame_at_tick(loop_length_ticks, plugin_sample_rate_u32)?
                    .max(1);
                let latency_frames = plugin_latency_in_output_frames(
                    channel_id,
                    self.pdc_latency_samples_for_plugin(
                        instance_id,
                        plugin_guard.latency_samples(),
                    ),
                    plugin_sample_rate,
                    output_sample_rate,
                )?;
                let mut render = prepare_playlist_pattern_render(
                    &plugin_guard,
                    &notes,
                    &tempo_map,
                    loop_frames,
                    pitch_offset_cents,
                    note_pitch_offset_semitones,
                    false,
                )?;
                apply_vst3_output_mixer_offsets(
                    &mut render,
                    &output_mixer_offsets_by_channel,
                    channel_id,
                );
                if let Some(lanes) = parameter_automation.get(&channel_id) {
                    let mut prepared =
                        prepare_parameter_automation(lanes, plugin_sample_rate, tempo_bpm)?;
                    for lane in &mut prepared {
                        lane.points.retain(|point| point.frame < loop_frames);
                    }
                    prepared.retain(|lane| !lane.points.is_empty());
                    validate_looped_parameter_automation_block_density(
                        &prepared,
                        render.block_size,
                        loop_frames,
                    )?;
                    render.parameter_automation = prepared;
                }
                (render, latency_frames, loop_frames)
            };
            let output_buses = prepare_playlist_plugin_output_buses(
                &render,
                output_sample_rate,
                channel
                    .map(|channel| channel.mixer_track())
                    .unwrap_or_default(),
                gain,
                pan,
                pan_law,
                &mixer_route_audibility,
            )?;
            notes_scheduled = notes_scheduled.saturating_add(render.note_count);
            let template_events = render.events.clone();
            let (live_midi_sender, live_midi_receiver) =
                mpsc::sync_channel(PLAYLIST_LIVE_MIDI_QUEUE_CAPACITY);
            let live_midi_stopped_generation = Arc::new(AtomicU64::new(0));
            live_input_senders.insert(
                channel_id,
                Vst3PlaylistLiveInputSender {
                    sender: live_midi_sender,
                    next_generation: Arc::new(AtomicU64::new(0)),
                    stopped_generation: Arc::clone(&live_midi_stopped_generation),
                },
            );
            streams.push(PlaylistPluginStream {
                channel_id,
                plugin_instance_id: instance_id,
                plugin,
                live_midi_receiver,
                live_midi_stopped_generation,
                live_midi_active_notes: BTreeMap::new(),
                live_midi_sustain_channels: BTreeMap::new(),
                render,
                output_buses,
                source_frame: 0,
                next_event: 0,
                resampler_finished: false,
                latency_frames,
                manual_latency_offset_samples: self.manual_latency_offset_for_plugin(instance_id),
                live_performance: true,
                active_performance_notes: HashMap::new(),
                active_pattern_note_ids: BTreeSet::new(),
                note_pitch_offset_semitones,
                pattern_loop: Some(PatternLoopEventQueue::new(0, loop_frames, template_events)),
            });
        }

        let stream_latencies = streams
            .iter()
            .map(|stream| stream.latency_frames)
            .collect::<Vec<_>>();
        let (maximum_latency_frames, _) = plan_pdc_delays(&stream_latencies);
        let mut processor = Vst3PlaylistStreamProcessor {
            streams,
            mixer_effects: Vec::new(),
            mixer_effect_chains: Vec::new(),
            mixer_effect_scratch: Vec::new(),
            mixer_effect_dry_scratch: Vec::new(),
            mixer_effect_post_effect_bus_scratch: Vec::new(),
            mixer_effect_tail_frames: 0,
            mixer_effect_dry_delay_frames: 0,
            mixer_effect_route_delay_frames: 0,
            mixer_effect_pdc_latency_frames: 0,
            mixer_effect_output_delay: StereoFrameDelay::default(),
            mixer_effect_frame: 0,
            mixer_effect_meter_input_delays: Vec::new(),
            mixer_effect_input_delays: Vec::new(),
            mixer_post_effect_stem_delays: Vec::new(),
            mixer_routing: MixerRouteAudibility::default(),
            include_effect_tails: false,
            live_input_senders,
            volume_expression_channel_ids: vst3_volume_expression_channels,
            performance_pattern_automation: BTreeMap::new(),
            tempo_map,
            output_sample_rate,
            content_frames: loop_output_frames,
            output_frames: loop_output_frames,
            maximum_latency_frames,
            automatic_pdc_enabled: document.mixer_automatic_pdc_enabled().unwrap_or(true),
            notes_scheduled,
            unloaded_plugin_channels: unloaded_plugin_channels.into_iter().collect(),
            started_count: 0,
            started_mixer_effect_count: 0,
        };
        processor.extend_to_output_frames(loop_output_frames)?;
        Ok(processor)
    }

    /// Prepare a repeating Pattern loop with loaded Mixer VST3 effects.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pattern_loop_stream_with_mixer_effects_and_automation(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        channel_instances: &BTreeMap<u16, u64>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        parameter_automation: &BTreeMap<u16, Vec<Vst3ParameterAutomation>>,
        mixer_effect_parameter_automation: &BTreeMap<
            (usize, u16),
            Vec<Vst3PlaylistParameterAutomation>,
        >,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        self.prepare_pattern_loop_stream_with_mixer_effects_and_automation_and_clap_audio_channels(
            document,
            pattern_id,
            loop_length_ticks,
            output_sample_rate,
            channel_instances,
            mixer_effect_instances,
            parameter_automation,
            mixer_effect_parameter_automation,
            &BTreeSet::new(),
        )
    }

    /// Prepare a repeating Pattern loop and Mixer effects with CLAP channel
    /// identities available for mixed-format Layer Crossfade routing.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pattern_loop_stream_with_mixer_effects_and_automation_and_clap_audio_channels(
        &self,
        document: &FlpDocument,
        pattern_id: u16,
        loop_length_ticks: u64,
        output_sample_rate: u32,
        channel_instances: &BTreeMap<u16, u64>,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        parameter_automation: &BTreeMap<u16, Vec<Vst3ParameterAutomation>>,
        mixer_effect_parameter_automation: &BTreeMap<
            (usize, u16),
            Vec<Vst3PlaylistParameterAutomation>,
        >,
        clap_audio_channels: &BTreeSet<u16>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let mut processor = self
            .prepare_pattern_loop_stream_with_automation_and_clap_audio_channels(
                document,
                pattern_id,
                loop_length_ticks,
                output_sample_rate,
                channel_instances,
                parameter_automation,
                clap_audio_channels,
            )?;
        let automation_loop_frames = processor.content_frames;
        self.attach_mixer_effects(
            document,
            mixer_effect_instances,
            &mut processor,
            false,
            true,
            mixer_effect_parameter_automation,
            Some(automation_loop_frames),
        )?;
        Ok(processor)
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_mixer_effects(
        &self,
        document: &FlpDocument,
        mixer_effect_instances: &BTreeMap<(usize, u16), u64>,
        processor: &mut Vst3PlaylistStreamProcessor,
        include_effect_tails: bool,
        live_mix_changes: bool,
        effect_parameter_automation: &BTreeMap<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>,
        effect_parameter_automation_loop_frames: Option<u64>,
    ) -> Result<(), String> {
        let inserts = document.mixer_inserts();
        let effect_slots = document.mixer_effect_slots();
        let routing = crate::MixerRouteAudibility::from_document(document);
        processor.mixer_routing = routing.clone();
        processor.include_effect_tails = include_effect_tails;
        let has_loaded_non_master_effect = effect_slots.iter().any(|slot| {
            if slot.insert_ordinal() == 0
                || (slot.enabled() == Some(false) && !live_mix_changes)
                || (!live_mix_changes && mixer_effect_wet_mix(slot.mix_raw()) == 0.0)
                || (!mixer_effect_instances
                    .contains_key(&(slot.insert_ordinal(), slot.slot_index()))
                    && !self
                        .mixer_effect_processor_factory
                        .as_ref()
                        .is_some_and(|factory| {
                            factory.contains(slot.insert_ordinal(), slot.slot_index())
                        }))
            {
                return false;
            }
            inserts
                .iter()
                .find(|insert| insert.ordinal() == slot.insert_ordinal())
                .is_none_or(|insert| live_mix_changes || insert.effects_enabled() != Some(false))
        });
        let mut insert_order = if has_loaded_non_master_effect {
            routing.mixer_insert_processing_order()?
        } else {
            Vec::new()
        };
        insert_order.push(0);
        let output_sample_rate = processor.output_sample_rate;
        let max_tail_frames =
            u64::from(output_sample_rate).saturating_mul(MAX_REPORTED_TAIL_SECONDS.ceil() as u64);
        let instrument_delay_frames = if processor.automatic_pdc_enabled {
            processor.streams.iter().try_fold(
                processor.maximum_latency_frames,
                |total, stream| {
                    total
                        .checked_add(
                            processor
                                .maximum_latency_frames
                                .saturating_sub(stream.latency_frames),
                        )
                        .ok_or_else(|| "PDC delay storage size overflow".to_owned())
                },
            )?
        } else {
            0
        };
        let mut mixer_effect_dry_delay_frames = processor.mixer_effect_dry_delay_frames;
        for insert_ordinal in insert_order {
            let saved_insert_effects_enabled = inserts
                .iter()
                .find(|insert| insert.ordinal() == insert_ordinal)
                .is_none_or(|insert| insert.effects_enabled() != Some(false));
            if !saved_insert_effects_enabled && !live_mix_changes {
                continue;
            }
            let sidechain_sources = routing.mixer_sidechain_sources_for_insert(insert_ordinal);
            let first_effect_index = processor.mixer_effects.len();
            for slot in effect_slots
                .iter()
                .filter(|slot| slot.insert_ordinal() == insert_ordinal)
            {
                if slot.enabled() == Some(false) && !live_mix_changes {
                    continue;
                }
                let key = (slot.insert_ordinal(), slot.slot_index());
                let wet_mix = mixer_effect_wet_mix(slot.mix_raw());
                if wet_mix == 0.0 && !live_mix_changes {
                    continue;
                }
                let saved_enabled = slot.enabled() != Some(false);
                let has_external_processor = self
                    .mixer_effect_processor_factory
                    .as_ref()
                    .is_some_and(|factory| factory.contains(key.0, key.1));
                let (
                    id,
                    plugin,
                    external_processor,
                    buffers,
                    block_size,
                    input_channels,
                    output_channels,
                    external_auxiliary_input_buses,
                    external_auxiliary_input_source_map,
                    external_auxiliary_scratch,
                    sample_rate,
                    latency_samples,
                    tail_samples,
                    manual_latency_offset_samples,
                ) = if has_external_processor {
                    let processor = self
                        .mixer_effect_processor_factory
                        .as_ref()
                        .expect("external Mixer processor factory was checked")
                        .create(key.0, key.1, output_sample_rate)?
                        .ok_or_else(|| {
                            format!(
                                "Mixer insert {} effect slot {} has a CLAP assignment but no processor",
                                key.0,
                                key.1 + 1
                            )
                        })?;
                    let sample_rate = processor.sample_rate();
                    let rate_tolerance = sample_rate.abs().max(1.0) * 1e-12;
                    if (sample_rate - f64::from(output_sample_rate)).abs() > rate_tolerance {
                        return Err(format!(
                            "CLAP Mixer insert {} effect slot {} uses {sample_rate} Hz but Playlist output is {output_sample_rate} Hz",
                            key.0,
                            key.1 + 1
                        ));
                    }
                    let input_channels = processor.input_channels();
                    let output_channels = processor.output_channels();
                    if !matches!(input_channels, 1 | 2) || !matches!(output_channels, 1 | 2) {
                        return Err(format!(
                            "CLAP Mixer insert {} effect slot {} needs mono or stereo main buses",
                            key.0,
                            key.1 + 1
                        ));
                    }
                    let block_size = processor
                        .block_size()
                        .clamp(1, VST3_EFFECT_RENDER_BLOCK_FRAMES);
                    let external_auxiliary_input_buses = processor.auxiliary_input_buses().to_vec();
                    let external_auxiliary_input_source_map = processor
                        .auxiliary_input_source_map()
                        .map(<[(usize, usize)]>::to_vec);
                    if external_auxiliary_input_buses.len() > 16
                        || external_auxiliary_input_buses
                            .iter()
                            .any(|(_, channels)| !matches!(channels, 1 | 2))
                    {
                        return Err(format!(
                            "CLAP Mixer insert {} effect slot {} reports an unsupported auxiliary input layout",
                            key.0,
                            key.1 + 1
                        ));
                    }
                    let auxiliary_scratch_capacity = external_auxiliary_input_buses
                        .len()
                        .checked_mul(block_size)
                        .and_then(|count| count.checked_mul(2))
                        .ok_or_else(|| "CLAP sidechain scratch size overflow".to_owned())?;
                    let external_auxiliary_scratch = Vec::with_capacity(auxiliary_scratch_capacity);
                    let latency_samples = processor.latency_samples();
                    let tail_samples = processor.tail_samples();
                    (
                        0u64,
                        None,
                        Some(processor),
                        None,
                        block_size,
                        input_channels,
                        output_channels,
                        external_auxiliary_input_buses,
                        external_auxiliary_input_source_map,
                        external_auxiliary_scratch,
                        sample_rate,
                        latency_samples,
                        tail_samples,
                        0i32,
                    )
                } else {
                    let Some(id) = mixer_effect_instances.get(&key).copied() else {
                        continue;
                    };
                    let plugin = Arc::clone(self.plugin(id)?);
                    let (
                        buffers,
                        block_size,
                        input_channels,
                        output_channels,
                        sample_rate,
                        latency_samples,
                        tail_samples,
                    ) = {
                        let plugin_guard = plugin
                            .lock()
                            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                        let sample_rate = plugin_guard.sample_rate();
                        let rate_tolerance = sample_rate.abs().max(1.0) * 1e-12;
                        if (sample_rate - f64::from(output_sample_rate)).abs() > rate_tolerance {
                            return Err(format!(
                                "VST3 Mixer insert {} effect slot {} uses {sample_rate} Hz but Playlist output is {output_sample_rate} Hz",
                                insert_ordinal,
                                slot.slot_index() + 1
                            ));
                        }
                        let layout = plugin_guard
                            .audio_bus_layout()
                            .map_err(|error| error.to_string())?;
                        let input = layout
                            .inputs
                            .first()
                            .filter(|bus| bus.active && matches!(bus.channel_count, 1 | 2))
                            .ok_or_else(|| {
                                format!(
                                    "VST3 Mixer insert {} effect slot {} has no active mono or stereo main input bus",
                                    insert_ordinal,
                                    slot.slot_index() + 1
                                )
                            })?;
                        let output = layout
                            .outputs
                            .first()
                            .filter(|bus| bus.active && matches!(bus.channel_count, 1 | 2))
                            .ok_or_else(|| {
                                format!(
                                    "VST3 Mixer insert {} effect slot {} has no active mono or stereo main output bus",
                                    insert_ordinal,
                                    slot.slot_index() + 1
                                )
                            })?;
                        let block_size = plugin_guard
                            .block_size()
                            .clamp(1, VST3_EFFECT_RENDER_BLOCK_FRAMES);
                        let buffers = plugin_guard
                            .create_bus_audio_buffers(block_size)
                            .map_err(|error| error.to_string())?;
                        (
                            buffers,
                            block_size,
                            input.channel_count,
                            output.channel_count,
                            sample_rate,
                            self.pdc_latency_samples_for_plugin(id, plugin_guard.latency_samples()),
                            (plugin_guard.tail_samples() != u32::MAX)
                                .then(|| u64::from(plugin_guard.tail_samples())),
                        )
                    };
                    (
                        id,
                        Some(plugin),
                        None,
                        Some(buffers),
                        block_size,
                        input_channels,
                        output_channels,
                        Vec::new(),
                        None,
                        Vec::new(),
                        sample_rate,
                        latency_samples,
                        tail_samples,
                        self.manual_latency_offset_for_plugin(id),
                    )
                };
                if !sample_rate.is_finite() || sample_rate <= 0.0 {
                    return Err(format!(
                        "VST3 Mixer insert {} effect slot {} reports an invalid sample rate",
                        insert_ordinal,
                        slot.slot_index() + 1
                    ));
                }
                let latency_frames = u64::from(latency_samples);
                let maximum_latency_frames =
                    (sample_rate * MAX_VST3_EFFECT_LATENCY_SECONDS).ceil() as u64;
                if latency_frames > maximum_latency_frames {
                    return Err(format!(
                        "VST3 Mixer insert {insert_ordinal} effect slot {} reports {latency_frames} frames of latency, exceeding the {MAX_VST3_EFFECT_LATENCY_SECONDS}-second processing limit",
                        slot.slot_index() + 1
                    ));
                }
                let dry_delay_frames = if live_mix_changes || wet_mix < 1.0 {
                    latency_frames
                } else {
                    0
                };
                let next_dry_delay_frames = mixer_effect_dry_delay_frames
                    .checked_add(dry_delay_frames)
                    .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
                let total_delay_frames = instrument_delay_frames
                    .checked_add(next_dry_delay_frames)
                    .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
                let delay_bytes = total_delay_frames
                    .checked_mul(std::mem::size_of::<[f32; 2]>() as u64)
                    .ok_or_else(|| "PDC delay storage size overflow".to_owned())?;
                if delay_bytes > MAX_PDC_DELAY_BYTES {
                    return Err(format!(
                        "Mixer effect PDC needs {delay_bytes} bytes of delay storage, exceeding the 128 MiB limit"
                    ));
                }
                let dry_delay_line = StereoFrameDelay::new(dry_delay_frames)?;
                mixer_effect_dry_delay_frames = next_dry_delay_frames;
                let parameter_automation = effect_parameter_automation
                    .get(&key)
                    .map(|lanes| {
                        let prepared = prepare_playlist_parameter_automation(
                            lanes,
                            &processor.tempo_map,
                            output_sample_rate,
                            processor.content_frames,
                        )?;
                        if let Some(loop_frames) = effect_parameter_automation_loop_frames {
                            validate_looped_parameter_automation_block_density(
                                &prepared,
                                block_size,
                                loop_frames,
                            )?;
                        } else {
                            validate_parameter_automation_block_density(
                                &prepared,
                                block_size,
                                processor.content_frames,
                            )?;
                        }
                        Ok::<_, String>(prepared)
                    })
                    .transpose()?
                    .unwrap_or_default();
                let reported_tail = tail_samples
                    .map(|tail_samples| {
                        ((tail_samples as f64 / sample_rate).min(MAX_REPORTED_TAIL_SECONDS)
                            * f64::from(output_sample_rate))
                        .ceil() as u64
                    })
                    .unwrap_or(0);
                if include_effect_tails {
                    processor.mixer_effect_tail_frames = processor
                        .mixer_effect_tail_frames
                        .saturating_add(reported_tail)
                        .min(max_tail_frames);
                }
                processor.mixer_effects.push(PlaylistMixerEffectStream {
                    id,
                    insert_ordinal,
                    slot_index: slot.slot_index(),
                    plugin,
                    external_processor,
                    buffers,
                    block_size,
                    input_channels,
                    output_channels,
                    external_auxiliary_input_buses,
                    external_auxiliary_input_source_map,
                    external_auxiliary_scratch,
                    sidechain_inputs: Vec::new(),
                    parameter_automation,
                    parameter_automation_loop_frames: effect_parameter_automation_loop_frames,
                    parameter_automation_loop_start_frame: 0,
                    wet_mix,
                    saved_enabled,
                    saved_insert_effects_enabled,
                    live_wet_mix_target: None,
                    live_enabled_target: None,
                    live_insert_effects_enabled_target: None,
                    wet_mix_ramp: LiveWetMixRamp::new(
                        if saved_enabled && saved_insert_effects_enabled {
                            wet_mix
                        } else {
                            0.0
                        },
                        (sample_rate * f64::from(MIXER_EFFECT_MIX_SMOOTHING_SECONDS))
                            .round()
                            .clamp(1.0, f64::from(u32::MAX)) as u32,
                    ),
                    latency_frames,
                    tail_frames: if include_effect_tails {
                        reported_tail
                    } else {
                        0
                    },
                    dry_delay_required: live_mix_changes || wet_mix < 1.0,
                    manual_latency_offset_samples,
                    dry_delay_line,
                    dry_delay_scratch: vec![0.0; block_size * 2],
                    wet_scratch: vec![0.0; block_size * 2],
                });
                update_playlist_mixer_effect_sidechain_inputs(
                    processor
                        .mixer_effects
                        .last_mut()
                        .expect("Mixer effect was just pushed"),
                    &sidechain_sources,
                    sample_rate,
                )?;
            }
            let end_effect_index = processor.mixer_effects.len();
            if first_effect_index == end_effect_index {
                continue;
            }
            let route_plan = if insert_ordinal == 0 {
                crate::MixerSourceRoutePlan::default()
            } else {
                let track = i8::try_from(insert_ordinal).map_err(|_| {
                    format!("Mixer insert ordinal {insert_ordinal} exceeds the supported range")
                })?;
                routing.source_route_plan_for_channel(Some(track))
            };
            let post_effect_meter_transform = route_plan
                .insert_meter_transforms
                .iter()
                .find(|(track, _)| *track == insert_ordinal)
                .map_or_else(MixerSourceRouteTransform::default, |(_, transform)| {
                    *transform
                });
            let live_source = if insert_ordinal == 0 {
                None
            } else {
                let track = i8::try_from(insert_ordinal).map_err(|_| {
                    format!("Mixer insert ordinal {insert_ordinal} exceeds the supported range")
                })?;
                routing
                    .live_source_route_plan_for_channel(Some(track))
                    .map(|route_plan| PlaylistMixerEffectLiveSource {
                        control: LiveMixerSourceControl::new(route_plan, output_sample_rate),
                        targets: None,
                    })
            };
            let latency_frames = processor.mixer_effects[first_effect_index..end_effect_index]
                .iter()
                .try_fold(0_u64, |total, effect| {
                    total
                        .checked_add(effect.latency_frames)
                        .ok_or_else(|| "Mixer effect chain latency overflow".to_owned())
                })?;
            processor
                .mixer_effect_chains
                .push(PlaylistMixerEffectChain {
                    insert_ordinal,
                    route_plan,
                    live_source,
                    post_effect_meter_transform,
                    first_effect_index,
                    end_effect_index,
                    latency_frames,
                    output_alignment_delay: StereoFrameDelay::default(),
                    dry_reference_delay: StereoFrameDelay::default(),
                    downstream_delays: Vec::new(),
                });
        }
        processor.mixer_effect_dry_delay_frames = mixer_effect_dry_delay_frames;
        configure_mixer_effect_pdc(processor, &routing, instrument_delay_frames)?;
        let content_frames = processor.content_frames;
        processor.extend_to_output_frames(content_frames)?;
        Ok(())
    }

    fn prepare_playlist_stream_inner(
        &self,
        document: &FlpDocument,
        options: PlaylistRenderOptions,
        channel_instances: &BTreeMap<u16, u64>,
        include_plugin_reported_tails: bool,
        parameter_automation: &BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>,
        live_performance_channels: &BTreeSet<u16>,
    ) -> Result<Vst3PlaylistStreamProcessor, String> {
        let arrangement_id = options.arrangement_id;
        let output_sample_rate = options.sample_rate;
        let tail_seconds = f64::from(options.tail_seconds);
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
        let pan_law = project_pan_law(document);
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))?;
        let tempo_map = PlaylistTempoMap::for_arrangement(document, arrangement_id)?;
        let patterns = document.patterns().map_err(|error| error.to_string())?;
        let channels = document.channels();
        let channels_by_id: BTreeMap<_, _> = channels
            .iter()
            .map(|channel| (channel.id(), channel))
            .collect();
        let output_mixer_offsets_by_channel =
            flp_vst3_output_mixer_offsets_for_loaded_channels(self, document, channel_instances);
        let project_master_pitch_cents = document.metadata().master_pitch_cents();
        let mixer_route_audibility = MixerRouteAudibility::from_document(document);
        let channel_router = ChannelNoteRouter::new(channels.clone());
        let vst3_volume_expression_channels = self.volume_expression_channels(channel_instances);
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
        let track_filter =
            PlaylistTrackFilter::new(&disabled_track_ids, options.soloed_playlist_track_range);
        let mut clap_audio_channels = options
            .clap_audio_channel_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        clap_audio_channels.extend(
            options
                .clap_audio_layers
                .iter()
                .map(|layer| layer.channel_id),
        );
        plugin_channels.extend(
            channels
                .iter()
                .filter(|channel| {
                    channel.plugin_identifier().is_some() && channel.enabled() != Some(false)
                })
                .map(|channel| channel.id()),
        );
        let schedule = schedule_playlist_pattern_notes_with_adjustments(
            &patterns,
            &arrangement,
            track_filter,
            PlaylistPatternScheduleOptions {
                ppq,
                global_swing_mix_raw: document.metadata().global_swing_mix(),
                play_truncated_notes_in_clips: document
                    .project_settings()
                    .is_none_or(|settings| settings.play_truncated_notes_in_clips),
            },
            |channel_id| {
                channels_by_id
                    .get(&channel_id)
                    .map_or(128, |channel| channel.swing_mix())
            },
            |channel_id, seed, key| {
                channel_router
                    .targets_for_note_with_clap_and_mixed_crossfade(
                        channel_id,
                        seed,
                        key,
                        &vst3_volume_expression_channels,
                        &clap_audio_channels,
                    )
                    .into_iter()
                    .filter(|(target_channel_id, _, _)| {
                        plugin_channels.contains(target_channel_id)
                            && channels_by_id
                                .get(target_channel_id)
                                .is_some_and(|channel| {
                                    vst3_channel_has_audible_output_route(
                                        &mixer_route_audibility,
                                        channel.mixer_track(),
                                        output_mixer_offsets_by_channel
                                            .get(target_channel_id)
                                            .map(Vec::as_slice),
                                        true,
                                    )
                                })
                    })
                    .collect()
            },
        )?;

        let mut notes_by_channel = BTreeMap::<u16, Vec<PlaylistMidiNote>>::new();
        for placed in &schedule.notes {
            let note = placed.note;
            let stop_tick = match placed.stop_tick {
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
                    mod_y: note.mod_y,
                    start_tick: placed.start_tick,
                    stop_tick,
                    key: note.key,
                    velocity: note.velocity,
                    midi_channel: note.midi_channel,
                    fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
                    pan: note.pan,
                    mod_x: note.mod_x,
                    release: note.release,
                    layer_gain: placed.target_gain,
                    layer_pan_raw: placed.target_pan_raw,
                });
        }
        for (&channel_id, lanes) in parameter_automation {
            if !lanes.is_empty() {
                notes_by_channel.entry(channel_id).or_default();
            }
        }
        for &channel_id in live_performance_channels {
            if channel_instances.contains_key(&channel_id) && plugin_channels.contains(&channel_id)
            {
                notes_by_channel.entry(channel_id).or_default();
            }
        }

        let clip_end_tick = arrangement.clips.iter().fold(0u64, |end, clip| {
            end.max(u64::from(clip.position_ticks) + u64::from(clip.length_ticks))
        });
        let scheduled_note_end_tick = notes_by_channel
            .values()
            .flatten()
            .map(|note| note.stop_tick)
            .max()
            .unwrap_or(0);
        let max_tick = clip_end_tick.max(scheduled_note_end_tick);
        let base_output_frames = tempo_map.frame_at_tick(max_tick, output_sample_rate)? as f64;
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
        let mut live_input_senders = BTreeMap::new();
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
            let pitch_offset_cents = channel.map_or(0, |channel| {
                channel_pitch_offset_cents(channel, project_master_pitch_cents)
            });
            let note_pitch_offset_semitones = channel.map_or(0, |channel| {
                channel.master_pitch_note_offset_semitones(project_master_pitch_cents)
            });
            let (gain, pan) = channel_gain_pan(
                channel.and_then(|channel| channel.volume()),
                channel.and_then(|channel| channel.pan()),
            );
            let (mut render, latency_frames) = {
                let plugin_guard = plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                let plugin_sample_rate = plugin_guard.sample_rate();
                let latency_frames = plugin_latency_in_output_frames(
                    channel_id,
                    self.pdc_latency_samples_for_plugin(
                        instance_id,
                        plugin_guard.latency_samples(),
                    ),
                    plugin_sample_rate,
                    output_sample_rate,
                )?;
                if include_plugin_reported_tails
                    && let Some(tail_seconds) =
                        reported_tail_seconds(plugin_guard.tail_samples(), plugin_sample_rate)
                {
                    longest_plugin_tail_seconds = longest_plugin_tail_seconds.max(tail_seconds);
                }
                let source_frames = (output_frames as f64 * plugin_sample_rate
                    / f64::from(output_sample_rate))
                .ceil();
                if !source_frames.is_finite() || source_frames > u64::MAX as f64 {
                    return Err(format!(
                        "VST3 channel {channel_id} Playlist length is outside the renderable range"
                    ));
                }
                let render = prepare_playlist_pattern_render(
                    &plugin_guard,
                    &notes,
                    &tempo_map,
                    source_frames as u64,
                    pitch_offset_cents,
                    note_pitch_offset_semitones,
                    live_performance_channels.contains(&channel_id)
                        || parameter_automation
                            .get(&channel_id)
                            .is_some_and(|lanes| !lanes.is_empty()),
                )?;
                (render, latency_frames)
            };
            apply_vst3_output_mixer_offsets(
                &mut render,
                &output_mixer_offsets_by_channel,
                channel_id,
            );
            if let Some(lanes) = parameter_automation.get(&channel_id) {
                let prepared = prepare_playlist_parameter_automation(
                    lanes,
                    &tempo_map,
                    render.sample_rate_u32,
                    render.total_frames,
                )?;
                validate_parameter_automation_block_density(
                    &prepared,
                    render.block_size,
                    render.total_frames,
                )?;
                render.parameter_automation = prepared;
            }
            let output_buses = prepare_playlist_plugin_output_buses(
                &render,
                output_sample_rate,
                channel
                    .map(|channel| channel.mixer_track())
                    .unwrap_or_default(),
                gain,
                pan,
                pan_law,
                &mixer_route_audibility,
            )?;
            notes_scheduled += render.note_count;
            let (live_midi_sender, live_midi_receiver) =
                mpsc::sync_channel(PLAYLIST_LIVE_MIDI_QUEUE_CAPACITY);
            let live_midi_stopped_generation = Arc::new(AtomicU64::new(0));
            live_input_senders.insert(
                channel_id,
                Vst3PlaylistLiveInputSender {
                    sender: live_midi_sender,
                    next_generation: Arc::new(AtomicU64::new(0)),
                    stopped_generation: Arc::clone(&live_midi_stopped_generation),
                },
            );
            streams.push(PlaylistPluginStream {
                channel_id,
                plugin_instance_id: instance_id,
                plugin,
                live_midi_receiver,
                live_midi_stopped_generation,
                live_midi_active_notes: BTreeMap::new(),
                live_midi_sustain_channels: BTreeMap::new(),
                render,
                output_buses,
                source_frame: 0,
                next_event: 0,
                resampler_finished: false,
                latency_frames,
                manual_latency_offset_samples: self.manual_latency_offset_for_plugin(instance_id),
                live_performance: live_performance_channels.contains(&channel_id),
                active_performance_notes: HashMap::new(),
                active_pattern_note_ids: BTreeSet::new(),
                note_pitch_offset_semitones,
                pattern_loop: None,
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
        let stream_latencies = streams
            .iter()
            .map(|stream| stream.latency_frames)
            .collect::<Vec<_>>();
        let (maximum_latency_frames, _) = plan_pdc_delays(&stream_latencies);
        let mut processor = Vst3PlaylistStreamProcessor {
            streams,
            mixer_effects: Vec::new(),
            mixer_effect_chains: Vec::new(),
            mixer_effect_scratch: Vec::new(),
            mixer_effect_dry_scratch: Vec::new(),
            mixer_effect_post_effect_bus_scratch: Vec::new(),
            mixer_effect_tail_frames: 0,
            mixer_effect_dry_delay_frames: 0,
            mixer_effect_route_delay_frames: 0,
            mixer_effect_pdc_latency_frames: 0,
            mixer_effect_output_delay: StereoFrameDelay::default(),
            mixer_effect_frame: 0,
            mixer_effect_meter_input_delays: Vec::new(),
            mixer_effect_input_delays: Vec::new(),
            mixer_post_effect_stem_delays: Vec::new(),
            mixer_routing: MixerRouteAudibility::default(),
            include_effect_tails: false,
            live_input_senders,
            volume_expression_channel_ids: vst3_volume_expression_channels,
            performance_pattern_automation: BTreeMap::new(),
            tempo_map,
            output_sample_rate,
            content_frames: output_frames,
            output_frames,
            maximum_latency_frames,
            automatic_pdc_enabled: document.mixer_automatic_pdc_enabled().unwrap_or(true),
            notes_scheduled,
            unloaded_plugin_channels: unloaded_plugin_channels.into_iter().collect(),
            started_count: 0,
            started_mixer_effect_count: 0,
        };
        processor.extend_to_output_frames(output_frames)?;
        Ok(processor)
    }

    /// Service editor/UI requests and VST3 restart requests on the control thread.
    ///
    /// This method refreshes reported latency in `HostedPluginInfo` after an instance has stopped
    /// processing. Active Performance streams can instead refresh the requested instances at a
    /// block boundary; other active streams must rebuild when `latency_changed` is returned.
    /// Output routing and buffers must be rebuilt when `io_changed` is returned.
    pub fn service_editors(&mut self) -> Result<Vst3HostServiceChanges, String> {
        let mut changes = Vst3HostServiceChanges::default();
        for loaded in &mut self.loaded {
            let mut plugin = loaded
                .plugin
                .lock()
                .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
            plugin.service_run_loop();
            let restart_flags = plugin
                .service_host_requests()
                .map_err(|error| error.to_string())?;
            if restart_flags.latency_changed() {
                self.pending_latency_refresh.insert(loaded.info.id);
                changes.latency_changed = true;
                changes.latency_changed_while_processing |= plugin.is_processing();
                changes.latency_changed_plugin_ids.insert(loaded.info.id);
            }
            if self.pending_latency_refresh.contains(&loaded.info.id) && !plugin.is_processing() {
                loaded.info.latency_samples = plugin.latency_samples();
                loaded.info.sample_rate_hz = plugin.sample_rate();
                self.pending_latency_refresh.remove(&loaded.info.id);
            }
            changes.io_changed |= restart_flags.io_changed();
        }
        Ok(changes)
    }

    pub fn loaded_plugins(&self) -> Vec<HostedPluginInfo> {
        self.loaded
            .iter()
            .map(|loaded| loaded.info.clone())
            .collect()
    }

    /// Apply a latency report received from an active Playlist processor.
    pub fn apply_latency_report(&mut self, report: Vst3LatencyReport) {
        if let Some(loaded) = self
            .loaded
            .iter_mut()
            .find(|loaded| loaded.info.id == report.plugin_id)
        {
            loaded.info.latency_samples = report.latency_samples;
            loaded.info.sample_rate_hz = report.sample_rate_hz;
            self.pending_latency_refresh.remove(&report.plugin_id);
        }
    }

    fn plugin(&self, id: u64) -> Result<&Arc<Mutex<Plugin>>, String> {
        self.loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map(|loaded| &loaded.plugin)
            .ok_or_else(|| format!("no loaded VST3 instance with id {id}"))
    }

    /// Returns loaded project channels whose plug-ins advertise Volume note expression.
    pub fn volume_expression_channels(
        &self,
        channel_instances: &BTreeMap<u16, u64>,
    ) -> BTreeSet<u16> {
        channel_instances
            .iter()
            .filter_map(|(channel_id, instance_id)| {
                let plugin = self.plugin(*instance_id).ok()?.lock().ok()?;
                plugin_note_expression_support(&plugin)
                    .1
                    .then_some(*channel_id)
            })
            .collect()
    }

    fn pdc_latency_samples_for_plugin(&self, id: u64, reported_latency_samples: u32) -> u32 {
        self.loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map_or(reported_latency_samples, |loaded| {
                apply_manual_latency_offset(
                    reported_latency_samples,
                    loaded.info.manual_latency_offset_samples,
                )
            })
    }

    fn manual_latency_offset_for_plugin(&self, id: u64) -> i32 {
        self.loaded
            .iter()
            .find(|loaded| loaded.info.id == id)
            .map_or(0, |loaded| loaded.info.manual_latency_offset_samples)
    }
}

fn expand_playlist_parameter_automation(
    document: &FlpDocument,
    options: PlaylistRenderOptions,
    pattern_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
    automation_clip_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
) -> Result<BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>, String> {
    if pattern_automation.is_empty() && automation_clip_automation.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == options.arrangement_id)
        .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
    let patterns = if pattern_automation.is_empty() {
        Vec::new()
    } else {
        document.patterns().map_err(|error| error.to_string())?
    };
    let patterns_by_id = patterns
        .iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<BTreeMap<_, _>>();
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let clip_is_muted = |clip: &crate::PlaylistClip| {
        if clip.is_muted() {
            return true;
        }
        let track_id = clip.playlist_track_id();
        if let Some((first_track, last_track)) = options.soloed_playlist_track_range {
            !track_id.is_some_and(|track_id| (first_track..=last_track).contains(&track_id))
        } else {
            track_id.is_some_and(|track_id| disabled_track_ids.contains(&track_id))
        }
    };
    let mut points_by_lane = BTreeMap::<(u16, u32), Vec<(u64, usize, usize, f64, f32)>>::new();
    let mut expanded_point_count = 0usize;

    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        if clip_is_muted(clip)
            || clip
                .scale
                .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            continue;
        }
        if clip.length_ticks == 0 {
            continue;
        }
        let clip_automation = match clip.target() {
            PlaylistClipTarget::Pattern { id: pattern_id } => {
                let Some(pattern) = patterns_by_id.get(&pattern_id).copied() else {
                    continue;
                };
                expand_pattern_parameter_automation_for_clip(
                    pattern_id,
                    pattern,
                    clip,
                    ppq,
                    document.metadata().time_signature().unwrap_or((4, 4)),
                    pattern_automation,
                )?
            }
            PlaylistClipTarget::Channel {
                id: automation_channel_id,
            } => expand_automation_clip_parameter_automation_for_clip(
                automation_channel_id,
                clip,
                ppq,
                automation_clip_automation,
            )?,
        };
        for (channel_id, lanes) in clip_automation {
            for lane in lanes {
                for (point_index, point) in lane.points.into_iter().enumerate() {
                    let absolute_tick = u64::from(clip.position_ticks)
                        .checked_add(point.tick)
                        .ok_or_else(|| "Playlist automation point position overflow".to_owned())?;
                    expanded_point_count = expanded_point_count
                        .checked_add(1)
                        .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                        .ok_or_else(|| {
                            format!(
                                "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                            )
                        })?;
                    points_by_lane
                        .entry((channel_id, lane.parameter_id))
                        .or_default()
                        .push((
                            absolute_tick,
                            clip_index,
                            point_index,
                            point.value,
                            point.tension,
                        ));
                }
            }
        }
    }

    let mut automation_by_channel = BTreeMap::<u16, Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((channel_id, parameter_id), mut points) in points_by_lane {
        points.sort_by_key(|point| (point.0, point.1, point.2));
        let mut unique_points = Vec::<Vst3PlaylistAutomationPoint>::with_capacity(points.len());
        for (tick, _, _, value, tension) in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.tick == tick
            {
                previous.value = value;
                previous.tension = tension;
            } else {
                unique_points.push(Vst3PlaylistAutomationPoint {
                    tick,
                    value,
                    tension,
                });
            }
        }
        if !unique_points.is_empty() {
            automation_by_channel.entry(channel_id).or_default().push(
                Vst3PlaylistParameterAutomation {
                    parameter_id,
                    points: unique_points,
                },
            );
        }
    }
    Ok(automation_by_channel)
}

pub(crate) fn expand_pattern_parameter_automation_for_clip(
    pattern_id: u16,
    pattern: &crate::Pattern,
    clip: &crate::PlaylistClip,
    ppq: u16,
    default_time_signature: (u8, u8),
    pattern_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
) -> Result<BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if clip.length_ticks == 0
        || clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Ok(BTreeMap::new());
    }
    let clip_length = u64::from(clip.length_ticks);
    let inferred_note_length = pattern
        .notes
        .iter()
        .filter(|note| note.length > 0)
        .try_fold(0u64, |end, note| {
            let note_end = u64::from(note.position)
                .checked_add(u64::from(note.length))
                .ok_or_else(|| "Pattern note end position overflow".to_owned())?;
            Ok::<_, String>(end.max(note_end))
        })?;
    let signature = pattern
        .time_markers
        .iter()
        .filter(|marker| marker.is_signature() && marker.position_ticks() == 0)
        .find_map(|marker| Some((marker.numerator()?, marker.denominator()?)))
        .unwrap_or(default_time_signature);
    let bar_length = (u64::from(ppq)
        .saturating_mul(u64::from(signature.0.max(1)))
        .saturating_mul(4)
        / u64::from(signature.1.max(1)))
    .max(1);
    let loop_length = match pattern
        .length_ticks
        .map(u64::from)
        .filter(|length| *length > 0)
    {
        Some(explicit_length) => explicit_length.max(inferred_note_length),
        None => inferred_note_length.max(bar_length),
    };
    if loop_length == 0 {
        return Ok(BTreeMap::new());
    }
    let repetitions = clip_length.div_ceil(loop_length);
    let mut points_by_lane = BTreeMap::<(u16, u32), Vec<(u64, u64, usize, usize, f64, f32)>>::new();
    let mut expanded_point_count = 0usize;
    for ((linked_pattern_id, target_channel_id), lanes) in pattern_automation {
        if *linked_pattern_id != pattern_id {
            continue;
        }
        for (lane_index, lane) in lanes.iter().enumerate() {
            for repetition in 0..repetitions {
                let repeat_start = repetition
                    .checked_mul(loop_length)
                    .ok_or_else(|| "Pattern automation repeat position overflow".to_owned())?;
                for (point_index, point) in lane.points.iter().enumerate() {
                    if !point.position_beats.is_finite() || point.position_beats < 0.0 {
                        return Err(
                            "VST3 automation positions must be finite and non-negative".to_owned()
                        );
                    }
                    if !point.value.is_finite() || !(0.0..=1.0).contains(&point.value) {
                        return Err(
                            "VST3 automation values must be finite and normalized to 0..=1"
                                .to_owned(),
                        );
                    }
                    let point_tick = point.position_beats * f64::from(ppq);
                    if !point_tick.is_finite() || point_tick >= u64::MAX as f64 {
                        return Err(
                            "VST3 automation position is outside the renderable range".to_owned()
                        );
                    }
                    let relative_tick = repeat_start
                        .checked_add(point_tick.round() as u64)
                        .ok_or_else(|| "Pattern automation point position overflow".to_owned())?;
                    if relative_tick >= clip_length {
                        continue;
                    }
                    expanded_point_count = expanded_point_count
                        .checked_add(1)
                        .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                        .ok_or_else(|| {
                            format!(
                                "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                            )
                        })?;
                    points_by_lane
                        .entry((*target_channel_id, lane.parameter_id))
                        .or_default()
                        .push((
                            relative_tick,
                            repetition,
                            lane_index,
                            point_index,
                            point.value,
                            point.tension,
                        ));
                }
            }
        }
    }

    let mut automation_by_channel = BTreeMap::<u16, Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((channel_id, parameter_id), mut points) in points_by_lane {
        points.sort_by_key(|point| (point.0, point.1, point.2, point.3));
        let mut unique_points = Vec::<Vst3PlaylistAutomationPoint>::with_capacity(points.len());
        for (tick, _, _, _, value, tension) in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.tick == tick
            {
                previous.value = value;
                previous.tension = tension;
            } else {
                unique_points.push(Vst3PlaylistAutomationPoint {
                    tick,
                    value,
                    tension,
                });
            }
        }
        if !unique_points.is_empty() {
            automation_by_channel.entry(channel_id).or_default().push(
                Vst3PlaylistParameterAutomation {
                    parameter_id,
                    points: unique_points,
                },
            );
        }
    }
    Ok(automation_by_channel)
}

fn expand_automation_clip_parameter_automation_for_clip(
    automation_channel_id: u16,
    clip: &crate::PlaylistClip,
    ppq: u16,
    automation_clip_automation: &BTreeMap<(u16, u16), Vec<Vst3ParameterAutomation>>,
) -> Result<BTreeMap<u16, Vec<Vst3PlaylistParameterAutomation>>, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if clip.length_ticks == 0
        || clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Ok(BTreeMap::new());
    }
    let Some((source_start_tick, source_end_tick, explicit_source_window)) =
        playlist_automation_source_window(clip)
    else {
        return Ok(BTreeMap::new());
    };
    let mut expanded_point_count = 0usize;
    let mut automation_by_channel = BTreeMap::<u16, Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((linked_automation_channel_id, target_channel_id), lanes) in automation_clip_automation {
        if *linked_automation_channel_id != automation_channel_id {
            continue;
        }
        for lane in lanes {
            let mapped_points = playlist_automation_points_in_source_window(
                &lane.points,
                ppq,
                source_start_tick,
                source_end_tick,
                explicit_source_window,
            );
            let mut points = Vec::with_capacity(mapped_points.len());
            for point in mapped_points {
                expanded_point_count = expanded_point_count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                    .ok_or_else(|| {
                        format!(
                            "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                        )
                    })?;
                points.push(Vst3PlaylistAutomationPoint {
                    tick: point.tick,
                    value: point.value,
                    tension: 0.0,
                });
            }
            if !points.is_empty() {
                automation_by_channel
                    .entry(*target_channel_id)
                    .or_default()
                    .push(Vst3PlaylistParameterAutomation {
                        parameter_id: lane.parameter_id,
                        points,
                    });
            }
        }
    }
    Ok(automation_by_channel)
}

/// Expand one type-5 Automation channel curve into a Playlist Automation Clip's visible timeline.
/// Points are returned relative to the clip start and preserve the decoded tension approximation.
pub fn expand_automation_channel_curve_for_clip(
    automation_points: &[crate::AutomationPoint],
    clip: &crate::PlaylistClip,
    ppq: u16,
) -> Result<Vec<Vst3PlaylistAutomationPoint>, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if clip.length_ticks == 0
        || clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Ok(Vec::new());
    }
    let Some((source_start_tick, source_end_tick, explicit_source_window)) =
        playlist_automation_source_window(clip)
    else {
        return Ok(Vec::new());
    };
    let points = automation_points
        .iter()
        .map(|point| Vst3AutomationPoint {
            position_beats: point.position_beats(),
            value: point.value(),
            tension: point.tension(),
        })
        .collect::<Vec<_>>();
    if points.len() > MAX_VST3_AUTOMATION_POINTS {
        return Err(format!(
            "Playlist automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
        ));
    }
    Ok(playlist_automation_points_in_source_window(
        &points,
        ppq,
        source_start_tick,
        source_end_tick,
        explicit_source_window,
    ))
}

fn playlist_automation_source_window(clip: &crate::PlaylistClip) -> Option<(u64, u64, bool)> {
    let clip_length = u64::from(clip.length_ticks);
    match clip.source_window_ticks() {
        Some((0, 0)) | None => Some((0, clip_length, false)),
        Some((start, end)) if end.checked_sub(start) == Some(clip.length_ticks) => {
            Some((u64::from(start), u64::from(end), true))
        }
        Some(_) => None,
    }
}

fn playlist_automation_points_in_source_window(
    points: &[Vst3AutomationPoint],
    ppq: u16,
    source_start_tick: u64,
    source_end_tick: u64,
    explicit_source_window: bool,
) -> Vec<Vst3PlaylistAutomationPoint> {
    let mut source_points = points
        .iter()
        .filter_map(|point| {
            if !point.position_beats.is_finite()
                || point.position_beats < 0.0
                || !point.value.is_finite()
                || !(0.0..=1.0).contains(&point.value)
            {
                return None;
            }
            let point_tick = point.position_beats * f64::from(ppq);
            if !point_tick.is_finite() || point_tick >= u64::MAX as f64 {
                return None;
            }
            Some((point_tick.round() as u64, point.value, point.tension))
        })
        .collect::<Vec<_>>();
    source_points.sort_by_key(|(tick, _, _)| *tick);
    source_points.dedup_by(|current, previous| {
        if current.0 == previous.0 {
            previous.1 = current.1;
            previous.2 = current.2;
            true
        } else {
            false
        }
    });

    let boundary_value = if source_start_tick == 0
        || source_points
            .iter()
            .any(|(tick, _, _)| *tick == source_start_tick)
    {
        None
    } else {
        let previous = source_points
            .iter()
            .rev()
            .find(|(tick, _, _)| *tick < source_start_tick);
        let next = source_points
            .iter()
            .find(|(tick, _, _)| *tick > source_start_tick);
        previous.map(|(from_tick, from_value, _)| {
            if let Some((to_tick, to_value, to_tension)) = next {
                let fraction =
                    (source_start_tick - from_tick) as f64 / (*to_tick - from_tick) as f64;
                automation_segment_value_with_tension(*from_value, *to_value, fraction, *to_tension)
            } else {
                *from_value
            }
        })
    };

    let mut mapped_points = Vec::new();
    if let Some(value) = boundary_value {
        mapped_points.push(Vst3PlaylistAutomationPoint {
            tick: 0,
            value,
            tension: 0.0,
        });
    }
    mapped_points.extend(
        source_points
            .into_iter()
            .filter(|(tick, _, _)| *tick >= source_start_tick && *tick < source_end_tick)
            .map(|(tick, value, tension)| Vst3PlaylistAutomationPoint {
                tick: tick - source_start_tick,
                value,
                tension,
            }),
    );
    if explicit_source_window
        && let Some((value, tension)) =
            playlist_automation_value_at_tick(points, ppq, source_end_tick)
    {
        mapped_points.push(Vst3PlaylistAutomationPoint {
            tick: source_end_tick - source_start_tick,
            value,
            tension,
        });
    }
    mapped_points
}

fn playlist_automation_value_at_tick(
    points: &[Vst3AutomationPoint],
    ppq: u16,
    tick: u64,
) -> Option<(f64, f32)> {
    let mut source_points = points
        .iter()
        .filter_map(|point| {
            if !point.position_beats.is_finite()
                || point.position_beats < 0.0
                || !point.value.is_finite()
                || !(0.0..=1.0).contains(&point.value)
            {
                return None;
            }
            let point_tick = point.position_beats * f64::from(ppq);
            (point_tick.is_finite() && point_tick < u64::MAX as f64).then_some((
                point_tick.round() as u64,
                point.value,
                point.tension,
            ))
        })
        .collect::<Vec<_>>();
    source_points.sort_by_key(|(point_tick, _, _)| *point_tick);
    source_points.dedup_by(|current, previous| {
        if current.0 == previous.0 {
            previous.1 = current.1;
            previous.2 = current.2;
            true
        } else {
            false
        }
    });
    let previous = source_points
        .iter()
        .rev()
        .find(|(point_tick, _, _)| *point_tick <= tick)?;
    if previous.0 == tick {
        return Some((previous.1, previous.2));
    }
    source_points
        .iter()
        .find(|(point_tick, _, _)| *point_tick > tick)
        .map_or(
            Some((previous.1, previous.2)),
            |(next_tick, next_value, next_tension)| {
                let fraction = (tick - previous.0) as f64 / (*next_tick - previous.0) as f64;
                Some((
                    automation_segment_value_with_tension(
                        previous.1,
                        *next_value,
                        fraction,
                        *next_tension,
                    ),
                    *next_tension,
                ))
            },
        )
}

fn expand_playlist_mixer_effect_parameter_automation(
    document: &FlpDocument,
    options: PlaylistRenderOptions,
    pattern_clip_automation: &BTreeMap<(u16, usize, u16), Vec<Vst3PlaylistParameterAutomation>>,
    automation_clip_automation: &BTreeMap<(u16, usize, u16), Vec<Vst3ParameterAutomation>>,
) -> Result<BTreeMap<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>, String> {
    if pattern_clip_automation.is_empty() && automation_clip_automation.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == options.arrangement_id)
        .ok_or_else(|| format!("arrangement {} was not found", options.arrangement_id))?;
    let patterns = if pattern_clip_automation.is_empty() {
        Vec::new()
    } else {
        document.patterns().map_err(|error| error.to_string())?
    };
    let patterns_by_id = patterns
        .iter()
        .map(|pattern| (pattern.id, pattern))
        .collect::<BTreeMap<_, _>>();
    let disabled_track_ids = document
        .playlist_tracks()
        .into_iter()
        .filter(|track| track.enabled == Some(false))
        .map(|track| track.id)
        .collect::<BTreeSet<_>>();
    let clip_is_muted = |clip: &crate::PlaylistClip| {
        if clip.is_muted() {
            return true;
        }
        let track_id = clip.playlist_track_id();
        if let Some((first_track, last_track)) = options.soloed_playlist_track_range {
            !track_id.is_some_and(|track_id| (first_track..=last_track).contains(&track_id))
        } else {
            track_id.is_some_and(|track_id| disabled_track_ids.contains(&track_id))
        }
    };
    let mut points_by_lane =
        BTreeMap::<(usize, u16, u32), Vec<(u64, usize, usize, usize, f64, f32)>>::new();
    let mut expanded_point_count = 0usize;

    for (clip_index, clip) in arrangement.clips.iter().enumerate() {
        if clip_is_muted(clip)
            || clip.length_ticks == 0
            || clip
                .scale
                .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            continue;
        }
        let clip_automation = match clip.target() {
            PlaylistClipTarget::Pattern { id: pattern_id } => {
                let Some(pattern) = patterns_by_id.get(&pattern_id).copied() else {
                    continue;
                };
                expand_pattern_mixer_effect_parameter_automation_for_clip(
                    pattern_id,
                    pattern,
                    clip,
                    ppq,
                    document.metadata().time_signature().unwrap_or((4, 4)),
                    pattern_clip_automation,
                )?
            }
            PlaylistClipTarget::Channel {
                id: automation_channel_id,
            } => expand_mixer_effect_automation_clip_parameter_automation_for_clip(
                automation_channel_id,
                clip,
                ppq,
                automation_clip_automation,
            )?,
        };
        for ((insert_ordinal, slot_index), lanes) in clip_automation {
            for (lane_index, lane) in lanes.into_iter().enumerate() {
                for (point_index, point) in lane.points.into_iter().enumerate() {
                    let absolute_tick = u64::from(clip.position_ticks)
                        .checked_add(point.tick)
                        .ok_or_else(|| "Playlist automation point position overflow".to_owned())?;
                    expanded_point_count = expanded_point_count
                        .checked_add(1)
                        .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                        .ok_or_else(|| {
                            format!(
                                "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                            )
                        })?;
                    points_by_lane
                        .entry((insert_ordinal, slot_index, lane.parameter_id))
                        .or_default()
                        .push((
                            absolute_tick,
                            clip_index,
                            lane_index,
                            point_index,
                            point.value,
                            point.tension,
                        ));
                }
            }
        }
    }

    let mut automation_by_effect =
        BTreeMap::<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((insert_ordinal, slot_index, parameter_id), mut points) in points_by_lane {
        points.sort_by_key(|point| (point.0, point.1, point.2, point.3));
        let mut unique_points = Vec::<Vst3PlaylistAutomationPoint>::with_capacity(points.len());
        for (tick, _, _, _, value, tension) in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.tick == tick
            {
                previous.value = value;
                previous.tension = tension;
            } else {
                unique_points.push(Vst3PlaylistAutomationPoint {
                    tick,
                    value,
                    tension,
                });
            }
        }
        if !unique_points.is_empty() {
            automation_by_effect
                .entry((insert_ordinal, slot_index))
                .or_default()
                .push(Vst3PlaylistParameterAutomation {
                    parameter_id,
                    points: unique_points,
                });
        }
    }
    Ok(automation_by_effect)
}

fn expand_pattern_mixer_effect_parameter_automation_for_clip(
    pattern_id: u16,
    pattern: &crate::Pattern,
    clip: &crate::PlaylistClip,
    ppq: u16,
    default_time_signature: (u8, u8),
    pattern_automation: &BTreeMap<(u16, usize, u16), Vec<Vst3PlaylistParameterAutomation>>,
) -> Result<BTreeMap<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if clip.length_ticks == 0
        || clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Ok(BTreeMap::new());
    }
    let clip_length = u64::from(clip.length_ticks);
    let inferred_note_length = pattern
        .notes
        .iter()
        .filter(|note| note.length > 0)
        .try_fold(0u64, |end, note| {
            let note_end = u64::from(note.position)
                .checked_add(u64::from(note.length))
                .ok_or_else(|| "Pattern note end position overflow".to_owned())?;
            Ok::<_, String>(end.max(note_end))
        })?;
    let signature = pattern
        .time_markers
        .iter()
        .filter(|marker| marker.is_signature() && marker.position_ticks() == 0)
        .find_map(|marker| Some((marker.numerator()?, marker.denominator()?)))
        .unwrap_or(default_time_signature);
    let bar_length = (u64::from(ppq)
        .saturating_mul(u64::from(signature.0.max(1)))
        .saturating_mul(4)
        / u64::from(signature.1.max(1)))
    .max(1);
    let loop_length = match pattern
        .length_ticks
        .map(u64::from)
        .filter(|length| *length > 0)
    {
        Some(explicit_length) => explicit_length.max(inferred_note_length),
        None => inferred_note_length.max(bar_length),
    };
    if loop_length == 0 {
        return Ok(BTreeMap::new());
    }
    let repetitions = clip_length.div_ceil(loop_length);
    let mut points_by_lane =
        BTreeMap::<(usize, u16, u32), Vec<(u64, u64, usize, usize, f64, f32)>>::new();
    let mut expanded_point_count = 0usize;
    for ((linked_pattern_id, insert_ordinal, slot_index), lanes) in pattern_automation {
        if *linked_pattern_id != pattern_id {
            continue;
        }
        for (lane_index, lane) in lanes.iter().enumerate() {
            for repetition in 0..repetitions {
                let repeat_start = repetition
                    .checked_mul(loop_length)
                    .ok_or_else(|| "Pattern automation repeat position overflow".to_owned())?;
                for (point_index, point) in lane.points.iter().enumerate() {
                    if point.tick >= loop_length
                        || !point.value.is_finite()
                        || !(0.0..=1.0).contains(&point.value)
                    {
                        continue;
                    }
                    let relative_tick = repeat_start
                        .checked_add(point.tick)
                        .ok_or_else(|| "Pattern automation point position overflow".to_owned())?;
                    if relative_tick >= clip_length {
                        continue;
                    }
                    expanded_point_count = expanded_point_count
                        .checked_add(1)
                        .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                        .ok_or_else(|| {
                            format!(
                                "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                            )
                        })?;
                    points_by_lane
                        .entry((*insert_ordinal, *slot_index, lane.parameter_id))
                        .or_default()
                        .push((
                            relative_tick,
                            repetition,
                            lane_index,
                            point_index,
                            point.value,
                            point.tension,
                        ));
                }
            }
        }
    }

    let mut automation_by_effect =
        BTreeMap::<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((insert_ordinal, slot_index, parameter_id), mut points) in points_by_lane {
        points.sort_by_key(|point| (point.0, point.1, point.2, point.3));
        let mut unique_points = Vec::<Vst3PlaylistAutomationPoint>::with_capacity(points.len());
        for (tick, _, _, _, value, tension) in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.tick == tick
            {
                previous.value = value;
                previous.tension = tension;
            } else {
                unique_points.push(Vst3PlaylistAutomationPoint {
                    tick,
                    value,
                    tension,
                });
            }
        }
        if !unique_points.is_empty() {
            automation_by_effect
                .entry((insert_ordinal, slot_index))
                .or_default()
                .push(Vst3PlaylistParameterAutomation {
                    parameter_id,
                    points: unique_points,
                });
        }
    }
    Ok(automation_by_effect)
}

fn expand_mixer_effect_automation_clip_parameter_automation_for_clip(
    automation_channel_id: u16,
    clip: &crate::PlaylistClip,
    ppq: u16,
    automation_clip_automation: &BTreeMap<(u16, usize, u16), Vec<Vst3ParameterAutomation>>,
) -> Result<BTreeMap<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>, String> {
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if clip.length_ticks == 0
        || clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
    {
        return Ok(BTreeMap::new());
    }
    let Some((source_start_tick, source_end_tick, explicit_source_window)) =
        playlist_automation_source_window(clip)
    else {
        return Ok(BTreeMap::new());
    };
    let mut expanded_point_count = 0usize;
    let mut automation_by_effect =
        BTreeMap::<(usize, u16), Vec<Vst3PlaylistParameterAutomation>>::new();
    for ((linked_automation_channel_id, insert_ordinal, slot_index), lanes) in
        automation_clip_automation
    {
        if *linked_automation_channel_id != automation_channel_id {
            continue;
        }
        for lane in lanes {
            let mapped_points = playlist_automation_points_in_source_window(
                &lane.points,
                ppq,
                source_start_tick,
                source_end_tick,
                explicit_source_window,
            );
            let mut points = Vec::with_capacity(mapped_points.len());
            for point in mapped_points {
                expanded_point_count = expanded_point_count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_VST3_AUTOMATION_POINTS)
                    .ok_or_else(|| {
                        format!(
                            "Playlist VST3 automation exceeds {MAX_VST3_AUTOMATION_POINTS} points"
                        )
                    })?;
                points.push(Vst3PlaylistAutomationPoint {
                    tick: point.tick,
                    value: point.value,
                    tension: 0.0,
                });
            }
            if !points.is_empty() {
                automation_by_effect
                    .entry((*insert_ordinal, *slot_index))
                    .or_default()
                    .push(Vst3PlaylistParameterAutomation {
                        parameter_id: lane.parameter_id,
                        points,
                    });
            }
        }
    }
    Ok(automation_by_effect)
}

fn prepare_playlist_parameter_automation(
    automation: &[Vst3PlaylistParameterAutomation],
    tempo_map: &PlaylistTempoMap,
    sample_rate: u32,
    total_frames: u64,
) -> Result<Vec<PreparedParameterAutomation>, String> {
    if sample_rate == 0 {
        return Err("VST3 automation sample rate must be positive".to_owned());
    }
    let mut point_count = 0usize;
    let mut parameter_ids = BTreeSet::new();
    let mut prepared = Vec::with_capacity(automation.len());
    for lane in automation {
        if lane.points.is_empty() {
            continue;
        }
        if !parameter_ids.insert(lane.parameter_id) {
            return Err(format!(
                "VST3 parameter {} has more than one Playlist automation lane",
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
            if !point.value.is_finite() || !(0.0..=1.0).contains(&point.value) {
                return Err(
                    "VST3 automation values must be finite and normalized to 0..=1".to_owned(),
                );
            }
            let frame = tempo_map.frame_at_tick(point.tick, sample_rate)?;
            if frame < total_frames {
                points.push(ScheduledParameterPoint {
                    frame,
                    value: point.value,
                    tension: point.tension,
                    performance_track_id: None,
                });
            }
        }
        points.sort_by_key(|point| point.frame);
        let mut unique_points: Vec<ScheduledParameterPoint> = Vec::with_capacity(points.len());
        for point in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.frame == point.frame
            {
                previous.value = point.value;
                previous.tension = point.tension;
            } else {
                unique_points.push(point);
            }
        }
        if !unique_points.is_empty() {
            prepared.push(PreparedParameterAutomation {
                parameter_id: lane.parameter_id,
                points: unique_points,
            });
        }
    }
    Ok(prepared)
}

fn prune_parameter_automation_before(automation: &mut [PreparedParameterAutomation], frame: u64) {
    for lane in automation {
        let next_index = lane.points.partition_point(|point| point.frame <= frame);
        let keep_index = next_index.saturating_sub(1);
        if keep_index > 0 {
            lane.points.drain(..keep_index);
        }
    }
}

fn stop_performance_parameter_automation(
    automation: &mut Vec<PreparedParameterAutomation>,
    track_id: u32,
    stop_frame: u64,
) {
    for lane in automation.iter_mut() {
        lane.points.retain(|point| {
            point.performance_track_id != Some(track_id) || point.frame < stop_frame
        });
    }
    automation.retain(|lane| !lane.points.is_empty());
}

fn prepare_playlist_pattern_render(
    plugin: &Plugin,
    notes: &[PlaylistMidiNote],
    tempo_map: &PlaylistTempoMap,
    total_frames: u64,
    pitch_offset_cents: i16,
    note_pitch_offset_semitones: i16,
    allow_empty_notes: bool,
) -> Result<PreparedPatternRender, String> {
    if notes.is_empty() && !allow_empty_notes {
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
    let output_bus_channels = plugin_output_bus_channel_counts(plugin)?;
    let output_channels = output_bus_channels.iter().sum();
    if !(-12..=12).contains(&note_pitch_offset_semitones) {
        return Err("VST3 note pitch offset must be between -12 and 12 semitones".to_owned());
    }
    let (mut events, note_count) =
        scheduled_playlist_midi_events_with_tempo_map(notes, tempo_map, sample_rate)?;
    transpose_scheduled_note_events(&mut events, note_pitch_offset_semitones);
    let (supports_note_pan, supports_note_volume, supports_note_brightness, mod_y_expression) =
        plugin_note_expression_support(plugin);
    let last_event_frame = events.iter().map(|event| event.frame).max().unwrap_or(0);
    Ok(PreparedPatternRender {
        events,
        parameter_automation: Vec::new(),
        pitch_offset_cents,
        supports_note_pan,
        supports_note_volume,
        supports_note_brightness,
        mod_y_expression,
        note_count,
        sample_rate,
        sample_rate_u32,
        output_channels,
        output_bus_channels,
        output_mixer_offsets: None,
        block_size,
        total_frames: total_frames.max(last_event_frame.saturating_add(1)),
    })
}

#[cfg(test)]
fn scheduled_playlist_midi_events(
    notes: &[PlaylistMidiNote],
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: f64,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    let tempo_map = PlaylistTempoMap::constant(ppq, tempo_bpm)?;
    scheduled_playlist_midi_events_with_tempo_map(notes, &tempo_map, sample_rate)
}

fn scheduled_playlist_midi_events_with_tempo_map(
    notes: &[PlaylistMidiNote],
    tempo_map: &PlaylistTempoMap,
    sample_rate: f64,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
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
        let start_frame = tempo_map
            .precise_frame_at_tick(note.start_tick, sample_rate)?
            .round();
        let end_frame = tempo_map
            .precise_frame_at_tick(
                note.stop_tick.max(note.start_tick.saturating_add(1)),
                sample_rate,
            )?
            .round();
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
        let note_id = next_vst3_note_expression_id();
        events.push(ScheduledMidiEvent {
            frame: start_frame,
            priority: 1,
            event: MidiEvent::NoteOn {
                channel,
                note: note.key as u8,
                velocity: note.velocity,
            },
            fine_pitch_offset_cents: note.fine_pitch_offset_cents,
            note_id,
            note_pan: note.pan,
            note_mod_x: note.mod_x,
            note_mod_y: note.mod_y,
            note_release: note.release,
            layer_gain: note.layer_gain,
            layer_pan_raw: note.layer_pan_raw,
            performance_track_id: None,
        });
        events.push(ScheduledMidiEvent {
            frame: end_frame,
            priority: 0,
            event: MidiEvent::NoteOff {
                channel,
                note: note.key as u8,
                velocity: 0,
            },
            fine_pitch_offset_cents: note.fine_pitch_offset_cents,
            note_id,
            note_pan: note.pan,
            note_mod_x: note.mod_x,
            note_mod_y: note.mod_y,
            note_release: note.release,
            layer_gain: note.layer_gain,
            layer_pan_raw: note.layer_pan_raw,
            performance_track_id: None,
        });
        note_count += 1;
    }
    events.sort_by_key(|event| (event.frame, event.priority));
    Ok((events, note_count))
}

fn prepare_pattern_render_with_automation(
    plugin: &Plugin,
    notes: &[PatternNote],
    options: Vst3PatternRenderOptions,
    parameter_automation: &[Vst3ParameterAutomation],
) -> Result<PreparedPatternRender, String> {
    prepare_pattern_render_with_layer_adjustments(
        plugin,
        notes,
        options,
        parameter_automation,
        None,
    )
}

fn prepare_pattern_render_with_layer_adjustments(
    plugin: &Plugin,
    notes: &[PatternNote],
    options: Vst3PatternRenderOptions,
    parameter_automation: &[Vst3ParameterAutomation],
    layer_adjustments: Option<&[(f32, i32)]>,
) -> Result<PreparedPatternRender, String> {
    let Vst3PatternRenderOptions {
        channel_id,
        ppq,
        tempo_bpm,
        tail_seconds,
        global_swing_mix_raw,
        channel_swing_mix_raw,
        pitch_offset_cents,
        note_pitch_offset_semitones,
    } = options;
    if !(-1300..=1300).contains(&pitch_offset_cents) {
        return Err("VST3 pitch offset must be between -1300 and 1300 cents".to_owned());
    }
    if !(-12..=12).contains(&note_pitch_offset_semitones) {
        return Err("VST3 note pitch offset must be between -12 and 12 semitones".to_owned());
    }
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
    let output_bus_channels = plugin_output_bus_channel_counts(plugin)?;
    let output_channels = output_bus_channels.iter().sum();
    let parameter_automation =
        prepare_parameter_automation(parameter_automation, sample_rate, tempo_bpm)?;

    let (mut events, note_count) = scheduled_pattern_events_with_layer_adjustments(
        notes,
        PatternNoteScheduleOptions {
            channel_id,
            ppq,
            tempo_bpm,
            sample_rate,
            global_swing_mix_raw,
            channel_swing_mix_raw,
        },
        layer_adjustments,
    )?;
    transpose_scheduled_note_events(&mut events, note_pitch_offset_semitones);
    let (supports_note_pan, supports_note_volume, supports_note_brightness, mod_y_expression) =
        plugin_note_expression_support(plugin);
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
        pitch_offset_cents,
        supports_note_pan,
        supports_note_volume,
        supports_note_brightness,
        mod_y_expression,
        note_count,
        sample_rate,
        sample_rate_u32,
        output_channels,
        output_bus_channels,
        output_mixer_offsets: None,
        block_size,
        total_frames,
    })
}

fn plugin_note_expression_support(
    plugin: &Plugin,
) -> (bool, bool, bool, Option<Vst3ModYExpression>) {
    let Ok(expressions) = plugin.note_expressions() else {
        return (false, false, false, None);
    };
    (
        note_expressions_support_pan(&expressions),
        note_expressions_support_volume(&expressions),
        note_expressions_support_brightness(&expressions),
        note_expressions_support_mod_y(&expressions),
    )
}

fn plugin_output_bus_channel_counts(plugin: &Plugin) -> Result<Vec<usize>, String> {
    let output_channels = plugin.output_channel_count();
    if output_channels == 0 || output_channels > 64 {
        return Err(format!(
            "VST3 instrument reports {output_channels} active output channels; supported range is 1..=64"
        ));
    }

    let layout = plugin
        .audio_bus_layout()
        .map_err(|error| format!("could not read VST3 output bus layout: {error}"))?;
    let bus_channels = layout
        .outputs
        .iter()
        .filter(|bus| bus.active)
        .map(|bus| bus.channel_count)
        .collect::<Vec<_>>();
    let layout_channels = bus_channels.iter().try_fold(0usize, |total, channels| {
        total
            .checked_add(*channels)
            .ok_or_else(|| "VST3 output bus channel count overflow".to_owned())
    })?;
    if bus_channels.is_empty() || layout_channels != output_channels {
        return Err(format!(
            "VST3 output layout reports {layout_channels} active channels across {} buses, but the host reports {output_channels} channels",
            bus_channels.len()
        ));
    }
    if bus_channels
        .iter()
        .any(|channels| !matches!(channels, 1 | 2))
    {
        return Err(format!(
            "VST3 stereo playback supports active mono or stereo output buses; the layout is {:?}",
            bus_channels
        ));
    }
    Ok(bus_channels)
}

/// Reads the output-connection Mixer offsets from FL's VST wrapper records.
///
/// Wrapper field 30 stores input/output counts and field 32 stores output-connection
/// records. The observed marker-12 VST3 record is three little-endian words, with the
/// signed Mixer offset in the middle word. Unknown record words and unsupported lengths
/// are left uninterpreted so callers can retain the existing single-route behavior.
fn flp_vst3_output_mixer_offsets(payload: &[u8]) -> Option<Vec<i32>> {
    if u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) != 12 {
        return None;
    }

    let mut cursor = 4usize;
    let mut io_data = None;
    let mut output_data = None;
    while cursor < payload.len() {
        let id_end = cursor.checked_add(4)?;
        let length_end = cursor.checked_add(12)?;
        let id = u32::from_le_bytes(payload.get(cursor..id_end)?.try_into().ok()?);
        let length = usize::try_from(u64::from_le_bytes(
            payload.get(id_end..length_end)?.try_into().ok()?,
        ))
        .ok()?;
        let end = length_end.checked_add(length)?;
        let data = payload.get(length_end..end)?;
        match id {
            30 if io_data.is_none() => io_data = Some(data),
            30 => return None,
            32 if output_data.is_none() => output_data = Some(data),
            32 => return None,
            _ => {}
        }
        cursor = end;
    }

    let io_data = io_data?;
    let output_data = output_data?;
    let output_count =
        usize::try_from(u32::from_le_bytes(io_data.get(4..8)?.try_into().ok()?)).ok()?;
    if output_count > 64 || output_data.len() != output_count.checked_mul(12)? {
        return None;
    }

    output_data
        .as_chunks::<12>()
        .0
        .iter()
        .map(|record| Some(i32::from_le_bytes(record[4..8].try_into().ok()?)))
        .collect()
}

fn flp_vst3_output_mixer_offsets_by_channel(document: &FlpDocument) -> HashMap<u16, Vec<i32>> {
    document
        .channel_plugin_states()
        .into_iter()
        .filter_map(|state| {
            let metadata = state.vst_metadata()?;
            if metadata.format_marker() != 12 || metadata.fourcc().is_some() {
                return None;
            }
            Some((
                state.channel_id(),
                flp_vst3_output_mixer_offsets(state.data_payload())?,
            ))
        })
        .collect()
}

fn flp_vst3_output_mixer_offsets_for_loaded_channels(
    host: &Vst3HostRuntime,
    document: &FlpDocument,
    channel_instances: &BTreeMap<u16, u64>,
) -> HashMap<u16, Vec<i32>> {
    let mut offsets_by_channel = flp_vst3_output_mixer_offsets_by_channel(document);
    offsets_by_channel.retain(|channel_id, offsets| {
        let Some(instance_id) = channel_instances.get(channel_id) else {
            return false;
        };
        let Ok(plugin) = host.plugin(*instance_id) else {
            return false;
        };
        let Ok(plugin) = plugin.lock() else {
            return false;
        };
        plugin_output_bus_channel_counts(&plugin)
            .is_ok_and(|bus_channels| bus_channels.len() == offsets.len())
    });
    offsets_by_channel
}

fn apply_vst3_output_mixer_offsets(
    render: &mut PreparedPatternRender,
    offsets_by_channel: &HashMap<u16, Vec<i32>>,
    channel_id: u16,
) {
    if let Some(offsets) = offsets_by_channel.get(&channel_id)
        && offsets.len() == render.output_bus_channels.len()
    {
        render.output_mixer_offsets = Some(offsets.clone());
    }
}

fn vst3_output_mixer_route(base_route: Option<i8>, output_offset: Option<i32>) -> Option<i8> {
    let Some(output_offset) = output_offset else {
        return base_route;
    };
    let base_track = i32::from(base_route.unwrap_or(0));
    let Some(routed_track) = base_track.checked_add(output_offset) else {
        return base_route;
    };
    if !(-1..=127).contains(&routed_track) {
        return base_route;
    }
    i8::try_from(routed_track).ok().or(base_route)
}

fn vst3_channel_has_audible_output_route(
    routing: &MixerRouteAudibility,
    base_route: Option<i8>,
    output_offsets: Option<&[i32]>,
    live: bool,
) -> bool {
    let Some(output_offsets) = output_offsets.filter(|offsets| !offsets.is_empty()) else {
        return if live {
            routing.allows_live_source_channel(base_route)
        } else {
            routing.allows_channel(base_route)
        };
    };
    output_offsets.iter().copied().any(|offset| {
        let route = vst3_output_mixer_route(base_route, Some(offset));
        if live {
            routing.allows_live_source_channel(route)
        } else {
            routing.allows_channel(route)
        }
    })
}

fn prepare_playlist_plugin_output_buses(
    render: &PreparedPatternRender,
    output_sample_rate: u32,
    base_route: Option<i8>,
    gain: f32,
    pan: f32,
    pan_law: crate::sample_render::PanLaw,
    routing: &MixerRouteAudibility,
) -> Result<Vec<PlaylistPluginOutputBus>, String> {
    render
        .output_bus_channels
        .iter()
        .enumerate()
        .map(|(bus_index, channel_count)| {
            let mixer_route = vst3_output_mixer_route(
                base_route,
                render
                    .output_mixer_offsets
                    .as_ref()
                    .and_then(|offsets| offsets.get(bus_index).copied()),
            );
            let route_plan = routing.source_route_plan_for_channel(mixer_route);
            let live_mixer_source =
                routing
                    .live_source_route_plan_for_channel(mixer_route)
                    .map(|route_plan| PlaylistLiveMixerSource {
                        control: LiveMixerSourceControl::new(route_plan, output_sample_rate),
                        targets: None,
                    });
            let (left_pan_gain, right_pan_gain) =
                channel_pan_gains(pan, *channel_count == 1, pan_law);
            Ok(PlaylistPluginOutputBus {
                resampler: StereoStreamResampler::new(render.sample_rate_u32, output_sample_rate)?,
                pending_output: VecDeque::new(),
                delay_line: StereoFrameDelay::default(),
                gain,
                left_pan_gain,
                right_pan_gain,
                mixer_transform: route_plan.output_transform,
                mixer_meter_transforms: route_plan.insert_meter_transforms,
                mixer_effect_input_transforms: route_plan.insert_effect_input_transforms,
                live_mixer_source,
            })
        })
        .collect()
}

fn note_expressions_support_pan(expressions: &[NoteExpressionInfo]) -> bool {
    expressions
        .iter()
        .any(|expression| expression.kind == NoteExpressionType::Pan)
}

fn note_expressions_support_volume(expressions: &[NoteExpressionInfo]) -> bool {
    expressions
        .iter()
        .any(|expression| expression.kind == NoteExpressionType::Volume)
}

fn note_expressions_support_brightness(expressions: &[NoteExpressionInfo]) -> bool {
    expressions
        .iter()
        .any(|expression| expression.kind == NoteExpressionType::Brightness)
}

fn note_expressions_support_mod_y(
    expressions: &[NoteExpressionInfo],
) -> Option<Vst3ModYExpression> {
    expressions
        .iter()
        .filter_map(|expression| {
            if !matches!(expression.kind, NoteExpressionType::Custom(_))
                || !expression.min.is_finite()
                || !expression.max.is_finite()
                || !(0.0..=1.0).contains(&expression.min)
                || !(0.0..=1.0).contains(&expression.max)
                || expression.min > expression.max
            {
                return None;
            }
            let title = format!(
                "{} {}",
                expression.title.to_ascii_lowercase(),
                expression.short_title.to_ascii_lowercase()
            );
            let priority = if title.contains("mod y") || title.contains("mody") {
                0
            } else if title.contains("resonance") {
                1
            } else if title.contains("timbre") {
                2
            } else {
                return None;
            };
            Some((
                priority,
                Vst3ModYExpression {
                    kind: expression.kind,
                    min: expression.min,
                    max: expression.max,
                },
            ))
        })
        .min_by_key(|(priority, _)| *priority)
        .map(|(_, expression)| expression)
}

fn transpose_scheduled_note_events(events: &mut [ScheduledMidiEvent], semitones: i16) {
    if semitones == 0 {
        return;
    }
    for scheduled in events {
        let note = match &mut scheduled.event {
            MidiEvent::NoteOn { note, .. } | MidiEvent::NoteOff { note, .. } => note,
            _ => continue,
        };
        *note = (i16::from(*note) + semitones).clamp(0, 127) as u8;
    }
}

fn next_vst3_note_expression_id() -> i32 {
    let offset = NEXT_VST3_NOTE_EXPRESSION_ID.fetch_add(1, Ordering::Relaxed)
        % VST3_NOTE_EXPRESSION_ID_COUNT;
    (VST3_NOTE_EXPRESSION_ID_START + offset) as i32
}

fn vst3_note_pan_value(note_pan: u8) -> f64 {
    f64::from(note_pan.min(128)) / 128.0
}

fn vst3_note_pan_with_layer_offset(note_pan: u8, layer_pan_raw: i32) -> f64 {
    (vst3_note_pan_value(note_pan) + f64::from(layer_pan_raw.clamp(-6_400, 6_400)) / 12_800.0)
        .clamp(0.0, 1.0)
}

fn vst3_note_volume_value(layer_gain: f32) -> f64 {
    // VST3's standard Volume note-expression range is 0..1, with 0.25 at unity gain.
    (f64::from(layer_gain.max(0.0)) / 4.0).clamp(0.0, 1.0)
}

fn vst3_note_brightness_value(note_mod_x: u8) -> f64 {
    f64::from(note_mod_x) / 255.0
}

fn vst3_note_mod_y_value(note_mod_y: u8, expression: Vst3ModYExpression) -> f64 {
    let progress = f64::from(note_mod_y) / 255.0;
    expression.min + (expression.max - expression.min) * progress
}

fn vst3_note_release_value(note_release: u8) -> f32 {
    f32::from(note_release.min(128)) / 128.0
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
                tension: point.tension,
                performance_track_id: None,
            });
        }
        points.sort_by_key(|point| point.frame);
        let mut unique_points: Vec<ScheduledParameterPoint> = Vec::with_capacity(points.len());
        for point in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.frame == point.frame
            {
                previous.value = point.value;
                previous.tension = point.tension;
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
    Some(automation_segment_value_with_tension(
        from.value, to.value, fraction, to.tension,
    ))
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
    set_parameter_automation_for_block_at_offset(
        plugin,
        automation,
        block_start_frame,
        frame_count,
        0,
    )
}

fn set_parameter_automation_for_block_at_offset(
    plugin: &mut Plugin,
    automation: &[PreparedParameterAutomation],
    block_start_frame: u64,
    frame_count: usize,
    output_block_offset: usize,
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
                let output_offset = usize::try_from(offset)
                    .ok()
                    .and_then(|offset| offset.checked_add(output_block_offset))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or_else(|| "VST3 automation offset exceeds the host range".to_owned())?;
                plugin
                    .set_parameter_at(lane.parameter_id, value, output_offset)
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

fn looped_parameter_automation_segments(
    block_start_frame: u64,
    frame_count: usize,
    loop_frames: u64,
) -> Result<Vec<(u64, usize, usize)>, String> {
    if loop_frames == 0 {
        return Err("VST3 Pattern automation loop length must be greater than zero".to_owned());
    }
    let mut segments = Vec::new();
    let mut block_offset = 0usize;
    while block_offset < frame_count {
        let absolute_frame = block_start_frame
            .checked_add(
                u64::try_from(block_offset)
                    .map_err(|_| "VST3 automation block offset overflow".to_owned())?,
            )
            .ok_or_else(|| "VST3 automation position overflow".to_owned())?;
        let pattern_frame = absolute_frame % loop_frames;
        let frames_to_loop_end = loop_frames - pattern_frame;
        let remaining = frame_count - block_offset;
        let segment_count = usize::try_from(
            frames_to_loop_end.min(
                u64::try_from(remaining)
                    .map_err(|_| "VST3 automation block size overflow".to_owned())?,
            ),
        )
        .map_err(|_| "VST3 automation segment size overflow".to_owned())?;
        if segment_count == 0 {
            return Err("VST3 Pattern automation produced an empty loop segment".to_owned());
        }
        segments.push((pattern_frame, block_offset, segment_count));
        block_offset = block_offset
            .checked_add(segment_count)
            .ok_or_else(|| "VST3 automation block offset overflow".to_owned())?;
    }
    Ok(segments)
}

fn looped_parameter_automation_segments_at_range(
    block_start_frame: u64,
    frame_count: usize,
    loop_start_frame: u64,
    loop_frames: u64,
) -> Result<Vec<(u64, usize, usize)>, String> {
    if loop_start_frame == 0 {
        return looped_parameter_automation_segments(block_start_frame, frame_count, loop_frames);
    }
    if loop_frames == 0 {
        return Err("VST3 Pattern automation loop length must be greater than zero".to_owned());
    }
    let loop_end_frame = loop_start_frame
        .checked_add(loop_frames)
        .ok_or_else(|| "VST3 automation loop range overflow".to_owned())?;
    let mut segments = Vec::new();
    let mut block_offset = 0usize;
    while block_offset < frame_count {
        let absolute_frame = block_start_frame
            .checked_add(
                u64::try_from(block_offset)
                    .map_err(|_| "VST3 automation block offset overflow".to_owned())?,
            )
            .ok_or_else(|| "VST3 automation position overflow".to_owned())?;
        let (pattern_frame, frames_to_loop_end) = if absolute_frame < loop_end_frame {
            (absolute_frame, loop_end_frame - absolute_frame)
        } else {
            let pattern_frame = loop_start_frame + (absolute_frame - loop_end_frame) % loop_frames;
            (pattern_frame, loop_end_frame - pattern_frame)
        };
        let remaining = frame_count - block_offset;
        let segment_count = usize::try_from(
            frames_to_loop_end.min(
                u64::try_from(remaining)
                    .map_err(|_| "VST3 automation block size overflow".to_owned())?,
            ),
        )
        .map_err(|_| "VST3 automation segment size overflow".to_owned())?;
        if segment_count == 0 {
            return Err("VST3 Pattern automation produced an empty loop segment".to_owned());
        }
        segments.push((pattern_frame, block_offset, segment_count));
        block_offset = block_offset
            .checked_add(segment_count)
            .ok_or_else(|| "VST3 automation block offset overflow".to_owned())?;
    }
    Ok(segments)
}

fn set_looped_parameter_automation_for_block(
    plugin: &mut Plugin,
    automation: &[PreparedParameterAutomation],
    block_start_frame: u64,
    frame_count: usize,
    loop_frames: u64,
) -> Result<(), String> {
    set_looped_parameter_automation_for_block_at_range(
        plugin,
        automation,
        block_start_frame,
        frame_count,
        0,
        loop_frames,
    )
}

fn set_looped_parameter_automation_for_block_at_range(
    plugin: &mut Plugin,
    automation: &[PreparedParameterAutomation],
    block_start_frame: u64,
    frame_count: usize,
    loop_start_frame: u64,
    loop_frames: u64,
) -> Result<(), String> {
    for (pattern_frame, output_offset, segment_count) in
        looped_parameter_automation_segments_at_range(
            block_start_frame,
            frame_count,
            loop_start_frame,
            loop_frames,
        )?
    {
        set_parameter_automation_for_block_at_offset(
            plugin,
            automation,
            pattern_frame,
            segment_count,
            output_offset,
        )?;
    }
    Ok(())
}

fn validate_looped_parameter_automation_block_density(
    automation: &[PreparedParameterAutomation],
    block_size: usize,
    loop_frames: u64,
) -> Result<(), String> {
    if automation.is_empty() || block_size == 0 {
        return Ok(());
    }
    if loop_frames == 0 {
        return Err("VST3 Pattern automation loop length must be greater than zero".to_owned());
    }
    let block_frames =
        u64::try_from(block_size).map_err(|_| "VST3 automation block size overflow".to_owned())?;
    let maximum_segments = if block_frames <= 1 {
        1
    } else {
        (block_frames - 2) / loop_frames + 2
    };
    let active_lanes = u64::try_from(automation.len())
        .map_err(|_| "VST3 automation lane count overflow".to_owned())?;
    let point_count = automation.iter().try_fold(0u64, |count, lane| {
        count
            .checked_add(
                u64::try_from(lane.points.len())
                    .map_err(|_| "VST3 automation point count overflow".to_owned())?,
            )
            .ok_or_else(|| "VST3 automation point count overflow".to_owned())
    })?;
    let maximum_changes = active_lanes
        .checked_add(point_count)
        .and_then(|count| count.checked_mul(maximum_segments))
        .ok_or_else(|| "VST3 automation block change count overflow".to_owned())?;
    if maximum_changes > MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK as u64 {
        return Err(format!(
            "VST3 Pattern automation may schedule {maximum_changes} parameter changes in one processing block; the limit is {MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK}"
        ));
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
                send_scheduled_midi_event(
                    plugin,
                    event.event,
                    offset,
                    render.pitch_offset_cents,
                    event.fine_pitch_offset_cents,
                    ScheduledVst3Note {
                        note_id: event.note_id,
                        note_pan: event.note_pan,
                        note_mod_x: event.note_mod_x,
                        note_mod_y: event.note_mod_y,
                        note_release: event.note_release,
                        supports_pan: render.supports_note_pan,
                        supports_volume: render.supports_note_volume,
                        supports_brightness: render.supports_note_brightness,
                        mod_y_expression: render.mod_y_expression,
                        layer_gain: event.layer_gain,
                        layer_pan_raw: event.layer_pan_raw,
                    },
                )?;
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

fn stream_live_midi_input(
    plugin: Arc<Mutex<Plugin>>,
    receiver: Receiver<MidiEvent>,
    writer: &StreamingAudioWriter,
    output_sample_rate: u32,
    pitch_offset_cents: i16,
    note_pitch_offset_semitones: i16,
    cancelled: Arc<AtomicBool>,
) -> Result<(), String> {
    crate::audio::enable_denormal_protection();
    let (sample_rate, block_size, output_bus_channels) = {
        let plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        (
            plugin.sample_rate(),
            plugin.block_size(),
            plugin_output_bus_channel_counts(&plugin)?,
        )
    };
    if !sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&sample_rate) {
        return Err("VST3 host sample rate is invalid".to_owned());
    }
    let sample_rate_u32 = sample_rate.round() as u32;
    if block_size == 0 || block_size > 65_536 {
        return Err("VST3 host block size is not renderable".to_owned());
    }
    let output_channels = output_bus_channels.iter().sum();
    let mut resampler = StereoStreamResampler::new(sample_rate_u32, output_sample_rate)?;
    {
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        plugin
            .start_processing()
            .map_err(|error| error.to_string())?;
    }

    let mut active_notes = BTreeMap::<(u8, u8), usize>::new();
    let mut sustain_channels = BTreeSet::<u8>::new();
    let render_result = (|| {
        loop {
            if cancelled.load(Ordering::Acquire) || writer.is_cancelled() {
                break Ok(());
            }
            let mut events = Vec::new();
            let mut disconnected = false;
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            let output = {
                let mut plugin = plugin
                    .lock()
                    .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
                for mut event in events {
                    if let MidiEvent::NoteOn {
                        channel,
                        note,
                        velocity: 0,
                    } = event
                    {
                        event = MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        };
                    }
                    if note_pitch_offset_semitones != 0 {
                        match &mut event {
                            MidiEvent::NoteOn { note, .. }
                            | MidiEvent::NoteOff { note, .. }
                            | MidiEvent::PolyAftertouch { note, .. } => {
                                *note = (i16::from(*note) + note_pitch_offset_semitones)
                                    .clamp(0, 127) as u8;
                            }
                            _ => {}
                        }
                    }
                    let release = match event {
                        MidiEvent::NoteOff { velocity, .. } => velocity,
                        _ => 0,
                    };
                    match event {
                        MidiEvent::NoteOn { .. } | MidiEvent::NoteOff { .. } => {
                            send_scheduled_midi_event(
                                &mut plugin,
                                event,
                                0,
                                pitch_offset_cents,
                                0,
                                ScheduledVst3Note {
                                    note_id: 0,
                                    note_pan: 64,
                                    note_mod_x: 128,
                                    note_mod_y: 128,
                                    note_release: release,
                                    supports_pan: false,
                                    supports_volume: false,
                                    supports_brightness: false,
                                    mod_y_expression: None,
                                    layer_gain: 1.0,
                                    layer_pan_raw: 0,
                                },
                            )?;
                        }
                        _ => plugin
                            .send_midi_event_at(event, 0)
                            .map_err(|error| error.to_string())?,
                    }
                    match event {
                        MidiEvent::NoteOn {
                            channel,
                            note,
                            velocity,
                        } if velocity > 0 => {
                            let count = active_notes.entry((channel.as_index(), note)).or_default();
                            *count = count.saturating_add(1);
                        }
                        MidiEvent::NoteOff { channel, note, .. } => {
                            let key = (channel.as_index(), note);
                            if let Some(count) = active_notes.get_mut(&key) {
                                if *count <= 1 {
                                    active_notes.remove(&key);
                                } else {
                                    *count -= 1;
                                }
                            }
                        }
                        MidiEvent::ControlChange {
                            channel,
                            controller: 64,
                            value,
                        } if value >= 64 => {
                            sustain_channels.insert(channel.as_index());
                        }
                        MidiEvent::ControlChange {
                            channel,
                            controller: 64 | 121,
                            ..
                        } => {
                            sustain_channels.remove(&channel.as_index());
                        }
                        MidiEvent::ControlChange {
                            channel,
                            controller,
                            ..
                        } if controller == 120 || (123..=127).contains(&controller) => {
                            active_notes.retain(|(active_channel, _), _| {
                                *active_channel != channel.as_index()
                            });
                        }
                        _ => {}
                    }
                }
                let mut buffers = AudioBuffers::new(0, output_channels, block_size, sample_rate);
                plugin
                    .process_audio(&mut buffers)
                    .map_err(|error| error.to_string())?;
                if buffers.outputs.len() != output_channels
                    || buffers
                        .outputs
                        .iter()
                        .any(|channel| channel.len() < block_size)
                {
                    return Err("VST3 returned audio buffers with an unexpected shape".to_owned());
                }
                resampler.push_outputs_with_bus_channels(
                    &buffers.outputs,
                    block_size,
                    &output_bus_channels,
                )?
            };
            if let Err(error) = writer.write_stereo_samples(&output) {
                if cancelled.load(Ordering::Acquire) || writer.is_cancelled() {
                    break Ok(());
                }
                break Err(error);
            }
            if disconnected {
                break Ok(());
            }
        }
    })();

    let cleanup_result = (|| {
        let mut plugin = plugin
            .lock()
            .map_err(|_| "plug-in state lock was poisoned".to_owned())?;
        for channel in sustain_channels {
            let Some(channel) = MidiChannel::from_index(channel) else {
                continue;
            };
            plugin
                .send_midi_event_at(
                    MidiEvent::ControlChange {
                        channel,
                        controller: 64,
                        value: 0,
                    },
                    0,
                )
                .map_err(|error| error.to_string())?;
        }
        for ((channel, note), count) in active_notes {
            let Some(channel) = MidiChannel::from_index(channel) else {
                continue;
            };
            for _ in 0..count {
                send_scheduled_midi_event(
                    &mut plugin,
                    MidiEvent::NoteOff {
                        channel,
                        note,
                        velocity: 0,
                    },
                    0,
                    pitch_offset_cents,
                    0,
                    ScheduledVst3Note {
                        note_id: 0,
                        note_pan: 64,
                        note_mod_x: 128,
                        note_mod_y: 128,
                        note_release: 0,
                        supports_pan: false,
                        supports_volume: false,
                        supports_brightness: false,
                        mod_y_expression: None,
                        layer_gain: 1.0,
                        layer_pan_raw: 0,
                    },
                )?;
            }
        }
        let mut buffers = AudioBuffers::new(0, output_channels, block_size, sample_rate);
        plugin
            .process_audio(&mut buffers)
            .map_err(|error| error.to_string())
    })();
    let stop_result = plugin
        .lock()
        .map_err(|_| "plug-in state lock was poisoned".to_owned())?
        .stop_processing()
        .map_err(|error| error.to_string());

    render_result.and(cleanup_result).and(stop_result)
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
                    send_scheduled_midi_event(
                        &mut plugin,
                        event.event,
                        offset,
                        render.pitch_offset_cents,
                        event.fine_pitch_offset_cents,
                        ScheduledVst3Note {
                            note_id: event.note_id,
                            note_pan: event.note_pan,
                            note_mod_x: event.note_mod_x,
                            note_mod_y: event.note_mod_y,
                            note_release: event.note_release,
                            supports_pan: render.supports_note_pan,
                            supports_volume: render.supports_note_volume,
                            supports_brightness: render.supports_note_brightness,
                            mod_y_expression: render.mod_y_expression,
                            layer_gain: event.layer_gain,
                            layer_pan_raw: event.layer_pan_raw,
                        },
                    )?;
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
                resampler.push_outputs_with_bus_channels(
                    &buffers.outputs,
                    frame_count,
                    &render.output_bus_channels,
                )?
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

    #[cfg(test)]
    fn push_outputs(
        &mut self,
        outputs: &[Vec<f32>],
        frame_count: usize,
        output_channels: usize,
    ) -> Result<Vec<f32>, String> {
        self.push_outputs_with_bus_channels(outputs, frame_count, &[output_channels])
    }

    fn push_outputs_with_bus_channels(
        &mut self,
        outputs: &[Vec<f32>],
        frame_count: usize,
        bus_channels: &[usize],
    ) -> Result<Vec<f32>, String> {
        let output_channels = bus_channels.iter().try_fold(0usize, |total, channels| {
            total
                .checked_add(*channels)
                .ok_or_else(|| "VST3 output channel count overflow".to_owned())
        })?;
        if bus_channels.is_empty()
            || bus_channels
                .iter()
                .any(|channels| !matches!(channels, 1 | 2))
            || outputs.len() != output_channels
            || outputs.iter().any(|channel| channel.len() < frame_count)
        {
            return Err(format!(
                "VST3 output block with bus layout {bus_channels:?} cannot be mixed to stereo"
            ));
        }
        self.source_frames = self
            .source_frames
            .checked_add(frame_count as u64)
            .ok_or_else(|| "VST3 stream frame count overflow".to_owned())?;
        for (frame_index, _) in outputs[0].iter().take(frame_count).enumerate() {
            let mut left = 0.0_f64;
            let mut right = 0.0_f64;
            let mut channel_index = 0;
            for bus_channel_count in bus_channels {
                if *bus_channel_count == 1 {
                    let sample = f64::from(finite_sample(outputs[channel_index][frame_index]));
                    left += sample;
                    right += sample;
                    channel_index += 1;
                } else {
                    left += f64::from(finite_sample(outputs[channel_index][frame_index]));
                    right += f64::from(finite_sample(outputs[channel_index + 1][frame_index]));
                    channel_index += 2;
                }
            }
            self.buffer
                .push_back([finite_mixed_sample(left), finite_mixed_sample(right)]);
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

fn finite_mixed_sample(sample: f64) -> f32 {
    if !sample.is_finite() {
        0.0
    } else {
        sample.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
    }
}

fn plugin_event_with_pitch_offset(
    event: MidiEvent,
    sample_offset: i32,
    pitch_offset_cents: i32,
) -> Result<PluginEvent, String> {
    let mut plugin_event = PluginEvent::from(event).at(sample_offset);
    match &mut plugin_event.data {
        PluginEventData::NoteOn { tuning, .. } | PluginEventData::NoteOff { tuning, .. } => {
            *tuning = pitch_offset_cents as f32;
        }
        _ => return Err("VST3 pitch tuning can only be applied to note events".to_owned()),
    }
    Ok(plugin_event)
}

fn plugin_event_with_pitch_offset_and_note_id(
    event: MidiEvent,
    sample_offset: i32,
    pitch_offset_cents: i32,
    note_id: i32,
) -> Result<PluginEvent, String> {
    let mut plugin_event =
        plugin_event_with_pitch_offset(event, sample_offset, pitch_offset_cents)?;
    match &mut plugin_event.data {
        PluginEventData::NoteOn {
            note_id: event_note_id,
            ..
        }
        | PluginEventData::NoteOff {
            note_id: event_note_id,
            ..
        } => *event_note_id = note_id,
        _ => return Err("VST3 note IDs can only be applied to note events".to_owned()),
    }
    Ok(plugin_event)
}

fn set_vst3_note_off_release_velocity(
    plugin_event: &mut PluginEvent,
    note_release: u8,
) -> Result<(), String> {
    match &mut plugin_event.data {
        PluginEventData::NoteOff { velocity, .. } => {
            *velocity = vst3_note_release_value(note_release);
            Ok(())
        }
        _ => Err("VST3 release velocity can only be applied to note-off events".to_owned()),
    }
}

fn send_scheduled_midi_event(
    plugin: &mut Plugin,
    event: MidiEvent,
    sample_offset: i32,
    project_pitch_offset_cents: i16,
    fine_pitch_offset_cents: i16,
    note: ScheduledVst3Note,
) -> Result<(), String> {
    let pitch_offset_cents =
        combined_pitch_offset_cents(project_pitch_offset_cents, fine_pitch_offset_cents);
    if !matches!(event, MidiEvent::NoteOn { .. } | MidiEvent::NoteOff { .. }) {
        return plugin
            .send_midi_event_at(event, sample_offset)
            .map_err(|error| error.to_string());
    }
    let apply_layer_volume = note.supports_volume && (note.layer_gain - 1.0).abs() > f32::EPSILON;
    if note.supports_pan
        || apply_layer_volume
        || note.supports_brightness
        || note.mod_y_expression.is_some()
    {
        let mut plugin_event = plugin_event_with_pitch_offset_and_note_id(
            event,
            sample_offset,
            pitch_offset_cents,
            note.note_id,
        )?;
        let is_note_on = matches!(event, MidiEvent::NoteOn { .. });
        if matches!(event, MidiEvent::NoteOff { .. }) {
            set_vst3_note_off_release_velocity(&mut plugin_event, note.note_release)?;
        }
        plugin
            .send_plugin_event(plugin_event)
            .map_err(|error| error.to_string())?;
        if is_note_on {
            let note_id = NoteId::from_raw(note.note_id);
            if note.supports_pan {
                plugin
                    .send_note_expression_at(
                        note_id,
                        NoteExpressionType::Pan,
                        vst3_note_pan_with_layer_offset(note.note_pan, note.layer_pan_raw),
                        sample_offset,
                    )
                    .map_err(|error| error.to_string())?;
            }
            if apply_layer_volume {
                plugin
                    .send_note_expression_at(
                        note_id,
                        NoteExpressionType::Volume,
                        vst3_note_volume_value(note.layer_gain),
                        sample_offset,
                    )
                    .map_err(|error| error.to_string())?;
            }
            if note.supports_brightness {
                plugin
                    .send_note_expression_at(
                        note_id,
                        NoteExpressionType::Brightness,
                        vst3_note_brightness_value(note.note_mod_x),
                        sample_offset,
                    )
                    .map_err(|error| error.to_string())?;
            }
            if let Some(expression) = note.mod_y_expression {
                plugin
                    .send_note_expression_at(
                        note_id,
                        expression.kind,
                        vst3_note_mod_y_value(note.note_mod_y, expression),
                        sample_offset,
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        return Ok(());
    }
    if matches!(event, MidiEvent::NoteOff { .. }) {
        let mut plugin_event =
            plugin_event_with_pitch_offset(event, sample_offset, pitch_offset_cents)?;
        set_vst3_note_off_release_velocity(&mut plugin_event, note.note_release)?;
        return plugin
            .send_plugin_event(plugin_event)
            .map_err(|error| error.to_string());
    }
    if pitch_offset_cents == 0 {
        plugin
            .send_midi_event_at(event, sample_offset)
            .map_err(|error| error.to_string())
    } else {
        let event = plugin_event_with_pitch_offset(event, sample_offset, pitch_offset_cents)?;
        plugin
            .send_plugin_event(event)
            .map_err(|error| error.to_string())
    }
}

fn combined_pitch_offset_cents(
    project_pitch_offset_cents: i16,
    fine_pitch_offset_cents: i16,
) -> i32 {
    i32::from(project_pitch_offset_cents) + i32::from(fine_pitch_offset_cents)
}

fn channel_pitch_offset_cents(
    channel: &crate::ChannelSummary,
    project_master_pitch_cents: Option<i16>,
) -> i16 {
    add_channel_fine_tune_cents(
        channel.master_pitch_render_offset_cents(project_master_pitch_cents),
        channel.keyboard_fine_tune_cents(),
    )
}

fn add_channel_fine_tune_cents(
    master_pitch_offset_cents: i16,
    fine_tune_cents: Option<i32>,
) -> i16 {
    let fine_tune_cents = fine_tune_cents
        .and_then(|cents| i16::try_from(cents).ok())
        .unwrap_or_default();
    master_pitch_offset_cents.saturating_add(fine_tune_cents)
}

#[cfg(test)]
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

fn append_vst_output_buses_to_stereo(
    stereo: &mut Vec<f32>,
    interleaved: &[f32],
    bus_channels: &[usize],
) -> Result<(), String> {
    append_vst_output_buses_to_stereo_with_mixer_transforms(stereo, interleaved, bus_channels, None)
}

fn append_vst_output_buses_to_stereo_with_mixer_transforms(
    stereo: &mut Vec<f32>,
    interleaved: &[f32],
    bus_channels: &[usize],
    mixer_transforms: Option<&[crate::MixerSourceRouteTransform]>,
) -> Result<(), String> {
    let output_channels = bus_channels.iter().try_fold(0usize, |total, channels| {
        total
            .checked_add(*channels)
            .ok_or_else(|| "VST3 output channel count overflow".to_owned())
    })?;
    if bus_channels.is_empty()
        || bus_channels
            .iter()
            .any(|channels| !matches!(channels, 1 | 2))
        || !interleaved.len().is_multiple_of(output_channels)
        || mixer_transforms.is_some_and(|transforms| transforms.len() != bus_channels.len())
    {
        return Err(format!(
            "VST3 output block with bus layout {bus_channels:?} cannot be routed to stereo"
        ));
    }

    for frame in interleaved.chunks_exact(output_channels) {
        let mut left = 0.0_f64;
        let mut right = 0.0_f64;
        let mut channel_index = 0;
        for (bus_index, bus_channel_count) in bus_channels.iter().copied().enumerate() {
            let mut bus_frame = if bus_channel_count == 1 {
                let sample = finite_sample(frame[channel_index]);
                channel_index += 1;
                [sample, sample]
            } else {
                let stereo = [
                    finite_sample(frame[channel_index]),
                    finite_sample(frame[channel_index + 1]),
                ];
                channel_index += 2;
                stereo
            };
            if let Some(transform) =
                mixer_transforms.and_then(|transforms| transforms.get(bus_index))
            {
                transform.apply_frame(&mut bus_frame);
            }
            left += f64::from(finite_sample(bus_frame[0]));
            right += f64::from(finite_sample(bus_frame[1]));
        }
        stereo.push(finite_mixed_sample(left));
        stereo.push(finite_mixed_sample(right));
    }
    Ok(())
}

fn mix_stereo_pattern_render_buffer(
    mix: &mut Vec<f32>,
    mix_frames: &mut u64,
    input: &[f32],
    input_frames: u64,
) -> Result<(), String> {
    let input_samples = usize::try_from(input_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(2))
        .ok_or_else(|| "VST3 Pattern render frame count overflow".to_owned())?;
    if input.len() != input_samples {
        return Err("VST3 Pattern render buffer is not interleaved stereo".to_owned());
    }

    let output_frames = (*mix_frames).max(input_frames);
    let output_samples = usize::try_from(output_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(2))
        .ok_or_else(|| "VST3 Pattern mix frame count overflow".to_owned())?;
    output_samples
        .checked_mul(std::mem::size_of::<f32>())
        .filter(|bytes| *bytes <= MAX_STEREO_BUFFER_BYTES)
        .ok_or_else(|| {
            format!(
                "VST3 Pattern mix exceeds the {} MiB in-memory render limit",
                MAX_STEREO_BUFFER_BYTES / (1024 * 1024)
            )
        })?;
    mix.resize(output_samples, 0.0);
    for (mixed, sample) in mix.iter_mut().zip(input) {
        *mixed = finite_sample(*mixed + finite_sample(*sample));
    }
    *mix_frames = output_frames;
    Ok(())
}

#[cfg(test)]
fn apply_stereo_mixer_transform(
    stereo: &mut [f32],
    transform: MixerSourceRouteTransform,
) -> Result<(), String> {
    if !stereo.len().is_multiple_of(2) {
        return Err("VST3 Pattern mixer transform requires interleaved stereo samples".to_owned());
    }
    for frame in stereo.as_chunks_mut::<2>().0 {
        let mut routed_frame = [frame[0], frame[1]];
        transform.apply_frame(&mut routed_frame);
        frame[0] = finite_sample(routed_frame[0]);
        frame[1] = finite_sample(routed_frame[1]);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PatternNoteScheduleOptions {
    channel_id: u16,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: f64,
    global_swing_mix_raw: u8,
    channel_swing_mix_raw: u16,
}

#[cfg(test)]
fn scheduled_pattern_events(
    notes: &[PatternNote],
    channel_id: u16,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: f64,
    global_swing_mix_raw: u8,
    channel_swing_mix_raw: u16,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    scheduled_pattern_events_with_layer_adjustments(
        notes,
        PatternNoteScheduleOptions {
            channel_id,
            ppq,
            tempo_bpm,
            sample_rate,
            global_swing_mix_raw,
            channel_swing_mix_raw,
        },
        None,
    )
}

fn scheduled_pattern_events_with_layer_adjustments(
    notes: &[PatternNote],
    options: PatternNoteScheduleOptions,
    layer_adjustments: Option<&[(f32, i32)]>,
) -> Result<(Vec<ScheduledMidiEvent>, usize), String> {
    let PatternNoteScheduleOptions {
        channel_id,
        ppq,
        tempo_bpm,
        sample_rate,
        global_swing_mix_raw,
        channel_swing_mix_raw,
    } = options;
    if ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Err("VST3 host sample rate must be finite and positive".to_owned());
    }
    if layer_adjustments.is_some_and(|adjustments| adjustments.len() != notes.len()) {
        return Err("Layer adjustment count does not match the Pattern note count".to_owned());
    }
    let mut events = Vec::new();
    let mut note_count = 0usize;
    for (note_index, note) in notes
        .iter()
        .enumerate()
        .filter(|(_, note)| note.channel_id == channel_id && !note.is_muted())
    {
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
        let note_id = next_vst3_note_expression_id();
        let (layer_gain, layer_pan_raw) = layer_adjustments
            .and_then(|adjustments| adjustments.get(note_index).copied())
            .unwrap_or((1.0, 0));
        events.push(ScheduledMidiEvent {
            frame: start_frame as u64,
            priority: 1,
            event: MidiEvent::NoteOn {
                channel,
                note: note.key as u8,
                velocity: note.velocity,
            },
            fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
            note_id,
            note_pan: note.pan,
            note_mod_x: note.mod_x,
            note_mod_y: note.mod_y,
            note_release: note.release,
            layer_gain,
            layer_pan_raw,
            performance_track_id: None,
        });
        events.push(ScheduledMidiEvent {
            frame: end_frame as u64,
            priority: 0,
            event: MidiEvent::NoteOff {
                channel,
                note: note.key as u8,
                velocity: 0,
            },
            fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
            note_id,
            note_pan: note.pan,
            note_mod_x: note.mod_x,
            note_mod_y: note.mod_y,
            note_release: note.release,
            layer_gain,
            layer_pan_raw,
            performance_track_id: None,
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
        audio_inputs: info.audio_inputs,
        audio_outputs: info.audio_outputs,
        latency_samples: plugin.latency_samples(),
        manual_latency_offset_samples: 0,
        sample_rate_hz: plugin.sample_rate(),
    }
}

fn apply_manual_latency_offset(reported_latency_samples: u32, offset_samples: i32) -> u32 {
    (i64::from(reported_latency_samples) + i64::from(offset_samples)).clamp(0, i64::from(u32::MAX))
        as u32
}

fn validate_vst3_audio_block(
    buffers: &BusAudioBuffers,
    plugin_sample_rate: f64,
    plugin_block_size: usize,
) -> Result<(), String> {
    if buffers.block_size == 0 {
        return Err("audio block size must be greater than zero".to_owned());
    }
    if buffers.block_size > plugin_block_size {
        return Err(format!(
            "audio block has {} frames; the VST3 instance is configured for at most {plugin_block_size}",
            buffers.block_size
        ));
    }
    if !buffers.sample_rate.is_finite() || buffers.sample_rate <= 0.0 {
        return Err("audio block sample rate must be finite and positive".to_owned());
    }
    let rate_tolerance = plugin_sample_rate.abs().max(1.0) * 1e-12;
    if (buffers.sample_rate - plugin_sample_rate).abs() > rate_tolerance {
        return Err(format!(
            "audio block sample rate {} does not match the VST3 instance rate {plugin_sample_rate}",
            buffers.sample_rate
        ));
    }
    if buffers
        .inputs
        .iter()
        .chain(&buffers.outputs)
        .flat_map(|bus| &bus.channels)
        .any(|channel| channel.len() != buffers.block_size)
    {
        return Err("every VST3 audio bus channel must match the block frame count".to_owned());
    }
    Ok(())
}

fn resize_vst3_bus_audio_buffers(buffers: &mut BusAudioBuffers, frame_count: usize) {
    for bus in buffers.inputs.iter_mut().chain(&mut buffers.outputs) {
        for channel in &mut bus.channels {
            channel.resize(frame_count, 0.0);
            channel.fill(0.0);
        }
    }
    buffers.block_size = frame_count;
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
    use crate::{MixerInsertSignalTransform, MixerLiveSourceRoutePlan};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_BINARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn live_pdc_delay_updates_resize_existing_track_lines_in_place() {
        let mut delays = (0..2)
            .map(|_| StereoFrameDelay::new(2).unwrap())
            .collect::<Vec<_>>();

        update_stereo_delay_lines(&mut delays, [4, 6]).unwrap();
        assert_eq!(
            delays
                .iter()
                .map(StereoFrameDelay::delay_frames)
                .collect::<Vec<_>>(),
            [4, 6]
        );

        update_stereo_delay_lines(&mut delays, [0, 1, 3]).unwrap();
        assert_eq!(
            delays
                .iter()
                .map(StereoFrameDelay::delay_frames)
                .collect::<Vec<_>>(),
            [0, 1, 3]
        );
    }

    fn temporary_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fl-studio-rebuild-vst3-{name}-{}-{}.bin",
            std::process::id(),
            TEST_BINARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn x86_pe_header(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0_u8; 70];
        bytes[0..2].copy_from_slice(b"MZ");
        bytes[0x3C..0x40].copy_from_slice(&64_u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    fn x86_elf_header(class: u8, machine: u16) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64];
        bytes[0..4].copy_from_slice(b"\x7FELF");
        bytes[4] = class;
        bytes[5] = 1;
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    #[test]
    fn live_mixer_effect_mix_ramp_smooths_each_target_change() {
        let mut ramp = LiveWetMixRamp::new(0.0, 4);
        ramp.set_target(1.0);
        assert!((ramp.next() - 0.25).abs() < f32::EPSILON);
        assert!((ramp.next() - 0.5).abs() < f32::EPSILON);
        assert!((ramp.next() - 0.75).abs() < f32::EPSILON);
        assert_eq!(ramp.next(), 1.0);

        ramp.set_target(0.5);
        assert!((ramp.next() - 0.875).abs() < f32::EPSILON);
        assert!((ramp.next() - 0.75).abs() < f32::EPSILON);
        assert!((ramp.next() - 0.625).abs() < f32::EPSILON);
        assert_eq!(ramp.next(), 0.5);
    }

    #[test]
    fn detects_32_bit_x86_pe_and_elf_binaries() {
        let pe32 = temporary_test_path("pe32");
        let pe64 = temporary_test_path("pe64");
        let elf32 = temporary_test_path("elf32");
        let elf64 = temporary_test_path("elf64");
        std::fs::write(&pe32, x86_pe_header(0x014C)).unwrap();
        std::fs::write(&pe64, x86_pe_header(0x8664)).unwrap();
        std::fs::write(&elf32, x86_elf_header(1, 3)).unwrap();
        std::fs::write(&elf64, x86_elf_header(2, 62)).unwrap();

        assert!(read_32bit_x86_binary(&pe32));
        assert!(!read_32bit_x86_binary(&pe64));
        assert!(read_32bit_x86_binary(&elf32));
        assert!(!read_32bit_x86_binary(&elf64));

        for path in [pe32, pe64, elf32, elf64] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn finds_architecture_specific_32_bit_vst3_bundle_binary() {
        let bundle = temporary_test_path("bundle");
        let contents = bundle.join("Contents");
        #[cfg(target_os = "linux")]
        let (folder, extension) = ("i386-linux", "so");
        #[cfg(target_os = "windows")]
        let (folder, extension) = ("x86-win", "vst3");
        let binary_directory = contents.join(folder);
        std::fs::create_dir_all(&binary_directory).unwrap();
        let binary = binary_directory.join(format!("Legacy.{extension}"));
        #[cfg(target_os = "linux")]
        std::fs::write(&binary, x86_elf_header(1, 3)).unwrap();
        #[cfg(target_os = "windows")]
        std::fs::write(&binary, x86_pe_header(0x014C)).unwrap();

        assert_eq!(x86_vst3_binary(&bundle), Some(binary));

        std::fs::remove_dir_all(bundle).unwrap();
    }

    fn playlist_parameter_automation_fixture() -> FlpDocument {
        fn append_data_event(event_stream: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
            assert!(payload.len() < 128);
            event_stream.extend_from_slice(&[opcode, payload.len() as u8]);
            event_stream.extend_from_slice(payload);
        }

        let mut event_stream = Vec::new();
        append_data_event(&mut event_stream, 0xC7, b"26.0.0\0");
        event_stream.extend_from_slice(&[0x40, 0, 0, 0x41, 7, 0, 0xA4]);
        event_stream.extend_from_slice(&96_u32.to_le_bytes());
        event_stream.extend_from_slice(&[0x40, 9, 0, 0x15, 4, 0x48, 9, 0, 0x62, 0, 0]);

        event_stream.extend_from_slice(&[0x40, 11, 0, 0x15, 5]);
        let mut automation = vec![0xA5; 17];
        automation.extend_from_slice(&3_u32.to_le_bytes());
        for (position_delta, value) in [(0.25_f64, 0.2_f64), (0.75, 0.8), (2.0, 0.3)] {
            automation.extend_from_slice(&position_delta.to_le_bytes());
            automation.extend_from_slice(&value.to_le_bytes());
            automation.extend_from_slice(&0.0_f32.to_le_bytes());
            automation.extend_from_slice(&[0; 4]);
        }
        append_data_event(&mut event_stream, 0xEA, &automation);
        event_stream.extend_from_slice(&[0x62, 0, 0, 0x63, 3, 0]);

        let mut clip = vec![0_u8; 80];
        clip[4..6].copy_from_slice(&0x5000_u16.to_le_bytes());
        clip[6..8].copy_from_slice(&0x5007_u16.to_le_bytes());
        clip[8..12].copy_from_slice(&240_u32.to_le_bytes());
        clip[12..14].copy_from_slice(&499_u16.to_le_bytes());
        clip[64..72].copy_from_slice(&1.0_f64.to_le_bytes());
        append_data_event(&mut event_stream, 0xE9, &clip);

        let mut automation_clip = vec![0_u8; 80];
        automation_clip[0..4].copy_from_slice(&384_u32.to_le_bytes());
        automation_clip[4..6].copy_from_slice(&0x5000_u16.to_le_bytes());
        automation_clip[6..8].copy_from_slice(&11_u16.to_le_bytes());
        automation_clip[8..12].copy_from_slice(&288_u32.to_le_bytes());
        automation_clip[12..14].copy_from_slice(&499_u16.to_le_bytes());
        automation_clip[64..72].copy_from_slice(&1.0_f64.to_le_bytes());
        append_data_event(&mut event_stream, 0xE9, &automation_clip);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FLhd");
        bytes.extend_from_slice(&6_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&96_u16.to_le_bytes());
        bytes.extend_from_slice(b"FLdt");
        bytes.extend_from_slice(&(event_stream.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&event_stream);
        FlpDocument::parse(&bytes).expect("Playlist automation fixture should parse")
    }

    #[test]
    fn pattern_loop_automation_splits_processing_blocks_at_loop_boundaries() {
        assert_eq!(
            looped_parameter_automation_segments(8, 6, 10).unwrap(),
            vec![(8, 0, 2), (0, 2, 4)]
        );
        assert_eq!(
            looped_parameter_automation_segments(20, 25, 10).unwrap(),
            vec![(0, 0, 10), (0, 10, 10), (0, 20, 5)]
        );
        assert!(looped_parameter_automation_segments(0, 1, 0).is_err());
    }

    #[test]
    fn playlist_live_midi_sessions_retire_only_their_own_queued_events() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let stopped_generation = Arc::new(AtomicU64::new(0));
        let playlist_sender = Vst3PlaylistLiveInputSender {
            sender,
            next_generation: Arc::new(AtomicU64::new(0)),
            stopped_generation: Arc::clone(&stopped_generation),
        };
        let channel = MidiChannel::from_index(0).expect("channel 1 is supported");

        let first_session = playlist_sender.begin_session();
        first_session
            .try_send(MidiEvent::NoteOn {
                channel,
                note: 60,
                velocity: 100,
            })
            .unwrap();
        first_session.stop();

        let second_session = playlist_sender.begin_session();
        second_session
            .try_send(MidiEvent::NoteOn {
                channel,
                note: 64,
                velocity: 100,
            })
            .unwrap();

        let stopped = stopped_generation.load(Ordering::Acquire);
        let first_event = receiver.try_recv().unwrap();
        let second_event = receiver.try_recv().unwrap();
        assert!(first_event.generation <= stopped);
        assert!(second_event.generation > stopped);
        assert_eq!(second_session.generation, first_session.generation + 1);
    }

    #[test]
    fn playlist_live_mixer_source_smooths_saved_pan_and_fader_targets() {
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        meters.set_live_mixer_insert_pan_raw(1, Some(-6_400));
        let mut source = PlaylistLiveMixerSource {
            control: LiveMixerSourceControl::new(
                MixerLiveSourceRoutePlan {
                    track: 1,
                    source_transform: MixerSourceRouteTransform::default(),
                    saved_fader_gain: 1.0,
                    saved_pan_raw: Some(-6_400),
                    saved_stereo_separation_raw: None,
                    saved_signal_flags: None,
                    saved_enabled: None,
                    pan_law_raw: Some(2),
                    output_transform: MixerSourceRouteTransform::default(),
                    insert_meter_transforms: Vec::new(),
                    ..MixerLiveSourceRoutePlan::default()
                },
                48_000,
            ),
            targets: None,
        };
        source.targets = source.control.targets(&meters);
        let targets = source.targets.as_ref().unwrap();
        let mut frame = [1.0, -0.5];
        let controls = source.control.next_frame_controls(targets);
        source.control.apply_output_frame(controls, &mut frame);
        assert_eq!(frame, [0.5, 0.0]);

        meters.set_live_mixer_insert_fader_raw(1, Some(2_500));
        meters.set_live_mixer_insert_pan_raw(1, Some(6_400));
        frame = [1.0, -0.5];
        let controls = source.control.next_frame_controls(targets);
        source.control.apply_output_frame(controls, &mut frame);
        assert!(frame[0] > 0.5 && frame[0] < 0.55);
        assert!(frame[1] < 0.0 && frame[1] > -0.05);

        for _ in 0..100_000 {
            let controls = source.control.next_frame_controls(targets);
            frame = [1.0, -0.5];
            source.control.apply_output_frame(controls, &mut frame);
        }
        assert!(frame[0].abs() < 0.0001);
        assert!((frame[1] - 0.125).abs() < 0.0001);
    }

    #[test]
    fn playlist_live_mixer_source_scales_every_send_meter_route() {
        let meters = MixerTrackPeakMeters::default();
        let source = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: None,
                saved_stereo_separation_raw: None,
                saved_signal_flags: None,
                saved_enabled: None,
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: vec![
                    (1, MixerSourceRouteTransform::default()),
                    (2, MixerSourceRouteTransform::default().scaled(0.5)),
                ],
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        let targets = source.targets(&meters).unwrap();
        let mut output_frame = [1.0, -0.5];
        let controls = source.next_frame_controls(&targets);
        source.apply_output_frame(controls, &mut output_frame);

        meters.set_live_mixer_insert_fader_raw(1, Some(2_500));
        let controls = source.next_frame_controls(&targets);
        output_frame = [1.0, -0.5];
        source.apply_output_frame(controls, &mut output_frame);
        let first_gain = output_frame[0];
        assert!(first_gain < 1.0);
        assert!(first_gain > 0.25);
        assert!((output_frame[0] - first_gain).abs() < f32::EPSILON);
        assert!((output_frame[1] + first_gain * 0.5).abs() < f32::EPSILON);
        for track in [1, 2] {
            let mut meter_frame = [1.0, -0.5];
            source.apply_meter_frame(
                controls,
                track,
                source
                    .insert_meter_transforms()
                    .iter()
                    .find(|(meter_track, _)| *meter_track == track)
                    .expect("every send destination has its own meter transform")
                    .1,
                &mut meter_frame,
            );
            let route_gain = if track == 1 {
                first_gain
            } else {
                first_gain * 0.5
            };
            assert!((meter_frame[0] - route_gain).abs() < f32::EPSILON);
            assert!((meter_frame[1] + route_gain * 0.5).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn playlist_mixer_effect_return_uses_live_send_bus_fader_and_pan() {
        let meters = MixerTrackPeakMeters::default();
        meters.set_live_mixer_insert_fader_raw(1, Some(10_000));
        meters.set_live_mixer_insert_pan_raw(1, Some(0));
        meters.set_live_mixer_send_level_raw(1, 0, Some(12_800));
        meters.set_live_mixer_send_enabled(1, 0, Some(true));

        let live_control = LiveMixerSourceControl::new(
            MixerLiveSourceRoutePlan {
                track: 1,
                source_transform: MixerSourceRouteTransform::default(),
                saved_fader_gain: 1.0,
                saved_pan_raw: Some(0),
                saved_stereo_separation_raw: None,
                saved_signal_flags: None,
                saved_enabled: None,
                pan_law_raw: Some(2),
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: vec![(1, MixerSourceRouteTransform::default())],
                live_send_target_keys: vec![crate::MixerLiveSendTargetKey {
                    source_track: 1,
                    destination_track: 0,
                }],
                saved_live_send_gains: vec![1.0],
                saved_live_send_enabled: vec![true],
                live_send_output_solo_tracks: vec![vec![1]],
                saved_live_send_output_solo_gains: vec![1.0],
                live_send_meter_solo_tracks: vec![vec![1]],
                saved_live_send_meter_solo_gains: vec![1.0],
                live_send_output_paths: Some(vec![crate::MixerLiveSendOutputPath {
                    transform: MixerSourceRouteTransform::default().scaled(0.5),
                    send_gains: vec![crate::MixerLiveSendGain {
                        target_index: Some(0),
                        saved_gain: 1.0,
                        enabled_target_index: Some(0),
                        saved_enabled: true,
                    }],
                    insert_enabled_gains: Vec::new(),
                }]),
                live_send_meter_paths: vec![crate::MixerLiveSendMeterPath {
                    track: 1,
                    transform: MixerSourceRouteTransform::default(),
                    send_gains: Vec::new(),
                    insert_enabled_gains: Vec::new(),
                }],
                ..MixerLiveSourceRoutePlan::default()
            },
            48_000,
        );
        let targets = live_control
            .targets(&meters)
            .expect("the effect return should resolve its live Mixer controls");
        let mut chain = PlaylistMixerEffectChain {
            insert_ordinal: 1,
            route_plan: crate::MixerSourceRoutePlan {
                output_transform: MixerSourceRouteTransform::default().scaled(0.5),
                insert_meter_transforms: vec![(1, MixerSourceRouteTransform::default())],
                ..crate::MixerSourceRoutePlan::default()
            },
            live_source: Some(PlaylistMixerEffectLiveSource {
                control: live_control,
                targets: Some(targets),
            }),
            post_effect_meter_transform: MixerSourceRouteTransform::default(),
            first_effect_index: 0,
            end_effect_index: 0,
            latency_frames: 0,
            output_alignment_delay: StereoFrameDelay::default(),
            dry_reference_delay: StereoFrameDelay::default(),
            downstream_delays: Vec::new(),
        };

        let aggregate_effect_delta = [3.5, -0.75];
        let initial_controls = chain
            .next_live_frame_controls()
            .expect("live controls should be attached");
        let initial_output = route_playlist_mixer_effect_output_delta(
            &mut chain.output_alignment_delay,
            &chain.route_plan,
            chain.live_source.as_ref(),
            aggregate_effect_delta,
            Some(initial_controls),
        );
        let mut expected_initial = aggregate_effect_delta;
        crate::apply_mixer_pan_raw_f32(&mut expected_initial, 0.0, Some(2));
        MixerSourceRouteTransform::default()
            .scaled(0.5)
            .apply_frame(&mut expected_initial);
        for (actual, expected) in initial_output.into_iter().zip(expected_initial) {
            assert!((actual - expected).abs() < 0.0001);
        }

        meters.set_live_mixer_insert_fader_raw(1, Some(5_000));
        meters.set_live_mixer_insert_pan_raw(1, Some(6_400));
        let mut output = initial_output;
        let mut meter = initial_output;
        for _ in 0..100_000 {
            let controls = chain
                .next_live_frame_controls()
                .expect("live controls should remain attached");
            output = route_playlist_mixer_effect_output_delta(
                &mut chain.output_alignment_delay,
                &chain.route_plan,
                chain.live_source.as_ref(),
                aggregate_effect_delta,
                Some(controls),
            );
            meter = aggregate_effect_delta;
            chain
                .live_source
                .as_ref()
                .expect("effect return live source")
                .apply_meter_frame(
                    controls,
                    1,
                    MixerSourceRouteTransform::default(),
                    &mut meter,
                );
        }

        let mut expected_output = aggregate_effect_delta;
        crate::apply_mixer_pan_raw_f32(&mut expected_output, 6_400.0, Some(2));
        expected_output[0] *= 0.5;
        expected_output[1] *= 0.5;
        MixerSourceRouteTransform::default()
            .scaled(0.5)
            .apply_frame(&mut expected_output);
        for (actual, expected) in output.into_iter().zip(expected_output) {
            assert!((actual - expected).abs() < 0.0001);
        }

        let mut expected_meter = aggregate_effect_delta;
        crate::apply_mixer_pan_raw_f32(&mut expected_meter, 6_400.0, Some(2));
        expected_meter[0] *= 0.5;
        expected_meter[1] *= 0.5;
        for (actual, expected) in meter.into_iter().zip(expected_meter) {
            assert!((actual - expected).abs() < 0.0001);
        }
    }

    #[test]
    fn pattern_loop_event_repeats_get_shifted_frames_and_distinct_note_ids() {
        let channel = MidiChannel::from_index(0).expect("channel 1 is supported");
        let template = [
            ScheduledMidiEvent {
                frame: 12,
                priority: 1,
                event: MidiEvent::NoteOn {
                    channel,
                    note: 60,
                    velocity: 100,
                },
                fine_pitch_offset_cents: 0,
                note_id: 7,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: 64,
                layer_gain: 1.0,
                layer_pan_raw: 0,
                performance_track_id: None,
            },
            ScheduledMidiEvent {
                frame: 40,
                priority: 0,
                event: MidiEvent::NoteOff {
                    channel,
                    note: 60,
                    velocity: 0,
                },
                fine_pitch_offset_cents: 0,
                note_id: 7,
                note_pan: 64,
                note_mod_x: 128,
                note_mod_y: 128,
                note_release: 64,
                layer_gain: 1.0,
                layer_pan_raw: 0,
                performance_track_id: None,
            },
        ];

        let mut queue = PatternLoopEventQueue::new(0, 384, template.to_vec());
        queue
            .repeat_cycle(384)
            .expect("the next Pattern iteration should schedule");
        assert_eq!(queue.repeated_events.len(), 2);
        assert_eq!(queue.repeated_events[0].frame, 396);
        assert_eq!(queue.repeated_events[1].frame, 424);
        assert_ne!(queue.repeated_events[0].note_id, template[0].note_id);
        assert_eq!(
            queue.repeated_events[0].note_id,
            queue.repeated_events[1].note_id
        );
        assert!(queue.repeat_cycle(u64::MAX).is_err());
    }

    #[test]
    fn parameter_automation_uses_pattern_beats_and_emits_block_offsets() {
        let lanes = [Vst3ParameterAutomation {
            parameter_id: 17,
            points: vec![
                Vst3AutomationPoint {
                    position_beats: 0.0,
                    value: 0.0,
                    tension: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 0.02,
                    value: 1.0,
                    tension: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 0.04,
                    value: 0.0,
                    tension: 0.0,
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
                    tension: 0.0,
                    performance_track_id: None,
                },
                ScheduledParameterPoint {
                    frame: 480,
                    value: 1.0,
                    tension: 0.0,
                    performance_track_id: None,
                },
                ScheduledParameterPoint {
                    frame: 960,
                    value: 0.0,
                    tension: 0.0,
                    performance_track_id: None,
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
    fn playlist_pattern_automation_repeats_to_clip_end_and_uses_absolute_ticks() {
        let document = playlist_parameter_automation_fixture();
        let automation = BTreeMap::from([(
            (7, 4),
            vec![Vst3ParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3AutomationPoint {
                        position_beats: 0.25,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 0.5,
                        value: 0.8,
                        tension: 0.0,
                    },
                ],
            }],
        )]);
        let expanded = expand_playlist_parameter_automation(
            &document,
            PlaylistRenderOptions {
                arrangement_id: 3,
                ..PlaylistRenderOptions::default()
            },
            &automation,
            &BTreeMap::new(),
        )
        .expect("Pattern automation should expand over the Playlist clip");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[&4].len(), 1);
        assert_eq!(
            expanded[&4][0].points,
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 24,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 48,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 120,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 144,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 216,
                    value: 0.2,
                    tension: 0.0,
                },
            ]
        );
    }

    #[test]
    fn automation_clip_parameter_points_are_clipped_and_use_project_ticks() {
        let document = playlist_parameter_automation_fixture();
        let mut clip = document
            .arrangements()
            .expect("arrangements should decode")
            .into_iter()
            .find(|arrangement| arrangement.id == 3)
            .and_then(|arrangement| arrangement.clips.first().cloned())
            .expect("fixture clip should exist");
        clip.length_ticks = 192;
        clip.scale = None;
        let automation = BTreeMap::from([(
            (11, 4),
            vec![Vst3ParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3AutomationPoint {
                        position_beats: 0.25,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 3.0,
                        value: 0.8,
                        tension: 0.0,
                    },
                ],
            }],
        )]);

        let expanded =
            expand_automation_clip_parameter_automation_for_clip(11, &clip, 96, &automation)
                .expect("Automation Clip points should expand");
        assert_eq!(
            expanded[&4][0].points,
            vec![Vst3PlaylistAutomationPoint {
                tick: 24,
                value: 0.2,
                tension: 0.0,
            }]
        );
    }

    #[test]
    fn automation_clip_source_windows_map_curve_boundaries_for_instrument_and_effects() {
        let document = playlist_parameter_automation_fixture();
        let mut clip = document
            .arrangements()
            .expect("arrangements should decode")
            .into_iter()
            .find(|arrangement| arrangement.id == 3)
            .and_then(|arrangement| arrangement.clips.first().cloned())
            .expect("fixture clip should exist");
        clip.length_ticks = 192;
        clip.record_size = 88;
        clip.source_window_ticks = Some((48, 240));
        clip.scale = None;
        let lane = Vst3ParameterAutomation {
            parameter_id: 81,
            points: vec![
                Vst3AutomationPoint {
                    position_beats: 0.25,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 1.0,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 3.0,
                    value: 0.3,
                    tension: 0.0,
                },
            ],
        };

        let instrument = expand_automation_clip_parameter_automation_for_clip(
            11,
            &clip,
            96,
            &BTreeMap::from([((11, 4), vec![lane.clone()])]),
        )
        .expect("Automation Clip source window should expand for instruments");
        let instrument_points = &instrument[&4][0].points;
        assert_eq!(
            instrument_points
                .iter()
                .map(|point| point.tick)
                .collect::<Vec<_>>(),
            vec![0, 48, 192]
        );
        assert!((instrument_points[0].value - 0.4).abs() < 1e-12);
        assert!((instrument_points[1].value - 0.8).abs() < 1e-12);
        assert!((instrument_points[2].value - 0.425).abs() < 1e-12);

        let effect = expand_mixer_effect_automation_clip_parameter_automation_for_clip(
            11,
            &clip,
            96,
            &BTreeMap::from([((11, 1, 2), vec![lane])]),
        )
        .expect("Automation Clip source window should expand for Mixer effects");
        let effect_points = &effect[&(1, 2)][0].points;
        assert_eq!(
            effect_points
                .iter()
                .map(|point| point.tick)
                .collect::<Vec<_>>(),
            vec![0, 48, 192]
        );
        assert!((effect_points[0].value - 0.4).abs() < 1e-12);
        assert!((effect_points[1].value - 0.8).abs() < 1e-12);
        assert!((effect_points[2].value - 0.425).abs() < 1e-12);
    }

    #[test]
    fn playlist_automation_clip_parameter_automation_uses_absolute_ticks_and_clip_length() {
        let document = playlist_parameter_automation_fixture();
        let automation = BTreeMap::from([(
            (11, 4),
            vec![Vst3ParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3AutomationPoint {
                        position_beats: 0.25,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 1.0,
                        value: 0.8,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 3.0,
                        value: 0.3,
                        tension: 0.0,
                    },
                ],
            }],
        )]);
        let expanded = expand_playlist_parameter_automation(
            &document,
            PlaylistRenderOptions {
                arrangement_id: 3,
                ..PlaylistRenderOptions::default()
            },
            &BTreeMap::new(),
            &automation,
        )
        .expect("Automation Clip parameter automation should expand");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[&4].len(), 1);
        assert_eq!(
            expanded[&4][0].points,
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 408,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 480,
                    value: 0.8,
                    tension: 0.0,
                },
            ]
        );
    }

    #[test]
    fn playlist_automation_clip_parameter_automation_targets_mixer_effects() {
        let document = playlist_parameter_automation_fixture();
        let automation = BTreeMap::from([(
            (11, 1, 2),
            vec![Vst3ParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3AutomationPoint {
                        position_beats: 0.25,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 1.0,
                        value: 0.8,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 3.0,
                        value: 0.3,
                        tension: 0.0,
                    },
                ],
            }],
        )]);
        let expanded = expand_playlist_mixer_effect_parameter_automation(
            &document,
            PlaylistRenderOptions {
                arrangement_id: 3,
                ..PlaylistRenderOptions::default()
            },
            &BTreeMap::new(),
            &automation,
        )
        .expect("Automation Clip parameter automation should target the Mixer effect");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[&(1, 2)].len(), 1);
        assert_eq!(
            expanded[&(1, 2)][0].points,
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 408,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 480,
                    value: 0.8,
                    tension: 0.0,
                },
            ]
        );
    }

    #[test]
    fn pattern_event_automation_repeats_over_mixer_effect_playlist_clips() {
        let document = playlist_parameter_automation_fixture();
        let automation = BTreeMap::from([(
            (7, 1, 2),
            vec![Vst3PlaylistParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3PlaylistAutomationPoint {
                        tick: 24,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3PlaylistAutomationPoint {
                        tick: 48,
                        value: 0.8,
                        tension: 0.0,
                    },
                ],
            }],
        )]);
        let expanded = expand_playlist_mixer_effect_parameter_automation(
            &document,
            PlaylistRenderOptions {
                arrangement_id: 3,
                ..PlaylistRenderOptions::default()
            },
            &automation,
            &BTreeMap::new(),
        )
        .expect("Pattern Event automation should expand over Mixer effect clips");

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[&(1, 2)].len(), 1);
        assert_eq!(
            expanded[&(1, 2)][0].points,
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 24,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 48,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 120,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 144,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 216,
                    value: 0.2,
                    tension: 0.0,
                },
            ]
        );
    }

    #[test]
    fn pattern_controller_automation_defaults_to_the_pattern_meter_bar() {
        let document = playlist_parameter_automation_fixture();
        let mut pattern = document
            .patterns()
            .expect("Patterns should decode")
            .into_iter()
            .find(|pattern| pattern.id == 7)
            .expect("fixture Pattern should exist");
        pattern.length_ticks = None;
        pattern.notes.clear();
        let mut clip = document
            .arrangements()
            .expect("arrangements should decode")
            .into_iter()
            .find(|arrangement| arrangement.id == 3)
            .and_then(|arrangement| arrangement.clips.first().cloned())
            .expect("fixture clip should exist");
        clip.length_ticks = 768;
        let automation = BTreeMap::from([(
            (7, 4),
            vec![Vst3ParameterAutomation {
                parameter_id: 81,
                points: vec![
                    Vst3AutomationPoint {
                        position_beats: 0.25,
                        value: 0.2,
                        tension: 0.0,
                    },
                    Vst3AutomationPoint {
                        position_beats: 0.5,
                        value: 0.8,
                        tension: 0.0,
                    },
                ],
            }],
        )]);

        let expanded = expand_pattern_parameter_automation_for_clip(
            7,
            &pattern,
            &clip,
            96,
            (4, 4),
            &automation,
        )
        .expect("default-meter Pattern automation should expand");
        assert_eq!(
            expanded[&4][0].points,
            vec![
                Vst3PlaylistAutomationPoint {
                    tick: 24,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 48,
                    value: 0.8,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 408,
                    value: 0.2,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 432,
                    value: 0.8,
                    tension: 0.0,
                },
            ]
        );
    }

    #[test]
    fn playlist_parameter_automation_uses_the_song_tempo_map() {
        let tempo_map = PlaylistTempoMap::from_tempo_events(96, 120.0, &[(96, 60.0)])
            .expect("tempo changes should build a map");
        let automation = [Vst3PlaylistParameterAutomation {
            parameter_id: 82,
            points: vec![
                Vst3PlaylistAutomationPoint {
                    tick: 96,
                    value: 0.25,
                    tension: 0.0,
                },
                Vst3PlaylistAutomationPoint {
                    tick: 192,
                    value: 0.75,
                    tension: 0.0,
                },
            ],
        }];
        let prepared =
            prepare_playlist_parameter_automation(&automation, &tempo_map, 48_000, 80_000)
                .expect("Playlist points should be tempo mapped");

        assert_eq!(
            prepared[0].points,
            vec![
                ScheduledParameterPoint {
                    frame: 24_000,
                    value: 0.25,
                    tension: 0.0,
                    performance_track_id: None,
                },
                ScheduledParameterPoint {
                    frame: 72_000,
                    value: 0.75,
                    tension: 0.0,
                    performance_track_id: None,
                },
            ]
        );
    }

    #[test]
    fn performance_automation_stop_removes_only_future_points_from_its_track() {
        let mut automation = vec![PreparedParameterAutomation {
            parameter_id: 83,
            points: vec![
                ScheduledParameterPoint {
                    frame: 10,
                    value: 0.1,
                    tension: 0.0,
                    performance_track_id: None,
                },
                ScheduledParameterPoint {
                    frame: 20,
                    value: 0.2,
                    tension: 0.0,
                    performance_track_id: Some(7),
                },
                ScheduledParameterPoint {
                    frame: 30,
                    value: 0.3,
                    tension: 0.0,
                    performance_track_id: Some(8),
                },
                ScheduledParameterPoint {
                    frame: 40,
                    value: 0.4,
                    tension: 0.0,
                    performance_track_id: Some(7),
                },
            ],
        }];

        stop_performance_parameter_automation(&mut automation, 7, 25);
        assert_eq!(
            automation[0].points,
            vec![
                ScheduledParameterPoint {
                    frame: 10,
                    value: 0.1,
                    tension: 0.0,
                    performance_track_id: None,
                },
                ScheduledParameterPoint {
                    frame: 20,
                    value: 0.2,
                    tension: 0.0,
                    performance_track_id: Some(7),
                },
                ScheduledParameterPoint {
                    frame: 30,
                    value: 0.3,
                    tension: 0.0,
                    performance_track_id: Some(8),
                },
            ]
        );

        prune_parameter_automation_before(&mut automation, 25);
        assert_eq!(
            automation[0].points,
            vec![
                ScheduledParameterPoint {
                    frame: 20,
                    value: 0.2,
                    tension: 0.0,
                    performance_track_id: Some(7),
                },
                ScheduledParameterPoint {
                    frame: 30,
                    value: 0.3,
                    tension: 0.0,
                    performance_track_id: Some(8),
                },
            ]
        );
    }

    #[test]
    fn parameter_automation_preserves_future_points_and_collapses_same_frame_points() {
        let future = [Vst3ParameterAutomation {
            parameter_id: 18,
            points: vec![Vst3AutomationPoint {
                position_beats: 0.02,
                value: 0.75,
                tension: 0.0,
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
                    tension: 0.0,
                },
                Vst3AutomationPoint {
                    position_beats: 0.000001,
                    value: 0.9,
                    tension: 0.0,
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
                tension: 0.0,
                performance_track_id: None,
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
                tension: 0.0,
            }],
        }];
        assert!(prepare_parameter_automation(&invalid, 48_000.0, 120.0).is_err());

        let duplicate_ids = [
            Vst3ParameterAutomation {
                parameter_id: 17,
                points: vec![Vst3AutomationPoint {
                    position_beats: 0.0,
                    value: 0.0,
                    tension: 0.0,
                }],
            },
            Vst3ParameterAutomation {
                parameter_id: 17,
                points: vec![Vst3AutomationPoint {
                    position_beats: 1.0,
                    value: 1.0,
                    tension: 0.0,
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
                    tension: 0.0,
                    performance_track_id: None,
                }],
            },
            PreparedParameterAutomation {
                parameter_id: 2,
                points: (1..=MAX_VST3_AUTOMATION_CHANGES_PER_BLOCK)
                    .map(|frame| ScheduledParameterPoint {
                        frame: frame as u64,
                        value: 0.5,
                        tension: 0.0,
                        performance_track_id: None,
                    })
                    .collect(),
            },
        ];
        let error = validate_parameter_automation_block_density(&automation, 4096, 4097)
            .expect_err("dense block should be rejected");
        assert!(error.contains("parameter changes in one block"));
    }

    #[test]
    fn automatic_pdc_delays_each_stream_to_the_longest_reported_latency() {
        let (maximum, delays) = plan_pdc_delays(&[0, 480, 240]);
        assert_eq!(maximum, 480);
        assert_eq!(delays, [480, 0, 240]);
        assert_eq!(plan_pdc_delays(&[]), (0, Vec::new()));
    }

    #[test]
    fn pdc_render_start_trim_covers_instrument_and_mixer_latency_only_when_enabled() {
        assert_eq!(pdc_render_start_trim_frames(true, 480, 240), 720);
        assert_eq!(pdc_render_start_trim_frames(false, 480, 240), 0);
    }

    #[test]
    fn mixer_stem_pdc_trim_uses_each_tracks_input_and_post_effect_alignment() {
        assert_eq!(pdc_mixer_stem_start_trim_frames(true, 480, 96, 48), 624);
        assert_eq!(pdc_mixer_stem_start_trim_frames(false, 480, 96, 48), 0);
    }

    #[test]
    fn pdc_extends_the_song_tail_and_checks_length_overflow() {
        assert_eq!(pdc_aligned_output_frames(48_000, 480).unwrap(), 48_480);
        assert!(pdc_aligned_output_frames(u64::MAX, 1).is_err());
    }

    #[test]
    fn plugin_latency_is_converted_to_output_rate_frames() {
        assert_eq!(
            plugin_latency_in_output_frames(7, 480, 48_000.0, 44_100)
                .expect("valid sample rates should convert"),
            441
        );
        assert_eq!(
            plugin_latency_in_output_frames(7, 1, 48_000.0, 44_100)
                .expect("fractional frames round up"),
            1
        );
        assert!(plugin_latency_in_output_frames(7, 1, f64::NAN, 44_100).is_err());
    }

    #[test]
    fn manual_plugin_latency_offsets_adjust_pdc_but_preserve_reported_latency() {
        let mut info = HostedPluginInfo {
            id: 1,
            name: "Test effect".to_owned(),
            vendor: "Test vendor".to_owned(),
            version: "1.0".to_owned(),
            category: "Fx|Delay".to_owned(),
            uid: "00000000000000000000000000000000".to_owned(),
            path: PathBuf::from("test.vst3"),
            has_editor: false,
            audio_inputs: 2,
            audio_outputs: 2,
            latency_samples: 480,
            manual_latency_offset_samples: -120,
            sample_rate_hz: 48_000.0,
        };

        assert_eq!(info.latency_samples, 480);
        assert_eq!(info.pdc_latency_samples(), 360);
        info.manual_latency_offset_samples = 96;
        assert_eq!(info.pdc_latency_samples(), 576);
        info.manual_latency_offset_samples = i32::MIN;
        assert_eq!(info.pdc_latency_samples(), 0);
        assert_eq!(apply_manual_latency_offset(u32::MAX, i32::MAX), u32::MAX);
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
            audio_inputs: 0,
            audio_outputs: 2,
            latency_samples: 480,
            manual_latency_offset_samples: 0,
            sample_rate_hz: 48_000.0,
        };

        assert_eq!(info.latency_milliseconds(), Some(10.0));
    }

    #[test]
    fn effect_audio_blocks_validate_rate_size_and_channel_shape() {
        let layout = AudioBusLayout {
            inputs: vec![vst3_host::audio::AudioBusConfig {
                channel_count: 2,
                active: true,
            }],
            outputs: vec![vst3_host::audio::AudioBusConfig {
                channel_count: 2,
                active: true,
            }],
        };
        let mut buffers = BusAudioBuffers::new(&layout, 64, 48_000.0);
        assert!(validate_vst3_audio_block(&buffers, 48_000.0, 512).is_ok());

        assert!(
            validate_vst3_audio_block(&buffers, 44_100.0, 512)
                .expect_err("different plugin rate should fail")
                .contains("does not match")
        );
        assert!(
            validate_vst3_audio_block(&buffers, 48_000.0, 32)
                .expect_err("oversized block should fail")
                .contains("at most 32")
        );

        buffers.outputs[0].channels[1].pop();
        assert!(
            validate_vst3_audio_block(&buffers, 48_000.0, 512)
                .expect_err("mismatched channel length should fail")
                .contains("must match the block frame count")
        );
    }

    #[test]
    fn effect_audio_block_resize_clears_all_buses_and_sets_variable_block_size() {
        let layout = AudioBusLayout {
            inputs: vec![vst3_host::audio::AudioBusConfig {
                channel_count: 2,
                active: true,
            }],
            outputs: vec![vst3_host::audio::AudioBusConfig {
                channel_count: 1,
                active: true,
            }],
        };
        let mut buffers = BusAudioBuffers::new(&layout, 64, 48_000.0);
        buffers.inputs[0].channels[0].fill(0.5);
        buffers.outputs[0].channels[0].fill(1.0);
        resize_vst3_bus_audio_buffers(&mut buffers, 17);

        assert_eq!(buffers.block_size, 17);
        assert_eq!(buffers.inputs[0].channels[0], vec![0.0; 17]);
        assert_eq!(buffers.inputs[0].channels[1], vec![0.0; 17]);
        assert_eq!(buffers.outputs[0].channels[0], vec![0.0; 17]);
        assert!(validate_vst3_audio_block(&buffers, 48_000.0, 512).is_ok());
    }

    #[test]
    fn mixer_effect_sidechain_inputs_receive_post_effect_source_buses() {
        let layout = AudioBusLayout {
            inputs: vec![
                vst3_host::audio::AudioBusConfig {
                    channel_count: 2,
                    active: true,
                },
                vst3_host::audio::AudioBusConfig {
                    channel_count: 2,
                    active: true,
                },
                vst3_host::audio::AudioBusConfig {
                    channel_count: 1,
                    active: true,
                },
            ],
            outputs: vec![vst3_host::audio::AudioBusConfig {
                channel_count: 2,
                active: true,
            }],
        };
        let mut buffers = BusAudioBuffers::new(&layout, 2, 48_000.0);
        let mut sidechain_inputs = vec![
            PlaylistMixerEffectSidechainInput {
                bus_index: 1,
                source_track: 1,
                channel_count: 2,
                saved_enabled: false,
                live_control: true,
                live_enabled_target: Some(Arc::new(AtomicI32::new(1))),
                enabled_ramp: LiveWetMixRamp::new(0.0, 1),
                delay: StereoFrameDelay::default(),
                stereo_scratch: vec![0.0; 4],
            },
            PlaylistMixerEffectSidechainInput {
                bus_index: 2,
                source_track: 0,
                channel_count: 1,
                saved_enabled: true,
                live_control: false,
                live_enabled_target: None,
                enabled_ramp: LiveWetMixRamp::new(1.0, 1),
                delay: StereoFrameDelay::default(),
                stereo_scratch: vec![0.0; 4],
            },
        ];
        let mut post_effect_buses = vec![0.0; crate::audio::MIXER_TRACK_METER_COUNT * 4];
        post_effect_buses[..4].copy_from_slice(&[1.0, 3.0, 5.0, 7.0]);
        post_effect_buses[4..8].copy_from_slice(&[0.1, 0.2, 0.3, 0.4]);

        fill_playlist_mixer_sidechain_inputs(
            &mut sidechain_inputs,
            &mut buffers.inputs,
            Some(&post_effect_buses),
            4,
            0,
            2,
        )
        .expect("sidechain buses should receive their source signals");
        assert_eq!(buffers.inputs[1].channels[0], vec![0.1, 0.3]);
        assert_eq!(buffers.inputs[1].channels[1], vec![0.2, 0.4]);
        assert_eq!(buffers.inputs[2].channels[0], vec![2.0, 6.0]);

        resize_vst3_bus_audio_buffers(&mut buffers, 1);
        sidechain_inputs[0]
            .live_enabled_target
            .as_ref()
            .expect("live sidechain route should have an enabled target")
            .store(0, Ordering::Release);
        fill_playlist_mixer_sidechain_inputs(
            &mut sidechain_inputs,
            &mut buffers.inputs,
            Some(&post_effect_buses),
            4,
            1,
            1,
        )
        .expect("sidechain offsets should follow variable-size processing blocks");
        assert_eq!(buffers.inputs[1].channels[0], vec![0.0]);
        assert_eq!(buffers.inputs[1].channels[1], vec![0.0]);
        assert_eq!(buffers.inputs[2].channels[0], vec![6.0]);

        resize_vst3_bus_audio_buffers(&mut buffers, 1);
        sidechain_inputs[0]
            .live_enabled_target
            .as_ref()
            .expect("live sidechain route should have an enabled target")
            .store(1, Ordering::Release);
        fill_playlist_mixer_sidechain_inputs(
            &mut sidechain_inputs,
            &mut buffers.inputs,
            Some(&post_effect_buses),
            4,
            1,
            1,
        )
        .expect("live sidechain enable changes should reach the auxiliary bus");
        assert_eq!(buffers.inputs[1].channels[0], vec![0.3]);
        assert_eq!(buffers.inputs[1].channels[1], vec![0.4]);
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
            audio_inputs: 0,
            audio_outputs: 2,
            latency_samples: 480,
            manual_latency_offset_samples: 0,
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
    fn pattern_scheduler_carries_fine_pitch_to_both_note_events() {
        let note = PatternNote {
            position: 0,
            length: 120,
            channel_id: 7,
            key: 64,
            velocity: 100,
            fine_pitch: 130,
            ..PatternNote::default()
        };
        let (events, _) =
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0, 0, 128).unwrap();

        assert_eq!(events[0].fine_pitch_offset_cents, 100);
        assert_eq!(events[1].fine_pitch_offset_cents, 100);
        for event in &events {
            let total_pitch = combined_pitch_offset_cents(375, event.fine_pitch_offset_cents);
            let plugin_event = plugin_event_with_pitch_offset(event.event, 0, total_pitch).unwrap();
            assert!(matches!(
                plugin_event.data,
                PluginEventData::NoteOn { tuning, .. }
                    | PluginEventData::NoteOff { tuning, .. }
                    if tuning == 475.0
            ));
        }
    }

    #[test]
    fn hosted_vst3_channel_fine_tune_combines_with_master_and_note_pitch() {
        let channel_pitch = add_channel_fine_tune_cents(375, Some(35));
        assert_eq!(channel_pitch, 410);
        assert_eq!(add_channel_fine_tune_cents(375, None), 375);
        assert_eq!(add_channel_fine_tune_cents(1200, Some(100)), 1300);
        assert_eq!(add_channel_fine_tune_cents(-1200, Some(-100)), -1300);
        assert_eq!(add_channel_fine_tune_cents(i16::MAX, Some(100)), i16::MAX);

        let note_pitch = combined_pitch_offset_cents(channel_pitch, 100);
        let plugin_event = plugin_event_with_pitch_offset(
            MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 60,
                velocity: 100,
            },
            0,
            note_pitch,
        )
        .unwrap();
        assert!(matches!(
            plugin_event.data,
            PluginEventData::NoteOn { tuning, .. } if tuning == 510.0
        ));
    }

    #[test]
    fn pattern_scheduler_carries_note_pan_and_pair_note_id() {
        let note = PatternNote {
            position: 0,
            length: 120,
            channel_id: 7,
            key: 64,
            velocity: 100,
            pan: 17,
            ..PatternNote::default()
        };
        let (events, _) =
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0, 0, 128).unwrap();

        assert_eq!(events[0].note_pan, 17);
        assert_eq!(events[1].note_pan, 17);
        assert_eq!(events[0].note_id, events[1].note_id);
    }

    #[test]
    fn pattern_scheduler_carries_layer_adjustments_to_note_events() {
        let note = PatternNote {
            position: 0,
            length: 120,
            channel_id: 7,
            key: 64,
            ..PatternNote::default()
        };
        let (events, _) = scheduled_pattern_events_with_layer_adjustments(
            &[note],
            PatternNoteScheduleOptions {
                channel_id: 7,
                ppq: 480,
                tempo_bpm: 120.0,
                sample_rate: 48_000.0,
                global_swing_mix_raw: 0,
                channel_swing_mix_raw: 128,
            },
            Some(&[(0.75, 1_600)]),
        )
        .unwrap();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| {
            (event.layer_gain - 0.75).abs() < f32::EPSILON && event.layer_pan_raw == 1_600
        }));
    }

    #[test]
    fn pattern_scheduler_carries_note_mod_y_to_note_off() {
        let note = PatternNote {
            position: 0,
            length: 120,
            channel_id: 7,
            key: 64,
            velocity: 100,
            mod_y: 231,
            ..PatternNote::default()
        };
        let (events, _) =
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0, 0, 128).unwrap();

        assert_eq!(events[0].note_mod_y, 231);
        assert_eq!(events[1].note_mod_y, 231);
    }

    #[test]
    fn pattern_scheduler_carries_note_release_to_note_off() {
        let note = PatternNote {
            position: 0,
            length: 120,
            channel_id: 7,
            key: 64,
            release: 110,
            ..PatternNote::default()
        };
        let (events, _) =
            scheduled_pattern_events(&[note], 7, 480, 120.0, 48_000.0, 0, 128).unwrap();

        assert_eq!(events[0].note_release, 110);
        assert_eq!(events[1].note_release, 110);
        assert!(matches!(events[1].event, MidiEvent::NoteOff { .. }));
    }

    #[test]
    fn vst3_note_pan_maps_fl_values_to_normalized_expression_values() {
        assert_eq!(vst3_note_pan_value(0), 0.0);
        assert_eq!(vst3_note_pan_value(64), 0.5);
        assert_eq!(vst3_note_pan_value(128), 1.0);
        assert_eq!(vst3_note_pan_value(255), 1.0);
        assert_eq!(vst3_note_pan_with_layer_offset(64, -6_400), 0.0);
        assert_eq!(vst3_note_pan_with_layer_offset(64, 0), 0.5);
        assert_eq!(vst3_note_pan_with_layer_offset(64, 6_400), 1.0);
        assert_eq!(vst3_note_pan_with_layer_offset(0, -6_400), 0.0);
        assert_eq!(vst3_note_pan_with_layer_offset(128, 6_400), 1.0);
    }

    #[test]
    fn vst3_layer_gain_maps_to_standard_volume_expression_values() {
        assert_eq!(vst3_note_volume_value(0.0), 0.0);
        assert_eq!(vst3_note_volume_value(1.0), 0.25);
        assert_eq!(vst3_note_volume_value(2.0), 0.5);
        assert_eq!(vst3_note_volume_value(4.0), 1.0);
        assert_eq!(vst3_note_volume_value(8.0), 1.0);
    }

    #[test]
    fn playlist_live_layer_adjustments_keep_note_id_and_controls_paired() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let live_sender = Vst3PlaylistLiveInputSender {
            sender,
            next_generation: Arc::new(AtomicU64::new(0)),
            stopped_generation: Arc::new(AtomicU64::new(0)),
        };
        let session = live_sender.begin_session();
        let channel = MidiChannel::from_index(0).expect("MIDI channel 1 should exist");
        session
            .try_send_with_layer_adjustments(
                MidiEvent::NoteOn {
                    channel,
                    note: 60,
                    velocity: 100,
                },
                0.75,
                1_600,
            )
            .expect("Layer-adjusted note-on should queue");
        let note_on = receiver.try_recv().expect("note-on should be queued");
        assert_eq!(note_on.layer_gain, 0.75);
        assert_eq!(note_on.layer_pan_raw, 1_600);
        assert!(note_on.note_id > 0);

        session
            .try_send(MidiEvent::NoteOff {
                channel,
                note: 60,
                velocity: 32,
            })
            .expect("note-off should queue");
        let note_off = receiver.try_recv().expect("note-off should be queued");
        assert_eq!(note_off.note_id, note_on.note_id);
        assert_eq!(note_off.layer_gain, note_on.layer_gain);
        assert_eq!(note_off.layer_pan_raw, note_on.layer_pan_raw);
    }

    #[test]
    fn vst3_note_release_maps_fl_values_to_normalized_velocity() {
        assert_eq!(vst3_note_release_value(0), 0.0);
        assert_eq!(vst3_note_release_value(64), 0.5);
        assert_eq!(vst3_note_release_value(128), 1.0);
        assert_eq!(vst3_note_release_value(255), 1.0);
    }

    #[test]
    fn vst3_note_pan_requires_an_advertised_pan_dimension() {
        let expression = |kind| NoteExpressionInfo {
            kind,
            title: String::new(),
            short_title: String::new(),
            units: String::new(),
            default_value: 0.5,
            min: 0.0,
            max: 1.0,
            step_count: 0,
            is_bipolar: true,
            is_one_shot: false,
            is_absolute: true,
        };

        assert!(!note_expressions_support_pan(&[]));
        assert!(!note_expressions_support_pan(&[expression(
            NoteExpressionType::Volume
        )]));
        assert!(note_expressions_support_volume(&[expression(
            NoteExpressionType::Volume
        )]));
        assert!(!note_expressions_support_volume(&[expression(
            NoteExpressionType::Pan
        )]));
        assert!(note_expressions_support_pan(&[expression(
            NoteExpressionType::Pan
        )]));
    }

    #[test]
    fn vst3_mod_y_uses_a_named_custom_timbre_or_resonance_dimension() {
        let expression = |id, title: &str, short_title: &str, min, max| NoteExpressionInfo {
            kind: NoteExpressionType::Custom(id),
            title: title.to_owned(),
            short_title: short_title.to_owned(),
            units: String::new(),
            default_value: (min + max) * 0.5,
            min,
            max,
            step_count: 0,
            is_bipolar: false,
            is_one_shot: false,
            is_absolute: true,
        };
        let unrelated = expression(100_010, "Position", "Pos", 0.0, 1.0);
        let timbre = expression(100_011, "Timbre", "Tone", 0.0, 1.0);
        let resonance = expression(100_012, "Resonance", "Res", 0.2, 0.8);
        let standard_brightness = NoteExpressionInfo {
            kind: NoteExpressionType::Brightness,
            title: "Brightness".to_owned(),
            short_title: "Bright".to_owned(),
            units: String::new(),
            default_value: 0.5,
            min: 0.0,
            max: 1.0,
            step_count: 0,
            is_bipolar: false,
            is_one_shot: false,
            is_absolute: true,
        };

        assert_eq!(note_expressions_support_mod_y(&[unrelated]), None);
        assert_eq!(note_expressions_support_mod_y(&[standard_brightness]), None);
        let mapped = note_expressions_support_mod_y(&[timbre, resonance]).unwrap();
        assert_eq!(
            mapped,
            Vst3ModYExpression {
                kind: NoteExpressionType::Custom(100_012),
                min: 0.2,
                max: 0.8,
            }
        );
        assert!((vst3_note_mod_y_value(0, mapped) - 0.2).abs() < 1e-12);
        assert!((vst3_note_mod_y_value(255, mapped) - 0.8).abs() < 1e-12);
    }

    #[test]
    fn applies_master_pitch_as_vst3_note_tuning() {
        let note_on = plugin_event_with_pitch_offset(
            MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 64,
                velocity: 100,
            },
            17,
            375,
        )
        .unwrap();
        assert_eq!(note_on.sample_offset, 17);
        assert!(matches!(
            note_on.data,
            PluginEventData::NoteOn {
                pitch: 64,
                tuning,
                ..
            } if tuning == 375.0
        ));

        let note_off = plugin_event_with_pitch_offset(
            MidiEvent::NoteOff {
                channel: MidiChannel::Ch1,
                note: 64,
                velocity: 0,
            },
            29,
            375,
        )
        .unwrap();
        assert_eq!(note_off.sample_offset, 29);
        assert!(matches!(
            note_off.data,
            PluginEventData::NoteOff {
                pitch: 64,
                tuning,
                ..
            } if tuning == 375.0
        ));
    }

    #[test]
    fn note_expression_events_preserve_note_id_and_fine_pitch() {
        let note_on = plugin_event_with_pitch_offset_and_note_id(
            MidiEvent::NoteOn {
                channel: MidiChannel::Ch1,
                note: 64,
                velocity: 100,
            },
            17,
            375,
            1_073_741_824,
        )
        .unwrap();
        assert_eq!(note_on.sample_offset, 17);
        assert!(matches!(
            note_on.data,
            PluginEventData::NoteOn {
                pitch: 64,
                tuning,
                note_id: 1_073_741_824,
                ..
            } if tuning == 375.0
        ));

        let note_off = plugin_event_with_pitch_offset_and_note_id(
            MidiEvent::NoteOff {
                channel: MidiChannel::Ch1,
                note: 64,
                velocity: 0,
            },
            29,
            375,
            1_073_741_824,
        )
        .unwrap();
        assert_eq!(note_off.sample_offset, 29);
        assert!(matches!(
            note_off.data,
            PluginEventData::NoteOff {
                pitch: 64,
                tuning,
                note_id: 1_073_741_824,
                ..
            } if tuning == 375.0
        ));
    }

    #[test]
    fn note_off_event_uses_fl_release_velocity() {
        let mut note_off = plugin_event_with_pitch_offset(
            MidiEvent::NoteOff {
                channel: MidiChannel::Ch1,
                note: 64,
                velocity: 0,
            },
            29,
            0,
        )
        .unwrap();
        set_vst3_note_off_release_velocity(&mut note_off, 96).unwrap();

        assert!(matches!(
            note_off.data,
            PluginEventData::NoteOff {
                velocity,
                tuning,
                ..
            } if velocity == 0.75 && tuning == 0.0
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
                mod_y: 31,
                start_tick: 480,
                stop_tick: 720,
                key: 64,
                velocity: 96,
                midi_channel: 0,
                fine_pitch_offset_cents: 0,
                pan: 0,
                mod_x: 128,
                release: 89,
                layer_gain: 1.0,
                layer_pan_raw: 0,
            },
            PlaylistMidiNote {
                mod_y: 219,
                start_tick: 720,
                stop_tick: 960,
                key: 64,
                velocity: 80,
                midi_channel: 0,
                fine_pitch_offset_cents: 0,
                pan: 128,
                mod_x: 128,
                release: 21,
                layer_gain: 1.0,
                layer_pan_raw: 0,
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
        assert_eq!(
            events
                .iter()
                .map(|event| event.note_mod_y)
                .collect::<Vec<_>>(),
            [31, 31, 219, 219]
        );
        assert_eq!(events[3].frame, 48_000);
        assert!(matches!(
            events[3].event,
            MidiEvent::NoteOff { note: 64, .. }
        ));
        assert_eq!(events[0].note_pan, 0);
        assert_eq!(events[0].note_id, events[1].note_id);
        assert_eq!(events[1].note_release, 89);
        assert_eq!(events[2].note_pan, 128);
        assert_eq!(events[2].note_id, events[3].note_id);
        assert_eq!(events[3].note_release, 21);
    }

    #[test]
    fn playlist_scheduler_carries_layer_adjustments_to_note_events() {
        let note = PlaylistMidiNote {
            start_tick: 480,
            stop_tick: 720,
            key: 64,
            velocity: 96,
            midi_channel: 0,
            fine_pitch_offset_cents: 0,
            pan: 64,
            mod_x: 128,
            mod_y: 128,
            release: 64,
            layer_gain: 0.8,
            layer_pan_raw: -2_400,
        };
        let (events, _) = scheduled_playlist_midi_events(&[note], 480, 120.0, 48_000.0).unwrap();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| {
            (event.layer_gain - 0.8).abs() < f32::EPSILON && event.layer_pan_raw == -2_400
        }));
    }

    #[test]
    fn playlist_note_transposition_applies_root_offset_to_note_on_and_off() {
        let note = PlaylistMidiNote {
            start_tick: 0,
            stop_tick: 240,
            key: 60,
            velocity: 96,
            midi_channel: 0,
            fine_pitch_offset_cents: 0,
            pan: 64,
            mod_x: 128,
            mod_y: 128,
            release: 64,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        };
        let (mut events, note_count) =
            scheduled_playlist_midi_events(&[note], 480, 120.0, 48_000.0).unwrap();

        transpose_scheduled_note_events(&mut events, -4);

        assert_eq!(note_count, 1);
        assert!(matches!(
            events[0].event,
            MidiEvent::NoteOn { note: 56, .. }
        ));
        assert!(matches!(
            events[1].event,
            MidiEvent::NoteOff { note: 56, .. }
        ));
    }

    #[test]
    fn playlist_vst3_notes_follow_the_shared_tempo_map() {
        let tempo_map = PlaylistTempoMap::from_tempo_events(480, 120.0, &[(480, 60.0)]).unwrap();
        let note = PlaylistMidiNote {
            mod_y: 128,
            start_tick: 480,
            stop_tick: 720,
            key: 64,
            velocity: 96,
            midi_channel: 0,
            fine_pitch_offset_cents: 0,
            pan: 64,
            mod_x: 128,
            release: 64,
            layer_gain: 1.0,
            layer_pan_raw: 0,
        };

        let (events, note_count) =
            scheduled_playlist_midi_events_with_tempo_map(&[note], &tempo_map, 48_000.0).unwrap();

        assert_eq!(note_count, 1);
        assert_eq!(events[0].frame, 24_000);
        assert_eq!(events[1].frame, 48_000);
    }

    #[test]
    fn playlist_midi_scheduler_rejects_invalid_channel_and_timing() {
        let note = PlaylistMidiNote {
            start_tick: 0,
            stop_tick: 1,
            key: 60,
            velocity: 100,
            midi_channel: 16,
            fine_pitch_offset_cents: 0,
            pan: 64,
            mod_x: 128,
            mod_y: 128,
            release: 64,
            layer_gain: 1.0,
            layer_pan_raw: 0,
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
    fn preview_mixes_active_mono_and_stereo_output_buses_in_order() {
        let mut stereo = Vec::new();
        append_vst_output_buses_to_stereo(
            &mut stereo,
            &[0.25, -0.5, 0.125, -0.75, 1.0, 0.25],
            &[2, 1],
        )
        .unwrap();
        assert_eq!(stereo, [0.375, -0.375, -0.5, 1.25]);

        assert!(append_vst_output_buses_to_stereo(&mut stereo, &[0.0; 4], &[4]).is_err());
        assert!(append_vst_output_buses_to_stereo(&mut stereo, &[0.0], &[]).is_err());
    }

    #[test]
    fn pattern_render_mix_sums_stereo_buffers_and_pads_short_channels() {
        let mut mixed = Vec::new();
        let mut frames = 0;
        mix_stereo_pattern_render_buffer(&mut mixed, &mut frames, &[0.2, 0.3], 1).unwrap();
        mix_stereo_pattern_render_buffer(&mut mixed, &mut frames, &[0.4, -0.1, 0.5, 0.25], 2)
            .unwrap();
        assert_eq!(frames, 2);
        assert_eq!(mixed, [0.6, 0.20000002, 0.5, 0.25]);
    }

    #[test]
    fn pattern_render_mix_rejects_bad_shapes_and_sanitizes_non_finite_samples() {
        let mut mixed = Vec::new();
        let mut frames = 0;
        assert!(mix_stereo_pattern_render_buffer(&mut mixed, &mut frames, &[1.0], 1).is_err());
        mix_stereo_pattern_render_buffer(&mut mixed, &mut frames, &[f32::NAN, f32::INFINITY], 1)
            .unwrap();
        assert_eq!(frames, 1);
        assert_eq!(mixed, [0.0, 0.0]);
    }

    #[test]
    fn pattern_render_applies_supported_mixer_signal_transforms() {
        let transform = MixerSourceRouteTransform::from_insert_transform(
            MixerInsertSignalTransform::new(true, true).with_fader_volume_raw(Some(5_000)),
        );
        let mut stereo = [0.25, 0.5, 1.0, -1.0];
        apply_stereo_mixer_transform(&mut stereo, transform).unwrap();
        assert!((stereo[0] + 0.25).abs() < f32::EPSILON);
        assert!((stereo[1] + 0.125).abs() < f32::EPSILON);
        assert!((stereo[2] - 0.5).abs() < f32::EPSILON);
        assert!((stereo[3] + 0.5).abs() < f32::EPSILON);
        assert!(apply_stereo_mixer_transform(&mut [0.0], transform).is_err());
    }

    #[test]
    fn pattern_render_routes_each_vst3_output_bus_independently() {
        let transforms = [
            MixerSourceRouteTransform::default(),
            MixerSourceRouteTransform::from_insert_transform(MixerInsertSignalTransform::new(
                true, false,
            )),
        ];
        let mut stereo = Vec::new();
        append_vst_output_buses_to_stereo_with_mixer_transforms(
            &mut stereo,
            &[0.25, 0.5, 0.5, 0.25],
            &[2, 2],
            Some(&transforms),
        )
        .unwrap();
        assert_eq!(stereo, [-0.25, 0.25]);
        assert!(
            append_vst_output_buses_to_stereo_with_mixer_transforms(
                &mut stereo,
                &[0.0; 4],
                &[2, 2],
                Some(&transforms[..1]),
            )
            .is_err()
        );
    }

    #[test]
    fn flp_vst3_output_mixer_offsets_read_supported_wrapper_records() {
        fn append_field(payload: &mut Vec<u8>, id: u32, data: &[u8]) {
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
            payload.extend_from_slice(data);
        }

        let mut payload = 12_u32.to_le_bytes().to_vec();
        let mut io_data = Vec::new();
        io_data.extend_from_slice(&0_u32.to_le_bytes());
        io_data.extend_from_slice(&2_u32.to_le_bytes());
        append_field(&mut payload, 30, &io_data);
        let mut output_data = Vec::new();
        for offset in [1_i32, -2_i32] {
            output_data.extend_from_slice(&0_u32.to_le_bytes());
            output_data.extend_from_slice(&offset.to_le_bytes());
            output_data.extend_from_slice(&0_u32.to_le_bytes());
        }
        append_field(&mut payload, 32, &output_data);

        assert_eq!(flp_vst3_output_mixer_offsets(&payload), Some(vec![1, -2]));
        payload.pop();
        assert_eq!(flp_vst3_output_mixer_offsets(&payload), None);
    }

    #[test]
    fn vst3_output_mixer_offsets_are_relative_to_the_channel_route() {
        assert_eq!(vst3_output_mixer_route(Some(4), Some(2)), Some(6));
        assert_eq!(vst3_output_mixer_route(None, Some(1)), Some(1));
        assert_eq!(vst3_output_mixer_route(Some(127), Some(1)), Some(127));
        assert_eq!(vst3_output_mixer_route(Some(4), None), Some(4));
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
    fn streaming_resampler_sums_active_mono_and_stereo_buses() {
        let mut resampler = StereoStreamResampler::new(2, 2).unwrap();
        let outputs = [vec![0.25, 0.5], vec![-0.5, -0.25], vec![0.125, 0.25]];
        assert_eq!(
            resampler
                .push_outputs_with_bus_channels(&outputs, 2, &[2, 1])
                .unwrap(),
            [0.375, -0.375]
        );
        assert_eq!(resampler.finish().unwrap(), [0.75, 0.0]);
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

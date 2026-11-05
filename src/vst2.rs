//! Isolated VST2 plug-in inspection and offline note rendering.
//!
//! The plug-in ABI and worker process are provided by `vst-audio`; the host
//! application only talks to its bounded process and audio APIs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::media::{DecodedAudio, decode_audio_file, write_audio_range_to_wav};
use crate::midi::fl_note_release_to_midi_velocity;
use crate::sample_render::swing_note_start_tick;
use crate::vst3::{MixerEffectProcessor, MixerEffectProcessorFactory};
use crate::{ChannelPluginState, PatternNote};
use vst_audio::{
    AudioEndpoint, AudioEvent, AudioStatus, BusChoice, BusKind, BusSelector, ClassInfo, Command,
    Configuration, EditPhase, EventData, Host, HostEvent, Instance, MAX_EVENTS, PluginDescriptor,
    PluginFormat, ProcessReport, RequestId, Transport,
};

const RENDER_QUANTUM: u32 = 512;
pub const MAX_VST2_MIXER_AUXILIARY_INPUT_BUSES: usize = 16;
const MAX_VST2_MIXER_EFFECT_INPUT_CHANNELS: usize = 2 + MAX_VST2_MIXER_AUXILIARY_INPUT_BUSES * 2;
const MAX_RENDER_SECONDS: f64 = 120.0;
const MAX_RENDER_BUFFER_BYTES: usize = 512 * 1024 * 1024;
const VST2_TAIL_PROBE_FLAG: &str = "--vst2-tail-probe";
const VST2_TAIL_PROBE_MARKER: &str = "FLP_VST2_TAIL_V1 ";
const VST2_TAIL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VST2_TAIL_PROBE_STATE_BYTES: usize = 64 * 1024 * 1024 - 9;
const MAX_VST2_TAIL_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const VST2_TAIL_PROBE_FRAMED_ARGUMENT: &str = "v2";
const VST2_TAIL_PROBE_FRAME_MAGIC: &[u8; 4] = b"FLT2";
const MAX_VST2_TAIL_PROBE_PARAMETERS: usize = 65_536;
const MAX_VST2_TAIL_PROBE_INPUT_BYTES: usize = MAX_VST2_TAIL_PROBE_STATE_BYTES
    + 8
    + 12 * MAX_VST2_TAIL_PROBE_PARAMETERS
    + 1;
const MAX_VST2_MIXER_PARAMETER_HISTORY: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst2NoteRenderOptions {
    pub sample_rate: u32,
    pub midi_note: u8,
    pub velocity: u8,
    pub note_seconds: f64,
    pub tail_seconds: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vst2NoteRenderSummary {
    pub plugin_name: String,
    pub vendor: String,
    pub sample_rate: u32,
    pub frames: usize,
    pub latency_frames: usize,
    pub channels: usize,
    pub peak: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst2AudioEffectRenderOptions {
    pub tail_seconds: f64,
}

impl Default for Vst2AudioEffectRenderOptions {
    fn default() -> Self {
        Self { tail_seconds: 2.0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vst2AudioEffectRenderSummary {
    pub plugin_name: String,
    pub vendor: String,
    pub input_frames: usize,
    pub output_frames: usize,
    pub sample_rate: u32,
    pub latency_frames: usize,
    pub tail_frames: usize,
    pub channels: usize,
    pub peak: f32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst2StateProbeSummary {
    pub plugin_name: String,
    pub vendor: String,
    pub saved_plugin_name: String,
    pub saved_vendor: String,
    pub saved_fourcc: String,
    pub class_id: String,
    pub id_byte_order: &'static str,
    pub state_bytes: usize,
    pub state_roundtrip_identical: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst2PatternRenderOptions {
    pub channel_id: u16,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub tail_seconds: f64,
    pub sample_rate: u32,
    pub global_swing_mix_raw: u8,
    pub channel_swing_mix_raw: u16,
    /// Project Master pitch as a note transposition for channels using Add to Key.
    pub note_pitch_offset_semitones: i16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vst2PatternParameterPoint {
    pub tick: u64,
    pub value: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vst2PatternParameterAutomation {
    pub parameter_id: u32,
    pub points: Vec<Vst2PatternParameterPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vst2PatternRenderSummary {
    pub plugin_name: String,
    pub vendor: String,
    pub notes_rendered: usize,
    pub sample_rate: u32,
    pub frames: usize,
    pub latency_frames: usize,
    pub channels: usize,
    pub peak: f32,
}

/// Saved project state for one VST2 Mixer effect slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst2ProjectMixerEffectAssignment {
    pub plugin_path: PathBuf,
    pub fourcc: String,
    pub class_id: [u8; 16],
    pub state_bytes: Vec<u8>,
    /// Optional machine-local map from source Mixer insert to plug-in input bus index.
    pub auxiliary_input_source_map: Option<Vec<(usize, usize)>>,
}

/// One automatable parameter reported by an assigned VST2 Mixer effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Vst2MixerEffectParameter {
    pub id: u32,
    pub name: String,
    pub unit: String,
    pub value: f64,
    pub automatable: bool,
    pub read_only: bool,
}

#[derive(Clone, Default)]
pub struct Vst2MixerEffectParameterSync {
    state: Arc<Mutex<BTreeMap<(usize, u16), Vst2MixerEffectParameterUpdates>>>,
}

#[derive(Default)]
struct Vst2MixerEffectParameterUpdates {
    revision: u64,
    history: VecDeque<(u64, u32, f64)>,
    latest: BTreeMap<u32, f64>,
}

impl Vst2MixerEffectParameterSync {
    pub fn clear(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub fn remap_after_move(&self, insert_ordinal: usize, from_slot: u16, to_slot: u16) {
        if from_slot == to_slot {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let mut remapped = BTreeMap::new();
        for (key, updates) in std::mem::take(&mut *state) {
            let slot_index = if key.0 != insert_ordinal {
                key.1
            } else if key.1 == from_slot {
                to_slot
            } else if from_slot < to_slot && key.1 > from_slot && key.1 <= to_slot {
                key.1 - 1
            } else if from_slot > to_slot && key.1 >= to_slot && key.1 < from_slot {
                key.1 + 1
            } else {
                key.1
            };
            remapped.insert((key.0, slot_index), updates);
        }
        *state = remapped;
    }

    fn publish(&self, key: (usize, u16), parameter_id: u32, value: f64) {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let updates = state.entry(key).or_default();
        updates.revision = updates.revision.wrapping_add(1).max(1);
        let revision = updates.revision;
        updates.history.push_back((revision, parameter_id, value));
        if updates.history.len() > MAX_VST2_MIXER_PARAMETER_HISTORY {
            updates.history.pop_front();
        }
        updates.latest.insert(parameter_id, value);
    }

    fn snapshot(&self, key: (usize, u16)) -> (u64, BTreeMap<u32, f64>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|updates| (updates.revision, updates.latest.clone()))
            .unwrap_or_default()
    }

    fn updates_since(
        &self,
        key: (usize, u16),
        last_revision: u64,
    ) -> Option<(u64, Vec<(u32, f64)>)> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let updates = state.get(&key)?;
        if updates.revision == last_revision {
            return None;
        }

        let first_revision = updates.history.front().map(|entry| entry.0);
        if first_revision.is_some_and(|first| last_revision.saturating_add(1) < first) {
            let values = updates
                .latest
                .iter()
                .map(|(parameter_id, value)| (*parameter_id, *value))
                .collect();
            return Some((updates.revision, values));
        }

        let mut values = Vec::new();
        let mut revision = last_revision;
        for (next_revision, parameter_id, value) in updates
            .history
            .iter()
            .filter(|entry| entry.0 > last_revision)
        {
            values.push((*parameter_id, *value));
            revision = *next_revision;
        }
        (!values.is_empty()).then_some((revision, values))
    }
}

/// Creates isolated VST2 effect workers for the shared Mixer processing graph.
pub struct Vst2ProjectMixerEffectProcessorFactory {
    assignments: BTreeMap<(usize, u16), Vst2ProjectMixerEffectAssignment>,
    parameter_sync: Vst2MixerEffectParameterSync,
}

impl Vst2ProjectMixerEffectProcessorFactory {
    pub fn new(
        assignments: BTreeMap<(usize, u16), Vst2ProjectMixerEffectAssignment>,
        parameter_sync: Vst2MixerEffectParameterSync,
    ) -> Self {
        Self {
            assignments,
            parameter_sync,
        }
    }
}

struct Vst2ChannelClass {
    class: ClassInfo,
    fourcc: String,
}

#[derive(Clone, Copy)]
struct ScheduledVst2MidiEvent {
    frame: u64,
    priority: u8,
    data: [u8; 3],
}

#[derive(Clone, Copy)]
struct ScheduledVst2ParameterPoint {
    frame: u64,
    value: f64,
}

struct ScheduledVst2ParameterLane {
    parameter_id: u32,
    points: Vec<ScheduledVst2ParameterPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vst2HostedChannelInfo {
    pub id: u64,
    pub channel_id: u16,
    pub plugin_name: String,
    pub vendor: String,
    pub fourcc: String,
    pub sample_rate: u32,
    pub latency_frames: usize,
    pub input_channels: usize,
    pub output_channels: usize,
}

struct Vst2HostedChannel {
    info: Vst2HostedChannelInfo,
    instance: Instance,
    audio: AudioEndpoint,
    parameter_events: Vec<AudioEvent>,
    pending_parameter_updates: BTreeMap<u32, f64>,
}

impl Vst2HostedChannel {
    fn queue_parameter_edit(&mut self, parameter_id: u32, value: f64) {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            self.pending_parameter_updates.insert(parameter_id, value);
        }
    }
}

struct Vst2HostedMixerEffect {
    fourcc: String,
    instance: Instance,
    _audio: AudioEndpoint,
    auxiliary_input_buses: Vec<(usize, usize)>,
}

/// Persistent isolated VST2 instances restored from FLP Channel Rack state.
pub struct Vst2HostRuntime {
    host: Host,
    sample_rate: u32,
    quantum: u32,
    next_id: u64,
    channels: BTreeMap<u16, Vst2HostedChannel>,
    mixer_effects: BTreeMap<(usize, u16), Vst2HostedMixerEffect>,
    pending_editor_requests: BTreeSet<(u16, RequestId)>,
    pending_editor_failures: Vec<(u16, String)>,
    pending_mixer_editor_requests: BTreeMap<RequestId, (usize, u16)>,
    pending_mixer_editor_failures: Vec<(usize, u16, String)>,
    mixer_effect_parameter_sync: Vst2MixerEffectParameterSync,
    pending_worker_failures: Vec<(u16, String)>,
}

/// Run the embedded worker when this binary is launched by `vst-audio`.
///
/// `vst-audio` starts the configured executable with `--control-in` as its
/// first argument. Dispatching before the normal CLI or GUI keeps plug-in code
/// in the child process while allowing installers to ship one application
/// executable.
pub fn run_vst2_worker_if_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    match arguments.next() {
        Some(argument) if argument == OsStr::new("--control-in") => {
            vst_audio_worker_runtime::host::main();
            true
        }
        Some(argument) if argument == OsStr::new(VST2_TAIL_PROBE_FLAG) => {
            let result = run_vst2_tail_probe_child(arguments);
            let response = match result {
                Ok(Some(samples)) => format!("samples={samples}"),
                Ok(None) => "unknown".to_owned(),
                Err(error) => {
                    eprintln!("VST2 tail probe failed: {error}");
                    "unknown".to_owned()
                }
            };
            println!("{VST2_TAIL_PROBE_MARKER}{response}");
            true
        }
        _ => false,
    }
}


/// Framed tail-probe input: magic, a u32 parameter count, that many
/// (u32 id, f64 value bits) pairs, then the raw saved plug-in state. The
/// framed form lets a refresh probe reproduce parameter edits made after the
/// state was saved; unframed input keeps the original state-only meaning.
fn encode_vst2_tail_probe_input(state: &[u8], parameters: &[(u32, f64)]) -> Vec<u8> {
    let mut input = Vec::with_capacity(8 + parameters.len() * 12 + state.len());
    input.extend_from_slice(VST2_TAIL_PROBE_FRAME_MAGIC);
    input.extend_from_slice(&(parameters.len() as u32).to_le_bytes());
    for (parameter_id, value) in parameters {
        input.extend_from_slice(&parameter_id.to_le_bytes());
        input.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    input.extend_from_slice(state);
    input
}

fn decode_vst2_tail_probe_input(input: &[u8]) -> Option<(Vec<(u32, f64)>, &[u8])> {
    let (magic, rest) = input.split_at_checked(4)?;
    if magic != VST2_TAIL_PROBE_FRAME_MAGIC {
        return None;
    }
    let (count_bytes, rest) = rest.split_at_checked(4)?;
    let count = u32::from_le_bytes(count_bytes.try_into().ok()?) as usize;
    if count > MAX_VST2_TAIL_PROBE_PARAMETERS {
        return None;
    }
    let (parameter_bytes, state) = rest.split_at_checked(count.checked_mul(12)?)?;
    let mut parameters = Vec::with_capacity(count);
    for entry in parameter_bytes.chunks_exact(12) {
        let parameter_id = u32::from_le_bytes(entry[..4].try_into().ok()?);
        let value = f64::from_bits(u64::from_le_bytes(entry[4..12].try_into().ok()?));
        if !value.is_finite() {
            return None;
        }
        parameters.push((parameter_id, value));
    }
    Some((parameters, state))
}

fn run_vst2_tail_probe_child(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<Option<u32>, String> {
    let plugin_path = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "missing VST2 plug-in path".to_owned())?;
    let class_id = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .and_then(|value| decode_vst2_class_id(&value))
        .ok_or_else(|| "invalid VST2 class ID".to_owned())?;
    let sample_rate = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| "invalid VST2 sample rate".to_owned())?;
    let framed_input = match arguments.next() {
        None => false,
        Some(argument) if argument == OsStr::new(VST2_TAIL_PROBE_FRAMED_ARGUMENT) => true,
        Some(_) => return Err("unexpected VST2 tail probe argument".to_owned()),
    };
    if arguments.next().is_some() {
        return Err("unexpected VST2 tail probe argument".to_owned());
    }

    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_VST2_TAIL_PROBE_INPUT_BYTES as u64)
        .read_to_end(&mut input)
        .map_err(|error| format!("could not read VST2 tail probe input: {error}"))?;
    let (parameters, state) = if framed_input {
        let (parameters, state) = decode_vst2_tail_probe_input(&input)
            .ok_or_else(|| "invalid framed VST2 tail probe input".to_owned())?;
        if state.len() > MAX_VST2_TAIL_PROBE_STATE_BYTES {
            return Err("VST2 plug-in state exceeds the tail probe limit".to_owned());
        }
        (parameters, state.to_vec())
    } else {
        if input.len() > MAX_VST2_TAIL_PROBE_STATE_BYTES {
            return Err("VST2 plug-in state exceeds the tail probe limit".to_owned());
        }
        (Vec::new(), input)
    };

    let mut plugin =
        vst_audio_worker_sys::plugin::Vst2Plugin::load_class(&plugin_path, Some(class_id))
            .map_err(|error| error.to_string())?;
    plugin
        .set_state(&state)
        .map_err(|error| format!("could not restore VST2 plug-in state: {error}"))?;
    let info = plugin.info();
    plugin
        .configure(sample_rate, RENDER_QUANTUM, info.inputs, info.outputs)
        .map_err(|error| format!("could not configure VST2 plug-in: {error}"))?;
    for (parameter_id, value) in &parameters {
        plugin
            .set_parameter(*parameter_id, *value)
            .map_err(|error| {
                format!("could not apply VST2 plug-in parameter {parameter_id}: {error}")
            })?;
    }
    plugin.activate();
    Ok(plugin.tail_samples())
}

fn decode_vst2_class_id(encoded: &str) -> Option<[u8; 16]> {
    if encoded.len() != 32 || !encoded.is_ascii() {
        return None;
    }
    let mut class_id = [0_u8; 16];
    for (index, byte) in class_id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(class_id)
}

/// Load and inspect a VST2 binary in the isolated worker process.
pub fn inspect_vst2_plugin(path: impl AsRef<Path>) -> Result<Vec<ClassInfo>, String> {
    let path = path.as_ref();
    if !is_vst2_plugin_path(path) {
        return Err(format!(
            "VST2 plug-in file or bundle does not exist: {}",
            path.display()
        ));
    }
    let host = host_for_current_executable()?;
    let descriptor = PluginDescriptor::new(path, PluginFormat::Vst2);
    let classes = host
        .scan(&descriptor)
        .map_err(|error| format!("could not inspect VST2 plug-in: {error}"))?;
    if classes.is_empty() {
        return Err("VST2 plug-in did not report a loadable class".to_owned());
    }
    Ok(classes)
}

/// Try restoring one FLP VST2 channel's field-53 state in an isolated worker.
///
/// FL stores the FourCC as a little-endian 32-bit value; the worker class ID
/// encodes the VST2 unique ID in big-endian bytes. The state is only sent after
/// exactly one loaded class matches.
pub fn probe_vst2_channel_state(
    plugin_path: impl AsRef<Path>,
    state: &ChannelPluginState,
) -> Result<Vst2StateProbeSummary, String> {
    let plugin_path = plugin_path.as_ref();
    let matched = match_vst2_channel_class(plugin_path, state)?;
    let metadata = state
        .vst_metadata()
        .ok_or_else(|| "FLP channel has no recognized VST state envelope".to_owned())?;
    let state_bytes = state
        .vst_state_bytes()
        .ok_or_else(|| "FLP VST2 channel state has no nested field 53".to_owned())?;
    let class_id = matched
        .class
        .id
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let descriptor = PluginDescriptor {
        path: plugin_path.to_path_buf(),
        format: PluginFormat::Vst2,
        class_id: Some(matched.class.id),
    };
    let host = host_for_current_executable()?;
    let configuration = Configuration {
        offline: true,
        ..Configuration::default()
    };
    let instance = host
        .spawn(&descriptor, &configuration, Some(state_bytes))
        .map_err(|error| format!("could not restore FLP VST2 state in worker: {error}"))?;
    let worker_state = save_worker_vst2_state(&instance, |_| {})?;
    let state_roundtrip_identical = worker_state.as_slice() == state_bytes;
    drop(instance);

    Ok(Vst2StateProbeSummary {
        plugin_name: matched.class.name,
        vendor: matched.class.vendor,
        saved_plugin_name: metadata.name().unwrap_or("unknown").to_owned(),
        saved_vendor: metadata.vendor().unwrap_or("unknown").to_owned(),
        saved_fourcc: matched.fourcc,
        class_id,
        id_byte_order: "FLP-little-endian",
        state_bytes: state_bytes.len(),
        state_roundtrip_identical,
    })
}

fn match_vst2_channel_class(
    plugin_path: &Path,
    state: &ChannelPluginState,
) -> Result<Vst2ChannelClass, String> {
    let fourcc = state
        .vst_metadata()
        .and_then(|metadata| metadata.fourcc())
        .ok_or_else(|| "FLP channel state has no VST2 FourCC identifier".to_owned())?;
    if state.vst_state_bytes().is_none() {
        return Err("FLP VST2 channel state has no nested field 53".to_owned());
    }
    match_vst2_fourcc(plugin_path, fourcc)
}

fn match_vst2_fourcc(plugin_path: &Path, fourcc: &str) -> Result<Vst2ChannelClass, String> {
    let fourcc_bytes: [u8; 4] = fourcc
        .as_bytes()
        .try_into()
        .map_err(|_| "FLP VST2 FourCC identifier is not exactly four bytes".to_owned())?;
    if !fourcc_bytes.is_ascii() {
        return Err("FLP VST2 FourCC identifier is not ASCII".to_owned());
    }
    let saved_unique_id = i32::from_le_bytes(fourcc_bytes);
    let worker_uid_bytes = saved_unique_id.to_be_bytes();
    let matching_classes = inspect_vst2_plugin(plugin_path)?
        .into_iter()
        .filter(|class| {
            &class.id[..4] == b"VST2"
                && class.id[8..].iter().all(|byte| *byte == 0)
                && class.id[4..8] == worker_uid_bytes
        })
        .collect::<Vec<_>>();
    match matching_classes.as_slice() {
        [] => Err(format!(
            "no VST2 class in {} matches saved FourCC {fourcc}",
            plugin_path.display()
        )),
        [class] => Ok(Vst2ChannelClass {
            class: class.clone(),
            fourcc: fourcc.to_owned(),
        }),
        _ => Err(format!(
            "saved VST2 FourCC {fourcc} matches multiple classes in {}",
            plugin_path.display()
        )),
    }
}

/// Resolve a saved VST2 FourCC to its isolated worker class identifier.
pub fn vst2_class_id_for_fourcc(
    plugin_path: impl AsRef<Path>,
    fourcc: &str,
) -> Result<[u8; 16], String> {
    match_vst2_fourcc(plugin_path.as_ref(), fourcc).map(|matched| matched.class.id)
}

impl Vst2HostRuntime {
    pub fn new(sample_rate: u32, quantum: u32) -> Result<Self, String> {
        if !(8_000..=384_000).contains(&sample_rate) {
            return Err("VST2 host sample rate must be between 8,000 and 384,000 Hz".to_owned());
        }
        if quantum == 0 || quantum > 65_536 {
            return Err("VST2 host quantum must be between 1 and 65,536 frames".to_owned());
        }
        Ok(Self {
            host: host_for_current_executable()?,
            sample_rate,
            quantum,
            next_id: 1,
            channels: BTreeMap::new(),
            mixer_effects: BTreeMap::new(),
            pending_editor_requests: BTreeSet::new(),
            pending_editor_failures: Vec::new(),
            pending_mixer_editor_requests: BTreeMap::new(),
            pending_mixer_editor_failures: Vec::new(),
            mixer_effect_parameter_sync: Vst2MixerEffectParameterSync::default(),
            pending_worker_failures: Vec::new(),
        })
    }

    pub fn set_mixer_effect_parameter_sync(
        &mut self,
        parameter_sync: Vst2MixerEffectParameterSync,
    ) {
        self.mixer_effect_parameter_sync = parameter_sync;
    }

    /// Load a project VST2 channel in its isolated worker and restore its nested field-53 state.
    pub fn load_channel(
        &mut self,
        plugin_path: impl AsRef<Path>,
        state: &ChannelPluginState,
    ) -> Result<Vst2HostedChannelInfo, String> {
        let plugin_path = plugin_path.as_ref();
        let matched = match_vst2_channel_class(plugin_path, state)?;
        let state_bytes = state
            .vst_state_bytes()
            .ok_or_else(|| "FLP VST2 channel state has no nested field 53".to_owned())?;
        let descriptor = PluginDescriptor {
            path: plugin_path.to_path_buf(),
            format: PluginFormat::Vst2,
            class_id: Some(matched.class.id),
        };
        let configuration = Configuration {
            sample_rate: self.sample_rate,
            quantum: self.quantum,
            max_callback_frames: Some(self.quantum),
            offline: false,
            buses: Some(BusSelector::main_only()),
            ..Configuration::default()
        };
        let mut instance = self
            .host
            .spawn(&descriptor, &configuration, Some(state_bytes))
            .map_err(|error| format!("could not restore FLP VST2 state in worker: {error}"))?;
        let audio = instance
            .take_audio()
            .ok_or_else(|| "VST2 worker did not provide an audio endpoint".to_owned())?;
        let geometry = audio.geometry();
        let output_channels = geometry.output_channels as usize;
        if output_channels == 0 {
            return Err("VST2 plug-in has no active main output bus".to_owned());
        }
        let info = Vst2HostedChannelInfo {
            id: self.next_id,
            channel_id: state.channel_id(),
            plugin_name: matched.class.name,
            vendor: matched.class.vendor,
            fourcc: matched.fourcc,
            sample_rate: self.sample_rate,
            latency_frames: audio.latency_frames(),
            input_channels: geometry.input_channels as usize,
            output_channels,
        };
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.pending_editor_requests
            .retain(|(channel_id, _)| *channel_id != info.channel_id);
        self.channels.insert(
            info.channel_id,
            Vst2HostedChannel {
                info: info.clone(),
                instance,
                audio,
                parameter_events: Vec::with_capacity(MAX_EVENTS),
                pending_parameter_updates: BTreeMap::new(),
            },
        );
        Ok(info)
    }

    pub fn loaded_channels(&self) -> Vec<Vst2HostedChannelInfo> {
        self.channels
            .values()
            .map(|channel| channel.info.clone())
            .collect()
    }

    /// Load one VST2 Mixer effect into a persistent isolated worker for its native editor.
    pub fn load_mixer_effect(
        &mut self,
        key: (usize, u16),
        assignment: &Vst2ProjectMixerEffectAssignment,
    ) -> Result<(), String> {
        let selected_bus_layout = Arc::new(Mutex::new(None));
        let configuration = Configuration {
            sample_rate: self.sample_rate,
            quantum: self.quantum,
            max_callback_frames: Some(self.quantum),
            offline: false,
            buses: Some(vst2_mixer_effect_bus_inspector(Arc::clone(
                &selected_bus_layout,
            ))),
            ..Configuration::default()
        };
        let descriptor = PluginDescriptor {
            path: assignment.plugin_path.clone(),
            format: PluginFormat::Vst2,
            class_id: Some(assignment.class_id),
        };
        let mut instance = self
            .host
            .spawn(&descriptor, &configuration, Some(&assignment.state_bytes))
            .map_err(|error| format!("could not restore VST2 Mixer effect state: {error}"))?;
        let audio = instance.take_audio().ok_or_else(|| {
            "VST2 worker did not provide a Mixer effect audio endpoint".to_owned()
        })?;
        let bus_layout = selected_bus_layout
            .lock()
            .map_err(|_| "VST2 Mixer effect bus layout state was poisoned".to_owned())?
            .take()
            .ok_or_else(|| "VST2 Mixer effect bus layout was not selected".to_owned())?;
        self.unload_mixer_effect(key);
        self.mixer_effects.insert(
            key,
            Vst2HostedMixerEffect {
                fourcc: assignment.fourcc.clone(),
                instance,
                _audio: audio,
                auxiliary_input_buses: bus_layout.auxiliary_input_buses,
            },
        );
        Ok(())
    }

    pub fn mixer_effect_auxiliary_input_buses(
        &self,
        key: (usize, u16),
    ) -> Option<Vec<(usize, usize)>> {
        self.mixer_effects
            .get(&key)
            .map(|effect| effect.auxiliary_input_buses.clone())
    }

    pub fn has_mixer_effect(&self, key: (usize, u16)) -> bool {
        self.mixer_effects.contains_key(&key)
    }

    /// Read a bounded snapshot of the parameters exposed by one loaded Mixer effect.
    pub fn mixer_effect_parameters(
        &mut self,
        key: (usize, u16),
    ) -> Result<Vec<Vst2MixerEffectParameter>, String> {
        let pending_requests = &mut self.pending_mixer_editor_requests;
        let pending_failures = &mut self.pending_mixer_editor_failures;
        let parameter_sync = self.mixer_effect_parameter_sync.clone();
        let effect = self.mixer_effects.get(&key).ok_or_else(|| {
            format!(
                "VST2 Mixer insert {} effect slot {} is not loaded",
                key.0,
                key.1 + 1
            )
        })?;
        read_vst2_parameters(
            &effect.instance,
            &effect.fourcc,
            "Mixer effect",
            |event| match event {
                HostEvent::Completed { request } => {
                    pending_requests.remove(&request);
                }
                HostEvent::RequestFailed { request, reason } => {
                    if let Some((insert, slot)) = pending_requests.remove(&request) {
                        pending_failures.push((insert, slot, reason));
                    }
                }
                HostEvent::ParameterEdited {
                    id,
                    phase: EditPhase::Perform,
                    value,
                } => parameter_sync.publish(key, id, value),
                _ => {}
            },
        )
    }

    /// Set one loaded Mixer effect parameter in its editor worker and the active audio processor.
    pub fn set_mixer_effect_parameter(
        &mut self,
        key: (usize, u16),
        parameter_id: u32,
        value: f64,
    ) -> Result<(), String> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("VST2 Mixer effect parameter value must be normalized to 0–1".to_owned());
        }
        let effect = self.mixer_effects.get(&key).ok_or_else(|| {
            format!(
                "VST2 Mixer insert {} effect slot {} is not loaded",
                key.0,
                key.1 + 1
            )
        })?;
        let request = effect
            .instance
            .controller()
            .request(Command::SetParameter {
                id: parameter_id,
                value,
            })
            .map_err(|error| format!("could not set VST2 Mixer effect parameter: {error}"))?;
        self.pending_mixer_editor_requests.insert(request, key);
        self.mixer_effect_parameter_sync
            .publish(key, parameter_id, value);
        Ok(())
    }

    /// Read a bounded snapshot of the parameters exposed by one loaded Channel Rack plug-in.
    pub fn channel_parameters(
        &mut self,
        channel_id: u16,
    ) -> Result<Vec<Vst2MixerEffectParameter>, String> {
        let mut parameter_edits = Vec::new();
        let result = {
            let pending_requests = &mut self.pending_editor_requests;
            let pending_failures = &mut self.pending_editor_failures;
            let channel = self
                .channels
                .get(&channel_id)
                .ok_or_else(|| format!("VST2 channel {channel_id} is not loaded"))?;
            read_vst2_parameters(
                &channel.instance,
                &channel.info.fourcc,
                "channel",
                |event| match event {
                    HostEvent::Completed { request } => {
                        pending_requests.remove(&(channel_id, request));
                    }
                    HostEvent::RequestFailed { request, reason }
                        if pending_requests.remove(&(channel_id, request)) =>
                    {
                        pending_failures.push((channel_id, reason));
                    }
                    HostEvent::ParameterEdited {
                        id,
                        phase: EditPhase::Perform,
                        value,
                    } => parameter_edits.push((id, value)),
                    _ => {}
                },
            )
        };
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            for (parameter_id, value) in parameter_edits {
                channel.queue_parameter_edit(parameter_id, value);
            }
        }
        result
    }

    /// Set one loaded Channel Rack parameter in its editor worker and queue it for audio processing.
    pub fn set_channel_parameter(
        &mut self,
        channel_id: u16,
        parameter_id: u32,
        value: f64,
    ) -> Result<(), String> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err("VST2 channel parameter value must be normalized to 0–1".to_owned());
        }
        let request = {
            let channel = self
                .channels
                .get_mut(&channel_id)
                .ok_or_else(|| format!("VST2 channel {channel_id} is not loaded"))?;
            let request = channel
                .instance
                .controller()
                .request(Command::SetParameter {
                    id: parameter_id,
                    value,
                })
                .map_err(|error| format!("could not set VST2 channel parameter: {error}"))?;
            channel.queue_parameter_edit(parameter_id, value);
            request
        };
        self.pending_editor_requests.insert((channel_id, request));
        Ok(())
    }

    /// Request the native editor for one loaded VST2 Mixer effect.
    pub fn open_mixer_effect_editor(&mut self, key: (usize, u16)) -> Result<(), String> {
        let effect = self.mixer_effects.get(&key).ok_or_else(|| {
            format!(
                "VST2 Mixer insert {} effect slot {} is not loaded",
                key.0,
                key.1 + 1
            )
        })?;
        let request = effect
            .instance
            .controller()
            .request(Command::OpenEditor)
            .map_err(|error| format!("could not request VST2 Mixer effect editor: {error}"))?;
        self.pending_mixer_editor_requests.insert(request, key);
        Ok(())
    }

    /// Snapshot the editor worker's current state for writing back to FLP field 53.
    pub fn save_mixer_effect_state(&mut self, key: (usize, u16)) -> Result<Vec<u8>, String> {
        let effect = self.mixer_effects.get(&key).ok_or_else(|| {
            format!(
                "VST2 Mixer insert {} effect slot {} is not loaded",
                key.0,
                key.1 + 1
            )
        })?;
        let request = effect
            .instance
            .controller()
            .request(Command::SaveState)
            .map_err(|error| format!("could not request VST2 Mixer effect state: {error}"))?;
        let fourcc = effect.fourcc.clone();
        let parameter_sync = self.mixer_effect_parameter_sync.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "VST2 Mixer effect {fourcc} state snapshot timed out"
                ));
            }
            let Some(event) = effect.instance.try_next_event() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            match event {
                HostEvent::Completed {
                    request: completed_request,
                } => {
                    self.pending_mixer_editor_requests
                        .remove(&completed_request);
                }
                HostEvent::State {
                    request: Some(state_request),
                    bytes,
                } if state_request == request => return Ok(bytes),
                HostEvent::RequestFailed {
                    request: failed_request,
                    reason,
                } if failed_request == request => {
                    return Err(format!(
                        "VST2 Mixer effect {fourcc} state snapshot failed: {reason}"
                    ));
                }
                HostEvent::RequestFailed { request, reason } => {
                    if let Some((insert_ordinal, slot_index)) =
                        self.pending_mixer_editor_requests.remove(&request)
                    {
                        self.pending_mixer_editor_failures.push((
                            insert_ordinal,
                            slot_index,
                            reason,
                        ));
                    }
                }
                HostEvent::ParameterEdited {
                    id,
                    phase: EditPhase::Perform,
                    value,
                } => parameter_sync.publish(key, id, value),
                HostEvent::Failed { reason } => {
                    return Err(format!(
                        "VST2 Mixer effect {fourcc} worker failed: {reason}"
                    ));
                }
                HostEvent::RecoveryRequired | HostEvent::Stopped => {
                    return Err(format!(
                        "VST2 Mixer effect {fourcc} worker stopped during state snapshot"
                    ));
                }
                _ => {}
            }
        }
    }

    pub fn take_mixer_editor_failures(&mut self) -> Vec<(usize, u16, String)> {
        self.poll_events();
        std::mem::take(&mut self.pending_mixer_editor_failures)
    }

    pub fn unload_mixer_effect(&mut self, key: (usize, u16)) -> bool {
        self.pending_mixer_editor_requests
            .retain(|_, pending_key| *pending_key != key);
        self.mixer_effects.remove(&key).is_some()
    }

    pub fn remap_mixer_effects_after_move(
        &mut self,
        insert_ordinal: usize,
        from_slot: u16,
        to_slot: u16,
    ) {
        if from_slot == to_slot {
            return;
        }
        self.mixer_effect_parameter_sync
            .remap_after_move(insert_ordinal, from_slot, to_slot);
        let mut remapped = BTreeMap::new();
        for (key, effect) in std::mem::take(&mut self.mixer_effects) {
            let slot_index = if key.0 != insert_ordinal {
                key.1
            } else if key.1 == from_slot {
                to_slot
            } else if from_slot < to_slot && key.1 > from_slot && key.1 <= to_slot {
                key.1 - 1
            } else if from_slot > to_slot && key.1 >= to_slot && key.1 < from_slot {
                key.1 + 1
            } else {
                key.1
            };
            remapped.insert((key.0, slot_index), effect);
        }
        self.mixer_effects = remapped;
        for key in self.pending_mixer_editor_requests.values_mut() {
            if key.0 == insert_ordinal {
                key.1 = if key.1 == from_slot {
                    to_slot
                } else if from_slot < to_slot && key.1 > from_slot && key.1 <= to_slot {
                    key.1 - 1
                } else if from_slot > to_slot && key.1 >= to_slot && key.1 < from_slot {
                    key.1 + 1
                } else {
                    key.1
                };
            }
        }
        for (effect_insert, effect_slot, _) in &mut self.pending_mixer_editor_failures {
            if *effect_insert == insert_ordinal {
                *effect_slot = if *effect_slot == from_slot {
                    to_slot
                } else if from_slot < to_slot && *effect_slot > from_slot && *effect_slot <= to_slot
                {
                    *effect_slot - 1
                } else if from_slot > to_slot && *effect_slot >= to_slot && *effect_slot < from_slot
                {
                    *effect_slot + 1
                } else {
                    *effect_slot
                };
            }
        }
    }

    /// Request the plug-in's native editor in its isolated worker process.
    pub fn open_editor(&mut self, channel_id: u16) -> Result<(), String> {
        let channel = self
            .channels
            .get(&channel_id)
            .ok_or_else(|| format!("VST2 channel {channel_id} is not loaded"))?;
        let request = channel
            .instance
            .controller()
            .request(Command::OpenEditor)
            .map_err(|error| format!("could not request VST2 editor: {error}"))?;
        self.pending_editor_requests.insert((channel_id, request));
        Ok(())
    }

    /// Snapshot one loaded channel's current plug-in state for FLP field 53.
    pub fn save_channel_state(&mut self, channel_id: u16) -> Result<Vec<u8>, String> {
        let mut parameter_edits = Vec::new();
        let result = {
            let channel = self
                .channels
                .get(&channel_id)
                .ok_or_else(|| format!("VST2 channel {channel_id} is not loaded"))?;
            save_worker_vst2_state(&channel.instance, |event| {
                if let HostEvent::ParameterEdited {
                    id,
                    phase: EditPhase::Perform,
                    value,
                } = event
                {
                    parameter_edits.push((id, value));
                }
            })
        };
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            for (parameter_id, value) in parameter_edits {
                channel.queue_parameter_edit(parameter_id, value);
            }
        }
        result.map_err(|error| format!("VST2 channel {channel_id} state snapshot failed: {error}"))
    }

    /// Process one block through a loaded channel. MIDI event buses use bus zero;
    /// MIDI channel selection is carried in each event's status byte.
    #[allow(clippy::too_many_arguments)]
    pub fn process_events(
        &mut self,
        channel_id: u16,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        transport: Transport,
        events: &[AudioEvent],
        output_events: &mut [AudioEvent],
    ) -> Result<ProcessReport, String> {
        let channel = self
            .channels
            .get_mut(&channel_id)
            .ok_or_else(|| format!("VST2 channel {channel_id} is not loaded"))?;
        channel.parameter_events.clear();
        let parameter_event_count = MAX_EVENTS.saturating_sub(events.len());
        let pending_parameter_ids = channel
            .pending_parameter_updates
            .keys()
            .take(parameter_event_count)
            .copied()
            .collect::<Vec<_>>();
        for parameter_id in pending_parameter_ids {
            let Some(value) = channel.pending_parameter_updates.remove(&parameter_id) else {
                continue;
            };
            channel.parameter_events.push(AudioEvent {
                sample_offset: 0,
                bus: 0,
                data: EventData::Parameter {
                    id: parameter_id,
                    value,
                },
            });
        }
        channel.parameter_events.extend_from_slice(events);
        channel
            .parameter_events
            .sort_by_key(|event| event.sample_offset);
        Ok(channel.audio.process_events(
            frames,
            inputs,
            outputs,
            transport,
            &channel.parameter_events,
            output_events,
        ))
    }

    /// Consume worker lifecycle errors for channels loaded into this host.
    pub fn poll_failures(&mut self) -> Vec<(u16, String)> {
        self.poll_events();
        std::mem::take(&mut self.pending_worker_failures)
    }

    /// Consume asynchronous errors from native editor requests.
    pub fn take_editor_failures(&mut self) -> Vec<(u16, String)> {
        self.poll_events();
        std::mem::take(&mut self.pending_editor_failures)
    }

    fn poll_events(&mut self) {
        let parameter_sync = self.mixer_effect_parameter_sync.clone();
        for (channel_id, channel) in &mut self.channels {
            while let Some(event) = channel.instance.try_next_event() {
                match event {
                    HostEvent::Completed { request } => {
                        self.pending_editor_requests.remove(&(*channel_id, request));
                    }
                    HostEvent::RequestFailed { request, reason }
                        if self.pending_editor_requests.remove(&(*channel_id, request)) =>
                    {
                        self.pending_editor_failures.push((*channel_id, reason));
                    }
                    HostEvent::Failed { reason } => {
                        self.pending_worker_failures.push((*channel_id, reason));
                    }
                    HostEvent::RecoveryRequired | HostEvent::Stopped => {
                        self.pending_worker_failures.push((
                            *channel_id,
                            "VST2 worker stopped or requires recovery".to_owned(),
                        ))
                    }
                    HostEvent::ParameterEdited {
                        id,
                        phase: EditPhase::Perform,
                        value,
                    } => channel.queue_parameter_edit(id, value),
                    _ => {}
                }
            }
        }
        for (key, effect) in &mut self.mixer_effects {
            while let Some(event) = effect.instance.try_next_event() {
                match event {
                    HostEvent::Completed { request } => {
                        self.pending_mixer_editor_requests.remove(&request);
                    }
                    HostEvent::RequestFailed { request, reason }
                        if self
                            .pending_mixer_editor_requests
                            .remove(&request)
                            .is_some() =>
                    {
                        self.pending_mixer_editor_failures
                            .push((key.0, key.1, reason));
                    }
                    HostEvent::Failed { reason } => self
                        .pending_mixer_editor_failures
                        .push((key.0, key.1, reason)),
                    HostEvent::RecoveryRequired | HostEvent::Stopped => {
                        self.pending_mixer_editor_failures.push((
                            key.0,
                            key.1,
                            "VST2 Mixer effect worker stopped or requires recovery".to_owned(),
                        ))
                    }
                    HostEvent::ParameterEdited {
                        id,
                        phase: EditPhase::Perform,
                        value,
                    } => parameter_sync.publish(*key, id, value),
                    _ => {}
                }
            }
        }
    }

    pub fn unload_channel(&mut self, channel_id: u16) -> bool {
        self.pending_editor_requests
            .retain(|(pending_channel_id, _)| *pending_channel_id != channel_id);
        self.channels.remove(&channel_id).is_some()
    }
}

fn read_vst2_parameters(
    instance: &Instance,
    identity: &str,
    plugin_kind: &str,
    mut handle_unrelated_event: impl FnMut(HostEvent),
) -> Result<Vec<Vst2MixerEffectParameter>, String> {
    const PAGE_SIZE: u16 = 32;
    const MAX_PARAMETERS: u32 = 65_536;
    const TIMEOUT: Duration = Duration::from_secs(5);

    let mut start = 0_u32;
    let mut request = instance
        .controller()
        .request(Command::ListParameters {
            start,
            count: PAGE_SIZE,
        })
        .map_err(|error| format!("could not request VST2 {plugin_kind} parameters: {error}"))?;
    let deadline = Instant::now() + TIMEOUT;
    let mut parameters = Vec::new();
    let mut total = None;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "VST2 {plugin_kind} {identity} parameter query timed out"
            ));
        }
        let Some(event) = instance.try_next_event() else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        match event {
            HostEvent::Parameters {
                request: completed_request,
                total: reported_total,
                entries,
            } if completed_request == request => {
                if total.is_some_and(|expected| expected != reported_total)
                    || reported_total > MAX_PARAMETERS
                {
                    return Err(format!(
                        "VST2 {plugin_kind} {identity} reported an unsupported parameter count"
                    ));
                }
                total = Some(reported_total);
                let received = u32::try_from(entries.len()).unwrap_or(u32::MAX);
                parameters.extend(
                    entries
                        .into_iter()
                        .map(|parameter| Vst2MixerEffectParameter {
                            id: parameter.id,
                            name: parameter.name,
                            unit: parameter.unit,
                            value: parameter.value,
                            automatable: parameter.automatable,
                            read_only: parameter.read_only,
                        }),
                );
                start = start.saturating_add(received);
                let total = total.unwrap_or_default();
                if start >= total || received == 0 {
                    return Ok(parameters);
                }
                request = instance
                    .controller()
                    .request(Command::ListParameters {
                        start,
                        count: PAGE_SIZE,
                    })
                    .map_err(|error| {
                        format!("could not request VST2 {plugin_kind} parameters: {error}")
                    })?;
            }
            HostEvent::RequestFailed {
                request: failed_request,
                reason,
            } if failed_request == request => {
                return Err(format!(
                    "VST2 {plugin_kind} {identity} parameter query failed: {reason}"
                ));
            }
            HostEvent::Failed { reason } => {
                return Err(format!(
                    "VST2 {plugin_kind} {identity} worker failed: {reason}"
                ));
            }
            HostEvent::RecoveryRequired | HostEvent::Stopped => {
                return Err(format!(
                    "VST2 {plugin_kind} {identity} worker stopped during parameter query"
                ));
            }
            event => handle_unrelated_event(event),
        }
    }
}

#[derive(Default)]
struct Vst2MixerEffectBusLayout {
    main_input_offset: usize,
    main_input_channels: usize,
    auxiliary_input_buses: Vec<(usize, usize)>,
    auxiliary_input_channel_offsets: Vec<usize>,
    input_channels: usize,
    output_channels: usize,
}

fn vst2_mixer_effect_bus_selector(
    selected_layout: Arc<Mutex<Option<Vst2MixerEffectBusLayout>>>,
) -> BusSelector {
    BusSelector::new(move |inputs, outputs| {
        let main_input_index = inputs
            .iter()
            .position(|bus| bus.kind == BusKind::Main)
            .ok_or_else(|| "VST2 Mixer effect has no main input bus".to_owned())?;
        let main_output_index = outputs
            .iter()
            .position(|bus| bus.kind == BusKind::Main)
            .ok_or_else(|| "VST2 Mixer effect has no main output bus".to_owned())?;
        let main_input_channels = usize::from(inputs[main_input_index].layout.channels);
        let output_channels = usize::from(outputs[main_output_index].layout.channels);
        if !matches!(main_input_channels, 1 | 2) || !matches!(output_channels, 1 | 2) {
            return Err(
                "VST2 Mixer effect must expose mono or stereo main input and output buses"
                    .to_owned(),
            );
        }

        let auxiliary_count = inputs.iter().filter(|bus| bus.kind == BusKind::Aux).count();
        if auxiliary_count > MAX_VST2_MIXER_AUXILIARY_INPUT_BUSES
            || inputs
                .iter()
                .filter(|bus| bus.kind == BusKind::Aux)
                .any(|bus| !matches!(bus.layout.channels, 1 | 2))
        {
            return Err(
                "VST2 Mixer effect supports up to 16 mono or stereo auxiliary input buses"
                    .to_owned(),
            );
        }

        let mut layout = Vst2MixerEffectBusLayout {
            main_input_channels,
            output_channels,
            ..Vst2MixerEffectBusLayout::default()
        };
        let mut channel_offset = 0usize;
        let input_configurations = inputs
            .iter()
            .enumerate()
            .map(|(bus_index, bus)| {
                let active = bus_index == main_input_index || bus.kind == BusKind::Aux;
                if active {
                    if bus_index == main_input_index {
                        layout.main_input_offset = channel_offset;
                    } else {
                        layout
                            .auxiliary_input_buses
                            .push((bus_index, usize::from(bus.layout.channels)));
                        layout.auxiliary_input_channel_offsets.push(channel_offset);
                    }
                    channel_offset =
                        channel_offset.saturating_add(usize::from(bus.layout.channels));
                }
                bus.configuration(active)
            })
            .collect();
        layout.input_channels = channel_offset;
        let output_configurations = outputs
            .iter()
            .enumerate()
            .map(|(bus_index, bus)| bus.configuration(bus_index == main_output_index))
            .collect();
        *selected_layout
            .lock()
            .map_err(|_| "VST2 Mixer effect bus layout state was poisoned".to_owned())? =
            Some(layout);
        Ok(BusChoice {
            inputs: input_configurations,
            outputs: output_configurations,
        })
    })
}

fn vst2_mixer_effect_bus_inspector(
    selected_layout: Arc<Mutex<Option<Vst2MixerEffectBusLayout>>>,
) -> BusSelector {
    BusSelector::new(move |inputs, outputs| {
        let main_input_index = inputs
            .iter()
            .position(|bus| bus.kind == BusKind::Main)
            .ok_or_else(|| "VST2 Mixer effect has no main input bus".to_owned())?;
        let main_output_index = outputs
            .iter()
            .position(|bus| bus.kind == BusKind::Main)
            .ok_or_else(|| "VST2 Mixer effect has no main output bus".to_owned())?;
        let mut auxiliary_input_buses = Vec::new();
        for (bus_index, bus) in inputs.iter().enumerate() {
            if bus.kind == BusKind::Aux
                && matches!(bus.layout.channels, 1 | 2)
                && auxiliary_input_buses.len() < MAX_VST2_MIXER_AUXILIARY_INPUT_BUSES
            {
                auxiliary_input_buses.push((bus_index, usize::from(bus.layout.channels)));
            }
        }
        *selected_layout
            .lock()
            .map_err(|_| "VST2 Mixer effect bus layout state was poisoned".to_owned())? =
            Some(Vst2MixerEffectBusLayout {
                main_input_channels: usize::from(inputs[main_input_index].layout.channels),
                output_channels: usize::from(outputs[main_output_index].layout.channels),
                auxiliary_input_buses,
                ..Vst2MixerEffectBusLayout::default()
            });
        Ok(BusChoice {
            inputs: inputs
                .iter()
                .enumerate()
                .map(|(bus_index, bus)| bus.configuration(bus_index == main_input_index))
                .collect(),
            outputs: outputs
                .iter()
                .enumerate()
                .map(|(bus_index, bus)| bus.configuration(bus_index == main_output_index))
                .collect(),
        })
    })
}

fn copy_vst2_mixer_stereo_bus(
    input_buffers: &mut [Vec<f32>],
    channel_offset: usize,
    channels: usize,
    stereo_samples: &[f32],
    frames: usize,
) -> Result<(), String> {
    let expected_sample_count = frames
        .checked_mul(2)
        .ok_or_else(|| "VST2 Mixer bus block size overflow".to_owned())?;
    if stereo_samples.len() != expected_sample_count {
        return Err("VST2 Mixer bus block has an invalid size".to_owned());
    }
    let stereo_frames = stereo_samples.as_chunks::<2>().0;
    if channels == 1 {
        let mono_buffer = input_buffers
            .get_mut(channel_offset)
            .and_then(|buffer| buffer.get_mut(..frames))
            .ok_or_else(|| "VST2 Mixer mono bus channel is outside its active layout".to_owned())?;
        for (mono, stereo) in mono_buffer.iter_mut().zip(stereo_frames.iter()) {
            *mono = finite_sample((stereo[0] + stereo[1]) * 0.5);
        }
    } else if channels == 2 {
        let right_channel = channel_offset
            .checked_add(1)
            .ok_or_else(|| "VST2 Mixer stereo bus channel offset overflow".to_owned())?;
        if right_channel >= input_buffers.len() {
            return Err("VST2 Mixer stereo bus is outside its active layout".to_owned());
        }
        let (left_buffers, right_buffers) = input_buffers.split_at_mut(right_channel);
        let left_buffer = left_buffers
            .get_mut(channel_offset)
            .and_then(|buffer| buffer.get_mut(..frames))
            .ok_or_else(|| "VST2 Mixer left channel is outside its active layout".to_owned())?;
        let right_buffer = right_buffers
            .first_mut()
            .and_then(|buffer| buffer.get_mut(..frames))
            .ok_or_else(|| "VST2 Mixer right channel is outside its active layout".to_owned())?;
        for ((left, right), stereo) in left_buffer
            .iter_mut()
            .zip(right_buffer.iter_mut())
            .zip(stereo_frames.iter())
        {
            *left = finite_sample(stereo[0]);
            *right = finite_sample(stereo[1]);
        }
    } else {
        return Err("VST2 Mixer bus must be mono or stereo".to_owned());
    }
    Ok(())
}


/// One-slot hand-off for background VST2 tail refresh probes. The processing
/// thread publishes parameter revisions; a detached probe thread reports the
/// refreshed tail for the revision it probed.
#[derive(Default)]
struct Vst2TailRefreshShared {
    in_flight: AtomicBool,
    result: Mutex<Option<(u64, Option<u64>)>>,
}

impl Vst2TailRefreshShared {
    fn take_result(&self) -> Option<(u64, Option<u64>)> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn store_result(&self, revision: u64, tail_samples: Option<u64>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((revision, tail_samples));
    }
}

fn vst2_tail_refresh_should_start(
    probed_revision: u64,
    current_revision: u64,
    in_flight: bool,
) -> bool {
    current_revision > probed_revision && !in_flight
}

struct Vst2MixerEffectProcessor {
    _instance: Instance,
    audio: AudioEndpoint,
    sample_rate: u32,
    quantum: usize,
    input_channels: usize,
    output_channels: usize,
    main_input_offset: usize,
    auxiliary_input_buses: Vec<(usize, usize)>,
    auxiliary_input_channel_offsets: Vec<usize>,
    auxiliary_input_source_map: Option<Vec<(usize, usize)>>,
    latency_samples: u32,
    tail_samples: Option<u64>,
    tail_assignment: Vst2ProjectMixerEffectAssignment,
    tail_parameters: BTreeMap<u32, f64>,
    tail_parameters_revision: u64,
    tail_probed_revision: u64,
    tail_refresh: Arc<Vst2TailRefreshShared>,
    startup_fallbacks: usize,
    transport: Transport,
    input_buffers: Vec<Vec<f32>>,
    output_left: Vec<f32>,
    output_right: Vec<f32>,
    parameter_events: Vec<AudioEvent>,
    parameter_sync: Vst2MixerEffectParameterSync,
    parameter_sync_key: (usize, u16),
    parameter_sync_revision: u64,
    pending_parameter_updates: BTreeMap<u32, f64>,
}

impl Vst2MixerEffectProcessor {
    fn spawn(
        assignment: &Vst2ProjectMixerEffectAssignment,
        key: (usize, u16),
        sample_rate: u32,
        parameter_sync: Vst2MixerEffectParameterSync,
    ) -> Result<Self, String> {
        let (parameter_sync_revision, pending_parameter_updates) = parameter_sync.snapshot(key);
        let tail_samples = if pending_parameter_updates.is_empty() {
            probe_vst2_mixer_effect_tail_samples(assignment, sample_rate)
        } else {
            probe_vst2_mixer_effect_tail_samples_with_parameters(
                assignment,
                sample_rate,
                &pending_parameter_updates,
            )
        };
        let selected_bus_layout = Arc::new(Mutex::new(None));
        let configuration = Configuration {
            sample_rate,
            quantum: RENDER_QUANTUM,
            max_callback_frames: Some(RENDER_QUANTUM),
            offline: false,
            buses: Some(vst2_mixer_effect_bus_selector(Arc::clone(
                &selected_bus_layout,
            ))),
            ..Configuration::default()
        };
        let descriptor = PluginDescriptor {
            path: assignment.plugin_path.clone(),
            format: PluginFormat::Vst2,
            class_id: Some(assignment.class_id),
        };
        let host = host_for_current_executable()?;
        let mut instance = host
            .spawn(&descriptor, &configuration, Some(&assignment.state_bytes))
            .map_err(|error| format!("could not restore VST2 Mixer effect state: {error}"))?;
        let audio = instance.take_audio().ok_or_else(|| {
            "VST2 worker did not provide a Mixer effect audio endpoint".to_owned()
        })?;
        let geometry = audio.geometry();
        let bus_layout = selected_bus_layout
            .lock()
            .map_err(|_| "VST2 Mixer effect bus layout state was poisoned".to_owned())?
            .take()
            .ok_or_else(|| "VST2 Mixer effect bus layout was not selected".to_owned())?;
        if geometry.input_channels as usize != bus_layout.input_channels
            || geometry.output_channels as usize != bus_layout.output_channels
        {
            return Err(format!(
                "VST2 Mixer effect {} has inconsistent active bus geometry",
                assignment.fourcc
            ));
        }
        let latency_samples = u32::try_from(audio.latency_frames()).map_err(|_| {
            "VST2 Mixer effect latency exceeds the supported sample range".to_owned()
        })?;
        let quantum = RENDER_QUANTUM as usize;
        Ok(Self {
            _instance: instance,
            audio,
            sample_rate,
            quantum,
            input_channels: bus_layout.main_input_channels,
            output_channels: bus_layout.output_channels,
            main_input_offset: bus_layout.main_input_offset,
            auxiliary_input_buses: bus_layout.auxiliary_input_buses,
            auxiliary_input_channel_offsets: bus_layout.auxiliary_input_channel_offsets,
            auxiliary_input_source_map: assignment.auxiliary_input_source_map.clone(),
            latency_samples,
            tail_samples,
            tail_assignment: assignment.clone(),
            tail_parameters: pending_parameter_updates.clone(),
            tail_parameters_revision: 0,
            tail_probed_revision: 0,
            tail_refresh: Arc::new(Vst2TailRefreshShared::default()),
            startup_fallbacks: (latency_samples as usize).div_ceil(quantum),
            transport: Transport::default(),
            input_buffers: vec![vec![0.0; quantum]; bus_layout.input_channels],
            output_left: vec![0.0; quantum],
            output_right: vec![0.0; quantum],
            parameter_events: Vec::with_capacity(MAX_EVENTS),
            parameter_sync,
            parameter_sync_key: key,
            parameter_sync_revision,
            pending_parameter_updates,
        })
    }
}

impl Vst2MixerEffectProcessor {
    /// Records a parameter value this processor has applied to its live
    /// instance so background tail probes reproduce the current state.
    fn note_tail_parameter(&mut self, parameter_id: u32, value: f64) {
        if self.tail_parameters.insert(parameter_id, value) != Some(value) {
            self.tail_parameters_revision =
                self.tail_parameters_revision.wrapping_add(1).max(1);
        }
    }

    /// Applies any finished tail refresh and, when parameters changed since
    /// the last probe, starts one detached refresh probe. At most one probe
    /// runs at a time; changes made during a probe coalesce into the next
    /// one. A failed probe keeps the previously reported tail.
    fn refresh_tail_samples_if_needed(&mut self) {
        if let Some((revision, refreshed)) = self.tail_refresh.take_result() {
            self.tail_probed_revision = self.tail_probed_revision.max(revision);
            if let Some(samples) = refreshed {
                self.tail_samples = Some(samples);
            }
        }
        let in_flight = self.tail_refresh.in_flight.load(Ordering::Acquire);
        if !vst2_tail_refresh_should_start(
            self.tail_probed_revision,
            self.tail_parameters_revision,
            in_flight,
        ) {
            return;
        }
        if self
            .tail_refresh
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let revision = self.tail_parameters_revision;
        let parameters = self.tail_parameters.clone();
        let assignment = self.tail_assignment.clone();
        let sample_rate = self.sample_rate;
        let shared = Arc::clone(&self.tail_refresh);
        let spawned = thread::Builder::new()
            .name("vst2-tail-refresh".to_owned())
            .spawn(move || {
                let refreshed = probe_vst2_mixer_effect_tail_samples_with_parameters(
                    &assignment,
                    sample_rate,
                    &parameters,
                );
                shared.store_result(revision, refreshed);
                shared.in_flight.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.tail_refresh.in_flight.store(false, Ordering::Release);
        }
    }
}

impl MixerEffectProcessor for Vst2MixerEffectProcessor {
    fn sample_rate(&self) -> f64 {
        f64::from(self.sample_rate)
    }

    fn block_size(&self) -> usize {
        self.quantum
    }

    fn input_channels(&self) -> usize {
        self.input_channels
    }

    fn auxiliary_input_buses(&self) -> &[(usize, usize)] {
        &self.auxiliary_input_buses
    }

    fn auxiliary_input_source_map(&self) -> Option<&[(usize, usize)]> {
        self.auxiliary_input_source_map.as_deref()
    }

    fn output_channels(&self) -> usize {
        self.output_channels
    }

    fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    fn tail_samples(&self) -> Option<u64> {
        self.tail_samples
    }

    fn process_block(
        &mut self,
        samples: &mut [f32],
        auxiliary_input_samples: &[f32],
        parameter_changes: &[(u32, u32, f64)],
    ) -> Result<(), String> {
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("VST2 Mixer effect input must contain non-empty stereo frames".to_owned());
        }
        let expected_auxiliary_sample_count = self
            .auxiliary_input_buses
            .len()
            .checked_mul(samples.len())
            .ok_or_else(|| "VST2 Mixer sidechain block size overflow".to_owned())?;
        if auxiliary_input_samples.len() != expected_auxiliary_sample_count {
            return Err("VST2 Mixer sidechain block has an invalid size".to_owned());
        }
        let frames = samples.len() / 2;
        if frames > self.quantum {
            return Err(format!(
                "VST2 Mixer effect block has {frames} frames; maximum is {}",
                self.quantum
            ));
        }
        if parameter_changes.len() > MAX_EVENTS {
            return Err(format!(
                "VST2 Mixer effect block has {} parameter changes; maximum is {MAX_EVENTS}",
                parameter_changes.len()
            ));
        }

        self.parameter_events.clear();
        if let Some((revision, updates)) = self
            .parameter_sync
            .updates_since(self.parameter_sync_key, self.parameter_sync_revision)
        {
            self.parameter_sync_revision = revision;
            for (parameter_id, value) in &updates {
                self.note_tail_parameter(*parameter_id, *value);
            }
            self.pending_parameter_updates.extend(updates);
        }
        let remaining_parameter_events = MAX_EVENTS.saturating_sub(parameter_changes.len());
        let pending_updates = self
            .pending_parameter_updates
            .iter()
            .take(remaining_parameter_events)
            .map(|(parameter_id, value)| (*parameter_id, *value))
            .collect::<Vec<_>>();
        for (parameter_id, value) in pending_updates {
            self.pending_parameter_updates.remove(&parameter_id);
            self.parameter_events.push(AudioEvent {
                sample_offset: 0,
                bus: 0,
                data: EventData::Parameter {
                    id: parameter_id,
                    value,
                },
            });
        }
        for (sample_offset, parameter_id, value) in parameter_changes {
            if *sample_offset as usize >= frames
                || !value.is_finite()
                || !(0.0..=1.0).contains(value)
            {
                return Err("VST2 Mixer effect received an invalid parameter change".to_owned());
            }
            self.note_tail_parameter(*parameter_id, *value);
            self.parameter_events.push(AudioEvent {
                sample_offset: *sample_offset,
                bus: 0,
                data: EventData::Parameter {
                    id: *parameter_id,
                    value: *value,
                },
            });
        }
        self.parameter_events
            .sort_by_key(|event| event.sample_offset);

        copy_vst2_mixer_stereo_bus(
            &mut self.input_buffers,
            self.main_input_offset,
            self.input_channels,
            samples,
            frames,
        )?;
        for (bus_position, ((_, channels), channel_offset)) in self
            .auxiliary_input_buses
            .iter()
            .zip(self.auxiliary_input_channel_offsets.iter())
            .enumerate()
        {
            let bus_start = bus_position
                .checked_mul(samples.len())
                .ok_or_else(|| "VST2 Mixer sidechain offset overflow".to_owned())?;
            let bus_end = bus_start
                .checked_add(samples.len())
                .ok_or_else(|| "VST2 Mixer sidechain block end overflow".to_owned())?;
            let bus_samples = auxiliary_input_samples
                .get(bus_start..bus_end)
                .ok_or_else(|| "VST2 Mixer sidechain block has an invalid size".to_owned())?;
            copy_vst2_mixer_stereo_bus(
                &mut self.input_buffers,
                *channel_offset,
                *channels,
                bus_samples,
                frames,
            )?;
        }
        self.output_left[..frames].fill(0.0);
        self.output_right[..frames].fill(0.0);

        let mut inputs = [&[][..]; MAX_VST2_MIXER_EFFECT_INPUT_CHANNELS];
        for (input, buffer) in inputs.iter_mut().zip(self.input_buffers.iter()) {
            *input = &buffer[..frames];
        }
        let mut outputs: [&mut [f32]; 2] = [&mut [], &mut []];
        outputs[0] = &mut self.output_left[..frames];
        if self.output_channels == 2 {
            outputs[1] = &mut self.output_right[..frames];
        }
        let report = self.audio.process_events(
            frames,
            &inputs[..self.input_buffers.len()],
            &mut outputs[..self.output_channels],
            self.transport,
            &self.parameter_events,
            &mut [],
        );
        match report.status {
            AudioStatus::Processed => self.startup_fallbacks = 0,
            AudioStatus::Fallback if self.startup_fallbacks > 0 => {
                self.startup_fallbacks -= 1;
            }
            status => {
                return Err(format!("VST2 Mixer effect processing failed: {status:?}"));
            }
        }
        if report.events_dropped != 0 {
            return Err(format!(
                "VST2 Mixer effect dropped {} parameter event(s)",
                report.events_dropped
            ));
        }
        for (index, stereo) in samples.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let left = finite_sample(self.output_left[index]);
            let right = if self.output_channels == 1 {
                left
            } else {
                finite_sample(self.output_right[index])
            };
            *stereo = [left, right];
        }
        self.transport = self.transport.advanced(frames, self.sample_rate);
        self.refresh_tail_samples_if_needed();
        Ok(())
    }

    fn stop_processing(&mut self) -> Result<(), String> {
        Ok(())
    }
}

fn probe_vst2_mixer_effect_tail_samples(
    assignment: &Vst2ProjectMixerEffectAssignment,
    sample_rate: u32,
) -> Option<u64> {
    run_vst2_tail_probe(assignment, sample_rate, None)
}

/// Probes the tail with the plug-in's current parameter values applied on
/// top of its saved state, so edits made after the state was saved are
/// reflected in the reported tail.
fn probe_vst2_mixer_effect_tail_samples_with_parameters(
    assignment: &Vst2ProjectMixerEffectAssignment,
    sample_rate: u32,
    parameters: &BTreeMap<u32, f64>,
) -> Option<u64> {
    if parameters.is_empty() {
        return probe_vst2_mixer_effect_tail_samples(assignment, sample_rate);
    }
    if parameters.len() > MAX_VST2_TAIL_PROBE_PARAMETERS {
        return None;
    }
    let parameters = parameters
        .iter()
        .map(|(parameter_id, value)| (*parameter_id, *value))
        .collect::<Vec<_>>();
    let input = encode_vst2_tail_probe_input(&assignment.state_bytes, &parameters);
    run_vst2_tail_probe(assignment, sample_rate, Some(input))
}

fn run_vst2_tail_probe(
    assignment: &Vst2ProjectMixerEffectAssignment,
    sample_rate: u32,
    framed_input: Option<Vec<u8>>,
) -> Option<u64> {
    if assignment.state_bytes.len() > MAX_VST2_TAIL_PROBE_STATE_BYTES {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let class_id = assignment
        .class_id
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let mut command = ProcessCommand::new(executable);
    command
        .arg(VST2_TAIL_PROBE_FLAG)
        .arg(&assignment.plugin_path)
        .arg(class_id)
        .arg(sample_rate.to_string());
    if framed_input.is_some() {
        command.arg(VST2_TAIL_PROBE_FRAMED_ARGUMENT);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let payload = framed_input.as_deref().unwrap_or(&assignment.state_bytes);
    if stdin.write_all(payload).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    drop(stdin);

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = match thread::Builder::new()
        .name("vst2-tail-probe-output".to_owned())
        .spawn(move || {
            let _ = sender.send(read_vst2_tail_probe_output(stdout));
        }) {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    let deadline = Instant::now() + VST2_TAIL_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let output = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;
    if reader.join().is_err() || !status.success() {
        return None;
    }
    let output = String::from_utf8_lossy(&output);
    let response = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(VST2_TAIL_PROBE_MARKER))?;
    response
        .strip_prefix("samples=")
        .and_then(|samples| samples.parse::<u64>().ok())
}

fn read_vst2_tail_probe_output(mut stdout: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let length = stdout.read(&mut chunk)?;
        if length == 0 {
            break;
        }
        let excess = output
            .len()
            .saturating_add(length)
            .saturating_sub(MAX_VST2_TAIL_PROBE_OUTPUT_BYTES);
        if excess >= output.len() {
            output.clear();
        } else if excess != 0 {
            output.drain(..excess);
        }
        output.extend_from_slice(&chunk[..length]);
    }
    Ok(output)
}

impl MixerEffectProcessorFactory for Vst2ProjectMixerEffectProcessorFactory {
    fn contains(&self, insert_ordinal: usize, slot_index: u16) -> bool {
        self.assignments.contains_key(&(insert_ordinal, slot_index))
    }

    fn create(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        sample_rate: u32,
    ) -> Result<Option<Box<dyn MixerEffectProcessor>>, String> {
        let Some(assignment) = self.assignments.get(&(insert_ordinal, slot_index)) else {
            return Ok(None);
        };
        let processor = Vst2MixerEffectProcessor::spawn(
            assignment,
            (insert_ordinal, slot_index),
            sample_rate,
            self.parameter_sync.clone(),
        )?;
        Ok(Some(Box::new(processor)))
    }
}

/// Render one FLP Pattern channel through its matching VST2 plug-in and saved field-53 state.
///
/// Pattern timing, note keys, velocity, MIDI channel, and project swing are applied. VST2
/// receives standard MIDI note events, so FLP per-note pitch, pan, release, and modulation
/// expression are not represented by this renderer.
pub fn render_vst2_pattern_channel_to_wav(
    plugin_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    state: &ChannelPluginState,
    notes: &[PatternNote],
    options: Vst2PatternRenderOptions,
) -> Result<Vst2PatternRenderSummary, String> {
    render_vst2_pattern_channel_to_wav_with_automation(
        plugin_path,
        output_path,
        state,
        notes,
        &[],
        options,
    )
}

/// Render one FLP Pattern channel through its matching VST2 plug-in, saved field-53 state, and
/// optional normalized parameter automation lanes.
pub fn render_vst2_pattern_channel_to_wav_with_automation(
    plugin_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    state: &ChannelPluginState,
    notes: &[PatternNote],
    parameter_automation: &[Vst2PatternParameterAutomation],
    options: Vst2PatternRenderOptions,
) -> Result<Vst2PatternRenderSummary, String> {
    let plugin_path = plugin_path.as_ref();
    let output_path = output_path.as_ref();
    if state.channel_id() != options.channel_id {
        return Err(format!(
            "VST2 state belongs to channel {}, not channel {}",
            state.channel_id(),
            options.channel_id
        ));
    }
    if !output_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err("VST2 Pattern output must use the .wav extension".to_owned());
    }
    if !(8_000..=384_000).contains(&options.sample_rate) {
        return Err("sample rate must be between 8,000 and 384,000 Hz".to_owned());
    }
    let (mut scheduled_events, note_count) = prepare_vst2_pattern_events(notes, options)?;
    let scheduled_parameter_lanes =
        prepare_vst2_pattern_parameter_automation(parameter_automation, options)?;
    if note_count == 0 {
        return Err(format!(
            "pattern channel {} has no unmuted notes to render",
            options.channel_id
        ));
    }
    let final_note_frame = scheduled_events
        .iter()
        .map(|event| event.frame)
        .max()
        .ok_or_else(|| "pattern channel has no renderable notes".to_owned())?;
    let tail_frames = seconds_to_frames(options.tail_seconds, options.sample_rate)?;
    let content_frames = final_note_frame
        .checked_add(tail_frames as u64)
        .and_then(|frames| frames.checked_add(1))
        .ok_or_else(|| "VST2 Pattern render frame count overflow".to_owned())?;
    if content_frames as f64 / f64::from(options.sample_rate) > MAX_RENDER_SECONDS {
        return Err(format!(
            "VST2 Pattern render must be at most {MAX_RENDER_SECONDS} seconds"
        ));
    }

    let matched = match_vst2_channel_class(plugin_path, state)?;
    let state_bytes = state
        .vst_state_bytes()
        .ok_or_else(|| "FLP VST2 channel state has no nested field 53".to_owned())?;
    let descriptor = PluginDescriptor {
        path: plugin_path.to_path_buf(),
        format: PluginFormat::Vst2,
        class_id: Some(matched.class.id),
    };
    let host = host_for_current_executable()?;
    let configuration = Configuration {
        sample_rate: options.sample_rate,
        quantum: RENDER_QUANTUM,
        max_callback_frames: Some(RENDER_QUANTUM),
        offline: true,
        buses: Some(BusSelector::main_only()),
        ..Configuration::default()
    };
    let mut instance = host
        .spawn(&descriptor, &configuration, Some(state_bytes))
        .map_err(|error| format!("could not restore and start FLP VST2 plug-in: {error}"))?;
    let mut audio = instance
        .take_audio()
        .ok_or_else(|| "VST2 worker did not provide an audio endpoint".to_owned())?;
    let geometry = audio.geometry();
    let output_channel_count = geometry.output_channels as usize;
    let captured_channel_count = output_channel_count.min(2);
    if captured_channel_count == 0 {
        return Err("VST2 plug-in has no active main output bus".to_owned());
    }
    let input_channel_count = geometry.input_channels as usize;
    let latency_frames = audio.latency_frames();
    let frames_with_latency = content_frames
        .checked_add(latency_frames as u64)
        .ok_or_else(|| "VST2 Pattern render frame count overflow".to_owned())?;
    let quantum = RENDER_QUANTUM as usize;
    let process_frames = usize::try_from(frames_with_latency)
        .ok()
        .and_then(|frames| frames.checked_add(quantum - 1))
        .map(|frames| frames / quantum * quantum)
        .ok_or_else(|| "VST2 Pattern render frame count overflow".to_owned())?;
    let buffer_bytes = process_frames
        .checked_mul(captured_channel_count)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "VST2 Pattern render buffer size overflow".to_owned())?;
    if buffer_bytes > MAX_RENDER_BUFFER_BYTES {
        return Err(format!(
            "VST2 Pattern render needs {buffer_bytes} bytes of audio storage; limit is {MAX_RENDER_BUFFER_BYTES}"
        ));
    }

    scheduled_events.sort_by_key(|event| (event.frame, event.priority));
    let mut next_event = 0usize;
    let mut captured = (0..captured_channel_count)
        .map(|_| Vec::with_capacity(process_frames))
        .collect::<Vec<_>>();
    let mut transport = Transport {
        tempo: options.tempo_bpm,
        ..Transport::default()
    };
    let mut startup_fallbacks = latency_frames.div_ceil(quantum);
    for block_start in (0..process_frames).step_by(quantum) {
        let block_end = (block_start + quantum) as u64;
        let mut events = Vec::new();
        for lane in &scheduled_parameter_lanes {
            if let Some(value) =
                vst2_pattern_parameter_value_at_frame(&lane.points, block_start as u64)
            {
                events.push(AudioEvent {
                    sample_offset: 0,
                    bus: 0,
                    data: EventData::Parameter {
                        id: lane.parameter_id,
                        value,
                    },
                });
            }
            let first_in_block = lane
                .points
                .partition_point(|point| point.frame <= block_start as u64);
            for point in &lane.points[first_in_block..] {
                if point.frame >= block_end {
                    break;
                }
                let sample_offset =
                    u32::try_from(point.frame - block_start as u64).map_err(|_| {
                        "VST2 Pattern parameter offset is outside the audio block".to_owned()
                    })?;
                events.push(AudioEvent {
                    sample_offset,
                    bus: 0,
                    data: EventData::Parameter {
                        id: lane.parameter_id,
                        value: point.value,
                    },
                });
            }
        }
        while scheduled_events
            .get(next_event)
            .is_some_and(|event| event.frame < block_end)
        {
            let event = scheduled_events[next_event];
            let offset = u32::try_from(event.frame.saturating_sub(block_start as u64))
                .map_err(|_| "VST2 Pattern MIDI event offset is outside the block".to_owned())?;
            events.push(AudioEvent::midi(offset, 0, event.data));
            next_event += 1;
        }
        events.sort_by_key(|event| event.sample_offset);
        if events.len() > MAX_EVENTS {
            return Err(format!(
                "VST2 worker accepts at most {MAX_EVENTS} events in one audio block"
            ));
        }
        let inputs = (0..input_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let input_slices = inputs.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = (0..output_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let mut output_slices = outputs
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        let report = audio.process_events(
            quantum,
            &input_slices,
            &mut output_slices,
            transport,
            &events,
            &mut [],
        );
        match report.status {
            AudioStatus::Processed if report.events_dropped == 0 => {
                startup_fallbacks = 0;
            }
            AudioStatus::Processed => {
                return Err(format!(
                    "VST2 worker dropped {} Pattern MIDI events in block {}",
                    report.events_dropped,
                    block_start / quantum
                ));
            }
            AudioStatus::Fallback if startup_fallbacks > 0 => startup_fallbacks -= 1,
            status => {
                return Err(format!(
                    "VST2 worker did not process Pattern block {}: {status:?}",
                    block_start / quantum
                ));
            }
        }
        for channel in 0..captured_channel_count {
            captured[channel].extend_from_slice(&outputs[channel]);
        }
        transport = transport.advanced(quantum, options.sample_rate);
        thread::sleep(Duration::from_secs_f64(
            f64::from(RENDER_QUANTUM) / f64::from(options.sample_rate) * 3.0,
        ));
    }

    let end = latency_frames
        .checked_add(content_frames as usize)
        .ok_or_else(|| "VST2 Pattern render slice overflow".to_owned())?;
    if captured.iter().any(|channel| channel.len() < end) {
        return Err("VST2 Pattern render did not produce the requested audio range".to_owned());
    }
    let channels = captured
        .into_iter()
        .map(|channel| channel[latency_frames..end].to_vec())
        .collect::<Vec<_>>();
    let peak = channels
        .iter()
        .flat_map(|channel| channel.iter())
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    write_audio_range_to_wav(
        &DecodedAudio {
            sample_rate: options.sample_rate,
            channels,
        },
        0..content_frames as usize,
        output_path,
    )?;

    Ok(Vst2PatternRenderSummary {
        plugin_name: matched.class.name,
        vendor: matched.class.vendor,
        notes_rendered: note_count,
        sample_rate: options.sample_rate,
        frames: content_frames as usize,
        latency_frames,
        channels: captured_channel_count,
        peak,
    })
}

fn prepare_vst2_pattern_events(
    notes: &[PatternNote],
    options: Vst2PatternRenderOptions,
) -> Result<(Vec<ScheduledVst2MidiEvent>, usize), String> {
    if options.ppq == 0 {
        return Err("project PPQ must be greater than zero".to_owned());
    }
    if !options.tempo_bpm.is_finite() || options.tempo_bpm <= 0.0 {
        return Err("project tempo must be finite and positive".to_owned());
    }
    if !options.tail_seconds.is_finite() || !(0.0..=60.0).contains(&options.tail_seconds) {
        return Err("VST2 Pattern tail must be between 0 and 60 seconds".to_owned());
    }
    if !(-12..=12).contains(&options.note_pitch_offset_semitones) {
        return Err("VST2 note pitch offset must be between -12 and 12 semitones".to_owned());
    }
    let matching_notes = notes
        .iter()
        .filter(|note| note.channel_id == options.channel_id && !note.is_muted())
        .collect::<Vec<_>>();
    if matching_notes.len() > 2_000_000 {
        return Err("Pattern channel expands to more than 2000000 notes".to_owned());
    }
    let mut events = Vec::with_capacity(matching_notes.len().saturating_mul(2));
    let mut note_count = 0usize;
    for note in matching_notes {
        if note.key > 127 {
            return Err(format!(
                "Pattern channel {} contains key {} outside the MIDI note range",
                options.channel_id, note.key
            ));
        }
        if note.length == 0 {
            return Err(format!(
                "Pattern channel {} contains a zero-length note",
                options.channel_id
            ));
        }
        if note.velocity > 127 {
            return Err(format!(
                "Pattern channel {} contains velocity {} outside the MIDI range",
                options.channel_id, note.velocity
            ));
        }
        let key = i32::from(note.key) + i32::from(options.note_pitch_offset_semitones);
        if !(0..=127).contains(&key) {
            return Err(format!(
                "Pattern key {} with project pitch offset is outside the MIDI note range",
                note.key
            ));
        }
        let midi_channel = note.midi_channel_index();
        let nominal_start_tick = u64::from(note.position);
        let start_tick = swing_note_start_tick(
            nominal_start_tick,
            options.ppq,
            options.global_swing_mix_raw,
            options.channel_swing_mix_raw,
        )?;
        let swing_offset = start_tick.saturating_sub(nominal_start_tick);
        let end_tick = nominal_start_tick
            .checked_add(u64::from(note.length))
            .and_then(|tick| tick.checked_add(swing_offset))
            .ok_or_else(|| "Pattern note end position overflow".to_owned())?;
        let frame_for_tick = |tick: u64| {
            ((tick as f64 / f64::from(options.ppq))
                * (60.0 / options.tempo_bpm)
                * f64::from(options.sample_rate))
            .round()
        };
        let start_frame = frame_for_tick(start_tick);
        let end_frame = frame_for_tick(end_tick);
        if !start_frame.is_finite()
            || !end_frame.is_finite()
            || start_frame < 0.0
            || end_frame < start_frame
            || end_frame > u64::MAX as f64
        {
            return Err("Pattern note timing is outside the renderable range".to_owned());
        }
        let start_frame = start_frame as u64;
        let end_frame = (end_frame as u64).max(start_frame.saturating_add(1));
        let key = key as u8;
        let channel_status = midi_channel & 0x0f;
        events.push(ScheduledVst2MidiEvent {
            frame: start_frame,
            priority: 1,
            data: [0x90 | channel_status, key, note.velocity],
        });
        events.push(ScheduledVst2MidiEvent {
            frame: end_frame,
            priority: 0,
            data: [
                0x80 | channel_status,
                key,
                fl_note_release_to_midi_velocity(note.release),
            ],
        });
        note_count += 1;
    }
    events.sort_by_key(|event| (event.frame, event.priority));
    Ok((events, note_count))
}

fn prepare_vst2_pattern_parameter_automation(
    automation: &[Vst2PatternParameterAutomation],
    options: Vst2PatternRenderOptions,
) -> Result<Vec<ScheduledVst2ParameterLane>, String> {
    let mut parameter_ids = BTreeSet::new();
    let mut lanes = Vec::with_capacity(automation.len());
    for lane in automation {
        if !parameter_ids.insert(lane.parameter_id) {
            return Err(format!(
                "VST2 parameter {} has more than one Pattern Event link",
                lane.parameter_id
            ));
        }
        let mut points = Vec::with_capacity(lane.points.len());
        for point in &lane.points {
            if !point.value.is_finite() || !(0.0..=1.0).contains(&point.value) {
                return Err(
                    "VST2 Pattern parameter values must be finite and normalized to 0..=1"
                        .to_owned(),
                );
            }
            let frame = point.tick as f64 * 60.0 * f64::from(options.sample_rate)
                / (f64::from(options.ppq) * options.tempo_bpm);
            if !frame.is_finite() || frame < 0.0 || frame.round() > u64::MAX as f64 {
                return Err(
                    "VST2 Pattern parameter timing is outside the renderable range".to_owned(),
                );
            }
            points.push(ScheduledVst2ParameterPoint {
                frame: frame.round() as u64,
                value: point.value,
            });
        }
        points.sort_by_key(|point| point.frame);
        let mut unique_points: Vec<ScheduledVst2ParameterPoint> = Vec::with_capacity(points.len());
        for point in points {
            if let Some(previous) = unique_points.last_mut()
                && previous.frame == point.frame
            {
                previous.value = point.value;
            } else {
                unique_points.push(point);
            }
        }
        if !unique_points.is_empty() {
            lanes.push(ScheduledVst2ParameterLane {
                parameter_id: lane.parameter_id,
                points: unique_points,
            });
        }
    }
    Ok(lanes)
}

fn vst2_pattern_parameter_value_at_frame(
    points: &[ScheduledVst2ParameterPoint],
    frame: u64,
) -> Option<f64> {
    let previous_index = points
        .partition_point(|point| point.frame <= frame)
        .checked_sub(1)?;
    let previous = points.get(previous_index)?;
    if previous.frame == frame {
        return Some(previous.value);
    }
    let next = points.get(previous_index + 1)?;
    let span = next.frame.saturating_sub(previous.frame);
    if span == 0 {
        return Some(next.value);
    }
    let amount = (frame - previous.frame) as f64 / span as f64;
    Some(previous.value + (next.value - previous.value) * amount)
}

fn save_worker_vst2_state(
    instance: &Instance,
    mut handle_unrelated_event: impl FnMut(HostEvent),
) -> Result<Vec<u8>, String> {
    let controller = instance.controller();
    let request = controller
        .request(Command::SaveState)
        .map_err(|error| format!("could not request VST2 state snapshot: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = instance.try_next_event() {
            match event {
                HostEvent::State {
                    request: Some(state_request),
                    bytes,
                } if state_request == request => return Ok(bytes),
                HostEvent::RequestFailed {
                    request: failed_request,
                    reason,
                } if failed_request == request => {
                    return Err(format!("VST2 state snapshot failed: {reason}"));
                }
                HostEvent::Failed { reason } => {
                    return Err(format!(
                        "VST2 worker failed during state snapshot: {reason}"
                    ));
                }
                HostEvent::RecoveryRequired | HostEvent::Stopped => {
                    return Err("VST2 worker stopped during state snapshot".to_owned());
                }
                event => handle_unrelated_event(event),
            }
        } else if Instant::now() >= deadline {
            return Err("VST2 state snapshot timed out".to_owned());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn is_vst2_plugin_path(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }

    #[cfg(target_os = "macos")]
    {
        path.is_dir()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("vst"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Render a MIDI note through the first class exposed by a VST2 plug-in.
///
/// The plug-in's first main output bus is rendered. At most its first two
/// channels are written, and worker misses after the startup latency fail the
/// render instead of silently writing fallback audio.
pub fn render_vst2_note_to_wav(
    plugin_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: Vst2NoteRenderOptions,
) -> Result<Vst2NoteRenderSummary, String> {
    validate_render_options(options)?;
    let plugin_path = plugin_path.as_ref();
    let output_path = output_path.as_ref();
    let classes = inspect_vst2_plugin(plugin_path)?;
    let class = classes
        .into_iter()
        .next()
        .ok_or_else(|| "VST2 plug-in did not report a loadable class".to_owned())?;
    let descriptor = PluginDescriptor {
        path: plugin_path.to_path_buf(),
        format: PluginFormat::Vst2,
        class_id: Some(class.id),
    };
    let host = host_for_current_executable()?;
    let configuration = Configuration {
        sample_rate: options.sample_rate,
        quantum: RENDER_QUANTUM,
        max_callback_frames: Some(RENDER_QUANTUM),
        offline: true,
        buses: Some(BusSelector::main_only()),
        ..Configuration::default()
    };
    let mut instance = host
        .spawn(&descriptor, &configuration, None)
        .map_err(|error| format!("could not start VST2 plug-in: {error}"))?;
    let mut audio = instance
        .take_audio()
        .ok_or_else(|| "VST2 worker did not provide an audio endpoint".to_owned())?;
    let geometry = audio.geometry();
    let output_channel_count = geometry.output_channels as usize;
    let captured_channel_count = output_channel_count.min(2);
    if output_channel_count == 0 || captured_channel_count == 0 {
        return Err("VST2 plug-in has no active main output bus".to_owned());
    }
    let input_channel_count = geometry.input_channels as usize;
    let note_frames = seconds_to_frames(options.note_seconds, options.sample_rate)?;
    let tail_frames = seconds_to_frames(options.tail_seconds, options.sample_rate)?;
    let content_frames = note_frames
        .checked_add(tail_frames)
        .ok_or_else(|| "VST2 render frame count overflow".to_owned())?;
    let latency_frames = audio.latency_frames();
    let frames_with_latency = content_frames
        .checked_add(latency_frames)
        .ok_or_else(|| "VST2 render frame count overflow".to_owned())?;
    let quantum = RENDER_QUANTUM as usize;
    let process_frames = frames_with_latency
        .checked_add(quantum - 1)
        .map(|frames| frames / quantum * quantum)
        .ok_or_else(|| "VST2 render frame count overflow".to_owned())?;
    let buffer_bytes = process_frames
        .checked_mul(captured_channel_count)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "VST2 render buffer size overflow".to_owned())?;
    if buffer_bytes > MAX_RENDER_BUFFER_BYTES {
        return Err(format!(
            "VST2 render needs {buffer_bytes} bytes of audio storage; limit is {MAX_RENDER_BUFFER_BYTES}"
        ));
    }

    let mut captured = (0..captured_channel_count)
        .map(|_| Vec::with_capacity(process_frames))
        .collect::<Vec<_>>();
    let mut transport = Transport::default();
    let mut startup_fallbacks = latency_frames.div_ceil(quantum);
    for block_start in (0..process_frames).step_by(quantum) {
        let mut events = Vec::with_capacity(2);
        if block_start == 0 {
            events.push(AudioEvent::midi(
                0,
                0,
                [0x90, options.midi_note, options.velocity],
            ));
        }
        if note_frames >= block_start && note_frames < block_start + quantum {
            events.push(AudioEvent::midi(
                (note_frames - block_start) as u32,
                0,
                [0x80, options.midi_note, 0],
            ));
        }

        let inputs = (0..input_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let input_slices = inputs.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = (0..output_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let mut output_slices = outputs
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        let report = audio.process_events(
            quantum,
            &input_slices,
            &mut output_slices,
            transport,
            &events,
            &mut [],
        );
        match report.status {
            AudioStatus::Processed => startup_fallbacks = 0,
            AudioStatus::Fallback if startup_fallbacks > 0 => startup_fallbacks -= 1,
            status => {
                return Err(format!(
                    "VST2 worker did not process block {}: {status:?}",
                    block_start / quantum
                ));
            }
        }
        for channel in 0..captured_channel_count {
            captured[channel].extend_from_slice(&outputs[channel]);
        }
        transport = transport.advanced(quantum, options.sample_rate);

        // The public block endpoint is nonblocking. Give its worker several
        // callback periods to complete each block before submitting the next.
        thread::sleep(Duration::from_secs_f64(
            f64::from(RENDER_QUANTUM) / f64::from(options.sample_rate) * 3.0,
        ));
    }

    let end = latency_frames
        .checked_add(content_frames)
        .ok_or_else(|| "VST2 render slice overflow".to_owned())?;
    if captured.iter().any(|channel| channel.len() < end) {
        return Err("VST2 render did not produce the requested audio range".to_owned());
    }
    let channels = captured
        .into_iter()
        .map(|channel| channel[latency_frames..end].to_vec())
        .collect::<Vec<_>>();
    let peak = channels
        .iter()
        .flat_map(|channel| channel.iter())
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    write_audio_range_to_wav(
        &DecodedAudio {
            sample_rate: options.sample_rate,
            channels,
        },
        0..content_frames,
        output_path,
    )?;

    Ok(Vst2NoteRenderSummary {
        plugin_name: class.name,
        vendor: class.vendor,
        sample_rate: options.sample_rate,
        frames: content_frames,
        latency_frames,
        channels: captured_channel_count,
        peak,
    })
}

/// Process a mono or stereo file through the first class and first main output bus
/// exposed by a VST2 effect, then write at most two output channels to WAV.
pub fn render_audio_through_vst2_effect_to_wav(
    input_path: impl AsRef<Path>,
    plugin_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: Vst2AudioEffectRenderOptions,
) -> Result<Vst2AudioEffectRenderSummary, String> {
    let output_path = output_path.as_ref();
    if !output_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err("VST2 effect output must use the .wav extension".to_owned());
    }
    let audio = decode_audio_file(input_path)?;
    if !(8_000..=384_000).contains(&audio.sample_rate) {
        return Err("audio sample rate must be between 8000 and 384000 Hz".to_owned());
    }
    if audio.channels.is_empty() || audio.channels.len() > 2 {
        return Err("VST2 effect rendering requires mono or stereo input audio".to_owned());
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
    if !options.tail_seconds.is_finite() || !(0.0..=60.0).contains(&options.tail_seconds) {
        return Err("VST2 effect tail must be between 0 and 60 seconds".to_owned());
    }

    let plugin_path = plugin_path.as_ref();
    let class = inspect_vst2_plugin(plugin_path)?
        .into_iter()
        .next()
        .ok_or_else(|| "VST2 plug-in did not report a loadable class".to_owned())?;
    let descriptor = PluginDescriptor {
        path: plugin_path.to_path_buf(),
        format: PluginFormat::Vst2,
        class_id: Some(class.id),
    };
    let host = host_for_current_executable()?;
    let configuration = Configuration {
        sample_rate: audio.sample_rate,
        quantum: RENDER_QUANTUM,
        max_callback_frames: Some(RENDER_QUANTUM),
        offline: true,
        buses: Some(BusSelector::main_only()),
        ..Configuration::default()
    };
    let mut instance = host
        .spawn(&descriptor, &configuration, None)
        .map_err(|error| format!("could not start VST2 effect: {error}"))?;
    let mut endpoint = instance
        .take_audio()
        .ok_or_else(|| "VST2 worker did not provide an audio endpoint".to_owned())?;
    let geometry = endpoint.geometry();
    let input_channel_count = geometry.input_channels as usize;
    let output_channel_count = geometry.output_channels as usize;
    let captured_channel_count = output_channel_count.min(2);
    if input_channel_count == 0 || captured_channel_count == 0 {
        return Err("VST2 plug-in must expose active main audio input and output buses".to_owned());
    }

    let tail_frames = seconds_to_frames(options.tail_seconds, audio.sample_rate)?;
    let content_frames = input_frames
        .checked_add(tail_frames)
        .ok_or_else(|| "VST2 effect render frame count overflow".to_owned())?;
    if content_frames as f64 / f64::from(audio.sample_rate) > MAX_RENDER_SECONDS {
        return Err(format!(
            "VST2 effect input plus tail must be at most {MAX_RENDER_SECONDS} seconds"
        ));
    }
    let latency_frames = endpoint.latency_frames();
    let frames_with_latency = content_frames
        .checked_add(latency_frames)
        .ok_or_else(|| "VST2 effect render frame count overflow".to_owned())?;
    let quantum = RENDER_QUANTUM as usize;
    let process_frames = frames_with_latency
        .checked_add(quantum - 1)
        .map(|frames| frames / quantum * quantum)
        .ok_or_else(|| "VST2 effect render frame count overflow".to_owned())?;
    let buffer_bytes = process_frames
        .checked_mul(captured_channel_count)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "VST2 effect render buffer size overflow".to_owned())?;
    if buffer_bytes > MAX_RENDER_BUFFER_BYTES {
        return Err(format!(
            "VST2 effect render needs {buffer_bytes} bytes of audio storage; limit is {MAX_RENDER_BUFFER_BYTES}"
        ));
    }

    let mut captured = (0..captured_channel_count)
        .map(|_| Vec::with_capacity(process_frames))
        .collect::<Vec<_>>();
    let mut transport = Transport::default();
    let mut startup_fallbacks = latency_frames.div_ceil(quantum);
    for block_start in (0..process_frames).step_by(quantum) {
        let mut inputs = (0..input_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let (first_input, remaining_inputs) = inputs
            .split_first_mut()
            .ok_or_else(|| "VST2 plug-in has no active main input bus".to_owned())?;
        if let Some(second_input) = remaining_inputs.first_mut() {
            for (frame, (left_slot, right_slot)) in first_input
                .iter_mut()
                .zip(second_input.iter_mut())
                .enumerate()
            {
                let source_frame = block_start + frame;
                if source_frame >= input_frames {
                    break;
                }
                *left_slot = finite_sample(audio.channels[0][source_frame]);
                *right_slot = audio
                    .channels
                    .get(1)
                    .map_or(*left_slot, |channel| finite_sample(channel[source_frame]));
            }
        } else {
            for (frame, input_slot) in first_input.iter_mut().enumerate() {
                let source_frame = block_start + frame;
                if source_frame >= input_frames {
                    break;
                }
                let left = finite_sample(audio.channels[0][source_frame]);
                let right = audio
                    .channels
                    .get(1)
                    .map_or(left, |channel| finite_sample(channel[source_frame]));
                *input_slot = (left + right) * 0.5;
            }
        }
        let input_slices = inputs.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = (0..output_channel_count)
            .map(|_| vec![0.0f32; quantum])
            .collect::<Vec<_>>();
        let mut output_slices = outputs
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        let report = endpoint.process_events(
            quantum,
            &input_slices,
            &mut output_slices,
            transport,
            &[],
            &mut [],
        );
        match report.status {
            AudioStatus::Processed => startup_fallbacks = 0,
            AudioStatus::Fallback if startup_fallbacks > 0 => startup_fallbacks -= 1,
            status => {
                return Err(format!(
                    "VST2 worker did not process block {}: {status:?}",
                    block_start / quantum
                ));
            }
        }
        for channel in 0..captured_channel_count {
            captured[channel].extend_from_slice(&outputs[channel]);
        }
        transport = transport.advanced(quantum, audio.sample_rate);
        thread::sleep(Duration::from_secs_f64(
            f64::from(RENDER_QUANTUM) / f64::from(audio.sample_rate) * 3.0,
        ));
    }

    let end = latency_frames
        .checked_add(content_frames)
        .ok_or_else(|| "VST2 effect render slice overflow".to_owned())?;
    if captured.iter().any(|channel| channel.len() < end) {
        return Err("VST2 effect render did not produce the requested audio range".to_owned());
    }
    let channels = captured
        .into_iter()
        .map(|channel| channel[latency_frames..end].to_vec())
        .collect::<Vec<_>>();
    let peak = channels
        .iter()
        .flat_map(|channel| channel.iter())
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    write_audio_range_to_wav(
        &DecodedAudio {
            sample_rate: audio.sample_rate,
            channels,
        },
        0..content_frames,
        output_path,
    )?;

    Ok(Vst2AudioEffectRenderSummary {
        plugin_name: class.name,
        vendor: class.vendor,
        input_frames,
        output_frames: content_frames,
        sample_rate: audio.sample_rate,
        latency_frames,
        tail_frames,
        channels: captured_channel_count,
        peak,
    })
}

fn host_for_current_executable() -> Result<Host, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the VST2 worker executable: {error}"))?;
    Ok(Host::new(executable))
}

fn validate_render_options(options: Vst2NoteRenderOptions) -> Result<(), String> {
    if !(8_000..=384_000).contains(&options.sample_rate) {
        return Err("sample rate must be between 8,000 and 384,000 Hz".to_owned());
    }
    if options.midi_note > 127 {
        return Err("MIDI note must be between 0 and 127".to_owned());
    }
    if !(1..=127).contains(&options.velocity) {
        return Err("MIDI velocity must be between 1 and 127".to_owned());
    }
    if !options.note_seconds.is_finite()
        || options.note_seconds <= 0.0
        || !options.tail_seconds.is_finite()
        || options.tail_seconds < 0.0
        || options.note_seconds + options.tail_seconds > MAX_RENDER_SECONDS
    {
        return Err(format!(
            "note plus tail duration must be finite, positive, and at most {MAX_RENDER_SECONDS} seconds"
        ));
    }
    Ok(())
}

fn seconds_to_frames(seconds: f64, sample_rate: u32) -> Result<usize, String> {
    let frames = (seconds * f64::from(sample_rate)).ceil();
    if !frames.is_finite() || frames < 0.0 || frames > usize::MAX as f64 {
        return Err("VST2 render frame count is outside the supported range".to_owned());
    }
    Ok(frames as usize)
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_probe_framing_round_trips_parameters_and_state() {
        let state = [9_u8, 8, 7, 6, 5];
        let parameters = [(3_u32, 0.5_f64), (7, 1.0), (3, 0.25)];
        let input = encode_vst2_tail_probe_input(&state, &parameters);
        let (decoded_parameters, decoded_state) =
            decode_vst2_tail_probe_input(&input).expect("framed input should decode");
        assert_eq!(decoded_parameters, parameters);
        assert_eq!(decoded_state, state);

        let empty = encode_vst2_tail_probe_input(&state, &[]);
        let (decoded_parameters, decoded_state) =
            decode_vst2_tail_probe_input(&empty).expect("parameterless frame should decode");
        assert!(decoded_parameters.is_empty());
        assert_eq!(decoded_state, state);
    }

    #[test]
    fn tail_probe_framing_rejects_raw_and_truncated_input() {
        // Legacy raw state is not framed input.
        assert!(decode_vst2_tail_probe_input(&[1, 2, 3, 4, 5, 6]).is_none());
        assert!(decode_vst2_tail_probe_input(&[]).is_none());
        // A frame cut short inside the parameter block does not parse.
        let input = encode_vst2_tail_probe_input(&[1, 2], &[(3, 0.5), (7, 1.0)]);
        assert!(decode_vst2_tail_probe_input(&input[..16]).is_none());
        // A parameter count beyond the cap does not parse.
        let mut oversized = Vec::new();
        oversized.extend_from_slice(VST2_TAIL_PROBE_FRAME_MAGIC);
        oversized.extend_from_slice(&((MAX_VST2_TAIL_PROBE_PARAMETERS + 1) as u32).to_le_bytes());
        assert!(decode_vst2_tail_probe_input(&oversized).is_none());
        // Non-finite parameter values do not parse.
        let input = encode_vst2_tail_probe_input(&[], &[(3, f64::NAN)]);
        assert!(decode_vst2_tail_probe_input(&input).is_none());
    }

    #[test]
    fn tail_refresh_starts_only_for_unprobed_revisions() {
        assert!(vst2_tail_refresh_should_start(0, 1, false));
        assert!(!vst2_tail_refresh_should_start(1, 1, false));
        assert!(!vst2_tail_refresh_should_start(2, 1, false));
        assert!(!vst2_tail_refresh_should_start(0, 1, true));
        assert!(!vst2_tail_refresh_should_start(0, 0, false));
    }

    #[test]
    fn tail_refresh_shared_slot_hands_results_back_once() {
        let shared = Vst2TailRefreshShared::default();
        assert!(shared.take_result().is_none());
        shared.store_result(4, Some(12_345));
        assert_eq!(shared.take_result(), Some((4, Some(12_345))));
        assert!(shared.take_result().is_none());
        shared.store_result(5, None);
        assert_eq!(shared.take_result(), Some((5, None)));
    }
}

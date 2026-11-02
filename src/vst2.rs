//! Isolated VST2 plug-in inspection and offline note rendering.
//!
//! The plug-in ABI and worker process are provided by `vst-audio`; the host
//! application only talks to its bounded process and audio APIs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::media::{DecodedAudio, decode_audio_file, write_audio_range_to_wav};
use crate::sample_render::swing_note_start_tick;
use crate::vst3::{MixerEffectProcessor, MixerEffectProcessorFactory};
use crate::{ChannelPluginState, PatternNote};
use vst_audio::{
    AudioEndpoint, AudioEvent, AudioStatus, BusSelector, ClassInfo, Command, Configuration,
    EditPhase, EventData, Host, HostEvent, Instance, MAX_EVENTS, PluginDescriptor, PluginFormat,
    ProcessReport, RequestId, Transport,
};

const RENDER_QUANTUM: u32 = 512;
const MAX_RENDER_SECONDS: f64 = 120.0;
const MAX_RENDER_BUFFER_BYTES: usize = 512 * 1024 * 1024;
const VST2_TAIL_PROBE_FLAG: &str = "--vst2-tail-probe";
const VST2_TAIL_PROBE_MARKER: &str = "FLP_VST2_TAIL_V1 ";
const VST2_TAIL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VST2_TAIL_PROBE_STATE_BYTES: usize = 64 * 1024 * 1024 - 9;
const MAX_VST2_TAIL_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
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
    if arguments.next().is_some() {
        return Err("unexpected VST2 tail probe argument".to_owned());
    }

    let mut state = Vec::new();
    std::io::stdin()
        .take((MAX_VST2_TAIL_PROBE_STATE_BYTES + 1) as u64)
        .read_to_end(&mut state)
        .map_err(|error| format!("could not read VST2 plug-in state: {error}"))?;
    if state.len() > MAX_VST2_TAIL_PROBE_STATE_BYTES {
        return Err("VST2 plug-in state exceeds the tail probe limit".to_owned());
    }

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
        let configuration = Configuration {
            sample_rate: self.sample_rate,
            quantum: self.quantum,
            max_callback_frames: Some(self.quantum),
            offline: false,
            buses: Some(BusSelector::main_only()),
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
        self.unload_mixer_effect(key);
        self.mixer_effects.insert(
            key,
            Vst2HostedMixerEffect {
                fourcc: assignment.fourcc.clone(),
                instance,
                _audio: audio,
            },
        );
        Ok(())
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

struct Vst2MixerEffectProcessor {
    _instance: Instance,
    audio: AudioEndpoint,
    sample_rate: u32,
    quantum: usize,
    input_channels: usize,
    output_channels: usize,
    latency_samples: u32,
    tail_samples: Option<u64>,
    startup_fallbacks: usize,
    transport: Transport,
    input_left: Vec<f32>,
    input_right: Vec<f32>,
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
        let tail_samples = probe_vst2_mixer_effect_tail_samples(assignment, sample_rate);
        let (parameter_sync_revision, pending_parameter_updates) = parameter_sync.snapshot(key);
        let configuration = Configuration {
            sample_rate,
            quantum: RENDER_QUANTUM,
            max_callback_frames: Some(RENDER_QUANTUM),
            offline: false,
            buses: Some(BusSelector::main_only()),
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
        let input_channels = geometry.input_channels as usize;
        let output_channels = geometry.output_channels as usize;
        if !matches!(input_channels, 1 | 2) || !matches!(output_channels, 1 | 2) {
            return Err(format!(
                "VST2 Mixer effect {} must expose mono or stereo main input and output buses",
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
            input_channels,
            output_channels,
            latency_samples,
            tail_samples,
            startup_fallbacks: (latency_samples as usize).div_ceil(quantum),
            transport: Transport::default(),
            input_left: vec![0.0; quantum],
            input_right: vec![0.0; quantum],
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
        if !auxiliary_input_samples.is_empty() {
            return Err("VST2 Mixer effects do not accept auxiliary input buses".to_owned());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("VST2 Mixer effect input must contain non-empty stereo frames".to_owned());
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

        if self.input_channels == 1 {
            for (mono, stereo) in self.input_left[..frames]
                .iter_mut()
                .zip(samples.as_chunks::<2>().0.iter())
            {
                *mono = finite_sample((stereo[0] + stereo[1]) * 0.5);
            }
        } else {
            for ((left, right), stereo) in self.input_left[..frames]
                .iter_mut()
                .zip(self.input_right[..frames].iter_mut())
                .zip(samples.as_chunks::<2>().0.iter())
            {
                *left = finite_sample(stereo[0]);
                *right = finite_sample(stereo[1]);
            }
        }
        self.output_left[..frames].fill(0.0);
        self.output_right[..frames].fill(0.0);

        let mut inputs = [&[][..]; 2];
        inputs[0] = &self.input_left[..frames];
        if self.input_channels == 2 {
            inputs[1] = &self.input_right[..frames];
        }
        let mut outputs: [&mut [f32]; 2] = [&mut [], &mut []];
        outputs[0] = &mut self.output_left[..frames];
        if self.output_channels == 2 {
            outputs[1] = &mut self.output_right[..frames];
        }
        let report = self.audio.process_events(
            frames,
            &inputs[..self.input_channels],
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
    if assignment.state_bytes.len() > MAX_VST2_TAIL_PROBE_STATE_BYTES {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let class_id = assignment
        .class_id
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let mut child = ProcessCommand::new(executable)
        .arg(VST2_TAIL_PROBE_FLAG)
        .arg(&assignment.plugin_path)
        .arg(class_id)
        .arg(sample_rate.to_string())
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
    if stdin.write_all(&assignment.state_bytes).is_err() {
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
        if events.len() > MAX_EVENTS {
            return Err(format!(
                "VST2 worker accepts at most {MAX_EVENTS} MIDI events in one audio block"
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
            data: [0x80 | channel_status, key, 0],
        });
        note_count += 1;
    }
    events.sort_by_key(|event| (event.frame, event.priority));
    Ok((events, note_count))
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

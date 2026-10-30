use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, OsString};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::latency::{HostLatency, HostLatencyImpl, PluginLatency};
use clack_extensions::note_ports::{NoteDialect, NotePortInfoBuffer, PluginNotePorts};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_extensions::state::PluginState;
use clack_extensions::tail::{HostTail, HostTailImpl, PluginTail, TailLength};
use clack_host::events::event_types::{
    Midi2Event, MidiEvent, NoteExpressionEvent, NoteExpressionType, NoteOffEvent, NoteOnEvent,
    ParamGestureBeginEvent, ParamGestureEndEvent, ParamValueEvent, TransportEvent, TransportFlags,
};
use clack_host::events::{EventFlags, EventHeader};
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, AudioProcessorHandler, ClapId, EventBuffer,
    HostError, HostExtensions, HostHandlers, HostInfo, InputAudioBuffers, InputChannel,
    MainThreadHandler, Pckn, PluginAudioConfiguration, PluginEntry, PluginInstance, ProcessStatus,
    SharedHandler, StartedPluginAudioProcessor,
};
use clack_host::utils::{BeatTime, SecondsTime};
use eframe::egui;
use flp_rebuild::plugin_settings::{
    ClapProjectMixerEffectAssignment, MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES,
};
use flp_rebuild::sample_render::PlaylistTempoMap;
use flp_rebuild::vst3::{MixerEffectProcessor, MixerEffectProcessorFactory};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub const PROBE_FLAG: &str = "--clap-probe";
pub const INSTANCE_PROBE_FLAG: &str = "--clap-instance-probe";
pub const NOTE_PREVIEW_FLAG: &str = "--clap-note-preview";
pub const NOTE_PREVIEW_SERVER_FLAG: &str = "--clap-note-preview-server";
pub const MIXER_EFFECT_SERVER_FLAG: &str = "--clap-mixer-effect-server";
pub const PLUGIN_EDITOR_FLAG: &str = "--clap-plugin-editor";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_NOTE_PREVIEW_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_NOTE_PREVIEW_PROTOCOL_HEADER_BYTES: usize = 4096;
const MAX_INSTANCE_METADATA_ITEMS: u32 = 512;
const MAX_NOTE_PREVIEW_AUDIO_PORTS: u32 = 16;
const MAX_NOTE_PREVIEW_TOTAL_CHANNELS: u32 = 64;
const MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES: u32 = 512;
const MAX_PATTERN_PREVIEW_NOTES: usize = 16_384;
const MAX_CLAP_PREVIEW_PARAMETER_CHANGES: usize = 512;
const MAX_CLAP_MIXER_EFFECT_PARAMETER_CHANGES: usize = 2_048;
const MAX_CLAP_SLIDE_EXPRESSION_EVENTS: usize = 1_000_000;
const NOTE_PREVIEW_BLOCK_FRAMES: u32 = 512;
const CLAP_MIXER_EFFECT_PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const NOTE_PREVIEW_DURATION_MILLISECONDS: u32 = 1500;
const DEFAULT_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 500;
pub const MIN_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 50;
pub const MAX_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 1000;
const PROBE_HEADER: &str = "FL_STUDIO_REBUILD_CLAP_PROBE_V1";
const INSTANCE_PROBE_HEADER: &str = "FL_STUDIO_REBUILD_CLAP_INSTANCE_V1";

struct ClapMixerEffectHost;

struct ClapMixerEffectHostShared {
    restart_requested: Arc<AtomicBool>,
    callback_requested: Arc<AtomicBool>,
    latency_changed: Arc<AtomicBool>,
    tail_changed: Arc<AtomicBool>,
}

struct ClapMixerEffectHostMainThread<'a> {
    latency_changed: &'a AtomicBool,
}

struct ClapMixerEffectHostAudioProcessor<'a> {
    tail_changed: &'a AtomicBool,
}

impl<'a> SharedHandler<'a> for ClapMixerEffectHostShared {
    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    fn request_process(&self) {}

    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::Release);
    }
}

impl<'a> MainThreadHandler<'a> for ClapMixerEffectHostMainThread<'a> {}

impl HostLatencyImpl for ClapMixerEffectHostMainThread<'_> {
    fn changed(&self) {
        self.latency_changed.store(true, Ordering::Release);
    }
}

impl<'a> AudioProcessorHandler<'a> for ClapMixerEffectHostAudioProcessor<'a> {}

impl HostTailImpl for ClapMixerEffectHostAudioProcessor<'_> {
    fn changed(&mut self) {
        self.tail_changed.store(true, Ordering::Release);
    }
}

impl HostHandlers for ClapMixerEffectHost {
    type Shared<'a> = ClapMixerEffectHostShared;
    type MainThread<'a> = ClapMixerEffectHostMainThread<'a>;
    type AudioProcessor<'a> = ClapMixerEffectHostAudioProcessor<'a>;

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostLatency>().register::<HostTail>();
    }
}

struct ClapPluginEditorHost;

struct ClapPluginEditorHostShared {
    parameter_flush_requested: Arc<AtomicBool>,
    gui_closed: Arc<AtomicBool>,
    gui_was_destroyed: Arc<AtomicBool>,
    gui_show_requested: Arc<AtomicBool>,
    gui_hide_requested: Arc<AtomicBool>,
    resize_requested: Arc<Mutex<Option<GuiSize>>>,
}

struct ClapPluginEditorHostMainThread;

struct ClapPluginEditorHostAudioProcessor;

impl SharedHandler<'_> for ClapPluginEditorHostShared {
    fn request_restart(&self) {}

    fn request_process(&self) {}

    fn request_callback(&self) {}
}

impl MainThreadHandler<'_> for ClapPluginEditorHostMainThread {}

impl AudioProcessorHandler<'_> for ClapPluginEditorHostAudioProcessor {}

impl HostGuiImpl for ClapPluginEditorHostShared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        if new_size.width == 0 || new_size.height == 0 {
            return Err(HostError::Message("CLAP editor requested an empty size"));
        }
        *self
            .resize_requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(new_size);
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        self.gui_show_requested.store(true, Ordering::Release);
        self.gui_hide_requested.store(false, Ordering::Release);
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        self.gui_hide_requested.store(true, Ordering::Release);
        self.gui_show_requested.store(false, Ordering::Release);
        Ok(())
    }

    fn closed(&self, was_destroyed: bool) {
        self.gui_was_destroyed
            .store(was_destroyed, Ordering::Release);
        self.gui_closed.store(true, Ordering::Release);
    }
}

impl HostParamsImplShared for ClapPluginEditorHostShared {
    fn request_flush(&self) {
        self.parameter_flush_requested
            .store(true, Ordering::Release);
    }
}

impl HostParamsImplMainThread for ClapPluginEditorHostMainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {}

    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostHandlers for ClapPluginEditorHost {
    type Shared<'a> = ClapPluginEditorHostShared;
    type MainThread<'a> = ClapPluginEditorHostMainThread;
    type AudioProcessor<'a> = ClapPluginEditorHostAudioProcessor;

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostGui>().register::<HostParams>();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapPluginDescriptor {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapAudioPortDescriptor {
    pub id: u32,
    pub name: String,
    pub channel_count: u32,
    pub is_main: bool,
    pub port_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClapNotePortDescriptor {
    pub id: u32,
    pub name: String,
    pub supported_dialects: u32,
    pub preferred_dialect: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapParameterDescriptor {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub flags: u32,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub current_value: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClapInstanceInfo {
    pub audio_input_count: u32,
    pub audio_output_count: u32,
    pub note_input_count: u32,
    pub note_output_count: u32,
    pub parameter_count: u32,
    pub audio_inputs: Vec<ClapAudioPortDescriptor>,
    pub audio_outputs: Vec<ClapAudioPortDescriptor>,
    pub note_inputs: Vec<ClapNotePortDescriptor>,
    pub note_outputs: Vec<ClapNotePortDescriptor>,
    pub parameters: Vec<ClapParameterDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClapInspectionState {
    Inspecting,
    Inspected(Vec<ClapPluginDescriptor>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClapInstanceInspectionState {
    Inspecting,
    Inspected(ClapInstanceInfo),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClapNotePreviewState {
    Rendering,
    Ready,
    Cancelled,
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClapNotePreviewSettings {
    pub note_key: u8,
    pub velocity: u8,
    pub note_duration_milliseconds: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPatternNote {
    pub position_ticks: u32,
    pub length_ticks: u32,
    pub key: u16,
    pub velocity: u8,
    pub midi_channel: u8,
    pub fine_pitch_cents: i16,
    pub pan: u8,
    pub release: u8,
    pub mod_x: u8,
    pub slide_note: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapParameterAutomationPoint {
    pub tick: u64,
    pub normalized_value: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapParameterAutomation {
    pub parameter_id: u32,
    pub points: Vec<ClapParameterAutomationPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPatternPreviewSettings {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub loop_length_ticks: u64,
    pub notes: Vec<ClapPatternNote>,
    /// Normalized CLAP parameter lanes in Pattern-relative ticks.
    pub parameter_automation: Vec<ClapParameterAutomation>,
    pub playlist: Option<ClapPlaylistPreviewSettings>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPlaylistTempoPoint {
    pub tick: u64,
    pub tempo_bpm: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPlaylistNote {
    pub start_tick: u64,
    pub stop_tick: u64,
    pub key: u16,
    pub velocity: u8,
    pub midi_channel: u8,
    pub fine_pitch_cents: i16,
    pub pan: u8,
    pub release: u8,
    pub mod_x: u8,
    pub slide_note: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPlaylistPreviewSettings {
    pub tempo_points: Vec<ClapPlaylistTempoPoint>,
    pub notes: Vec<ClapPlaylistNote>,
    /// Normalized CLAP parameter lanes in arrangement ticks.
    pub parameter_automation: Vec<ClapParameterAutomation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClapPatternPreviewPurpose {
    Audition,
    Transport,
    Playlist,
    SetParameters,
    SaveState,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClapPatternPreviewOutput {
    Audio(Vec<f32>),
    ParametersSet,
    SavedState(Vec<u8>),
}

pub type ClapInspectionResponse = (PathBuf, Result<Vec<ClapPluginDescriptor>, String>);
pub type ClapInstanceInspectionResponse = (PathBuf, String, Result<ClapInstanceInfo, String>);
pub type ClapNotePreviewRequest = (PathBuf, String, String, u32, u8, u8, u32, u64);
pub type ClapNotePreviewResponse = (
    PathBuf,
    String,
    String,
    u8,
    u8,
    u32,
    u64,
    Result<Vec<f32>, String>,
);
pub type ClapPatternPreviewRequest = (
    PathBuf,
    String,
    String,
    u32,
    u16,
    Option<ClapPatternPreviewSettings>,
    Option<Vec<u8>>,
    Vec<(u32, f64)>,
    ClapPatternPreviewPurpose,
    u64,
);
pub type ClapPatternPreviewResponse = (
    PathBuf,
    String,
    String,
    Option<ClapPatternPreviewSettings>,
    ClapPatternPreviewPurpose,
    u64,
    Result<ClapPatternPreviewOutput, String>,
);
pub type ClapMixerEffectStateCaptureRequest =
    (usize, u16, ClapProjectMixerEffectAssignment, u32, u64);
pub type ClapMixerEffectStateCaptureResponse = (
    usize,
    u16,
    PathBuf,
    String,
    String,
    u64,
    Result<Vec<u8>, String>,
);

type ClapMixerEffectStateCaptureKey = (usize, u16);

struct ActiveClapMixerEffectStateCaptureRequest {
    response: mpsc::SyncSender<Result<Vec<u8>, String>>,
}

struct ActiveClapMixerEffectCaptureProcessor {
    path: PathBuf,
    plugin_id: String,
    request_sender: Sender<ActiveClapMixerEffectStateCaptureRequest>,
    parameter_update_sender: Sender<BTreeMap<u32, f64>>,
}

#[derive(Default)]
struct ClapMixerEffectStateCaptureRegistry {
    next_registration_id: u64,
    processors: BTreeMap<
        ClapMixerEffectStateCaptureKey,
        BTreeMap<u64, ActiveClapMixerEffectCaptureProcessor>,
    >,
}

#[derive(Clone, Default)]
pub struct ClapMixerEffectStateCaptureController {
    registry: Arc<Mutex<ClapMixerEffectStateCaptureRegistry>>,
}

struct ClapMixerEffectStateCaptureRegistration {
    controller: ClapMixerEffectStateCaptureController,
    key: ClapMixerEffectStateCaptureKey,
    registration_id: u64,
}

impl ClapMixerEffectStateCaptureController {
    fn register_processor(
        &self,
        key: ClapMixerEffectStateCaptureKey,
        path: &Path,
        plugin_id: &str,
        request_sender: Sender<ActiveClapMixerEffectStateCaptureRequest>,
        parameter_update_sender: Sender<BTreeMap<u32, f64>>,
    ) -> ClapMixerEffectStateCaptureRegistration {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.next_registration_id = registry.next_registration_id.wrapping_add(1).max(1);
        let registration_id = registry.next_registration_id;
        registry.processors.entry(key).or_default().insert(
            registration_id,
            ActiveClapMixerEffectCaptureProcessor {
                path: path.to_owned(),
                plugin_id: plugin_id.to_owned(),
                request_sender,
                parameter_update_sender,
            },
        );
        ClapMixerEffectStateCaptureRegistration {
            controller: self.clone(),
            key,
            registration_id,
        }
    }

    fn capture_active_state(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        path: &Path,
        plugin_id: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let request_sender = {
            let registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(processors) = registry.processors.get(&(insert_ordinal, slot_index)) else {
                return Ok(None);
            };
            let matching = processors
                .values()
                .filter(|processor| processor.path == path && processor.plugin_id == plugin_id)
                .collect::<Vec<_>>();
            match matching.as_slice() {
                [] => return Ok(None),
                [processor] => processor.request_sender.clone(),
                _ => {
                    return Err(format!(
                        "multiple active instances of {plugin_id} are assigned to Mixer insert {insert_ordinal} slot {}; stop the other render or playback stream before capturing state",
                        slot_index + 1
                    ));
                }
            }
        };
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        request_sender
            .send(ActiveClapMixerEffectStateCaptureRequest {
                response: response_sender,
            })
            .map_err(|_| "active CLAP Mixer effect processor is no longer available".to_owned())?;
        match response_receiver.recv_timeout(CLAP_MIXER_EFFECT_PROCESS_TIMEOUT) {
            Ok(result) => result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(
                "active CLAP Mixer effect did not reach a block boundary within 10 seconds"
                    .to_owned(),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("active CLAP Mixer effect stopped before its state was captured".to_owned())
            }
        }
    }

    pub fn update_active_parameters(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        path: &Path,
        plugin_id: &str,
        parameter_values: &BTreeMap<u32, f64>,
    ) {
        if parameter_values.is_empty() {
            return;
        }
        let request_senders = {
            let registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry
                .processors
                .get(&(insert_ordinal, slot_index))
                .into_iter()
                .flat_map(BTreeMap::values)
                .filter(|processor| processor.path == path && processor.plugin_id == plugin_id)
                .map(|processor| processor.parameter_update_sender.clone())
                .collect::<Vec<_>>()
        };
        for sender in request_senders {
            let _ = sender.send(parameter_values.clone());
        }
    }
}

impl Drop for ClapMixerEffectStateCaptureRegistration {
    fn drop(&mut self) {
        let mut registry = self
            .controller
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(processors) = registry.processors.get_mut(&self.key) {
            processors.remove(&self.registration_id);
            if processors.is_empty() {
                registry.processors.remove(&self.key);
            }
        }
    }
}

pub fn start_worker(
    egui_context: eframe::egui::Context,
) -> (Sender<PathBuf>, Receiver<ClapInspectionResponse>) {
    let (request_sender, request_receiver) = mpsc::channel::<PathBuf>();
    let (response_sender, response_receiver) = mpsc::channel::<ClapInspectionResponse>();

    thread::Builder::new()
        .name("clap-descriptor-probe".to_owned())
        .spawn(move || {
            while let Ok(path) = request_receiver.recv() {
                let response = (path.clone(), inspect_in_subprocess(&path));
                if response_sender.send(response).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP descriptor worker should start");

    (request_sender, response_receiver)
}

pub fn start_instance_worker(
    egui_context: eframe::egui::Context,
) -> (
    Sender<(PathBuf, String)>,
    Receiver<ClapInstanceInspectionResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<(PathBuf, String)>();
    let (response_sender, response_receiver) = mpsc::channel::<ClapInstanceInspectionResponse>();

    thread::Builder::new()
        .name("clap-instance-probe".to_owned())
        .spawn(move || {
            while let Ok((path, plugin_id)) = request_receiver.recv() {
                let response = (
                    path.clone(),
                    plugin_id.clone(),
                    inspect_instance_in_subprocess(&path, &plugin_id),
                );
                if response_sender.send(response).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP instance probe worker should start");

    (request_sender, response_receiver)
}

pub fn start_note_preview_worker(
    egui_context: eframe::egui::Context,
) -> (
    Sender<ClapNotePreviewRequest>,
    Receiver<ClapNotePreviewResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<ClapNotePreviewRequest>();
    let (response_sender, response_receiver) = mpsc::channel::<ClapNotePreviewResponse>();

    thread::Builder::new()
        .name("clap-note-preview".to_owned())
        .spawn(move || {
            let mut preview_process = None;
            while let Ok((
                path,
                plugin_id,
                plugin_name,
                sample_rate,
                note_key,
                velocity,
                note_duration_milliseconds,
                request_id,
            )) = request_receiver.recv()
            {
                let process_matches =
                    preview_process
                        .as_ref()
                        .is_some_and(|process: &ClapPreviewProcess| {
                            process.matches(&path, &plugin_id, sample_rate)
                        });
                if !process_matches {
                    preview_process.take();
                }
                let result = if let Some(process) = preview_process.as_mut() {
                    process.render(note_key, velocity, note_duration_milliseconds)
                } else {
                    match ClapPreviewProcess::spawn(&path, &plugin_id, sample_rate) {
                        Ok(mut process) => {
                            let result =
                                process.render(note_key, velocity, note_duration_milliseconds);
                            preview_process = Some(process);
                            result
                        }
                        Err(error) => Err(error),
                    }
                };
                if result.is_err() {
                    preview_process.take();
                }
                let response = (
                    path.clone(),
                    plugin_id.clone(),
                    plugin_name,
                    note_key,
                    velocity,
                    note_duration_milliseconds,
                    request_id,
                    result,
                );
                if response_sender.send(response).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP note preview worker should start");

    (request_sender, response_receiver)
}

pub fn start_pattern_preview_worker(
    egui_context: eframe::egui::Context,
) -> (
    Sender<ClapPatternPreviewRequest>,
    Receiver<ClapPatternPreviewResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<ClapPatternPreviewRequest>();
    let (response_sender, response_receiver) = mpsc::channel::<ClapPatternPreviewResponse>();

    thread::Builder::new()
        .name("clap-pattern-preview".to_owned())
        .spawn(move || {
            let mut preview_process = None;
            let mut preview_process_channel = None;
            let mut preview_process_has_loaded_state = false;
            while let Ok((
                path,
                plugin_id,
                plugin_name,
                sample_rate,
                channel_id,
                settings,
                state,
                parameter_values,
                purpose,
                request_id,
            )) = request_receiver.recv()
            {
                let process_matches = preview_process_channel == Some(channel_id)
                    && preview_process
                        .as_ref()
                        .is_some_and(|process: &ClapPreviewProcess| {
                            process.matches(&path, &plugin_id, sample_rate)
                        });
                if !process_matches
                    || (state.is_none()
                        && preview_process_has_loaded_state
                        && parameter_values.is_empty())
                {
                    preview_process.take();
                    preview_process_channel = None;
                }
                let render = |process: &mut ClapPreviewProcess| {
                    if let Some(state) = &state {
                        process.load_state(state)?;
                    }
                    if !parameter_values.is_empty() {
                        process.set_parameter_values(&parameter_values)?;
                    }
                    match purpose {
                        ClapPatternPreviewPurpose::Audition => process
                            .render_pattern(settings.as_ref().ok_or_else(|| {
                                "CLAP Pattern audition is missing its settings".to_owned()
                            })?)
                            .map(ClapPatternPreviewOutput::Audio),
                        ClapPatternPreviewPurpose::Transport => process
                            .render_pattern_loop(settings.as_ref().ok_or_else(|| {
                                "CLAP Pattern transport is missing its settings".to_owned()
                            })?)
                            .map(ClapPatternPreviewOutput::Audio),
                        ClapPatternPreviewPurpose::Playlist => process
                            .render_playlist(settings.as_ref().ok_or_else(|| {
                                "CLAP Playlist playback is missing its settings".to_owned()
                            })?)
                            .map(ClapPatternPreviewOutput::Audio),
                        ClapPatternPreviewPurpose::SetParameters => {
                            if parameter_values.is_empty() {
                                Err("CLAP parameter update did not contain any values".to_owned())
                            } else {
                                Ok(ClapPatternPreviewOutput::ParametersSet)
                            }
                        }
                        ClapPatternPreviewPurpose::SaveState => process
                            .save_state()
                            .map(ClapPatternPreviewOutput::SavedState),
                    }
                };
                let result = if let Some(process) = preview_process.as_mut() {
                    render(process)
                } else {
                    match ClapPreviewProcess::spawn(&path, &plugin_id, sample_rate) {
                        Ok(mut process) => {
                            let result = render(&mut process);
                            preview_process = Some(process);
                            preview_process_channel = Some(channel_id);
                            result
                        }
                        Err(error) => Err(error),
                    }
                };
                if result.is_err() {
                    preview_process.take();
                    preview_process_channel = None;
                }
                preview_process_has_loaded_state =
                    result.is_ok() && (state.is_some() || !parameter_values.is_empty());
                if response_sender
                    .send((
                        path,
                        plugin_id,
                        plugin_name,
                        settings,
                        purpose,
                        request_id,
                        result,
                    ))
                    .is_err()
                {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP Pattern preview worker should start");

    (request_sender, response_receiver)
}

pub fn start_mixer_effect_state_capture_worker(
    egui_context: eframe::egui::Context,
    capture_controller: ClapMixerEffectStateCaptureController,
) -> (
    Sender<ClapMixerEffectStateCaptureRequest>,
    Receiver<ClapMixerEffectStateCaptureResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<ClapMixerEffectStateCaptureRequest>();
    let (response_sender, response_receiver) =
        mpsc::channel::<ClapMixerEffectStateCaptureResponse>();

    thread::Builder::new()
        .name("clap-mixer-effect-state-capture".to_owned())
        .spawn(move || {
            while let Ok((insert_ordinal, slot_index, assignment, sample_rate, request_id)) =
                request_receiver.recv()
            {
                let ClapProjectMixerEffectAssignment {
                    path,
                    plugin_id,
                    plugin_name,
                    state,
                    ..
                } = assignment;
                let result = match capture_controller.capture_active_state(
                    insert_ordinal,
                    slot_index,
                    &path,
                    &plugin_id,
                ) {
                    Ok(Some(state)) => Ok(state),
                    Ok(None) => capture_mixer_effect_state_in_subprocess(
                        &path,
                        &plugin_id,
                        sample_rate,
                        state,
                    ),
                    Err(error) => Err(error),
                };
                if response_sender
                    .send((
                        insert_ordinal,
                        slot_index,
                        path,
                        plugin_id,
                        plugin_name,
                        request_id,
                        result,
                    ))
                    .is_err()
                {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("CLAP Mixer effect state-capture worker should start");

    (request_sender, response_receiver)
}

fn capture_mixer_effect_state_in_subprocess(
    path: &Path,
    plugin_id: &str,
    sample_rate: u32,
    previous_state: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    if previous_state
        .as_ref()
        .is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
    {
        return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the CLAP Mixer effect helper: {error}"))?;
    let mut child = Command::new(executable)
        .arg(MIXER_EFFECT_SERVER_FLAG)
        .arg(path)
        .arg(plugin_id)
        .arg(sample_rate.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the CLAP Mixer effect helper: {error}"))?;
    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("CLAP Mixer effect helper did not provide input".to_owned());
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("CLAP Mixer effect helper did not provide output".to_owned());
    };
    let (response_sender, response_receiver) = mpsc::sync_channel(1);
    let protocol = thread::Builder::new()
        .name("clap-mixer-effect-state-response".to_owned())
        .spawn(move || {
            let result = capture_mixer_effect_state_protocol(stdin, stdout, previous_state);
            let _ = response_sender.send(result);
        })
        .map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            format!("could not start the CLAP Mixer effect state reader: {error}")
        })?;

    let result = match response_receiver.recv_timeout(CLAP_MIXER_EFFECT_PROCESS_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = protocol.join();
            return Err("CLAP Mixer effect state capture timed out after 10 seconds".to_owned());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = protocol.join();
            return Err("CLAP Mixer effect state helper stopped unexpectedly".to_owned());
        }
    };
    if result.is_err() {
        let _ = child.kill();
    }
    let deadline = Instant::now() + CLAP_MIXER_EFFECT_PROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = protocol.join();
                return Err("CLAP Mixer effect helper did not exit after state capture".to_owned());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = protocol.join();
                return Err(format!(
                    "could not wait for the CLAP Mixer effect helper: {error}"
                ));
            }
        }
    };
    protocol
        .join()
        .map_err(|_| "CLAP Mixer effect state reader stopped unexpectedly".to_owned())?;
    let state = result?;
    if !status.success() {
        return Err(
            "CLAP Mixer effect helper exited unsuccessfully after state capture".to_owned(),
        );
    }
    Ok(state)
}

fn capture_mixer_effect_state_protocol(
    stdin: ChildStdin,
    stdout: ChildStdout,
    previous_state: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let mut input = BufReader::new(stdout);
    let mut output = BufWriter::new(stdin);
    let ready = read_preview_protocol_header(&mut input)
        .map_err(|error| format!("could not read CLAP Mixer effect helper status: {error}"))?
        .ok_or_else(|| "CLAP Mixer effect helper closed before initialization".to_owned())?;
    if let Some(error) = ready.strip_prefix("ERR ") {
        return Err(error.to_owned());
    }
    if !ready.starts_with("READY ") {
        return Err("CLAP Mixer effect helper sent an invalid initialization response".to_owned());
    }

    if let Some(state) = previous_state {
        writeln!(output, "LOAD_STATE {}", state.len())
            .and_then(|_| output.write_all(&state))
            .and_then(|_| output.write_all(b"\n"))
            .and_then(|_| output.flush())
            .map_err(|error| format!("could not restore CLAP Mixer effect state: {error}"))?;
        let loaded = read_preview_protocol_header(&mut input)
            .map_err(|error| format!("could not read CLAP Mixer effect state response: {error}"))?
            .ok_or_else(|| "CLAP Mixer effect helper closed during state restore".to_owned())?;
        if let Some(error) = loaded.strip_prefix("ERR ") {
            return Err(error.to_owned());
        }
        if loaded != "STATE_LOADED" {
            return Err("CLAP Mixer effect helper did not acknowledge its state".to_owned());
        }
    }

    output
        .write_all(b"SAVE_STATE\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("could not request CLAP Mixer effect state: {error}"))?;
    let response = read_preview_protocol_header(&mut input)
        .map_err(|error| format!("could not read CLAP Mixer effect state: {error}"))?
        .ok_or_else(|| "CLAP Mixer effect helper closed before saving state".to_owned())?;
    if let Some(error) = response.strip_prefix("ERR ") {
        return Err(error.to_owned());
    }
    let state_header = response
        .strip_prefix("STATE ")
        .ok_or_else(|| "CLAP Mixer effect helper returned no saved state".to_owned())?;
    let state = read_preview_server_state(&mut input, state_header)?;
    output
        .write_all(b"STOP\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("could not stop the CLAP Mixer effect helper: {error}"))?;
    Ok(state)
}

/// Loads one CLAP entry in a child process so a crashing plug-in cannot terminate the DAW.
pub fn inspect_in_subprocess(path: &Path) -> Result<Vec<ClapPluginDescriptor>, String> {
    let arguments = [OsString::from(PROBE_FLAG), path.as_os_str().to_owned()];
    run_probe_subprocess(&arguments, "descriptor probe")
        .and_then(|output| parse_probe_output(&output))
}

pub fn inspect_instance_in_subprocess(
    path: &Path,
    plugin_id: &str,
) -> Result<ClapInstanceInfo, String> {
    let arguments = [
        OsString::from(INSTANCE_PROBE_FLAG),
        path.as_os_str().to_owned(),
        OsString::from(plugin_id),
    ];
    run_probe_subprocess(&arguments, "instance probe")
        .and_then(|output| parse_instance_probe_output(&output))
}

enum ClapPreviewProcessResponse {
    Ready,
    Audio(Vec<f32>),
    State(Vec<u8>),
    Error(String),
}

enum ClapMixerEffectProcessResponse {
    Ready {
        input_channels: usize,
        output_channels: usize,
        auxiliary_input_buses: Vec<(usize, usize)>,
    },
    ParametersSet,
    StateLoaded,
    State(Vec<u8>),
    StateError(String),
    Started {
        latency_samples: u32,
        tail_samples: Option<u64>,
    },
    Audio {
        samples: Vec<f32>,
        latency_samples: u32,
        tail_samples: Option<u64>,
    },
    Stopped,
    Error(String),
}

pub struct ClapPluginEditorOutput {
    pub state: Option<Vec<u8>>,
    pub parameter_overrides: BTreeMap<u32, f64>,
}

pub enum ClapPluginEditorEvent {
    LiveParameters(BTreeMap<u32, f64>),
    Finished(Result<ClapPluginEditorOutput, String>),
}

type ClapPluginEditorInput = (Option<Vec<u8>>, BTreeMap<u32, f64>);

pub struct ClapPluginEditorProcess {
    child: Arc<Mutex<Child>>,
    responses: Receiver<ClapPluginEditorEvent>,
    completed: Arc<AtomicBool>,
}

impl ClapPluginEditorProcess {
    pub fn spawn(
        path: &Path,
        plugin_id: &str,
        state: Option<&[u8]>,
        parameter_overrides: &BTreeMap<u32, f64>,
    ) -> Result<Self, String> {
        if state.is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES) {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        if parameter_overrides.len() > MAX_INSTANCE_METADATA_ITEMS as usize {
            return Err(
                "CLAP Mixer parameter override count exceeds the metadata limit".to_owned(),
            );
        }
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate the CLAP editor helper: {error}"))?;
        let mut child = Command::new(executable)
            .arg(PLUGIN_EDITOR_FLAG)
            .arg(path)
            .arg(plugin_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start the CLAP editor helper: {error}"))?;
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP editor helper did not provide input".to_owned());
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP editor helper did not provide output".to_owned());
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP editor helper did not provide live parameter updates".to_owned());
        };
        let write_result = (|| {
            if let Some(state) = state {
                writeln!(stdin, "STATE 1 {}", state.len())?;
                stdin.write_all(state)?;
                stdin.write_all(b"\n")?;
            } else {
                writeln!(stdin, "STATE 0 0")?;
            }
            writeln!(stdin, "PARAMS {}", parameter_overrides.len())?;
            for (parameter_id, value) in parameter_overrides {
                if !value.is_finite() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "CLAP Mixer parameter override is not finite",
                    ));
                }
                writeln!(stdin, "PARAM {parameter_id} {:016x}", value.to_bits())?;
            }
            writeln!(stdin, "END")?;
            stdin.flush()
        })();
        drop(stdin);
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("could not send CLAP editor state: {error}"));
        }

        let child = Arc::new(Mutex::new(child));
        let worker_child = Arc::clone(&child);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let (response_sender, responses) = mpsc::channel();
        let live_sender = response_sender.clone();
        thread::Builder::new()
            .name("clap-plugin-editor-live-parameters".to_owned())
            .spawn(move || read_plugin_editor_live_parameters(stderr, live_sender))
            .map_err(|error| {
                let mut child = child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = child.kill();
                let _ = child.wait();
                format!("could not start the CLAP editor parameter reader: {error}")
            })?;
        thread::Builder::new()
            .name("clap-plugin-editor-response".to_owned())
            .spawn(move || {
                let result = read_plugin_editor_response(stdout).and_then(|response| {
                    let deadline = Instant::now() + CLAP_MIXER_EFFECT_PROCESS_TIMEOUT;
                    loop {
                        let status = worker_child
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .try_wait()
                            .map_err(|error| {
                                format!("could not wait for the CLAP editor helper: {error}")
                            })?;
                        if let Some(status) = status {
                            if status.success() {
                                return Ok(response);
                            }
                            return Err(
                                "CLAP editor helper exited unsuccessfully after closing".to_owned()
                            );
                        }
                        if Instant::now() >= deadline {
                            let mut child = worker_child
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(
                                "CLAP editor helper did not exit after saving state".to_owned()
                            );
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                });
                worker_completed.store(true, Ordering::Release);
                let _ = response_sender.send(ClapPluginEditorEvent::Finished(result));
            })
            .map_err(|error| {
                let mut child = child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = child.kill();
                let _ = child.wait();
                format!("could not start the CLAP editor response reader: {error}")
            })?;

        Ok(Self {
            child,
            responses,
            completed,
        })
    }

    pub fn try_recv(&self) -> Result<ClapPluginEditorEvent, mpsc::TryRecvError> {
        self.responses.try_recv()
    }
}

impl Drop for ClapPluginEditorProcess {
    fn drop(&mut self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct ClapMixerEffectProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    responses: Receiver<ClapMixerEffectProcessResponse>,
    reader: Option<JoinHandle<()>>,
    input_channels: usize,
    output_channels: usize,
    auxiliary_input_buses: Vec<(usize, usize)>,
    auxiliary_input_source_map: Option<Vec<(usize, usize)>>,
    sample_rate: u32,
    latency_samples: u32,
    manual_latency_offset_samples: i32,
    tail_samples: Option<u64>,
    stopped: bool,
    capture_request_sender: Sender<ActiveClapMixerEffectStateCaptureRequest>,
    capture_request_receiver: Receiver<ActiveClapMixerEffectStateCaptureRequest>,
    parameter_update_sender: Sender<BTreeMap<u32, f64>>,
    parameter_update_receiver: Receiver<BTreeMap<u32, f64>>,
    pending_parameter_updates: BTreeMap<u32, f64>,
    _capture_registration: Option<ClapMixerEffectStateCaptureRegistration>,
}

impl ClapMixerEffectProcess {
    fn spawn(
        path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        state: Option<&[u8]>,
        parameter_overrides: &BTreeMap<u32, f64>,
    ) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate the application executable: {error}"))?;
        let mut child = Command::new(executable)
            .arg(MIXER_EFFECT_SERVER_FLAG)
            .arg(path)
            .arg(plugin_id)
            .arg(sample_rate.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start the CLAP Mixer effect helper: {error}"))?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP Mixer effect helper did not provide input".to_owned());
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP Mixer effect helper did not provide output".to_owned());
        };
        let (response_sender, responses) = mpsc::channel();
        let (capture_request_sender, capture_request_receiver) = mpsc::channel();
        let (parameter_update_sender, parameter_update_receiver) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("clap-mixer-effect-response".to_owned())
            .spawn(move || read_mixer_effect_process_responses(stdout, response_sender))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "could not start the CLAP Mixer effect response reader: {error}"
                ));
            }
        };
        let mut process = Self {
            child,
            stdin: BufWriter::new(stdin),
            responses,
            reader: Some(reader),
            input_channels: 0,
            output_channels: 0,
            auxiliary_input_buses: Vec::new(),
            auxiliary_input_source_map: None,
            sample_rate,
            latency_samples: 0,
            manual_latency_offset_samples: 0,
            tail_samples: None,
            stopped: false,
            capture_request_sender,
            capture_request_receiver,
            parameter_update_sender,
            parameter_update_receiver,
            pending_parameter_updates: BTreeMap::new(),
            _capture_registration: None,
        };
        match process.receive_response()? {
            ClapMixerEffectProcessResponse::Ready {
                input_channels,
                output_channels,
                auxiliary_input_buses,
            } if matches!(input_channels, 1 | 2) && matches!(output_channels, 1 | 2) => {
                process.input_channels = input_channels;
                process.output_channels = output_channels;
                process.auxiliary_input_buses = auxiliary_input_buses;
            }
            ClapMixerEffectProcessResponse::Ready { .. } => {
                return Err(
                    "CLAP Mixer effect helper reported an unsupported bus layout".to_owned(),
                );
            }
            ClapMixerEffectProcessResponse::Error(error) => return Err(error),
            _ => {
                return Err(
                    "CLAP Mixer effect helper sent an invalid initialization response".to_owned(),
                );
            }
        }
        if let Some(state) = state {
            if state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
                return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
            }
            writeln!(process.stdin, "LOAD_STATE {}", state.len())
                .and_then(|_| process.stdin.write_all(state))
                .and_then(|_| process.stdin.write_all(b"\n"))
                .and_then(|_| process.stdin.flush())
                .map_err(|error| format!("could not send CLAP Mixer effect state: {error}"))?;
            match process.receive_response()? {
                ClapMixerEffectProcessResponse::StateLoaded => {}
                ClapMixerEffectProcessResponse::Error(error) => return Err(error),
                _ => {
                    return Err("CLAP Mixer effect helper did not acknowledge its state".to_owned());
                }
            }
        }
        if !parameter_overrides.is_empty() {
            if parameter_overrides.len() > MAX_INSTANCE_METADATA_ITEMS as usize {
                return Err(
                    "CLAP Mixer parameter override count exceeds the metadata limit".to_owned(),
                );
            }
            writeln!(process.stdin, "SET_PARAMS {}", parameter_overrides.len()).map_err(
                |error| format!("could not send CLAP Mixer parameter overrides: {error}"),
            )?;
            for (parameter_id, value) in parameter_overrides {
                if !value.is_finite() {
                    return Err("CLAP Mixer parameter override is not finite".to_owned());
                }
                writeln!(
                    process.stdin,
                    "PARAMETER {parameter_id} {:016x}",
                    value.to_bits()
                )
                .map_err(|error| {
                    format!("could not send CLAP Mixer parameter override: {error}")
                })?;
            }
            process.stdin.flush().map_err(|error| {
                format!("could not flush CLAP Mixer parameter overrides: {error}")
            })?;
            match process.receive_response()? {
                ClapMixerEffectProcessResponse::ParametersSet => {}
                ClapMixerEffectProcessResponse::Error(error) => return Err(error),
                _ => {
                    return Err(
                        "CLAP Mixer effect helper did not acknowledge parameter overrides"
                            .to_owned(),
                    );
                }
            }
        }
        writeln!(process.stdin, "START")
            .and_then(|_| process.stdin.flush())
            .map_err(|error| format!("could not start CLAP Mixer effect processing: {error}"))?;
        match process.receive_response()? {
            ClapMixerEffectProcessResponse::Started {
                latency_samples,
                tail_samples,
            } => {
                process.latency_samples = latency_samples;
                process.tail_samples = tail_samples;
                Ok(process)
            }
            ClapMixerEffectProcessResponse::Error(error) => Err(error),
            _ => Err("CLAP Mixer effect helper did not start processing".to_owned()),
        }
    }

    fn register_for_state_capture(
        &mut self,
        controller: &ClapMixerEffectStateCaptureController,
        key: ClapMixerEffectStateCaptureKey,
        path: &Path,
        plugin_id: &str,
    ) {
        self._capture_registration = Some(controller.register_processor(
            key,
            path,
            plugin_id,
            self.capture_request_sender.clone(),
            self.parameter_update_sender.clone(),
        ));
    }

    fn process_block(
        &mut self,
        samples: &mut [f32],
        auxiliary_input_samples: &[f32],
        parameter_changes: &[(u32, u32, f64)],
    ) -> Result<(), String> {
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("CLAP Mixer effect input must contain non-empty stereo frames".to_owned());
        }
        let input_sample_count = samples.len();
        let expected_auxiliary_sample_count = self
            .auxiliary_input_buses
            .len()
            .checked_mul(input_sample_count)
            .ok_or_else(|| "CLAP Mixer sidechain block size overflow".to_owned())?;
        if auxiliary_input_samples.len() != expected_auxiliary_sample_count {
            return Err("CLAP Mixer sidechain block has an invalid size".to_owned());
        }
        let block_sample_limit = MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES as usize * 2;
        for (chunk_index, chunk) in samples.chunks_mut(block_sample_limit).enumerate() {
            let block_start = chunk_index
                .checked_mul(block_sample_limit / 2)
                .ok_or_else(|| "CLAP Mixer effect block offset overflow".to_owned())?;
            let frame_count = chunk.len() / 2;
            let block_end = block_start
                .checked_add(frame_count)
                .ok_or_else(|| "CLAP Mixer effect block end overflow".to_owned())?;
            self.collect_pending_parameter_updates();
            let block_parameter_change_count = parameter_changes
                .iter()
                .filter(|(frame_offset, _, _)| {
                    usize::try_from(*frame_offset)
                        .is_ok_and(|offset| (block_start..block_end).contains(&offset))
                })
                .count();
            if self
                .pending_parameter_updates
                .len()
                .checked_add(block_parameter_change_count)
                .is_none_or(|count| count > MAX_CLAP_MIXER_EFFECT_PARAMETER_CHANGES)
            {
                return Err("CLAP Mixer effect block has too many parameter changes".to_owned());
            }
            let live_parameter_frame_offset = u32::try_from(block_start)
                .map_err(|_| "CLAP Mixer block offset exceeds its parameter range".to_owned())?;
            let mut block_changes = self
                .pending_parameter_updates
                .iter()
                .map(|(parameter_id, value)| (live_parameter_frame_offset, *parameter_id, *value))
                .collect::<Vec<_>>();
            self.pending_parameter_updates.clear();
            block_changes.extend(
                parameter_changes
                    .iter()
                    .filter(|(frame_offset, _, _)| {
                        usize::try_from(*frame_offset)
                            .is_ok_and(|offset| (block_start..block_end).contains(&offset))
                    })
                    .copied(),
            );
            if block_changes.len() > MAX_CLAP_MIXER_EFFECT_PARAMETER_CHANGES {
                return Err("CLAP Mixer effect block has too many parameter changes".to_owned());
            }
            writeln!(
                self.stdin,
                "BLOCK {frame_count} {} {}",
                block_changes.len(),
                self.auxiliary_input_buses.len()
            )
            .map_err(|error| format!("could not send CLAP Mixer effect block: {error}"))?;
            for (frame_offset, parameter_id, value) in block_changes {
                let frame_offset = usize::try_from(frame_offset)
                    .ok()
                    .and_then(|offset| offset.checked_sub(block_start))
                    .ok_or_else(|| "CLAP Mixer parameter frame offset overflow".to_owned())?;
                if frame_offset >= frame_count || !value.is_finite() {
                    return Err("CLAP Mixer effect parameter change is invalid".to_owned());
                }
                writeln!(
                    self.stdin,
                    "PARAM {frame_offset} {parameter_id} {:016x}",
                    value.to_bits()
                )
                .map_err(|error| format!("could not send CLAP Mixer parameter change: {error}"))?;
            }
            for sample in chunk.iter() {
                let sample = if sample.is_finite() { *sample } else { 0.0 };
                self.stdin
                    .write_all(&sample.to_le_bytes())
                    .map_err(|error| format!("could not send CLAP Mixer effect audio: {error}"))?;
            }
            self.stdin
                .write_all(b"\n")
                .and_then(|_| self.stdin.flush())
                .map_err(|error| format!("could not finish CLAP Mixer effect block: {error}"))?;
            for (bus_position, (bus_index, _)) in self.auxiliary_input_buses.iter().enumerate() {
                let block_sample_offset = block_start
                    .checked_mul(2)
                    .ok_or_else(|| "CLAP Mixer sidechain offset overflow".to_owned())?;
                let bus_start = bus_position
                    .checked_mul(input_sample_count)
                    .and_then(|start| start.checked_add(block_sample_offset))
                    .ok_or_else(|| "CLAP Mixer sidechain offset overflow".to_owned())?;
                let bus_end = bus_start
                    .checked_add(chunk.len())
                    .filter(|end| *end <= auxiliary_input_samples.len())
                    .ok_or_else(|| "CLAP Mixer sidechain block end overflow".to_owned())?;
                writeln!(self.stdin, "SIDECHAIN {bus_index}").map_err(|error| {
                    format!("could not send CLAP Mixer sidechain header: {error}")
                })?;
                for sample in &auxiliary_input_samples[bus_start..bus_end] {
                    let sample = if sample.is_finite() { *sample } else { 0.0 };
                    self.stdin
                        .write_all(&sample.to_le_bytes())
                        .map_err(|error| {
                            format!("could not send CLAP Mixer sidechain audio: {error}")
                        })?;
                }
                self.stdin.write_all(b"\n").map_err(|error| {
                    format!("could not finish CLAP Mixer sidechain block: {error}")
                })?;
            }
            self.stdin
                .flush()
                .map_err(|error| format!("could not flush CLAP Mixer sidechain audio: {error}"))?;
            match self.receive_response()? {
                ClapMixerEffectProcessResponse::Audio {
                    samples,
                    latency_samples,
                    tail_samples,
                } if samples.len() == chunk.len() => {
                    chunk.copy_from_slice(&samples);
                    self.latency_samples = latency_samples;
                    self.tail_samples = tail_samples;
                }
                ClapMixerEffectProcessResponse::Audio { .. } => {
                    return Err("CLAP Mixer effect helper returned the wrong block size".to_owned());
                }
                ClapMixerEffectProcessResponse::Error(error) => return Err(error),
                _ => {
                    return Err(
                        "CLAP Mixer effect helper returned an invalid audio response".to_owned(),
                    );
                }
            }
        }
        self.capture_active_state_if_requested()?;
        Ok(())
    }

    fn collect_pending_parameter_updates(&mut self) {
        while let Ok(parameter_values) = self.parameter_update_receiver.try_recv() {
            for (parameter_id, value) in parameter_values {
                self.pending_parameter_updates.insert(parameter_id, value);
            }
        }
    }

    fn capture_active_state_if_requested(&mut self) -> Result<(), String> {
        let request = match self.capture_request_receiver.try_recv() {
            Ok(request) => request,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return Ok(()),
        };
        while let Ok(additional_request) = self.capture_request_receiver.try_recv() {
            let _ = additional_request.response.send(Err(
                "another CLAP Mixer effect state capture is already in progress".to_owned(),
            ));
        }
        if let Err(error) = self
            .stdin
            .write_all(b"SAVE_STATE\n")
            .and_then(|_| self.stdin.flush())
        {
            let message = format!("could not request active CLAP Mixer effect state: {error}");
            let _ = request.response.send(Err(message.clone()));
            return Err(message);
        }
        match self.receive_response()? {
            ClapMixerEffectProcessResponse::State(state) => {
                let _ = request.response.send(Ok(state));
                Ok(())
            }
            ClapMixerEffectProcessResponse::StateError(error) => {
                let _ = request.response.send(Err(error));
                Ok(())
            }
            ClapMixerEffectProcessResponse::Error(error) => {
                let _ = request.response.send(Err(error.clone()));
                Err(error)
            }
            _ => {
                let error =
                    "CLAP Mixer effect helper returned an invalid state response".to_owned();
                let _ = request.response.send(Err(error.clone()));
                Err(error)
            }
        }
    }

    fn stop(&mut self) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        self.stdin
            .write_all(b"STOP\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not stop CLAP Mixer effect processing: {error}"))?;
        match self.receive_response()? {
            ClapMixerEffectProcessResponse::Stopped => {
                self.stopped = true;
                Ok(())
            }
            ClapMixerEffectProcessResponse::Error(error) => Err(error),
            _ => Err("CLAP Mixer effect helper did not stop cleanly".to_owned()),
        }
    }

    fn receive_response(&self) -> Result<ClapMixerEffectProcessResponse, String> {
        match self
            .responses
            .recv_timeout(CLAP_MIXER_EFFECT_PROCESS_TIMEOUT)
        {
            Ok(response) => Ok(response),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err("CLAP Mixer effect helper timed out after 10 seconds".to_owned())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("CLAP Mixer effect helper stopped unexpectedly".to_owned())
            }
        }
    }
}

impl Drop for ClapMixerEffectProcess {
    fn drop(&mut self) {
        self._capture_registration.take();
        if !self.stopped {
            let _ = self.stdin.write_all(b"STOP\n");
            let _ = self.stdin.flush();
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub struct ClapProjectMixerEffectProcessorFactory {
    assignments: BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment>,
    capture_controller: ClapMixerEffectStateCaptureController,
}

impl ClapProjectMixerEffectProcessorFactory {
    pub fn new(
        assignments: BTreeMap<(usize, u16), ClapProjectMixerEffectAssignment>,
        capture_controller: ClapMixerEffectStateCaptureController,
    ) -> Self {
        Self {
            assignments,
            capture_controller,
        }
    }
}

impl MixerEffectProcessor for ClapMixerEffectProcess {
    fn sample_rate(&self) -> f64 {
        f64::from(self.sample_rate)
    }

    fn block_size(&self) -> usize {
        MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES as usize
    }

    fn input_channels(&self) -> usize {
        self.input_channels
    }

    fn output_channels(&self) -> usize {
        self.output_channels
    }

    fn auxiliary_input_buses(&self) -> &[(usize, usize)] {
        &self.auxiliary_input_buses
    }

    fn auxiliary_input_source_map(&self) -> Option<&[(usize, usize)]> {
        self.auxiliary_input_source_map.as_deref()
    }

    fn latency_samples(&self) -> u32 {
        self.latency_samples
    }

    fn manual_latency_offset_samples(&self) -> i32 {
        self.manual_latency_offset_samples
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
        ClapMixerEffectProcess::process_block(
            self,
            samples,
            auxiliary_input_samples,
            parameter_changes,
        )
    }

    fn stop_processing(&mut self) -> Result<(), String> {
        self.stop()
    }
}

impl MixerEffectProcessorFactory for ClapProjectMixerEffectProcessorFactory {
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
        let mut process = ClapMixerEffectProcess::spawn(
            &assignment.path,
            &assignment.plugin_id,
            sample_rate,
            assignment.state.as_deref(),
            &assignment.parameter_overrides,
        )?;
        process.manual_latency_offset_samples = assignment.manual_latency_offset_samples;
        if let Some(mapping) = &assignment.auxiliary_input_source_map {
            let mut validated_mapping = Vec::with_capacity(mapping.len());
            for (source_track, bus_index) in mapping {
                if !process
                    .auxiliary_input_buses
                    .iter()
                    .any(|(available_bus, _)| available_bus == bus_index)
                {
                    return Err(format!(
                        "CLAP Mixer insert {insert_ordinal} slot {} maps source insert {source_track} to unavailable auxiliary input {bus_index}",
                        slot_index + 1
                    ));
                }
                validated_mapping.push((*source_track, *bus_index));
            }
            process.auxiliary_input_source_map = Some(validated_mapping);
        }
        process.register_for_state_capture(
            &self.capture_controller,
            (insert_ordinal, slot_index),
            &assignment.path,
            &assignment.plugin_id,
        );
        Ok(Some(Box::new(process)))
    }
}

struct ClapPreviewProcess {
    path: PathBuf,
    plugin_id: String,
    sample_rate: u32,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    responses: Receiver<ClapPreviewProcessResponse>,
    reader: Option<JoinHandle<()>>,
}

impl ClapPreviewProcess {
    fn spawn(path: &Path, plugin_id: &str, sample_rate: u32) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate the application executable: {error}"))?;
        let mut child = Command::new(executable)
            .arg(NOTE_PREVIEW_SERVER_FLAG)
            .arg(path)
            .arg(plugin_id)
            .arg(sample_rate.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start the CLAP note preview helper: {error}"))?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP note preview helper did not provide input".to_owned());
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("CLAP note preview helper did not provide output".to_owned());
        };
        let (response_sender, responses) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("clap-preview-response".to_owned())
            .spawn(move || read_preview_process_responses(stdout, response_sender))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "could not start the CLAP preview response reader: {error}"
                ));
            }
        };
        let process = Self {
            path: path.to_owned(),
            plugin_id: plugin_id.to_owned(),
            sample_rate,
            child,
            stdin: BufWriter::new(stdin),
            responses,
            reader: Some(reader),
        };
        match process.receive_response()? {
            ClapPreviewProcessResponse::Ready => Ok(process),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Audio(_) => {
                Err("CLAP preview helper sent audio before initialization".to_owned())
            }
            ClapPreviewProcessResponse::State(_) => {
                Err("CLAP preview helper sent state before initialization".to_owned())
            }
        }
    }

    fn matches(&self, path: &Path, plugin_id: &str, sample_rate: u32) -> bool {
        self.path == path && self.plugin_id == plugin_id && self.sample_rate == sample_rate
    }

    fn render(
        &mut self,
        note_key: u8,
        velocity: u8,
        note_duration_milliseconds: u32,
    ) -> Result<Vec<f32>, String> {
        writeln!(
            self.stdin,
            "PREVIEW {note_key} {velocity} {note_duration_milliseconds}"
        )
        .and_then(|_| self.stdin.flush())
        .map_err(|error| format!("could not send a CLAP preview request: {error}"))?;
        match self.receive_response()? {
            ClapPreviewProcessResponse::Audio(samples) => Ok(samples),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Ready => {
                Err("CLAP preview helper restarted unexpectedly".to_owned())
            }
            ClapPreviewProcessResponse::State(_) => {
                Err("CLAP preview helper sent state instead of audio".to_owned())
            }
        }
    }

    fn render_pattern(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        self.render_pattern_request("PATTERN", settings)
    }

    fn render_pattern_loop(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        self.render_pattern_request("PATTERN_LOOP", settings)
    }

    fn render_playlist(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        self.render_pattern_request("PLAYLIST", settings)
    }

    fn render_pattern_request(
        &mut self,
        command: &str,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        if command == "PLAYLIST" {
            let playlist = settings
                .playlist
                .as_ref()
                .ok_or_else(|| "CLAP Playlist playback is missing its schedule".to_owned())?;
            validate_playlist_preview_settings(settings, self.sample_rate)?;
            writeln!(
                self.stdin,
                "PLAYLIST {} {:016x} {} {} {}",
                settings.ppq,
                settings.tempo_bpm.to_bits(),
                playlist.tempo_points.len(),
                playlist.notes.len(),
                parameter_automation_point_count(&playlist.parameter_automation),
            )
            .map_err(|error| format!("could not send a CLAP Playlist request: {error}"))?;
            for point in &playlist.tempo_points {
                writeln!(
                    self.stdin,
                    "TEMPO {} {:016x}",
                    point.tick,
                    point.tempo_bpm.to_bits()
                )
                .map_err(|error| format!("could not send a CLAP Playlist tempo: {error}"))?;
            }
            for note in &playlist.notes {
                writeln!(
                    self.stdin,
                    "NOTE {} {} {} {} {} {} {} {} {} {}",
                    note.start_tick,
                    note.stop_tick,
                    note.key,
                    note.velocity,
                    note.midi_channel,
                    note.fine_pitch_cents,
                    note.pan,
                    note.release,
                    note.mod_x,
                    u8::from(note.slide_note),
                )
                .map_err(|error| format!("could not send a CLAP Playlist note: {error}"))?;
            }
            write_parameter_automation(&mut self.stdin, &playlist.parameter_automation)?;
        } else {
            validate_pattern_preview_settings(settings, self.sample_rate)?;
            writeln!(
                self.stdin,
                "{command} {} {:016x} {} {} {}",
                settings.ppq,
                settings.tempo_bpm.to_bits(),
                settings.loop_length_ticks,
                settings.notes.len(),
                parameter_automation_point_count(&settings.parameter_automation),
            )
            .map_err(|error| format!("could not send a CLAP Pattern preview request: {error}"))?;
            for note in &settings.notes {
                writeln!(
                    self.stdin,
                    "NOTE {} {} {} {} {} {} {} {} {} {}",
                    note.position_ticks,
                    note.length_ticks,
                    note.key,
                    note.velocity,
                    note.midi_channel,
                    note.fine_pitch_cents,
                    note.pan,
                    note.release,
                    note.mod_x,
                    u8::from(note.slide_note),
                )
                .map_err(|error| format!("could not send a CLAP Pattern note: {error}"))?;
            }
            write_parameter_automation(&mut self.stdin, &settings.parameter_automation)?;
        }
        self.stdin
            .write_all(b"END\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not finish a CLAP Pattern preview request: {error}"))?;
        match self.receive_response()? {
            ClapPreviewProcessResponse::Audio(samples) => Ok(samples),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Ready => {
                Err("CLAP preview helper restarted unexpectedly".to_owned())
            }
            ClapPreviewProcessResponse::State(_) => {
                Err("CLAP preview helper sent state instead of audio".to_owned())
            }
        }
    }

    fn load_state(&mut self, state: &[u8]) -> Result<(), String> {
        if state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        writeln!(self.stdin, "LOAD_STATE {}", state.len())
            .and_then(|_| self.stdin.write_all(state))
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not send CLAP plug-in state: {error}"))?;
        match self.receive_response()? {
            ClapPreviewProcessResponse::Ready => Ok(()),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Audio(_) | ClapPreviewProcessResponse::State(_) => {
                Err("CLAP preview helper sent an unexpected response to state load".to_owned())
            }
        }
    }

    fn save_state(&mut self) -> Result<Vec<u8>, String> {
        self.stdin
            .write_all(b"SAVE_STATE\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not request CLAP plug-in state: {error}"))?;
        match self.receive_response()? {
            ClapPreviewProcessResponse::State(state) => Ok(state),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Audio(_) | ClapPreviewProcessResponse::Ready => {
                Err("CLAP preview helper sent an unexpected response to state save".to_owned())
            }
        }
    }

    fn set_parameter_values(&mut self, values: &[(u32, f64)]) -> Result<(), String> {
        if values.is_empty() || values.len() > MAX_CLAP_PREVIEW_PARAMETER_CHANGES {
            return Err(format!(
                "CLAP parameter update must contain between 1 and {MAX_CLAP_PREVIEW_PARAMETER_CHANGES} values"
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for &(param_id, value) in values {
            if ClapId::from_raw(param_id).is_none() || !value.is_finite() || !seen.insert(param_id)
            {
                return Err("CLAP parameter update contains an invalid value or ID".to_owned());
            }
        }
        writeln!(self.stdin, "SET_PARAMS {}", values.len())
            .map_err(|error| format!("could not send CLAP parameter updates: {error}"))?;
        for &(param_id, value) in values {
            writeln!(self.stdin, "PARAM {param_id} {:016x}", value.to_bits())
                .map_err(|error| format!("could not send a CLAP parameter value: {error}"))?;
        }
        self.stdin
            .write_all(b"END\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not finish CLAP parameter updates: {error}"))?;
        match self.receive_response()? {
            ClapPreviewProcessResponse::Ready => Ok(()),
            ClapPreviewProcessResponse::Error(error) => Err(error),
            ClapPreviewProcessResponse::Audio(_) | ClapPreviewProcessResponse::State(_) => Err(
                "CLAP preview helper sent an unexpected response to parameter updates".to_owned(),
            ),
        }
    }

    fn receive_response(&self) -> Result<ClapPreviewProcessResponse, String> {
        match self.responses.recv_timeout(PROBE_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err("CLAP note preview helper timed out after 10 seconds".to_owned())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("CLAP note preview helper stopped unexpectedly".to_owned())
            }
        }
    }
}

impl Drop for ClapPreviewProcess {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"STOP\n");
        let _ = self.stdin.flush();
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_preview_process_responses(
    stdout: ChildStdout,
    response_sender: Sender<ClapPreviewProcessResponse>,
) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let header = match read_preview_protocol_header(&mut stdout) {
            Ok(Some(header)) => header,
            Ok(None) => {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP note preview helper closed its output".to_owned(),
                ));
                return;
            }
            Err(error) => {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(format!(
                    "could not read from the CLAP preview helper: {error}"
                )));
                return;
            }
        };
        if header == "READY" {
            if response_sender
                .send(ClapPreviewProcessResponse::Ready)
                .is_err()
            {
                return;
            }
            continue;
        }
        if let Some(byte_count) = header.strip_prefix("OK ") {
            let Ok(byte_count) = byte_count.parse::<usize>() else {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP preview helper sent an invalid audio length".to_owned(),
                ));
                return;
            };
            if byte_count == 0
                || byte_count > MAX_NOTE_PREVIEW_OUTPUT_BYTES
                || !byte_count.is_multiple_of(8)
            {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP preview helper sent an invalid stereo audio length".to_owned(),
                ));
                return;
            }
            let mut samples = Vec::with_capacity(byte_count / std::mem::size_of::<f32>());
            let mut remaining_bytes = byte_count;
            let mut chunk = [0_u8; 16 * 1024];
            while remaining_bytes > 0 {
                let chunk_length = remaining_bytes.min(chunk.len());
                if let Err(error) = stdout.read_exact(&mut chunk[..chunk_length]) {
                    let _ = response_sender.send(ClapPreviewProcessResponse::Error(format!(
                        "could not read CLAP preview audio: {error}"
                    )));
                    return;
                }
                let (sample_bytes, remainder) = chunk[..chunk_length].as_chunks::<4>();
                debug_assert!(remainder.is_empty());
                for sample_bytes in sample_bytes {
                    let sample = f32::from_le_bytes(*sample_bytes);
                    samples.push(if sample.is_finite() { sample } else { 0.0 });
                }
                remaining_bytes -= chunk_length;
            }
            if response_sender
                .send(ClapPreviewProcessResponse::Audio(samples))
                .is_err()
            {
                return;
            }
            continue;
        }
        if let Some(byte_count) = header.strip_prefix("STATE ") {
            let Ok(byte_count) = byte_count.parse::<usize>() else {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP preview helper sent an invalid plug-in state length".to_owned(),
                ));
                return;
            };
            if byte_count > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP preview helper plug-in state exceeded the 32 MiB limit".to_owned(),
                ));
                return;
            }
            let mut state = vec![0; byte_count];
            if let Err(error) = stdout.read_exact(&mut state) {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(format!(
                    "could not read CLAP plug-in state: {error}"
                )));
                return;
            }
            let mut delimiter = [0; 1];
            if let Err(error) = stdout.read_exact(&mut delimiter) {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(format!(
                    "could not finish reading CLAP plug-in state: {error}"
                )));
                return;
            }
            if delimiter[0] != b'\n' {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(
                    "CLAP preview helper sent an invalid plug-in state delimiter".to_owned(),
                ));
                return;
            }
            if response_sender
                .send(ClapPreviewProcessResponse::State(state))
                .is_err()
            {
                return;
            }
            continue;
        }
        let error = header
            .strip_prefix("ERR ")
            .unwrap_or("CLAP preview helper sent an invalid response")
            .to_owned();
        let _ = response_sender.send(ClapPreviewProcessResponse::Error(error));
        return;
    }
}

fn read_mixer_effect_process_responses(
    stdout: ChildStdout,
    response_sender: Sender<ClapMixerEffectProcessResponse>,
) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let header = match read_preview_protocol_header(&mut stdout) {
            Ok(Some(header)) => header,
            Ok(None) => {
                let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                    "CLAP Mixer effect helper closed its output".to_owned(),
                ));
                return;
            }
            Err(error) => {
                let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(format!(
                    "could not read from the CLAP Mixer effect helper: {error}"
                )));
                return;
            }
        };
        if let Some(rest) = header.strip_prefix("READY ") {
            let mut fields = rest.split_whitespace();
            let input_channels = fields.next().and_then(|value| value.parse::<usize>().ok());
            let output_channels = fields.next().and_then(|value| value.parse::<usize>().ok());
            let auxiliary_bus_count = fields.next().and_then(|value| value.parse::<usize>().ok());
            let auxiliary_input_buses = auxiliary_bus_count.and_then(|count| {
                if count > MAX_NOTE_PREVIEW_AUDIO_PORTS as usize {
                    return None;
                }
                let mut buses = Vec::with_capacity(count);
                for _ in 0..count {
                    let bus_index = fields.next()?.parse::<usize>().ok()?;
                    let channel_count = fields.next()?.parse::<usize>().ok()?;
                    if bus_index >= MAX_NOTE_PREVIEW_AUDIO_PORTS as usize
                        || !matches!(channel_count, 1 | 2)
                        || buses.iter().any(|(existing, _)| *existing == bus_index)
                    {
                        return None;
                    }
                    buses.push((bus_index, channel_count));
                }
                (fields.next().is_none()).then_some(buses)
            });
            if let (Some(input_channels), Some(output_channels), Some(auxiliary_input_buses)) =
                (input_channels, output_channels, auxiliary_input_buses)
            {
                if response_sender
                    .send(ClapMixerEffectProcessResponse::Ready {
                        input_channels,
                        output_channels,
                        auxiliary_input_buses,
                    })
                    .is_err()
                {
                    return;
                }
                continue;
            }
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid bus layout".to_owned(),
            ));
            return;
        }
        if header == "STATE_LOADED" {
            if response_sender
                .send(ClapMixerEffectProcessResponse::StateLoaded)
                .is_err()
            {
                return;
            }
            continue;
        }
        if header == "PARAMETERS_SET" {
            if response_sender
                .send(ClapMixerEffectProcessResponse::ParametersSet)
                .is_err()
            {
                return;
            }
            continue;
        }
        if let Some(state_header) = header.strip_prefix("STATE ") {
            match read_preview_server_state(&mut stdout, state_header) {
                Ok(state) => {
                    if response_sender
                        .send(ClapMixerEffectProcessResponse::State(state))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(error));
                    return;
                }
            }
        }
        if let Some(error) = header.strip_prefix("STATE_ERROR ") {
            if response_sender
                .send(ClapMixerEffectProcessResponse::StateError(error.to_owned()))
                .is_err()
            {
                return;
            }
            continue;
        }
        if let Some(value) = header.strip_prefix("STARTED ") {
            let mut fields = value.split_whitespace();
            let latency_samples = fields.next().and_then(|field| field.parse::<u32>().ok());
            let tail_samples = match fields.next() {
                Some("NONE") => Some(None),
                Some(field) => field.parse::<u64>().ok().map(Some),
                None => None,
            };
            if let (Some(latency_samples), Some(tail_samples), None) =
                (latency_samples, tail_samples, fields.next())
            {
                if response_sender
                    .send(ClapMixerEffectProcessResponse::Started {
                        latency_samples,
                        tail_samples,
                    })
                    .is_err()
                {
                    return;
                }
                continue;
            }
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid start report".to_owned(),
            ));
            return;
        }
        if header == "BYE" {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Stopped);
            return;
        }
        if let Some(message) = header.strip_prefix("ERR ") {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(message.to_owned()));
            return;
        }
        let Some(value) = header.strip_prefix("BLOCK ") else {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid block header".to_owned(),
            ));
            return;
        };
        let mut fields = value.split_whitespace();
        let frame_count = fields.next().and_then(|field| field.parse::<u32>().ok());
        let latency_samples = fields.next().and_then(|field| field.parse::<u32>().ok());
        let tail_samples = match fields.next() {
            Some("NONE") => Some(None),
            Some(field) => field.parse::<u64>().ok().map(Some),
            None => None,
        };
        let (Some(frame_count), Some(latency_samples), Some(tail_samples), None) =
            (frame_count, latency_samples, tail_samples, fields.next())
        else {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid dynamic report".to_owned(),
            ));
            return;
        };
        if !(1..=MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES).contains(&frame_count) {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid block size".to_owned(),
            ));
            return;
        }
        let byte_count = frame_count as usize * 2 * std::mem::size_of::<f32>();
        let mut bytes = vec![0; byte_count];
        if let Err(error) = stdout.read_exact(&mut bytes) {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(format!(
                "could not read CLAP Mixer effect audio: {error}"
            )));
            return;
        }
        let mut delimiter = [0; 1];
        if stdout.read_exact(&mut delimiter).is_err() || delimiter[0] != b'\n' {
            let _ = response_sender.send(ClapMixerEffectProcessResponse::Error(
                "CLAP Mixer effect helper sent an invalid block delimiter".to_owned(),
            ));
            return;
        }
        let samples = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| {
                let sample = f32::from_le_bytes(*sample);
                if sample.is_finite() { sample } else { 0.0 }
            })
            .collect();
        if response_sender
            .send(ClapMixerEffectProcessResponse::Audio {
                samples,
                latency_samples,
                tail_samples,
            })
            .is_err()
        {
            return;
        }
    }
}

fn read_preview_protocol_header(stdout: &mut impl Read) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(64);
    let mut byte = [0; 1];
    loop {
        match stdout.read(&mut byte)? {
            0 if bytes.is_empty() => return Ok(None),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "CLAP preview helper ended an incomplete response header",
                ));
            }
            _ if byte[0] == b'\n' => break,
            _ if bytes.len() == MAX_NOTE_PREVIEW_PROTOCOL_HEADER_BYTES => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "CLAP preview helper response header exceeded its size limit",
                ));
            }
            _ => bytes.push(byte[0]),
        }
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn run_probe_subprocess(arguments: &[OsString], description: &str) -> Result<String, String> {
    let output = run_limited_subprocess(arguments, description, MAX_PROBE_OUTPUT_BYTES)?;
    String::from_utf8(output)
        .map_err(|error| format!("could not read CLAP {description} output: {error}"))
}

fn run_limited_subprocess(
    arguments: &[OsString],
    description: &str,
    max_output_bytes: usize,
) -> Result<Vec<u8>, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the application executable: {error}"))?;
    let mut child = Command::new(executable)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the CLAP {description}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("CLAP {description} did not provide output"))?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take((max_output_bytes + 1) as u64)
            .read_to_end(&mut output);
        result.map(|_| output)
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader
                    .join()
                    .map_err(|_| format!("CLAP {description} output reader stopped"))?
                    .map_err(|error| {
                        format!("could not read CLAP {description} output: {error}")
                    })?;
                if output.len() > max_output_bytes {
                    return Err(format!("CLAP {description} output exceeded its size limit"));
                }
                if !status.success() {
                    return Err(format!("CLAP {description} exited with {status}"));
                }
                return Ok(output);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("CLAP {description} timed out after 10 seconds"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not wait for CLAP {description}: {error}"));
            }
        }
    }
}

/// Entry point used only by the short-lived descriptor-probe child process.
pub fn run_child(path: &Path) -> i32 {
    match unsafe { PluginEntry::load(path) }
        .map_err(|error| format!("could not load CLAP entry: {error}"))
        .and_then(|entry| {
            let factory = entry
                .get_plugin_factory()
                .ok_or_else(|| "CLAP entry does not expose a plug-in factory".to_owned())?;
            let descriptors = factory
                .plugin_descriptors()
                .map(|descriptor| ClapPluginDescriptor {
                    id: descriptor
                        .id()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    name: descriptor
                        .name()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    vendor: descriptor
                        .vendor()
                        .map(|value| value.to_string_lossy().into_owned()),
                    version: descriptor
                        .version()
                        .map(|value| value.to_string_lossy().into_owned()),
                    description: descriptor
                        .description()
                        .map(|value| value.to_string_lossy().into_owned()),
                    features: descriptor
                        .features()
                        .map(|value| value.to_string_lossy().into_owned())
                        .collect(),
                })
                .collect::<Vec<_>>();
            Ok(encode_probe_output(&descriptors))
        }) {
        Ok(output) => {
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(output.as_bytes()).is_ok() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Entry point used only by the short-lived instance-inspection child process.
pub fn run_instance_child(path: &Path, plugin_id: &str) -> i32 {
    match inspect_plugin_instance(path, plugin_id).map(|info| encode_instance_probe_output(&info)) {
        Ok(output) => {
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(output.as_bytes()).is_ok() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Entry point used only by the short-lived note-preview child process.
pub fn run_note_preview_child(
    path: &Path,
    plugin_id: &str,
    sample_rate: u32,
    note_key: u8,
    velocity: u8,
) -> i32 {
    match render_note_preview(
        path,
        plugin_id,
        sample_rate,
        note_key,
        velocity,
        DEFAULT_NOTE_PREVIEW_NOTE_MILLISECONDS,
    ) {
        Ok(samples) => {
            let mut bytes = Vec::with_capacity(samples.len() * std::mem::size_of::<f32>());
            for sample in samples {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            if std::io::stdout().lock().write_all(&bytes).is_ok() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Serves note auditions from one initialized CLAP instance until the parent exits.
pub fn run_note_preview_server_child(path: &Path, plugin_id: &str, sample_rate: u32) -> i32 {
    let mut output = BufWriter::new(std::io::stdout().lock());
    let mut instance = match ClapNotePreviewInstance::new(path, plugin_id, sample_rate) {
        Ok(instance) => instance,
        Err(error) => {
            let _ = write_preview_server_error(&mut output, &error);
            let _ = output.flush();
            return 1;
        }
    };
    if writeln!(output, "READY")
        .and_then(|_| output.flush())
        .is_err()
    {
        return 1;
    }

    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    loop {
        let line = match read_preview_protocol_header(&mut input) {
            Ok(Some(line)) => line,
            Ok(None) => return 0,
            Err(error) => {
                let _ = write_preview_server_error(&mut output, &error.to_string());
                let _ = output.flush();
                return 1;
            }
        };
        if line == "STOP" {
            return 0;
        }

        if line == "SAVE_STATE" {
            match instance.save_state() {
                Ok(state) => {
                    if write_preview_server_state(&mut output, &state).is_err() {
                        return 1;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            }
        }

        if let Some(header) = line.strip_prefix("LOAD_STATE ") {
            let state = match read_preview_server_state(&mut input, header) {
                Ok(state) => state,
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            };
            match instance.load_state(&state) {
                Ok(()) => {
                    if writeln!(output, "READY")
                        .and_then(|_| output.flush())
                        .is_err()
                    {
                        return 1;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            }
        }

        if let Some(header) = line.strip_prefix("SET_PARAMS ") {
            let values = match read_preview_server_parameter_values(&mut input, header) {
                Ok(values) => values,
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            };
            match instance.set_parameter_values(&values) {
                Ok(()) => {
                    if writeln!(output, "READY")
                        .and_then(|_| output.flush())
                        .is_err()
                    {
                        return 1;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            }
        }

        let samples = if let Some(header) = line.strip_prefix("PREVIEW ") {
            match parse_note_preview_request(header) {
                Ok((note_key, velocity, note_duration)) => {
                    instance.render(note_key, velocity, note_duration)
                }
                Err(error) => Err(error),
            }
        } else if let Some(header) = line.strip_prefix("PATTERN ") {
            match parse_pattern_preview_request(header, &mut input, sample_rate) {
                Ok(settings) => instance.render_pattern(&settings),
                Err(error) => Err(error),
            }
        } else if let Some(header) = line.strip_prefix("PATTERN_LOOP ") {
            match parse_pattern_preview_request(header, &mut input, sample_rate) {
                Ok(settings) => instance.render_pattern_loop(&settings),
                Err(error) => Err(error),
            }
        } else if let Some(header) = line.strip_prefix("PLAYLIST ") {
            match parse_playlist_preview_request(header, &mut input, sample_rate) {
                Ok(settings) => instance.render_playlist(&settings),
                Err(error) => Err(error),
            }
        } else {
            Err("invalid CLAP preview request".to_owned())
        };

        let samples = match samples {
            Ok(samples) => samples,
            Err(error) => {
                let _ = write_preview_server_error(&mut output, &error);
                let _ = output.flush();
                return 1;
            }
        };
        if let Err(error) = write_preview_server_audio(&mut output, &samples) {
            let _ = write_preview_server_error(&mut output, &error.to_string());
            let _ = output.flush();
            return 1;
        }
    }
}

/// Serves Mixer effect blocks from one isolated CLAP instance until the parent exits.
pub fn run_mixer_effect_server_child(path: &Path, plugin_id: &str, sample_rate: u32) -> i32 {
    let mut output = BufWriter::new(std::io::stdout().lock());
    let mut instance = match ClapMixerEffectInstance::new(path, plugin_id, sample_rate) {
        Ok(instance) => instance,
        Err(error) => {
            let _ = write_preview_server_error(&mut output, &error);
            let _ = output.flush();
            return 1;
        }
    };
    let input_channels = instance.input_port_channels[instance.main_input_index];
    let output_channels = instance.output_port_channels[instance.main_output_index];
    let ready_result = (|| {
        write!(
            output,
            "READY {input_channels} {output_channels} {}",
            instance.auxiliary_input_buses.len()
        )?;
        for (bus_index, channel_count) in &instance.auxiliary_input_buses {
            write!(output, " {bus_index} {channel_count}")?;
        }
        writeln!(output)?;
        output.flush()
    })();
    if ready_result.is_err() {
        return 1;
    }

    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    loop {
        let line = match read_preview_protocol_header(&mut input) {
            Ok(Some(line)) => line,
            Ok(None) => return 0,
            Err(error) => {
                let _ = write_preview_server_error(&mut output, &error.to_string());
                let _ = output.flush();
                return 1;
            }
        };
        if line == "STOP" {
            return 0;
        }
        if let Some(header) = line.strip_prefix("LOAD_STATE ") {
            let state = match read_preview_server_state(&mut input, header) {
                Ok(state) => state,
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            };
            match instance.load_state(&state) {
                Ok(()) => {
                    if writeln!(output, "STATE_LOADED")
                        .and_then(|_| output.flush())
                        .is_err()
                    {
                        return 1;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            }
        }
        if line == "SAVE_STATE" {
            match instance.save_state() {
                Ok(state) => {
                    if write_preview_server_state(&mut output, &state).is_err() {
                        return 1;
                    }
                    continue;
                }
                Err(error) => {
                    let _ = write_preview_server_error(&mut output, &error);
                    let _ = output.flush();
                    return 1;
                }
            }
        }
        if let Some(encoded_count) = line.strip_prefix("SET_PARAMS ") {
            let Some(count) = encoded_count.parse::<usize>().ok() else {
                let _ = write_preview_server_error(
                    &mut output,
                    "invalid CLAP Mixer parameter override count",
                );
                let _ = output.flush();
                return 1;
            };
            if let Err(error) = instance.set_initial_parameter_values(&mut input, count) {
                let _ = write_preview_server_error(&mut output, &error);
                let _ = output.flush();
                return 1;
            }
            if writeln!(output, "PARAMETERS_SET")
                .and_then(|_| output.flush())
                .is_err()
            {
                return 1;
            }
            continue;
        }
        if line == "START" {
            return instance.run_processing_server(&mut input, &mut output);
        }

        let _ = write_preview_server_error(&mut output, "invalid CLAP Mixer effect command");
        let _ = output.flush();
        return 1;
    }
}

pub fn run_plugin_editor_child(path: &Path, plugin_id: &str) -> i32 {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let result = read_plugin_editor_input(&mut input).and_then(|(state, parameters)| {
        run_plugin_editor_ui(path, plugin_id, state.as_deref(), &parameters)
    });
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    match result {
        Ok(result) => match write_plugin_editor_response(&mut output, &result) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("could not return CLAP editor state: {error}");
                1
            }
        },
        Err(error) => {
            let _ = write_preview_server_error(&mut output, &error);
            let _ = output.flush();
            eprintln!("{error}");
            1
        }
    }
}

fn read_mixer_effect_sidechain_input(
    input: &mut impl Read,
    input_audio_storage: &mut [Vec<Vec<f32>>],
    frame_count: usize,
    bus_index: usize,
    channel_count: usize,
) -> Result<(), String> {
    let header = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP Mixer sidechain header: {error}"))?
        .ok_or_else(|| "CLAP Mixer sidechain header ended early".to_owned())?;
    let mut fields = header.split_whitespace();
    if fields.next() != Some("SIDECHAIN")
        || fields.next().and_then(|value| value.parse::<usize>().ok()) != Some(bus_index)
        || fields.next().is_some()
    {
        return Err("CLAP Mixer sidechain bus order does not match the plug-in".to_owned());
    }
    let byte_count = frame_count
        .checked_mul(2)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "CLAP Mixer sidechain block size overflow".to_owned())?;
    let mut bytes = vec![0; byte_count];
    input
        .read_exact(&mut bytes)
        .map_err(|error| format!("could not read CLAP Mixer sidechain audio: {error}"))?;
    let mut delimiter = [0; 1];
    if input.read_exact(&mut delimiter).is_err() || delimiter[0] != b'\n' {
        return Err("invalid CLAP Mixer sidechain block delimiter".to_owned());
    }
    let channels = input_audio_storage
        .get_mut(bus_index)
        .ok_or_else(|| "CLAP Mixer sidechain input bus is unavailable".to_owned())?;
    if channels.len() != channel_count || !matches!(channel_count, 1 | 2) {
        return Err("CLAP Mixer sidechain input bus has an unsupported layout".to_owned());
    }
    for frame in 0..frame_count {
        let left = f32::from_le_bytes(
            bytes[frame * 8..frame * 8 + 4]
                .try_into()
                .expect("left sample has a fixed width"),
        );
        let right = f32::from_le_bytes(
            bytes[frame * 8 + 4..frame * 8 + 8]
                .try_into()
                .expect("right sample has a fixed width"),
        );
        let left = if left.is_finite() { left } else { 0.0 };
        let right = if right.is_finite() { right } else { 0.0 };
        if channel_count == 1 {
            channels[0][frame] = (left + right) * 0.5;
        } else {
            channels[0][frame] = left;
            channels[1][frame] = right;
        }
    }
    Ok(())
}

struct ClapMixerEffectInstance {
    instance: PluginInstance<ClapMixerEffectHost>,
    sample_rate: u32,
    input_port_channels: Vec<u32>,
    main_input_index: usize,
    auxiliary_input_buses: Vec<(usize, usize)>,
    output_port_channels: Vec<u32>,
    main_output_index: usize,
    parameter_ranges: BTreeMap<u32, (f64, f64, bool)>,
    initial_parameter_values: Vec<(u32, f64)>,
    latency_samples: u32,
    tail_samples: Option<u64>,
    restart_requested: Arc<AtomicBool>,
    callback_requested: Arc<AtomicBool>,
    latency_changed: Arc<AtomicBool>,
    tail_changed: Arc<AtomicBool>,
}

impl ClapMixerEffectInstance {
    fn new(path: &Path, plugin_id: &str, sample_rate: u32) -> Result<Self, String> {
        if !(8_000..=192_000).contains(&sample_rate) {
            return Err(format!(
                "unsupported CLAP Mixer effect sample rate: {sample_rate}"
            ));
        }

        let entry = unsafe { PluginEntry::load(path) }
            .map_err(|error| format!("could not load CLAP entry: {error}"))?;
        let plugin_id = CString::new(plugin_id)
            .map_err(|_| "CLAP plug-in ID contains an invalid null byte".to_owned())?;
        let host_info = HostInfo::new(
            "FL Studio Rebuild",
            "fl-studio-rust contributors",
            "https://github.com/cyroz1/fl-studio-rust",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|error| format!("could not describe the host to the CLAP plug-in: {error}"))?;
        let restart_requested = Arc::new(AtomicBool::new(false));
        let callback_requested = Arc::new(AtomicBool::new(false));
        let latency_changed = Arc::new(AtomicBool::new(false));
        let tail_changed = Arc::new(AtomicBool::new(false));
        let shared_restart_requested = Arc::clone(&restart_requested);
        let shared_callback_requested = Arc::clone(&callback_requested);
        let shared_latency_changed = Arc::clone(&latency_changed);
        let shared_tail_changed = Arc::clone(&tail_changed);
        let mut instance = PluginInstance::<ClapMixerEffectHost>::new(
            move |_| ClapMixerEffectHostShared {
                restart_requested: shared_restart_requested,
                callback_requested: shared_callback_requested,
                latency_changed: shared_latency_changed,
                tail_changed: shared_tail_changed,
            },
            |shared| ClapMixerEffectHostMainThread {
                latency_changed: shared.latency_changed.as_ref(),
            },
            &entry,
            &plugin_id,
            &host_info,
        )
        .map_err(|error| format!("could not initialize CLAP plug-in instance: {error}"))?;

        let (
            input_port_channels,
            main_input_index,
            auxiliary_input_buses,
            output_port_channels,
            main_output_index,
            parameter_ranges,
        ) = {
            let plugin = instance.plugin_handle();
            let audio_ports = plugin
                .get_extension::<PluginAudioPorts>()
                .ok_or_else(|| "CLAP effect does not expose audio ports".to_owned())?;
            let (input_port_channels, main_input_index) =
                read_preview_audio_inputs(&audio_ports, &plugin)?;
            let auxiliary_input_buses = input_port_channels
                .iter()
                .enumerate()
                .filter_map(|(bus_index, channel_count)| {
                    (bus_index != main_input_index && matches!(channel_count, 1 | 2))
                        .then_some((bus_index, *channel_count as usize))
                })
                .collect::<Vec<_>>();
            let (output_port_channels, main_output_index) =
                read_preview_audio_outputs(&audio_ports, &plugin)?;
            let main_input_channels = input_port_channels[main_input_index];
            let main_output_channels = output_port_channels[main_output_index];
            if !(1..=2).contains(&main_input_channels) || !(1..=2).contains(&main_output_channels) {
                return Err(format!(
                    "CLAP Mixer effects need mono or stereo main buses; plug-in reports {main_input_channels} input and {main_output_channels} output channels"
                ));
            }
            let mut parameter_ranges = BTreeMap::new();
            if let Some(params) = plugin.get_extension::<PluginParams>() {
                let count = params.count(&plugin);
                if count > MAX_INSTANCE_METADATA_ITEMS {
                    return Err(format!(
                        "CLAP effect exposes {count} parameters; the metadata limit is {MAX_INSTANCE_METADATA_ITEMS}"
                    ));
                }
                let mut buffer = ParamInfoBuffer::new();
                for index in 0..count {
                    let Some(info) = params.get_info(&plugin, index, &mut buffer) else {
                        continue;
                    };
                    if !info.flags.contains(ParamInfoFlags::IS_READONLY)
                        && info.min_value.is_finite()
                        && info.max_value.is_finite()
                        && info.min_value < info.max_value
                    {
                        parameter_ranges.insert(
                            info.id.get(),
                            (
                                info.min_value,
                                info.max_value,
                                info.flags.contains(ParamInfoFlags::IS_STEPPED),
                            ),
                        );
                    }
                }
            }
            (
                input_port_channels,
                main_input_index,
                auxiliary_input_buses,
                output_port_channels,
                main_output_index,
                parameter_ranges,
            )
        };

        Ok(Self {
            instance,
            sample_rate,
            input_port_channels,
            main_input_index,
            auxiliary_input_buses,
            output_port_channels,
            main_output_index,
            parameter_ranges,
            initial_parameter_values: Vec::new(),
            latency_samples: 0,
            tail_samples: None,
            restart_requested,
            callback_requested,
            latency_changed,
            tail_changed,
        })
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        let plugin = self.instance.plugin_handle();
        let state = plugin
            .get_extension::<PluginState>()
            .ok_or_else(|| "CLAP effect does not support state restoration".to_owned())?;
        let mut input = std::io::Cursor::new(bytes);
        state
            .load(&plugin, &mut input)
            .map_err(|error| format!("CLAP effect could not restore its state: {error}"))
    }

    fn set_initial_parameter_values(
        &mut self,
        input: &mut impl Read,
        count: usize,
    ) -> Result<(), String> {
        if count > MAX_INSTANCE_METADATA_ITEMS as usize {
            return Err(
                "CLAP Mixer parameter override count exceeds the metadata limit".to_owned(),
            );
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let line = read_preview_protocol_header(input)
                .map_err(|error| format!("could not read CLAP Mixer parameter override: {error}"))?
                .ok_or_else(|| "CLAP Mixer parameter override ended early".to_owned())?;
            let mut fields = line.split_whitespace();
            let (Some("PARAMETER"), Some(parameter_id), Some(value_bits), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err("invalid CLAP Mixer parameter override".to_owned());
            };
            let parameter_id = parameter_id
                .parse::<u32>()
                .ok()
                .and_then(ClapId::from_raw)
                .ok_or_else(|| "CLAP Mixer parameter ID is invalid".to_owned())?;
            let value = u64::from_str_radix(value_bits, 16)
                .ok()
                .map(f64::from_bits)
                .filter(|value| value.is_finite())
                .ok_or_else(|| "CLAP Mixer parameter value is invalid".to_owned())?;
            let Some((minimum, maximum, stepped)) =
                self.parameter_ranges.get(&parameter_id.get()).copied()
            else {
                return Err(format!(
                    "CLAP Mixer parameter {} is not writable",
                    parameter_id.get()
                ));
            };
            if !(minimum..=maximum).contains(&value) || (stepped && value.fract() != 0.0) {
                return Err(format!(
                    "CLAP Mixer parameter {} override is outside its supported range",
                    parameter_id.get()
                ));
            }
            if values
                .iter()
                .any(|(existing_id, _)| *existing_id == parameter_id.get())
            {
                return Err(format!(
                    "CLAP Mixer parameter {} override is duplicated",
                    parameter_id.get()
                ));
            }
            values.push((parameter_id.get(), value));
        }
        self.initial_parameter_values = values;
        Ok(())
    }

    fn save_state(&mut self) -> Result<Vec<u8>, String> {
        let plugin = self.instance.plugin_handle();
        let state = plugin
            .get_extension::<PluginState>()
            .ok_or_else(|| "CLAP effect does not support state saving".to_owned())?;
        let mut writer = LimitedClapStateWriter {
            bytes: Vec::new(),
            exceeded_limit: false,
        };
        let save_result = state.save(&plugin, &mut writer);
        if writer.exceeded_limit {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        save_result.map_err(|error| format!("CLAP effect could not save its state: {error}"))?;
        Ok(writer.bytes)
    }

    fn read_latency_samples(&mut self) -> u32 {
        let plugin = self.instance.plugin_handle();
        plugin
            .get_extension::<PluginLatency>()
            .map_or(0, |latency| latency.get(&plugin))
    }

    fn read_tail_samples(
        &self,
        audio_processor: &mut StartedPluginAudioProcessor<ClapMixerEffectHost>,
    ) -> Option<u64> {
        let plugin = audio_processor.plugin_handle();
        plugin
            .get_extension::<PluginTail>()
            .map(|tail| match tail.get(&plugin) {
                TailLength::Finite(samples) => u64::from(samples),
                TailLength::Infinite => u64::from(i32::MAX as u32),
            })
    }

    fn flush_initial_parameter_values(
        &mut self,
        audio_processor: &mut StartedPluginAudioProcessor<ClapMixerEffectHost>,
    ) -> Result<(), String> {
        if self.initial_parameter_values.is_empty() {
            return Ok(());
        }
        let mut input_events = EventBuffer::with_capacity(self.initial_parameter_values.len());
        for (parameter_id, value) in &self.initial_parameter_values {
            let parameter_id = ClapId::from_raw(*parameter_id)
                .ok_or_else(|| "CLAP Mixer parameter ID is invalid".to_owned())?;
            input_events.push(&ParamValueEvent::new(
                0,
                parameter_id,
                Pckn::match_all(),
                *value,
            ));
        }
        input_events.sort();
        let mut output_events = EventBuffer::new();
        let mut plugin = audio_processor.plugin_handle();
        let params = plugin
            .get_extension::<PluginParams>()
            .ok_or_else(|| "CLAP effect does not support parameter updates".to_owned())?;
        params.flush_active(
            &mut plugin,
            &input_events.as_input(),
            &mut output_events.as_output(),
        );
        self.initial_parameter_values.clear();
        Ok(())
    }

    fn validate_audio_layout_after_restart(&mut self) -> Result<(), String> {
        let plugin = self.instance.plugin_handle();
        let audio_ports = plugin
            .get_extension::<PluginAudioPorts>()
            .ok_or_else(|| "CLAP effect no longer exposes audio ports after restart".to_owned())?;
        let (input_channels, main_input_index) = read_preview_audio_inputs(&audio_ports, &plugin)?;
        let (output_channels, main_output_index) =
            read_preview_audio_outputs(&audio_ports, &plugin)?;
        if input_channels != self.input_port_channels
            || main_input_index != self.main_input_index
            || output_channels != self.output_port_channels
            || main_output_index != self.main_output_index
        {
            return Err(
                "CLAP effect changed its audio port layout during restart, which is unsupported"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn restart_audio_processor(
        &mut self,
        audio_processor: StartedPluginAudioProcessor<ClapMixerEffectHost>,
        audio_configuration: PluginAudioConfiguration,
    ) -> Result<StartedPluginAudioProcessor<ClapMixerEffectHost>, String> {
        let stopped_processor = audio_processor.stop_processing();
        self.instance.deactivate(stopped_processor);
        let stopped_processor = self
            .instance
            .activate(
                |shared, _| ClapMixerEffectHostAudioProcessor {
                    tail_changed: shared.tail_changed.as_ref(),
                },
                audio_configuration,
            )
            .map_err(|error| format!("could not reactivate CLAP effect: {error}"))?;
        if let Err(error) = self.validate_audio_layout_after_restart() {
            self.instance.deactivate(stopped_processor);
            return Err(error);
        }
        self.latency_samples = self.read_latency_samples();
        self.latency_changed.store(false, Ordering::Release);
        self.tail_changed.store(false, Ordering::Release);
        match stopped_processor.start_processing() {
            Ok(mut audio_processor) => {
                self.tail_samples = self.read_tail_samples(&mut audio_processor);
                self.tail_changed.store(false, Ordering::Release);
                Ok(audio_processor)
            }
            Err(error) => {
                self.instance.deactivate(error.into_stopped_processor());
                Err("CLAP effect could not restart audio processing".to_owned())
            }
        }
    }

    fn service_host_requests(
        &mut self,
        audio_processor: &mut Option<StartedPluginAudioProcessor<ClapMixerEffectHost>>,
        audio_configuration: PluginAudioConfiguration,
    ) -> Result<(), String> {
        if self.callback_requested.swap(false, Ordering::AcqRel) {
            self.instance.call_on_main_thread_callback();
        }
        if self.restart_requested.swap(false, Ordering::AcqRel) {
            let current = audio_processor
                .take()
                .ok_or_else(|| "CLAP effect processor is unavailable for restart".to_owned())?;
            *audio_processor = Some(self.restart_audio_processor(current, audio_configuration)?);
        } else if self.latency_changed.swap(false, Ordering::AcqRel) {
            self.latency_samples = self.read_latency_samples();
        }
        if self.tail_changed.swap(false, Ordering::AcqRel) {
            let audio_processor = audio_processor.as_mut().ok_or_else(|| {
                "CLAP effect processor is unavailable for tail refresh".to_owned()
            })?;
            self.tail_samples = self.read_tail_samples(audio_processor);
        }
        Ok(())
    }

    fn run_processing_server(&mut self, input: &mut impl Read, output: &mut impl Write) -> i32 {
        let audio_configuration = PluginAudioConfiguration {
            sample_rate: f64::from(self.sample_rate),
            min_frames_count: 1,
            max_frames_count: MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES,
        };
        let stopped_processor = match self.instance.activate(
            |shared, _| ClapMixerEffectHostAudioProcessor {
                tail_changed: shared.tail_changed.as_ref(),
            },
            audio_configuration,
        ) {
            Ok(processor) => processor,
            Err(error) => {
                let _ = write_preview_server_error(
                    output,
                    &format!("could not activate CLAP effect: {error}"),
                );
                let _ = output.flush();
                return 1;
            }
        };
        self.latency_samples = self.read_latency_samples();
        let mut audio_processor = match stopped_processor.start_processing() {
            Ok(processor) => Some(processor),
            Err(error) => {
                let stopped_processor = error.into_stopped_processor();
                self.instance.deactivate(stopped_processor);
                let _ = write_preview_server_error(
                    output,
                    "CLAP effect could not start audio processing",
                );
                let _ = output.flush();
                return 1;
            }
        };
        let initial_parameter_result = self.flush_initial_parameter_values(
            audio_processor
                .as_mut()
                .expect("started CLAP effect processor is present"),
        );
        if let Err(error) = initial_parameter_result {
            if let Some(audio_processor) = audio_processor.take() {
                self.instance.deactivate(audio_processor.stop_processing());
            }
            let _ = write_preview_server_error(output, &error);
            let _ = output.flush();
            return 1;
        }
        self.tail_samples = self.read_tail_samples(
            audio_processor
                .as_mut()
                .expect("started CLAP effect processor is present"),
        );
        self.latency_changed.store(false, Ordering::Release);
        self.tail_changed.store(false, Ordering::Release);
        if let Err(error) = self.service_host_requests(&mut audio_processor, audio_configuration) {
            if let Some(audio_processor) = audio_processor.take() {
                self.instance.deactivate(audio_processor.stop_processing());
            }
            let _ = write_preview_server_error(output, &error);
            let _ = output.flush();
            return 1;
        }
        let tail_report = self
            .tail_samples
            .map_or_else(|| "NONE".to_owned(), |tail| tail.to_string());
        if writeln!(output, "STARTED {} {tail_report}", self.latency_samples)
            .and_then(|_| output.flush())
            .is_err()
        {
            if let Some(audio_processor) = audio_processor.take() {
                self.instance.deactivate(audio_processor.stop_processing());
            }
            return 1;
        }

        let mut steady_time = 0u64;
        let mut input_audio_ports = AudioPorts::with_capacity(
            self.input_port_channels.iter().sum::<u32>() as usize,
            self.input_port_channels.len(),
        );
        let mut output_audio_ports = AudioPorts::with_capacity(
            self.output_port_channels.iter().sum::<u32>() as usize,
            self.output_port_channels.len(),
        );
        let mut result = 0;
        loop {
            let line = match read_preview_protocol_header(input) {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    let _ = write_preview_server_error(output, &error.to_string());
                    let _ = output.flush();
                    result = 1;
                    break;
                }
            };
            if line == "STOP" {
                let _ = writeln!(output, "BYE").and_then(|_| output.flush());
                break;
            }
            if line == "SAVE_STATE" {
                match self.save_state() {
                    Ok(state) => {
                        if write_preview_server_state(output, &state).is_err() {
                            result = 1;
                            break;
                        }
                    }
                    Err(error) => {
                        let message = error
                            .chars()
                            .map(|character| {
                                if character.is_control() {
                                    ' '
                                } else {
                                    character
                                }
                            })
                            .take((MAX_NOTE_PREVIEW_PROTOCOL_HEADER_BYTES - 32) / 4)
                            .collect::<String>();
                        if writeln!(output, "STATE_ERROR {message}")
                            .and_then(|_| output.flush())
                            .is_err()
                        {
                            result = 1;
                            break;
                        }
                    }
                }
                continue;
            }
            let mut fields = line.split_whitespace();
            let (
                Some("BLOCK"),
                Some(frame_count),
                Some(parameter_count),
                Some(sidechain_count),
                None,
            ) = (
                fields.next(),
                fields.next().and_then(|value| value.parse::<u32>().ok()),
                fields.next().and_then(|value| value.parse::<usize>().ok()),
                fields.next().and_then(|value| value.parse::<usize>().ok()),
                fields.next(),
            )
            else {
                let _ =
                    write_preview_server_error(output, "invalid CLAP Mixer effect block header");
                let _ = output.flush();
                result = 1;
                break;
            };
            if !(1..=MAX_CLAP_MIXER_EFFECT_BLOCK_FRAMES).contains(&frame_count)
                || parameter_count > MAX_CLAP_MIXER_EFFECT_PARAMETER_CHANGES
                || sidechain_count != self.auxiliary_input_buses.len()
            {
                let _ = write_preview_server_error(
                    output,
                    "CLAP Mixer effect block exceeds its limits",
                );
                let _ = output.flush();
                result = 1;
                break;
            }
            let mut input_events = EventBuffer::with_capacity(parameter_count);
            for _ in 0..parameter_count {
                let event_line = match read_preview_protocol_header(input) {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        let _ = write_preview_server_error(
                            output,
                            "CLAP Mixer effect parameter list ended early",
                        );
                        let _ = output.flush();
                        result = 1;
                        break;
                    }
                    Err(error) => {
                        let _ = write_preview_server_error(output, &error.to_string());
                        let _ = output.flush();
                        result = 1;
                        break;
                    }
                };
                let mut event_fields = event_line.split_whitespace();
                let (Some("PARAM"), Some(frame_offset), Some(parameter_id), Some(value_bits), None) = (
                    event_fields.next(),
                    event_fields
                        .next()
                        .and_then(|value| value.parse::<u32>().ok()),
                    event_fields
                        .next()
                        .and_then(|value| value.parse::<u32>().ok()),
                    event_fields.next(),
                    event_fields.next(),
                ) else {
                    let _ = write_preview_server_error(
                        output,
                        "invalid CLAP Mixer effect parameter record",
                    );
                    let _ = output.flush();
                    result = 1;
                    break;
                };
                let Some(parameter_id) = ClapId::from_raw(parameter_id) else {
                    let _ = write_preview_server_error(
                        output,
                        "CLAP Mixer effect parameter ID is invalid",
                    );
                    let _ = output.flush();
                    result = 1;
                    break;
                };
                let normalized_value = match u64::from_str_radix(value_bits, 16) {
                    Ok(bits) => f64::from_bits(bits),
                    Err(_) => {
                        let _ = write_preview_server_error(
                            output,
                            "CLAP Mixer effect parameter value is invalid",
                        );
                        let _ = output.flush();
                        result = 1;
                        break;
                    }
                };
                if frame_offset >= frame_count
                    || !normalized_value.is_finite()
                    || !(0.0..=1.0).contains(&normalized_value)
                {
                    let _ = write_preview_server_error(
                        output,
                        "CLAP Mixer effect parameter change is outside its block",
                    );
                    let _ = output.flush();
                    result = 1;
                    break;
                }
                let Some((minimum, maximum, stepped)) =
                    self.parameter_ranges.get(&parameter_id.get()).copied()
                else {
                    let _ = write_preview_server_error(
                        output,
                        "CLAP Mixer effect parameter is not automatable",
                    );
                    let _ = output.flush();
                    result = 1;
                    break;
                };
                let mut value = minimum * (1.0 - normalized_value) + maximum * normalized_value;
                if !value.is_finite() {
                    let _ = write_preview_server_error(
                        output,
                        "CLAP Mixer effect parameter value is invalid",
                    );
                    let _ = output.flush();
                    result = 1;
                    break;
                }
                value = value.clamp(minimum, maximum);
                if stepped {
                    value = value.trunc();
                }
                input_events.push(&ParamValueEvent::new(
                    frame_offset,
                    parameter_id,
                    Pckn::match_all(),
                    value,
                ));
            }
            if result != 0 {
                break;
            }
            input_events.sort();
            let sample_count = frame_count as usize * 2;
            let byte_count = sample_count * std::mem::size_of::<f32>();
            let mut input_bytes = vec![0; byte_count];
            if let Err(error) = input.read_exact(&mut input_bytes) {
                let _ = write_preview_server_error(
                    output,
                    &format!("could not read CLAP effect audio: {error}"),
                );
                let _ = output.flush();
                result = 1;
                break;
            }
            let mut delimiter = [0; 1];
            if input.read_exact(&mut delimiter).is_err() || delimiter[0] != b'\n' {
                let _ =
                    write_preview_server_error(output, "invalid CLAP Mixer effect block delimiter");
                let _ = output.flush();
                result = 1;
                break;
            }

            let mut input_audio_storage = self
                .input_port_channels
                .iter()
                .map(|channel_count| {
                    (0..*channel_count)
                        .map(|_| vec![0.0f32; frame_count as usize])
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let main_input = &mut input_audio_storage[self.main_input_index];
            for frame in 0..frame_count as usize {
                let left = f32::from_le_bytes(
                    input_bytes[frame * 8..frame * 8 + 4]
                        .try_into()
                        .expect("left sample has a fixed width"),
                );
                let right = f32::from_le_bytes(
                    input_bytes[frame * 8 + 4..frame * 8 + 8]
                        .try_into()
                        .expect("right sample has a fixed width"),
                );
                let left = if left.is_finite() { left } else { 0.0 };
                let right = if right.is_finite() { right } else { 0.0 };
                if main_input.len() == 1 {
                    main_input[0][frame] = (left + right) * 0.5;
                } else {
                    main_input[0][frame] = left;
                    main_input[1][frame] = right;
                }
            }
            for (bus_index, channel_count) in &self.auxiliary_input_buses {
                if let Err(error) = read_mixer_effect_sidechain_input(
                    input,
                    &mut input_audio_storage,
                    frame_count as usize,
                    *bus_index,
                    *channel_count,
                ) {
                    let _ = write_preview_server_error(output, &error);
                    let _ = output.flush();
                    result = 1;
                    break;
                }
            }
            if result != 0 {
                break;
            }
            let mut output_audio_storage = self
                .output_port_channels
                .iter()
                .map(|channel_count| {
                    (0..*channel_count)
                        .map(|_| vec![0.0f32; frame_count as usize])
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let mut output_events = EventBuffer::new();
            let process_result = {
                let input_audio =
                    input_audio_ports.with_input_buffers(input_audio_storage.iter_mut().map(
                        |channels| AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_input_only(
                                channels.iter_mut().map(InputChannel::constant),
                            ),
                        },
                    ));
                let mut output_audio =
                    output_audio_ports.with_output_buffers(output_audio_storage.iter_mut().map(
                        |channels| AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_output_only(
                                channels.iter_mut().map(Vec::as_mut_slice),
                            ),
                        },
                    ));
                audio_processor
                    .as_mut()
                    .expect("started CLAP effect processor is present")
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events.as_input(),
                        &mut output_events.as_output(),
                        Some(steady_time),
                        None,
                    )
            };
            if let Err(error) = process_result {
                let _ = write_preview_server_error(
                    output,
                    &format!("CLAP effect failed while processing audio: {error}"),
                );
                let _ = output.flush();
                result = 1;
                break;
            }
            if let Err(error) =
                self.service_host_requests(&mut audio_processor, audio_configuration)
            {
                let _ = write_preview_server_error(output, &error);
                let _ = output.flush();
                result = 1;
                break;
            }
            steady_time = steady_time.saturating_add(u64::from(frame_count));
            let main_output = &output_audio_storage[self.main_output_index];
            let tail_report = self
                .tail_samples
                .map_or_else(|| "NONE".to_owned(), |tail| tail.to_string());
            if writeln!(
                output,
                "BLOCK {frame_count} {} {tail_report}",
                self.latency_samples
            )
            .is_err()
            {
                result = 1;
                break;
            }
            for frame in 0..frame_count as usize {
                let left = main_output[0][frame];
                let right = if main_output.len() == 1 {
                    left
                } else {
                    main_output[1][frame]
                };
                let left = if left.is_finite() { left } else { 0.0 };
                let right = if right.is_finite() { right } else { 0.0 };
                if output.write_all(&left.to_le_bytes()).is_err()
                    || output.write_all(&right.to_le_bytes()).is_err()
                {
                    result = 1;
                    break;
                }
            }
            if result != 0
                || output
                    .write_all(b"\n")
                    .and_then(|_| output.flush())
                    .is_err()
            {
                result = 1;
                break;
            }
        }
        if let Some(audio_processor) = audio_processor {
            let stopped_processor = audio_processor.stop_processing();
            self.instance.deactivate(stopped_processor);
        }
        result
    }
}

fn parse_note_preview_request(header: &str) -> Result<(u8, u8, u32), String> {
    let mut fields = header.split_whitespace();
    match (
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next(),
    ) {
        (Some(note_key), Some(velocity), Some(note_duration), None) => {
            Ok((note_key, velocity, note_duration))
        }
        _ => Err("invalid CLAP note preview request".to_owned()),
    }
}

fn parse_pattern_preview_request(
    header: &str,
    input: &mut impl Read,
    sample_rate: u32,
) -> Result<ClapPatternPreviewSettings, String> {
    let mut fields = header.split_whitespace();
    let (
        Some(ppq),
        Some(tempo_bits),
        Some(loop_length_ticks),
        Some(note_count),
        Some(parameter_count),
        None,
    ) = (
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next(),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next(),
    )
    else {
        return Err("invalid CLAP Pattern preview header".to_owned());
    };
    let tempo_bpm = u64::from_str_radix(tempo_bits, 16)
        .map(f64::from_bits)
        .map_err(|_| "invalid CLAP Pattern preview tempo".to_owned())?;
    if note_count > MAX_PATTERN_PREVIEW_NOTES || parameter_count > MAX_PATTERN_PREVIEW_NOTES {
        return Err("CLAP Pattern preview request exceeds its event limit".to_owned());
    }
    let mut notes = Vec::with_capacity(note_count);
    for _ in 0..note_count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP Pattern note: {error}"))?
            .ok_or_else(|| "CLAP Pattern preview ended before all notes arrived".to_owned())?;
        notes.push(parse_pattern_preview_note(&line)?);
    }
    let parameter_automation = read_parameter_automation(input, parameter_count, "Pattern")?;
    let end = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP Pattern preview terminator: {error}"))?
        .ok_or_else(|| "CLAP Pattern preview ended before its terminator".to_owned())?;
    if end != "END" {
        return Err("invalid CLAP Pattern preview terminator".to_owned());
    }
    let settings = ClapPatternPreviewSettings {
        pattern_id: 0,
        channel_id: 0,
        ppq,
        tempo_bpm,
        loop_length_ticks,
        notes,
        parameter_automation,
        playlist: None,
    };
    validate_pattern_preview_settings(&settings, sample_rate)?;
    Ok(settings)
}

fn parse_playlist_preview_request(
    header: &str,
    input: &mut impl Read,
    sample_rate: u32,
) -> Result<ClapPatternPreviewSettings, String> {
    let mut fields = header.split_whitespace();
    let (
        Some(ppq),
        Some(tempo_bits),
        Some(tempo_count),
        Some(note_count),
        Some(parameter_count),
        None,
    ) = (
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next(),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next(),
    )
    else {
        return Err("invalid CLAP Playlist preview header".to_owned());
    };
    let tempo_bpm = u64::from_str_radix(tempo_bits, 16)
        .map(f64::from_bits)
        .map_err(|_| "invalid CLAP Playlist base tempo".to_owned())?;
    if tempo_count > MAX_PATTERN_PREVIEW_NOTES
        || note_count > MAX_PATTERN_PREVIEW_NOTES
        || parameter_count > MAX_PATTERN_PREVIEW_NOTES
    {
        return Err("CLAP Playlist request exceeds its event limit".to_owned());
    }
    let mut tempo_points = Vec::with_capacity(tempo_count);
    for _ in 0..tempo_count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP Playlist tempo: {error}"))?
            .ok_or_else(|| "CLAP Playlist request ended before all tempos arrived".to_owned())?;
        tempo_points.push(parse_playlist_tempo_point(&line)?);
    }
    let mut notes = Vec::with_capacity(note_count);
    for _ in 0..note_count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP Playlist note: {error}"))?
            .ok_or_else(|| "CLAP Playlist request ended before all notes arrived".to_owned())?;
        notes.push(parse_playlist_note(&line)?);
    }
    let parameter_automation = read_parameter_automation(input, parameter_count, "Playlist")?;
    let end = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP Playlist terminator: {error}"))?
        .ok_or_else(|| "CLAP Playlist request ended before its terminator".to_owned())?;
    if end != "END" {
        return Err("invalid CLAP Playlist preview terminator".to_owned());
    }
    let settings = ClapPatternPreviewSettings {
        pattern_id: 0,
        channel_id: 0,
        ppq,
        tempo_bpm,
        loop_length_ticks: 1,
        notes: Vec::new(),
        parameter_automation: Vec::new(),
        playlist: Some(ClapPlaylistPreviewSettings {
            tempo_points,
            notes,
            parameter_automation,
        }),
    };
    validate_playlist_preview_settings(&settings, sample_rate)?;
    Ok(settings)
}

fn parameter_automation_point_count(lanes: &[ClapParameterAutomation]) -> usize {
    lanes.iter().fold(0usize, |count, lane| {
        count.saturating_add(lane.points.len())
    })
}

fn write_parameter_automation(
    output: &mut impl Write,
    lanes: &[ClapParameterAutomation],
) -> Result<(), String> {
    for lane in lanes {
        for point in &lane.points {
            writeln!(
                output,
                "PARAM {} {} {:016x}",
                lane.parameter_id,
                point.tick,
                point.normalized_value.to_bits(),
            )
            .map_err(|error| {
                format!("could not send a CLAP parameter automation event: {error}")
            })?;
        }
    }
    Ok(())
}

fn read_parameter_automation(
    input: &mut impl Read,
    parameter_count: usize,
    label: &str,
) -> Result<Vec<ClapParameterAutomation>, String> {
    let mut lanes = std::collections::BTreeMap::<u32, Vec<ClapParameterAutomationPoint>>::new();
    for _ in 0..parameter_count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP {label} parameter event: {error}"))?
            .ok_or_else(|| {
                format!("CLAP {label} request ended before all parameter events arrived")
            })?;
        let (parameter_id, tick, normalized_value) = parse_parameter_automation_point(&line)?;
        lanes
            .entry(parameter_id)
            .or_default()
            .push(ClapParameterAutomationPoint {
                tick,
                normalized_value,
            });
    }
    Ok(lanes
        .into_iter()
        .map(|(parameter_id, points)| ClapParameterAutomation {
            parameter_id,
            points,
        })
        .collect())
}

fn parse_parameter_automation_point(line: &str) -> Result<(u32, u64, f64), String> {
    let mut fields = line.split_whitespace();
    let (Some("PARAM"), Some(parameter_id), Some(tick), Some(value_bits), None) = (
        fields.next(),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next(),
        fields.next(),
    ) else {
        return Err("invalid CLAP parameter automation event".to_owned());
    };
    let normalized_value = u64::from_str_radix(value_bits, 16)
        .map(f64::from_bits)
        .map_err(|_| "invalid CLAP parameter automation value".to_owned())?;
    if ClapId::from_raw(parameter_id).is_none()
        || !normalized_value.is_finite()
        || !(0.0..=1.0).contains(&normalized_value)
    {
        return Err("CLAP parameter automation event has an invalid ID or value".to_owned());
    }
    Ok((parameter_id, tick, normalized_value))
}

fn parse_playlist_tempo_point(line: &str) -> Result<ClapPlaylistTempoPoint, String> {
    let mut fields = line.split_whitespace();
    let (Some("TEMPO"), Some(tick), Some(tempo_bits), None) = (
        fields.next(),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next(),
        fields.next(),
    ) else {
        return Err("invalid CLAP Playlist tempo record".to_owned());
    };
    let tempo_bpm = u64::from_str_radix(tempo_bits, 16)
        .map(f64::from_bits)
        .map_err(|_| "invalid CLAP Playlist tempo value".to_owned())?;
    Ok(ClapPlaylistTempoPoint { tick, tempo_bpm })
}

fn parse_playlist_note(line: &str) -> Result<ClapPlaylistNote, String> {
    let mut fields = line.split_whitespace();
    let (
        Some("NOTE"),
        Some(start_tick),
        Some(stop_tick),
        Some(key),
        Some(velocity),
        Some(midi_channel),
        Some(fine_pitch_cents),
        Some(pan),
        Some(release),
        Some(mod_x),
        Some(slide_note),
        None,
    ) = (
        fields.next(),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<i16>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next(),
        fields.next(),
    )
    else {
        return Err("invalid CLAP Playlist note".to_owned());
    };
    let slide_note = slide_note
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 1)
        .ok_or_else(|| "invalid CLAP Playlist slide-note flag".to_owned())?;
    Ok(ClapPlaylistNote {
        start_tick,
        stop_tick,
        key,
        velocity,
        midi_channel,
        fine_pitch_cents,
        pan,
        release,
        mod_x,
        slide_note: slide_note != 0,
    })
}

fn parse_pattern_preview_note(line: &str) -> Result<ClapPatternNote, String> {
    let mut fields = line.split_whitespace();
    let (
        Some("NOTE"),
        Some(position_ticks),
        Some(length_ticks),
        Some(key),
        Some(velocity),
        Some(midi_channel),
        Some(fine_pitch_cents),
        Some(pan),
        Some(release),
        Some(mod_x),
        Some(slide_note),
        None,
    ) = (
        fields.next(),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<i16>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next(),
        fields.next(),
    )
    else {
        return Err("invalid CLAP Pattern note".to_owned());
    };
    let slide_note = slide_note
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 1)
        .ok_or_else(|| "invalid CLAP Pattern slide-note flag".to_owned())?;
    Ok(ClapPatternNote {
        position_ticks,
        length_ticks,
        key,
        velocity,
        midi_channel,
        fine_pitch_cents,
        pan,
        release,
        mod_x,
        slide_note: slide_note != 0,
    })
}

fn write_preview_server_audio(output: &mut impl Write, samples: &[f32]) -> std::io::Result<()> {
    let byte_count = samples
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .filter(|byte_count| {
            *byte_count > 0
                && *byte_count <= MAX_NOTE_PREVIEW_OUTPUT_BYTES
                && byte_count.is_multiple_of(8)
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CLAP preview audio length is invalid",
            )
        })?;
    writeln!(output, "OK {byte_count}")?;
    for sample in samples {
        output.write_all(&sample.to_le_bytes())?;
    }
    output.flush()
}

fn read_preview_server_state(input: &mut impl Read, header: &str) -> Result<Vec<u8>, String> {
    let mut fields = header.split_whitespace();
    let Some(byte_count) = fields.next().and_then(|field| field.parse::<usize>().ok()) else {
        return Err("invalid CLAP plug-in state length".to_owned());
    };
    if fields.next().is_some() || byte_count > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
        return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
    }
    let mut state = vec![0; byte_count];
    input
        .read_exact(&mut state)
        .map_err(|error| format!("could not read CLAP plug-in state: {error}"))?;
    let mut delimiter = [0; 1];
    input
        .read_exact(&mut delimiter)
        .map_err(|error| format!("could not finish reading CLAP plug-in state: {error}"))?;
    if delimiter[0] != b'\n' {
        return Err("invalid CLAP plug-in state delimiter".to_owned());
    }
    Ok(state)
}

fn read_preview_server_parameter_values(
    input: &mut impl Read,
    header: &str,
) -> Result<Vec<(u32, f64)>, String> {
    let mut fields = header.split_whitespace();
    let Some(count) = fields.next().and_then(|field| field.parse::<usize>().ok()) else {
        return Err("invalid CLAP parameter update count".to_owned());
    };
    if fields.next().is_some() || count == 0 || count > MAX_CLAP_PREVIEW_PARAMETER_CHANGES {
        return Err("CLAP parameter update count is outside its supported range".to_owned());
    }
    let mut values = Vec::with_capacity(count);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP parameter value: {error}"))?
            .ok_or_else(|| "CLAP parameter update ended before all values arrived".to_owned())?;
        let mut fields = line.split_whitespace();
        let (Some("PARAM"), Some(param_id), Some(value_bits), None) = (
            fields.next(),
            fields.next().and_then(|field| field.parse::<u32>().ok()),
            fields.next(),
            fields.next(),
        ) else {
            return Err("invalid CLAP parameter value record".to_owned());
        };
        let value = u64::from_str_radix(value_bits, 16)
            .map(f64::from_bits)
            .map_err(|_| "invalid CLAP parameter value".to_owned())?;
        if ClapId::from_raw(param_id).is_none() || !value.is_finite() || !seen.insert(param_id) {
            return Err("CLAP parameter update contains an invalid value or ID".to_owned());
        }
        values.push((param_id, value));
    }
    let end = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP parameter update terminator: {error}"))?
        .ok_or_else(|| "CLAP parameter update ended before its terminator".to_owned())?;
    if end != "END" {
        return Err("invalid CLAP parameter update terminator".to_owned());
    }
    Ok(values)
}

fn write_preview_server_state(output: &mut impl Write, state: &[u8]) -> std::io::Result<()> {
    if state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CLAP plug-in state exceeds the 32 MiB limit",
        ));
    }
    writeln!(output, "STATE {}", state.len())?;
    output.write_all(state)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn read_plugin_editor_live_parameters(stderr: ChildStderr, sender: Sender<ClapPluginEditorEvent>) {
    let mut input = BufReader::new(stderr);
    loop {
        let line = match read_bounded_editor_stderr_line(&mut input, 128) {
            Ok(Some(line)) if !line.is_empty() => line,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        };
        let line = String::from_utf8_lossy(&line);
        let mut fields = line.split_whitespace();
        let (Some("LIVE_PARAMETER"), Some(parameter_id), Some(value_bits), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(parameter_id) = parameter_id
            .parse::<u32>()
            .ok()
            .filter(|id| ClapId::from_raw(*id).is_some())
        else {
            continue;
        };
        let Some(value) = u64::from_str_radix(value_bits, 16)
            .ok()
            .map(f64::from_bits)
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        if sender
            .send(ClapPluginEditorEvent::LiveParameters(BTreeMap::from([(
                parameter_id,
                value,
            )])))
            .is_err()
        {
            break;
        }
    }
}

fn read_bounded_editor_stderr_line(
    input: &mut impl BufRead,
    maximum_length: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::with_capacity(maximum_length);
    let mut too_long = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok((!line.is_empty() || too_long).then_some(if too_long {
                Vec::new()
            } else {
                line
            }));
        }
        let newline_offset = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline_offset.map_or(available.len(), |offset| offset + 1);
        if !too_long {
            let Some(new_length) = line.len().checked_add(consumed) else {
                too_long = true;
                line.clear();
                input.consume(consumed);
                if newline_offset.is_some() {
                    return Ok(Some(Vec::new()));
                }
                continue;
            };
            if new_length > maximum_length {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..consumed]);
            }
        }
        input.consume(consumed);
        if newline_offset.is_some() {
            return Ok(Some(if too_long { Vec::new() } else { line }));
        }
    }
}

fn read_plugin_editor_input(input: &mut impl Read) -> Result<ClapPluginEditorInput, String> {
    let state_header = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP editor state header: {error}"))?
        .ok_or_else(|| "CLAP editor request ended before its state header".to_owned())?;
    let mut fields = state_header.split_whitespace();
    let (Some("STATE"), Some(present), Some(length), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err("invalid CLAP editor state header".to_owned());
    };
    let present = present
        .parse::<u8>()
        .ok()
        .filter(|present| *present <= 1)
        .ok_or_else(|| "invalid CLAP editor state presence flag".to_owned())?;
    let length = length
        .parse::<usize>()
        .ok()
        .filter(|length| *length <= MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
        .ok_or_else(|| "CLAP editor state exceeds the 32 MiB limit".to_owned())?;
    if present == 0 && length != 0 {
        return Err("CLAP editor state presence and length do not match".to_owned());
    }
    let state = if present == 1 {
        let mut state = vec![0; length];
        input
            .read_exact(&mut state)
            .map_err(|error| format!("could not read CLAP editor state: {error}"))?;
        let mut delimiter = [0; 1];
        input
            .read_exact(&mut delimiter)
            .map_err(|error| format!("could not finish CLAP editor state: {error}"))?;
        if delimiter[0] != b'\n' {
            return Err("invalid CLAP editor state delimiter".to_owned());
        }
        Some(state)
    } else {
        None
    };
    let parameter_header = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP editor parameter header: {error}"))?
        .ok_or_else(|| "CLAP editor request ended before its parameters".to_owned())?;
    let mut fields = parameter_header.split_whitespace();
    let (Some("PARAMS"), Some(count), None) = (fields.next(), fields.next(), fields.next()) else {
        return Err("invalid CLAP editor parameter header".to_owned());
    };
    let count = count
        .parse::<usize>()
        .ok()
        .filter(|count| *count <= MAX_INSTANCE_METADATA_ITEMS as usize)
        .ok_or_else(|| "CLAP editor parameter count exceeds the metadata limit".to_owned())?;
    let mut parameter_values = BTreeMap::new();
    for _ in 0..count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP editor parameter: {error}"))?
            .ok_or_else(|| "CLAP editor request ended before all parameters arrived".to_owned())?;
        let mut fields = line.split_whitespace();
        let (Some("PARAM"), Some(parameter_id), Some(value_bits), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err("invalid CLAP editor parameter".to_owned());
        };
        let parameter_id = parameter_id
            .parse::<u32>()
            .ok()
            .filter(|id| ClapId::from_raw(*id).is_some())
            .ok_or_else(|| "CLAP editor parameter ID is invalid".to_owned())?;
        let value = u64::from_str_radix(value_bits, 16)
            .ok()
            .map(f64::from_bits)
            .filter(|value| value.is_finite())
            .ok_or_else(|| "CLAP editor parameter value is invalid".to_owned())?;
        if parameter_values.insert(parameter_id, value).is_some() {
            return Err("CLAP editor parameter ID is duplicated".to_owned());
        }
    }
    let end = read_preview_protocol_header(input)
        .map_err(|error| format!("could not read CLAP editor request terminator: {error}"))?
        .ok_or_else(|| "CLAP editor request ended before its terminator".to_owned())?;
    if end != "END" {
        return Err("invalid CLAP editor request terminator".to_owned());
    }
    Ok((state, parameter_values))
}

fn read_plugin_editor_response(output: ChildStdout) -> Result<ClapPluginEditorOutput, String> {
    let mut input = BufReader::new(output);
    let header = read_preview_protocol_header(&mut input)
        .map_err(|error| format!("could not read CLAP editor result: {error}"))?
        .ok_or_else(|| "CLAP editor helper closed without a result".to_owned())?;
    if let Some(error) = header.strip_prefix("ERR ") {
        return Err(error.to_owned());
    }
    let mut fields = header.split_whitespace();
    let (Some("RESULT"), Some(state_present), Some(state_length), Some(parameter_count), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Err("invalid CLAP editor result header".to_owned());
    };
    let state_present = state_present
        .parse::<u8>()
        .ok()
        .filter(|present| *present <= 1)
        .ok_or_else(|| "invalid CLAP editor result state flag".to_owned())?;
    let state_length = state_length
        .parse::<usize>()
        .ok()
        .filter(|length| *length <= MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
        .ok_or_else(|| "CLAP editor state exceeds the 32 MiB limit".to_owned())?;
    let parameter_count = parameter_count
        .parse::<usize>()
        .ok()
        .filter(|count| *count <= MAX_INSTANCE_METADATA_ITEMS as usize)
        .ok_or_else(|| "CLAP editor parameter count exceeds the metadata limit".to_owned())?;
    if state_present == 0 && state_length != 0 {
        return Err("CLAP editor result state presence and length do not match".to_owned());
    }
    let state = if state_present == 1 {
        let mut state = vec![0; state_length];
        input
            .read_exact(&mut state)
            .map_err(|error| format!("could not read CLAP editor result state: {error}"))?;
        let mut delimiter = [0; 1];
        input
            .read_exact(&mut delimiter)
            .map_err(|error| format!("could not finish CLAP editor result state: {error}"))?;
        if delimiter[0] != b'\n' {
            return Err("invalid CLAP editor result state delimiter".to_owned());
        }
        Some(state)
    } else {
        None
    };
    let mut parameter_overrides = BTreeMap::new();
    for _ in 0..parameter_count {
        let line = read_preview_protocol_header(&mut input)
            .map_err(|error| format!("could not read CLAP editor result parameter: {error}"))?
            .ok_or_else(|| "CLAP editor result ended before all parameters arrived".to_owned())?;
        let mut fields = line.split_whitespace();
        let (Some("PARAM"), Some(parameter_id), Some(value_bits), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err("invalid CLAP editor result parameter".to_owned());
        };
        let parameter_id = parameter_id
            .parse::<u32>()
            .ok()
            .filter(|id| ClapId::from_raw(*id).is_some())
            .ok_or_else(|| "CLAP editor result parameter ID is invalid".to_owned())?;
        let value = u64::from_str_radix(value_bits, 16)
            .ok()
            .map(f64::from_bits)
            .filter(|value| value.is_finite())
            .ok_or_else(|| "CLAP editor result parameter value is invalid".to_owned())?;
        if parameter_overrides.insert(parameter_id, value).is_some() {
            return Err("CLAP editor result parameter ID is duplicated".to_owned());
        }
    }
    if read_preview_protocol_header(&mut input)
        .map_err(|error| format!("could not finish reading CLAP editor result: {error}"))?
        .is_some()
    {
        return Err("CLAP editor helper returned trailing data".to_owned());
    }
    Ok(ClapPluginEditorOutput {
        state,
        parameter_overrides,
    })
}

fn write_plugin_editor_response(
    output: &mut impl Write,
    result: &ClapPluginEditorOutput,
) -> std::io::Result<()> {
    if result
        .state
        .as_ref()
        .is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CLAP plug-in state exceeds the 32 MiB limit",
        ));
    }
    if result.parameter_overrides.len() > MAX_INSTANCE_METADATA_ITEMS as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CLAP editor parameter count exceeds the metadata limit",
        ));
    }
    let state_length = result.state.as_ref().map_or(0, Vec::len);
    writeln!(
        output,
        "RESULT {} {state_length} {}",
        u8::from(result.state.is_some()),
        result.parameter_overrides.len()
    )?;
    if let Some(state) = &result.state {
        output.write_all(state)?;
        output.write_all(b"\n")?;
    }
    for (parameter_id, value) in &result.parameter_overrides {
        if ClapId::from_raw(*parameter_id).is_none() || !value.is_finite() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CLAP editor returned an invalid parameter value",
            ));
        }
        writeln!(output, "PARAM {parameter_id} {:016x}", value.to_bits())?;
    }
    output.flush()
}

fn write_preview_server_error(output: &mut impl Write, error: &str) -> std::io::Result<()> {
    let message = error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect::<String>();
    writeln!(output, "ERR {message}")
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PreviewNoteDialect {
    Clap,
    Midi,
    Midi2,
}

fn render_note_preview(
    path: &Path,
    plugin_id: &str,
    sample_rate: u32,
    note_key: u8,
    velocity: u8,
    note_duration_milliseconds: u32,
) -> Result<Vec<f32>, String> {
    let mut instance = ClapNotePreviewInstance::new(path, plugin_id, sample_rate)?;
    instance.render(note_key, velocity, note_duration_milliseconds)
}

struct ClapNotePreviewInstance {
    instance: PluginInstance<()>,
    sample_rate: u32,
    input_port_channels: Vec<u32>,
    output_port_channels: Vec<u32>,
    main_output_index: usize,
    note_port_index: u16,
    dialect: PreviewNoteDialect,
    next_steady_time: u64,
}

struct LimitedClapStateWriter {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

impl Write for LimitedClapStateWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(new_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded_limit = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CLAP plug-in state length overflowed",
            ));
        };
        if new_len > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
            self.exceeded_limit = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CLAP plug-in state exceeds the 32 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct ClapPluginEditorSignals {
    parameter_flush_requested: Arc<AtomicBool>,
    gui_closed: Arc<AtomicBool>,
    gui_was_destroyed: Arc<AtomicBool>,
    gui_show_requested: Arc<AtomicBool>,
    gui_hide_requested: Arc<AtomicBool>,
    resize_requested: Arc<Mutex<Option<GuiSize>>>,
}

impl ClapPluginEditorSignals {
    fn new() -> Self {
        Self {
            parameter_flush_requested: Arc::new(AtomicBool::new(false)),
            gui_closed: Arc::new(AtomicBool::new(false)),
            gui_was_destroyed: Arc::new(AtomicBool::new(false)),
            gui_show_requested: Arc::new(AtomicBool::new(false)),
            gui_hide_requested: Arc::new(AtomicBool::new(false)),
            resize_requested: Arc::new(Mutex::new(None)),
        }
    }

    fn host_shared(&self) -> ClapPluginEditorHostShared {
        ClapPluginEditorHostShared {
            parameter_flush_requested: Arc::clone(&self.parameter_flush_requested),
            gui_closed: Arc::clone(&self.gui_closed),
            gui_was_destroyed: Arc::clone(&self.gui_was_destroyed),
            gui_show_requested: Arc::clone(&self.gui_show_requested),
            gui_hide_requested: Arc::clone(&self.gui_hide_requested),
            resize_requested: Arc::clone(&self.resize_requested),
        }
    }
}

struct ClapPluginEditorSession {
    instance: PluginInstance<ClapPluginEditorHost>,
    signals: ClapPluginEditorSignals,
    gui: Option<PluginGui>,
    gui_created: bool,
    gui_floating: bool,
    gui_size_scale: f64,
    parameter_values_before_editor: BTreeMap<u32, f64>,
}

impl ClapPluginEditorSession {
    fn new(
        path: &Path,
        plugin_id: &str,
        state: Option<&[u8]>,
        parameter_overrides: &BTreeMap<u32, f64>,
    ) -> Result<Self, String> {
        if state.is_some_and(|state| state.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES) {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        let entry = unsafe { PluginEntry::load(path) }
            .map_err(|error| format!("could not load CLAP entry: {error}"))?;
        let plugin_id = CString::new(plugin_id)
            .map_err(|_| "CLAP plug-in ID contains an invalid null byte".to_owned())?;
        let host_info = HostInfo::new(
            "FL Studio Rebuild",
            "fl-studio-rust contributors",
            "https://github.com/cyroz1/fl-studio-rust",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|error| format!("could not describe the host to the CLAP plug-in: {error}"))?;
        let signals = ClapPluginEditorSignals::new();
        let shared_signals = signals.clone();
        let mut instance = PluginInstance::<ClapPluginEditorHost>::new(
            move |_| shared_signals.host_shared(),
            |_| ClapPluginEditorHostMainThread,
            &entry,
            &plugin_id,
            &host_info,
        )
        .map_err(|error| format!("could not initialize CLAP plug-in instance: {error}"))?;
        if let Some(state) = state {
            let plugin = instance.plugin_handle();
            let state_extension = plugin
                .get_extension::<PluginState>()
                .ok_or_else(|| "CLAP plug-in does not support state restoration".to_owned())?;
            let mut input = std::io::Cursor::new(state);
            state_extension
                .load(&plugin, &mut input)
                .map_err(|error| format!("CLAP plug-in could not restore its state: {error}"))?;
        }
        if !parameter_overrides.is_empty() {
            apply_clap_editor_parameter_overrides(&mut instance, parameter_overrides)?;
        }
        let mut session = Self {
            instance,
            signals,
            gui: None,
            gui_created: false,
            gui_floating: false,
            gui_size_scale: 1.0,
            parameter_values_before_editor: BTreeMap::new(),
        };
        session.parameter_values_before_editor = session.capture_parameters()?;
        Ok(session)
    }

    fn open_gui(&mut self, creation: &eframe::CreationContext<'_>) -> Result<egui::Vec2, String> {
        let parent = clap_editor_parent_window(creation)?;
        let api_type = parent
            .api_type()
            .to_standard_api()
            .ok_or_else(|| "the CLAP editor host window uses an unsupported API".to_owned())?;
        let plugin = self.instance.plugin_handle();
        let gui = plugin
            .get_extension::<PluginGui>()
            .ok_or_else(|| "CLAP plug-in does not expose a native editor".to_owned())?;
        let configurations = if api_type.supports_embedding() {
            [
                Some(GuiConfiguration {
                    api_type,
                    is_floating: false,
                }),
                Some(GuiConfiguration {
                    api_type,
                    is_floating: true,
                }),
            ]
        } else {
            [
                Some(GuiConfiguration {
                    api_type,
                    is_floating: true,
                }),
                None,
            ]
        };
        let configuration = configurations
            .into_iter()
            .flatten()
            .find(|configuration| gui.is_api_supported(&plugin, *configuration))
            .ok_or_else(|| {
                format!(
                    "CLAP plug-in does not support a native editor on {}",
                    api_type.0.to_string_lossy()
                )
            })?;
        gui.create(&plugin, configuration)
            .map_err(|error| format!("could not create CLAP editor: {error}"))?;
        self.gui = Some(gui);
        self.gui_created = true;
        self.gui_floating = configuration.is_floating;

        let logical_size = if !api_type.uses_logical_size() {
            let scale = f64::from(creation.egui_ctx.pixels_per_point()).max(1.0);
            let _ = gui.set_scale(&plugin, scale);
            scale
        } else {
            1.0
        };
        self.gui_size_scale = logical_size;
        let plugin_size = gui.get_size(&plugin).unwrap_or(GuiSize {
            width: 800,
            height: 600,
        });
        if configuration.is_floating {
            let title = CString::new("CLAP plug-in editor")
                .expect("constant editor title contains no null byte");
            gui.suggest_title(&plugin, &title);
            // SAFETY: `parent` is a live eframe window and remains alive until `gui.destroy`.
            unsafe { gui.set_transient(&plugin, parent) }
                .map_err(|error| format!("could not parent the floating CLAP editor: {error}"))?;
        } else {
            // SAFETY: `parent` is a live eframe window and remains alive until `gui.destroy`.
            unsafe { gui.set_parent(&plugin, parent) }
                .map_err(|error| format!("could not embed the CLAP editor: {error}"))?;
        }
        gui.show(&plugin)
            .map_err(|error| format!("could not show the CLAP editor: {error}"))?;
        if configuration.is_floating {
            Ok(egui::vec2(480.0, 120.0))
        } else {
            Ok(egui::vec2(
                (f64::from(plugin_size.width) / logical_size) as f32,
                (f64::from(plugin_size.height) / logical_size) as f32,
            ))
        }
    }

    fn service_host_requests(&mut self) -> Result<Option<GuiSize>, String> {
        if self
            .signals
            .parameter_flush_requested
            .swap(false, Ordering::AcqRel)
        {
            self.flush_parameters()?;
        }
        let requested_size = self
            .signals
            .resize_requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if self.gui_floating || requested_size.is_none() {
            return Ok(None);
        }
        let can_resize = self.gui.is_some_and(|gui| {
            let plugin = self.instance.plugin_handle();
            gui.can_resize(&plugin)
        });
        Ok(if can_resize { requested_size } else { None })
    }

    fn flush_parameters(&mut self) -> Result<(), String> {
        let params = {
            let plugin = self.instance.plugin_handle();
            plugin.get_extension::<PluginParams>()
        };
        let Some(params) = params else {
            return Ok(());
        };
        let mut input_events = EventBuffer::new();
        let mut output_events = EventBuffer::new();
        let mut plugin = self
            .instance
            .inactive_plugin_handle()
            .ok_or_else(|| "CLAP editor plug-in unexpectedly became active".to_owned())?;
        params.flush(
            &mut plugin,
            &input_events.as_input(),
            &mut output_events.as_output(),
        );
        input_events.clear();
        Ok(())
    }

    fn capture_parameters(&mut self) -> Result<BTreeMap<u32, f64>, String> {
        let plugin = self.instance.plugin_handle();
        let Some(params) = plugin.get_extension::<PluginParams>() else {
            return Ok(BTreeMap::new());
        };
        let count = params.count(&plugin);
        if count > MAX_INSTANCE_METADATA_ITEMS {
            return Err(format!(
                "CLAP plug-in exposes {count} parameters; the metadata limit is {MAX_INSTANCE_METADATA_ITEMS}"
            ));
        }
        let mut buffer = ParamInfoBuffer::new();
        let mut values = BTreeMap::new();
        for index in 0..count {
            let Some(info) = params.get_info(&plugin, index, &mut buffer) else {
                continue;
            };
            if info.flags.contains(ParamInfoFlags::IS_READONLY) {
                continue;
            }
            let parameter_id = info.id.get();
            let Some(value) = params
                .get_value(&plugin, info.id)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            values.insert(parameter_id, value);
        }
        Ok(values)
    }

    fn save_state(&mut self) -> Result<Option<Vec<u8>>, String> {
        let plugin = self.instance.plugin_handle();
        let Some(state) = plugin.get_extension::<PluginState>() else {
            return Ok(None);
        };
        let mut writer = LimitedClapStateWriter {
            bytes: Vec::new(),
            exceeded_limit: false,
        };
        let save_result = state.save(&plugin, &mut writer);
        if writer.exceeded_limit {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        save_result.map_err(|error| format!("CLAP plug-in could not save its state: {error}"))?;
        Ok(Some(writer.bytes))
    }

    fn acknowledge_gui_closed(&mut self) {
        if self.signals.gui_was_destroyed.swap(false, Ordering::AcqRel) && self.gui_created {
            if let Some(gui) = self.gui {
                gui.destroy(&self.instance.plugin_handle());
            }
            self.gui_created = false;
        }
    }

    fn close_and_capture(&mut self) -> Result<ClapPluginEditorOutput, String> {
        if self.gui_created {
            if let Some(gui) = self.gui {
                gui.destroy(&self.instance.plugin_handle());
            }
            self.gui_created = false;
        }
        let current_parameters = self.capture_parameters()?;
        let parameter_overrides = current_parameters
            .into_iter()
            .filter(|(parameter_id, value)| {
                self.parameter_values_before_editor
                    .get(parameter_id)
                    .is_none_or(|previous| previous.to_bits() != value.to_bits())
            })
            .collect();
        let state = self.save_state()?;
        Ok(ClapPluginEditorOutput {
            state,
            parameter_overrides,
        })
    }
}

impl Drop for ClapPluginEditorSession {
    fn drop(&mut self) {
        if self.gui_created {
            if let Some(gui) = self.gui {
                gui.destroy(&self.instance.plugin_handle());
            }
            self.gui_created = false;
        }
    }
}

fn apply_clap_editor_parameter_overrides(
    instance: &mut PluginInstance<ClapPluginEditorHost>,
    parameter_overrides: &BTreeMap<u32, f64>,
) -> Result<(), String> {
    let params = {
        let plugin = instance.plugin_handle();
        plugin
            .get_extension::<PluginParams>()
            .ok_or_else(|| "CLAP plug-in does not expose parameters".to_owned())?
    };
    let ranges = {
        let plugin = instance.plugin_handle();
        let count = params.count(&plugin);
        if count > MAX_INSTANCE_METADATA_ITEMS {
            return Err(format!(
                "CLAP plug-in exposes {count} parameters; the metadata limit is {MAX_INSTANCE_METADATA_ITEMS}"
            ));
        }
        let mut ranges = BTreeMap::new();
        let mut buffer = ParamInfoBuffer::new();
        for index in 0..count {
            let Some(info) = params.get_info(&plugin, index, &mut buffer) else {
                continue;
            };
            if !info.flags.contains(ParamInfoFlags::IS_READONLY)
                && info.min_value.is_finite()
                && info.max_value.is_finite()
                && info.min_value < info.max_value
            {
                ranges.insert(
                    info.id.get(),
                    (
                        info.min_value,
                        info.max_value,
                        info.flags.contains(ParamInfoFlags::IS_STEPPED),
                    ),
                );
            }
        }
        ranges
    };
    let mut input_events = EventBuffer::with_capacity(parameter_overrides.len());
    for (parameter_id, value) in parameter_overrides {
        let Some((minimum, maximum, stepped)) = ranges.get(parameter_id).copied() else {
            return Err(format!("CLAP parameter {parameter_id} is not writable"));
        };
        if !value.is_finite()
            || !(minimum..=maximum).contains(value)
            || (stepped && value.fract() != 0.0)
        {
            return Err(format!(
                "CLAP parameter {parameter_id} has an invalid saved value"
            ));
        }
        let parameter_id = ClapId::from_raw(*parameter_id)
            .ok_or_else(|| "CLAP parameter ID is invalid".to_owned())?;
        input_events.push(&ParamValueEvent::new(
            0,
            parameter_id,
            Pckn::match_all(),
            *value,
        ));
    }
    input_events.sort();
    let mut output_events = EventBuffer::new();
    let mut plugin = instance
        .inactive_plugin_handle()
        .ok_or_else(|| "CLAP editor plug-in unexpectedly became active".to_owned())?;
    params.flush(
        &mut plugin,
        &input_events.as_input(),
        &mut output_events.as_output(),
    );
    Ok(())
}

fn clap_editor_parent_window<'a>(
    creation: &'a eframe::CreationContext<'_>,
) -> Result<Window<'static, 'a>, String> {
    let handle = creation
        .window_handle()
        .map_err(|error| format!("could not access the CLAP editor host window: {error}"))?;
    match handle.as_raw() {
        RawWindowHandle::Wayland(handle) => Ok(Window::from_generic_ptr(
            GuiApiType::WAYLAND,
            handle.surface.as_ptr(),
        )),
        _ => Window::from_window_handle(handle)
            .ok_or_else(|| "CLAP editor host does not support this window system".to_owned()),
    }
}

struct ClapPluginEditorApp {
    session: Option<ClapPluginEditorSession>,
    result: Arc<Mutex<Option<Result<ClapPluginEditorOutput, String>>>>,
    last_reported_parameter_values: BTreeMap<u32, f64>,
    initial_window_size: egui::Vec2,
    initial_window_size_applied: bool,
    host_error: Option<String>,
    last_parameter_poll: Instant,
}

impl ClapPluginEditorApp {
    fn finish(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let result = session.close_and_capture();
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    }
}

impl eframe::App for ClapPluginEditorApp {
    fn on_exit(&mut self) {
        self.finish();
    }

    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        if !self.initial_window_size_applied {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(self.initial_window_size));
            self.initial_window_size_applied = true;
        }
        let Some(session) = self.session.as_mut() else {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        match session.service_host_requests() {
            Ok(Some(size)) => {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                        (f64::from(size.width) / session.gui_size_scale) as f32,
                        (f64::from(size.height) / session.gui_size_scale) as f32,
                    )));
            }
            Ok(None) => {}
            Err(error) => self.host_error = Some(error),
        }
        if session.signals.gui_closed.swap(false, Ordering::AcqRel) {
            session.acknowledge_gui_closed();
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if session
            .signals
            .gui_show_requested
            .swap(false, Ordering::AcqRel)
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        if session
            .signals
            .gui_hide_requested
            .swap(false, Ordering::AcqRel)
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if self.last_parameter_poll.elapsed() >= Duration::from_millis(100) {
            self.last_parameter_poll = Instant::now();
            match session.capture_parameters() {
                Ok(parameter_values) => {
                    for (parameter_id, value) in parameter_values {
                        let changed = self
                            .last_reported_parameter_values
                            .get(&parameter_id)
                            .is_none_or(|previous| previous.to_bits() != value.to_bits());
                        if changed {
                            self.last_reported_parameter_values
                                .insert(parameter_id, value);
                            eprintln!("LIVE_PARAMETER {parameter_id} {:016x}", value.to_bits());
                        }
                    }
                }
                Err(error) => self.host_error = Some(error),
            }
        }
        if session.gui_floating {
            ui.centered_and_justified(|ui| {
                ui.label("Close this helper window to save native CLAP editor changes.");
            });
        }
        if let Some(error) = &self.host_error {
            ui.label(egui::RichText::new(error).color(egui::Color32::RED));
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

impl Drop for ClapPluginEditorApp {
    fn drop(&mut self) {
        self.finish();
    }
}

fn run_plugin_editor_ui(
    path: &Path,
    plugin_id: &str,
    state: Option<&[u8]>,
    parameter_overrides: &BTreeMap<u32, f64>,
) -> Result<ClapPluginEditorOutput, String> {
    let session = ClapPluginEditorSession::new(path, plugin_id, state, parameter_overrides)?;
    let output = Arc::new(Mutex::new(None));
    let app_output = Arc::clone(&output);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{plugin_id} - CLAP editor"))
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([160.0, 80.0]),
        ..Default::default()
    };
    let run_result = eframe::run_native(
        "CLAP plug-in editor",
        options,
        Box::new(move |creation| {
            let mut session = session;
            let size = session.open_gui(creation).map_err(|error| {
                Box::new(std::io::Error::other(error)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let last_reported_parameter_values = session.parameter_values_before_editor.clone();
            Ok(Box::new(ClapPluginEditorApp {
                session: Some(session),
                result: app_output,
                last_reported_parameter_values,
                initial_window_size: size,
                initial_window_size_applied: false,
                host_error: None,
                last_parameter_poll: Instant::now(),
            }))
        }),
    );
    run_result.map_err(|error| format!("CLAP editor host window failed: {error}"))?;
    Arc::try_unwrap(output)
        .ok()
        .and_then(|output| output.into_inner().ok())
        .flatten()
        .ok_or_else(|| "CLAP editor closed without saving its state".to_owned())?
}

impl ClapNotePreviewInstance {
    fn new(path: &Path, plugin_id: &str, sample_rate: u32) -> Result<Self, String> {
        if !(8_000..=192_000).contains(&sample_rate) {
            return Err(format!(
                "unsupported CLAP preview sample rate: {sample_rate}"
            ));
        }

        let entry = unsafe { PluginEntry::load(path) }
            .map_err(|error| format!("could not load CLAP entry: {error}"))?;
        let plugin_id = CString::new(plugin_id)
            .map_err(|_| "CLAP plug-in ID contains an invalid null byte".to_owned())?;
        let host_info = HostInfo::new(
            "FL Studio Rebuild",
            "fl-studio-rust contributors",
            "https://github.com/cyroz1/fl-studio-rust",
            env!("CARGO_PKG_VERSION"),
        )
        .map_err(|error| format!("could not describe the host to the CLAP plug-in: {error}"))?;
        let mut instance =
            PluginInstance::<()>::new(|_| (), |_| (), &entry, &plugin_id, &host_info)
                .map_err(|error| format!("could not initialize CLAP plug-in instance: {error}"))?;

        let (
            input_port_channels,
            output_port_channels,
            main_output_index,
            note_port_index,
            dialect,
        ) = {
            let plugin = instance.plugin_handle();
            let audio_ports = plugin
                .get_extension::<PluginAudioPorts>()
                .ok_or_else(|| "CLAP instrument does not expose audio ports".to_owned())?;
            let note_ports = plugin
                .get_extension::<PluginNotePorts>()
                .ok_or_else(|| "CLAP instrument does not expose note input ports".to_owned())?;
            let input_port_channels =
                read_preview_audio_port_channels(&audio_ports, &plugin, true)?;
            let (output_port_channels, main_output_index) =
                read_preview_audio_outputs(&audio_ports, &plugin)?;
            let (note_port_index, dialect) = find_preview_note_input(&note_ports, &plugin)?;
            (
                input_port_channels,
                output_port_channels,
                main_output_index,
                note_port_index,
                dialect,
            )
        };

        let main_output_channels = output_port_channels[main_output_index];
        if !(1..=2).contains(&main_output_channels) {
            return Err(format!(
                "CLAP preview needs a mono or stereo main output; plug-in reports {main_output_channels} channels"
            ));
        }

        Ok(Self {
            instance,
            sample_rate,
            input_port_channels,
            output_port_channels,
            main_output_index,
            note_port_index,
            dialect,
            next_steady_time: 0,
        })
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_CLAP_PROJECT_PLUGIN_STATE_BYTES {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        let plugin = self.instance.plugin_handle();
        let state = plugin
            .get_extension::<PluginState>()
            .ok_or_else(|| "CLAP instrument does not support state restoration".to_owned())?;
        let mut input = std::io::Cursor::new(bytes);
        state
            .load(&plugin, &mut input)
            .map_err(|error| format!("CLAP instrument could not restore its state: {error}"))
    }

    fn set_parameter_values(&mut self, values: &[(u32, f64)]) -> Result<(), String> {
        if values.is_empty() || values.len() > MAX_CLAP_PREVIEW_PARAMETER_CHANGES {
            return Err("CLAP parameter update contains an unsupported number of values".into());
        }
        let mut plugin = self
            .instance
            .inactive_plugin_handle()
            .ok_or_else(|| "CLAP instrument is active during parameter update".to_owned())?;
        let params = plugin
            .get_extension::<PluginParams>()
            .ok_or_else(|| "CLAP instrument does not support parameter updates".to_owned())?;
        let mut input_buffer = EventBuffer::with_capacity(values.len() * 3);
        for &(param_id, value) in values {
            let id = ClapId::from_raw(param_id)
                .ok_or_else(|| "CLAP parameter update contains an invalid ID".to_owned())?;
            if !value.is_finite() || params.get_value(&plugin, id).is_none() {
                return Err(format!(
                    "CLAP instrument does not expose parameter {param_id}"
                ));
            }
            input_buffer.push(&ParamGestureBeginEvent::new(0, id));
            input_buffer.push(&ParamValueEvent::new(0, id, Pckn::match_all(), value));
            input_buffer.push(&ParamGestureEndEvent::new(0, id));
        }
        let input_events = input_buffer.as_input();
        let mut output_buffer = EventBuffer::new();
        params.flush(&mut plugin, &input_events, &mut output_buffer.as_output());
        for &(param_id, value) in values {
            let id = ClapId::from_raw(param_id).expect("parameter IDs were validated above");
            let current = params
                .get_value(&plugin, id)
                .ok_or_else(|| format!("CLAP parameter {param_id} became unavailable"))?;
            if !current.is_finite() {
                return Err(format!(
                    "CLAP parameter {param_id} returned a non-finite value"
                ));
            }
            if (current - value).abs() > 1e-8 * value.abs().max(1.0) {
                return Err(format!(
                    "CLAP instrument did not accept parameter {param_id} value {value}"
                ));
            }
        }
        Ok(())
    }

    fn parameter_ranges_for_automation(
        &mut self,
        events: &[ScheduledClapParameterEvent],
    ) -> Result<std::collections::BTreeMap<u32, (f64, f64, bool)>, String> {
        let mut ranges = std::collections::BTreeMap::new();
        if events.is_empty() {
            return Ok(ranges);
        }

        let requested_ids = events
            .iter()
            .map(|event| event.parameter_id)
            .collect::<std::collections::BTreeSet<_>>();
        let plugin = self.instance.plugin_handle();
        let params = plugin
            .get_extension::<PluginParams>()
            .ok_or_else(|| "CLAP instrument does not expose parameter metadata".to_owned())?;
        let parameter_count = params.count(&plugin);
        if parameter_count > MAX_INSTANCE_METADATA_ITEMS {
            return Err(format!(
                "CLAP instrument exposes {parameter_count} parameters; the metadata limit is {MAX_INSTANCE_METADATA_ITEMS}"
            ));
        }
        for index in 0..parameter_count {
            let mut buffer = ParamInfoBuffer::new();
            let Some(info) = params.get_info(&plugin, index, &mut buffer) else {
                continue;
            };
            let id = info.id.get();
            if !requested_ids.contains(&id) {
                continue;
            }
            if !info.flags.contains(ParamInfoFlags::IS_AUTOMATABLE)
                || info.flags.contains(ParamInfoFlags::IS_READONLY)
                || !info.min_value.is_finite()
                || !info.max_value.is_finite()
                || info.min_value >= info.max_value
            {
                return Err(format!(
                    "CLAP parameter {id} is not available for Pattern Event automation"
                ));
            }
            ranges.insert(
                id,
                (
                    info.min_value,
                    info.max_value,
                    info.flags.contains(ParamInfoFlags::IS_STEPPED),
                ),
            );
        }
        for id in requested_ids {
            if !ranges.contains_key(&id) {
                return Err(format!(
                    "CLAP instrument does not expose automatable parameter {id}"
                ));
            }
        }
        Ok(ranges)
    }

    fn save_state(&mut self) -> Result<Vec<u8>, String> {
        let plugin = self.instance.plugin_handle();
        let state = plugin
            .get_extension::<PluginState>()
            .ok_or_else(|| "CLAP instrument does not support state saving".to_owned())?;
        let mut writer = LimitedClapStateWriter {
            bytes: Vec::new(),
            exceeded_limit: false,
        };
        let save_result = state.save(&plugin, &mut writer);
        if writer.exceeded_limit {
            return Err("CLAP plug-in state exceeds the 32 MiB limit".to_owned());
        }
        save_result
            .map_err(|error| format!("CLAP instrument could not save its state: {error}"))?;
        Ok(writer.bytes)
    }

    fn render(
        &mut self,
        note_key: u8,
        velocity: u8,
        note_duration_milliseconds: u32,
    ) -> Result<Vec<f32>, String> {
        if note_key > 127 {
            return Err("CLAP preview MIDI note must be between 0 and 127".to_owned());
        }
        if !(1..=127).contains(&velocity) {
            return Err("CLAP preview MIDI velocity must be between 1 and 127".to_owned());
        }
        validate_note_preview_note_length(note_duration_milliseconds)?;

        let note_off_frame = self.sample_rate * note_duration_milliseconds / 1000;
        let events = [
            ScheduledClapNoteEvent {
                frame: 0,
                priority: 1,
                key: note_key,
                velocity,
                midi_channel: 0,
                note_id: 0,
                fine_pitch_cents: 0,
                pan: 64,
                release: 0,
                mod_x: 128,
                is_note_on: true,
                is_slide_note: false,
                expression: None,
            },
            ScheduledClapNoteEvent {
                frame: u64::from(note_off_frame),
                priority: 0,
                key: note_key,
                velocity: 0,
                midi_channel: 0,
                note_id: 0,
                fine_pitch_cents: 0,
                pan: 64,
                release: 0,
                mod_x: 128,
                is_note_on: false,
                is_slide_note: false,
                expression: None,
            },
        ];
        let total_frames = self.sample_rate * NOTE_PREVIEW_DURATION_MILLISECONDS / 1000;
        self.render_scheduled_events(&events, &[], total_frames, None)
    }

    fn render_pattern(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        let (events, total_frames) = schedule_pattern_preview_events(settings, self.sample_rate)?;
        let parameter_events = schedule_pattern_parameter_events(settings, self.sample_rate)?;
        self.render_scheduled_events(&events, &parameter_events, total_frames, None)
    }

    fn render_pattern_loop(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        let (one_loop_events, _) = schedule_pattern_preview_events(settings, self.sample_rate)?;
        let one_loop_parameter_events =
            schedule_pattern_parameter_events(settings, self.sample_rate)?;
        let loop_frames = pattern_preview_frame_at_tick(
            settings.loop_length_ticks,
            settings.ppq,
            settings.tempo_bpm,
            self.sample_rate,
        )?;
        if loop_frames == 0
            || one_loop_events
                .iter()
                .any(|event| event.frame > loop_frames)
        {
            return Err("CLAP Pattern notes extend beyond the playback loop".to_owned());
        }

        let mut events = Vec::with_capacity(one_loop_events.len().saturating_mul(2));
        events.extend_from_slice(&one_loop_events);
        for event in &one_loop_events {
            let mut repeated = *event;
            repeated.frame = repeated
                .frame
                .checked_add(loop_frames)
                .ok_or_else(|| "CLAP Pattern loop event position overflowed".to_owned())?;
            events.push(repeated);
        }
        events.sort_by_key(|event| (event.frame, event.priority));
        let mut parameter_events =
            Vec::with_capacity(one_loop_parameter_events.len().saturating_mul(2));
        parameter_events.extend_from_slice(&one_loop_parameter_events);
        for event in &one_loop_parameter_events {
            let mut repeated = *event;
            repeated.frame = repeated
                .frame
                .checked_add(loop_frames)
                .ok_or_else(|| "CLAP Pattern parameter loop position overflowed".to_owned())?;
            parameter_events.push(repeated);
        }
        parameter_events.sort_by_key(|event| (event.frame, event.priority, event.order));

        let tail_frames =
            u64::from(self.sample_rate) * u64::from(NOTE_PREVIEW_DURATION_MILLISECONDS) / 1000;
        let total_frames = loop_frames
            .checked_mul(2)
            .and_then(|frames| frames.checked_add(tail_frames))
            .ok_or_else(|| "CLAP Pattern loop render length overflowed".to_owned())?;
        let byte_count = total_frames
            .checked_mul(8)
            .ok_or_else(|| "CLAP Pattern loop audio size overflowed".to_owned())?;
        if byte_count == 0 || byte_count > MAX_NOTE_PREVIEW_OUTPUT_BYTES as u64 {
            return Err(format!(
                "CLAP Pattern loop exceeds the {} MiB audio limit at this sample rate",
                MAX_NOTE_PREVIEW_OUTPUT_BYTES / (1024 * 1024)
            ));
        }
        let total_frames = u32::try_from(total_frames)
            .map_err(|_| "CLAP Pattern loop render exceeds the supported range".to_owned())?;
        let rendered =
            self.render_scheduled_events(&events, &parameter_events, total_frames, None)?;
        second_pattern_loop_audio(&rendered, loop_frames)
    }

    fn render_playlist(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        let (events, total_frames, transport_points) =
            schedule_playlist_preview_events(settings, self.sample_rate)?;
        let parameter_events = schedule_playlist_parameter_events(settings, self.sample_rate)?;
        self.render_scheduled_events(
            &events,
            &parameter_events,
            total_frames,
            Some((&transport_points, settings.ppq)),
        )
    }

    fn render_scheduled_events(
        &mut self,
        events: &[ScheduledClapNoteEvent],
        parameter_events: &[ScheduledClapParameterEvent],
        total_frames: u32,
        transport: Option<(&[ClapTransportPoint], u16)>,
    ) -> Result<Vec<f32>, String> {
        if total_frames == 0 {
            return Err("CLAP preview has no audio frames to render".to_owned());
        }

        let parameter_ranges = self.parameter_ranges_for_automation(parameter_events)?;
        let audio_configuration = PluginAudioConfiguration {
            sample_rate: f64::from(self.sample_rate),
            min_frames_count: 1,
            max_frames_count: NOTE_PREVIEW_BLOCK_FRAMES,
        };
        let stopped_processor = self
            .instance
            .activate(|_, _| (), audio_configuration)
            .map_err(|error| format!("could not activate CLAP instrument: {error}"))?;
        let mut audio_processor = match stopped_processor.start_processing() {
            Ok(processor) => processor,
            Err(error) => {
                let stopped_processor = error.into_stopped_processor();
                self.instance.deactivate(stopped_processor);
                return Err("CLAP instrument could not start audio processing".to_owned());
            }
        };

        let main_output_channels = self.output_port_channels[self.main_output_index];
        let mut samples = Vec::with_capacity(total_frames as usize * 2);
        let mut input_audio_ports = AudioPorts::with_capacity(
            self.input_port_channels.iter().sum::<u32>() as usize,
            self.input_port_channels.len(),
        );
        let mut output_audio_ports = AudioPorts::with_capacity(
            self.output_port_channels.iter().sum::<u32>() as usize,
            self.output_port_channels.len(),
        );
        let render_start_time = self.next_steady_time;
        let mut frame_offset = 0;
        let mut event_index = 0usize;
        let mut parameter_event_index = 0usize;
        let mut transport_index = 0usize;
        let mut render_error = None;

        while frame_offset < total_frames {
            let mut frame_count = (total_frames - frame_offset).min(NOTE_PREVIEW_BLOCK_FRAMES);
            if let Some((points, _)) = transport {
                while transport_index + 1 < points.len()
                    && points[transport_index + 1].frame <= u64::from(frame_offset)
                {
                    transport_index += 1;
                }
                if let Some(next_point) = points.get(transport_index + 1) {
                    let frames_to_change = next_point.frame.saturating_sub(u64::from(frame_offset));
                    if frames_to_change > 0 {
                        frame_count = u32::try_from(frames_to_change.min(u64::from(frame_count)))
                            .expect("the block limit fits in u32");
                    }
                }
            }
            let frame_end = frame_offset + frame_count;
            let block_event_start = event_index;
            while event_index < events.len() && events[event_index].frame < u64::from(frame_end) {
                event_index += 1;
            }
            let event_count = event_index.saturating_sub(block_event_start);
            let block_parameter_start = parameter_event_index;
            while parameter_event_index < parameter_events.len()
                && parameter_events[parameter_event_index].frame < u64::from(frame_end)
            {
                parameter_event_index += 1;
            }
            let parameter_event_count = parameter_event_index.saturating_sub(block_parameter_start);
            let mut scheduled_block_events =
                Vec::with_capacity(event_count.saturating_add(parameter_event_count));
            for (event_index, event) in events
                .iter()
                .enumerate()
                .take(event_index)
                .skip(block_event_start)
            {
                scheduled_block_events.push((
                    event.frame,
                    if event.expression.is_some() {
                        3
                    } else if event.is_note_on {
                        2
                    } else {
                        0
                    },
                    event_index,
                    false,
                ));
            }
            for (parameter_index, event) in parameter_events
                .iter()
                .enumerate()
                .take(parameter_event_index)
                .skip(block_parameter_start)
            {
                scheduled_block_events.push((event.frame, event.priority, parameter_index, true));
            }
            scheduled_block_events.sort_by_key(|event| (event.0, event.1, event.2));
            let mut input_event_buffer = EventBuffer::with_capacity(
                event_count
                    .saturating_mul(4)
                    .saturating_add(parameter_event_count),
            );
            for (_, _, event_index, is_parameter) in scheduled_block_events {
                if is_parameter {
                    let event = parameter_events[event_index];
                    let event_time =
                        u32::try_from(event.frame.saturating_sub(u64::from(frame_offset)))
                            .map_err(|_| {
                                "CLAP parameter event offset exceeds the audio block".to_owned()
                            })?;
                    let id = ClapId::from_raw(event.parameter_id)
                        .expect("parameter IDs were validated before processing");
                    let (minimum, maximum, stepped) = parameter_ranges[&event.parameter_id];
                    let mut value =
                        minimum * (1.0 - event.normalized_value) + maximum * event.normalized_value;
                    if !value.is_finite() {
                        return Err(format!(
                            "CLAP parameter {} could not map its normalized value",
                            event.parameter_id
                        ));
                    }
                    value = value.clamp(minimum, maximum);
                    if stepped {
                        value = value.trunc();
                    }
                    input_event_buffer.push(&ParamValueEvent::new(
                        event_time,
                        id,
                        Pckn::match_all(),
                        value,
                    ));
                    continue;
                }
                let event = &events[event_index];
                let event_time = u32::try_from(event.frame.saturating_sub(u64::from(frame_offset)))
                    .map_err(|_| "CLAP note event offset exceeds the audio block".to_owned())?;
                let pckn = clack_host::prelude::Pckn::new(
                    self.note_port_index,
                    u16::from(event.midi_channel),
                    u16::from(event.key),
                    event.note_id,
                );
                if let Some(expression) = event.expression {
                    if self.dialect == PreviewNoteDialect::Clap {
                        let (kind, value) = match expression {
                            ScheduledClapExpression::TuningCents(cents) => {
                                (NoteExpressionType::Tuning, f64::from(cents) / 100.0)
                            }
                            ScheduledClapExpression::PanRaw(value) => {
                                (NoteExpressionType::Pan, f64::from(value) / 128.0)
                            }
                            ScheduledClapExpression::VolumeMillionths(value) => {
                                (NoteExpressionType::Volume, f64::from(value) / 1_000_000.0)
                            }
                        };
                        input_event_buffer
                            .push(&NoteExpressionEvent::new(event_time, pckn, kind, value));
                    }
                    continue;
                }
                if event.is_slide_note && self.dialect == PreviewNoteDialect::Clap {
                    continue;
                }
                match (self.dialect, event.is_note_on) {
                    (PreviewNoteDialect::Clap, true) => {
                        input_event_buffer.push(&NoteOnEvent::new(
                            event_time,
                            pckn,
                            f64::from(event.velocity) / 127.0,
                        ));
                        input_event_buffer.push(&NoteExpressionEvent::new(
                            event_time,
                            pckn,
                            NoteExpressionType::Tuning,
                            f64::from(event.fine_pitch_cents) / 100.0,
                        ));
                        input_event_buffer.push(&NoteExpressionEvent::new(
                            event_time,
                            pckn,
                            NoteExpressionType::Pan,
                            f64::from(event.pan) / 128.0,
                        ));
                        input_event_buffer.push(&NoteExpressionEvent::new(
                            event_time,
                            pckn,
                            NoteExpressionType::Brightness,
                            f64::from(event.mod_x) / 255.0,
                        ));
                    }
                    (PreviewNoteDialect::Clap, false) => {
                        input_event_buffer.push(&NoteOffEvent::new(
                            event_time,
                            pckn,
                            f64::from(event.release) / 128.0,
                        ));
                    }
                    (PreviewNoteDialect::Midi, true) => {
                        input_event_buffer.push(&MidiEvent::new(
                            event_time,
                            self.note_port_index,
                            [0x90 | event.midi_channel, event.key, event.velocity],
                        ));
                    }
                    (PreviewNoteDialect::Midi, false) => {
                        input_event_buffer.push(&MidiEvent::new(
                            event_time,
                            self.note_port_index,
                            [
                                0x80 | event.midi_channel,
                                event.key,
                                midi_preview_release_velocity(event.release),
                            ],
                        ));
                    }
                    (PreviewNoteDialect::Midi2, true) => {
                        input_event_buffer.push(&midi2_preview_note_event(
                            event_time,
                            self.note_port_index,
                            event.midi_channel,
                            event.key,
                            midi2_preview_velocity(event.velocity),
                            true,
                        ));
                    }
                    (PreviewNoteDialect::Midi2, false) => {
                        input_event_buffer.push(&midi2_preview_note_event(
                            event_time,
                            self.note_port_index,
                            event.midi_channel,
                            event.key,
                            midi2_preview_velocity(midi_preview_release_velocity(event.release)),
                            false,
                        ));
                    }
                }
            }
            input_event_buffer.sort();
            let mut output_event_buffer = EventBuffer::with_capacity(16);
            let mut input_audio_storage = self
                .input_port_channels
                .iter()
                .map(|channel_count| {
                    (0..*channel_count)
                        .map(|_| vec![0.0f32; frame_count as usize])
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let mut output_audio_storage = self
                .output_port_channels
                .iter()
                .map(|channel_count| {
                    (0..*channel_count)
                        .map(|_| vec![0.0f32; frame_count as usize])
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            let steady_time = render_start_time.saturating_add(u64::from(frame_offset));
            let transport_event = transport.map(|(points, ppq)| {
                let point = points[transport_index];
                let elapsed_frames = u64::from(frame_offset).saturating_sub(point.frame);
                let current_tick = point.tick as f64
                    + elapsed_frames as f64 * point.tempo_bpm * f64::from(ppq)
                        / (60.0 * f64::from(self.sample_rate));
                TransportEvent {
                    header: EventHeader::<TransportEvent>::new_core(0, EventFlags::empty()),
                    flags: TransportFlags::HAS_TEMPO
                        | TransportFlags::HAS_BEATS_TIMELINE
                        | TransportFlags::HAS_SECONDS_TIMELINE
                        | TransportFlags::IS_PLAYING,
                    song_pos_beats: BeatTime::from_float(current_tick / f64::from(ppq)),
                    song_pos_seconds: SecondsTime::from_float(
                        f64::from(frame_offset) / f64::from(self.sample_rate),
                    ),
                    tempo: point.tempo_bpm,
                    tempo_inc: 0.0,
                    loop_start_beats: BeatTime::default(),
                    loop_end_beats: BeatTime::default(),
                    loop_start_seconds: SecondsTime::default(),
                    loop_end_seconds: SecondsTime::default(),
                    bar_start: BeatTime::default(),
                    bar_number: 0,
                    time_signature_numerator: 0,
                    time_signature_denominator: 0,
                }
            });
            let process_result: Result<(), String> = {
                let input_audio = if input_audio_storage.is_empty() {
                    InputAudioBuffers::empty()
                } else {
                    input_audio_ports.with_input_buffers(input_audio_storage.iter_mut().map(
                        |channels| AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_input_only(
                                channels.iter_mut().map(InputChannel::constant),
                            ),
                        },
                    ))
                };
                let mut output_audio =
                    output_audio_ports.with_output_buffers(output_audio_storage.iter_mut().map(
                        |channels| AudioPortBuffer {
                            latency: 0,
                            channels: AudioPortBufferType::f32_output_only(
                                channels.iter_mut().map(Vec::as_mut_slice),
                            ),
                        },
                    ));
                let input_events = input_event_buffer.as_input();
                let mut output_events = output_event_buffer.as_output();
                audio_processor
                    .process(
                        &input_audio,
                        &mut output_audio,
                        &input_events,
                        &mut output_events,
                        Some(steady_time),
                        transport_event.as_ref(),
                    )
                    .map(|_: ProcessStatus| ())
                    .map_err(|error| {
                        format!("CLAP instrument failed while rendering audio: {error}")
                    })
            };

            if let Err(error) = process_result {
                render_error = Some(error);
                break;
            }

            let main_output = &output_audio_storage[self.main_output_index];
            let left_channel = &main_output[0];
            let right_channel = if main_output_channels == 1 {
                left_channel
            } else {
                &main_output[1]
            };
            for (&left, &right) in left_channel
                .iter()
                .zip(right_channel.iter())
                .take(frame_count as usize)
            {
                samples.push(if left.is_finite() { left } else { 0.0 });
                samples.push(if right.is_finite() { right } else { 0.0 });
            }
            frame_offset = frame_end;
        }

        let stopped_processor = audio_processor.stop_processing();
        self.instance.deactivate(stopped_processor);
        if let Some(error) = render_error {
            return Err(error);
        }
        self.next_steady_time = render_start_time.saturating_add(u64::from(total_frames));
        Ok(samples)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScheduledClapExpression {
    TuningCents(i32),
    PanRaw(u8),
    VolumeMillionths(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScheduledClapNoteEvent {
    frame: u64,
    priority: u8,
    key: u8,
    velocity: u8,
    midi_channel: u8,
    note_id: u32,
    fine_pitch_cents: i16,
    pan: u8,
    release: u8,
    mod_x: u8,
    is_note_on: bool,
    is_slide_note: bool,
    expression: Option<ScheduledClapExpression>,
}

#[derive(Clone, Copy)]
struct ClapScheduledNoteSpan {
    start_frame: u64,
    stop_frame: u64,
    note_on: ScheduledClapNoteEvent,
}

#[derive(Clone, Copy)]
struct ClapSlideCurve {
    note_on: ScheduledClapNoteEvent,
    note_stop_frame: u64,
    start_frame: u64,
    end_frame: u64,
    start_tuning_cents: i32,
    end_tuning_cents: i32,
    start_pan_raw: u8,
    end_pan_raw: u8,
    start_gain: f64,
    end_gain: f64,
}

fn clap_slide_curve_values(curve: ClapSlideCurve, frame: u64) -> (i32, u8, f64) {
    if frame >= curve.end_frame {
        return (curve.end_tuning_cents, curve.end_pan_raw, curve.end_gain);
    }
    let duration = curve.end_frame.saturating_sub(curve.start_frame).max(1);
    let progress = frame.saturating_sub(curve.start_frame).min(duration) as f64 / duration as f64;
    let tuning_delta = curve.end_tuning_cents - curve.start_tuning_cents;
    let pan_delta = i32::from(curve.end_pan_raw) - i32::from(curve.start_pan_raw);
    (
        curve.start_tuning_cents + (f64::from(tuning_delta) * progress).round() as i32,
        (i32::from(curve.start_pan_raw) + (f64::from(pan_delta) * progress).round() as i32)
            .clamp(0, 128) as u8,
        curve.start_gain + (curve.end_gain - curve.start_gain) * progress,
    )
}

fn clap_note_slide_values(
    curves: Option<&[ClapSlideCurve]>,
    note: ClapScheduledNoteSpan,
    frame: u64,
) -> (i32, u8, f64) {
    curves
        .and_then(|curves| {
            curves
                .iter()
                .rev()
                .find(|curve| curve.start_frame <= frame)
                .copied()
        })
        .map(|curve| clap_slide_curve_values(curve, frame))
        .unwrap_or((
            i32::from(note.note_on.fine_pitch_cents),
            note.note_on.pan,
            f64::from(note.note_on.velocity) / 127.0,
        ))
}

fn clap_note_expression_event(
    mut note: ScheduledClapNoteEvent,
    frame: u64,
    expression: ScheduledClapExpression,
) -> ScheduledClapNoteEvent {
    note.frame = frame;
    note.priority = 2;
    note.velocity = 0;
    note.is_note_on = false;
    note.is_slide_note = false;
    note.expression = Some(expression);
    note
}

fn clap_expression_sort_order(expression: Option<ScheduledClapExpression>) -> u8 {
    match expression {
        Some(ScheduledClapExpression::TuningCents(_)) => 0,
        Some(ScheduledClapExpression::PanRaw(_)) => 1,
        Some(ScheduledClapExpression::VolumeMillionths(_)) => 2,
        None => 3,
    }
}

fn append_clap_slide_expressions(
    events: &mut Vec<ScheduledClapNoteEvent>,
    note_spans: &[ClapScheduledNoteSpan],
    sample_rate: u32,
) -> Result<u64, String> {
    let mut slides = note_spans
        .iter()
        .copied()
        .filter(|span| span.note_on.is_slide_note)
        .collect::<Vec<_>>();
    slides.sort_by_key(|span| (span.start_frame, span.note_on.note_id));
    let mut curves_by_note = HashMap::<u32, Vec<ClapSlideCurve>>::new();

    for slide in slides {
        let active_notes = note_spans
            .iter()
            .copied()
            .filter(|span| {
                !span.note_on.is_slide_note
                    && span.note_on.midi_channel == slide.note_on.midi_channel
                    && span.start_frame <= slide.start_frame
                    && span.stop_frame > slide.start_frame
            })
            .map(|span| {
                let values = clap_note_slide_values(
                    curves_by_note.get(&span.note_on.note_id).map(Vec::as_slice),
                    span,
                    slide.start_frame,
                );
                (span, values)
            })
            .collect::<Vec<_>>();
        let Some(reference_pitch_cents) = active_notes
            .iter()
            .map(|(span, (tuning_cents, _, _))| i32::from(span.note_on.key) * 100 + *tuning_cents)
            .max()
        else {
            continue;
        };

        let target_pitch_cents =
            i32::from(slide.note_on.key) * 100 + i32::from(slide.note_on.fine_pitch_cents);
        let pitch_delta_cents = target_pitch_cents - reference_pitch_cents;
        let target_pan_raw = slide.note_on.pan;
        let target_gain = f64::from(slide.note_on.velocity) / 127.0;
        for (note_span, (start_tuning_cents, start_pan_raw, start_gain)) in active_notes {
            let end_frame = slide.stop_frame.min(note_span.stop_frame);
            if end_frame <= slide.start_frame {
                continue;
            }
            let end_tuning_cents = start_tuning_cents + pitch_delta_cents;
            if !(-12_000..=12_000).contains(&start_tuning_cents)
                || !(-12_000..=12_000).contains(&end_tuning_cents)
            {
                return Err(
                    "CLAP note slide exceeds the per-note tuning range of −120…120 semitones"
                        .into(),
                );
            }

            let curves = curves_by_note.entry(note_span.note_on.note_id).or_default();
            let mut retained = Vec::with_capacity(curves.len().saturating_add(1));
            for mut curve in curves.drain(..) {
                if curve.start_frame >= slide.start_frame {
                    continue;
                }
                if curve.end_frame > slide.start_frame {
                    curve.end_frame = slide.start_frame;
                    curve.end_tuning_cents = start_tuning_cents;
                    curve.end_pan_raw = start_pan_raw;
                    curve.end_gain = start_gain;
                }
                retained.push(curve);
            }
            retained.push(ClapSlideCurve {
                note_on: note_span.note_on,
                note_stop_frame: note_span.stop_frame,
                start_frame: slide.start_frame,
                end_frame,
                start_tuning_cents,
                end_tuning_cents,
                start_pan_raw,
                end_pan_raw: target_pan_raw,
                start_gain,
                end_gain: target_gain,
            });
            *curves = retained;
        }
    }

    let update_interval = (u64::from(sample_rate) / 200).max(1);
    let mut expression_count = 0usize;
    let mut last_expression_frame = 0u64;
    for curves in curves_by_note.values() {
        for curve in curves {
            let base_velocity = f64::from(curve.note_on.velocity) / 127.0;
            if base_velocity <= 0.0 {
                continue;
            }
            let mut frame = curve.start_frame;
            loop {
                if frame >= curve.note_stop_frame {
                    break;
                }
                let (tuning_cents, pan_raw, gain) = clap_slide_curve_values(*curve, frame);
                let volume_ratio = (gain / base_velocity).clamp(0.000_001, 4.0);
                let volume_millionths = (volume_ratio * 1_000_000.0).round() as u32;
                events.push(clap_note_expression_event(
                    curve.note_on,
                    frame,
                    ScheduledClapExpression::TuningCents(tuning_cents),
                ));
                events.push(clap_note_expression_event(
                    curve.note_on,
                    frame,
                    ScheduledClapExpression::PanRaw(pan_raw),
                ));
                events.push(clap_note_expression_event(
                    curve.note_on,
                    frame,
                    ScheduledClapExpression::VolumeMillionths(volume_millionths),
                ));
                expression_count = expression_count.saturating_add(3);
                if expression_count > MAX_CLAP_SLIDE_EXPRESSION_EVENTS {
                    return Err(format!(
                        "CLAP note slides exceed {MAX_CLAP_SLIDE_EXPRESSION_EVENTS} scheduled expression events"
                    ));
                }
                last_expression_frame = last_expression_frame.max(frame);
                if frame >= curve.end_frame {
                    break;
                }
                frame = frame.saturating_add(update_interval).min(curve.end_frame);
            }
        }
    }
    Ok(last_expression_frame)
}

#[derive(Clone, Copy, Debug)]
struct ScheduledClapParameterEvent {
    frame: u64,
    priority: u8,
    order: usize,
    parameter_id: u32,
    normalized_value: f64,
}

#[derive(Clone, Copy, Debug)]
struct ClapTransportPoint {
    frame: u64,
    tick: u64,
    tempo_bpm: f64,
}

fn validate_pattern_preview_settings(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<(), String> {
    schedule_pattern_preview_events(settings, sample_rate)?;
    schedule_pattern_parameter_events(settings, sample_rate).map(|_| ())
}

fn validate_playlist_preview_settings(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<(), String> {
    schedule_playlist_preview_events(settings, sample_rate)?;
    schedule_playlist_parameter_events(settings, sample_rate).map(|_| ())
}

fn schedule_pattern_parameter_events(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<Vec<ScheduledClapParameterEvent>, String> {
    let lanes = settings
        .parameter_automation
        .iter()
        .filter_map(|lane| {
            let points = lane
                .points
                .iter()
                .filter(|point| point.tick <= settings.loop_length_ticks)
                .cloned()
                .collect::<Vec<_>>();
            (!points.is_empty()).then_some(ClapParameterAutomation {
                parameter_id: lane.parameter_id,
                points,
            })
        })
        .collect::<Vec<_>>();
    schedule_parameter_events(&lanes, |tick| {
        pattern_preview_frame_at_tick(tick, settings.ppq, settings.tempo_bpm, sample_rate)
    })
}

fn schedule_playlist_parameter_events(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<Vec<ScheduledClapParameterEvent>, String> {
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "unsupported CLAP preview sample rate: {sample_rate}"
        ));
    }
    let playlist = settings
        .playlist
        .as_ref()
        .ok_or_else(|| "CLAP Playlist playback is missing its schedule".to_owned())?;
    let mut tempo_events = Vec::with_capacity(playlist.tempo_points.len());
    for point in &playlist.tempo_points {
        tempo_events.push((point.tick, point.tempo_bpm));
    }
    let tempo_map =
        PlaylistTempoMap::from_tempo_events(settings.ppq, settings.tempo_bpm, &tempo_events)?;
    schedule_parameter_events(&playlist.parameter_automation, |tick| {
        tempo_map.frame_at_tick(tick, sample_rate)
    })
}

fn schedule_parameter_events(
    lanes: &[ClapParameterAutomation],
    mut frame_at_tick: impl FnMut(u64) -> Result<u64, String>,
) -> Result<Vec<ScheduledClapParameterEvent>, String> {
    let mut point_count = 0usize;
    let mut seen_ids = std::collections::BTreeSet::new();
    let mut events = Vec::new();
    for lane in lanes {
        if ClapId::from_raw(lane.parameter_id).is_none() || !seen_ids.insert(lane.parameter_id) {
            return Err(
                "CLAP parameter automation contains an invalid or duplicate parameter ID".into(),
            );
        }
        point_count = point_count.saturating_add(lane.points.len());
        if point_count > MAX_PATTERN_PREVIEW_NOTES {
            return Err(format!(
                "CLAP parameter automation exceeds {MAX_PATTERN_PREVIEW_NOTES} points"
            ));
        }
        for point in &lane.points {
            if !point.normalized_value.is_finite() || !(0.0..=1.0).contains(&point.normalized_value)
            {
                return Err("CLAP parameter automation values must be normalized to 0–1".into());
            }
            events.push(ScheduledClapParameterEvent {
                frame: frame_at_tick(point.tick)?,
                priority: 1,
                order: events.len(),
                parameter_id: lane.parameter_id,
                normalized_value: point.normalized_value,
            });
        }
    }
    events.sort_by_key(|event| (event.frame, event.priority, event.order));
    Ok(events)
}

fn schedule_playlist_preview_events(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<(Vec<ScheduledClapNoteEvent>, u32, Vec<ClapTransportPoint>), String> {
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "unsupported CLAP preview sample rate: {sample_rate}"
        ));
    }
    if settings.ppq == 0 {
        return Err("CLAP Playlist playback requires a positive project PPQ".to_owned());
    }
    if !settings.tempo_bpm.is_finite() || !(1.0..=1000.0).contains(&settings.tempo_bpm) {
        return Err("CLAP Playlist tempo must be between 1 and 1000 BPM".to_owned());
    }
    let playlist = settings
        .playlist
        .as_ref()
        .ok_or_else(|| "CLAP Playlist playback is missing its schedule".to_owned())?;
    if playlist.notes.is_empty() {
        return Err("CLAP Playlist schedule has no notes".to_owned());
    }
    if playlist.notes.len() > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "CLAP Playlist schedule exceeds {MAX_PATTERN_PREVIEW_NOTES} notes"
        ));
    }
    if playlist.tempo_points.len() > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "CLAP Playlist tempo map exceeds {MAX_PATTERN_PREVIEW_NOTES} changes"
        ));
    }
    let mut tempo_events = Vec::with_capacity(playlist.tempo_points.len());
    for point in &playlist.tempo_points {
        if !point.tempo_bpm.is_finite() || !(1.0..=1000.0).contains(&point.tempo_bpm) {
            return Err("CLAP Playlist tempo must be between 1 and 1000 BPM".to_owned());
        }
        tempo_events.push((point.tick, point.tempo_bpm));
    }
    let tempo_map =
        PlaylistTempoMap::from_tempo_events(settings.ppq, settings.tempo_bpm, &tempo_events)?;
    let mut events = Vec::with_capacity(playlist.notes.len().saturating_mul(2));
    let mut note_spans = Vec::with_capacity(playlist.notes.len());
    let mut last_event_frame = 0u64;
    for (index, note) in playlist.notes.iter().enumerate() {
        if note.stop_tick <= note.start_tick {
            return Err("CLAP Playlist schedule contains a zero-length note".to_owned());
        }
        if note.key > 127 {
            return Err(format!(
                "CLAP Playlist key {} is outside the MIDI range",
                note.key
            ));
        }
        if note.velocity > 127 || (!note.slide_note && note.velocity == 0) {
            return Err(format!(
                "CLAP Playlist velocity {} is outside 1–127",
                note.velocity
            ));
        }
        if !(-1200..=1200).contains(&note.fine_pitch_cents) {
            return Err(format!(
                "CLAP Playlist fine pitch {} is outside −1200…1200 cents",
                note.fine_pitch_cents
            ));
        }
        if note.pan > 128 {
            return Err(format!("CLAP Playlist pan {} is outside 0–128", note.pan));
        }
        if note.release > 128 {
            return Err(format!(
                "CLAP Playlist release {} is outside 0–128",
                note.release
            ));
        }
        if note.midi_channel > 15 {
            return Err(format!(
                "CLAP Playlist MIDI channel {} is outside 0–15",
                note.midi_channel
            ));
        }
        let start_frame = tempo_map.frame_at_tick(note.start_tick, sample_rate)?;
        let stop_frame = tempo_map
            .frame_at_tick(note.stop_tick, sample_rate)?
            .max(start_frame.saturating_add(1));
        let note_id =
            u32::try_from(index + 1).map_err(|_| "CLAP Playlist note ID overflowed".to_owned())?;
        let note_on = ScheduledClapNoteEvent {
            frame: start_frame,
            priority: 1,
            key: note.key as u8,
            velocity: note.velocity,
            midi_channel: note.midi_channel,
            note_id,
            fine_pitch_cents: note.fine_pitch_cents,
            pan: note.pan,
            release: note.release,
            mod_x: note.mod_x,
            is_note_on: true,
            is_slide_note: note.slide_note,
            expression: None,
        };
        events.push(note_on);
        events.push(ScheduledClapNoteEvent {
            frame: stop_frame,
            priority: 0,
            key: note.key as u8,
            velocity: 0,
            midi_channel: note.midi_channel,
            note_id,
            fine_pitch_cents: note.fine_pitch_cents,
            pan: note.pan,
            release: note.release,
            mod_x: note.mod_x,
            is_note_on: false,
            is_slide_note: note.slide_note,
            expression: None,
        });
        note_spans.push(ClapScheduledNoteSpan {
            start_frame,
            stop_frame,
            note_on,
        });
        last_event_frame = last_event_frame.max(stop_frame);
    }
    last_event_frame = last_event_frame.max(append_clap_slide_expressions(
        &mut events,
        &note_spans,
        sample_rate,
    )?);
    events.sort_by_key(|event| {
        (
            event.frame,
            event.priority,
            event.note_id,
            clap_expression_sort_order(event.expression),
        )
    });
    if let Some(last_parameter_event) =
        schedule_parameter_events(&playlist.parameter_automation, |tick| {
            tempo_map.frame_at_tick(tick, sample_rate)
        })?
        .last()
    {
        last_event_frame = last_event_frame.max(last_parameter_event.frame);
    }

    let mut transport_points = Vec::<ClapTransportPoint>::new();
    for (tick, tempo_bpm) in tempo_map.tempo_segments() {
        let point = ClapTransportPoint {
            frame: tempo_map.frame_at_tick(tick, sample_rate)?,
            tick,
            tempo_bpm,
        };
        if let Some(previous) = transport_points.last_mut()
            && previous.frame == point.frame
        {
            *previous = point;
        } else {
            transport_points.push(point);
        }
    }
    let release_tail_frames =
        u64::from(sample_rate) * u64::from(NOTE_PREVIEW_DURATION_MILLISECONDS) / 1000;
    let total_frames = last_event_frame
        .checked_add(release_tail_frames)
        .ok_or_else(|| "CLAP Playlist render length overflowed".to_owned())?;
    let byte_count = total_frames
        .checked_mul(8)
        .ok_or_else(|| "CLAP Playlist audio size overflowed".to_owned())?;
    if byte_count == 0 || byte_count > MAX_NOTE_PREVIEW_OUTPUT_BYTES as u64 {
        return Err(format!(
            "CLAP Playlist render exceeds the {} MiB audio limit at this sample rate",
            MAX_NOTE_PREVIEW_OUTPUT_BYTES / (1024 * 1024)
        ));
    }
    let total_frames = u32::try_from(total_frames)
        .map_err(|_| "CLAP Playlist render exceeds the supported range".to_owned())?;
    Ok((events, total_frames, transport_points))
}

fn pattern_preview_frame_at_tick(
    tick: u64,
    ppq: u16,
    tempo_bpm: f64,
    sample_rate: u32,
) -> Result<u64, String> {
    let frames = (tick as f64 * 60.0 * f64::from(sample_rate)) / (f64::from(ppq) * tempo_bpm);
    if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
        Err("CLAP Pattern note timing is outside the renderable range".to_owned())
    } else {
        Ok(frames.round() as u64)
    }
}

fn second_pattern_loop_audio(rendered: &[f32], loop_frames: u64) -> Result<Vec<f32>, String> {
    let loop_samples = loop_frames
        .checked_mul(2)
        .and_then(|samples| usize::try_from(samples).ok())
        .ok_or_else(|| "CLAP Pattern loop sample count overflowed".to_owned())?;
    let loop_end = loop_samples
        .checked_mul(2)
        .ok_or_else(|| "CLAP Pattern loop sample range overflowed".to_owned())?;
    rendered
        .get(loop_samples..loop_end)
        .map(<[f32]>::to_vec)
        .ok_or_else(|| "CLAP Pattern helper rendered an incomplete loop".to_owned())
}

fn schedule_pattern_preview_events(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<(Vec<ScheduledClapNoteEvent>, u32), String> {
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "unsupported CLAP preview sample rate: {sample_rate}"
        ));
    }
    if settings.ppq == 0 {
        return Err("CLAP Pattern preview requires a positive project PPQ".to_owned());
    }
    if !settings.tempo_bpm.is_finite() || !(1.0..=1000.0).contains(&settings.tempo_bpm) {
        return Err("CLAP Pattern preview tempo must be between 1 and 1000 BPM".to_owned());
    }
    if settings.loop_length_ticks == 0 {
        return Err("CLAP Pattern preview loop must have a positive length".to_owned());
    }
    if settings.notes.is_empty() {
        return Err("selected Pattern channel has no notes to preview".to_owned());
    }
    if settings.notes.len() > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "CLAP Pattern preview exceeds {MAX_PATTERN_PREVIEW_NOTES} notes"
        ));
    }

    let frame_at_tick = |tick: u64| {
        pattern_preview_frame_at_tick(tick, settings.ppq, settings.tempo_bpm, sample_rate)
    };
    let mut events = Vec::with_capacity(settings.notes.len().saturating_mul(2));
    let mut note_spans = Vec::with_capacity(settings.notes.len());
    let mut last_event_frame = frame_at_tick(settings.loop_length_ticks)?;
    for (index, note) in settings.notes.iter().enumerate() {
        if note.length_ticks == 0 {
            return Err("CLAP Pattern preview contains a zero-length note".to_owned());
        }
        if note.key > 127 {
            return Err(format!(
                "CLAP Pattern preview key {} is outside the MIDI range",
                note.key
            ));
        }
        if note.velocity > 127 || (!note.slide_note && note.velocity == 0) {
            return Err(format!(
                "CLAP Pattern preview velocity {} is outside 1–127",
                note.velocity
            ));
        }
        if !(-1200..=1200).contains(&note.fine_pitch_cents) {
            return Err(format!(
                "CLAP Pattern preview fine pitch {} is outside −1200…1200 cents",
                note.fine_pitch_cents
            ));
        }
        if note.pan > 128 {
            return Err(format!(
                "CLAP Pattern preview pan {} is outside 0–128",
                note.pan
            ));
        }
        if note.release > 128 {
            return Err(format!(
                "CLAP Pattern preview release {} is outside 0–128",
                note.release
            ));
        }
        if note.midi_channel > 15 {
            return Err(format!(
                "CLAP Pattern preview MIDI channel {} is outside 0–15",
                note.midi_channel
            ));
        }
        let end_tick = u64::from(note.position_ticks)
            .checked_add(u64::from(note.length_ticks))
            .ok_or_else(|| "CLAP Pattern note end position overflowed".to_owned())?;
        let start_frame = frame_at_tick(u64::from(note.position_ticks))?;
        let end_frame = frame_at_tick(end_tick)?.max(start_frame.saturating_add(1));
        let note_id = u32::try_from(index + 1)
            .map_err(|_| "CLAP Pattern preview note ID overflowed".to_owned())?;
        let note_on = ScheduledClapNoteEvent {
            frame: start_frame,
            priority: 1,
            key: note.key as u8,
            velocity: note.velocity,
            midi_channel: note.midi_channel,
            note_id,
            fine_pitch_cents: note.fine_pitch_cents,
            pan: note.pan,
            release: note.release,
            mod_x: note.mod_x,
            is_note_on: true,
            is_slide_note: note.slide_note,
            expression: None,
        };
        events.push(note_on);
        events.push(ScheduledClapNoteEvent {
            frame: end_frame,
            priority: 0,
            key: note.key as u8,
            velocity: 0,
            midi_channel: note.midi_channel,
            note_id,
            fine_pitch_cents: note.fine_pitch_cents,
            pan: note.pan,
            release: note.release,
            mod_x: note.mod_x,
            is_note_on: false,
            is_slide_note: note.slide_note,
            expression: None,
        });
        note_spans.push(ClapScheduledNoteSpan {
            start_frame,
            stop_frame: end_frame,
            note_on,
        });
        last_event_frame = last_event_frame.max(end_frame);
    }
    last_event_frame = last_event_frame.max(append_clap_slide_expressions(
        &mut events,
        &note_spans,
        sample_rate,
    )?);
    events.sort_by_key(|event| {
        (
            event.frame,
            event.priority,
            event.note_id,
            clap_expression_sort_order(event.expression),
        )
    });

    let release_tail_frames =
        u64::from(sample_rate) * u64::from(NOTE_PREVIEW_DURATION_MILLISECONDS) / 1000;
    let total_frames = last_event_frame
        .checked_add(release_tail_frames)
        .ok_or_else(|| "CLAP Pattern preview duration overflowed".to_owned())?;
    let byte_count = total_frames
        .checked_mul(8)
        .ok_or_else(|| "CLAP Pattern preview audio size overflowed".to_owned())?;
    if byte_count == 0 || byte_count > MAX_NOTE_PREVIEW_OUTPUT_BYTES as u64 {
        return Err(format!(
            "CLAP Pattern preview exceeds the {} MiB audio limit at this sample rate",
            MAX_NOTE_PREVIEW_OUTPUT_BYTES / (1024 * 1024)
        ));
    }
    let total_frames = u32::try_from(total_frames)
        .map_err(|_| "CLAP Pattern preview duration exceeds the supported range".to_owned())?;
    Ok((events, total_frames))
}

fn validate_note_preview_note_length(note_duration_milliseconds: u32) -> Result<(), String> {
    if (MIN_NOTE_PREVIEW_NOTE_MILLISECONDS..=MAX_NOTE_PREVIEW_NOTE_MILLISECONDS)
        .contains(&note_duration_milliseconds)
    {
        Ok(())
    } else {
        Err(format!(
            "CLAP preview note length must be between {MIN_NOTE_PREVIEW_NOTE_MILLISECONDS} and {MAX_NOTE_PREVIEW_NOTE_MILLISECONDS} milliseconds"
        ))
    }
}

fn read_preview_audio_port_channels(
    ports: &PluginAudioPorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
    is_input: bool,
) -> Result<Vec<u32>, String> {
    let count = ports.count(plugin, is_input);
    if count > MAX_NOTE_PREVIEW_AUDIO_PORTS {
        return Err("CLAP instrument has too many audio ports for preview".to_owned());
    }
    let mut total_channels = 0u32;
    let mut channels = Vec::with_capacity(count as usize);
    let mut buffer = AudioPortInfoBuffer::new();
    for index in 0..count {
        let port = ports
            .get(plugin, index, is_input, &mut buffer)
            .ok_or_else(|| {
                "CLAP instrument returned incomplete audio port information".to_owned()
            })?;
        if port.channel_count == 0 {
            return Err("CLAP instrument reports an audio port with no channels".to_owned());
        }
        total_channels = total_channels
            .checked_add(port.channel_count)
            .ok_or_else(|| "CLAP instrument audio channel count overflowed".to_owned())?;
        if total_channels > MAX_NOTE_PREVIEW_TOTAL_CHANNELS {
            return Err("CLAP instrument has too many audio channels for preview".to_owned());
        }
        channels.push(port.channel_count);
    }
    Ok(channels)
}

fn read_preview_audio_outputs(
    ports: &PluginAudioPorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
) -> Result<(Vec<u32>, usize), String> {
    let channels = read_preview_audio_port_channels(ports, plugin, false)?;
    if channels.is_empty() {
        return Err("CLAP instrument has no audio output ports".to_owned());
    }
    let mut main_output_index = None;
    let mut buffer = AudioPortInfoBuffer::new();
    for index in 0..channels.len() as u32 {
        let port = ports
            .get(plugin, index, false, &mut buffer)
            .ok_or_else(|| {
                "CLAP instrument returned incomplete audio output information".to_owned()
            })?;
        if port.flags.contains(AudioPortFlags::IS_MAIN)
            && main_output_index.replace(index as usize).is_some()
        {
            return Err("CLAP instrument reports multiple main audio outputs".to_owned());
        }
    }
    let main_output_index = main_output_index
        .ok_or_else(|| "CLAP instrument does not identify a main audio output".to_owned())?;
    Ok((channels, main_output_index))
}

fn read_preview_audio_inputs(
    ports: &PluginAudioPorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
) -> Result<(Vec<u32>, usize), String> {
    let channels = read_preview_audio_port_channels(ports, plugin, true)?;
    if channels.is_empty() {
        return Err("CLAP effect has no audio input ports".to_owned());
    }
    let mut main_input_index = None;
    let mut buffer = AudioPortInfoBuffer::new();
    for index in 0..channels.len() as u32 {
        let port = ports
            .get(plugin, index, true, &mut buffer)
            .ok_or_else(|| "CLAP effect returned incomplete audio input information".to_owned())?;
        if port.flags.contains(AudioPortFlags::IS_MAIN)
            && main_input_index.replace(index as usize).is_some()
        {
            return Err("CLAP effect reports multiple main audio inputs".to_owned());
        }
    }
    let main_input_index = main_input_index
        .ok_or_else(|| "CLAP effect does not identify a main audio input".to_owned())?;
    Ok((channels, main_input_index))
}

fn find_preview_note_input(
    ports: &PluginNotePorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
) -> Result<(u16, PreviewNoteDialect), String> {
    let count = ports.count(plugin, true).min(MAX_INSTANCE_METADATA_ITEMS);
    let mut info_buffer = NotePortInfoBuffer::new();
    for index in 0..count {
        let Some(port) = ports.get(plugin, index, true, &mut info_buffer) else {
            continue;
        };
        let dialect = port
            .preferred_dialect
            .and_then(preview_note_dialect)
            .or_else(|| {
                port.supported_dialects
                    .supports(NoteDialect::Clap)
                    .then_some(PreviewNoteDialect::Clap)
            })
            .or_else(|| {
                (port.supported_dialects.supports(NoteDialect::Midi)
                    || port.supported_dialects.supports(NoteDialect::MidiMpe))
                .then_some(PreviewNoteDialect::Midi)
            })
            .or_else(|| {
                port.supported_dialects
                    .supports(NoteDialect::Midi2)
                    .then_some(PreviewNoteDialect::Midi2)
            });
        if let Some(dialect) = dialect {
            let port_index = u16::try_from(index)
                .map_err(|_| "CLAP instrument note input port index is too large".to_owned())?;
            return Ok((port_index, dialect));
        }
    }
    Err(
        "CLAP instrument has no note input port supporting CLAP, MIDI 1.0, or MIDI 2.0 events"
            .to_owned(),
    )
}

fn preview_note_dialect(dialect: NoteDialect) -> Option<PreviewNoteDialect> {
    match dialect {
        NoteDialect::Clap => Some(PreviewNoteDialect::Clap),
        NoteDialect::Midi | NoteDialect::MidiMpe => Some(PreviewNoteDialect::Midi),
        NoteDialect::Midi2 => Some(PreviewNoteDialect::Midi2),
    }
}

fn midi2_preview_note_event(
    time: u32,
    port_index: u16,
    channel: u8,
    key: u8,
    velocity: u16,
    note_on: bool,
) -> Midi2Event {
    let status = if note_on { 0x9_u32 } else { 0x8_u32 };
    let header =
        0x4000_0000 | (status << 20) | (u32::from(channel & 0x0f) << 16) | (u32::from(key) << 8);
    Midi2Event::new(time, port_index, [header, u32::from(velocity) << 16, 0, 0])
}

fn midi2_preview_velocity(velocity: u8) -> u16 {
    ((u32::from(velocity) * u32::from(u16::MAX)) / 127) as u16
}

fn midi_preview_release_velocity(release: u8) -> u8 {
    ((u16::from(release.min(128)) * 127 + 64) / 128) as u8
}

fn inspect_plugin_instance(path: &Path, plugin_id: &str) -> Result<ClapInstanceInfo, String> {
    let entry = unsafe { PluginEntry::load(path) }
        .map_err(|error| format!("could not load CLAP entry: {error}"))?;
    let plugin_id = CString::new(plugin_id)
        .map_err(|_| "CLAP plug-in ID contains an invalid null byte".to_owned())?;
    let host_info = HostInfo::new(
        "FL Studio Rebuild",
        "fl-studio-rust contributors",
        "https://github.com/cyroz1/fl-studio-rust",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|error| format!("could not describe the host to the CLAP plug-in: {error}"))?;
    let mut instance = PluginInstance::<()>::new(|_| (), |_| (), &entry, &plugin_id, &host_info)
        .map_err(|error| format!("could not initialize CLAP plug-in instance: {error}"))?;
    let plugin = instance.plugin_handle();
    let mut info = ClapInstanceInfo::default();

    if let Some(audio_ports) = plugin.get_extension::<PluginAudioPorts>() {
        info.audio_input_count = audio_ports.count(&plugin, true);
        info.audio_output_count = audio_ports.count(&plugin, false);
        read_audio_ports(&audio_ports, &plugin, true, &mut info.audio_inputs);
        read_audio_ports(&audio_ports, &plugin, false, &mut info.audio_outputs);
    }

    if let Some(note_ports) = plugin.get_extension::<PluginNotePorts>() {
        info.note_input_count = note_ports.count(&plugin, true);
        info.note_output_count = note_ports.count(&plugin, false);
        read_note_ports(&note_ports, &plugin, true, &mut info.note_inputs);
        read_note_ports(&note_ports, &plugin, false, &mut info.note_outputs);
    }

    if let Some(parameters) = plugin.get_extension::<PluginParams>() {
        info.parameter_count = parameters.count(&plugin);
        for index in 0..info.parameter_count.min(MAX_INSTANCE_METADATA_ITEMS) {
            let mut buffer = ParamInfoBuffer::new();
            let Some(parameter) = parameters.get_info(&plugin, index, &mut buffer) else {
                continue;
            };
            info.parameters.push(ClapParameterDescriptor {
                id: parameter.id.get(),
                name: String::from_utf8_lossy(parameter.name).into_owned(),
                module: String::from_utf8_lossy(parameter.module).into_owned(),
                flags: parameter.flags.bits(),
                min_value: parameter.min_value,
                max_value: parameter.max_value,
                default_value: parameter.default_value,
                current_value: parameters.get_value(&plugin, parameter.id),
            });
        }
    }

    Ok(info)
}

fn read_audio_ports(
    ports: &PluginAudioPorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
    is_input: bool,
    destination: &mut Vec<ClapAudioPortDescriptor>,
) {
    let count = ports
        .count(plugin, is_input)
        .min(MAX_INSTANCE_METADATA_ITEMS);
    let mut buffer = AudioPortInfoBuffer::new();
    for index in 0..count {
        let Some(port) = ports.get(plugin, index, is_input, &mut buffer) else {
            continue;
        };
        destination.push(ClapAudioPortDescriptor {
            id: port.id.get(),
            name: String::from_utf8_lossy(port.name).into_owned(),
            channel_count: port.channel_count,
            is_main: port
                .flags
                .contains(clack_extensions::audio_ports::AudioPortFlags::IS_MAIN),
            port_type: port
                .port_type
                .map(|port_type| port_type.0.to_string_lossy().into_owned()),
        });
    }
}

fn read_note_ports(
    ports: &PluginNotePorts,
    plugin: &clack_host::plugin::PluginMainThreadHandle<'_>,
    is_input: bool,
    destination: &mut Vec<ClapNotePortDescriptor>,
) {
    let count = ports
        .count(plugin, is_input)
        .min(MAX_INSTANCE_METADATA_ITEMS);
    let mut buffer = NotePortInfoBuffer::new();
    for index in 0..count {
        let Some(port) = ports.get(plugin, index, is_input, &mut buffer) else {
            continue;
        };
        destination.push(ClapNotePortDescriptor {
            id: port.id.get(),
            name: String::from_utf8_lossy(port.name).into_owned(),
            supported_dialects: port.supported_dialects.bits(),
            preferred_dialect: port.preferred_dialect.map(|dialect| format!("{dialect:?}")),
        });
    }
}

fn encode_probe_output(descriptors: &[ClapPluginDescriptor]) -> String {
    let mut output = format!("{PROBE_HEADER}\n");
    for descriptor in descriptors {
        let features = descriptor
            .features
            .iter()
            .map(|feature| encode_hex(feature))
            .collect::<Vec<_>>()
            .join(",");
        output.push_str(&format!(
            "P\t{}\t{}\t{}\t{}\t{}\t{}\n",
            encode_hex(&descriptor.id),
            encode_hex(&descriptor.name),
            encode_optional(&descriptor.vendor),
            encode_optional(&descriptor.version),
            encode_optional(&descriptor.description),
            features,
        ));
    }
    output
}

fn encode_instance_probe_output(info: &ClapInstanceInfo) -> String {
    let mut output = format!(
        "{INSTANCE_PROBE_HEADER}\nC\t{}\t{}\t{}\t{}\t{}\n",
        info.audio_input_count,
        info.audio_output_count,
        info.note_input_count,
        info.note_output_count,
        info.parameter_count,
    );

    for (direction, ports) in [("I", &info.audio_inputs), ("O", &info.audio_outputs)] {
        for port in ports {
            output.push_str(&format!(
                "A\t{direction}\t{}\t{}\t{}\t{}\t{}\n",
                port.id,
                port.channel_count,
                u8::from(port.is_main),
                port.port_type
                    .as_deref()
                    .map(encode_hex)
                    .unwrap_or_default(),
                encode_hex(&port.name),
            ));
        }
    }

    for (direction, ports) in [("I", &info.note_inputs), ("O", &info.note_outputs)] {
        for port in ports {
            output.push_str(&format!(
                "N\t{direction}\t{}\t{}\t{}\t{}\n",
                port.id,
                port.supported_dialects,
                port.preferred_dialect
                    .as_deref()
                    .map(encode_hex)
                    .unwrap_or_default(),
                encode_hex(&port.name),
            ));
        }
    }

    for parameter in &info.parameters {
        output.push_str(&format!(
            "P\t{}\t{}\t{:016x}\t{:016x}\t{:016x}\t{}\t{}\t{}\n",
            parameter.id,
            parameter.flags,
            parameter.min_value.to_bits(),
            parameter.max_value.to_bits(),
            parameter.default_value.to_bits(),
            parameter
                .current_value
                .map(|value| format!("{:016x}", value.to_bits()))
                .unwrap_or_default(),
            encode_hex(&parameter.name),
            encode_hex(&parameter.module),
        ));
    }

    output
}

fn encode_optional(value: &Option<String>) -> String {
    value.as_deref().map(encode_hex).unwrap_or_default()
}

fn encode_hex(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("invalid odd-length CLAP descriptor field".to_owned());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).map_err(|_| "CLAP descriptor field was not UTF-8".to_owned())
}

fn hex_value(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("invalid hexadecimal CLAP descriptor field".to_owned()),
    }
}

fn decode_optional(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        Ok(None)
    } else {
        decode_hex(value).map(Some)
    }
}

fn parse_probe_output(output: &str) -> Result<Vec<ClapPluginDescriptor>, String> {
    let mut lines = output.lines();
    if !lines.any(|line| line == PROBE_HEADER) {
        return Err("CLAP descriptor probe returned an invalid response".to_owned());
    }

    let mut descriptors = Vec::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.first() != Some(&"P") {
            continue;
        }
        if fields.len() != 7 {
            return Err("CLAP descriptor probe returned an incomplete descriptor".to_owned());
        }
        let id = decode_hex(fields[1])?;
        let name = decode_hex(fields[2])?;
        if id.is_empty() || name.is_empty() {
            return Err("CLAP descriptor is missing its required ID or name".to_owned());
        }
        let features = if fields[6].is_empty() {
            Vec::new()
        } else {
            fields[6]
                .split(',')
                .map(decode_hex)
                .collect::<Result<Vec<_>, _>>()?
        };
        descriptors.push(ClapPluginDescriptor {
            id,
            name,
            vendor: decode_optional(fields[3])?,
            version: decode_optional(fields[4])?,
            description: decode_optional(fields[5])?,
            features,
        });
    }
    Ok(descriptors)
}

fn parse_instance_probe_output(output: &str) -> Result<ClapInstanceInfo, String> {
    let mut lines = output.lines();
    if !lines.any(|line| line == INSTANCE_PROBE_HEADER) {
        return Err("CLAP instance probe returned an invalid response".to_owned());
    }

    let mut info = ClapInstanceInfo::default();
    let mut found_counts = false;
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.first().copied() {
            Some("C") => {
                if found_counts || fields.len() != 6 {
                    return Err("CLAP instance probe returned invalid metadata counts".to_owned());
                }
                info.audio_input_count = parse_count(fields[1])?;
                info.audio_output_count = parse_count(fields[2])?;
                info.note_input_count = parse_count(fields[3])?;
                info.note_output_count = parse_count(fields[4])?;
                info.parameter_count = parse_count(fields[5])?;
                found_counts = true;
            }
            Some("A") => {
                if fields.len() != 7 {
                    return Err("CLAP instance probe returned an incomplete audio port".to_owned());
                }
                let port = ClapAudioPortDescriptor {
                    id: parse_count(fields[2])?,
                    channel_count: parse_count(fields[3])?,
                    is_main: match fields[4] {
                        "0" => false,
                        "1" => true,
                        _ => {
                            return Err(
                                "CLAP instance probe returned an invalid main-port flag".to_owned()
                            );
                        }
                    },
                    port_type: decode_optional(fields[5])?,
                    name: decode_hex(fields[6])?,
                };
                match fields[1] {
                    "I" => info.audio_inputs.push(port),
                    "O" => info.audio_outputs.push(port),
                    _ => {
                        return Err(
                            "CLAP instance probe returned an invalid audio direction".to_owned()
                        );
                    }
                }
            }
            Some("N") => {
                if fields.len() != 6 {
                    return Err("CLAP instance probe returned an incomplete note port".to_owned());
                }
                let port = ClapNotePortDescriptor {
                    id: parse_count(fields[2])?,
                    supported_dialects: parse_count(fields[3])?,
                    preferred_dialect: decode_optional(fields[4])?,
                    name: decode_hex(fields[5])?,
                };
                match fields[1] {
                    "I" => info.note_inputs.push(port),
                    "O" => info.note_outputs.push(port),
                    _ => {
                        return Err(
                            "CLAP instance probe returned an invalid note direction".to_owned()
                        );
                    }
                }
            }
            Some("P") => {
                if fields.len() != 9 {
                    return Err("CLAP instance probe returned an incomplete parameter".to_owned());
                }
                info.parameters.push(ClapParameterDescriptor {
                    id: parse_count(fields[1])?,
                    flags: parse_count(fields[2])?,
                    min_value: parse_f64_bits(fields[3])?,
                    max_value: parse_f64_bits(fields[4])?,
                    default_value: parse_f64_bits(fields[5])?,
                    current_value: if fields[6].is_empty() {
                        None
                    } else {
                        Some(parse_f64_bits(fields[6])?)
                    },
                    name: decode_hex(fields[7])?,
                    module: decode_hex(fields[8])?,
                });
            }
            _ => {}
        }
    }

    if !found_counts {
        return Err("CLAP instance probe omitted metadata counts".to_owned());
    }
    if info.audio_inputs.len() > info.audio_input_count as usize
        || info.audio_outputs.len() > info.audio_output_count as usize
        || info.note_inputs.len() > info.note_input_count as usize
        || info.note_outputs.len() > info.note_output_count as usize
        || info.parameters.len() > info.parameter_count as usize
    {
        return Err("CLAP instance probe returned more metadata than reported".to_owned());
    }

    Ok(info)
}

fn parse_count(value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| "CLAP instance probe returned an invalid count or ID".to_owned())
}

fn parse_f64_bits(value: &str) -> Result<f64, String> {
    u64::from_str_radix(value, 16)
        .map(f64::from_bits)
        .map_err(|_| "CLAP instance probe returned an invalid parameter value".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        ClapAudioPortDescriptor, ClapInstanceInfo, ClapNotePortDescriptor, ClapParameterDescriptor,
        ClapPatternNote, ClapPatternPreviewSettings, ClapPlaylistNote, ClapPlaylistPreviewSettings,
        ClapPlaylistTempoPoint, ClapPluginDescriptor, MAX_NOTE_PREVIEW_NOTE_MILLISECONDS,
        MIN_NOTE_PREVIEW_NOTE_MILLISECONDS, encode_instance_probe_output, encode_probe_output,
        parse_instance_probe_output, parse_pattern_preview_request, parse_playlist_preview_request,
        parse_probe_output, read_preview_server_parameter_values, read_preview_server_state,
        schedule_pattern_preview_events, schedule_playlist_preview_events,
        second_pattern_loop_audio, validate_note_preview_note_length, write_preview_server_state,
    };
    use std::io::Cursor;

    #[test]
    fn descriptor_probe_protocol_round_trips_unicode_and_delimiters() {
        let descriptor = ClapPluginDescriptor {
            id: "org.example.test\tplugin".to_owned(),
            name: "Écho Synth\nMk II".to_owned(),
            vendor: Some("Vendor, Inc.".to_owned()),
            version: Some("1.2.3".to_owned()),
            description: None,
            features: vec!["instrument".to_owned(), "synthesizer".to_owned()],
        };
        let output = format!(
            "plug-in diagnostic output\n{}",
            encode_probe_output(std::slice::from_ref(&descriptor))
        );

        assert_eq!(
            parse_probe_output(&output).expect("probe output should parse"),
            vec![descriptor]
        );
    }

    #[test]
    fn instance_probe_protocol_round_trips_ports_and_parameters() {
        let info = ClapInstanceInfo {
            audio_input_count: 1,
            audio_output_count: 1,
            note_input_count: 1,
            note_output_count: 0,
            parameter_count: 1,
            audio_inputs: vec![ClapAudioPortDescriptor {
                id: 3,
                name: "入力\tMain".to_owned(),
                channel_count: 2,
                is_main: true,
                port_type: Some("stéréo".to_owned()),
            }],
            audio_outputs: vec![ClapAudioPortDescriptor {
                id: 4,
                name: "Output".to_owned(),
                channel_count: 2,
                is_main: true,
                port_type: Some("stereo".to_owned()),
            }],
            note_inputs: vec![ClapNotePortDescriptor {
                id: 8,
                name: "Notes".to_owned(),
                supported_dialects: 3,
                preferred_dialect: Some("Midi".to_owned()),
            }],
            note_outputs: Vec::new(),
            parameters: vec![ClapParameterDescriptor {
                id: 12,
                name: "Cutoff\tFrequency".to_owned(),
                module: "Filter/Étage".to_owned(),
                flags: 7,
                min_value: 20.0,
                max_value: 20_000.0,
                default_value: 440.0,
                current_value: Some(880.0),
            }],
        };
        let output = format!(
            "plug-in diagnostic output\n{}",
            encode_instance_probe_output(&info)
        );

        assert_eq!(
            parse_instance_probe_output(&output).expect("instance metadata should parse"),
            info
        );
    }

    #[test]
    fn preview_note_length_accepts_only_the_supported_range() {
        assert!(validate_note_preview_note_length(MIN_NOTE_PREVIEW_NOTE_MILLISECONDS).is_ok());
        assert!(validate_note_preview_note_length(MAX_NOTE_PREVIEW_NOTE_MILLISECONDS).is_ok());
        assert!(validate_note_preview_note_length(MIN_NOTE_PREVIEW_NOTE_MILLISECONDS - 1).is_err());
        assert!(validate_note_preview_note_length(MAX_NOTE_PREVIEW_NOTE_MILLISECONDS + 1).is_err());
    }

    #[test]
    fn clap_state_protocol_preserves_arbitrary_binary_bytes() {
        let state = [0, b'\n', 0xff, b'\r', 7];
        let mut response = Vec::new();
        write_preview_server_state(&mut response, &state).expect("state response should write");
        let header_end = response
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("state response should include a header");
        assert_eq!(
            std::str::from_utf8(&response[..header_end]).expect("header should be UTF-8"),
            format!("STATE {}", state.len())
        );
        assert_eq!(&response[header_end + 1..response.len() - 1], &state);
        assert_eq!(response.last(), Some(&b'\n'));

        let mut input = Cursor::new(&response[header_end + 1..]);
        assert_eq!(
            read_preview_server_state(&mut input, &state.len().to_string())
                .expect("state request should parse"),
            state
        );
        assert_eq!(input.position() as usize, state.len() + 1);
    }

    #[test]
    fn clap_parameter_protocol_reads_finite_unique_values() {
        let expected = [(42, 0.25_f64), (17, -1.5_f64)];
        let input = format!(
            "PARAM 42 {:016x}\nPARAM 17 {:016x}\nEND\n",
            expected[0].1.to_bits(),
            expected[1].1.to_bits(),
        );
        let parsed =
            read_preview_server_parameter_values(&mut Cursor::new(input.into_bytes()), "2")
                .expect("valid parameter values should parse");

        assert_eq!(parsed, expected);
    }

    #[test]
    fn clap_parameter_protocol_rejects_duplicate_and_non_finite_values() {
        let duplicate = b"PARAM 42 3fd0000000000000\nPARAM 42 3fe0000000000000\nEND\n";
        assert!(read_preview_server_parameter_values(&mut Cursor::new(duplicate), "2").is_err());

        let non_finite = b"PARAM 42 7ff8000000000000\nEND\n";
        assert!(read_preview_server_parameter_values(&mut Cursor::new(non_finite), "1").is_err());
        assert!(read_preview_server_parameter_values(&mut Cursor::new([]), "0").is_err());
    }

    fn pattern_preview_settings() -> ClapPatternPreviewSettings {
        ClapPatternPreviewSettings {
            pattern_id: 4,
            channel_id: 9,
            ppq: 96,
            tempo_bpm: 120.0,
            loop_length_ticks: 192,
            parameter_automation: Vec::new(),
            playlist: None,
            notes: vec![
                ClapPatternNote {
                    position_ticks: 0,
                    length_ticks: 96,
                    key: 60,
                    velocity: 100,
                    midi_channel: 0,
                    fine_pitch_cents: -35,
                    pan: 48,
                    release: 32,
                    mod_x: 100,
                    slide_note: false,
                },
                ClapPatternNote {
                    position_ticks: 96,
                    length_ticks: 96,
                    key: 60,
                    velocity: 80,
                    midi_channel: 0,
                    fine_pitch_cents: 0,
                    pan: 64,
                    release: 64,
                    mod_x: 128,
                    slide_note: false,
                },
            ],
        }
    }

    #[test]
    fn pattern_preview_schedules_tick_positions_and_orders_note_off_before_note_on() {
        let (events, total_frames) =
            schedule_pattern_preview_events(&pattern_preview_settings(), 48_000)
                .expect("valid Pattern notes should schedule");

        assert_eq!(total_frames, 120_000);
        assert_eq!(events.len(), 4);
        assert_eq!((events[0].frame, events[0].is_note_on), (0, true));
        assert_eq!(events[1].frame, 24_000);
        assert!(!events[1].is_note_on);
        assert_eq!(events[1].release, 32);
        assert_eq!(events[2].frame, 24_000);
        assert!(events[2].is_note_on);
        assert_eq!(events[2].fine_pitch_cents, 0);
        assert_eq!(events[2].pan, 64);
        assert_eq!(events[2].mod_x, 128);
        assert_eq!((events[3].frame, events[3].is_note_on), (48_000, false));
    }

    #[test]
    fn playlist_preview_schedules_notes_and_transport_across_tempo_changes() {
        let settings = ClapPatternPreviewSettings {
            pattern_id: 0,
            channel_id: 9,
            ppq: 96,
            tempo_bpm: 120.0,
            loop_length_ticks: 192,
            notes: Vec::new(),
            parameter_automation: Vec::new(),
            playlist: Some(ClapPlaylistPreviewSettings {
                tempo_points: vec![
                    ClapPlaylistTempoPoint {
                        tick: 0,
                        tempo_bpm: 120.0,
                    },
                    ClapPlaylistTempoPoint {
                        tick: 96,
                        tempo_bpm: 60.0,
                    },
                ],
                notes: vec![ClapPlaylistNote {
                    start_tick: 0,
                    stop_tick: 192,
                    key: 60,
                    velocity: 100,
                    midi_channel: 0,
                    fine_pitch_cents: -35,
                    pan: 48,
                    release: 32,
                    mod_x: 100,
                    slide_note: false,
                }],
                parameter_automation: Vec::new(),
            }),
        };

        let (events, total_frames, transport_points) =
            schedule_playlist_preview_events(&settings, 48_000)
                .expect("Playlist note should follow the tempo map");

        assert_eq!(total_frames, 144_000);
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].frame, events[0].is_note_on), (0, true));
        assert_eq!((events[1].frame, events[1].is_note_on), (72_000, false));
        assert_eq!(transport_points.len(), 2);
        assert_eq!(transport_points[0].frame, 0);
        assert_eq!(transport_points[0].tempo_bpm, 120.0);
        assert_eq!(transport_points[1].frame, 24_000);
        assert_eq!(transport_points[1].tick, 96);
        assert_eq!(transport_points[1].tempo_bpm, 60.0);
    }

    #[test]
    fn playlist_preview_parser_reads_tempo_and_note_records() {
        let base_tempo = 123.5_f64;
        let header = format!("96 {:016x} 2 1 0", base_tempo.to_bits());
        let mut input = Cursor::new(
            b"TEMPO 0 405e000000000000\nTEMPO 96 404e000000000000\nNOTE 0 192 60 100 0 -35 48 32 100 0\nEND\n".as_slice(),
        );

        let parsed = parse_playlist_preview_request(&header, &mut input, 48_000)
            .expect("valid Playlist protocol should parse");

        assert_eq!(parsed.ppq, 96);
        assert_eq!(parsed.tempo_bpm, base_tempo);
        let playlist = parsed.playlist.expect("Playlist schedule should parse");
        assert_eq!(playlist.tempo_points.len(), 2);
        assert_eq!(playlist.tempo_points[0].tempo_bpm, 120.0);
        assert_eq!(playlist.tempo_points[1].tempo_bpm, 60.0);
        assert_eq!(playlist.notes.len(), 1);
        assert_eq!(playlist.notes[0].start_tick, 0);
        assert_eq!(playlist.notes[0].stop_tick, 192);
        assert_eq!(playlist.notes[0].fine_pitch_cents, -35);
    }

    #[test]
    fn pattern_loop_audio_discards_warmup_cycle_and_tail() {
        let rendered = [1.0, 2.0, 3.0, 4.0, 90.0, 91.0];

        let loop_audio = second_pattern_loop_audio(&rendered, 1)
            .expect("second Pattern loop should be extracted from helper audio");

        assert_eq!(loop_audio, [3.0, 4.0]);
    }

    #[test]
    fn pattern_preview_parser_reads_bounded_note_records() {
        let tempo_bpm = 123.5_f64;
        let header = format!("96 {:016x} 192 2 0", tempo_bpm.to_bits());
        let mut input = Cursor::new(
            b"NOTE 0 96 60 100 0 -35 48 32 100 0\nNOTE 96 96 64 80 2 0 64 64 128 0\nEND\n"
                .as_slice(),
        );

        let parsed = parse_pattern_preview_request(&header, &mut input, 48_000)
            .expect("valid Pattern protocol should parse");

        assert_eq!(parsed.ppq, 96);
        assert_eq!(parsed.tempo_bpm, tempo_bpm);
        assert_eq!(parsed.loop_length_ticks, 192);
        assert_eq!(parsed.notes.len(), 2);
        assert_eq!(parsed.notes[1].key, 64);
        assert_eq!(parsed.notes[1].midi_channel, 2);
        assert_eq!(parsed.notes[0].fine_pitch_cents, -35);
        assert_eq!(parsed.notes[0].pan, 48);
        assert_eq!(parsed.notes[0].release, 32);
        assert_eq!(parsed.notes[0].mod_x, 100);
    }

    #[test]
    fn pattern_preview_rejects_invalid_timing_and_midi_values() {
        let mut settings = pattern_preview_settings();
        settings.ppq = 0;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.tempo_bpm = f64::NAN;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.notes[0].midi_channel = 16;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.notes[0].key = 128;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.notes[0].fine_pitch_cents = 1201;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.notes[0].pan = 129;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());

        let mut settings = pattern_preview_settings();
        settings.notes[0].release = 129;
        assert!(schedule_pattern_preview_events(&settings, 48_000).is_err());
    }

    #[test]
    fn pattern_release_maps_to_the_midi_note_off_velocity_range() {
        assert_eq!(super::midi_preview_release_velocity(0), 0);
        assert_eq!(super::midi_preview_release_velocity(64), 64);
        assert_eq!(super::midi_preview_release_velocity(128), 127);
        assert_eq!(super::midi_preview_release_velocity(255), 127);
    }
}

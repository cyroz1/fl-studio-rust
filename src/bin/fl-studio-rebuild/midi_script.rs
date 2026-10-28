use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const MIDI_SCRIPT_RUNNER: &str = include_str!("midi_script_runner.py");
const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiControllerScript {
    pub path: PathBuf,
    pub name: String,
    pub url: Option<String>,
    pub supported_devices: Vec<String>,
    pub supported_hardware_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptMidiResult {
    pub handled: bool,
    pub bytes: Vec<u8>,
    pub actions: Vec<ScriptTransportAction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerSendRouteSnapshot {
    pub source_index: u16,
    pub destination_index: u16,
    pub enabled: Option<bool>,
    pub value_raw: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptTransportAction {
    Start,
    Stop,
    RecordToggle,
    TogglePlayback,
    ToggleLoopMode,
    SetSongPosition(u32),
    SetSongPositionTicks(u64),
    SetSongPositionTimeUs(u64),
    MoveSongPosition {
        speed_millionths: i32,
        start: bool,
    },
    SetPlaybackSpeed(u32),
    SetCurrentTempoMilliBpm(u32),
    SetProjectTimeSignatureNumerator(u8),
    SetProjectTimeSignatureDenominator(u8),
    SetGlobalSwingMixRaw(u8),
    AddArrangementTimeMarker {
        tick: u32,
        name: String,
        stop_playback: bool,
    },
    SetArrangementLiveSelection {
        tick: u32,
        stop: bool,
    },
    SetPlaylistTrackSelection(Vec<u32>),
    SetPlaylistTrackName {
        track_id: u32,
        name: String,
    },
    SelectChannel(u16),
    SetMixerTrackSelection {
        index: u16,
        flags: u32,
    },
    SetMixerTrackName {
        index: u16,
        name: String,
    },
    SetMixerTrackColor {
        index: u16,
        color: u32,
    },
    SetMixerSlotColor {
        index: u16,
        slot: u8,
        color: u32,
    },
    SetMixerTrackEnabled {
        index: u16,
        enabled: bool,
    },
    SetMixerTrackFlag {
        index: u16,
        flag: MixerTrackFlag,
        value: bool,
        solo_mode: Option<u8>,
    },
    SetMixerTrackParameter {
        index: u16,
        parameter: MixerTrackParameter,
        value_raw: i32,
    },
    SetMixerRoute {
        source_index: u16,
        destination_index: u16,
        enabled: bool,
        update_ui: bool,
    },
    SetMixerRouteLevel {
        source_index: u16,
        destination_index: u16,
        value_raw: i32,
    },
    LinkChannelToMixerTrack {
        channel_index: u16,
        track_index: u8,
        select: bool,
    },
    JumpToMarker {
        delta: i32,
        select: bool,
    },
    SelectNextEmptyPatternWithNameDialog,
    FocusSelectedMixerTrackName,
    OpenHelpManual,
    Undo,
    Redo,
    UndoHistoryJog(i32),
    TogglePianoRollSnap,
    PianoRollSnapJog(i32),
    PatternJog(i32),
    TempoJog(i32),
    MarkerJumpJog(i32),
    MarkerSelJog(i32),
    ShowMainView(u8),
    ToggleAudioSettingsWindow,
    ToggleProjectInfo,
    CloseAllWindows,
    MidiOutput(Vec<u8>),
    RepeatMidiOutput {
        bytes: Vec<u8>,
        delay_ms: u32,
        rate_ms: u32,
    },
    StopRepeatMidiOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerTrackFlag {
    Soloed,
    PolarityReversed,
    SwapLeftRight,
    EffectsEnabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerTrackParameter {
    Volume,
    Pan,
    StereoSeparation,
}

const SCRIPT_SONG_POSITION_SCALE: f64 = 1_000_000.0;
const SCRIPT_SONG_POSITION_SCALE_U32: u32 = 1_000_000;
const SCRIPT_MIN_TEMPO_MILLI_BPM: u32 = 10_000;
const SCRIPT_MAX_TEMPO_MILLI_BPM: u32 = 522_000;
const SCRIPT_MAX_ARRANGEMENT_MARKER_TICK: u32 = 0x07FF_FFFF;
const SCRIPT_MAX_PLAYLIST_TRACK_COUNT: usize = 65_536;
impl ScriptTransportAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "record" => Some(Self::RecordToggle),
            "toggle_playback" => Some(Self::TogglePlayback),
            "loop_mode" => Some(Self::ToggleLoopMode),
            "select_next_empty_pattern_with_name_dialog" => {
                Some(Self::SelectNextEmptyPatternWithNameDialog)
            }
            "focus_selected_mixer_track_name" => Some(Self::FocusSelectedMixerTrackName),
            "open_help_manual" => Some(Self::OpenHelpManual),
            "undo" => Some(Self::Undo),
            "redo" => Some(Self::Redo),
            "toggle_piano_roll_snap" => Some(Self::TogglePianoRollSnap),
            _ => None,
        }
    }
}

pub struct MidiScriptRuntime {
    child: Arc<Mutex<Child>>,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    _runner_script: tempfile::TempPath,
    transport_state: Arc<MidiScriptTransportState>,
    startup_actions: Vec<ScriptTransportAction>,
    supports_midi_output: bool,
}

pub const SCRIPT_TRANSPORT_PLAYING: u8 = 1 << 0;
pub const SCRIPT_TRANSPORT_RECORDING: u8 = 1 << 1;
pub const SCRIPT_TRANSPORT_PATTERN_MODE: u8 = 1 << 2;
pub const SCRIPT_PROJECT_LOAD_START: i32 = 0;
pub const SCRIPT_PROJECT_LOAD_OK: i32 = 100;
pub const SCRIPT_PROJECT_LOAD_ERROR: i32 = 101;
pub const SCRIPT_BEAT_INDICATOR_OFF: i32 = 0;
pub const SCRIPT_BEAT_INDICATOR_BAR: i32 = 1;
pub const SCRIPT_BEAT_INDICATOR_BEAT: i32 = 2;
pub const SCRIPT_MIXER_SCROLL_TO_MAKE_VISIBLE: u32 = 1 << 0;

pub struct MidiScriptTransportState {
    pub state: AtomicU8,
    pub midi_output_port_number: AtomicI32,
    pub song_position: AtomicU64,
    pub song_position_ticks: AtomicU64,
    pub ticks_per_quarter: AtomicU64,
    pub current_tempo_milli_bpm: AtomicU64,
    pub project_changed_flag: AtomicI32,
    pub time_signature_numerator: AtomicU64,
    pub time_signature_denominator: AtomicU64,
    pub global_swing_mix_raw: AtomicU64,
    pub song_length_ticks: AtomicU64,
    pub song_position_seconds: AtomicU64,
    pub song_length_seconds: AtomicU64,
    pub hardware_beat_led_state: AtomicI32,
    pub song_position_hint: Mutex<String>,
    pub song_length_hint: Mutex<String>,
    pub arrangement_marker_names: Mutex<Vec<String>>,
    pub pattern_time_signature_changes: Mutex<Vec<(u32, u8, u8)>>,
    pub arrangement_time_signature_changes: Mutex<Vec<(u32, u8, u8)>>,
    pub arrangement_current_time: AtomicU64,
    pub arrangement_current_time_snapped: AtomicU64,
    pub arrangement_selection_start: AtomicU64,
    pub arrangement_selection_end: AtomicU64,
    pub arrangement_live_selection_start: AtomicU64,
    pub arrangement_live_selection_end: AtomicU64,
    pub playlist_track_count: AtomicU64,
    pub playlist_track_names: Mutex<Vec<String>>,
    pub selected_playlist_track_ids: Mutex<Vec<u32>>,
    pub midi_pickup_enabled: AtomicU8,
    pub channel_ids: Mutex<Vec<u16>>,
    pub selected_channel_id: AtomicU64,
    pub mixer_track_names: Mutex<Vec<String>>,
    pub mixer_track_recording_file_names: Mutex<Vec<String>>,
    pub mixer_selected_track: AtomicU64,
    pub mixer_track_colors: Mutex<Vec<u32>>,
    pub mixer_track_slot_colors: Mutex<Vec<Vec<Option<u32>>>>,
    pub mixer_track_peaks: Mutex<Vec<(f32, f32)>>,
    pub mixer_send_routes: Mutex<Vec<MixerSendRouteSnapshot>>,
    pub mixer_track_enabled: Mutex<Vec<Option<bool>>>,
    pub mixer_track_mute_locked: Mutex<Vec<Option<bool>>>,
    pub mixer_track_soloed: Mutex<Vec<Option<bool>>>,
    pub mixer_track_solo_modes: Mutex<Vec<Option<u8>>>,
    pub mixer_track_polarity_reversed: Mutex<Vec<Option<bool>>>,
    pub mixer_track_swap_left_right: Mutex<Vec<Option<bool>>>,
    pub mixer_track_effects_enabled: Mutex<Vec<Option<bool>>>,
    pub mixer_track_volume_raw: Mutex<Vec<Option<i32>>>,
    pub mixer_track_pan_raw: Mutex<Vec<Option<i32>>>,
    pub mixer_track_stereo_separation_raw: Mutex<Vec<Option<i32>>>,
    pub project_title: Mutex<String>,
    pub project_author: Mutex<String>,
    pub project_genre: Mutex<String>,
}

impl Default for MidiScriptTransportState {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(0),
            midi_output_port_number: AtomicI32::new(-1),
            song_position: AtomicU64::new(0.0_f64.to_bits()),
            song_position_ticks: AtomicU64::new(0),
            ticks_per_quarter: AtomicU64::new(96),
            current_tempo_milli_bpm: AtomicU64::new(140_000),
            project_changed_flag: AtomicI32::new(0),
            time_signature_numerator: AtomicU64::new(4),
            time_signature_denominator: AtomicU64::new(4),
            global_swing_mix_raw: AtomicU64::new(0),
            song_length_ticks: AtomicU64::new(0),
            song_position_seconds: AtomicU64::new(0.0_f64.to_bits()),
            song_length_seconds: AtomicU64::new(0.0_f64.to_bits()),
            hardware_beat_led_state: AtomicI32::new(SCRIPT_BEAT_INDICATOR_OFF),
            song_position_hint: Mutex::new("1:01:000".to_owned()),
            song_length_hint: Mutex::new("0:00:000".to_owned()),
            arrangement_marker_names: Mutex::new(Vec::new()),
            pattern_time_signature_changes: Mutex::new(Vec::new()),
            arrangement_time_signature_changes: Mutex::new(Vec::new()),
            arrangement_current_time: AtomicU64::new(0),
            arrangement_current_time_snapped: AtomicU64::new(0),
            arrangement_selection_start: AtomicU64::new(0),
            arrangement_selection_end: AtomicU64::new(0),
            arrangement_live_selection_start: AtomicU64::new(0),
            arrangement_live_selection_end: AtomicU64::new(0),
            playlist_track_count: AtomicU64::new(0),
            playlist_track_names: Mutex::new(Vec::new()),
            selected_playlist_track_ids: Mutex::new(Vec::new()),
            midi_pickup_enabled: AtomicU8::new(1),
            channel_ids: Mutex::new(Vec::new()),
            selected_channel_id: AtomicU64::new(u64::MAX),
            mixer_track_names: Mutex::new(vec!["Master".to_owned()]),
            mixer_track_recording_file_names: Mutex::new(vec![String::new()]),
            mixer_selected_track: AtomicU64::new(0),
            mixer_track_colors: Mutex::new(vec![0]),
            mixer_track_slot_colors: Mutex::new(vec![vec![None; 10]]),
            mixer_track_peaks: Mutex::new(vec![(0.0, 0.0)]),
            mixer_send_routes: Mutex::new(Vec::new()),
            mixer_track_enabled: Mutex::new(vec![None]),
            mixer_track_mute_locked: Mutex::new(vec![None]),
            mixer_track_soloed: Mutex::new(vec![None]),
            mixer_track_solo_modes: Mutex::new(vec![None]),
            mixer_track_polarity_reversed: Mutex::new(vec![None]),
            mixer_track_swap_left_right: Mutex::new(vec![None]),
            mixer_track_effects_enabled: Mutex::new(vec![None]),
            mixer_track_volume_raw: Mutex::new(vec![None]),
            mixer_track_pan_raw: Mutex::new(vec![None]),
            mixer_track_stereo_separation_raw: Mutex::new(vec![None]),
            project_title: Mutex::new(String::new()),
            project_author: Mutex::new(String::new()),
            project_genre: Mutex::new(String::new()),
        }
    }
}

pub fn normalized_song_position(position_ticks: f64, length_ticks: u64) -> f64 {
    if !position_ticks.is_finite() || length_ticks == 0 {
        return 0.0;
    }
    (position_ticks / length_ticks as f64).clamp(0.0, 1.0)
}

pub fn absolute_song_position_ticks(position_ticks: f64) -> u64 {
    if !position_ticks.is_finite() || position_ticks <= 0.0 {
        return 0;
    }
    position_ticks.round() as u64
}

pub fn song_position_ticks(position: u32, length_ticks: u64) -> u64 {
    let scaled =
        u128::from(position.min(SCRIPT_SONG_POSITION_SCALE_U32)) * u128::from(length_ticks);
    ((scaled + u128::from(SCRIPT_SONG_POSITION_SCALE_U32 / 2))
        / u128::from(SCRIPT_SONG_POSITION_SCALE_U32)) as u64
}

#[derive(Clone)]
pub struct MidiScriptStopHandle(Arc<Mutex<Child>>);

impl MidiScriptStopHandle {
    pub fn kill(&self) {
        if let Ok(mut child) = self.0.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

impl MidiControllerScript {
    fn parse(path: PathBuf) -> Result<Self, String> {
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if metadata.len() > MAX_SCRIPT_BYTES {
            return Err(format!(
                "MIDI script is larger than {} bytes: {}",
                MAX_SCRIPT_BYTES,
                path.display()
            ));
        }
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let mut name = None;
        let mut url = None;
        let mut supported_devices = Vec::new();
        let mut supported_hardware_ids = Vec::new();
        for line in source.lines().take(100) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(parameter) = line.strip_prefix('#') else {
                break;
            };
            let Some((key, value)) = parameter.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "name" => name = (!value.is_empty()).then(|| value.to_owned()),
                "url" => {
                    if value.starts_with("https://forum.image-line.com/") {
                        url = Some(value.to_owned());
                    }
                }
                "supportedDevices" => supported_devices = split_names(value),
                "supportedHardwareIds" => supported_hardware_ids = split_names(value),
                _ => {}
            }
        }
        let name = name.ok_or_else(|| {
            format!(
                "MIDI script is missing its required '# name=' header: {}",
                path.display()
            )
        })?;
        Ok(Self {
            path,
            name,
            url,
            supported_devices,
            supported_hardware_ids,
        })
    }
}

fn split_names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn discover_midi_scripts(
    hardware_directory: &Path,
) -> Result<Vec<MidiControllerScript>, String> {
    let entries = match fs::read_dir(hardware_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "could not list MIDI script directory {}: {error}",
                hardware_directory.display()
            ));
        }
    };
    let mut directories = entries
        .map(|entry| entry.map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("could not list MIDI script directories: {error}"))?;
    directories.sort_by_key(|entry| entry.file_name());

    let mut scripts = Vec::new();
    let mut errors = Vec::new();
    for directory in directories {
        let file_type = directory
            .file_type()
            .map_err(|error| format!("could not inspect MIDI script folder: {error}"))?;
        if !file_type.is_dir() {
            continue;
        }
        let folder = directory.path();
        let files = fs::read_dir(&folder).map_err(|error| {
            format!(
                "could not list MIDI script folder {}: {error}",
                folder.display()
            )
        })?;
        let mut files = files
            .map(|entry| entry.map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("could not list MIDI script files: {error}"))?;
        files.sort_by_key(|entry| entry.file_name());
        for file in files {
            let file_type = file
                .file_type()
                .map_err(|error| format!("could not inspect MIDI script file: {error}"))?;
            if !file_type.is_file() {
                continue;
            }
            let path = file.path();
            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !filename.starts_with("device_")
                || !path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
            {
                continue;
            }
            match MidiControllerScript::parse(path) {
                Ok(script) => scripts.push(script),
                Err(error) => errors.push(error),
            }
        }
    }
    if scripts.is_empty() && !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(scripts)
}

pub fn find_script_for_device<'a>(
    scripts: &'a [MidiControllerScript],
    device_name: &str,
) -> Option<&'a MidiControllerScript> {
    let device_name = normalize_device_name(device_name);
    if device_name.is_empty() {
        return None;
    }
    let mut matches = scripts.iter().filter(|script| {
        script
            .supported_devices
            .iter()
            .any(|name| normalize_device_name(name) == device_name)
    });
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched)
}

fn normalize_device_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl MidiScriptRuntime {
    #[cfg(test)]
    pub fn start_for_device(
        script: &MidiControllerScript,
        device_name: &str,
    ) -> Result<Self, String> {
        Self::start_for_device_with_transport_state(
            script,
            device_name,
            Arc::new(MidiScriptTransportState::default()),
        )
    }

    pub fn start_for_device_with_transport_state(
        script: &MidiControllerScript,
        device_name: &str,
        transport_state: Arc<MidiScriptTransportState>,
    ) -> Result<Self, String> {
        let (program, prefix_args) = python_command()?;
        let mut runner_script = tempfile::Builder::new()
            .prefix("fl-studio-midi-script-runner-")
            .suffix(".py")
            .tempfile()
            .map_err(|error| format!("could not create temporary MIDI script runner: {error}"))?;
        runner_script
            .write_all(MIDI_SCRIPT_RUNNER.as_bytes())
            .map_err(|error| format!("could not write temporary MIDI script runner: {error}"))?;
        let runner_script = runner_script.into_temp_path();
        let mut command = Command::new(program);
        command
            .args(prefix_args)
            .arg("-u")
            .arg(runner_script.as_os_str())
            .arg(&script.path)
            .arg(&script.name)
            .arg(device_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start Python for MIDI script '{}': {error}",
                script.name
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Python did not expose its input stream".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Python did not expose its output stream".to_owned())?;
        let child = Arc::new(Mutex::new(child));
        let mut runtime = Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            _runner_script: runner_script,
            transport_state,
            startup_actions: Vec::new(),
            supports_midi_output: false,
        };
        let mut ready = String::new();
        runtime.stdout.read_line(&mut ready).map_err(|error| {
            format!(
                "could not initialize MIDI script '{}': {error}",
                script.name
            )
        })?;
        let ready: Value = serde_json::from_str(&ready).map_err(|error| {
            format!(
                "MIDI script '{}' returned invalid startup data: {error}",
                script.name
            )
        })?;
        if !ready["ready"].as_bool().unwrap_or(false) {
            return Err(format!(
                "could not initialize MIDI script '{}': {}",
                script.name,
                ready["error"]
                    .as_str()
                    .unwrap_or("script initialization failed")
            ));
        }
        runtime.supports_midi_output = ready["on_midi_out_msg"].as_bool().unwrap_or(false);
        let startup_response = runtime.exchange(&json!({"initialize": true}))?;
        runtime.startup_actions = parse_transport_actions(&startup_response)?;
        Ok(runtime)
    }

    pub fn take_startup_actions(&mut self) -> Vec<ScriptTransportAction> {
        std::mem::take(&mut self.startup_actions)
    }

    pub fn supports_midi_output(&self) -> bool {
        self.supports_midi_output
    }

    pub fn process_event(
        &mut self,
        timestamp_micros: u64,
        bytes: &[u8],
    ) -> Result<ScriptMidiResult, String> {
        if bytes.is_empty() || bytes.len() > 65_536 {
            return Err("MIDI script input must contain 1–65,536 bytes".to_owned());
        }
        let status = bytes[0];
        let value = json!({
            "timestamp": timestamp_micros,
            "status": status,
            "data1": bytes.get(1).copied().unwrap_or(0),
            "data2": bytes.get(2).copied().unwrap_or(0),
            "pmeflags": 2 | 32,
            "sysex": if status == 0xF0 { bytes } else { &[] },
        });
        let response = self.exchange(&value)?;
        let actions = parse_transport_actions(&response)?;
        if response["handled"].as_bool().unwrap_or(false) {
            return Ok(ScriptMidiResult {
                handled: true,
                bytes: Vec::new(),
                actions,
            });
        }
        let transformed = transform_message(bytes, &response)?;
        Ok(ScriptMidiResult {
            handled: false,
            bytes: transformed,
            actions,
        })
    }

    pub fn process_midi_output(&mut self, bytes: &[u8]) -> Result<ScriptMidiResult, String> {
        if !valid_short_midi_output(bytes) {
            return Err("MIDI script output callback requires a short MIDI message".to_owned());
        }
        let status = bytes[0];
        let response = self.exchange(&json!({
            "midi_output": true,
            "status": status,
            "data1": bytes.get(1).copied().unwrap_or(0),
            "data2": bytes.get(2).copied().unwrap_or(0),
            "port": self.transport_state.midi_output_port_number.load(Ordering::Relaxed),
            "midi_id": status & 0xF0,
            "midi_chan": status & 0x0F,
            "midi_chan_ex": 0,
        }))?;
        let actions = parse_transport_actions(&response)?;
        if response["handled"].as_bool().unwrap_or(false) {
            return Ok(ScriptMidiResult {
                handled: true,
                bytes: Vec::new(),
                actions,
            });
        }
        Ok(ScriptMidiResult {
            handled: false,
            bytes: transform_midi_output_message(bytes, &response)?,
            actions,
        })
    }

    pub fn process_idle(&mut self) -> Result<ScriptMidiResult, String> {
        let response = self.exchange(&json!({"idle": true}))?;
        Ok(ScriptMidiResult {
            handled: response["handled"].as_bool().unwrap_or(false),
            bytes: Vec::new(),
            actions: parse_transport_actions(&response)?,
        })
    }

    pub fn process_project_load(&mut self, status: i32) -> Result<ScriptMidiResult, String> {
        let response = self.exchange(&json!({"project_load_status": status}))?;
        Ok(ScriptMidiResult {
            handled: false,
            bytes: Vec::new(),
            actions: parse_transport_actions(&response)?,
        })
    }

    pub fn process_beat_indicator(&mut self, value: i32) -> Result<ScriptMidiResult, String> {
        let response = self.exchange(&json!({"beat_indicator": value}))?;
        Ok(ScriptMidiResult {
            handled: false,
            bytes: Vec::new(),
            actions: parse_transport_actions(&response)?,
        })
    }

    pub fn process_first_connect(&mut self) -> Result<ScriptMidiResult, String> {
        let response = self.exchange(&json!({"first_connect": true}))?;
        Ok(ScriptMidiResult {
            handled: false,
            bytes: Vec::new(),
            actions: parse_transport_actions(&response)?,
        })
    }

    fn exchange(&mut self, value: &Value) -> Result<Value, String> {
        let transport = self.transport_state.state.load(Ordering::Relaxed);
        let song_position =
            f64::from_bits(self.transport_state.song_position.load(Ordering::Relaxed));
        let song_position = if song_position.is_finite() {
            song_position.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let song_position_hint = self
            .transport_state
            .song_position_hint
            .lock()
            .map(|hint| hint.clone())
            .unwrap_or_else(|_| "1:01:000".to_owned());
        let song_length_hint = self
            .transport_state
            .song_length_hint
            .lock()
            .map(|hint| hint.clone())
            .unwrap_or_else(|_| "0:00:000".to_owned());
        let arrangement_marker_names = self
            .transport_state
            .arrangement_marker_names
            .lock()
            .map(|names| names.clone())
            .unwrap_or_default();
        let playlist_track_count = self
            .transport_state
            .playlist_track_count
            .load(Ordering::Relaxed);
        let playlist_track_names = self
            .transport_state
            .playlist_track_names
            .lock()
            .map(|track_names| track_names.clone())
            .unwrap_or_default();
        let selected_playlist_track_ids = self
            .transport_state
            .selected_playlist_track_ids
            .lock()
            .map(|track_ids| track_ids.clone())
            .unwrap_or_default();
        let channel_ids = self
            .transport_state
            .channel_ids
            .lock()
            .map(|ids| ids.clone())
            .unwrap_or_default();
        let selected_channel_id = self
            .transport_state
            .selected_channel_id
            .load(Ordering::Relaxed);
        let mixer_track_names = self
            .transport_state
            .mixer_track_names
            .lock()
            .map(|names| names.clone())
            .unwrap_or_else(|_| vec!["Master".to_owned()]);
        let mixer_track_recording_file_names = self
            .transport_state
            .mixer_track_recording_file_names
            .lock()
            .map(|names| names.clone())
            .unwrap_or_else(|_| vec![String::new()]);
        let mixer_track_colors = self
            .transport_state
            .mixer_track_colors
            .lock()
            .map(|colors| colors.clone())
            .unwrap_or_else(|_| vec![0]);
        let mixer_track_slot_colors = self
            .transport_state
            .mixer_track_slot_colors
            .lock()
            .map(|colors| colors.clone())
            .unwrap_or_else(|_| vec![vec![None; 10]]);
        let mixer_track_peaks = self
            .transport_state
            .mixer_track_peaks
            .lock()
            .map(|peaks| peaks.clone())
            .unwrap_or_else(|_| vec![(0.0, 0.0)]);
        let mixer_send_routes = self
            .transport_state
            .mixer_send_routes
            .lock()
            .map(|routes| routes.clone())
            .unwrap_or_default();
        let mixer_track_enabled = self
            .transport_state
            .mixer_track_enabled
            .lock()
            .map(|enabled| enabled.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_soloed = self
            .transport_state
            .mixer_track_soloed
            .lock()
            .map(|soloed| soloed.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_mute_locked = self
            .transport_state
            .mixer_track_mute_locked
            .lock()
            .map(|locked| locked.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_polarity_reversed = self
            .transport_state
            .mixer_track_polarity_reversed
            .lock()
            .map(|flags| flags.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_swap_left_right = self
            .transport_state
            .mixer_track_swap_left_right
            .lock()
            .map(|flags| flags.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_effects_enabled = self
            .transport_state
            .mixer_track_effects_enabled
            .lock()
            .map(|flags| flags.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_pan_raw = self
            .transport_state
            .mixer_track_pan_raw
            .lock()
            .map(|values| values.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_volume_raw = self
            .transport_state
            .mixer_track_volume_raw
            .lock()
            .map(|values| values.clone())
            .unwrap_or_else(|_| vec![None]);
        let mixer_track_stereo_separation_raw = self
            .transport_state
            .mixer_track_stereo_separation_raw
            .lock()
            .map(|values| values.clone())
            .unwrap_or_else(|_| vec![None]);
        let pattern_time_signature_changes = self
            .transport_state
            .pattern_time_signature_changes
            .lock()
            .map(|changes| changes.clone())
            .unwrap_or_default();
        let arrangement_time_signature_changes = self
            .transport_state
            .arrangement_time_signature_changes
            .lock()
            .map(|changes| changes.clone())
            .unwrap_or_default();
        let arrangement_current_time = self
            .transport_state
            .arrangement_current_time
            .load(Ordering::Relaxed);
        let arrangement_current_time_snapped = self
            .transport_state
            .arrangement_current_time_snapped
            .load(Ordering::Relaxed);
        let arrangement_selection_start = self
            .transport_state
            .arrangement_selection_start
            .load(Ordering::Relaxed);
        let arrangement_selection_end = self
            .transport_state
            .arrangement_selection_end
            .load(Ordering::Relaxed);
        let project_title = self
            .transport_state
            .project_title
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let project_author = self
            .transport_state
            .project_author
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let project_genre = self
            .transport_state
            .project_genre
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        let mut request = value.clone();
        if let Some(object) = request.as_object_mut() {
            let mut transport = json!({
                "is_playing": transport & SCRIPT_TRANSPORT_PLAYING != 0,
                "is_recording": transport & SCRIPT_TRANSPORT_RECORDING != 0,
                "pattern_mode": transport & SCRIPT_TRANSPORT_PATTERN_MODE != 0,
                "midi_output_port_number": self
                    .transport_state
                    .midi_output_port_number
                    .load(Ordering::Relaxed),
                "song_position": song_position,
                "song_position_ticks": self
                    .transport_state
                    .song_position_ticks
                    .load(Ordering::Relaxed),
                "ticks_per_quarter": self
                    .transport_state
                    .ticks_per_quarter
                    .load(Ordering::Relaxed)
                    .max(1),
                "current_tempo_milli_bpm": self
                    .transport_state
                    .current_tempo_milli_bpm
                    .load(Ordering::Relaxed)
                    .max(1),
                "project_changed_flag": self
                    .transport_state
                    .project_changed_flag
                    .load(Ordering::Relaxed),
                "time_signature_numerator": self
                    .transport_state
                    .time_signature_numerator
                    .load(Ordering::Relaxed)
                    .max(1),
                "time_signature_denominator": self
                    .transport_state
                    .time_signature_denominator
                    .load(Ordering::Relaxed)
                    .max(1),
                "song_length_ticks": self
                    .transport_state
                    .song_length_ticks
                    .load(Ordering::Relaxed),
                "song_position_seconds": f64::from_bits(
                    self.transport_state
                        .song_position_seconds
                        .load(Ordering::Relaxed),
                ),
                "song_length_seconds": f64::from_bits(
                    self.transport_state
                        .song_length_seconds
                        .load(Ordering::Relaxed),
                ),
                "hardware_beat_led_state": self
                    .transport_state
                    .hardware_beat_led_state
                    .load(Ordering::Relaxed),
                "song_position_hint": song_position_hint,
                "song_length_hint": song_length_hint,
                "arrangement_marker_names": arrangement_marker_names,
                "mixer_track_names": mixer_track_names,
                "mixer_selected_track": self
                    .transport_state
                    .mixer_selected_track
                    .load(Ordering::Relaxed),
                "mixer_track_colors": mixer_track_colors,
                "mixer_send_routes": mixer_send_routes
                    .iter()
                    .map(|route| {
                        json!([
                            route.source_index,
                            route.destination_index,
                            route.enabled,
                            route.value_raw
                        ])
                    })
                    .collect::<Vec<_>>(),
                "mixer_track_enabled": mixer_track_enabled,
                "mixer_track_soloed": mixer_track_soloed,
                "mixer_track_polarity_reversed": mixer_track_polarity_reversed,
                "mixer_track_swap_left_right": mixer_track_swap_left_right,
                "mixer_track_effects_enabled": mixer_track_effects_enabled,
                "mixer_track_pan_raw": mixer_track_pan_raw,
                "mixer_track_volume_raw": mixer_track_volume_raw,
                "mixer_track_stereo_separation_raw": mixer_track_stereo_separation_raw,
                "pattern_time_signature_changes": pattern_time_signature_changes,
                "arrangement_time_signature_changes": arrangement_time_signature_changes,
                "arrangement_current_time": arrangement_current_time,
                "arrangement_current_time_snapped": arrangement_current_time_snapped,
                "arrangement_selection_start": arrangement_selection_start,
                "arrangement_selection_end": arrangement_selection_end,
                "arrangement_live_selection_start": self
                    .transport_state
                    .arrangement_live_selection_start
                    .load(Ordering::Relaxed),
                "arrangement_live_selection_end": self
                    .transport_state
                    .arrangement_live_selection_end
                    .load(Ordering::Relaxed),
                "project_title": project_title,
                "project_author": project_author,
                "project_genre": project_genre,
            });
            if let Some(transport_object) = transport.as_object_mut() {
                transport_object.insert(
                    "playlist_track_count".to_owned(),
                    json!(playlist_track_count),
                );
                transport_object.insert(
                    "playlist_track_names".to_owned(),
                    json!(playlist_track_names),
                );
                transport_object.insert(
                    "selected_playlist_track_ids".to_owned(),
                    json!(selected_playlist_track_ids),
                );
                transport_object.insert("channel_ids".to_owned(), json!(channel_ids));
                transport_object
                    .insert("selected_channel_id".to_owned(), json!(selected_channel_id));
                transport_object.insert(
                    "global_swing_mix_raw".to_owned(),
                    json!(
                        self.transport_state
                            .global_swing_mix_raw
                            .load(Ordering::Relaxed)
                    ),
                );
                transport_object.insert(
                    "midi_pickup_enabled".to_owned(),
                    json!(
                        self.transport_state
                            .midi_pickup_enabled
                            .load(Ordering::Relaxed)
                            != 0
                    ),
                );
                transport_object.insert("mixer_track_peaks".to_owned(), json!(mixer_track_peaks));
                transport_object.insert(
                    "mixer_track_slot_colors".to_owned(),
                    json!(mixer_track_slot_colors),
                );
                transport_object.insert(
                    "mixer_track_mute_locked".to_owned(),
                    json!(mixer_track_mute_locked),
                );
                transport_object.insert(
                    "mixer_track_recording_file_names".to_owned(),
                    json!(mixer_track_recording_file_names),
                );
            }
            object.insert("transport".to_owned(), transport);
        }
        serde_json::to_writer(&mut self.stdin, &request)
            .map_err(|error| format!("could not send MIDI to Python script: {error}"))?;
        self.stdin
            .write_all(b"\n")
            .and_then(|_| self.stdin.flush())
            .map_err(|error| format!("could not send MIDI to Python script: {error}"))?;

        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .map_err(|error| format!("could not read MIDI script response: {error}"))?;
        if response.is_empty() {
            return Err("Python MIDI script stopped unexpectedly".to_owned());
        }
        let response: Value = serde_json::from_str(&response)
            .map_err(|error| format!("MIDI script returned invalid data: {error}"))?;
        if let Some(error) = response["error"].as_str() {
            return Err(format!("Python MIDI script callback failed: {error}"));
        }
        Ok(response)
    }

    pub fn stop_handle(&self) -> MidiScriptStopHandle {
        MidiScriptStopHandle(Arc::clone(&self.child))
    }

    pub fn shutdown(&mut self) -> Result<Vec<ScriptTransportAction>, String> {
        let response = self.exchange(&json!({"shutdown": true}))?;
        parse_transport_actions(&response)
    }

    pub fn kill(&self) {
        self.stop_handle().kill();
    }
}

impl Drop for MidiScriptRuntime {
    fn drop(&mut self) {
        self.kill();
    }
}

fn python_command() -> Result<(PathBuf, Vec<String>), String> {
    if let Some(executable) = std::env::var_os("FL_STUDIO_REBUILD_PYTHON") {
        return Ok((PathBuf::from(executable), Vec::new()));
    }
    if cfg!(target_os = "windows") {
        Ok((PathBuf::from("py"), vec!["-3".to_owned()]))
    } else {
        Ok((PathBuf::from("python3"), Vec::new()))
    }
}

fn parse_transport_actions(response: &Value) -> Result<Vec<ScriptTransportAction>, String> {
    response["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(parse_transport_action)
        .collect()
}

fn parse_transport_action(action: &Value) -> Result<ScriptTransportAction, String> {
    if let Some(name) = action.as_str() {
        return ScriptTransportAction::parse(name)
            .ok_or_else(|| "MIDI script returned an unknown transport action".to_owned());
    }
    let name = action["action"].as_str();
    if name == Some("set_playlist_track_selection") {
        let track_ids = action["track_ids"]
            .as_array()
            .filter(|track_ids| track_ids.len() <= SCRIPT_MAX_PLAYLIST_TRACK_COUNT)
            .ok_or_else(|| "MIDI script returned an invalid Playlist track selection".to_owned())?;
        let track_ids = track_ids
            .iter()
            .map(|track_id| {
                track_id
                    .as_u64()
                    .and_then(|track_id| u32::try_from(track_id).ok())
                    .filter(|track_id| {
                        (1..=SCRIPT_MAX_PLAYLIST_TRACK_COUNT as u32).contains(track_id)
                    })
                    .ok_or_else(|| "MIDI script returned an invalid Playlist track ID".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut unique_track_ids = track_ids.clone();
        unique_track_ids.sort_unstable();
        unique_track_ids.dedup();
        if unique_track_ids.len() != track_ids.len() {
            return Err("MIDI script returned duplicate Playlist track IDs".to_owned());
        }
        return Ok(ScriptTransportAction::SetPlaylistTrackSelection(track_ids));
    }
    if name == Some("set_playlist_track_name") {
        let track_id = action["track_id"]
            .as_u64()
            .and_then(|track_id| u32::try_from(track_id).ok())
            .filter(|track_id| (1..=SCRIPT_MAX_PLAYLIST_TRACK_COUNT as u32).contains(track_id))
            .ok_or_else(|| "MIDI script returned an invalid Playlist track ID".to_owned())?;
        let track_name = action["name"]
            .as_str()
            .filter(|track_name| !track_name.contains('\0'))
            .ok_or_else(|| "MIDI script returned an invalid Playlist track name".to_owned())?;
        return Ok(ScriptTransportAction::SetPlaylistTrackName {
            track_id,
            name: track_name.to_owned(),
        });
    }
    if name == Some("set_song_position") {
        let position = action["position"]
            .as_f64()
            .filter(|position| position.is_finite())
            .ok_or_else(|| "MIDI script returned an invalid song position".to_owned())?;
        let normalized = position.clamp(0.0, 1.0);
        let position = (normalized * SCRIPT_SONG_POSITION_SCALE).round() as u32;
        return Ok(ScriptTransportAction::SetSongPosition(
            position.min(SCRIPT_SONG_POSITION_SCALE_U32),
        ));
    }
    if name == Some("undo_history_jog") {
        let increment = action["increment"]
            .as_i64()
            .and_then(|increment| i32::try_from(increment).ok())
            .ok_or_else(|| "MIDI script returned an invalid undo-history increment".to_owned())?;
        return Ok(ScriptTransportAction::UndoHistoryJog(increment));
    }
    if name == Some("piano_roll_snap_jog") {
        let increment = action["increment"]
            .as_i64()
            .and_then(|increment| i32::try_from(increment).ok())
            .ok_or_else(|| "MIDI script returned an invalid snap-mode increment".to_owned())?;
        return Ok(ScriptTransportAction::PianoRollSnapJog(increment));
    }
    if name == Some("set_global_swing_mix") {
        let value_raw = action["value_raw"]
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= 128)
            .ok_or_else(|| "MIDI script returned an invalid global swing value".to_owned())?;
        return Ok(ScriptTransportAction::SetGlobalSwingMixRaw(value_raw));
    }
    if name == Some("set_song_position_ticks") {
        let position = action["position"]
            .as_u64()
            .ok_or_else(|| "MIDI script returned an invalid absolute song position".to_owned())?;
        return Ok(ScriptTransportAction::SetSongPositionTicks(position));
    }
    if name == Some("set_song_position_seconds") {
        let position = action["position_microseconds"]
            .as_u64()
            .ok_or_else(|| "MIDI script returned an invalid time song position".to_owned())?;
        return Ok(ScriptTransportAction::SetSongPositionTimeUs(position));
    }
    if name == Some("move_song_position") {
        let speed_millionths = action["speed_millionths"]
            .as_i64()
            .and_then(|speed| i32::try_from(speed).ok())
            .ok_or_else(|| "MIDI script returned an invalid song-position move speed".to_owned())?;
        let start = action["start"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid song-position move state".to_owned())?;
        return Ok(ScriptTransportAction::MoveSongPosition {
            speed_millionths,
            start,
        });
    }
    if name == Some("set_playback_speed") {
        let speed_millionths = action["speed_millionths"]
            .as_u64()
            .and_then(|speed| u32::try_from(speed).ok())
            .filter(|speed| (250_000..=4_000_000).contains(speed))
            .ok_or_else(|| "MIDI script returned an invalid playback speed".to_owned())?;
        return Ok(ScriptTransportAction::SetPlaybackSpeed(speed_millionths));
    }
    if name == Some("set_current_tempo") {
        let milli_bpm = action["tempo_milli_bpm"]
            .as_u64()
            .and_then(|tempo| u32::try_from(tempo).ok())
            .filter(|tempo| {
                (SCRIPT_MIN_TEMPO_MILLI_BPM..=SCRIPT_MAX_TEMPO_MILLI_BPM).contains(tempo)
            })
            .ok_or_else(|| "MIDI script returned an invalid project tempo".to_owned())?;
        return Ok(ScriptTransportAction::SetCurrentTempoMilliBpm(milli_bpm));
    }
    if name == Some("set_project_time_signature_numerator")
        || name == Some("set_project_time_signature_denominator")
    {
        let key = if name == Some("set_project_time_signature_numerator") {
            "numerator"
        } else {
            "denominator"
        };
        let value = action[key]
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| "MIDI script returned an invalid project time signature".to_owned())?;
        return Ok(if name == Some("set_project_time_signature_numerator") {
            ScriptTransportAction::SetProjectTimeSignatureNumerator(value)
        } else {
            ScriptTransportAction::SetProjectTimeSignatureDenominator(value)
        });
    }
    if name == Some("add_arrangement_time_marker") {
        let tick = action["time_ticks"]
            .as_u64()
            .and_then(|tick| u32::try_from(tick).ok())
            .filter(|tick| *tick <= SCRIPT_MAX_ARRANGEMENT_MARKER_TICK)
            .ok_or_else(|| {
                "MIDI script returned an invalid Playlist time-marker position".to_owned()
            })?;
        let marker_name = action["name"]
            .as_str()
            .filter(|name| !name.contains('\0'))
            .ok_or_else(|| {
                "MIDI script returned an invalid Playlist time-marker name".to_owned()
            })?;
        let stop_playback = action
            .get("stop_playback")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        return Ok(ScriptTransportAction::AddArrangementTimeMarker {
            tick,
            name: marker_name.to_owned(),
            stop_playback,
        });
    }
    if name == Some("set_arrangement_live_selection") {
        let tick = action["time_ticks"]
            .as_u64()
            .and_then(|tick| u32::try_from(tick).ok())
            .ok_or_else(|| "MIDI script returned an invalid live-selection position".to_owned())?;
        let stop = action["stop"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid live-selection endpoint".to_owned())?;
        return Ok(ScriptTransportAction::SetArrangementLiveSelection { tick, stop });
    }
    if name == Some("set_mixer_track_name") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let track_name = action["name"]
            .as_str()
            .filter(|track_name| !track_name.contains('\0'))
            .ok_or_else(|| "MIDI script returned an invalid Mixer track name".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerTrackName {
            index,
            name: track_name.to_owned(),
        });
    }
    if name == Some("set_mixer_track_selection") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let flags = action["flags"]
            .as_u64()
            .and_then(|flags| u32::try_from(flags).ok())
            .ok_or_else(|| "MIDI script returned invalid Mixer track selection flags".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerTrackSelection { index, flags });
    }
    if name == Some("select_channel") {
        let channel_id = action["channel_id"]
            .as_u64()
            .and_then(|channel_id| u16::try_from(channel_id).ok())
            .ok_or_else(|| "MIDI script returned an invalid Channel Rack channel ID".to_owned())?;
        return Ok(ScriptTransportAction::SelectChannel(channel_id));
    }
    if name == Some("set_mixer_track_color") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let color = action["color"]
            .as_u64()
            .and_then(|color| u32::try_from(color).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track color".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerTrackColor { index, color });
    }
    if name == Some("set_mixer_slot_color") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let slot = action["slot"]
            .as_u64()
            .and_then(|slot| u8::try_from(slot).ok())
            .filter(|slot| *slot < 10)
            .ok_or_else(|| "MIDI script returned an invalid Mixer effect slot".to_owned())?;
        let color = action["color"]
            .as_u64()
            .and_then(|color| u32::try_from(color).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer effect-slot color".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerSlotColor { index, slot, color });
    }
    if name == Some("set_mixer_track_enabled") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let enabled = action["enabled"].as_bool().ok_or_else(|| {
            "MIDI script returned an invalid Mixer track enabled state".to_owned()
        })?;
        return Ok(ScriptTransportAction::SetMixerTrackEnabled { index, enabled });
    }
    if name == Some("set_mixer_track_flag") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let flag = match action["flag"].as_str() {
            Some("soloed") => MixerTrackFlag::Soloed,
            Some("polarity_reversed") => MixerTrackFlag::PolarityReversed,
            Some("swap_left_right") => MixerTrackFlag::SwapLeftRight,
            Some("effects_enabled") => MixerTrackFlag::EffectsEnabled,
            _ => return Err("MIDI script returned an invalid Mixer track flag".to_owned()),
        };
        let value = action["value"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid Mixer track flag value".to_owned())?;
        let solo_mode = if flag == MixerTrackFlag::Soloed {
            match action.get("solo_mode") {
                None => None,
                Some(mode) => match mode.as_i64() {
                    Some(-1) => None,
                    Some(1..=4) => Some(mode.as_u64().expect("positive solo mode") as u8),
                    _ => return Err("MIDI script returned an invalid Mixer solo mode".to_owned()),
                },
            }
        } else {
            None
        };
        return Ok(ScriptTransportAction::SetMixerTrackFlag {
            index,
            flag,
            value,
            solo_mode,
        });
    }
    if name == Some("set_mixer_track_parameter") {
        let index = action["index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let parameter = match action["parameter"].as_str() {
            Some("volume") => MixerTrackParameter::Volume,
            Some("pan") => MixerTrackParameter::Pan,
            Some("stereo_separation") => MixerTrackParameter::StereoSeparation,
            _ => return Err("MIDI script returned an invalid Mixer track parameter".to_owned()),
        };
        let value_raw = action["value_raw"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer parameter value".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerTrackParameter {
            index,
            parameter,
            value_raw,
        });
    }
    if name == Some("set_mixer_route") {
        let source_index = action["source_index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer source track index".to_owned())?;
        let destination_index = action["destination_index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| {
                "MIDI script returned an invalid Mixer destination track index".to_owned()
            })?;
        let enabled = action["enabled"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid Mixer route state".to_owned())?;
        let update_ui = action["update_ui"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid Mixer route UI flag".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerRoute {
            source_index,
            destination_index,
            enabled,
            update_ui,
        });
    }
    if name == Some("set_mixer_route_level") {
        let source_index = action["source_index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer source track index".to_owned())?;
        let destination_index = action["destination_index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| {
                "MIDI script returned an invalid Mixer destination track index".to_owned()
            })?;
        let value_raw = action["value_raw"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| "MIDI script returned an invalid Mixer route level".to_owned())?;
        return Ok(ScriptTransportAction::SetMixerRouteLevel {
            source_index,
            destination_index,
            value_raw,
        });
    }
    if name == Some("link_channel_to_mixer_track") {
        let channel_index = action["channel_index"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok())
            .ok_or_else(|| "MIDI script returned an invalid Channel Rack index".to_owned())?;
        let track_index = action["track_index"]
            .as_u64()
            .and_then(|index| u8::try_from(index).ok())
            .filter(|index| *index <= 127)
            .ok_or_else(|| "MIDI script returned an invalid Mixer track index".to_owned())?;
        let select = action["select"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid Mixer selection flag".to_owned())?;
        return Ok(ScriptTransportAction::LinkChannelToMixerTrack {
            channel_index,
            track_index,
            select,
        });
    }
    if name == Some("jump_to_marker") {
        let delta = action["delta"]
            .as_i64()
            .and_then(|delta| i32::try_from(delta).ok())
            .ok_or_else(|| "MIDI script returned an invalid marker jump delta".to_owned())?;
        let select = action["select"]
            .as_bool()
            .ok_or_else(|| "MIDI script returned an invalid marker selection flag".to_owned())?;
        return Ok(ScriptTransportAction::JumpToMarker { delta, select });
    }
    if name == Some("marker_jump_jog") || name == Some("marker_sel_jog") {
        let increment = action["increment"]
            .as_i64()
            .and_then(|increment| i32::try_from(increment).ok())
            .ok_or_else(|| "MIDI script returned an invalid marker jog increment".to_owned())?;
        return Ok(if name == Some("marker_jump_jog") {
            ScriptTransportAction::MarkerJumpJog(increment)
        } else {
            ScriptTransportAction::MarkerSelJog(increment)
        });
    }
    if name == Some("pattern_jog") {
        let increment = action["increment"]
            .as_i64()
            .and_then(|increment| i32::try_from(increment).ok())
            .ok_or_else(|| "MIDI script returned an invalid pattern jog increment".to_owned())?;
        return Ok(ScriptTransportAction::PatternJog(increment));
    }
    if name == Some("tempo_jog") {
        let increment = action["increment"]
            .as_i64()
            .and_then(|increment| i32::try_from(increment).ok())
            .ok_or_else(|| "MIDI script returned an invalid tempo jog increment".to_owned())?;
        return Ok(ScriptTransportAction::TempoJog(increment));
    }
    if name == Some("show_main_view") {
        let view = action["view"]
            .as_u64()
            .and_then(|view| u8::try_from(view).ok())
            .filter(|view| *view <= 4)
            .ok_or_else(|| "MIDI script returned an invalid main view".to_owned())?;
        return Ok(ScriptTransportAction::ShowMainView(view));
    }
    if name == Some("toggle_audio_settings_window") {
        return Ok(ScriptTransportAction::ToggleAudioSettingsWindow);
    }
    if name == Some("toggle_project_info") {
        return Ok(ScriptTransportAction::ToggleProjectInfo);
    }
    if name == Some("close_all_windows") {
        return Ok(ScriptTransportAction::CloseAllWindows);
    }
    if name == Some("midi_out_message") || name == Some("midi_out_sysex") {
        let message = action["bytes"]
            .as_array()
            .filter(|message| message.len() <= 65_536)
            .ok_or_else(|| "MIDI script returned an invalid output message".to_owned())?;
        let bytes = message
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| "MIDI script returned an invalid output byte".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if name == Some("midi_out_sysex") {
            if bytes.len() < 2
                || bytes.first() != Some(&0xF0)
                || bytes.last() != Some(&0xF7)
                || bytes[1..bytes.len() - 1].iter().any(|byte| *byte > 0x7F)
            {
                return Err("MIDI script returned incomplete SysEx output".to_owned());
            }
        } else if !valid_short_midi_output(&bytes) {
            return Err("MIDI script returned an invalid short MIDI output message".to_owned());
        }
        return Ok(ScriptTransportAction::MidiOutput(bytes));
    }
    if name == Some("repeat_midi_event") {
        let message = action["bytes"]
            .as_array()
            .filter(|message| message.len() <= 65_536)
            .ok_or_else(|| "MIDI script returned an invalid repeat message".to_owned())?;
        let bytes = message
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| "MIDI script returned an invalid output byte".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if bytes.first() == Some(&0xF0) {
            if bytes.len() < 2
                || bytes.last() != Some(&0xF7)
                || bytes[1..bytes.len() - 1].iter().any(|byte| *byte > 0x7F)
            {
                return Err("MIDI script returned incomplete repeated SysEx output".to_owned());
            }
        } else if !valid_short_midi_output(&bytes) {
            return Err("MIDI script returned an invalid repeated MIDI output message".to_owned());
        }
        let delay_ms = action["delay_ms"]
            .as_u64()
            .and_then(|delay| u32::try_from(delay).ok())
            .ok_or_else(|| "MIDI script returned an invalid repeat delay".to_owned())?;
        let rate_ms = action["rate_ms"]
            .as_u64()
            .and_then(|rate| u32::try_from(rate).ok())
            .filter(|rate| *rate > 0)
            .ok_or_else(|| "MIDI script returned an invalid repeat rate".to_owned())?;
        return Ok(ScriptTransportAction::RepeatMidiOutput {
            bytes,
            delay_ms,
            rate_ms,
        });
    }
    if name == Some("stop_repeat_midi_event") {
        return Ok(ScriptTransportAction::StopRepeatMidiOutput);
    }
    Err("MIDI script returned an unknown transport action".to_owned())
}

fn valid_short_midi_output(bytes: &[u8]) -> bool {
    let Some(&status) = bytes.first() else {
        return false;
    };
    short_midi_output_length(status).is_some_and(|expected_length| {
        bytes.len() == expected_length && bytes.iter().skip(1).all(|byte| *byte <= 0x7F)
    })
}

fn short_midi_output_length(status: u8) -> Option<usize> {
    match status {
        0x80..=0xEF if matches!(status & 0xF0, 0xC0 | 0xD0) => Some(2),
        0x80..=0xEF => Some(3),
        0xF1 | 0xF3 => Some(2),
        0xF2 => Some(3),
        0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE..=0xFF => Some(1),
        _ => None,
    }
}

fn transform_midi_output_message(original: &[u8], response: &Value) -> Result<Vec<u8>, String> {
    let status = byte_value(response, "status", original[0])?;
    let length = short_midi_output_length(status)
        .ok_or_else(|| "MIDI script returned an invalid short-message status".to_owned())?;
    let mut transformed = Vec::with_capacity(length);
    transformed.push(status);
    for (index, key) in ["data1", "data2"].into_iter().enumerate().take(length - 1) {
        let default = original.get(index + 1).copied().unwrap_or(0);
        let data = byte_value(response, key, default)?;
        if data > 0x7F {
            return Err(format!("MIDI script returned {key} outside 0–127"));
        }
        transformed.push(data);
    }
    Ok(transformed)
}

fn transform_message(original: &[u8], response: &Value) -> Result<Vec<u8>, String> {
    let status = byte_value(response, "status", original[0])?;
    if status == 0xF0 {
        let sysex = response["sysex"]
            .as_array()
            .ok_or_else(|| "MIDI script returned invalid SysEx data".to_owned())?;
        let mut bytes = Vec::with_capacity(sysex.len());
        for value in sysex {
            bytes.push(
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| "MIDI script returned an invalid SysEx byte".to_owned())?,
            );
        }
        if bytes.len() < 2 || bytes.first() != Some(&0xF0) || bytes.last() != Some(&0xF7) {
            return Err("MIDI script returned incomplete SysEx data".to_owned());
        }
        return Ok(bytes);
    }
    if !(0x80..=0xEF).contains(&status) {
        if status != original[0] {
            return Err("MIDI scripts can only remap channel-voice status bytes".to_owned());
        }
        return Ok(original.to_vec());
    }
    let data1 = byte_value(response, "data1", original.get(1).copied().unwrap_or(0))?;
    let data2 = byte_value(response, "data2", original.get(2).copied().unwrap_or(0))?;
    if data1 > 0x7F || data2 > 0x7F {
        return Err("MIDI script returned a data byte outside 0–127".to_owned());
    }
    let kind = status & 0xF0;
    let mut transformed = vec![status, data1];
    if !matches!(kind, 0xC0 | 0xD0) {
        transformed.push(data2);
    }
    Ok(transformed)
}

fn byte_value(response: &Value, key: &str, default: u8) -> Result<u8, String> {
    response[key]
        .as_u64()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(|| format!("MIDI script returned an invalid {key} value"))
        .or_else(|error| {
            if response[key].is_null() {
                Ok(default)
            } else {
                Err(error)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::{
        MidiControllerScript, MidiScriptRuntime, MidiScriptTransportState, MixerTrackFlag,
        SCRIPT_BEAT_INDICATOR_BAR, SCRIPT_BEAT_INDICATOR_BEAT, SCRIPT_BEAT_INDICATOR_OFF,
        SCRIPT_TRANSPORT_PATTERN_MODE, SCRIPT_TRANSPORT_PLAYING, SCRIPT_TRANSPORT_RECORDING,
        ScriptTransportAction, absolute_song_position_ticks, discover_midi_scripts,
        find_script_for_device, normalized_song_position, song_position_ticks, transform_message,
    };
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fl-midi-script-test-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovers_scripts_and_reads_supported_device_headers() {
        let temporary = TestDirectory::new();
        let folder = temporary.path().join("Controller");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("device_example.py"),
            "# name=Example Controller\n# url=https://forum.image-line.com/viewtopic.php?id=1\n# supportedDevices=Example, Example XL\n# supportedHardwareIds=00 01,00 02\n\ndef OnMidiIn(event):\n    pass\n",
        )
        .unwrap();
        fs::write(folder.join("ignored.py"), "# name=Ignored\n").unwrap();
        fs::write(
            folder.join("device_invalid.py"),
            "def OnMidiIn(event): pass\n",
        )
        .unwrap();

        let scripts = discover_midi_scripts(temporary.path()).unwrap();

        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "Example Controller");
        assert_eq!(
            scripts[0].url.as_deref(),
            Some("https://forum.image-line.com/viewtopic.php?id=1")
        );
        assert_eq!(scripts[0].supported_devices, ["Example", "Example XL"]);
        assert_eq!(scripts[0].supported_hardware_ids, ["00 01", "00 02"]);
    }

    #[test]
    fn supported_device_names_match_case_and_spacing_but_reject_ambiguity() {
        let temporary = TestDirectory::new();
        let folder = temporary.path().join("Controller");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("device_first.py"),
            "# name=First\n# supportedDevices=Example Controller\n",
        )
        .unwrap();
        let scripts = discover_midi_scripts(temporary.path()).unwrap();

        assert_eq!(
            find_script_for_device(&scripts, "  EXAMPLE   controller ")
                .map(|script| script.name.as_str()),
            Some("First")
        );
        assert!(find_script_for_device(&scripts, "Other Controller").is_none());
        assert!(find_script_for_device(&scripts, " ").is_none());

        fs::write(
            folder.join("device_second.py"),
            "# name=Second\n# supportedDevices=example controller\n",
        )
        .unwrap();
        let scripts = discover_midi_scripts(temporary.path()).unwrap();
        assert!(find_script_for_device(&scripts, "Example Controller").is_none());
    }

    #[test]
    fn normalized_song_position_handles_bounds_and_missing_lengths() {
        assert_eq!(normalized_song_position(24.0, 96), 0.25);
        assert_eq!(normalized_song_position(-1.0, 96), 0.0);
        assert_eq!(normalized_song_position(120.0, 96), 1.0);
        assert_eq!(normalized_song_position(24.0, 0), 0.0);
        assert_eq!(normalized_song_position(f64::NAN, 96), 0.0);
    }

    #[test]
    fn normalized_song_position_maps_to_ticks_and_clamps_inputs() {
        assert_eq!(song_position_ticks(0, 96), 0);
        assert_eq!(song_position_ticks(250_000, 96), 24);
        assert_eq!(song_position_ticks(1_000_000, 96), 96);
        assert_eq!(song_position_ticks(2_000_000, 96), 96);
        assert_eq!(song_position_ticks(500_000, 0), 0);
        assert_eq!(absolute_song_position_ticks(24.4), 24);
        assert_eq!(absolute_song_position_ticks(-1.0), 0);
        assert_eq!(absolute_song_position_ticks(f64::INFINITY), 0);
    }

    #[test]
    fn python_callbacks_can_remap_and_suppress_input_events() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport device\ndef OnMidiIn(event):\n    if event.status & 0xF0 == 0xB0:\n        event.controlVal = 127 - event.controlVal\ndef OnNoteOn(event):\n    event.note = 64\ndef OnNoteOff(event):\n    event.handled = True\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();
        assert!(!runtime.supports_midi_output());

        let controller = runtime.process_event(12, &[0xB2, 7, 25]).unwrap();
        let note_on = runtime.process_event(20, &[0x92, 60, 90]).unwrap();
        let note_off = runtime.process_event(40, &[0x82, 60, 64]).unwrap();

        assert_eq!(controller.bytes, [0xB2, 7, 102]);
        assert_eq!(note_on.bytes, [0x92, 64, 90]);
        assert!(note_off.handled);
    }

    #[test]
    fn python_midi_out_callback_can_edit_messages_and_reads_output_port() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\ndef OnMidiOutMsg(event):\n    if event.port != 3:\n        raise RuntimeError('wrong output port')\n    event.midiId = 0xB0\n    event.midiChan = 2\n    event.data1 = 7\n    event.data2 = 99\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .midi_output_port_number
            .store(3, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();
        assert!(runtime.supports_midi_output());

        let result = runtime.process_midi_output(&[0x91, 60, 42]).unwrap();

        assert!(!result.handled);
        assert_eq!(result.bytes, [0xB2, 7, 99]);
    }

    #[test]
    fn python_midi_out_callback_can_consume_messages_and_queue_script_output() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnMidiOutMsg(event):\n    device.midiOutMsg(0xB0, 0, 1, 2)\n    event.handled = event.data1 == 60\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_midi_output(&[0x91, 60, 42]).unwrap();

        assert!(result.handled);
        assert!(result.bytes.is_empty());
        assert_eq!(
            result.actions,
            [ScriptTransportAction::MidiOutput(vec![0xB0, 1, 2])]
        );
    }

    #[test]
    fn python_transport_calls_are_returned_even_when_the_event_is_handled() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    transport.start()\n    transport.record()\n    transport.setLoopMode()\n    transport.setSongPos(0.75)\n    transport.setSongPos(48, 2)\n    event.handled = True\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 127]).unwrap();

        assert!(result.handled);
        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::Start,
                ScriptTransportAction::RecordToggle,
                ScriptTransportAction::ToggleLoopMode,
                ScriptTransportAction::SetSongPosition(750_000),
                ScriptTransportAction::SetSongPositionTicks(48),
            ]
        );
    }

    #[test]
    fn python_playlist_selection_uses_one_based_track_indices_and_toggle_state() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport playlist\ndef OnMidiIn(event):\n    if playlist.trackCount() != 3:\n        raise RuntimeError('wrong Playlist track count')\n    if not playlist.isTrackSelected(2):\n        raise RuntimeError('track 2 should start selected')\n    playlist.selectTrack(2)\n    if playlist.isTrackSelected(2) or not playlist.isTrackSelected(3):\n        raise RuntimeError('selectTrack should toggle one-based track 2')\n    playlist.selectTrack(1)\n    if not playlist.isTrackSelected(1) or not playlist.isTrackSelected(3):\n        raise RuntimeError('track selection should preserve other selected rows')\n    playlist.selectAll()\n    if not all(playlist.isTrackSelected(index) for index in range(1, 4)):\n        raise RuntimeError('selectAll should select every Playlist row')\n    playlist.deselectAll()\n    if any(playlist.isTrackSelected(index) for index in range(1, 4)):\n        raise RuntimeError('deselectAll should clear Playlist row selection')\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .playlist_track_count
            .store(3, Ordering::Relaxed);
        *transport_state.selected_playlist_track_ids.lock().unwrap() = vec![2, 3];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 127]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetPlaylistTrackSelection(vec![3]),
                ScriptTransportAction::SetPlaylistTrackSelection(vec![1, 3]),
                ScriptTransportAction::SetPlaylistTrackSelection(vec![1, 2, 3]),
                ScriptTransportAction::SetPlaylistTrackSelection(Vec::new()),
            ]
        );
    }

    #[test]
    fn python_playlist_track_names_use_one_based_indices_and_queue_edits() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport playlist\ndef OnMidiIn(event):\n    if playlist.getTrackName(1) != 'Drums':\n        raise RuntimeError('Playlist track name was not read')\n    if playlist.getTrackName(2) != 'Track 2':\n        raise RuntimeError('unnamed Playlist row should use its default label')\n    playlist.setTrackName(2, 'Bass')\n    playlist.setTrackName(2, '')\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .playlist_track_count
            .store(2, Ordering::Relaxed);
        *transport_state.playlist_track_names.lock().unwrap() =
            vec!["Drums".to_owned(), "".to_owned()];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 127]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetPlaylistTrackName {
                    track_id: 2,
                    name: "Bass".to_owned(),
                },
                ScriptTransportAction::SetPlaylistTrackName {
                    track_id: 2,
                    name: String::new(),
                },
            ]
        );
    }

    #[test]
    fn python_device_name_reports_the_connected_input_device() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnMidiIn(event):\n    event.data1 = ord(device.getName()[0])\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "MIDI Keyboard").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 127]).unwrap();

        assert_eq!(result.bytes, [0xB0, b'M', 127]);
    }

    #[test]
    fn python_hardware_beat_led_query_reports_the_current_indicator_state() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = transport.getHWBeatLEDState()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let stopped = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .hardware_beat_led_state
            .store(SCRIPT_BEAT_INDICATOR_BAR, Ordering::Relaxed);
        let bar = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();
        transport_state
            .hardware_beat_led_state
            .store(SCRIPT_BEAT_INDICATOR_BEAT, Ordering::Relaxed);
        let beat = runtime.process_event(28, &[0xB0, 7, 1]).unwrap();

        assert_eq!(stopped.bytes, [0xB0, 0, 1]);
        assert_eq!(bar.bytes, [0xB0, 1, 1]);
        assert_eq!(beat.bytes, [0xB0, 2, 1]);
    }

    #[test]
    fn python_general_project_info_getters_report_loaded_metadata() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport general\ndef OnMidiIn(event):\n    event.data1 = len(general.getProjectTitle())\n    event.data2 = len(general.getProjectAuthor()) + len(general.getProjectGenre())\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.project_title.lock().unwrap() = "Album".to_owned();
        *transport_state.project_author.lock().unwrap() = "Artist".to_owned();
        *transport_state.project_genre.lock().unwrap() = "Jazz".to_owned();
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 5, 10]);
    }

    #[test]
    fn python_general_version_and_changed_flag_getters_report_runtime_state() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport general\ndef OnMidiIn(event):\n    event.data1 = general.getVersion()\n    event.data2 = general.getChangedFlag()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        transport_state
            .project_changed_flag
            .store(1, Ordering::Relaxed);
        let dirty = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .project_changed_flag
            .store(0, Ordering::Relaxed);
        let clean = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(dirty.bytes, [0xB0, 1, 1]);
        assert_eq!(clean.bytes, [0xB0, 1, 0]);
    }

    #[test]
    fn python_general_recording_timebase_getters_report_loaded_project_values() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport general\ndef OnMidiIn(event):\n    event.data1 = general.getRecPPQ()\n    event.data2 = int(general.getRecPPB() == 672)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .ticks_per_quarter
            .store(96, Ordering::Relaxed);
        transport_state
            .time_signature_numerator
            .store(7, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 96, 1]);
    }

    #[test]
    fn python_general_time_signature_setters_emit_project_edit_actions() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport general\ndef OnMidiIn(event):\n    general.setNumerator(7)\n    general.setDenominator(8)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetProjectTimeSignatureNumerator(7),
                ScriptTransportAction::SetProjectTimeSignatureDenominator(8),
            ]
        );
    }

    #[test]
    fn python_device_midi_out_message_emits_packed_channel_voice_bytes() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnMidiIn(event):\n    event.data2 = device.isAssigned()\n    device.midiOutNewMsg(3, 0x00643C90)\n    device.midiOutNewMsg(3, 0x00643C90)\n    device.directFeedback(event)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .midi_output_port_number
            .store(0, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 0]).unwrap();

        assert_eq!(result.bytes, [0xB0, 7, 1]);
        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::MidiOutput(vec![0x90, 60, 100]),
                ScriptTransportAction::MidiOutput(vec![0xB0, 7, 1]),
            ]
        );
    }

    #[test]
    fn python_device_assignment_defaults_to_unassigned() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnMidiIn(event):\n    event.data1 = device.getPortNumber() + 1\n    event.data2 = device.isAssigned()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 0, 0]).unwrap();

        assert_eq!(result.bytes, [0xB0, 0, 0]);
    }

    #[test]
    fn python_device_on_init_sends_channel_voice_and_sysex_messages() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnInit():\n    if device.isAssigned() and device.getPortNumber() == 0:\n        device.midiOutMsg(0x90, 2, 60, 100)\n        device.midiOutSysex(bytes([0xF0, 0x7D, 0x01, 0xF7]))\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .midi_output_port_number
            .store(0, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        assert_eq!(
            runtime.take_startup_actions(),
            [
                ScriptTransportAction::MidiOutput(vec![0x92, 60, 100]),
                ScriptTransportAction::MidiOutput(vec![0xF0, 0x7D, 0x01, 0xF7]),
            ]
        );
    }

    #[test]
    fn python_device_schedules_event_repeats_and_stops_them() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\nidle_calls = 0\ndef OnMidiIn(event):\n    device.repeatMidiEvent(event, 0, 10)\ndef OnIdle():\n    global idle_calls\n    idle_calls += 1\n    if idle_calls == 2:\n        device.stopRepeatMidiEvent()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let event = runtime.process_event(12, &[0xB0, 7, 99]).unwrap();
        let first_idle = runtime.process_idle().unwrap();
        let second_idle = runtime.process_idle().unwrap();

        assert_eq!(
            event.actions,
            [ScriptTransportAction::RepeatMidiOutput {
                bytes: vec![0xB0, 7, 99],
                delay_ms: 0,
                rate_ms: 10,
            }]
        );
        assert!(first_idle.actions.is_empty());
        assert_eq!(
            second_idle.actions,
            [ScriptTransportAction::StopRepeatMidiOutput]
        );
    }

    #[test]
    fn python_device_meter_updates_are_opt_in_and_read_live_mixer_peaks() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\nimport midi\nimport mixer\nimport transport\ndef OnInit():\n    device.setHasMeters()\ndef OnUpdateMeters():\n    if mixer.getTrackPeaks(1, midi.PEAK_R) >= 0.75:\n        transport.stop()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.mixer_track_peaks.lock().unwrap() = vec![(0.0, 0.0), (0.25, 0.75)];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_idle().unwrap();

        assert_eq!(result.actions, [ScriptTransportAction::Stop]);
    }

    #[test]
    fn python_device_on_deinit_emits_final_midi_output() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnDeInit():\n    device.midiOutMsg(0x00643C90)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .midi_output_port_number
            .store(0, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let actions = runtime.shutdown().unwrap();

        assert_eq!(
            actions,
            [ScriptTransportAction::MidiOutput(vec![0x90, 60, 100])]
        );
    }

    #[test]
    fn python_transport_queries_report_live_playback_recording_and_loop_state() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = transport.isPlaying() * 64 + transport.isRecording() * 32 + transport.getLoopMode() * 16\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state.state.store(
            SCRIPT_TRANSPORT_PLAYING | SCRIPT_TRANSPORT_RECORDING | SCRIPT_TRANSPORT_PATTERN_MODE,
            Ordering::Relaxed,
        );
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let pattern_mode = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .state
            .store(SCRIPT_TRANSPORT_PLAYING, Ordering::Relaxed);
        let song_mode = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(pattern_mode.bytes, [0xB0, 96, 1]);
        assert_eq!(song_mode.bytes, [0xB0, 80, 1]);
    }

    #[test]
    fn python_transport_song_position_reports_a_live_normalized_value() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = int(transport.getSongPos() * 127)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .song_position
            .store(0.25_f64.to_bits(), Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let first = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .song_position
            .store(0.5_f64.to_bits(), Ordering::Relaxed);
        let second = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(first.bytes, [0xB0, 31, 1]);
        assert_eq!(second.bytes, [0xB0, 63, 1]);
    }

    #[test]
    fn python_transport_song_position_reports_live_absolute_ticks() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = transport.getSongPos(2)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .song_position_ticks
            .store(24, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let first = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .song_position_ticks
            .store(48, Ordering::Relaxed);
        let second = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(first.bytes, [0xB0, 24, 1]);
        assert_eq!(second.bytes, [0xB0, 48, 1]);
    }

    #[test]
    fn python_mixer_song_position_getters_report_ticks_beats_and_steps() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = int(mixer.getSongTickPos(midi.ST_Int) == 144 and mixer.getSongTickPos(midi.ST_Beat) == 1.5)\n    event.data2 = int(mixer.getSongStepPos() == 6)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .song_position_ticks
            .store(144, Ordering::Relaxed);
        transport_state
            .ticks_per_quarter
            .store(96, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
    }

    #[test]
    fn python_mixer_peak_getters_report_stereo_insert_and_master_levels() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport mixer\ndef OnMidiIn(event):\n    assert mixer.getTrackPeaks(2, midi.PEAK_L) == 0.25\n    assert mixer.getTrackPeaks(2, midi.PEAK_R) == 0.5\n    assert mixer.getTrackPeaks(2, midi.PEAK_LR) == 0.5\n    assert mixer.getTrackPeaks(0, midi.PEAK_L) == 0.75\n    assert mixer.getLastPeakVol(0) == 0.75\n    assert mixer.getLastPeakVol(1) == 0.875\n    assert mixer.getTrackPeaks(127, midi.PEAK_LR) == 0.0\n    event.data1 = int(mixer.getTrackPeaks(2, midi.PEAK_LR) * 100)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.mixer_track_peaks.lock().unwrap() =
            vec![(0.75, 0.875), (0.0, 0.0), (0.25, 0.5)];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 50, 1]);
    }

    #[test]
    fn python_mixer_solo_modes_are_forwarded_to_the_live_mixer() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    mixer.soloTrack(2, 1, 3)\n    event.data1 = int(mixer.isTrackSolo(2))\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::new(MidiScriptTransportState::default()),
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
        assert_eq!(
            result.actions,
            vec![ScriptTransportAction::SetMixerTrackFlag {
                index: 2,
                flag: MixerTrackFlag::Soloed,
                value: true,
                solo_mode: Some(3),
            }]
        );
    }

    #[test]
    fn python_mixer_solo_rejects_unknown_modes() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    mixer.soloTrack(2, 1, 0)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::new(MidiScriptTransportState::default()),
        )
        .unwrap();

        assert!(
            runtime
                .process_event(12, &[0xB0, 7, 1])
                .unwrap_err()
                .contains("Mixer solo mode must be -1, 1, 2, 3, or 4")
        );
    }

    #[test]
    fn python_mixer_track_count_includes_master_current_and_insert_range() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = mixer.trackCount()\n    event.data2 = mixer.getTrackInfo(midi.TN_LastIns)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let empty_mixer = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        *transport_state.mixer_track_names.lock().unwrap() =
            vec!["Master".to_owned(), "Drums".to_owned(), "Bass".to_owned()];
        let populated_mixer = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(empty_mixer.bytes, [0xB0, 2, 1]);
        assert_eq!(populated_mixer.bytes, [0xB0, 4, 2]);
    }

    #[test]
    fn python_mixer_get_track_count_excludes_master_and_current() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = mixer.getTrackCount()\n    event.data2 = mixer.trackCount()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let empty_mixer = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        *transport_state.mixer_track_names.lock().unwrap() =
            vec!["Master".to_owned(), "Drums".to_owned(), "Bass".to_owned()];
        let populated_mixer = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(empty_mixer.bytes, [0xB0, 0, 2]);
        assert_eq!(populated_mixer.bytes, [0xB0, 2, 4]);
    }

    #[test]
    fn python_mixer_track_name_honors_optional_max_length() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    name = mixer.getTrackName(1, 3)\n    assert name == 'Pia'\n    assert mixer.getTrackName(1, 0) == ''\n    assert mixer.getTrackName(1) == 'Piano'\n    assert mixer.getTrackName(8, 2) == 'In'\n    assert mixer.getTrackName(1, -2) == 'Piano'\n    event.data1 = len(name)\n    event.data2 = ord(mixer.getTrackName(8, 2)[0])\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.mixer_track_names.lock().unwrap() =
            vec!["Master".to_owned(), "Piano".to_owned()];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 3, b'I']);
    }

    #[test]
    fn python_mixer_get_track_recording_file_name_reads_live_snapshot() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = int(mixer.getTrackRecordingFileName(1) == '/tmp/audio-take.wav')\n    event.data2 = int(mixer.getTrackRecordingFileName(0) == '')\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let idle = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        *transport_state
            .mixer_track_recording_file_names
            .lock()
            .unwrap() = vec![String::new(), "/tmp/audio-take.wav".to_owned()];
        let recording = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(idle.bytes, [0xB0, 0, 1]);
        assert_eq!(recording.bytes, [0xB0, 1, 1]);
    }

    #[test]
    fn python_mixer_track_dock_side_matches_centered_workspace() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = mixer.getTrackDockSide(0)\n    event.data2 = mixer.getTrackDockSide(127)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
    }

    #[test]
    fn python_mixer_track_mute_lock_reads_saved_insert_flags() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = int(mixer.isTrackMuteLock(1))\n    event.data2 = int(mixer.isTrackMuteLock(2))\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.mixer_track_mute_locked.lock().unwrap() =
            vec![None, Some(true), Some(false)];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 0]);
    }

    #[test]
    fn python_mixer_current_tempo_returns_fractional_or_integer_bpm() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    event.data1 = int(mixer.getCurrentTempo() * 1000) % 128\n    event.data2 = mixer.getCurrentTempo(asInt=1)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .current_tempo_milli_bpm
            .store(123_456, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 64, 123]);
    }

    #[test]
    fn python_transport_song_length_reports_live_absolute_ticks() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = transport.getSongLength(2)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .song_length_ticks
            .store(96, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let first = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        transport_state
            .song_length_ticks
            .store(120, Ordering::Relaxed);
        let second = runtime.process_event(20, &[0xB0, 7, 1]).unwrap();

        assert_eq!(first.bytes, [0xB0, 96, 1]);
        assert_eq!(second.bytes, [0xB0, 120, 1]);
    }

    #[test]
    fn python_transport_time_modes_report_live_song_position_and_length() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    event.data1 = int(transport.getSongPos(midi.SONGLENGTH_MS) == 1234.5 and transport.getSongPos(midi.SONGLENGTH_S) == 1.2345)\n    event.data2 = int(transport.getSongLength(midi.SONGLENGTH_MS) == 2345 and transport.getSongLength(midi.SONGLENGTH_S) == 2)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .song_position_seconds
            .store(1.2345_f64.to_bits(), Ordering::Relaxed);
        transport_state
            .song_length_seconds
            .store(2.3454_f64.to_bits(), Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
    }

    #[test]
    fn python_transport_bst_modes_report_position_and_length_components() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    event.data1 = int((transport.getSongPos(midi.SONGLENGTH_BARS), transport.getSongPos(midi.SONGLENGTH_STEPS), transport.getSongPos(midi.SONGLENGTH_TICKS)) == (5, 3, 17))\n    event.data2 = int((transport.getSongLength(midi.SONGLENGTH_BARS), transport.getSongLength(midi.SONGLENGTH_STEPS), transport.getSongLength(midi.SONGLENGTH_TICKS)) == (4, 2, 9))\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.song_position_hint.lock().unwrap() = "5:03:017".to_owned();
        *transport_state.song_length_hint.lock().unwrap() = "4:02:009".to_owned();
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
    }

    #[test]
    fn python_transport_bst_song_position_setting_remains_unsupported() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    try:\n        transport.setSongPos(2, midi.SONGLENGTH_BARS)\n    except NotImplementedError:\n        event.data1 = 127\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 127, 1]);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn python_transport_time_modes_seek_with_millisecond_precision() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    transport.setSongPos(1500.25, midi.SONGLENGTH_MS)\n    transport.setSongPos(2.5, midi.SONGLENGTH_S)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetSongPositionTimeUs(1_500_250),
                ScriptTransportAction::SetSongPositionTimeUs(2_500_000),
            ]
        );
    }

    #[test]
    fn python_transport_song_position_hint_reports_the_live_position_label() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    event.data1 = int(transport.getSongPosHint() == '2:03:004') * 127\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.song_position_hint.lock().unwrap() = "2:03:004".to_owned();
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            Arc::clone(&transport_state),
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 127, 1]);
    }

    #[test]
    fn python_transport_marker_jogs_request_global_marker_actions() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    transport.markerJumpJog(2)\n    transport.markerSelJog(-1)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::MarkerJumpJog(2),
                ScriptTransportAction::MarkerSelJog(-1),
            ]
        );
    }

    #[test]
    fn python_global_transport_dispatches_supported_global_commands() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    if event.pmeflags == midi.PME_System | midi.PME_FromMIDI:\n        play = transport.globalTransport(midi.FPT_Play, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Stop, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Record, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Loop, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_MarkerJumpJog, 2, event.pmeflags)\n        transport.globalTransport(midi.FPT_MarkerSelJog, -1, event.pmeflags)\n        transport.globalTransport(midi.FPT_F5, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_F9, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_F10, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_F11, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_F12, 1, event.pmeflags)\n        event.data1 = play\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 8, 1]);
        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::TogglePlayback,
                ScriptTransportAction::Stop,
                ScriptTransportAction::RecordToggle,
                ScriptTransportAction::ToggleLoopMode,
                ScriptTransportAction::MarkerJumpJog(2),
                ScriptTransportAction::MarkerSelJog(-1),
                ScriptTransportAction::ShowMainView(0),
                ScriptTransportAction::ShowMainView(4),
                ScriptTransportAction::ToggleAudioSettingsWindow,
                ScriptTransportAction::ToggleProjectInfo,
                ScriptTransportAction::CloseAllWindows,
            ]
        );
    }

    #[test]
    fn python_global_transport_jogs_channels_shuffle_and_editing_commands() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            r#"# name=Test Script
import midi
import transport
def OnMidiIn(event):
    flags = event.pmeflags
    transport.globalTransport(midi.FPT_ChannelJog, 5, flags)
    transport.globalTransport(midi.FPT_ChannelJog, -5, flags)
    transport.globalTransport(midi.FPT_ChannelJog, 0, flags)
    transport.globalTransport(midi.FPT_ShuffleJog, 50, flags)
    transport.globalTransport(midi.FPT_ShuffleJog, -30, flags)
    transport.globalTransport(midi.FPT_Undo, 1, flags)
    transport.globalTransport(midi.FPT_UndoUp, 1, flags)
    transport.globalTransport(midi.FPT_UndoJog, -2, flags)
    transport.globalTransport(midi.FPT_Snap, 1, flags)
    transport.globalTransport(midi.FPT_SnapMode, 3, flags)
"#,
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let state = Arc::new(MidiScriptTransportState::default());
        *state.channel_ids.lock().unwrap() = vec![3, 8, 20];
        state.selected_channel_id.store(8, Ordering::Relaxed);
        state.global_swing_mix_raw.store(100, Ordering::Relaxed);
        let mut runtime =
            MidiScriptRuntime::start_for_device_with_transport_state(&script, "Test Input", state)
                .unwrap();
        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();
        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SelectChannel(20),
                ScriptTransportAction::SelectChannel(3),
                ScriptTransportAction::SetGlobalSwingMixRaw(128),
                ScriptTransportAction::SetGlobalSwingMixRaw(98),
                ScriptTransportAction::Undo,
                ScriptTransportAction::Redo,
                ScriptTransportAction::UndoHistoryJog(-2),
                ScriptTransportAction::TogglePianoRollSnap,
                ScriptTransportAction::PianoRollSnapJog(3),
            ]
        );
    }

    #[test]
    fn python_global_transport_f1_opens_help_for_global_midi_events() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    result = transport.globalTransport(midi.FPT_F1, 1, event.pmeflags)\n    event.data1 = int(result == midi.GT_Global)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
        assert_eq!(result.actions, [ScriptTransportAction::OpenHelpManual]);
    }

    #[test]
    fn python_global_transport_f4_selects_and_names_next_empty_pattern() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    result = transport.globalTransport(midi.FPT_F4, 1, event.pmeflags)\n    event.data1 = int(result == midi.GT_Global)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
        assert_eq!(
            result.actions,
            [ScriptTransportAction::SelectNextEmptyPatternWithNameDialog]
        );
    }

    #[test]
    fn python_global_transport_f2_focuses_the_selected_mixer_track_name() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    result = transport.globalTransport(midi.FPT_F2, 1, event.pmeflags)\n    event.data1 = int(result == midi.GT_Global)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
        assert_eq!(
            result.actions,
            [ScriptTransportAction::FocusSelectedMixerTrackName]
        );
    }

    #[test]
    fn python_arrangement_api_adds_a_named_time_marker_at_absolute_ticks() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    arrangement.addAutoTimeMarker(960, 'Verse')\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [ScriptTransportAction::AddArrangementTimeMarker {
                tick: 960,
                name: "Verse".to_owned(),
                stop_playback: true,
            }]
        );
    }

    #[test]
    fn python_arrangement_api_reads_marker_names_by_absolute_index() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    event.data1 = ord(arrangement.getMarkerName(1)[0])\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state.arrangement_marker_names.lock().unwrap() =
            vec!["Intro".to_owned(), "Verse".to_owned()];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, b'V', 1]);
    }

    #[test]
    fn python_arrangement_api_jumps_to_a_relative_marker_index() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    arrangement.jumpToMarker(-2, False)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [ScriptTransportAction::JumpToMarker {
                delta: -2,
                select: false,
            }]
        );
    }

    #[test]
    fn python_arrangement_api_reads_selected_clip_time_bounds() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    event.data1 = arrangement.selectionStart()\n    event.data2 = arrangement.selectionEnd()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .arrangement_selection_start
            .store(24, Ordering::Relaxed);
        transport_state
            .arrangement_selection_end
            .store(96, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 24, 96]);
    }

    #[test]
    fn python_arrangement_api_reads_current_time_snapped_or_raw() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    event.data1 = arrangement.currentTime(0)\n    event.data2 = arrangement.currentTime(1)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .arrangement_current_time
            .store(48, Ordering::Relaxed);
        transport_state
            .arrangement_current_time_snapped
            .store(72, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 48, 72]);
    }

    #[test]
    fn python_arrangement_api_formats_arbitrary_time_in_bar_step_tick_units() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    bar, step, tick = map(int, arrangement.currentTimeHint(1, 200).split(':'))\n    event.data1 = bar\n    event.data2 = step * 24 + tick\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .ticks_per_quarter
            .store(96, Ordering::Relaxed);
        transport_state
            .time_signature_numerator
            .store(4, Ordering::Relaxed);
        transport_state
            .time_signature_denominator
            .store(4, Ordering::Relaxed);
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 80]);
    }

    #[test]
    fn python_arrangement_api_formats_time_with_pattern_signature_changes() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    bar, step, tick = map(int, arrangement.currentTimeHint(0, 192).split(':'))\n    event.data1 = bar\n    event.data2 = step * 24 + tick\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let transport_state = Arc::new(MidiScriptTransportState::default());
        *transport_state
            .pattern_time_signature_changes
            .lock()
            .unwrap() = vec![(96, 3, 4)];
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            &script,
            "Test Input",
            transport_state,
        )
        .unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 2, 48]);
    }

    #[test]
    fn python_arrangement_api_formats_lengths_from_zero() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport arrangement\ndef OnMidiIn(event):\n    bar, step, tick = map(int, arrangement.currentTimeHint(1, 400, 0, 1).split(':'))\n    event.data1 = bar\n    event.data2 = step * 24 + tick\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 16]);
    }

    #[test]
    fn python_global_transport_leaves_unsupported_or_non_global_commands_unhandled() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    unsupported = transport.globalTransport(midi.FPT_MoveJog, 1, event.pmeflags)\n    focused = transport.globalTransport(midi.FPT_Play, 1, event.pmeflags, midi.GT_Form)\n    event.data1 = int(unsupported == midi.GT_Cannot and focused == midi.GT_Cannot)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 1, 1]);
        assert!(result.actions.is_empty());
    }

    #[test]
    fn python_global_transport_rewind_and_fast_forward_are_hold_commands() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    transport.globalTransport(midi.FPT_Rewind, midi.SS_Start, event.pmeflags)\n    transport.globalTransport(midi.FPT_Rewind, midi.SS_Stop, event.pmeflags)\n    transport.globalTransport(midi.FPT_FastForward, midi.SS_Start, event.pmeflags)\n    transport.globalTransport(midi.FPT_FastForward, midi.SS_Stop, event.pmeflags)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -1_000_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -1_000_000,
                    start: false,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 1_000_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 1_000_000,
                    start: false,
                },
            ]
        );
    }

    #[test]
    fn python_continuous_move_apis_dispatch_speed_and_start_state() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    transport.rewind(midi.SS_Start)\n    transport.rewind(midi.SS_Stop)\n    transport.continuousMove(-0.5, midi.SS_Start)\n    transport.continuousMove(-0.5, midi.SS_Stop)\n    transport.continuousMovePos(2.0, midi.SS_Start)\n    transport.continuousMovePos(2.0, midi.SS_Stop)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -1_000_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -1_000_000,
                    start: false,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -500_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: -500_000,
                    start: false,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 2_000_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 2_000_000,
                    start: false,
                },
            ]
        );
    }

    #[test]
    fn python_transport_fast_forward_moves_while_held_and_stops_on_release() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    transport.fastForward(midi.SS_Start)\n    transport.fastForward(midi.SS_Stop)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 1_000_000,
                    start: true,
                },
                ScriptTransportAction::MoveSongPosition {
                    speed_millionths: 1_000_000,
                    start: false,
                },
            ]
        );
    }

    #[test]
    fn python_global_transport_pattern_jog_dispatches_selection_action() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    result = transport.globalTransport(midi.FPT_PatternJog, -2, event.pmeflags)\n    event.data1 = result\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 8, 1]);
        assert_eq!(result.actions, [ScriptTransportAction::PatternJog(-2)]);
    }

    #[test]
    fn python_global_transport_tempo_jog_dispatches_increment() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    result = transport.globalTransport(midi.FPT_TempoJog, 3, event.pmeflags)\n    event.data1 = result\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(result.bytes, [0xB0, 8, 1]);
        assert_eq!(result.actions, [ScriptTransportAction::TempoJog(3)]);
    }

    #[test]
    fn python_mixer_set_current_tempo_supports_fractional_integer_and_bounded_values() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    mixer.setCurrentTempo(123.456)\n    mixer.setCurrentTempo(123.4565)\n    mixer.setCurrentTempo(120.9, asInt=1)\n    mixer.setCurrentTempo(1.0)\n    mixer.setCurrentTempo(600.0)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetCurrentTempoMilliBpm(123_456),
                ScriptTransportAction::SetCurrentTempoMilliBpm(123_457),
                ScriptTransportAction::SetCurrentTempoMilliBpm(120_000),
                ScriptTransportAction::SetCurrentTempoMilliBpm(10_000),
                ScriptTransportAction::SetCurrentTempoMilliBpm(522_000),
            ]
        );
    }

    #[test]
    fn python_transport_set_playback_speed_accepts_the_documented_range() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    transport.setPlaybackSpeed(0.25)\n    transport.setPlaybackSpeed(1.5)\n    transport.setPlaybackSpeed(4.0)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [
                ScriptTransportAction::SetPlaybackSpeed(250_000),
                ScriptTransportAction::SetPlaybackSpeed(1_500_000),
                ScriptTransportAction::SetPlaybackSpeed(4_000_000),
            ]
        );
    }

    #[test]
    fn python_transport_set_playback_speed_rejects_values_outside_the_documented_range() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnMidiIn(event):\n    transport.setPlaybackSpeed(0.1)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let error = runtime.process_event(12, &[0xB0, 7, 1]).unwrap_err();

        assert!(error.contains("playback speed must be between 0.25 and 4.0"));
    }

    #[test]
    fn python_idle_callback_can_request_transport_actions() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnIdle():\n    transport.stop()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_idle().unwrap();

        assert_eq!(result.actions, [ScriptTransportAction::Stop]);
    }

    #[test]
    fn python_project_load_callback_receives_documented_status_values() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnProjectLoad(status):\n    if status == midi.PL_Start:\n        transport.stop()\n    elif status == midi.PL_LoadOk:\n        transport.start()\n    elif status == midi.PL_LoadError:\n        transport.record()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        assert_eq!(
            runtime.process_project_load(0).unwrap().actions,
            [ScriptTransportAction::Stop]
        );
        assert_eq!(
            runtime.process_project_load(100).unwrap().actions,
            [ScriptTransportAction::Start]
        );
        assert_eq!(
            runtime.process_project_load(101).unwrap().actions,
            [ScriptTransportAction::RecordToggle]
        );
    }

    #[test]
    fn python_full_refresh_runs_after_successful_project_load() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnDoFullRefresh():\n    transport.start()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        assert!(runtime.process_project_load(0).unwrap().actions.is_empty());
        assert_eq!(
            runtime.process_project_load(100).unwrap().actions,
            [ScriptTransportAction::Start]
        );
        assert!(
            runtime
                .process_project_load(101)
                .unwrap()
                .actions
                .is_empty()
        );
    }

    #[test]
    fn python_beat_indicator_callback_receives_off_bar_and_beat_values() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport midi\nimport transport\ndef OnUpdateBeatIndicator(value):\n    if value == 0:\n        transport.stop()\n    elif value == 1:\n        transport.start()\n    elif value == 2:\n        transport.record()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        assert_eq!(
            runtime
                .process_beat_indicator(SCRIPT_BEAT_INDICATOR_OFF)
                .unwrap()
                .actions,
            [ScriptTransportAction::Stop]
        );
        assert_eq!(
            runtime
                .process_beat_indicator(SCRIPT_BEAT_INDICATOR_BAR)
                .unwrap()
                .actions,
            [ScriptTransportAction::Start]
        );
        assert_eq!(
            runtime
                .process_beat_indicator(SCRIPT_BEAT_INDICATOR_BEAT)
                .unwrap()
                .actions,
            [ScriptTransportAction::RecordToggle]
        );
    }

    #[test]
    fn python_mixer_link_channel_to_track_queues_group_channel_and_track_indexes() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport mixer\ndef OnMidiIn(event):\n    mixer.linkChannelToTrack(2, 127, True)\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        let result = runtime.process_event(12, &[0xB0, 7, 1]).unwrap();

        assert_eq!(
            result.actions,
            [ScriptTransportAction::LinkChannelToMixerTrack {
                channel_index: 2,
                track_index: 127,
                select: true,
            }]
        );
    }

    #[test]
    fn python_first_connect_callback_runs_without_arguments() {
        let temporary = TestDirectory::new();
        let script_path = temporary.path().join("device_test.py");
        fs::write(
            &script_path,
            "# name=Test Script\nimport transport\ndef OnFirstConnect():\n    transport.start()\n",
        )
        .unwrap();
        let script = MidiControllerScript::parse(script_path).unwrap();
        let mut runtime = MidiScriptRuntime::start_for_device(&script, "Test Input").unwrap();

        assert_eq!(
            runtime.process_first_connect().unwrap().actions,
            [ScriptTransportAction::Start]
        );
    }

    #[test]
    fn transformed_messages_reject_invalid_midi_data_bytes() {
        assert!(
            transform_message(
                &[0x90, 60, 90],
                &json!({"status":144,"data1":256,"data2":90})
            )
            .is_err()
        );
        assert!(
            transform_message(
                &[0x90, 60, 90],
                &json!({"status":144,"data1":60,"data2":128})
            )
            .is_err()
        );
    }
}

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
    PatternJog(i32),
    TempoJog(i32),
    MarkerJumpJog(i32),
    MarkerSelJog(i32),
    MidiOutput(Vec<u8>),
    RepeatMidiOutput {
        bytes: Vec<u8>,
        delay_ms: u32,
        rate_ms: u32,
    },
    StopRepeatMidiOutput,
}

const SCRIPT_SONG_POSITION_SCALE: f64 = 1_000_000.0;
const SCRIPT_SONG_POSITION_SCALE_U32: u32 = 1_000_000;
impl ScriptTransportAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "record" => Some(Self::RecordToggle),
            "toggle_playback" => Some(Self::TogglePlayback),
            "loop_mode" => Some(Self::ToggleLoopMode),
            _ => None,
        }
    }
}

pub struct MidiScriptRuntime {
    child: Arc<Mutex<Child>>,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    transport_state: Arc<MidiScriptTransportState>,
    startup_actions: Vec<ScriptTransportAction>,
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

pub struct MidiScriptTransportState {
    pub state: AtomicU8,
    pub midi_output_port_number: AtomicI32,
    pub song_position: AtomicU64,
    pub song_position_ticks: AtomicU64,
    pub ticks_per_quarter: AtomicU64,
    pub current_tempo_milli_bpm: AtomicU64,
    pub time_signature_numerator: AtomicU64,
    pub song_length_ticks: AtomicU64,
    pub song_position_seconds: AtomicU64,
    pub song_length_seconds: AtomicU64,
    pub hardware_beat_led_state: AtomicI32,
    pub song_position_hint: Mutex<String>,
    pub song_length_hint: Mutex<String>,
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
            time_signature_numerator: AtomicU64::new(4),
            song_length_ticks: AtomicU64::new(0),
            song_position_seconds: AtomicU64::new(0.0_f64.to_bits()),
            song_length_seconds: AtomicU64::new(0.0_f64.to_bits()),
            hardware_beat_led_state: AtomicI32::new(SCRIPT_BEAT_INDICATOR_OFF),
            song_position_hint: Mutex::new("1:01:000".to_owned()),
            song_length_hint: Mutex::new("0:00:000".to_owned()),
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
        let mut command = Command::new(program);
        command
            .args(prefix_args)
            .arg("-u")
            .arg("-c")
            .arg(MIDI_SCRIPT_RUNNER)
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
            transport_state,
            startup_actions: Vec::new(),
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
        let startup_response = runtime.exchange(&json!({"initialize": true}))?;
        runtime.startup_actions = parse_transport_actions(&startup_response)?;
        Ok(runtime)
    }

    pub fn take_startup_actions(&mut self) -> Vec<ScriptTransportAction> {
        std::mem::take(&mut self.startup_actions)
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
            object.insert(
                "transport".to_owned(),
                json!({
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
                    "time_signature_numerator": self
                        .transport_state
                        .time_signature_numerator
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
                    "project_title": project_title,
                    "project_author": project_author,
                    "project_genre": project_genre,
                }),
            );
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
    let expected_length = match status {
        0x80..=0xEF if matches!(status & 0xF0, 0xC0 | 0xD0) => 2,
        0x80..=0xEF => 3,
        0xF1 | 0xF3 => 2,
        0xF2 => 3,
        0xF6 | 0xF8 | 0xFA..=0xFC | 0xFE..=0xFF => 1,
        _ => return false,
    };
    bytes.len() == expected_length && bytes.iter().skip(1).all(|byte| *byte <= 0x7F)
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
        MidiControllerScript, MidiScriptRuntime, MidiScriptTransportState,
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

        let controller = runtime.process_event(12, &[0xB2, 7, 25]).unwrap();
        let note_on = runtime.process_event(20, &[0x92, 60, 90]).unwrap();
        let note_off = runtime.process_event(40, &[0x82, 60, 64]).unwrap();

        assert_eq!(controller.bytes, [0xB2, 7, 102]);
        assert_eq!(note_on.bytes, [0x92, 64, 90]);
        assert!(note_off.handled);
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
            "# name=Test Script\nimport midi\nimport transport\ndef OnMidiIn(event):\n    if event.pmeflags == midi.PME_System | midi.PME_FromMIDI:\n        play = transport.globalTransport(midi.FPT_Play, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Stop, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Record, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_Loop, 1, event.pmeflags)\n        transport.globalTransport(midi.FPT_MarkerJumpJog, 2, event.pmeflags)\n        transport.globalTransport(midi.FPT_MarkerSelJog, -1, event.pmeflags)\n        event.data1 = play\n",
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
            ]
        );
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

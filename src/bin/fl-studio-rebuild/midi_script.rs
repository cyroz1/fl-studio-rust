use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptTransportAction {
    Start,
    Stop,
    RecordToggle,
    ToggleLoopMode,
    SetSongPosition(u32),
    SetSongPositionTicks(u64),
    MarkerJumpJog(i32),
    MarkerSelJog(i32),
}

const SCRIPT_SONG_POSITION_SCALE: f64 = 1_000_000.0;
const SCRIPT_SONG_POSITION_SCALE_U32: u32 = 1_000_000;

impl ScriptTransportAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "record" => Some(Self::RecordToggle),
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
}

pub const SCRIPT_TRANSPORT_PLAYING: u8 = 1 << 0;
pub const SCRIPT_TRANSPORT_RECORDING: u8 = 1 << 1;
pub const SCRIPT_TRANSPORT_PATTERN_MODE: u8 = 1 << 2;

#[derive(Default)]
pub struct MidiScriptTransportState {
    pub state: AtomicU8,
    pub song_position: AtomicU64,
    pub song_position_ticks: AtomicU64,
    pub song_length_ticks: AtomicU64,
    pub song_position_hint: Mutex<String>,
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
        Ok(runtime)
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
        let mut request = value.clone();
        if let Some(object) = request.as_object_mut() {
            object.insert(
                "transport".to_owned(),
                json!({
                    "is_playing": transport & SCRIPT_TRANSPORT_PLAYING != 0,
                    "is_recording": transport & SCRIPT_TRANSPORT_RECORDING != 0,
                    "pattern_mode": transport & SCRIPT_TRANSPORT_PATTERN_MODE != 0,
                    "song_position": song_position,
                    "song_position_ticks": self
                        .transport_state
                        .song_position_ticks
                        .load(Ordering::Relaxed),
                    "song_length_ticks": self
                        .transport_state
                        .song_length_ticks
                        .load(Ordering::Relaxed),
                    "song_position_hint": song_position_hint,
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

    pub fn shutdown(&mut self) {
        let _ = serde_json::to_writer(&mut self.stdin, &json!({"shutdown": true}));
        let _ = self.stdin.write_all(b"\n").and_then(|_| self.stdin.flush());
        let mut response = String::new();
        let _ = self.stdout.read_line(&mut response);
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
    Err("MIDI script returned an unknown transport action".to_owned())
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

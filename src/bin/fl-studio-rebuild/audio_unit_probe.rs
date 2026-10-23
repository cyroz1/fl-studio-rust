use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use flp_rebuild::plugins::AudioUnitComponentInfo;

pub const PROBE_FLAG: &str = "--audio-unit-initialize-probe";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const PROBE_HEADER: &str = "FL_STUDIO_REBUILD_AUDIO_UNIT_PROBE_V1";

pub type AudioUnitProbeKey = (PathBuf, String, String, String);
pub type AudioUnitProbeRequest = (PathBuf, AudioUnitComponentInfo);
pub type AudioUnitProbeResponse = (
    PathBuf,
    AudioUnitComponentInfo,
    Result<AudioUnitProbeResult, String>,
);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioUnitProbeResult {
    pub component_found: bool,
    pub instance_status: Option<i32>,
    pub initialize_status: Option<i32>,
    pub uninitialize_status: Option<i32>,
    pub dispose_status: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioUnitProbeState {
    Probing,
    Completed(AudioUnitProbeResult),
    Failed(String),
}

impl AudioUnitProbeResult {
    pub fn summary(&self) -> String {
        if !self.component_found {
            return "No matching component is registered".to_owned();
        }
        if let Some(status) = self.instance_status.filter(|status| *status != 0) {
            return format!("Instance creation failed (OSStatus {status})");
        }
        if self.instance_status.is_none() {
            return "Instance creation returned no status".to_owned();
        }
        if let Some(status) = self.initialize_status.filter(|status| *status != 0) {
            return format!("Initialization failed (OSStatus {status})");
        }
        if self.component_found && self.initialize_status.is_none() {
            return "Instance was created, but initialization did not run".to_owned();
        }
        if let Some(status) = self.uninitialize_status.filter(|status| *status != 0) {
            return format!("Uninitialization failed (OSStatus {status})");
        }
        if self.component_found && self.uninitialize_status.is_none() {
            return "Initialized instance was not uninitialized".to_owned();
        }
        if let Some(status) = self.dispose_status.filter(|status| *status != 0) {
            return format!("Instance disposal failed (OSStatus {status})");
        }
        if self.component_found && self.dispose_status.is_none() {
            return "Audio Unit instance was not disposed".to_owned();
        }
        "Instance created, initialized, and disposed successfully".to_owned()
    }
}

pub fn probe_key(path: &Path, component: &AudioUnitComponentInfo) -> AudioUnitProbeKey {
    (
        path.to_path_buf(),
        component.type_code.clone(),
        component.subtype_code.clone(),
        component.manufacturer_code.clone(),
    )
}

pub fn start_worker(
    egui_context: eframe::egui::Context,
) -> (
    Sender<AudioUnitProbeRequest>,
    Receiver<AudioUnitProbeResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<AudioUnitProbeRequest>();
    let (response_sender, response_receiver) = mpsc::channel::<AudioUnitProbeResponse>();

    thread::Builder::new()
        .name("audio-unit-initialize-probe".to_owned())
        .spawn(move || {
            while let Ok((path, component)) = request_receiver.recv() {
                let result = probe_in_subprocess(&component);
                if response_sender.send((path, component, result)).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("Audio Unit probe worker should start");

    (request_sender, response_receiver)
}

pub fn probe_in_subprocess(
    component: &AudioUnitComponentInfo,
) -> Result<AudioUnitProbeResult, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the application executable: {error}"))?;
    let mut child = Command::new(executable)
        .arg(PROBE_FLAG)
        .arg(&component.type_code)
        .arg(&component.subtype_code)
        .arg(&component.manufacturer_code)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the Audio Unit probe: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Audio Unit probe did not provide output".to_owned())?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take((MAX_PROBE_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output)
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader
                    .join()
                    .map_err(|_| "Audio Unit probe output reader stopped".to_owned())?
                    .map_err(|error| format!("could not read Audio Unit probe output: {error}"))?;
                if output.len() > MAX_PROBE_OUTPUT_BYTES {
                    return Err("Audio Unit probe output exceeded its size limit".to_owned());
                }
                if !status.success() {
                    return Err(format!("Audio Unit probe exited with {status}"));
                }
                let output = String::from_utf8(output)
                    .map_err(|error| format!("Audio Unit probe output was not UTF-8: {error}"))?;
                return parse_probe_output(&output);
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("Audio Unit probe timed out after 10 seconds".to_owned());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not wait for Audio Unit probe: {error}"));
            }
        }
    }
}

fn parse_probe_output(output: &str) -> Result<AudioUnitProbeResult, String> {
    let mut after_header = false;
    let mut fields = BTreeMap::new();
    for line in output.lines() {
        if after_header {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key, value);
            }
        } else if line.trim() == PROBE_HEADER {
            after_header = true;
        }
    }
    if !after_header {
        return Err("Audio Unit probe returned no result header".to_owned());
    }

    let component_found = match fields.get("component_found").copied() {
        Some("true") => true,
        Some("false") => false,
        _ => return Err("Audio Unit probe returned an invalid component status".to_owned()),
    };
    let optional_status = |key: &str| -> Result<Option<i32>, String> {
        match fields.get(key).copied() {
            Some("none") => Ok(None),
            Some(value) => value
                .parse::<i32>()
                .map(Some)
                .map_err(|_| format!("Audio Unit probe returned an invalid {key}")),
            None => Err(format!("Audio Unit probe omitted {key}")),
        }
    };

    Ok(AudioUnitProbeResult {
        component_found,
        instance_status: optional_status("instance_status")?,
        initialize_status: optional_status("initialize_status")?,
        uninitialize_status: optional_status("uninitialize_status")?,
        dispose_status: optional_status("dispose_status")?,
    })
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_subtype: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[cfg(target_os = "macos")]
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        component: *mut std::ffi::c_void,
        description: *const AudioComponentDescription,
    ) -> *mut std::ffi::c_void;
    fn AudioComponentInstanceNew(
        component: *mut std::ffi::c_void,
        instance: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn AudioComponentInstanceDispose(instance: *mut std::ffi::c_void) -> i32;
    fn AudioUnitInitialize(unit: *mut std::ffi::c_void) -> i32;
    fn AudioUnitUninitialize(unit: *mut std::ffi::c_void) -> i32;
}

pub fn run_child(type_code: &str, subtype_code: &str, manufacturer_code: &str) -> i32 {
    #[cfg(target_os = "macos")]
    {
        match initialize_component(type_code, subtype_code, manufacturer_code) {
            Ok(result) => {
                print_probe_result(&result);
                0
            }
            Err(error) => {
                eprintln!("{error}");
                2
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (type_code, subtype_code, manufacturer_code);
        eprintln!("Audio Unit instantiation is only available on macOS");
        2
    }
}

#[cfg(target_os = "macos")]
fn initialize_component(
    type_code: &str,
    subtype_code: &str,
    manufacturer_code: &str,
) -> Result<AudioUnitProbeResult, String> {
    let description = AudioComponentDescription {
        component_type: fourcc(type_code)?,
        component_subtype: fourcc(subtype_code)?,
        component_manufacturer: fourcc(manufacturer_code)?,
        component_flags: 0,
        component_flags_mask: 0,
    };
    // SAFETY: AudioComponentFindNext receives an initialized C-compatible
    // description and does not retain its address.
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
    if component.is_null() {
        return Ok(AudioUnitProbeResult {
            component_found: false,
            instance_status: None,
            initialize_status: None,
            uninitialize_status: None,
            dispose_status: None,
        });
    }

    let mut instance = std::ptr::null_mut();
    // SAFETY: `component` was returned by AudioComponentFindNext and the output
    // pointer refers to a valid, writable Audio Component instance slot.
    let instance_status = unsafe { AudioComponentInstanceNew(component, &mut instance) };
    if instance_status != 0 {
        let dispose_status = if instance.is_null() {
            None
        } else {
            // SAFETY: the failed constructor returned a non-null instance pointer.
            Some(unsafe { AudioComponentInstanceDispose(instance) })
        };
        return Ok(AudioUnitProbeResult {
            component_found: true,
            instance_status: Some(instance_status),
            initialize_status: None,
            uninitialize_status: None,
            dispose_status,
        });
    }
    if instance.is_null() {
        return Err("AudioComponentInstanceNew succeeded without returning an instance".to_owned());
    }

    // SAFETY: `instance` is a live Audio Unit instance returned by the system.
    let initialize_status = unsafe { AudioUnitInitialize(instance) };
    let uninitialize_status = if initialize_status == 0 {
        // SAFETY: the instance initialized successfully and remains live.
        Some(unsafe { AudioUnitUninitialize(instance) })
    } else {
        None
    };
    // SAFETY: `instance` is no longer initialized and is disposed exactly once.
    let dispose_status = unsafe { AudioComponentInstanceDispose(instance) };
    Ok(AudioUnitProbeResult {
        component_found: true,
        instance_status: Some(instance_status),
        initialize_status: Some(initialize_status),
        uninitialize_status,
        dispose_status: Some(dispose_status),
    })
}

#[cfg(target_os = "macos")]
fn fourcc(code: &str) -> Result<u32, String> {
    let bytes: [u8; 4] = code
        .as_bytes()
        .try_into()
        .map_err(|_| format!("Audio Unit component ID {code:?} is not four bytes"))?;
    Ok(u32::from_be_bytes(bytes))
}

#[cfg(target_os = "macos")]
fn print_probe_result(result: &AudioUnitProbeResult) {
    println!("{PROBE_HEADER}");
    println!("component_found={}", result.component_found);
    print_optional_status("instance_status", result.instance_status);
    print_optional_status("initialize_status", result.initialize_status);
    print_optional_status("uninitialize_status", result.uninitialize_status);
    print_optional_status("dispose_status", result.dispose_status);
}

#[cfg(target_os = "macos")]
fn print_optional_status(field: &str, status: Option<i32>) {
    match status {
        Some(status) => println!("{field}={status}"),
        None => println!("{field}=none"),
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioUnitProbeResult, parse_probe_output};

    #[test]
    fn parses_audio_unit_probe_status_and_reports_success() {
        let result = parse_probe_output(
            "plug-in diagnostic\nFL_STUDIO_REBUILD_AUDIO_UNIT_PROBE_V1\ncomponent_found=true\ninstance_status=0\ninitialize_status=0\nuninitialize_status=0\ndispose_status=0\n",
        )
        .expect("probe output should parse");
        assert_eq!(
            result,
            AudioUnitProbeResult {
                component_found: true,
                instance_status: Some(0),
                initialize_status: Some(0),
                uninitialize_status: Some(0),
                dispose_status: Some(0),
            }
        );
        assert_eq!(
            result.summary(),
            "Instance created, initialized, and disposed successfully"
        );
    }

    #[test]
    fn reports_audio_unit_initialization_status_from_probe_output() {
        let result = parse_probe_output(
            "FL_STUDIO_REBUILD_AUDIO_UNIT_PROBE_V1\ncomponent_found=true\ninstance_status=0\ninitialize_status=-10868\nuninitialize_status=none\ndispose_status=0\n",
        )
        .expect("probe output should parse");
        assert_eq!(result.summary(), "Initialization failed (OSStatus -10868)");
    }

    #[test]
    fn rejects_incomplete_audio_unit_probe_output() {
        assert!(parse_probe_output("component_found=true\n").is_err());
        assert!(
            parse_probe_output(
                "FL_STUDIO_REBUILD_AUDIO_UNIT_PROBE_V1\ncomponent_found=true\ninstance_status=bad\n"
            )
            .is_err()
        );
    }
}

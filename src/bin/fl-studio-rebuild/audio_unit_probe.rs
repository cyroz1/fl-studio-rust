use std::collections::BTreeMap;
use std::io::Read;
#[cfg(target_os = "macos")]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use flp_rebuild::plugins::AudioUnitComponentInfo;

pub const PROBE_FLAG: &str = "--audio-unit-initialize-probe";
pub const NOTE_PREVIEW_FLAG: &str = "--audio-unit-note-preview";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_NOTE_PREVIEW_OUTPUT_BYTES: usize = 192_000 * 3 * 2 * std::mem::size_of::<f32>();
#[cfg(target_os = "macos")]
const NOTE_PREVIEW_BLOCK_FRAMES: usize = 512;
#[cfg(target_os = "macos")]
const NOTE_PREVIEW_DURATION_SECONDS: u32 = 3;
#[cfg(target_os = "macos")]
const NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 500;
const PROBE_HEADER: &str = "FL_STUDIO_REBUILD_AUDIO_UNIT_PROBE_V1";

pub type AudioUnitProbeKey = (PathBuf, String, String, String);
pub type AudioUnitProbeRequest = (PathBuf, AudioUnitComponentInfo);
pub type AudioUnitProbeResponse = (
    PathBuf,
    AudioUnitComponentInfo,
    Result<AudioUnitProbeResult, String>,
);
pub type AudioUnitNotePreviewRequest = (PathBuf, AudioUnitComponentInfo, u32);
pub type AudioUnitNotePreviewResponse = (PathBuf, AudioUnitComponentInfo, Result<Vec<f32>, String>);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioUnitNotePreviewState {
    Rendering,
    Ready,
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

pub fn start_note_preview_worker(
    egui_context: eframe::egui::Context,
) -> (
    Sender<AudioUnitNotePreviewRequest>,
    Receiver<AudioUnitNotePreviewResponse>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<AudioUnitNotePreviewRequest>();
    let (response_sender, response_receiver) = mpsc::channel::<AudioUnitNotePreviewResponse>();

    thread::Builder::new()
        .name("audio-unit-note-preview".to_owned())
        .spawn(move || {
            while let Ok((path, component, sample_rate)) = request_receiver.recv() {
                let result = note_preview_in_subprocess(&component, sample_rate);
                if response_sender.send((path, component, result)).is_err() {
                    break;
                }
                egui_context.request_repaint();
            }
        })
        .expect("Audio Unit preview worker should start");

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

pub fn note_preview_in_subprocess(
    component: &AudioUnitComponentInfo,
    sample_rate: u32,
) -> Result<Vec<f32>, String> {
    if component.type_code != "aumu" {
        return Err("C4 preview is available for Audio Unit instruments only".to_owned());
    }
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "unsupported Audio Unit preview sample rate: {sample_rate}"
        ));
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the application executable: {error}"))?;
    let mut child = Command::new(executable)
        .arg(NOTE_PREVIEW_FLAG)
        .arg(&component.type_code)
        .arg(&component.subtype_code)
        .arg(&component.manufacturer_code)
        .arg(sample_rate.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the Audio Unit preview: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Audio Unit preview did not provide output".to_owned())?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take((MAX_NOTE_PREVIEW_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output)
    });

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let output = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader
                    .join()
                    .map_err(|_| "Audio Unit preview output reader stopped".to_owned())?
                    .map_err(|error| {
                        format!("could not read Audio Unit preview output: {error}")
                    })?;
                if !status.success() {
                    return Err(format!("Audio Unit preview exited with {status}"));
                }
                break output;
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("Audio Unit preview timed out after 10 seconds".to_owned());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("could not wait for Audio Unit preview: {error}"));
            }
        }
    };
    if output.len() > MAX_NOTE_PREVIEW_OUTPUT_BYTES {
        return Err("Audio Unit preview output exceeded its size limit".to_owned());
    }
    if output.is_empty() || !output.len().is_multiple_of(8) {
        return Err("Audio Unit preview returned incomplete stereo audio".to_owned());
    }

    Ok(output
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| {
            let sample = f32::from_le_bytes(*sample);
            if sample.is_finite() { sample } else { 0.0 }
        })
        .collect())
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
#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    channels: u32,
    data_byte_size: u32,
    data: *mut std::ffi::c_void,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 2],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: [u8; 24],
    flags: u32,
    reserved: u32,
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
    fn AudioUnitSetProperty(
        unit: *mut std::ffi::c_void,
        property_id: u32,
        scope: u32,
        element: u32,
        data: *const std::ffi::c_void,
        data_size: u32,
    ) -> i32;
    fn AudioUnitRender(
        unit: *mut std::ffi::c_void,
        action_flags: *mut u32,
        time_stamp: *const AudioTimeStamp,
        output_bus_number: u32,
        number_frames: u32,
        io_data: *mut AudioBufferList,
    ) -> i32;
    fn MusicDeviceMIDIEvent(
        unit: *mut std::ffi::c_void,
        status: u32,
        data1: u32,
        data2: u32,
        event_offset_sample_frame: u32,
    ) -> i32;
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

pub fn run_note_preview_child(
    type_code: &str,
    subtype_code: &str,
    manufacturer_code: &str,
    sample_rate: u32,
) -> i32 {
    #[cfg(target_os = "macos")]
    {
        match render_note_preview(type_code, subtype_code, manufacturer_code, sample_rate) {
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
                2
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (type_code, subtype_code, manufacturer_code, sample_rate);
        eprintln!("Audio Unit preview is only available on macOS");
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
fn render_note_preview(
    type_code: &str,
    subtype_code: &str,
    manufacturer_code: &str,
    sample_rate: u32,
) -> Result<Vec<f32>, String> {
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!(
            "unsupported Audio Unit preview sample rate: {sample_rate}"
        ));
    }
    if type_code != "aumu" {
        return Err("C4 preview is available for Audio Unit instruments only".to_owned());
    }

    let description = AudioComponentDescription {
        component_type: fourcc(type_code)?,
        component_subtype: fourcc(subtype_code)?,
        component_manufacturer: fourcc(manufacturer_code)?,
        component_flags: 0,
        component_flags_mask: 0,
    };
    // SAFETY: The description is a fully initialized C-compatible value. The
    // system registry owns the returned component and does not retain the pointer.
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
    if component.is_null() {
        return Err("No matching Audio Unit is registered with macOS".to_owned());
    }

    let mut instance = std::ptr::null_mut();
    // SAFETY: `component` came from AudioComponentFindNext and `instance` is a
    // valid writable output slot.
    let instance_status = unsafe { AudioComponentInstanceNew(component, &mut instance) };
    if instance_status != 0 {
        return Err(format!(
            "Audio Unit instance creation failed (OSStatus {instance_status})"
        ));
    }
    if instance.is_null() {
        return Err("Audio Unit instance creation returned no instance".to_owned());
    }

    let result = render_initialized_unit(instance, sample_rate);
    // SAFETY: The instance is owned by this child. `render_initialized_unit`
    // uninitializes it after successful initialization, and dispose is called
    // exactly once on every path.
    let dispose_status = unsafe { AudioComponentInstanceDispose(instance) };
    match (result, dispose_status) {
        (Ok(samples), 0) => Ok(samples),
        (Ok(_), status) => Err(format!("Audio Unit disposal failed (OSStatus {status})")),
        (Err(error), 0) => Err(error),
        (Err(error), status) => Err(format!("{error}; disposal failed (OSStatus {status})")),
    }
}

#[cfg(target_os = "macos")]
fn render_initialized_unit(
    instance: *mut std::ffi::c_void,
    sample_rate: u32,
) -> Result<Vec<f32>, String> {
    const AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
    const AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
    const AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
    const AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
    const LINEAR_PCM_FORMAT: u32 = u32::from_be_bytes(*b"lpcm");
    const LINEAR_PCM_IS_FLOAT: u32 = 1;
    const LINEAR_PCM_IS_PACKED: u32 = 1 << 3;
    const LINEAR_PCM_IS_NON_INTERLEAVED: u32 = 1 << 5;

    let format = AudioStreamBasicDescription {
        sample_rate: f64::from(sample_rate),
        format_id: LINEAR_PCM_FORMAT,
        format_flags: LINEAR_PCM_IS_FLOAT | LINEAR_PCM_IS_PACKED | LINEAR_PCM_IS_NON_INTERLEAVED,
        bytes_per_packet: std::mem::size_of::<f32>() as u32,
        frames_per_packet: 1,
        bytes_per_frame: std::mem::size_of::<f32>() as u32,
        channels_per_frame: 2,
        bits_per_channel: (std::mem::size_of::<f32>() * 8) as u32,
        reserved: 0,
    };
    // SAFETY: The instance is valid, `format` has AudioStreamBasicDescription
    // layout, and the property call copies the value during the call.
    let format_status = unsafe {
        AudioUnitSetProperty(
            instance,
            AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
            AUDIO_UNIT_SCOPE_OUTPUT,
            0,
            (&format as *const AudioStreamBasicDescription).cast(),
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        )
    };
    if format_status != 0 {
        return Err(format!(
            "Audio Unit rejected the stereo float output format (OSStatus {format_status})"
        ));
    }

    let maximum_frames = NOTE_PREVIEW_BLOCK_FRAMES as u32;
    // SAFETY: The property receives a valid 32-bit frame limit by value.
    let frame_limit_status = unsafe {
        AudioUnitSetProperty(
            instance,
            AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
            AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            (&maximum_frames as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if frame_limit_status != 0 {
        return Err(format!(
            "Audio Unit rejected the preview frame limit (OSStatus {frame_limit_status})"
        ));
    }

    // SAFETY: `instance` is live and all output-format properties are configured.
    let initialize_status = unsafe { AudioUnitInitialize(instance) };
    if initialize_status != 0 {
        return Err(format!(
            "Audio Unit initialization failed (OSStatus {initialize_status})"
        ));
    }

    let render_result = render_initialized_instrument(instance, sample_rate);
    // SAFETY: Initialization succeeded and this function owns the initialized instance.
    let uninitialize_status = unsafe { AudioUnitUninitialize(instance) };
    match (render_result, uninitialize_status) {
        (Ok(samples), 0) => Ok(samples),
        (Ok(_), status) => Err(format!(
            "Audio Unit uninitialization failed (OSStatus {status})"
        )),
        (Err(error), 0) => Err(error),
        (Err(error), status) => Err(format!(
            "{error}; uninitialization failed (OSStatus {status})"
        )),
    }
}

#[cfg(target_os = "macos")]
fn render_initialized_instrument(
    instance: *mut std::ffi::c_void,
    sample_rate: u32,
) -> Result<Vec<f32>, String> {
    let total_frames = sample_rate as usize * NOTE_PREVIEW_DURATION_SECONDS as usize;
    let note_off_frame = sample_rate as usize * NOTE_PREVIEW_NOTE_MILLISECONDS as usize / 1_000;
    let mut output = Vec::with_capacity(total_frames * 2);
    let mut frame_offset = 0usize;

    while frame_offset < total_frames {
        let frame_count = (total_frames - frame_offset).min(NOTE_PREVIEW_BLOCK_FRAMES);
        if frame_offset == 0 {
            // SAFETY: A live initialized Music Device receives a valid MIDI C4 note-on.
            let status = unsafe { MusicDeviceMIDIEvent(instance, 0x90, 60, 100, 0) };
            if status != 0 {
                return Err(format!(
                    "Audio Unit rejected the C4 note-on (OSStatus {status})"
                ));
            }
        }
        if (frame_offset..frame_offset + frame_count).contains(&note_off_frame) {
            let event_offset = (note_off_frame - frame_offset) as u32;
            // SAFETY: The note-off is timestamped within this render block.
            let status = unsafe { MusicDeviceMIDIEvent(instance, 0x80, 60, 0, event_offset) };
            if status != 0 {
                return Err(format!(
                    "Audio Unit rejected the C4 note-off (OSStatus {status})"
                ));
            }
        }

        let mut left = vec![0.0f32; frame_count];
        let mut right = vec![0.0f32; frame_count];
        let mut buffers = AudioBufferList {
            number_buffers: 2,
            buffers: [
                AudioBuffer {
                    channels: 1,
                    data_byte_size: (frame_count * std::mem::size_of::<f32>()) as u32,
                    data: left.as_mut_ptr().cast(),
                },
                AudioBuffer {
                    channels: 1,
                    data_byte_size: (frame_count * std::mem::size_of::<f32>()) as u32,
                    data: right.as_mut_ptr().cast(),
                },
            ],
        };
        let timestamp = AudioTimeStamp {
            sample_time: frame_offset as f64,
            host_time: 0,
            rate_scalar: 0.0,
            word_clock_time: 0,
            smpte_time: [0; 24],
            flags: 1,
            reserved: 0,
        };
        let mut action_flags = 0;
        // SAFETY: The initialized unit renders at the configured frame rate into
        // two writable mono f32 buffers with capacity for this block.
        let render_status = unsafe {
            AudioUnitRender(
                instance,
                &mut action_flags,
                &timestamp,
                0,
                frame_count as u32,
                &mut buffers,
            )
        };
        if render_status != 0 {
            return Err(format!(
                "Audio Unit rendering failed (OSStatus {render_status})"
            ));
        }
        for (left, right) in left.into_iter().zip(right) {
            output.push(if left.is_finite() { left } else { 0.0 });
            output.push(if right.is_finite() { right } else { 0.0 });
        }
        frame_offset += frame_count;
    }

    Ok(output)
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

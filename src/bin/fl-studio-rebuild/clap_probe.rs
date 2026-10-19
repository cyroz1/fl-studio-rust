use std::ffi::{CString, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::note_ports::{NoteDialect, NotePortInfoBuffer, PluginNotePorts};
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_host::events::event_types::{MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, EventBuffer, HostInfo, InputAudioBuffers,
    InputChannel, PluginAudioConfiguration, PluginEntry, PluginInstance, ProcessStatus,
};

pub const PROBE_FLAG: &str = "--clap-probe";
pub const INSTANCE_PROBE_FLAG: &str = "--clap-instance-probe";
pub const NOTE_PREVIEW_FLAG: &str = "--clap-note-preview";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_NOTE_PREVIEW_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_INSTANCE_METADATA_ITEMS: u32 = 512;
const MAX_NOTE_PREVIEW_AUDIO_PORTS: u32 = 16;
const MAX_NOTE_PREVIEW_TOTAL_CHANNELS: u32 = 64;
const NOTE_PREVIEW_BLOCK_FRAMES: u32 = 512;
const NOTE_PREVIEW_DURATION_MILLISECONDS: u32 = 1500;
const NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 500;
const PROBE_HEADER: &str = "FL_STUDIO_REBUILD_CLAP_PROBE_V1";
const INSTANCE_PROBE_HEADER: &str = "FL_STUDIO_REBUILD_CLAP_INSTANCE_V1";

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

pub type ClapInspectionResponse = (PathBuf, Result<Vec<ClapPluginDescriptor>, String>);
pub type ClapInstanceInspectionResponse = (PathBuf, String, Result<ClapInstanceInfo, String>);
pub type ClapNotePreviewRequest = (PathBuf, String, String, u32);
pub type ClapNotePreviewResponse = (PathBuf, String, String, Result<Vec<f32>, String>);

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
            while let Ok((path, plugin_id, plugin_name, sample_rate)) = request_receiver.recv() {
                let response = (
                    path.clone(),
                    plugin_id.clone(),
                    plugin_name,
                    note_preview_in_subprocess(&path, &plugin_id, sample_rate),
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

pub fn note_preview_in_subprocess(
    path: &Path,
    plugin_id: &str,
    sample_rate: u32,
) -> Result<Vec<f32>, String> {
    let arguments = [
        OsString::from(NOTE_PREVIEW_FLAG),
        path.as_os_str().to_owned(),
        OsString::from(plugin_id),
        OsString::from(sample_rate.to_string()),
    ];
    let output = run_limited_subprocess(&arguments, "note preview", MAX_NOTE_PREVIEW_OUTPUT_BYTES)?;
    if output.is_empty() || !output.len().is_multiple_of(8) {
        return Err("CLAP note preview returned incomplete stereo audio".to_owned());
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
pub fn run_note_preview_child(path: &Path, plugin_id: &str, sample_rate: u32) -> i32 {
    match render_note_preview(path, plugin_id, sample_rate) {
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

#[derive(Clone, Copy)]
enum PreviewNoteDialect {
    Clap,
    Midi,
}

fn render_note_preview(path: &Path, plugin_id: &str, sample_rate: u32) -> Result<Vec<f32>, String> {
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
    let mut instance = PluginInstance::<()>::new(|_| (), |_| (), &entry, &plugin_id, &host_info)
        .map_err(|error| format!("could not initialize CLAP plug-in instance: {error}"))?;

    let (input_port_channels, output_port_channels, main_output_index, note_port_index, dialect) = {
        let plugin = instance.plugin_handle();
        let audio_ports = plugin
            .get_extension::<PluginAudioPorts>()
            .ok_or_else(|| "CLAP instrument does not expose audio ports".to_owned())?;
        let note_ports = plugin
            .get_extension::<PluginNotePorts>()
            .ok_or_else(|| "CLAP instrument does not expose note input ports".to_owned())?;
        let input_port_channels = read_preview_audio_port_channels(&audio_ports, &plugin, true)?;
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

    let audio_configuration = PluginAudioConfiguration {
        sample_rate: f64::from(sample_rate),
        min_frames_count: 1,
        max_frames_count: NOTE_PREVIEW_BLOCK_FRAMES,
    };
    let stopped_processor = instance
        .activate(|_, _| (), audio_configuration)
        .map_err(|error| format!("could not activate CLAP instrument: {error}"))?;
    let mut audio_processor = match stopped_processor.start_processing() {
        Ok(processor) => processor,
        Err(error) => {
            let stopped_processor = error.into_stopped_processor();
            instance.deactivate(stopped_processor);
            return Err("CLAP instrument could not start audio processing".to_owned());
        }
    };

    let total_frames = sample_rate * NOTE_PREVIEW_DURATION_MILLISECONDS / 1000;
    let note_off_frame = sample_rate * NOTE_PREVIEW_NOTE_MILLISECONDS / 1000;
    let mut samples = Vec::with_capacity(total_frames as usize * 2);
    let mut input_audio_ports = AudioPorts::with_capacity(
        input_port_channels.iter().sum::<u32>() as usize,
        input_port_channels.len(),
    );
    let mut output_audio_ports = AudioPorts::with_capacity(
        output_port_channels.iter().sum::<u32>() as usize,
        output_port_channels.len(),
    );
    let mut frame_offset = 0;
    let mut render_error = None;

    while frame_offset < total_frames {
        let frame_count = (total_frames - frame_offset).min(NOTE_PREVIEW_BLOCK_FRAMES);
        let frame_end = frame_offset + frame_count;
        let mut input_event_buffer = EventBuffer::with_capacity(2);
        let pckn = clack_host::prelude::Pckn::new(note_port_index, 0u16, 60u16, 0u32);
        if frame_offset == 0 {
            match dialect {
                PreviewNoteDialect::Clap => {
                    input_event_buffer.push(&NoteOnEvent::new(0, pckn, 0.8));
                }
                PreviewNoteDialect::Midi => {
                    input_event_buffer.push(&MidiEvent::new(0, note_port_index, [0x90, 60, 100]));
                }
            }
        }
        if (frame_offset..frame_end).contains(&note_off_frame) {
            let event_time = note_off_frame - frame_offset;
            match dialect {
                PreviewNoteDialect::Clap => {
                    input_event_buffer.push(&NoteOffEvent::new(event_time, pckn, 0.0));
                }
                PreviewNoteDialect::Midi => {
                    input_event_buffer.push(&MidiEvent::new(
                        event_time,
                        note_port_index,
                        [0x80, 60, 0],
                    ));
                }
            }
        }
        input_event_buffer.sort();
        let mut output_event_buffer = EventBuffer::with_capacity(16);
        let mut input_audio_storage = input_port_channels
            .iter()
            .map(|channel_count| {
                (0..*channel_count)
                    .map(|_| vec![0.0f32; frame_count as usize])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut output_audio_storage = output_port_channels
            .iter()
            .map(|channel_count| {
                (0..*channel_count)
                    .map(|_| vec![0.0f32; frame_count as usize])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

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
                    Some(u64::from(frame_offset)),
                    None,
                )
                .map(|_: ProcessStatus| ())
                .map_err(|error| format!("CLAP instrument failed while rendering audio: {error}"))
        };

        if let Err(error) = process_result {
            render_error = Some(error);
            break;
        }

        let main_output = &output_audio_storage[main_output_index];
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
    instance.deactivate(stopped_processor);
    if let Some(error) = render_error {
        return Err(error);
    }
    Ok(samples)
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
            });
        if let Some(dialect) = dialect {
            let port_index = u16::try_from(index)
                .map_err(|_| "CLAP instrument note input port index is too large".to_owned())?;
            return Ok((port_index, dialect));
        }
    }
    Err("CLAP instrument has no note input port supporting CLAP or MIDI events".to_owned())
}

fn preview_note_dialect(dialect: NoteDialect) -> Option<PreviewNoteDialect> {
    match dialect {
        NoteDialect::Clap => Some(PreviewNoteDialect::Clap),
        NoteDialect::Midi | NoteDialect::MidiMpe => Some(PreviewNoteDialect::Midi),
        NoteDialect::Midi2 => None,
    }
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
        ClapPluginDescriptor, encode_instance_probe_output, encode_probe_output,
        parse_instance_probe_output, parse_probe_output,
    };

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
}

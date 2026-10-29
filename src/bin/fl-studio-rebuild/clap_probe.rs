use std::ffi::{CString, OsString};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clack_extensions::audio_ports::{AudioPortFlags, AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::note_ports::{NoteDialect, NotePortInfoBuffer, PluginNotePorts};
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_host::events::event_types::{Midi2Event, MidiEvent, NoteOffEvent, NoteOnEvent};
use clack_host::prelude::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, EventBuffer, HostInfo, InputAudioBuffers,
    InputChannel, PluginAudioConfiguration, PluginEntry, PluginInstance, ProcessStatus,
};

pub const PROBE_FLAG: &str = "--clap-probe";
pub const INSTANCE_PROBE_FLAG: &str = "--clap-instance-probe";
pub const NOTE_PREVIEW_FLAG: &str = "--clap-note-preview";
pub const NOTE_PREVIEW_SERVER_FLAG: &str = "--clap-note-preview-server";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_NOTE_PREVIEW_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_NOTE_PREVIEW_PROTOCOL_HEADER_BYTES: usize = 4096;
const MAX_INSTANCE_METADATA_ITEMS: u32 = 512;
const MAX_NOTE_PREVIEW_AUDIO_PORTS: u32 = 16;
const MAX_NOTE_PREVIEW_TOTAL_CHANNELS: u32 = 64;
const MAX_PATTERN_PREVIEW_NOTES: usize = 16_384;
const NOTE_PREVIEW_BLOCK_FRAMES: u32 = 512;
const NOTE_PREVIEW_DURATION_MILLISECONDS: u32 = 1500;
const DEFAULT_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 500;
pub const MIN_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 50;
pub const MAX_NOTE_PREVIEW_NOTE_MILLISECONDS: u32 = 1000;
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClapPatternPreviewSettings {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub loop_length_ticks: u64,
    pub notes: Vec<ClapPatternNote>,
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
    ClapPatternPreviewSettings,
    u64,
);
pub type ClapPatternPreviewResponse = (
    PathBuf,
    String,
    String,
    ClapPatternPreviewSettings,
    u64,
    Result<Vec<f32>, String>,
);

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
            while let Ok((path, plugin_id, plugin_name, sample_rate, settings, request_id)) =
                request_receiver.recv()
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
                    process.render_pattern(&settings)
                } else {
                    match ClapPreviewProcess::spawn(&path, &plugin_id, sample_rate) {
                        Ok(mut process) => {
                            let result = process.render_pattern(&settings);
                            preview_process = Some(process);
                            result
                        }
                        Err(error) => Err(error),
                    }
                };
                if result.is_err() {
                    preview_process.take();
                }
                if response_sender
                    .send((path, plugin_id, plugin_name, settings, request_id, result))
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
    Error(String),
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
        }
    }

    fn render_pattern(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        validate_pattern_preview_settings(settings, self.sample_rate)?;
        writeln!(
            self.stdin,
            "PATTERN {} {:016x} {} {}",
            settings.ppq,
            settings.tempo_bpm.to_bits(),
            settings.loop_length_ticks,
            settings.notes.len(),
        )
        .map_err(|error| format!("could not send a CLAP Pattern preview request: {error}"))?;
        for note in &settings.notes {
            writeln!(
                self.stdin,
                "NOTE {} {} {} {} {}",
                note.position_ticks, note.length_ticks, note.key, note.velocity, note.midi_channel,
            )
            .map_err(|error| format!("could not send a CLAP Pattern note: {error}"))?;
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
            let mut output = vec![0; byte_count];
            if let Err(error) = stdout.read_exact(&mut output) {
                let _ = response_sender.send(ClapPreviewProcessResponse::Error(format!(
                    "could not read CLAP preview audio: {error}"
                )));
                return;
            }
            let samples = output
                .as_chunks::<4>()
                .0
                .iter()
                .map(|sample| {
                    let sample = f32::from_le_bytes(*sample);
                    if sample.is_finite() { sample } else { 0.0 }
                })
                .collect();
            if response_sender
                .send(ClapPreviewProcessResponse::Audio(samples))
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
    let (Some(ppq), Some(tempo_bits), Some(loop_length_ticks), Some(note_count), None) = (
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next(),
        fields.next().and_then(|field| field.parse::<u64>().ok()),
        fields.next().and_then(|field| field.parse::<usize>().ok()),
        fields.next(),
    ) else {
        return Err("invalid CLAP Pattern preview header".to_owned());
    };
    let tempo_bpm = u64::from_str_radix(tempo_bits, 16)
        .map(f64::from_bits)
        .map_err(|_| "invalid CLAP Pattern preview tempo".to_owned())?;
    if note_count > MAX_PATTERN_PREVIEW_NOTES {
        return Err(format!(
            "CLAP Pattern preview exceeds {MAX_PATTERN_PREVIEW_NOTES} notes"
        ));
    }
    let mut notes = Vec::with_capacity(note_count);
    for _ in 0..note_count {
        let line = read_preview_protocol_header(input)
            .map_err(|error| format!("could not read CLAP Pattern note: {error}"))?
            .ok_or_else(|| "CLAP Pattern preview ended before all notes arrived".to_owned())?;
        notes.push(parse_pattern_preview_note(&line)?);
    }
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
    };
    validate_pattern_preview_settings(&settings, sample_rate)?;
    Ok(settings)
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
        None,
    ) = (
        fields.next(),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next().and_then(|field| field.parse::<u32>().ok()),
        fields.next().and_then(|field| field.parse::<u16>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next().and_then(|field| field.parse::<u8>().ok()),
        fields.next(),
    )
    else {
        return Err("invalid CLAP Pattern note".to_owned());
    };
    Ok(ClapPatternNote {
        position_ticks,
        length_ticks,
        key,
        velocity,
        midi_channel,
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

#[derive(Clone, Copy)]
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
                is_note_on: true,
            },
            ScheduledClapNoteEvent {
                frame: u64::from(note_off_frame),
                priority: 0,
                key: note_key,
                velocity: 0,
                midi_channel: 0,
                note_id: 0,
                is_note_on: false,
            },
        ];
        let total_frames = self.sample_rate * NOTE_PREVIEW_DURATION_MILLISECONDS / 1000;
        self.render_scheduled_events(&events, total_frames)
    }

    fn render_pattern(
        &mut self,
        settings: &ClapPatternPreviewSettings,
    ) -> Result<Vec<f32>, String> {
        let (events, total_frames) = schedule_pattern_preview_events(settings, self.sample_rate)?;
        self.render_scheduled_events(&events, total_frames)
    }

    fn render_scheduled_events(
        &mut self,
        events: &[ScheduledClapNoteEvent],
        total_frames: u32,
    ) -> Result<Vec<f32>, String> {
        if total_frames == 0 {
            return Err("CLAP preview has no audio frames to render".to_owned());
        }

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
        let mut render_error = None;

        while frame_offset < total_frames {
            let frame_count = (total_frames - frame_offset).min(NOTE_PREVIEW_BLOCK_FRAMES);
            let frame_end = frame_offset + frame_count;
            let block_event_start = event_index;
            while event_index < events.len() && events[event_index].frame < u64::from(frame_end) {
                event_index += 1;
            }
            let mut input_event_buffer =
                EventBuffer::with_capacity(event_index.saturating_sub(block_event_start));
            for event in &events[block_event_start..event_index] {
                let event_time = u32::try_from(event.frame.saturating_sub(u64::from(frame_offset)))
                    .map_err(|_| "CLAP note event offset exceeds the audio block".to_owned())?;
                let pckn = clack_host::prelude::Pckn::new(
                    self.note_port_index,
                    u16::from(event.midi_channel),
                    u16::from(event.key),
                    event.note_id,
                );
                match (self.dialect, event.is_note_on) {
                    (PreviewNoteDialect::Clap, true) => {
                        input_event_buffer.push(&NoteOnEvent::new(
                            event_time,
                            pckn,
                            f64::from(event.velocity) / 127.0,
                        ));
                    }
                    (PreviewNoteDialect::Clap, false) => {
                        input_event_buffer.push(&NoteOffEvent::new(event_time, pckn, 0.0));
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
                            [0x80 | event.midi_channel, event.key, 0],
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
                            0,
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
                        None,
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
struct ScheduledClapNoteEvent {
    frame: u64,
    priority: u8,
    key: u8,
    velocity: u8,
    midi_channel: u8,
    note_id: u32,
    is_note_on: bool,
}

fn validate_pattern_preview_settings(
    settings: &ClapPatternPreviewSettings,
    sample_rate: u32,
) -> Result<(), String> {
    schedule_pattern_preview_events(settings, sample_rate).map(|_| ())
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
        let frames = (tick as f64 * 60.0 * f64::from(sample_rate))
            / (f64::from(settings.ppq) * settings.tempo_bpm);
        if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
            Err("CLAP Pattern note timing is outside the renderable range".to_owned())
        } else {
            Ok(frames.round() as u64)
        }
    };
    let mut events = Vec::with_capacity(settings.notes.len().saturating_mul(2));
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
        if !(1..=127).contains(&note.velocity) {
            return Err(format!(
                "CLAP Pattern preview velocity {} is outside 1–127",
                note.velocity
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
        events.push(ScheduledClapNoteEvent {
            frame: start_frame,
            priority: 1,
            key: note.key as u8,
            velocity: note.velocity,
            midi_channel: note.midi_channel,
            note_id,
            is_note_on: true,
        });
        events.push(ScheduledClapNoteEvent {
            frame: end_frame,
            priority: 0,
            key: note.key as u8,
            velocity: 0,
            midi_channel: note.midi_channel,
            note_id,
            is_note_on: false,
        });
        last_event_frame = last_event_frame.max(end_frame);
    }
    events.sort_by_key(|event| (event.frame, event.priority));

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
        ClapPatternNote, ClapPatternPreviewSettings, ClapPluginDescriptor,
        MAX_NOTE_PREVIEW_NOTE_MILLISECONDS, MIN_NOTE_PREVIEW_NOTE_MILLISECONDS,
        encode_instance_probe_output, encode_probe_output, parse_instance_probe_output,
        parse_pattern_preview_request, parse_probe_output, schedule_pattern_preview_events,
        validate_note_preview_note_length,
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

    fn pattern_preview_settings() -> ClapPatternPreviewSettings {
        ClapPatternPreviewSettings {
            pattern_id: 4,
            channel_id: 9,
            ppq: 96,
            tempo_bpm: 120.0,
            loop_length_ticks: 192,
            notes: vec![
                ClapPatternNote {
                    position_ticks: 0,
                    length_ticks: 96,
                    key: 60,
                    velocity: 100,
                    midi_channel: 0,
                },
                ClapPatternNote {
                    position_ticks: 96,
                    length_ticks: 96,
                    key: 60,
                    velocity: 80,
                    midi_channel: 0,
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
        assert_eq!(events[2].frame, 24_000);
        assert!(events[2].is_note_on);
        assert_eq!((events[3].frame, events[3].is_note_on), (48_000, false));
    }

    #[test]
    fn pattern_preview_parser_reads_bounded_note_records() {
        let tempo_bpm = 123.5_f64;
        let header = format!("96 {:016x} 192 2", tempo_bpm.to_bits());
        let mut input = Cursor::new(b"NOTE 0 96 60 100 0\nNOTE 96 96 64 80 2\nEND\n".as_slice());

        let parsed = parse_pattern_preview_request(&header, &mut input, 48_000)
            .expect("valid Pattern protocol should parse");

        assert_eq!(parsed.ppq, 96);
        assert_eq!(parsed.tempo_bpm, tempo_bpm);
        assert_eq!(parsed.loop_length_ticks, 192);
        assert_eq!(parsed.notes.len(), 2);
        assert_eq!(parsed.notes[1].key, 64);
        assert_eq!(parsed.notes[1].midi_channel, 2);
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
    }
}

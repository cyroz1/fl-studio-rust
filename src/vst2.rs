//! Isolated VST2 plug-in inspection and offline note rendering.
//!
//! The plug-in ABI and worker process are provided by `vst-audio`; the host
//! application only talks to its bounded process and audio APIs.

use std::ffi::OsStr;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::ChannelPluginState;
use crate::media::{DecodedAudio, decode_audio_file, write_audio_range_to_wav};
use vst_audio::{
    AudioEvent, AudioStatus, BusSelector, ClassInfo, Command, Configuration, Host, HostEvent,
    Instance, PluginDescriptor, PluginFormat, Transport,
};

const RENDER_QUANTUM: u32 = 512;
const MAX_RENDER_SECONDS: f64 = 120.0;
const MAX_RENDER_BUFFER_BYTES: usize = 512 * 1024 * 1024;

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

/// Run the embedded worker when this binary is launched by `vst-audio`.
///
/// `vst-audio` starts the configured executable with `--control-in` as its
/// first argument. Dispatching before the normal CLI or GUI keeps plug-in code
/// in the child process while allowing installers to ship one application
/// executable.
pub fn run_vst2_worker_if_requested() -> bool {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == OsStr::new("--control-in"))
    {
        vst_audio_worker_runtime::host::main();
        true
    } else {
        false
    }
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
    let metadata = state
        .vst_metadata()
        .ok_or_else(|| "FLP channel has no recognized VST state envelope".to_owned())?;
    let fourcc = metadata
        .fourcc()
        .ok_or_else(|| "FLP channel state has no VST2 FourCC identifier".to_owned())?;
    let fourcc_bytes: [u8; 4] = fourcc
        .as_bytes()
        .try_into()
        .map_err(|_| "FLP VST2 FourCC identifier is not exactly four bytes".to_owned())?;
    if !fourcc_bytes.is_ascii() {
        return Err("FLP VST2 FourCC identifier is not ASCII".to_owned());
    }
    let state_bytes = state
        .vst_state_bytes()
        .ok_or_else(|| "FLP VST2 channel state has no nested field 53".to_owned())?;
    let saved_unique_id = i32::from_le_bytes(fourcc_bytes);
    let worker_uid_bytes = saved_unique_id.to_be_bytes();

    let mut matching_classes = inspect_vst2_plugin(plugin_path.as_ref())?
        .into_iter()
        .filter_map(|class| {
            if &class.id[..4] != b"VST2" || class.id[8..].iter().any(|byte| *byte != 0) {
                return None;
            }
            let class_uid = &class.id[4..8];
            if class_uid == worker_uid_bytes {
                Some((class, "FLP-little-endian"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if matching_classes.is_empty() {
        return Err(format!(
            "no VST2 class in {} matches saved FourCC {fourcc}",
            plugin_path.as_ref().display()
        ));
    }
    if matching_classes.len() != 1 {
        return Err(format!(
            "saved VST2 FourCC {fourcc} matches multiple classes in {}",
            plugin_path.as_ref().display()
        ));
    }
    let (class, id_byte_order) = matching_classes.pop().expect("one VST2 class matched");
    let class_id = class
        .id
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let descriptor = PluginDescriptor {
        path: plugin_path.as_ref().to_path_buf(),
        format: PluginFormat::Vst2,
        class_id: Some(class.id),
    };
    let host = host_for_current_executable()?;
    let configuration = Configuration {
        offline: true,
        ..Configuration::default()
    };
    let instance = host
        .spawn(&descriptor, &configuration, Some(state_bytes))
        .map_err(|error| format!("could not restore FLP VST2 state in worker: {error}"))?;
    let worker_state = save_worker_vst2_state(&instance)?;
    let state_roundtrip_identical = worker_state.as_slice() == state_bytes;
    drop(instance);

    Ok(Vst2StateProbeSummary {
        plugin_name: class.name,
        vendor: class.vendor,
        saved_plugin_name: metadata.name().unwrap_or("unknown").to_owned(),
        saved_vendor: metadata.vendor().unwrap_or("unknown").to_owned(),
        saved_fourcc: fourcc.to_owned(),
        class_id,
        id_byte_order,
        state_bytes: state_bytes.len(),
        state_roundtrip_identical,
    })
}

fn save_worker_vst2_state(instance: &Instance) -> Result<Vec<u8>, String> {
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
                _ => {}
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

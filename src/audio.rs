//! Live device input and output for the desktop application.
//!
//! Shared streams use CPAL (WASAPI shared mode on Windows). Windows exclusive
//! streams are opened directly with WASAPI so the requested endpoint format and
//! exclusive-access errors are visible to the application.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};

const AUDIO_SOURCE_SILENT: u8 = 0;
const AUDIO_SOURCE_TEST_TONE: u8 = 1;
const AUDIO_SOURCE_INPUT_MONITOR: u8 = 2;
const AUDIO_SOURCE_PROJECT: u8 = 3;
const AUDIO_SOURCE_STREAM: u8 = 4;
const AUDIO_RING_CAPACITY: usize = 65_536;
const AUDIO_ERROR_HISTORY_LIMIT: usize = 16;
const STREAM_PREFILL_FRAMES: usize = 512;
const TEST_TONE_HZ: f32 = 440.0;
const TEST_TONE_LEVEL: f32 = 0.12;

/// Enable flush-to-zero for floating-point work on the current audio/render thread.
///
/// x86-64 uses MXCSR.FTZ and MXCSR.DAZ; AArch64 uses FPCR.FZ. The floating-point
/// control state belongs to the calling thread and is left enabled for its lifetime.
#[inline]
pub(crate) fn enable_denormal_protection() {
    #[cfg(target_arch = "x86_64")]
    {
        const FTZ_AND_DAZ: u32 = (1 << 15) | (1 << 6);
        write_mxcsr(read_mxcsr() | FTZ_AND_DAZ);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        let control: u64;
        std::arch::asm!(
            "mrs {control}, fpcr",
            control = out(reg) control,
            options(nostack, preserves_flags)
        );
        std::arch::asm!(
            "msr fpcr, {control}",
            control = in(reg) (control | (1 << 24)),
            options(nostack, preserves_flags)
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_mxcsr() -> u32 {
    let mut value = 0_u32;
    unsafe {
        std::arch::asm!(
            "stmxcsr [{address}]",
            address = in(reg) &mut value,
            options(nostack, preserves_flags)
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn write_mxcsr(value: u32) {
    unsafe {
        std::arch::asm!(
            "ldmxcsr [{address}]",
            address = in(reg) &value,
            options(nostack, preserves_flags)
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioAccess {
    #[default]
    Shared,
    Exclusive,
}

impl AudioAccess {
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared",
            Self::Exclusive => "Exclusive",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioDeviceCatalog {
    pub inputs: Vec<AudioDeviceInfo>,
    pub outputs: Vec<AudioDeviceInfo>,
    pub default_input_id: Option<String>,
    pub default_output_id: Option<String>,
    pub default_sample_rate: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub enable_input: bool,
    pub enable_output: bool,
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub access: AudioAccess,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enable_input: true,
            enable_output: true,
            input_device_id: None,
            output_device_id: None,
            sample_rate: 48_000,
            buffer_frames: 256,
            access: AudioAccess::Shared,
        }
    }
}

struct PlaybackState {
    samples: ArcSwapOption<Vec<f32>>,
    streaming: ArcSwapOption<StreamingPlayback>,
    cursor_frames: AtomicU64,
    active: AtomicBool,
    browser_preview_samples: ArcSwapOption<Vec<f32>>,
    browser_preview_cursor_frames: AtomicU64,
    browser_preview_active: AtomicBool,
    browser_preview_gain: AtomicU32,
}

struct StreamingPlayback {
    ring: AudioRingBuffer,
    active: AtomicBool,
    paused: AtomicBool,
    finished: AtomicBool,
    cancelled: AtomicBool,
    underrun_frames: AtomicU64,
    consumed_frames: AtomicU64,
}

#[derive(Default)]
struct AudioErrorState {
    latest: Mutex<Option<String>>,
    recent: Mutex<VecDeque<String>>,
    count: AtomicU64,
}

impl AudioErrorState {
    fn report(&self, message: String) {
        self.count.fetch_add(1, Ordering::Relaxed);
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());

        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if recent.len() == AUDIO_ERROR_HISTORY_LIMIT {
            recent.pop_front();
        }
        recent.push_back(message);
    }

    fn take_latest(&self) -> Option<String> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn recent(&self) -> Vec<String> {
        self.recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

/// The producer side of audio streamed into an active device callback.
///
/// Writes may wait for the callback to consume ring-buffer space. This handle is intended for
/// worker threads; the device callback never waits for the producer.
pub struct StreamingAudioWriter {
    playback: Arc<StreamingPlayback>,
}

impl StreamingAudioWriter {
    /// Write interleaved stereo samples, waiting outside the audio callback when the ring fills.
    pub fn write_stereo_samples(&self, samples: &[f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(2) {
            return Err("streamed audio must contain stereo sample frames".to_owned());
        }
        let (frames, _) = samples.as_chunks::<2>();
        for &[left, right] in frames {
            while !self.playback.ring.push_stereo_frame(left, right) {
                if self.playback.cancelled.load(Ordering::Acquire) {
                    return Err("streamed audio playback was stopped".to_owned());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.playback.cancelled.load(Ordering::Acquire) {
                return Err("streamed audio playback was stopped".to_owned());
            }
        }
        Ok(())
    }

    /// Mark the stream complete. Buffered frames continue playing before the device goes idle.
    pub fn finish(&self) {
        self.playback.finished.store(true, Ordering::Release);
    }

    /// Whether Stop or a replacement playback source cancelled this stream.
    pub fn is_cancelled(&self) -> bool {
        self.playback.cancelled.load(Ordering::Acquire)
    }

    /// Number of stereo frames the device requested after the stream began but before data was
    /// available. This is useful for diagnosing producer underruns.
    pub fn underrun_frames(&self) -> u64 {
        self.playback.underrun_frames.load(Ordering::Acquire)
    }
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            samples: ArcSwapOption::empty(),
            streaming: ArcSwapOption::empty(),
            cursor_frames: AtomicU64::new(0),
            active: AtomicBool::new(false),
            browser_preview_samples: ArcSwapOption::empty(),
            browser_preview_cursor_frames: AtomicU64::new(0),
            browser_preview_active: AtomicBool::new(false),
            browser_preview_gain: AtomicU32::new(1.0_f32.to_bits()),
        }
    }

    fn start(&self, samples: Vec<f32>) {
        self.cancel_stream();
        self.samples.store(Some(Arc::new(samples)));
        self.cursor_frames.store(0, Ordering::Release);
        self.active.store(true, Ordering::Release);
    }

    fn start_browser_preview(&self, samples: Vec<f32>) {
        self.stop_browser_preview();
        self.browser_preview_samples.store(Some(Arc::new(samples)));
        self.browser_preview_cursor_frames
            .store(0, Ordering::Release);
        self.browser_preview_active.store(true, Ordering::Release);
    }

    fn stop_browser_preview(&self) {
        self.browser_preview_active.store(false, Ordering::Release);
        self.browser_preview_cursor_frames
            .store(0, Ordering::Release);
        self.browser_preview_samples.store(None);
    }

    fn browser_preview_active(&self) -> bool {
        self.browser_preview_active.load(Ordering::Acquire)
    }

    fn next_browser_preview_frame(&self, samples: Option<&[f32]>) -> [f32; 2] {
        if !self.browser_preview_active.load(Ordering::Acquire) {
            return [0.0, 0.0];
        }
        let frame = self
            .browser_preview_cursor_frames
            .fetch_add(1, Ordering::Relaxed);
        let Some(offset) = usize::try_from(frame)
            .ok()
            .and_then(|frame| frame.checked_mul(2))
        else {
            self.browser_preview_active.store(false, Ordering::Release);
            return [0.0, 0.0];
        };
        let Some(samples) =
            samples.and_then(|samples| samples.get(offset..offset.saturating_add(2)))
        else {
            self.browser_preview_active.store(false, Ordering::Release);
            return [0.0, 0.0];
        };
        let gain = f32::from_bits(self.browser_preview_gain.load(Ordering::Relaxed));
        [samples[0] * gain, samples[1] * gain]
    }

    fn start_streaming(&self) -> StreamingAudioWriter {
        self.stop();
        let playback = Arc::new(StreamingPlayback {
            ring: AudioRingBuffer::new(AUDIO_RING_CAPACITY),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            underrun_frames: AtomicU64::new(0),
            consumed_frames: AtomicU64::new(0),
        });
        self.streaming.store(Some(Arc::clone(&playback)));
        StreamingAudioWriter { playback }
    }

    fn pause(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(streaming) = self.streaming.load().as_deref() {
            streaming.active.store(false, Ordering::Release);
            streaming.paused.store(true, Ordering::Release);
        }
    }

    fn resume(&self) -> Result<(), String> {
        if let Some(streaming) = self.streaming.load().as_deref() {
            if streaming.cancelled.load(Ordering::Acquire)
                || (streaming.finished.load(Ordering::Acquire)
                    && streaming.ring.available_stereo_frames() == 0)
            {
                return Err("No streamed project audio remains to resume".to_owned());
            }
            streaming.paused.store(false, Ordering::Release);
            streaming.active.store(true, Ordering::Release);
            return Ok(());
        }
        let frame_count = self
            .samples
            .load()
            .as_deref()
            .map(|samples| samples.len() / 2)
            .unwrap_or(0);
        if frame_count == 0 {
            return Err("No rendered project audio is loaded".into());
        }
        if self.cursor_frames.load(Ordering::Acquire) >= frame_count as u64 {
            self.cursor_frames.store(0, Ordering::Release);
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        self.cursor_frames.store(0, Ordering::Release);
        self.samples.store(None);
        self.cancel_stream();
    }

    fn cancel_stream(&self) {
        if let Some(streaming) = self.streaming.swap(None) {
            streaming.cancelled.store(true, Ordering::Release);
            streaming.active.store(false, Ordering::Release);
        }
    }

    fn is_active(&self) -> bool {
        if self.active.load(Ordering::Acquire) {
            return true;
        }
        self.streaming.load().as_deref().is_some_and(|streaming| {
            !streaming.cancelled.load(Ordering::Acquire)
                && !streaming.paused.load(Ordering::Acquire)
                && (streaming.active.load(Ordering::Acquire)
                    || !streaming.finished.load(Ordering::Acquire)
                    || streaming.ring.available_stereo_frames() > 0)
        })
    }

    fn is_streaming(&self) -> bool {
        self.streaming.load().is_some()
    }

    fn position_frames(&self) -> u64 {
        self.streaming.load().as_deref().map_or_else(
            || self.cursor_frames.load(Ordering::Acquire),
            |streaming| streaming.consumed_frames.load(Ordering::Acquire),
        )
    }
}

/// Enumerate the default system host's active input and output endpoints.
pub fn enumerate_devices() -> AudioDeviceCatalog {
    let host = cpal::default_host();
    let default_input_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host.default_output_device();
    let default_sample_rate = default_output
        .as_ref()
        .and_then(|device| device.default_output_config().ok())
        .map(|config| config.sample_rate());
    let default_output_id = default_output
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut catalog = AudioDeviceCatalog {
        default_input_id,
        default_output_id,
        default_sample_rate,
        ..AudioDeviceCatalog::default()
    };

    match host.devices() {
        Ok(devices) => {
            for device in devices {
                let Ok(id) = device.id() else {
                    continue;
                };
                let name = device
                    .description()
                    .map(|description| description.to_string())
                    .unwrap_or_else(|_| device.to_string());
                let info = AudioDeviceInfo {
                    id: id.to_string(),
                    name,
                };
                if device.supports_input() {
                    catalog.inputs.push(info.clone());
                }
                if device.supports_output() {
                    catalog.outputs.push(info);
                }
            }
        }
        Err(error) => catalog.error = Some(error.to_string()),
    }

    catalog
        .inputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    catalog
        .outputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    catalog
}

/// An active pair of device streams. Input is captured into a lock-free mono
/// ring buffer and exposed as a peak meter; output can play project audio, a
/// test tone, or monitor that input.
pub struct AudioEngine {
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    sample_rate: u32,
    input_peak: Arc<AtomicU32>,
    error: Arc<AudioErrorState>,
    input_stream: Option<Stream>,
    output_stream: Option<Stream>,
    input_active: bool,
    output_active: bool,
    #[cfg(windows)]
    exclusive_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(windows)]
    exclusive_threads: Vec<std::thread::JoinHandle<()>>,
}

impl AudioEngine {
    pub fn start(settings: &AudioSettings) -> Result<Self, String> {
        validate_settings(settings)?;
        match settings.access {
            AudioAccess::Shared => Self::start_shared(settings),
            AudioAccess::Exclusive => Self::start_exclusive(settings),
        }
    }

    pub fn set_test_tone(&self, enabled: bool) {
        if enabled {
            self.playback.pause();
        }
        self.source.store(
            if enabled {
                AUDIO_SOURCE_TEST_TONE
            } else {
                AUDIO_SOURCE_SILENT
            },
            Ordering::Release,
        );
    }

    pub fn set_input_monitor(&self, enabled: bool) -> Result<(), String> {
        if enabled && (!self.input_active || !self.output_active) {
            return Err(
                "Select and start both an input and an output device to monitor input".into(),
            );
        }
        if enabled {
            self.playback.pause();
        }
        self.source.store(
            if enabled {
                AUDIO_SOURCE_INPUT_MONITOR
            } else {
                AUDIO_SOURCE_SILENT
            },
            Ordering::Release,
        );
        Ok(())
    }

    /// Start outputting interleaved stereo project audio at the device sample rate.
    pub fn set_project_playback(&self, samples: Vec<f32>) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before playing the project".into());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("Rendered project audio must contain stereo sample frames".into());
        }
        self.playback.start(samples);
        self.source.store(AUDIO_SOURCE_PROJECT, Ordering::Release);
        Ok(())
    }

    /// Play a browser sample preview alongside any active project audio.
    pub fn set_browser_preview(&self, samples: Vec<f32>) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before previewing a sample".into());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("Sample preview audio must contain stereo frames".into());
        }
        self.playback.start_browser_preview(samples);
        Ok(())
    }

    /// Stop the independent Browser preview voice without stopping project playback.
    pub fn stop_browser_preview(&self) {
        self.playback.stop_browser_preview();
    }

    pub fn browser_preview_active(&self) -> bool {
        self.playback.browser_preview_active()
    }

    pub fn set_browser_preview_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.playback
            .browser_preview_gain
            .store(gain.to_bits(), Ordering::Release);
    }

    /// Begin consuming interleaved stereo samples from a bounded producer ring.
    ///
    /// VST3 and other block processors can render on a worker thread while the shared or
    /// WASAPI-exclusive device callback pulls frames without allocating or locking.
    pub fn begin_streaming_playback(&self) -> Result<StreamingAudioWriter, String> {
        if !self.output_active {
            return Err("Start an output device before streaming audio".into());
        }
        let writer = self.playback.start_streaming();
        self.source.store(AUDIO_SOURCE_STREAM, Ordering::Release);
        Ok(writer)
    }

    pub fn pause_project_playback(&self) {
        self.playback.pause();
        self.stop_playback_source();
    }

    pub fn resume_project_playback(&self) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before resuming project playback".into());
        }
        self.playback.resume()?;
        self.source.store(
            if self.playback.is_streaming() {
                AUDIO_SOURCE_STREAM
            } else {
                AUDIO_SOURCE_PROJECT
            },
            Ordering::Release,
        );
        Ok(())
    }

    pub fn stop_project_playback(&self) {
        self.playback.stop();
        self.stop_playback_source();
    }

    pub fn project_playback_active(&self) -> bool {
        self.playback.is_active()
    }

    /// Number of device frames that arrived before streamed audio was ready.
    pub fn streaming_underrun_frames(&self) -> u64 {
        self.playback
            .streaming
            .load()
            .as_deref()
            .map(|streaming| streaming.underrun_frames.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Number of project audio frames consumed by the device since the current play started.
    pub fn project_playback_position_frames(&self) -> u64 {
        self.playback.position_frames()
    }

    pub fn input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.load(Ordering::Acquire)).clamp(0.0, 1.0)
    }

    pub fn input_level_db(&self) -> f32 {
        let peak = self.input_peak().max(0.000_001);
        20.0 * peak.log10()
    }

    pub fn input_active(&self) -> bool {
        self.input_active
    }

    /// Number of device stream errors reported since this engine started.
    pub fn stream_error_count(&self) -> u64 {
        self.error.count.load(Ordering::Relaxed)
    }

    /// Recent device stream error messages, oldest first.
    pub fn stream_error_history(&self) -> Vec<String> {
        self.error.recent()
    }

    pub fn output_active(&self) -> bool {
        self.output_active
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.take_latest()
    }

    fn stop_playback_source(&self) {
        for playback_source in [AUDIO_SOURCE_PROJECT, AUDIO_SOURCE_STREAM] {
            let _ = self.source.compare_exchange(
                playback_source,
                AUDIO_SOURCE_SILENT,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    fn start_shared(settings: &AudioSettings) -> Result<Self, String> {
        let host = cpal::default_host();
        let ring = Arc::new(AudioRingBuffer::new(AUDIO_RING_CAPACITY));
        let source = Arc::new(AtomicU8::new(AUDIO_SOURCE_SILENT));
        let playback = Arc::new(PlaybackState::new());
        let input_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let error = Arc::new(AudioErrorState::default());

        let output_device = if settings.enable_output {
            resolve_device(&host, settings.output_device_id.as_deref(), false)?
        } else {
            None
        };
        let input_device = if settings.enable_input {
            resolve_device(&host, settings.input_device_id.as_deref(), true)?
        } else {
            None
        };
        let mut output_stream = None;
        let mut input_stream = None;

        if let Some(device) = output_device {
            let stream = build_output_stream(
                &device,
                settings,
                Arc::clone(&source),
                Arc::clone(&playback),
                Arc::clone(&ring),
                Arc::clone(&error),
            )?;
            stream
                .play()
                .map_err(|error| format!("Could not start output stream: {error}"))?;
            output_stream = Some(stream);
        }

        if let Some(device) = input_device {
            let stream = build_input_stream(
                &device,
                settings,
                Arc::clone(&ring),
                Arc::clone(&input_peak),
                Arc::clone(&source),
                Arc::clone(&error),
            )?;
            stream
                .play()
                .map_err(|error| format!("Could not start input stream: {error}"))?;
            input_stream = Some(stream);
        }

        if output_stream.is_none() && input_stream.is_none() {
            return Err("No active input or output device is available".into());
        }

        Ok(Self {
            source,
            playback,
            sample_rate: settings.sample_rate,
            input_peak,
            error,
            input_active: input_stream.is_some(),
            output_active: output_stream.is_some(),
            input_stream,
            output_stream,
            #[cfg(windows)]
            exclusive_stop: None,
            #[cfg(windows)]
            exclusive_threads: Vec::new(),
        })
    }

    #[cfg(windows)]
    fn start_exclusive(settings: &AudioSettings) -> Result<Self, String> {
        use std::sync::mpsc;
        use std::thread;

        let ring = Arc::new(AudioRingBuffer::new(AUDIO_RING_CAPACITY));
        let source = Arc::new(AtomicU8::new(AUDIO_SOURCE_SILENT));
        let playback = Arc::new(PlaybackState::new());
        let input_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let error = Arc::new(AudioErrorState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads: Vec<thread::JoinHandle<()>> = Vec::new();
        let mut receivers = Vec::new();

        if settings.enable_output
            && (settings.output_device_id.is_some()
                || cpal::default_host().default_output_device().is_some())
        {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker_settings = settings.clone();
            let worker_source = Arc::clone(&source);
            let worker_playback = Arc::clone(&playback);
            let worker_ring = Arc::clone(&ring);
            let worker_peak = Arc::clone(&input_peak);
            let worker_error = Arc::clone(&error);
            let worker_stop = Arc::clone(&stop);
            let output_thread = thread::Builder::new()
                .name("wasapi-exclusive-render".into())
                .spawn(move || {
                    wasapi_exclusive_output_worker(
                        worker_settings.output_device_id.as_deref(),
                        &worker_settings,
                        WasapiWorkerState {
                            source: worker_source,
                            playback: worker_playback,
                            ring: worker_ring,
                            input_peak: worker_peak,
                            error: worker_error,
                            stop: worker_stop,
                        },
                        ready_tx,
                    );
                });
            match output_thread {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads.drain(..) {
                        let _ = thread.join();
                    }
                    return Err(format!("Could not start WASAPI output worker: {error}"));
                }
            }
            receivers.push(("output", ready_rx));
        }

        if settings.enable_input
            && (settings.input_device_id.is_some()
                || cpal::default_host().default_input_device().is_some())
        {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker_settings = settings.clone();
            let worker_ring = Arc::clone(&ring);
            let worker_peak = Arc::clone(&input_peak);
            let worker_source = Arc::clone(&source);
            let worker_playback = Arc::clone(&playback);
            let worker_error = Arc::clone(&error);
            let worker_stop = Arc::clone(&stop);
            let input_thread = thread::Builder::new()
                .name("wasapi-exclusive-capture".into())
                .spawn(move || {
                    wasapi_exclusive_input_worker(
                        worker_settings.input_device_id.as_deref(),
                        &worker_settings,
                        WasapiWorkerState {
                            source: worker_source,
                            playback: worker_playback,
                            ring: worker_ring,
                            input_peak: worker_peak,
                            error: worker_error,
                            stop: worker_stop,
                        },
                        ready_tx,
                    );
                });
            match input_thread {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads.drain(..) {
                        let _ = thread.join();
                    }
                    return Err(format!("Could not start WASAPI input worker: {error}"));
                }
            }
            receivers.push(("input", ready_rx));
        }

        if receivers.is_empty() {
            return Err("No active input or output device is available".into());
        }

        for (direction, receiver) in receivers {
            match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(format!("WASAPI exclusive {direction} failed: {message}"));
                }
                Err(error_kind) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(format!(
                        "WASAPI exclusive {direction} did not start: {error_kind}"
                    ));
                }
            }
        }

        let output_active = settings.enable_output
            && (settings.output_device_id.is_some()
                || cpal::default_host().default_output_device().is_some());
        let input_active = settings.enable_input
            && (settings.input_device_id.is_some()
                || cpal::default_host().default_input_device().is_some());
        Ok(Self {
            source,
            playback,
            sample_rate: settings.sample_rate,
            input_peak,
            error,
            input_active,
            output_active,
            input_stream: None,
            output_stream: None,
            exclusive_stop: Some(stop),
            exclusive_threads: threads,
        })
    }

    #[cfg(not(windows))]
    fn start_exclusive(_settings: &AudioSettings) -> Result<Self, String> {
        Err("Exclusive device access is available through WASAPI on Windows".into())
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.playback.stop();
        #[cfg(windows)]
        if let Some(stop) = &self.exclusive_stop {
            stop.store(true, Ordering::Release);
            for thread in self.exclusive_threads.drain(..) {
                let _ = thread.join();
            }
        }
        // Explicit field reads keep the streams alive for the lifetime of the engine.
        let _ = (&self.input_stream, &self.output_stream);
    }
}

fn validate_settings(settings: &AudioSettings) -> Result<(), String> {
    if !settings.enable_input && !settings.enable_output {
        return Err("Enable input, output, or both before starting audio".into());
    }
    if !(8_000..=384_000).contains(&settings.sample_rate) {
        return Err("Sample rate must be between 8,000 and 384,000 Hz".into());
    }
    if !(32..=8_192).contains(&settings.buffer_frames) {
        return Err("Audio buffer must be between 32 and 8,192 frames".into());
    }
    Ok(())
}

fn resolve_device(
    host: &cpal::Host,
    selected_id: Option<&str>,
    input: bool,
) -> Result<Option<cpal::Device>, String> {
    match selected_id {
        Some(selected_id) => {
            let id = selected_id
                .parse::<cpal::DeviceId>()
                .map_err(|error| format!("Invalid audio device id: {error}"))?;
            host.device_by_id(&id).map(Some).ok_or_else(|| {
                format!("Selected audio device is no longer available: {selected_id}")
            })
        }
        None if input => Ok(host.default_input_device()),
        None => Ok(host.default_output_device()),
    }
}

fn select_config(
    device: &cpal::Device,
    input: bool,
    sample_rate: u32,
) -> Result<SupportedStreamConfig, String> {
    let configs = if input {
        device
            .supported_input_configs()
            .map_err(|error| format!("Could not query input formats: {error}"))?
            .collect::<Vec<_>>()
    } else {
        device
            .supported_output_configs()
            .map_err(|error| format!("Could not query output formats: {error}"))?
            .collect::<Vec<_>>()
    };
    configs
        .into_iter()
        .filter(|config| {
            config.min_sample_rate() <= sample_rate && sample_rate <= config.max_sample_rate()
        })
        .max_by_key(|config| {
            let channels = u32::from(config.channels());
            let format_score = match config.sample_format() {
                SampleFormat::F32 => 8,
                SampleFormat::I16 => 7,
                SampleFormat::F64 => 6,
                SampleFormat::I24 => 5,
                SampleFormat::I32 => 4,
                SampleFormat::U16 => 3,
                _ => 0,
            };
            (channels == 2, format_score, channels)
        })
        .map(|config| config.with_sample_rate(sample_rate))
        .ok_or_else(|| {
            format!(
                "The selected {} device does not support {sample_rate} Hz",
                if input { "input" } else { "output" }
            )
        })
}

fn build_output_stream(
    device: &cpal::Device,
    settings: &AudioSettings,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    let supported = select_config(device, false, settings.sample_rate)?;
    let format = supported.sample_format();
    let mut config = supported.config();
    config.buffer_size = cpal::BufferSize::Fixed(settings.buffer_frames);
    match build_typed_output(
        device,
        config,
        format,
        Arc::clone(&source),
        Arc::clone(&playback),
        Arc::clone(&ring),
        Arc::clone(&error),
    ) {
        Ok(stream) => Ok(stream),
        Err(first_error) => {
            let mut default_config = supported.config();
            default_config.buffer_size = cpal::BufferSize::Default;
            build_typed_output(
                device,
                default_config,
                format,
                source,
                playback,
                ring,
                error,
            )
            .map_err(|error| format!("{error} (requested buffer was also rejected: {first_error})"))
        }
    }
}

fn build_typed_output(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_tone_stream::<$sample>(device, config, source, playback, ring, error)
        };
    }
    match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        unsupported => Err(format!(
            "Unsupported audio output sample format: {unsupported}"
        )),
    }
}

fn build_tone_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + FromSample<f32>,
{
    let sample_rate = config.sample_rate as f32;
    let channels = usize::from(config.channels).max(1);
    let mut phase = 0.0_f32;
    let playback_for_callback = Arc::clone(&playback);
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |output, _| {
                enable_denormal_protection();
                let playback_samples = playback_for_callback.samples.load();
                let streaming = playback_for_callback.streaming.load();
                let browser_preview_samples = playback_for_callback.browser_preview_samples.load();
                for frame in output.chunks_mut(channels) {
                    let [left, right] = next_output_frame_with_browser_preview(AudioFrameInputs {
                        source: &source,
                        ring: &ring,
                        project_samples: playback_samples.as_deref().map(Vec::as_slice),
                        streaming: streaming.as_deref(),
                        playback: &playback_for_callback,
                        browser_preview_samples: browser_preview_samples
                            .as_deref()
                            .map(Vec::as_slice),
                        sample_rate,
                        phase: &mut phase,
                    });
                    for (channel, destination) in frame.iter_mut().enumerate() {
                        let sample = match channel {
                            0 => left,
                            1 => right,
                            _ => 0.0,
                        };
                        *destination = T::from_sample(sample.clamp(-1.0, 1.0));
                    }
                }
            },
            move |stream_error| set_error(&error, stream_error.to_string()),
            Some(Duration::from_secs(5)),
        )
        .map_err(|error| format!("Could not build output stream: {error}"))?;
    Ok(stream)
}

fn build_input_stream(
    device: &cpal::Device,
    settings: &AudioSettings,
    ring: Arc<AudioRingBuffer>,
    peak: Arc<AtomicU32>,
    source: Arc<AtomicU8>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    let supported = select_config(device, true, settings.sample_rate)?;
    let format = supported.sample_format();
    let mut config = supported.config();
    config.buffer_size = cpal::BufferSize::Fixed(settings.buffer_frames);
    match build_typed_input(
        device,
        config,
        format,
        Arc::clone(&ring),
        Arc::clone(&peak),
        Arc::clone(&source),
        Arc::clone(&error),
    ) {
        Ok(stream) => Ok(stream),
        Err(first_error) => {
            let mut default_config = supported.config();
            default_config.buffer_size = cpal::BufferSize::Default;
            build_typed_input(device, default_config, format, ring, peak, source, error).map_err(
                |error| format!("{error} (requested buffer was also rejected: {first_error})"),
            )
        }
    }
}

fn build_typed_input(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    ring: Arc<AudioRingBuffer>,
    peak: Arc<AtomicU32>,
    source: Arc<AtomicU8>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_capture_stream::<$sample>(device, config, ring, peak, source, error)
        };
    }
    match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        unsupported => Err(format!(
            "Unsupported audio input sample format: {unsupported}"
        )),
    }
}

fn build_capture_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: Arc<AudioRingBuffer>,
    peak: Arc<AtomicU32>,
    source: Arc<AtomicU8>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + Copy,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |input, _| {
                enable_denormal_protection();
                let mut max_peak = 0.0_f32;
                for frame in input.chunks(channels) {
                    if frame.is_empty() {
                        continue;
                    }
                    let mut mono = 0.0_f32;
                    for sample in frame {
                        let value = f32::from_sample(*sample).clamp(-1.0, 1.0);
                        mono += value;
                        max_peak = max_peak.max(value.abs());
                    }
                    if source.load(Ordering::Acquire) == AUDIO_SOURCE_INPUT_MONITOR {
                        ring.push(mono / frame.len() as f32);
                    }
                }
                peak.store(max_peak.to_bits(), Ordering::Release);
            },
            move |stream_error| set_error(&error, stream_error.to_string()),
            Some(Duration::from_secs(5)),
        )
        .map_err(|error| format!("Could not build input stream: {error}"))?;
    Ok(stream)
}

fn next_output_frame(
    source: &AtomicU8,
    ring: &AudioRingBuffer,
    project_samples: Option<&[f32]>,
    streaming: Option<&StreamingPlayback>,
    playback: &PlaybackState,
    sample_rate: f32,
    phase: &mut f32,
) -> [f32; 2] {
    match source.load(Ordering::Acquire) {
        AUDIO_SOURCE_TEST_TONE => {
            let (value, next_phase) = tone_sample(*phase, sample_rate);
            *phase = next_phase;
            [value, value]
        }
        AUDIO_SOURCE_INPUT_MONITOR => {
            let value = ring.pop().unwrap_or(0.0);
            [value, value]
        }
        AUDIO_SOURCE_PROJECT => {
            if !playback.active.load(Ordering::Acquire) {
                return [0.0, 0.0];
            }
            let frame = playback.cursor_frames.fetch_add(1, Ordering::Relaxed);
            let Some(offset) = usize::try_from(frame)
                .ok()
                .and_then(|frame| frame.checked_mul(2))
            else {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            };
            let Some(samples) =
                project_samples.and_then(|samples| samples.get(offset..offset.saturating_add(2)))
            else {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            };
            [samples[0], samples[1]]
        }
        AUDIO_SOURCE_STREAM => {
            let Some(streaming) = streaming else {
                return [0.0, 0.0];
            };
            if streaming.cancelled.load(Ordering::Acquire)
                || streaming.paused.load(Ordering::Acquire)
            {
                return [0.0, 0.0];
            }
            if !streaming.active.load(Ordering::Acquire) {
                let available = streaming.ring.available_stereo_frames();
                if available < STREAM_PREFILL_FRAMES
                    && !(streaming.finished.load(Ordering::Acquire) && available > 0)
                {
                    return [0.0, 0.0];
                }
                streaming.active.store(true, Ordering::Release);
            }
            if let Some(frame) = streaming.ring.pop_stereo_frame() {
                streaming.consumed_frames.fetch_add(1, Ordering::Relaxed);
                frame
            } else if streaming.finished.load(Ordering::Acquire) {
                streaming.active.store(false, Ordering::Release);
                [0.0, 0.0]
            } else {
                streaming.underrun_frames.fetch_add(1, Ordering::Relaxed);
                streaming.consumed_frames.fetch_add(1, Ordering::Relaxed);
                [0.0, 0.0]
            }
        }
        _ => [0.0, 0.0],
    }
}

struct AudioFrameInputs<'a> {
    source: &'a AtomicU8,
    ring: &'a AudioRingBuffer,
    project_samples: Option<&'a [f32]>,
    streaming: Option<&'a StreamingPlayback>,
    playback: &'a PlaybackState,
    browser_preview_samples: Option<&'a [f32]>,
    sample_rate: f32,
    phase: &'a mut f32,
}

fn next_output_frame_with_browser_preview(inputs: AudioFrameInputs<'_>) -> [f32; 2] {
    let AudioFrameInputs {
        source,
        ring,
        project_samples,
        streaming,
        playback,
        browser_preview_samples,
        sample_rate,
        phase,
    } = inputs;
    let project = next_output_frame(
        source,
        ring,
        project_samples,
        streaming,
        playback,
        sample_rate,
        phase,
    );
    let preview = playback.next_browser_preview_frame(browser_preview_samples);
    [
        (project[0] + preview[0]).clamp(-1.0, 1.0),
        (project[1] + preview[1]).clamp(-1.0, 1.0),
    ]
}

fn tone_sample(phase: f32, sample_rate: f32) -> (f32, f32) {
    let value = (phase * std::f32::consts::TAU).sin() * TEST_TONE_LEVEL;
    let next_phase = (phase + TEST_TONE_HZ / sample_rate.max(1.0)).fract();
    (value, next_phase)
}

fn set_error(error: &AudioErrorState, message: String) {
    error.report(message);
}

struct AudioRingBuffer {
    samples: Box<[AtomicU32]>,
    write_index: AtomicU64,
    read_index: AtomicU64,
}

impl AudioRingBuffer {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        let samples = (0..capacity)
            .map(|_| AtomicU32::new(0.0_f32.to_bits()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            samples,
            write_index: AtomicU64::new(0),
            read_index: AtomicU64::new(0),
        }
    }

    fn push(&self, sample: f32) -> bool {
        let write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= self.samples.len() as u64 {
            return false;
        }
        self.samples[write as usize % self.samples.len()]
            .store(sample.to_bits(), Ordering::Relaxed);
        self.write_index
            .store(write.wrapping_add(1), Ordering::Release);
        true
    }

    fn push_stereo_frame(&self, left: f32, right: f32) -> bool {
        let write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        if write.wrapping_sub(read).saturating_add(2) > self.samples.len() as u64 {
            return false;
        }
        let capacity = self.samples.len();
        self.samples[write as usize % capacity].store(left.to_bits(), Ordering::Relaxed);
        self.samples[(write as usize + 1) % capacity].store(right.to_bits(), Ordering::Relaxed);
        self.write_index
            .store(write.wrapping_add(2), Ordering::Release);
        true
    }

    fn pop_stereo_frame(&self) -> Option<[f32; 2]> {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        if write.wrapping_sub(read) < 2 {
            return None;
        }
        let capacity = self.samples.len();
        let frame = [
            f32::from_bits(self.samples[read as usize % capacity].load(Ordering::Relaxed)),
            f32::from_bits(self.samples[(read as usize + 1) % capacity].load(Ordering::Relaxed)),
        ];
        self.read_index
            .store(read.wrapping_add(2), Ordering::Release);
        Some(frame)
    }

    fn available_stereo_frames(&self) -> usize {
        let write = self.write_index.load(Ordering::Acquire);
        let read = self.read_index.load(Ordering::Acquire);
        (write.wrapping_sub(read) / 2).min(usize::MAX as u64) as usize
    }

    fn pop(&self) -> Option<f32> {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let sample = f32::from_bits(
            self.samples[read as usize % self.samples.len()].load(Ordering::Relaxed),
        );
        self.read_index
            .store(read.wrapping_add(1), Ordering::Release);
        Some(sample)
    }
}

#[cfg(windows)]
fn wasapi_device_id(cpal_id: &str) -> Result<&str, String> {
    cpal_id
        .strip_prefix("wasapi:")
        .ok_or_else(|| format!("Selected endpoint is not a WASAPI device: {cpal_id}"))
}

#[cfg(windows)]
fn set_wasapi_error(error: &AudioErrorState, message: String) {
    set_error(error, message);
}

#[cfg(windows)]
struct WasapiWorkerState {
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    input_peak: Arc<AtomicU32>,
    error: Arc<AudioErrorState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(windows)]
fn wasapi_exclusive_output_worker(
    selected_id: Option<&str>,
    settings: &AudioSettings,
    state: WasapiWorkerState,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    enable_denormal_protection();
    let WasapiWorkerState {
        source,
        playback,
        ring,
        error,
        stop,
        ..
    } = state;
    let mut started = false;
    let result = (|| -> Result<(), String> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|error| format!("Could not initialize Windows audio COM: {error}"))?;
        let _com = WasapiComGuard;
        let enumerator = wasapi::DeviceEnumerator::new().map_err(|error| error.to_string())?;
        let device = if let Some(id) = selected_id {
            enumerator
                .get_device(wasapi_device_id(id)?)
                .map_err(|error| error.to_string())?
        } else {
            enumerator
                .get_default_device(&wasapi::Direction::Render)
                .map_err(|error| error.to_string())?
        };
        let mut client = device
            .get_iaudioclient()
            .map_err(|error| error.to_string())?;
        let format = select_exclusive_format(&mut client, &device, settings.sample_rate)?;
        let format_spec = WasapiSampleSpec::from_format(&format)?;
        let period = exclusive_period(&client, &format, settings.buffer_frames)?;
        client
            .initialize_client(
                &format,
                &wasapi::Direction::Render,
                &wasapi::StreamMode::EventsExclusive { period_hns: period },
            )
            .map_err(|error| format_exclusive_error("output", error.to_string()))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|error| error.to_string())?;
        let render = client
            .get_audiorenderclient()
            .map_err(|error| error.to_string())?;
        let mut phase = 0.0_f32;
        let initial_frames = client
            .get_available_space_in_frames()
            .map_err(|error| error.to_string())? as usize;
        let initial = render_wasapi_buffer(
            initial_frames,
            &format_spec,
            &source,
            &playback,
            &ring,
            &mut phase,
        )?;
        render
            .write_to_device(initial_frames, &initial, None)
            .map_err(|error| error.to_string())?;
        client
            .start_stream()
            .map_err(|error| format_exclusive_error("output", error.to_string()))?;
        started = true;
        let _ = ready.send(Ok(()));

        while !stop.load(Ordering::Acquire) {
            match event.wait_for_event(100) {
                Ok(()) => {
                    let frames = client
                        .get_available_space_in_frames()
                        .map_err(|error| error.to_string())?
                        as usize;
                    let bytes = render_wasapi_buffer(
                        frames,
                        &format_spec,
                        &source,
                        &playback,
                        &ring,
                        &mut phase,
                    )?;
                    render
                        .write_to_device(frames, &bytes, None)
                        .map_err(|error| error.to_string())?;
                }
                Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        client.stop_stream().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(message) = result {
        set_wasapi_error(
            &error,
            format!("WASAPI exclusive output stopped: {message}"),
        );
        if !started {
            let _ = ready.send(Err(message));
        }
    }
}

#[cfg(windows)]
fn wasapi_exclusive_input_worker(
    selected_id: Option<&str>,
    settings: &AudioSettings,
    state: WasapiWorkerState,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    enable_denormal_protection();
    let WasapiWorkerState {
        ring,
        input_peak: peak,
        source,
        error,
        stop,
        ..
    } = state;
    let mut started = false;
    let result = (|| -> Result<(), String> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|error| format!("Could not initialize Windows audio COM: {error}"))?;
        let _com = WasapiComGuard;
        let enumerator = wasapi::DeviceEnumerator::new().map_err(|error| error.to_string())?;
        let device = if let Some(id) = selected_id {
            enumerator
                .get_device(wasapi_device_id(id)?)
                .map_err(|error| error.to_string())?
        } else {
            enumerator
                .get_default_device(&wasapi::Direction::Capture)
                .map_err(|error| error.to_string())?
        };
        let mut client = device
            .get_iaudioclient()
            .map_err(|error| error.to_string())?;
        let format = select_exclusive_format(&mut client, &device, settings.sample_rate)?;
        let format_spec = WasapiSampleSpec::from_format(&format)?;
        let period = exclusive_period(&client, &format, settings.buffer_frames)?;
        client
            .initialize_client(
                &format,
                &wasapi::Direction::Capture,
                &wasapi::StreamMode::EventsExclusive { period_hns: period },
            )
            .map_err(|error| format_exclusive_error("input", error.to_string()))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|error| error.to_string())?;
        let buffer_frames = client
            .get_buffer_size()
            .map_err(|error| error.to_string())? as usize;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|error| error.to_string())?;
        let mut bytes = vec![0_u8; buffer_frames.saturating_mul(format_spec.frame_bytes)];
        client
            .start_stream()
            .map_err(|error| format_exclusive_error("input", error.to_string()))?;
        started = true;
        let _ = ready.send(Ok(()));

        while !stop.load(Ordering::Acquire) {
            match event.wait_for_event(100) {
                Ok(()) => {
                    let (frames, _) = capture
                        .read_from_device(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    let active_bytes = (frames as usize).saturating_mul(format_spec.frame_bytes);
                    let mut max_peak = 0.0_f32;
                    for frame in bytes[..active_bytes].chunks_exact(format_spec.frame_bytes) {
                        let mut mono = 0.0_f32;
                        for sample_bytes in frame.chunks_exact(format_spec.sample_bytes) {
                            let sample = format_spec.decode(sample_bytes)?;
                            max_peak = max_peak.max(sample.abs());
                            mono += sample;
                        }
                        if source.load(Ordering::Acquire) == AUDIO_SOURCE_INPUT_MONITOR {
                            ring.push((mono / format_spec.channels as f32).clamp(-1.0, 1.0));
                        }
                    }
                    peak.store(max_peak.clamp(0.0, 1.0).to_bits(), Ordering::Release);
                }
                Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        client.stop_stream().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(message) = result {
        set_wasapi_error(&error, format!("WASAPI exclusive input stopped: {message}"));
        if !started {
            let _ = ready.send(Err(message));
        }
    }
}

#[cfg(windows)]
struct WasapiComGuard;

#[cfg(windows)]
impl Drop for WasapiComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[cfg(windows)]
fn select_exclusive_format(
    client: &mut wasapi::AudioClient,
    device: &wasapi::Device,
    sample_rate: u32,
) -> Result<wasapi::WaveFormat, String> {
    let device_format = device
        .get_device_format()
        .map_err(|error| format!("Could not read the device format: {error}"))?;
    let channels = usize::from(device_format.get_nchannels().max(1));
    let candidates = [
        (32, 32, wasapi::SampleType::Float),
        (32, 32, wasapi::SampleType::Int),
        (24, 24, wasapi::SampleType::Int),
        (16, 16, wasapi::SampleType::Int),
    ];
    for (storage_bits, valid_bits, sample_type) in candidates {
        let requested = wasapi::WaveFormat::new(
            storage_bits,
            valid_bits,
            &sample_type,
            sample_rate as usize,
            channels,
            None,
        );
        if let Ok(format) = client.is_supported_exclusive_with_quirks(&requested) {
            return Ok(format);
        }
    }
    Err(format!(
        "The device does not support an exclusive 32-bit float, 32/24/16-bit PCM stream at {sample_rate} Hz"
    ))
}

#[cfg(windows)]
fn exclusive_period(
    client: &wasapi::AudioClient,
    format: &wasapi::WaveFormat,
    buffer_frames: u32,
) -> Result<i64, String> {
    let requested_hns =
        (i64::from(buffer_frames) * 10_000_000) / i64::from(format.get_samplespersec().max(1));
    client
        .calculate_aligned_period_near(requested_hns, None, format)
        .map_err(|error| format!("Could not choose an exclusive buffer period: {error}"))
}

#[cfg(windows)]
fn format_exclusive_error(direction: &str, message: String) -> String {
    let lower = message.to_lowercase();
    if lower.contains("device in use") || lower.contains("already in use") {
        format!("The selected {direction} device is already in use by another application")
    } else if lower.contains("unsupported format") || lower.contains("unsupportedformat") {
        format!("The selected {direction} device does not support the requested exclusive format")
    } else if lower.contains("exclusive mode is not allowed") {
        format!("Windows does not allow exclusive access to the selected {direction} device")
    } else {
        message
    }
}

#[cfg(windows)]
struct WasapiSampleSpec {
    sample_type: wasapi::SampleType,
    storage_bits: usize,
    valid_bits: usize,
    channels: usize,
    sample_bytes: usize,
    frame_bytes: usize,
    sample_rate: u32,
}

#[cfg(windows)]
impl WasapiSampleSpec {
    fn from_format(format: &wasapi::WaveFormat) -> Result<Self, String> {
        let storage_bits = usize::from(format.get_bitspersample());
        let valid_bits = usize::from(format.get_validbitspersample());
        let channels = usize::from(format.get_nchannels());
        if storage_bits == 0
            || storage_bits % 8 != 0
            || valid_bits == 0
            || valid_bits > storage_bits
            || channels == 0
        {
            return Err("WASAPI returned an unsupported sample layout".into());
        }
        let sample_type = format
            .get_subformat()
            .map_err(|error| format!("Unsupported WASAPI sample type: {error}"))?;
        let sample_bytes = storage_bits / 8;
        let frame_bytes = usize::try_from(format.get_blockalign())
            .map_err(|_| "Invalid WASAPI frame size".to_owned())?;
        if frame_bytes != sample_bytes.saturating_mul(channels) {
            return Err("WASAPI sample layout has an unexpected frame size".into());
        }
        Ok(Self {
            sample_type,
            storage_bits,
            valid_bits,
            channels,
            sample_bytes,
            frame_bytes,
            sample_rate: format.get_samplespersec(),
        })
    }

    fn encode(&self, sample: f32, output: &mut [u8]) -> Result<(), String> {
        if output.len() != self.sample_bytes {
            return Err("Internal WASAPI sample buffer size mismatch".into());
        }
        let sample = sample.clamp(-1.0, 1.0);
        match self.sample_type {
            wasapi::SampleType::Float => match self.storage_bits {
                32 => output.copy_from_slice(&sample.to_le_bytes()),
                64 => output.copy_from_slice(&(f64::from(sample)).to_le_bytes()),
                bits => return Err(format!("Unsupported WASAPI float depth: {bits}")),
            },
            wasapi::SampleType::Int if self.storage_bits == 8 => {
                output[0] = ((sample + 1.0) * 127.5).round().clamp(0.0, 255.0) as u8;
            }
            wasapi::SampleType::Int => {
                let max_positive = (1_i128 << (self.valid_bits - 1)) - 1;
                let value = (f64::from(sample) * max_positive as f64).round() as i128;
                let aligned = value << (self.storage_bits - self.valid_bits);
                let raw = aligned.to_le_bytes();
                output.copy_from_slice(&raw[..self.sample_bytes]);
            }
        }
        Ok(())
    }

    fn decode(&self, input: &[u8]) -> Result<f32, String> {
        if input.len() != self.sample_bytes {
            return Err("Internal WASAPI input buffer size mismatch".into());
        }
        let value = match self.sample_type {
            wasapi::SampleType::Float => match self.storage_bits {
                32 => f32::from_le_bytes(input.try_into().expect("validated 32-bit sample")),
                64 => f64::from_le_bytes(input.try_into().expect("validated 64-bit sample")) as f32,
                bits => return Err(format!("Unsupported WASAPI float depth: {bits}")),
            },
            wasapi::SampleType::Int if self.storage_bits == 8 => {
                (i16::from(input[0]) - 128) as f32 / 128.0
            }
            wasapi::SampleType::Int => {
                let raw = input.iter().enumerate().fold(0_u128, |acc, (index, byte)| {
                    acc | (u128::from(*byte) << (index * 8))
                });
                let sign = 1_u128 << (self.storage_bits - 1);
                let signed = if raw & sign != 0 {
                    raw as i128 - (1_i128 << self.storage_bits)
                } else {
                    raw as i128
                };
                let aligned = signed >> (self.storage_bits - self.valid_bits);
                let denominator = (1_u128 << (self.valid_bits - 1)) as f64;
                (aligned as f64 / denominator) as f32
            }
        };
        Ok(if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        })
    }
}

#[cfg(windows)]
fn render_wasapi_buffer(
    frames: usize,
    format: &WasapiSampleSpec,
    source: &AtomicU8,
    playback: &PlaybackState,
    ring: &AudioRingBuffer,
    phase: &mut f32,
) -> Result<Vec<u8>, String> {
    let mut output = vec![0_u8; frames.saturating_mul(format.frame_bytes)];
    let sample_rate = format.get_sample_rate();
    let playback_samples = playback.samples.load();
    let streaming = playback.streaming.load();
    let browser_preview_samples = playback.browser_preview_samples.load();
    for frame in output.chunks_exact_mut(format.frame_bytes) {
        let stereo = next_output_frame_with_browser_preview(AudioFrameInputs {
            source,
            ring,
            project_samples: playback_samples.as_deref().map(Vec::as_slice),
            streaming: streaming.as_deref(),
            playback,
            browser_preview_samples: browser_preview_samples.as_deref().map(Vec::as_slice),
            sample_rate,
            phase,
        });
        for (channel_index, channel) in frame.chunks_exact_mut(format.sample_bytes).enumerate() {
            let sample = match channel_index {
                0 => stereo[0],
                1 => stereo[1],
                _ => 0.0,
            };
            format.encode(sample, channel)?;
        }
    }
    Ok(output)
}

#[cfg(windows)]
impl WasapiSampleSpec {
    fn get_sample_rate(&self) -> f32 {
        self.sample_rate.max(1) as f32
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::{
        AUDIO_SOURCE_PROJECT, AUDIO_SOURCE_STREAM, AudioAccess, AudioRingBuffer, AudioSettings,
        PlaybackState, enable_denormal_protection, next_output_frame, tone_sample,
        validate_settings,
    };

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn denormal_protection_enables_the_architecture_flush_mode() {
        #[cfg(target_arch = "x86_64")]
        {
            use super::{read_mxcsr, write_mxcsr};
            const FTZ_AND_DAZ: u32 = (1 << 15) | (1 << 6);
            let previous = read_mxcsr();
            enable_denormal_protection();
            let enabled = read_mxcsr();
            write_mxcsr(previous);
            assert_eq!(enabled & FTZ_AND_DAZ, FTZ_AND_DAZ);
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            let previous: u64;
            std::arch::asm!(
                "mrs {control}, fpcr",
                control = out(reg) previous,
                options(nostack, preserves_flags)
            );
            enable_denormal_protection();
            let enabled: u64;
            std::arch::asm!(
                "mrs {control}, fpcr",
                control = out(reg) enabled,
                options(nostack, preserves_flags)
            );
            std::arch::asm!(
                "msr fpcr, {control}",
                control = in(reg) previous,
                options(nostack, preserves_flags)
            );
            assert_ne!(enabled & (1 << 24), 0);
        }
    }

    #[test]
    fn shared_is_the_default_access_mode_and_device_settings_are_sane() {
        let settings = AudioSettings::default();
        assert_eq!(settings.access, AudioAccess::Shared);
        assert_eq!(settings.sample_rate, 48_000);
        assert_eq!(settings.buffer_frames, 256);
    }

    #[test]
    fn audio_settings_reject_unreasonable_rates_and_buffers() {
        let settings = AudioSettings {
            sample_rate: 0,
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_err());
        let settings = AudioSettings {
            buffer_frames: 1,
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn ring_buffer_preserves_capture_order_and_reports_overflow() {
        let ring = AudioRingBuffer::new(2);
        assert!(ring.push(-0.5));
        assert!(ring.push(0.25));
        assert!(!ring.push(0.75));
        assert_eq!(ring.pop(), Some(-0.5));
        assert_eq!(ring.pop(), Some(0.25));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn test_tone_uses_a_stable_amplitude_and_advancing_phase() {
        let (first, phase) = tone_sample(0.0, 48_000.0);
        let (second, next_phase) = tone_sample(phase, 48_000.0);
        assert_eq!(first, 0.0);
        assert!(second > 0.0);
        assert!(next_phase > phase);
        assert!(second <= 0.12);
    }

    #[test]
    fn project_output_preserves_stereo_order_and_stops_at_the_end() {
        let playback = PlaybackState::new();
        playback.start(vec![0.25_f32, -0.5, 0.75, -1.0]);
        let source = AtomicU8::new(AUDIO_SOURCE_PROJECT);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let samples = playback.samples.load();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -1.0]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.active.load(Ordering::Acquire));
        playback.resume().expect("loaded playback should resume");
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        playback.stop();
        assert!(playback.resume().is_err());
    }

    #[test]
    fn stereo_ring_keeps_frames_atomic_and_in_order() {
        let ring = AudioRingBuffer::new(4);
        assert!(ring.push_stereo_frame(0.25, -0.5));
        assert!(ring.push_stereo_frame(0.75, -1.0));
        assert!(!ring.push_stereo_frame(0.0, 0.0));
        assert_eq!(ring.pop_stereo_frame(), Some([0.25, -0.5]));
        assert_eq!(ring.pop_stereo_frame(), Some([0.75, -1.0]));
        assert_eq!(ring.pop_stereo_frame(), None);
    }

    #[test]
    fn streamed_output_waits_for_prefill_and_drains_before_stopping() {
        let playback = PlaybackState::new();
        let writer = playback.start_streaming();
        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let stream = playback.streaming.load();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert_eq!(writer.underrun_frames(), 0);
        writer
            .write_stereo_samples(&[0.25, -0.5, 0.75, -1.0])
            .unwrap();
        writer.finish();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -1.0]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.is_active());
    }

    #[test]
    fn streamed_output_pause_preserves_queued_frames_for_resume() {
        let playback = PlaybackState::new();
        let writer = playback.start_streaming();
        writer.write_stereo_samples(&[0.25, -0.5]).unwrap();
        writer.finish();
        let stream = playback.streaming.load();
        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;

        playback.pause();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert_eq!(stream.as_deref().unwrap().ring.available_stereo_frames(), 1);

        playback.resume().unwrap();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
    }
}

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flp_rebuild::PatternNote;
use flp_rebuild::midi::{
    midi_modulation_to_fl_note_value, midi_pan_to_fl_note_pan,
    midi_pitch_bend_cents_to_fl_note_fine_pitch, midi_pitch_bend_offset_cents,
    midi_release_velocity_to_fl_note_release,
};
use midir::{Ignore, MidiInput, MidiInputConnection as MidirInputConnection};

use super::midi_script::{
    MidiControllerScript, MidiScriptRuntime, MidiScriptStopHandle, MidiScriptTransportState,
    ScriptTransportAction,
};
use flp_rebuild::sample_render::PlaylistTempoMap;

pub const MIDI_INPUT_QUEUE_CAPACITY: usize = 4_096;
pub const MIDI_THRU_INPUT_QUEUE_CAPACITY: usize = 4_096;
pub const MIDI_THRU_MAX_SYSTEM_EXCLUSIVE_BYTES: usize = 65_536;
const MAX_CAPTURED_NOTES: usize = 100_000;
const MAX_NOTE_TICK: u32 = u32::MAX - 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiInputDevice {
    pub id: String,
    pub name: String,
}

pub struct ConnectedMidiInput {
    _connection: MidirInputConnection<()>,
    pub device_id: String,
    pub device_name: String,
    pub script_name: Option<String>,
    script_worker: Option<MidiInputScriptWorker>,
}

struct ScriptInputMessage {
    timestamp_micros: u64,
    received_at: Instant,
    bytes: Vec<u8>,
}

enum MidiInputScriptMessage {
    Input(ScriptInputMessage),
    MidiOutput {
        bytes: Vec<u8>,
        cancelled: Arc<AtomicBool>,
        reply: SyncSender<Result<Option<Vec<u8>>, String>>,
    },
    ProjectLoad(i32),
    BeatIndicator(i32),
    FirstConnect,
    Stop,
}

struct RepeatingMidiOutput {
    bytes: Vec<u8>,
    next_at: Instant,
    rate: Duration,
}

impl RepeatingMidiOutput {
    fn new(bytes: Vec<u8>, delay_ms: u32, rate_ms: u32, now: Instant) -> Self {
        let delay = Duration::from_millis(u64::from(delay_ms));
        let rate = Duration::from_millis(u64::from(rate_ms));
        Self {
            bytes,
            next_at: now + delay,
            rate,
        }
    }

    fn wait_duration(&self, now: Instant) -> Duration {
        self.next_at.saturating_duration_since(now)
    }

    fn take_due(&mut self, now: Instant) -> Option<Vec<u8>> {
        if now < self.next_at {
            return None;
        }
        let next_at = self.next_at + self.rate;
        self.next_at = if next_at > now {
            next_at
        } else {
            now + self.rate
        };
        Some(self.bytes.clone())
    }
}

fn script_worker_wait_duration(
    repeat: Option<&RepeatingMidiOutput>,
    next_idle_at: Instant,
    now: Instant,
) -> Duration {
    let until_idle = next_idle_at.saturating_duration_since(now);
    repeat.map_or(until_idle, |repeat| {
        repeat.wait_duration(now).min(until_idle)
    })
}

fn apply_script_repeat_actions(
    actions: impl IntoIterator<Item = ScriptTransportAction>,
    repeat: &mut Option<RepeatingMidiOutput>,
) -> Vec<ScriptTransportAction> {
    let now = Instant::now();
    let mut remaining = Vec::new();
    for action in actions {
        match action {
            ScriptTransportAction::RepeatMidiOutput {
                bytes,
                delay_ms,
                rate_ms,
            } => *repeat = Some(RepeatingMidiOutput::new(bytes, delay_ms, rate_ms, now)),
            ScriptTransportAction::StopRepeatMidiOutput => *repeat = None,
            action => remaining.push(action),
        }
    }
    remaining
}

fn send_due_repeating_midi_output(
    repeat: &mut Option<RepeatingMidiOutput>,
    midi_output_sender: &SyncSender<Vec<u8>>,
    dropped_messages: &AtomicU64,
    now: Instant,
) {
    let Some(repeat) = repeat.as_mut() else {
        return;
    };
    if let Some(bytes) = repeat.take_due(now)
        && midi_output_sender.try_send(bytes).is_err()
    {
        dropped_messages.fetch_add(1, Ordering::Relaxed);
    }
}

fn queue_script_actions_with_repeat(
    sender: &SyncSender<ReceivedMidiMessage>,
    midi_output_sender: &SyncSender<Vec<u8>>,
    dropped_messages: &AtomicU64,
    timestamp_micros: u64,
    received_at: Instant,
    actions: impl IntoIterator<Item = ScriptTransportAction>,
    repeat: &mut Option<RepeatingMidiOutput>,
) {
    let actions = apply_script_repeat_actions(actions, repeat);
    queue_script_transport_actions(
        sender,
        midi_output_sender,
        dropped_messages,
        timestamp_micros,
        received_at,
        actions,
    );
    send_due_repeating_midi_output(repeat, midi_output_sender, dropped_messages, Instant::now());
}

struct MidiInputScriptWorker {
    sender: SyncSender<MidiInputScriptMessage>,
    errors: Receiver<String>,
    finished: Receiver<()>,
    join: Option<JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
    stop_handle: MidiScriptStopHandle,
    supports_midi_output: bool,
}

#[derive(Clone)]
pub struct MidiScriptOutputHandle {
    sender: SyncSender<MidiInputScriptMessage>,
}

impl MidiScriptOutputHandle {
    pub fn process_message(&self, bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(MidiInputScriptMessage::MidiOutput {
                bytes: bytes.to_vec(),
                cancelled: Arc::clone(&cancelled),
                reply,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "MIDI script callback queue is full".to_owned(),
                TrySendError::Disconnected(_) => "MIDI script worker is unavailable".to_owned(),
            })?;
        match response.recv_timeout(Duration::from_millis(500)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancelled.store(true, Ordering::Release);
                Err("MIDI script OnMidiOutMsg callback timed out".to_owned())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("MIDI script OnMidiOutMsg callback stopped unexpectedly".to_owned())
            }
        }
    }
}

impl MidiInputScriptWorker {
    fn notify_project_load(&self, status: i32) -> bool {
        self.sender
            .try_send(MidiInputScriptMessage::ProjectLoad(status))
            .is_ok()
    }

    fn notify_beat_indicator(&self, value: i32) -> bool {
        self.sender
            .try_send(MidiInputScriptMessage::BeatIndicator(value))
            .is_ok()
    }

    fn notify_first_connect(&self) -> bool {
        self.sender
            .try_send(MidiInputScriptMessage::FirstConnect)
            .is_ok()
    }

    fn start(
        script: &MidiControllerScript,
        device_name: &str,
        output_sender: SyncSender<ReceivedMidiMessage>,
        midi_output_sender: SyncSender<Vec<u8>>,
        thru_queue: MidiThruInputQueue,
        dropped_messages: Arc<AtomicU64>,
        transport_state: Arc<MidiScriptTransportState>,
    ) -> Result<Self, String> {
        let mut runtime = MidiScriptRuntime::start_for_device_with_transport_state(
            script,
            device_name,
            transport_state,
        )?;
        let supports_midi_output = runtime.supports_midi_output();
        let startup_actions = runtime.take_startup_actions();
        let stop_handle = runtime.stop_handle();
        let mut repeat_output = None;
        let startup_actions = apply_script_repeat_actions(startup_actions, &mut repeat_output);
        queue_script_transport_actions(
            &output_sender,
            &midi_output_sender,
            &dropped_messages,
            0,
            Instant::now(),
            startup_actions,
        );
        send_due_repeating_midi_output(
            &mut repeat_output,
            &midi_output_sender,
            &dropped_messages,
            Instant::now(),
        );
        let (sender, input_receiver) = mpsc::sync_channel(MIDI_INPUT_QUEUE_CAPACITY);
        let (error_sender, errors) = mpsc::sync_channel(16);
        let (finished_sender, finished) = mpsc::sync_channel(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop_requested = Arc::clone(&stop_requested);
        let join = thread::Builder::new()
            .name("midi-script-input".to_owned())
            .spawn(move || {
                let mut runtime = Some(runtime);
                let mut repeat_output = repeat_output;
                let mut timestamp_reference_micros = 0_u64;
                let mut timestamp_reference_at = Instant::now();
                let mut next_idle_at = Instant::now() + Duration::from_millis(50);
                while !worker_stop_requested.load(Ordering::Acquire) {
                    let wait_duration = script_worker_wait_duration(
                        repeat_output.as_ref(),
                        next_idle_at,
                        Instant::now(),
                    );
                    let message = match input_receiver.recv_timeout(wait_duration) {
                        Ok(message) => message,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let received_at = Instant::now();
                            if received_at < next_idle_at {
                                send_due_repeating_midi_output(
                                    &mut repeat_output,
                                    &midi_output_sender,
                                    &dropped_messages,
                                    received_at,
                                );
                                continue;
                            }
                            let elapsed_micros = u64::try_from(
                                received_at
                                    .duration_since(timestamp_reference_at)
                                    .as_micros(),
                            )
                            .unwrap_or(u64::MAX);
                            let timestamp_micros =
                                timestamp_reference_micros.saturating_add(elapsed_micros);
                            let idle_result = runtime.as_mut().map(MidiScriptRuntime::process_idle);
                            match idle_result {
                                Some(Ok(result)) => queue_script_actions_with_repeat(
                                    &output_sender,
                                    &midi_output_sender,
                                    &dropped_messages,
                                    timestamp_micros,
                                    received_at,
                                    result.actions,
                                    &mut repeat_output,
                                ),
                                Some(Err(error)) => {
                                    let _ = error_sender.try_send(error);
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                }
                                None => {}
                            }
                            next_idle_at = Instant::now() + Duration::from_millis(50);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match message {
                        MidiInputScriptMessage::Stop => break,
                        MidiInputScriptMessage::MidiOutput {
                            bytes,
                            cancelled,
                            reply,
                        } => {
                            if cancelled.load(Ordering::Acquire) {
                                let _ = reply.try_send(Ok(Some(bytes)));
                                continue;
                            }
                            let callback_result = runtime
                                .as_mut()
                                .map(|runtime| runtime.process_midi_output(&bytes));
                            match callback_result {
                                Some(Ok(result)) => {
                                    if cancelled.load(Ordering::Acquire) {
                                        continue;
                                    }
                                    queue_script_actions_with_repeat(
                                        &output_sender,
                                        &midi_output_sender,
                                        &dropped_messages,
                                        0,
                                        Instant::now(),
                                        result.actions,
                                        &mut repeat_output,
                                    );
                                    let output = if result.handled {
                                        None
                                    } else {
                                        Some(result.bytes)
                                    };
                                    let _ = reply.try_send(Ok(output));
                                }
                                Some(Err(error)) => {
                                    if !cancelled.load(Ordering::Acquire) {
                                        let _ = error_sender.try_send(error.clone());
                                        let _ = reply.try_send(Err(error));
                                    }
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                }
                                None => {
                                    let _ = reply.try_send(Ok(Some(bytes)));
                                }
                            }
                        }
                        MidiInputScriptMessage::ProjectLoad(status) => {
                            let received_at = Instant::now();
                            let elapsed_micros = u64::try_from(
                                received_at
                                    .duration_since(timestamp_reference_at)
                                    .as_micros(),
                            )
                            .unwrap_or(u64::MAX);
                            let timestamp_micros =
                                timestamp_reference_micros.saturating_add(elapsed_micros);
                            let callback_result = runtime
                                .as_mut()
                                .map(|runtime| runtime.process_project_load(status));
                            match callback_result {
                                Some(Ok(result)) => queue_script_actions_with_repeat(
                                    &output_sender,
                                    &midi_output_sender,
                                    &dropped_messages,
                                    timestamp_micros,
                                    received_at,
                                    result.actions,
                                    &mut repeat_output,
                                ),
                                Some(Err(error)) => {
                                    let _ = error_sender.try_send(error);
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                }
                                None => {}
                            }
                        }
                        MidiInputScriptMessage::BeatIndicator(value) => {
                            let received_at = Instant::now();
                            let elapsed_micros = u64::try_from(
                                received_at
                                    .duration_since(timestamp_reference_at)
                                    .as_micros(),
                            )
                            .unwrap_or(u64::MAX);
                            let timestamp_micros =
                                timestamp_reference_micros.saturating_add(elapsed_micros);
                            let callback_result = runtime
                                .as_mut()
                                .map(|runtime| runtime.process_beat_indicator(value));
                            match callback_result {
                                Some(Ok(result)) => queue_script_actions_with_repeat(
                                    &output_sender,
                                    &midi_output_sender,
                                    &dropped_messages,
                                    timestamp_micros,
                                    received_at,
                                    result.actions,
                                    &mut repeat_output,
                                ),
                                Some(Err(error)) => {
                                    let _ = error_sender.try_send(error);
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                }
                                None => {}
                            }
                        }
                        MidiInputScriptMessage::FirstConnect => {
                            let received_at = Instant::now();
                            let elapsed_micros = u64::try_from(
                                received_at
                                    .duration_since(timestamp_reference_at)
                                    .as_micros(),
                            )
                            .unwrap_or(u64::MAX);
                            let timestamp_micros =
                                timestamp_reference_micros.saturating_add(elapsed_micros);
                            let callback_result = runtime
                                .as_mut()
                                .map(MidiScriptRuntime::process_first_connect);
                            match callback_result {
                                Some(Ok(result)) => queue_script_actions_with_repeat(
                                    &output_sender,
                                    &midi_output_sender,
                                    &dropped_messages,
                                    timestamp_micros,
                                    received_at,
                                    result.actions,
                                    &mut repeat_output,
                                ),
                                Some(Err(error)) => {
                                    let _ = error_sender.try_send(error);
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                }
                                None => {}
                            }
                        }
                        MidiInputScriptMessage::Input(message) => {
                            timestamp_reference_micros = message.timestamp_micros;
                            timestamp_reference_at = message.received_at;
                            let transformed = runtime.as_mut().map(|runtime| {
                                runtime.process_event(message.timestamp_micros, &message.bytes)
                            });
                            match transformed {
                                Some(Ok(result)) => {
                                    queue_script_actions_with_repeat(
                                        &output_sender,
                                        &midi_output_sender,
                                        &dropped_messages,
                                        message.timestamp_micros,
                                        message.received_at,
                                        result.actions,
                                        &mut repeat_output,
                                    );
                                    if !result.handled {
                                        forward_scripted_input(
                                            &output_sender,
                                            &thru_queue,
                                            &dropped_messages,
                                            message.timestamp_micros,
                                            message.received_at,
                                            &result.bytes,
                                        );
                                    }
                                }
                                Some(Err(error)) => {
                                    let _ = error_sender.try_send(error);
                                    repeat_output = None;
                                    if let Some(runtime) = runtime.take() {
                                        runtime.kill();
                                    }
                                    forward_scripted_input(
                                        &output_sender,
                                        &thru_queue,
                                        &dropped_messages,
                                        message.timestamp_micros,
                                        message.received_at,
                                        &message.bytes,
                                    );
                                }
                                None => forward_scripted_input(
                                    &output_sender,
                                    &thru_queue,
                                    &dropped_messages,
                                    message.timestamp_micros,
                                    message.received_at,
                                    &message.bytes,
                                ),
                            }
                        }
                    }
                    next_idle_at = Instant::now() + Duration::from_millis(50);
                }
                if let Some(mut runtime) = runtime {
                    match runtime.shutdown() {
                        Ok(actions) => {
                            let received_at = Instant::now();
                            let elapsed_micros = u64::try_from(
                                received_at
                                    .duration_since(timestamp_reference_at)
                                    .as_micros(),
                            )
                            .unwrap_or(u64::MAX);
                            queue_script_transport_actions(
                                &output_sender,
                                &midi_output_sender,
                                &dropped_messages,
                                timestamp_reference_micros.saturating_add(elapsed_micros),
                                received_at,
                                actions,
                            );
                        }
                        Err(error) => {
                            let _ = error_sender.try_send(error);
                        }
                    }
                }
                let _ = finished_sender.try_send(());
            })
            .map_err(|error| format!("could not start MIDI script worker: {error}"))?;
        Ok(Self {
            sender,
            errors,
            finished,
            join: Some(join),
            stop_requested,
            stop_handle,
            supports_midi_output,
        })
    }

    fn take_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
}

impl Drop for MidiInputScriptWorker {
    fn drop(&mut self) {
        let _ = self.sender.try_send(MidiInputScriptMessage::Stop);
        if self
            .finished
            .recv_timeout(Duration::from_millis(250))
            .is_err()
        {
            self.stop_requested.store(true, Ordering::Release);
            self.stop_handle.kill();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl ConnectedMidiInput {
    pub fn take_script_error(&self) -> Option<String> {
        self.script_worker.as_ref()?.take_error()
    }

    pub fn notify_script_project_load(&self, status: i32) -> Option<bool> {
        Some(self.script_worker.as_ref()?.notify_project_load(status))
    }

    pub fn notify_script_beat_indicator(&self, value: i32) -> Option<bool> {
        Some(self.script_worker.as_ref()?.notify_beat_indicator(value))
    }

    pub fn notify_script_first_connect(&self) -> Option<bool> {
        Some(self.script_worker.as_ref()?.notify_first_connect())
    }

    pub fn midi_script_output_handle(&self) -> Option<MidiScriptOutputHandle> {
        if !self.script_worker.as_ref()?.supports_midi_output {
            return None;
        }
        Some(MidiScriptOutputHandle {
            sender: self.script_worker.as_ref()?.sender.clone(),
        })
    }
}

fn forward_scripted_input(
    sender: &SyncSender<ReceivedMidiMessage>,
    thru_queue: &MidiThruInputQueue,
    dropped_messages: &AtomicU64,
    timestamp_micros: u64,
    received_at: Instant,
    bytes: &[u8],
) {
    let event = parse_midi_message(bytes);
    if let Some(event) = event {
        let message = ReceivedMidiMessage {
            timestamp_micros,
            received_at,
            event,
        };
        if let Err(TrySendError::Full(_)) = sender.try_send(message) {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
        }
    }
    if bytes.first() == Some(&0xF0) {
        thru_queue.try_send_system_exclusive(bytes);
    } else if let Some(event) = event {
        thru_queue.try_send(event);
    }
}

fn queue_script_transport_actions(
    sender: &SyncSender<ReceivedMidiMessage>,
    midi_output_sender: &SyncSender<Vec<u8>>,
    dropped_messages: &AtomicU64,
    timestamp_micros: u64,
    received_at: Instant,
    actions: impl IntoIterator<Item = ScriptTransportAction>,
) {
    for action in actions {
        if let ScriptTransportAction::MidiOutput(bytes) = action {
            if midi_output_sender.try_send(bytes).is_err() {
                dropped_messages.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }
        let command = match action {
            ScriptTransportAction::Start => MidiTransportCommand::Start,
            ScriptTransportAction::Stop => MidiTransportCommand::Stop,
            ScriptTransportAction::RecordToggle => MidiTransportCommand::RecordToggle,
            ScriptTransportAction::TogglePlayback => MidiTransportCommand::TogglePlayback,
            ScriptTransportAction::ToggleLoopMode => MidiTransportCommand::ToggleLoopMode,
            ScriptTransportAction::SetSongPosition(position) => {
                MidiTransportCommand::SetSongPosition(position)
            }
            ScriptTransportAction::SetSongPositionTicks(position) => {
                MidiTransportCommand::SetSongPositionTicks(position)
            }
            ScriptTransportAction::SetSongPositionTimeUs(position) => {
                MidiTransportCommand::SetSongPositionTimeUs(position)
            }
            ScriptTransportAction::MoveSongPosition {
                speed_millionths,
                start,
            } => MidiTransportCommand::MoveSongPosition {
                speed_millionths,
                start,
            },
            ScriptTransportAction::SetPlaybackSpeed(speed_millionths) => {
                MidiTransportCommand::SetPlaybackSpeed(speed_millionths)
            }
            ScriptTransportAction::SetCurrentTempoMilliBpm(milli_bpm) => {
                MidiTransportCommand::SetCurrentTempo(milli_bpm)
            }
            ScriptTransportAction::PatternJog(increment) => {
                MidiTransportCommand::PatternJog(increment)
            }
            ScriptTransportAction::TempoJog(increment) => MidiTransportCommand::TempoJog(increment),
            ScriptTransportAction::MarkerJumpJog(increment) => {
                MidiTransportCommand::MarkerJumpJog(increment)
            }
            ScriptTransportAction::MarkerSelJog(increment) => {
                MidiTransportCommand::MarkerSelJog(increment)
            }
            ScriptTransportAction::ShowMainView(view) => MidiTransportCommand::ShowMainView(view),
            ScriptTransportAction::OpenProjectInfo => MidiTransportCommand::OpenProjectInfo,
            ScriptTransportAction::MidiOutput(_) => unreachable!("handled above"),
            ScriptTransportAction::RepeatMidiOutput { .. }
            | ScriptTransportAction::StopRepeatMidiOutput => continue,
        };
        let transport_message = ReceivedMidiMessage {
            timestamp_micros,
            received_at,
            event: MidiInputEvent::Transport(command),
        };
        if let Err(TrySendError::Full(_)) = sender.try_send(transport_message) {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiTransportCommand {
    Start,
    Continue,
    Stop,
    Pause,
    RecordStart,
    RecordStop,
    RecordToggle,
    TogglePlayback,
    ToggleLoopMode,
    SetSongPosition(u32),
    SetSongPositionTicks(u64),
    SetSongPositionTimeUs(u64),
    MoveSongPosition { speed_millionths: i32, start: bool },
    SetPlaybackSpeed(u32),
    SetCurrentTempo(u32),
    PatternJog(i32),
    TempoJog(i32),
    MarkerJumpJog(i32),
    MarkerSelJog(i32),
    ShowMainView(u8),
    OpenProjectInfo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiInputEvent {
    NoteOn {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        key: u8,
        velocity: u8,
    },
    PolyphonicPressure {
        channel: u8,
        key: u8,
        pressure: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelPressure {
        channel: u8,
        pressure: u8,
    },
    PitchBend {
        channel: u8,
        value: i16,
    },
    TimingClock,
    SongPosition {
        sixteenth_notes: u16,
    },
    Transport(MidiTransportCommand),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiInputChannelState {
    pan: u8,
    pan_lsb: u8,
    pan_lsb_received: bool,
    channel_volume: u8,
    channel_volume_lsb: u8,
    channel_volume_lsb_received: bool,
    expression: u8,
    expression_lsb: u8,
    expression_lsb_received: bool,
    mod_x: u8,
    mod_y: u8,
    pitch_bend_value: u16,
    pitch_bend_sensitivity_semitones: u8,
    pitch_bend_sensitivity_cents: u8,
    rpn_msb: u8,
    rpn_lsb: u8,
}

impl Default for MidiInputChannelState {
    fn default() -> Self {
        Self {
            pan: 64,
            pan_lsb: 0,
            pan_lsb_received: false,
            channel_volume: 127,
            channel_volume_lsb: 0,
            channel_volume_lsb_received: false,
            expression: 127,
            expression_lsb: 0,
            expression_lsb_received: false,
            mod_x: 64,
            mod_y: 64,
            pitch_bend_value: 8192,
            pitch_bend_sensitivity_semitones: 2,
            pitch_bend_sensitivity_cents: 0,
            rpn_msb: 127,
            rpn_lsb: 127,
        }
    }
}

impl MidiInputChannelState {
    pub fn update(&mut self, event: MidiInputEvent) {
        match event {
            MidiInputEvent::ControlChange {
                controller: 7,
                value,
                ..
            } => self.channel_volume = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 39,
                value,
                ..
            } => {
                self.channel_volume_lsb = value.min(127);
                self.channel_volume_lsb_received = true;
            }
            MidiInputEvent::ControlChange {
                controller: 10,
                value,
                ..
            } => self.pan = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 42,
                value,
                ..
            } => {
                self.pan_lsb = value.min(127);
                self.pan_lsb_received = true;
            }
            MidiInputEvent::ControlChange {
                controller: 11,
                value,
                ..
            } => self.expression = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 43,
                value,
                ..
            } => {
                self.expression_lsb = value.min(127);
                self.expression_lsb_received = true;
            }
            MidiInputEvent::ControlChange {
                controller: 74,
                value,
                ..
            } => self.mod_x = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 71,
                value,
                ..
            } => self.mod_y = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 121, ..
            } => {
                self.pan = 64;
                self.pan_lsb = 0;
                self.pan_lsb_received = false;
                self.channel_volume = 127;
                self.channel_volume_lsb = 0;
                self.channel_volume_lsb_received = false;
                self.expression = 127;
                self.expression_lsb = 0;
                self.expression_lsb_received = false;
                self.mod_x = 64;
                self.mod_y = 64;
                self.pitch_bend_value = 8192;
                self.pitch_bend_sensitivity_semitones = 2;
                self.pitch_bend_sensitivity_cents = 0;
                self.rpn_msb = 127;
                self.rpn_lsb = 127;
            }
            MidiInputEvent::ControlChange {
                controller: 101,
                value,
                ..
            } => self.rpn_msb = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 100,
                value,
                ..
            } => self.rpn_lsb = value.min(127),
            MidiInputEvent::ControlChange {
                controller: 6,
                value,
                ..
            } if (self.rpn_msb, self.rpn_lsb) == (0, 0) => {
                self.pitch_bend_sensitivity_semitones = value.min(127);
            }
            MidiInputEvent::ControlChange {
                controller: 38,
                value,
                ..
            } if (self.rpn_msb, self.rpn_lsb) == (0, 0) => {
                self.pitch_bend_sensitivity_cents = value.min(99);
            }
            MidiInputEvent::PitchBend { value, .. } => {
                self.pitch_bend_value = (i32::from(value) + 8192).clamp(0, 0x3FFF) as u16;
            }
            _ => {}
        }
    }

    pub fn raw_pan(self) -> u8 {
        self.pan
    }

    pub fn raw_pan_14bit(self) -> u16 {
        (u16::from(self.pan) << 7) | u16::from(self.pan_lsb)
    }

    pub fn pan_lsb_received(self) -> bool {
        self.pan_lsb_received
    }

    pub fn raw_mod_x(self) -> u8 {
        self.mod_x
    }

    pub fn raw_mod_y(self) -> u8 {
        self.mod_y
    }

    pub fn raw_pitch_bend_value(self) -> u16 {
        self.pitch_bend_value
    }

    pub fn raw_pitch_bend_sensitivity_semitones(self) -> u8 {
        self.pitch_bend_sensitivity_semitones
    }

    pub fn raw_pitch_bend_sensitivity_cents(self) -> u8 {
        self.pitch_bend_sensitivity_cents
    }

    pub fn raw_expression(self) -> u8 {
        self.expression
    }

    pub fn raw_expression_14bit(self) -> u16 {
        (u16::from(self.expression) << 7) | u16::from(self.expression_lsb)
    }

    pub fn expression_lsb_received(self) -> bool {
        self.expression_lsb_received
    }

    pub fn raw_channel_volume(self) -> u8 {
        self.channel_volume
    }

    pub fn raw_channel_volume_14bit(self) -> u16 {
        (u16::from(self.channel_volume) << 7) | u16::from(self.channel_volume_lsb)
    }

    pub fn channel_volume_lsb_received(self) -> bool {
        self.channel_volume_lsb_received
    }

    fn note_mod_x(self) -> u8 {
        midi_modulation_to_fl_note_value(self.mod_x)
    }

    fn note_mod_y(self) -> u8 {
        midi_modulation_to_fl_note_value(self.mod_y)
    }

    pub fn pitch_bend_offset_cents(self) -> i16 {
        midi_pitch_bend_offset_cents(
            self.pitch_bend_value,
            self.pitch_bend_sensitivity_semitones,
            self.pitch_bend_sensitivity_cents,
        )
    }

    fn note_fine_pitch(self) -> u8 {
        midi_pitch_bend_cents_to_fl_note_fine_pitch(self.pitch_bend_offset_cents())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReceivedMidiMessage {
    pub timestamp_micros: u64,
    pub received_at: Instant,
    pub event: MidiInputEvent,
}

#[derive(Clone, Debug)]
pub struct MidiThruInputMessage {
    pub generation: u64,
    pub payload: MidiThruInputPayload,
}

#[derive(Clone, Debug)]
pub enum MidiThruInputPayload {
    Event(MidiInputEvent),
    SystemExclusive(Arc<[u8]>),
}

#[derive(Clone)]
pub struct MidiThruInputQueue {
    sender: SyncSender<MidiThruInputMessage>,
    enabled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    dropped_messages: Arc<AtomicU64>,
}

impl MidiThruInputQueue {
    pub(super) fn new(
        sender: SyncSender<MidiThruInputMessage>,
        enabled: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        dropped_messages: Arc<AtomicU64>,
    ) -> Self {
        Self {
            sender,
            enabled,
            generation,
            dropped_messages,
        }
    }

    fn try_send(&self, event: MidiInputEvent) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let message = MidiThruInputMessage {
            generation: self.generation.load(Ordering::Acquire),
            payload: MidiThruInputPayload::Event(event),
        };
        if let Err(TrySendError::Full(_)) = self.sender.try_send(message) {
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn try_send_system_exclusive(&self, bytes: &[u8]) {
        if !self.enabled.load(Ordering::Acquire)
            || bytes.len() < 2
            || bytes.first() != Some(&0xF0)
            || bytes.last() != Some(&0xF7)
        {
            return;
        }
        if bytes.len() > MIDI_THRU_MAX_SYSTEM_EXCLUSIVE_BYTES {
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let message = MidiThruInputMessage {
            generation: self.generation.load(Ordering::Acquire),
            payload: MidiThruInputPayload::SystemExclusive(Arc::from(bytes)),
        };
        if let Err(TrySendError::Full(_)) = self.sender.try_send(message) {
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(super) fn dropped_count(&self) -> u64 {
        self.dropped_messages.load(Ordering::Relaxed)
    }

    pub(super) fn enabled(&self) -> &AtomicBool {
        &self.enabled
    }
}

pub fn enumerate_input_devices() -> Result<Vec<MidiInputDevice>, String> {
    let input = MidiInput::new("FL Studio Rebuild MIDI Input")
        .map_err(|error| format!("could not initialize MIDI input: {error}"))?;
    input
        .ports()
        .into_iter()
        .map(|port| {
            let name = input
                .port_name(&port)
                .map_err(|error| format!("could not read MIDI input name: {error}"))?;
            Ok(MidiInputDevice {
                id: port.id(),
                name,
            })
        })
        .collect()
}

pub fn connect_input_device(
    device_id: &str,
    sender: SyncSender<ReceivedMidiMessage>,
    midi_output_sender: SyncSender<Vec<u8>>,
    thru_queue: MidiThruInputQueue,
    dropped_messages: Arc<AtomicU64>,
    transport_state: Arc<MidiScriptTransportState>,
    script: Option<&MidiControllerScript>,
) -> Result<ConnectedMidiInput, String> {
    let mut input = MidiInput::new("FL Studio Rebuild MIDI Input")
        .map_err(|error| format!("could not initialize MIDI input: {error}"))?;
    input.ignore(Ignore::None);
    let port = input
        .ports()
        .into_iter()
        .find(|port| port.id() == device_id)
        .ok_or_else(|| "the selected MIDI input is no longer available".to_owned())?;
    let device_name = input
        .port_name(&port)
        .map_err(|error| format!("could not read MIDI input name: {error}"))?;
    let script_worker = script
        .map(|script| {
            MidiInputScriptWorker::start(
                script,
                &device_name,
                sender.clone(),
                midi_output_sender.clone(),
                thru_queue.clone(),
                Arc::clone(&dropped_messages),
                Arc::clone(&transport_state),
            )
        })
        .transpose()?;
    let script_sender = script_worker.as_ref().map(|worker| worker.sender.clone());
    let connection = input
        .connect(
            &port,
            "FL Studio Rebuild MIDI Input",
            move |timestamp_micros, bytes, _| {
                let received_at = Instant::now();
                if let Some(script_sender) = &script_sender
                    && bytes.len() <= 65_536
                {
                    let message = MidiInputScriptMessage::Input(ScriptInputMessage {
                        timestamp_micros,
                        received_at,
                        bytes: bytes.to_vec(),
                    });
                    match script_sender.try_send(message) {
                        Ok(()) => return,
                        Err(TrySendError::Full(_)) => {
                            dropped_messages.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        Err(TrySendError::Disconnected(_)) => {}
                    }
                }
                forward_scripted_input(
                    &sender,
                    &thru_queue,
                    &dropped_messages,
                    timestamp_micros,
                    received_at,
                    bytes,
                );
            },
            (),
        )
        .map_err(|error| format!("could not connect to MIDI input: {error}"))?;
    Ok(ConnectedMidiInput {
        _connection: connection,
        device_id: device_id.to_owned(),
        device_name,
        script_name: script.map(|script| script.name.clone()),
        script_worker,
    })
}

pub fn parse_midi_message(bytes: &[u8]) -> Option<MidiInputEvent> {
    let status = *bytes.first()?;
    match status {
        0xF0 => parse_mmc_message(bytes),
        0xF2 => Some(MidiInputEvent::SongPosition {
            sixteenth_notes: u16::from(*bytes.get(1)? & 0x7F)
                | (u16::from(*bytes.get(2)? & 0x7F) << 7),
        }),
        0xF8 => Some(MidiInputEvent::TimingClock),
        0xFA => Some(MidiInputEvent::Transport(MidiTransportCommand::Start)),
        0xFB => Some(MidiInputEvent::Transport(MidiTransportCommand::Continue)),
        0xFC => Some(MidiInputEvent::Transport(MidiTransportCommand::Stop)),
        0x80..=0xEF => {
            let channel = status & 0x0F;
            let kind = status & 0xF0;
            let first = *bytes.get(1)?;
            if first > 0x7F {
                return None;
            }
            let second = match kind {
                0xC0 | 0xD0 => None,
                _ => Some(*bytes.get(2)?),
            };
            if second.is_some_and(|value| value > 0x7F) {
                return None;
            }
            match kind {
                0x80 => Some(MidiInputEvent::NoteOff {
                    channel,
                    key: first,
                    velocity: second?,
                }),
                0x90 => {
                    let velocity = second?;
                    if velocity == 0 {
                        Some(MidiInputEvent::NoteOff {
                            channel,
                            key: first,
                            velocity,
                        })
                    } else {
                        Some(MidiInputEvent::NoteOn {
                            channel,
                            key: first,
                            velocity,
                        })
                    }
                }
                0xA0 => Some(MidiInputEvent::PolyphonicPressure {
                    channel,
                    key: first,
                    pressure: second?,
                }),
                0xB0 => Some(MidiInputEvent::ControlChange {
                    channel,
                    controller: first,
                    value: second?,
                }),
                0xC0 => Some(MidiInputEvent::ProgramChange {
                    channel,
                    program: first,
                }),
                0xD0 => Some(MidiInputEvent::ChannelPressure {
                    channel,
                    pressure: first,
                }),
                0xE0 => {
                    let value = u16::from(first) | (u16::from(second?) << 7);
                    Some(MidiInputEvent::PitchBend {
                        channel,
                        value: value as i16 - 8192,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_mmc_message(bytes: &[u8]) -> Option<MidiInputEvent> {
    if bytes.len() < 6 || bytes.last() != Some(&0xF7) || bytes[1] != 0x7F || bytes[3] != 0x06 {
        return None;
    }
    let command = match bytes[4] {
        0x01 => MidiTransportCommand::Stop,
        0x02 | 0x03 => MidiTransportCommand::Start,
        0x06 => MidiTransportCommand::RecordStart,
        0x07 | 0x08 => MidiTransportCommand::RecordStop,
        0x09 => MidiTransportCommand::Pause,
        _ => return None,
    };
    Some(MidiInputEvent::Transport(command))
}

pub fn describe_event(event: MidiInputEvent) -> String {
    match event {
        MidiInputEvent::NoteOn {
            channel,
            key,
            velocity,
        } => format!(
            "Note {} · channel {} · velocity {velocity}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::NoteOff {
            channel,
            key,
            velocity,
        } => format!(
            "Release {} · channel {} · velocity {velocity}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::PolyphonicPressure {
            channel,
            key,
            pressure,
        } => format!(
            "Pressure {} · channel {} · {pressure}",
            midi_note_name(key),
            channel + 1
        ),
        MidiInputEvent::ControlChange {
            channel,
            controller,
            value,
        } => format!("CC {controller} · channel {} · {value}", channel + 1),
        MidiInputEvent::ProgramChange { channel, program } => {
            format!("Program {program} · channel {}", channel + 1)
        }
        MidiInputEvent::ChannelPressure { channel, pressure } => {
            format!("Channel pressure · channel {} · {pressure}", channel + 1)
        }
        MidiInputEvent::PitchBend { channel, value } => {
            format!("Pitch bend · channel {} · {value}", channel + 1)
        }
        MidiInputEvent::TimingClock => "MIDI clock".to_owned(),
        MidiInputEvent::SongPosition { sixteenth_notes } => {
            format!("Song position · {sixteenth_notes} sixteenths")
        }
        MidiInputEvent::Transport(command) => match command {
            MidiTransportCommand::Start => "MIDI Start".to_owned(),
            MidiTransportCommand::Continue => "MIDI Continue".to_owned(),
            MidiTransportCommand::Stop => "MIDI Stop".to_owned(),
            MidiTransportCommand::Pause => "MMC Pause".to_owned(),
            MidiTransportCommand::RecordStart => "MMC Record".to_owned(),
            MidiTransportCommand::RecordStop => "MMC Record Exit".to_owned(),
            MidiTransportCommand::RecordToggle => "MIDI script Record".to_owned(),
            MidiTransportCommand::TogglePlayback => "MIDI script Play/Pause".to_owned(),
            MidiTransportCommand::ToggleLoopMode => "MIDI script Loop Mode".to_owned(),
            MidiTransportCommand::SetSongPosition(_) => "MIDI script Song Position".to_owned(),
            MidiTransportCommand::SetSongPositionTicks(_) => "MIDI script Song Position".to_owned(),
            MidiTransportCommand::SetSongPositionTimeUs(_) => {
                "MIDI script Song Position".to_owned()
            }
            MidiTransportCommand::MoveSongPosition { .. } => {
                "MIDI script Song Position Move".to_owned()
            }
            MidiTransportCommand::SetPlaybackSpeed(_) => "MIDI script Playback Speed".to_owned(),
            MidiTransportCommand::SetCurrentTempo(_) => "MIDI script Tempo".to_owned(),
            MidiTransportCommand::PatternJog(_) => "MIDI script Pattern Selection".to_owned(),
            MidiTransportCommand::TempoJog(_) => "MIDI script Tempo".to_owned(),
            MidiTransportCommand::MarkerJumpJog(_) => "MIDI script Marker Jump".to_owned(),
            MidiTransportCommand::MarkerSelJog(_) => "MIDI script Marker Selection".to_owned(),
            MidiTransportCommand::ShowMainView(_) => "MIDI script View Selection".to_owned(),
            MidiTransportCommand::OpenProjectInfo => "MIDI script Project Info".to_owned(),
        },
    }
}

fn midi_note_name(key: u8) -> String {
    const NOTE_NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    let octave = i16::from(key) / 12 - 1;
    format!("{}{}", NOTE_NAMES[usize::from(key % 12)], octave)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiRecordingResult {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub project_path: Option<PathBuf>,
    pub notes: Vec<PatternNote>,
    pub overflowed: bool,
}

pub struct MidiPatternRecorderSettings {
    pub pattern_id: u16,
    pub channel_id: u16,
    pub project_path: Option<PathBuf>,
    pub ppq: u16,
    pub tempo_bpm: f64,
    pub start_tick: u32,
    pub snap_ticks: Option<u32>,
    pub timing_offset_ms: i16,
    pub song_timeline_start: Option<(PlaylistTempoMap, f64)>,
}

struct ActiveMidiNote {
    start_tick: u32,
    velocity: u8,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    fine_pitch: u8,
    sostenuto_held: bool,
}

struct SustainedMidiNote {
    key: u8,
    note: ActiveMidiNote,
    release_velocity: Option<u8>,
    sostenuto_held: bool,
}

pub struct MidiPatternRecorder {
    pattern_id: u16,
    channel_id: u16,
    project_path: Option<PathBuf>,
    ppq: u16,
    tempo_bpm: f64,
    start_tick: u32,
    snap_ticks: Option<u32>,
    timing_offset_ms: i16,
    song_timeline_start: Option<(PlaylistTempoMap, f64)>,
    song_start_frame_micros: Option<u64>,
    started_at: Instant,
    timestamp_origin_micros: Option<u64>,
    channel_state: [MidiInputChannelState; 16],
    sustain_channels: [bool; 16],
    sostenuto_channels: [bool; 16],
    active_notes: BTreeMap<(u8, u8), VecDeque<ActiveMidiNote>>,
    sustained_notes: BTreeMap<u8, Vec<SustainedMidiNote>>,
    notes: Vec<PatternNote>,
    started_note_count: usize,
    last_position_tick: u32,
    overflowed: bool,
}

impl MidiPatternRecorder {
    pub fn new(
        settings: MidiPatternRecorderSettings,
        started_at: Instant,
    ) -> Result<Self, &'static str> {
        let MidiPatternRecorderSettings {
            pattern_id,
            channel_id,
            project_path,
            ppq,
            tempo_bpm,
            start_tick,
            snap_ticks,
            timing_offset_ms,
            song_timeline_start,
        } = settings;
        if ppq == 0 {
            return Err("MIDI recording requires a non-zero project PPQ value");
        }
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err("MIDI recording requires a positive project tempo");
        }
        if snap_ticks == Some(0) {
            return Err("MIDI recording snap requires a non-zero tick value");
        }
        let song_start_frame_micros = match &song_timeline_start {
            Some((tempo_map, song_start_tick)) => {
                if !song_start_tick.is_finite() || *song_start_tick < 0.0 {
                    return Err("MIDI recording Playlist position must be finite and non-negative");
                }
                Some(
                    tempo_map
                        .frame_at_fractional_tick(*song_start_tick, 1_000_000)
                        .map_err(|_| "MIDI recording Playlist position is outside the timeline")?,
                )
            }
            None => None,
        };
        Ok(Self {
            pattern_id,
            channel_id,
            project_path,
            ppq,
            tempo_bpm,
            start_tick,
            snap_ticks,
            timing_offset_ms,
            song_timeline_start,
            song_start_frame_micros,
            started_at,
            timestamp_origin_micros: None,
            channel_state: [MidiInputChannelState::default(); 16],
            sustain_channels: [false; 16],
            sostenuto_channels: [false; 16],
            active_notes: BTreeMap::new(),
            sustained_notes: BTreeMap::new(),
            notes: Vec::new(),
            started_note_count: 0,
            last_position_tick: 0,
            overflowed: false,
        })
    }

    pub fn pattern_id(&self) -> u16 {
        self.pattern_id
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn set_channel_state(&mut self, channel_state: [MidiInputChannelState; 16]) {
        self.channel_state = channel_state;
    }

    pub fn set_pedal_state(
        &mut self,
        sustain_channels: [bool; 16],
        sostenuto_channels: [bool; 16],
    ) {
        self.sustain_channels = sustain_channels;
        self.sostenuto_channels = sostenuto_channels;
    }

    /// Returns the Pattern-relative position for a received MIDI message using the same
    /// timestamp origin, tempo map, and input offset as recorded notes.
    pub fn position_for_message(&mut self, message: ReceivedMidiMessage) -> Option<u32> {
        if message.received_at < self.started_at {
            return None;
        }
        let tick = self
            .tick_for_message(message.timestamp_micros, message.received_at)
            .saturating_add(self.start_tick)
            .min(MAX_NOTE_TICK);
        self.last_position_tick = self.last_position_tick.max(tick);
        Some(tick)
    }

    pub fn record(&mut self, message: ReceivedMidiMessage) {
        self.record_with_velocity_override(message, None);
    }

    pub fn record_with_velocity_override(
        &mut self,
        message: ReceivedMidiMessage,
        fixed_velocity: Option<u8>,
    ) {
        match message.event {
            MidiInputEvent::ControlChange { channel, .. }
            | MidiInputEvent::PitchBend { channel, .. } => {
                if let Some(state) = self.channel_state.get_mut(usize::from(channel)) {
                    state.update(message.event);
                }
            }
            _ => {}
        }
        let Some(tick) = self.position_for_message(message) else {
            return;
        };
        match message.event {
            MidiInputEvent::NoteOn {
                channel,
                key,
                velocity,
            } => {
                if self.started_note_count >= MAX_CAPTURED_NOTES {
                    self.overflowed = true;
                    return;
                }
                self.started_note_count += 1;
                let channel_state = self
                    .channel_state
                    .get(usize::from(channel))
                    .copied()
                    .unwrap_or_default();
                self.active_notes
                    .entry((channel, key))
                    .or_default()
                    .push_back(ActiveMidiNote {
                        start_tick: tick,
                        velocity: fixed_velocity.unwrap_or(velocity).min(127),
                        pan: channel_state.raw_pan(),
                        mod_x: channel_state.note_mod_x(),
                        mod_y: channel_state.note_mod_y(),
                        fine_pitch: channel_state.note_fine_pitch(),
                        sostenuto_held: false,
                    });
            }
            MidiInputEvent::NoteOff {
                channel,
                key,
                velocity: release_velocity,
            } => {
                let event_key = (channel, key);
                let (ended_note, remove_key) =
                    if let Some(active) = self.active_notes.get_mut(&event_key) {
                        let ended_note = active.pop_front();
                        (ended_note, active.is_empty())
                    } else {
                        (None, false)
                    };
                if remove_key {
                    self.active_notes.remove(&event_key);
                }
                if let Some(active_note) = ended_note {
                    self.finish_or_sustain_note(
                        active_note,
                        tick,
                        channel,
                        key,
                        Some(release_velocity),
                    );
                }
            }
            MidiInputEvent::ControlChange {
                channel,
                controller: 64,
                value,
            } => {
                let Some(sustain) = self.sustain_channels.get_mut(usize::from(channel)) else {
                    return;
                };
                let was_enabled = *sustain;
                *sustain = value >= 64;
                if was_enabled && !*sustain {
                    self.finish_unheld_sustained_notes(channel, tick);
                }
            }
            MidiInputEvent::ControlChange {
                channel,
                controller: 66,
                value,
            } => {
                let Some(sostenuto) = self.sostenuto_channels.get_mut(usize::from(channel)) else {
                    return;
                };
                let was_enabled = *sostenuto;
                *sostenuto = value >= 64;
                if !was_enabled && *sostenuto {
                    for ((active_channel, _), notes) in &mut self.active_notes {
                        if *active_channel == channel {
                            for note in notes {
                                note.sostenuto_held = true;
                            }
                        }
                    }
                } else if was_enabled && !*sostenuto {
                    for ((active_channel, _), notes) in &mut self.active_notes {
                        if *active_channel == channel {
                            for note in notes {
                                note.sostenuto_held = false;
                            }
                        }
                    }
                    if let Some(notes) = self.sustained_notes.get_mut(&channel) {
                        for note in notes {
                            note.sostenuto_held = false;
                        }
                    }
                    self.finish_unheld_sustained_notes(channel, tick);
                }
            }
            MidiInputEvent::ControlChange {
                channel,
                controller: 120,
                ..
            } => self.stop_channel_notes(channel, tick),
            MidiInputEvent::ControlChange {
                channel,
                controller: 121,
                ..
            } => {
                if let Some(sustain) = self.sustain_channels.get_mut(usize::from(channel)) {
                    *sustain = false;
                }
                if let Some(sostenuto) = self.sostenuto_channels.get_mut(usize::from(channel)) {
                    *sostenuto = false;
                }
                for ((active_channel, _), notes) in &mut self.active_notes {
                    if *active_channel == channel {
                        for note in notes {
                            note.sostenuto_held = false;
                        }
                    }
                }
                self.finish_sustained_notes(channel, tick);
            }
            MidiInputEvent::ControlChange {
                channel,
                controller: 123..=127,
                ..
            } => self.release_channel_notes(channel, tick),
            _ => {}
        }
    }

    fn finish_or_sustain_note(
        &mut self,
        active_note: ActiveMidiNote,
        end_tick: u32,
        channel: u8,
        key: u8,
        release_velocity: Option<u8>,
    ) {
        let sostenuto_held = active_note.sostenuto_held;
        if self
            .sustain_channels
            .get(usize::from(channel))
            .copied()
            .unwrap_or(false)
            || sostenuto_held
        {
            self.sustained_notes
                .entry(channel)
                .or_default()
                .push(SustainedMidiNote {
                    key,
                    note: active_note,
                    release_velocity,
                    sostenuto_held,
                });
        } else {
            self.notes
                .push(self.make_note(active_note, end_tick, channel, key, release_velocity));
        }
    }

    fn release_channel_notes(&mut self, channel: u8, end_tick: u32) {
        let active_keys = self
            .active_notes
            .keys()
            .copied()
            .filter(|(active_channel, _)| *active_channel == channel)
            .collect::<Vec<_>>();
        for (active_channel, key) in active_keys {
            if let Some(active) = self.active_notes.remove(&(active_channel, key)) {
                for active_note in active {
                    self.finish_or_sustain_note(active_note, end_tick, active_channel, key, None);
                }
            }
        }
    }

    fn finish_sustained_notes(&mut self, channel: u8, end_tick: u32) {
        if let Some(sustained) = self.sustained_notes.remove(&channel) {
            for sustained_note in sustained {
                self.notes.push(self.make_note(
                    sustained_note.note,
                    end_tick,
                    channel,
                    sustained_note.key,
                    sustained_note.release_velocity,
                ));
            }
        }
    }

    fn finish_unheld_sustained_notes(&mut self, channel: u8, end_tick: u32) {
        let sustain_down = self
            .sustain_channels
            .get(usize::from(channel))
            .copied()
            .unwrap_or(false);
        let Some(sustained) = self.sustained_notes.remove(&channel) else {
            return;
        };
        let mut still_held = Vec::new();
        for sustained_note in sustained {
            if sustain_down || sustained_note.sostenuto_held {
                still_held.push(sustained_note);
            } else {
                self.notes.push(self.make_note(
                    sustained_note.note,
                    end_tick,
                    channel,
                    sustained_note.key,
                    sustained_note.release_velocity,
                ));
            }
        }
        if !still_held.is_empty() {
            self.sustained_notes.insert(channel, still_held);
        }
    }

    fn stop_channel_notes(&mut self, channel: u8, end_tick: u32) {
        let active_keys = self
            .active_notes
            .keys()
            .copied()
            .filter(|(active_channel, _)| *active_channel == channel)
            .collect::<Vec<_>>();
        for (active_channel, key) in active_keys {
            if let Some(active) = self.active_notes.remove(&(active_channel, key)) {
                for active_note in active {
                    self.notes.push(self.make_note(
                        active_note,
                        end_tick,
                        active_channel,
                        key,
                        None,
                    ));
                }
            }
        }
        self.finish_sustained_notes(channel, end_tick);
    }

    pub fn finish(
        mut self,
        timestamp_micros: Option<u64>,
        finished_at: Instant,
    ) -> MidiRecordingResult {
        let finish_tick = timestamp_micros
            .map(|timestamp| self.tick_for_message(timestamp, finished_at))
            .unwrap_or_else(|| {
                let elapsed_micros = finished_at
                    .saturating_duration_since(self.started_at)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64;
                self.tick_for_elapsed(Duration::from_micros(
                    self.corrected_elapsed_micros(elapsed_micros),
                ))
            })
            .saturating_add(self.start_tick)
            .min(MAX_NOTE_TICK);
        let finish_tick = finish_tick.max(self.last_position_tick);
        for ((channel, key), active) in std::mem::take(&mut self.active_notes) {
            for active_note in active {
                let note = self.make_note(active_note, finish_tick, channel, key, None);
                self.notes.push(note);
            }
        }
        for (channel, sustained) in std::mem::take(&mut self.sustained_notes) {
            for sustained_note in sustained {
                let note = self.make_note(
                    sustained_note.note,
                    finish_tick,
                    channel,
                    sustained_note.key,
                    sustained_note.release_velocity,
                );
                self.notes.push(note);
            }
        }
        self.notes
            .sort_by_key(|note| (note.position, note.midi_channel, note.key, note.length));
        MidiRecordingResult {
            pattern_id: self.pattern_id,
            channel_id: self.channel_id,
            project_path: self.project_path,
            notes: self.notes,
            overflowed: self.overflowed,
        }
    }

    fn tick_for_message(&mut self, timestamp_micros: u64, received_at: Instant) -> u32 {
        if self.timestamp_origin_micros.is_none() {
            let before_first_message = received_at
                .saturating_duration_since(self.started_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            self.timestamp_origin_micros =
                Some(timestamp_micros.saturating_sub(before_first_message));
        }
        let origin = self
            .timestamp_origin_micros
            .expect("the timestamp origin is set above");
        let elapsed_micros = self.corrected_elapsed_micros(timestamp_micros.saturating_sub(origin));
        self.tick_for_elapsed(Duration::from_micros(elapsed_micros))
    }

    fn corrected_elapsed_micros(&self, elapsed_micros: u64) -> u64 {
        let offset_micros = i64::from(self.timing_offset_ms) * 1_000;
        if offset_micros >= 0 {
            elapsed_micros.saturating_sub(offset_micros as u64)
        } else {
            elapsed_micros.saturating_add(offset_micros.unsigned_abs())
        }
    }

    fn tick_for_elapsed(&self, elapsed: Duration) -> u32 {
        if let (Some((tempo_map, song_start_tick)), Some(song_start_frame_micros)) =
            (&self.song_timeline_start, self.song_start_frame_micros)
        {
            let elapsed_micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
            let frame_micros = song_start_frame_micros.saturating_add(elapsed_micros);
            if let Some(song_tick) = tempo_map.tick_at_frame(frame_micros, 1_000_000) {
                let elapsed_ticks = (song_tick - song_start_tick).max(0.0);
                if !elapsed_ticks.is_finite() || elapsed_ticks >= f64::from(MAX_NOTE_TICK) {
                    return MAX_NOTE_TICK;
                }
                return elapsed_ticks.round() as u32;
            }
            return 0;
        }
        let ticks = elapsed.as_secs_f64() * self.tempo_bpm * f64::from(self.ppq) / 60.0;
        if !ticks.is_finite() || ticks >= f64::from(MAX_NOTE_TICK) {
            MAX_NOTE_TICK
        } else {
            ticks.round().max(0.0) as u32
        }
    }

    fn make_note(
        &self,
        active_note: ActiveMidiNote,
        end_tick: u32,
        midi_channel: u8,
        key: u8,
        release_velocity: Option<u8>,
    ) -> PatternNote {
        let position = quantize_recording_tick(active_note.start_tick, self.snap_ticks);
        let end_tick = quantize_recording_tick(end_tick, self.snap_ticks);
        let mut note = PatternNote {
            position,
            channel_id: self.channel_id,
            length: end_tick.saturating_sub(position).max(1),
            key: u16::from(key),
            midi_channel,
            pan: midi_pan_to_fl_note_pan(active_note.pan),
            mod_x: active_note.mod_x,
            mod_y: active_note.mod_y,
            velocity: active_note.velocity,
            fine_pitch: active_note.fine_pitch,
            ..PatternNote::default()
        };
        if let Some(release_velocity) = release_velocity {
            note.release = midi_release_velocity_to_fl_note_release(release_velocity);
        }
        note
    }
}

fn quantize_recording_tick(tick: u32, snap_ticks: Option<u32>) -> u32 {
    let tick = u64::from(tick.min(MAX_NOTE_TICK));
    let Some(quantum) = snap_ticks.filter(|quantum| *quantum > 1) else {
        return tick as u32;
    };
    let quantum = u64::from(quantum);
    let rounded = tick.saturating_add(quantum / 2) / quantum * quantum;
    rounded.min(u64::from(MAX_NOTE_TICK)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fl-midi-input-script-test-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn midi_script_output_handle_applies_callback_and_queues_script_messages() {
        let temporary = TestDirectory::new();
        let script_path = temporary.0.join("device_test.py");
        std::fs::write(
            &script_path,
            "# name=Test Script\nimport device\ndef OnMidiOutMsg(event):\n    if event.port != 5:\n        raise RuntimeError('wrong output port')\n    device.midiOutMsg(0xB0, 0, 1, 2)\n    if event.data1 == 60:\n        event.handled = True\n    else:\n        event.data2 = 10\n",
        )
        .unwrap();
        let script = MidiControllerScript {
            path: script_path,
            name: "Test Script".to_owned(),
            url: None,
            supported_devices: Vec::new(),
            supported_hardware_ids: Vec::new(),
        };
        let (transport_sender, _transport_receiver) = mpsc::sync_channel(4);
        let (midi_output_sender, midi_output_receiver) = mpsc::sync_channel(4);
        let (thru_sender, _thru_receiver) = mpsc::sync_channel(4);
        let enabled = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));
        let dropped_messages = Arc::new(AtomicU64::new(0));
        let thru_queue = MidiThruInputQueue::new(
            thru_sender,
            enabled,
            generation,
            Arc::clone(&dropped_messages),
        );
        let transport_state = Arc::new(MidiScriptTransportState::default());
        transport_state
            .midi_output_port_number
            .store(5, Ordering::Relaxed);
        let worker = MidiInputScriptWorker::start(
            &script,
            "Test Input",
            transport_sender,
            midi_output_sender,
            thru_queue,
            dropped_messages,
            transport_state,
        )
        .unwrap();
        let handle = MidiScriptOutputHandle {
            sender: worker.sender.clone(),
        };

        assert_eq!(handle.process_message(&[0x90, 60, 100]).unwrap(), None);
        assert_eq!(
            handle.process_message(&[0x90, 61, 100]).unwrap(),
            Some(vec![0x90, 61, 10])
        );
        assert_eq!(
            midi_output_receiver.try_iter().collect::<Vec<_>>(),
            [vec![0xB0, 1, 2], vec![0xB0, 1, 2]]
        );
    }

    #[test]
    fn script_midi_output_actions_reach_the_dedicated_output_queue() {
        let (transport_sender, transport_receiver) = mpsc::sync_channel(8);
        let (midi_output_sender, midi_output_receiver) = mpsc::sync_channel(2);
        let dropped_messages = AtomicU64::new(0);
        let received_at = Instant::now();

        queue_script_transport_actions(
            &transport_sender,
            &midi_output_sender,
            &dropped_messages,
            12,
            received_at,
            [
                ScriptTransportAction::MidiOutput(vec![0x90, 60, 100]),
                ScriptTransportAction::SetCurrentTempoMilliBpm(123_456),
                ScriptTransportAction::Start,
                ScriptTransportAction::ShowMainView(4),
                ScriptTransportAction::OpenProjectInfo,
            ],
        );

        assert_eq!(
            midi_output_receiver.try_recv().unwrap(),
            vec![0x90, 60, 100]
        );
        assert_eq!(
            transport_receiver.try_recv().unwrap().event,
            MidiInputEvent::Transport(MidiTransportCommand::SetCurrentTempo(123_456))
        );
        assert_eq!(
            transport_receiver.try_recv().unwrap().event,
            MidiInputEvent::Transport(MidiTransportCommand::Start)
        );
        assert_eq!(
            transport_receiver.try_recv().unwrap().event,
            MidiInputEvent::Transport(MidiTransportCommand::ShowMainView(4))
        );
        assert_eq!(
            transport_receiver.try_recv().unwrap().event,
            MidiInputEvent::Transport(MidiTransportCommand::OpenProjectInfo)
        );
        assert_eq!(dropped_messages.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn script_midi_repeat_observes_millisecond_delay_and_rate() {
        let start = Instant::now();
        let mut repeat = RepeatingMidiOutput::new(vec![0xB0, 7, 99], 10, 20, start);

        let idle_deadline = start + Duration::from_millis(50);
        assert_eq!(
            script_worker_wait_duration(Some(&repeat), idle_deadline, start),
            Duration::from_millis(10)
        );
        assert!(repeat.take_due(start + Duration::from_millis(9)).is_none());
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(10)),
            Some(vec![0xB0, 7, 99])
        );
        assert_eq!(
            script_worker_wait_duration(
                Some(&repeat),
                idle_deadline,
                start + Duration::from_millis(10)
            ),
            Duration::from_millis(20)
        );
        assert!(repeat.take_due(start + Duration::from_millis(29)).is_none());
        assert_eq!(
            repeat.take_due(start + Duration::from_millis(30)),
            Some(vec![0xB0, 7, 99])
        );
    }

    #[test]
    fn zero_delay_script_midi_repeat_queues_immediately_and_can_be_stopped() {
        let (transport_sender, transport_receiver) = mpsc::sync_channel(2);
        let (midi_output_sender, midi_output_receiver) = mpsc::sync_channel(2);
        let dropped_messages = AtomicU64::new(0);
        let mut repeat = None;

        queue_script_actions_with_repeat(
            &transport_sender,
            &midi_output_sender,
            &dropped_messages,
            12,
            Instant::now(),
            [
                ScriptTransportAction::RepeatMidiOutput {
                    bytes: vec![0xB0, 7, 99],
                    delay_ms: 0,
                    rate_ms: 10,
                },
                ScriptTransportAction::Start,
            ],
            &mut repeat,
        );

        assert_eq!(midi_output_receiver.try_recv().unwrap(), vec![0xB0, 7, 99]);
        assert_eq!(
            transport_receiver.try_recv().unwrap().event,
            MidiInputEvent::Transport(MidiTransportCommand::Start)
        );
        assert!(repeat.is_some());

        let remaining =
            apply_script_repeat_actions([ScriptTransportAction::StopRepeatMidiOutput], &mut repeat);
        assert!(remaining.is_empty());
        assert!(repeat.is_none());
        assert_eq!(dropped_messages.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn parses_channel_voice_messages_and_zero_velocity_note_off() {
        assert_eq!(
            parse_midi_message(&[0x93, 60, 101]),
            Some(MidiInputEvent::NoteOn {
                channel: 3,
                key: 60,
                velocity: 101,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x93, 60, 0]),
            Some(MidiInputEvent::NoteOff {
                channel: 3,
                key: 60,
                velocity: 0,
            })
        );
        assert_eq!(
            parse_midi_message(&[0xB1, 74, 99]),
            Some(MidiInputEvent::ControlChange {
                channel: 1,
                controller: 74,
                value: 99,
            })
        );
        assert_eq!(
            parse_midi_message(&[0xE2, 0, 64]),
            Some(MidiInputEvent::PitchBend {
                channel: 2,
                value: 0,
            })
        );
    }

    #[test]
    fn parses_realtime_transport_and_mmc_commands() {
        assert_eq!(
            parse_midi_message(&[0xFA]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Start))
        );
        assert_eq!(
            parse_midi_message(&[0xFB]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Continue))
        );
        assert_eq!(
            parse_midi_message(&[0xFC]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::Stop))
        );
        assert_eq!(
            parse_midi_message(&[0xF0, 0x7F, 0x7F, 0x06, 0x06, 0xF7]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::RecordStart))
        );
        assert_eq!(
            parse_midi_message(&[0xF0, 0x7F, 0x01, 0x06, 0x07, 0xF7]),
            Some(MidiInputEvent::Transport(MidiTransportCommand::RecordStop))
        );
        assert_eq!(parse_midi_message(&[0xF0, 0x7F, 0x00, 0x06, 0x02]), None);
    }

    #[test]
    fn system_exclusive_midi_thru_payload_is_bounded_and_preserves_bytes() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let enabled = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(4));
        let dropped_messages = Arc::new(AtomicU64::new(0));
        let queue = MidiThruInputQueue::new(
            sender,
            Arc::clone(&enabled),
            Arc::clone(&generation),
            Arc::clone(&dropped_messages),
        );
        let message = [0xF0, 0x7D, 0x01, 0x02, 0xF7];

        queue.try_send_system_exclusive(&message);

        let queued = receiver
            .try_recv()
            .expect("SysEx should enter MIDI Thru queue");
        assert_eq!(queued.generation, 4);
        assert!(matches!(
            queued.payload,
            MidiThruInputPayload::SystemExclusive(bytes) if bytes.as_ref() == message
        ));
        assert_eq!(dropped_messages.load(Ordering::Relaxed), 0);
        assert_eq!(generation.load(Ordering::Relaxed), 4);

        queue.try_send_system_exclusive(&[0xF0, 0x01]);
        let mut oversized = vec![0x01; MIDI_THRU_MAX_SYSTEM_EXCLUSIVE_BYTES + 1];
        oversized[0] = 0xF0;
        *oversized.last_mut().unwrap() = 0xF7;
        queue.try_send_system_exclusive(&oversized);
        assert!(receiver.try_recv().is_err());
        assert_eq!(dropped_messages.load(Ordering::Relaxed), 1);
        assert_eq!(generation.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn records_note_timing_velocity_channel_and_target() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 7,
                channel_id: 42,
                project_path: Some(PathBuf::from("song.flp")),
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_000_000,
            received_at: started_at + Duration::from_millis(200),
            event: MidiInputEvent::NoteOn {
                channel: 9,
                key: 60,
                velocity: 101,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_300_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 9,
                key: 60,
                velocity: 64,
            },
        });
        let result = recorder.finish(Some(1_300_000), started_at + Duration::from_millis(500));
        assert_eq!(result.pattern_id, 7);
        assert_eq!(result.channel_id, 42);
        assert_eq!(result.project_path, Some(PathBuf::from("song.flp")));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 38);
        assert_eq!(result.notes[0].length, 58);
        assert_eq!(result.notes[0].key, 60);
        assert_eq!(result.notes[0].midi_channel, 9);
        assert_eq!(result.notes[0].velocity, 101);
        assert_eq!(result.notes[0].pan, 64);
        assert_eq!(result.notes[0].release, 65);
    }

    #[test]
    fn records_channel_expression_at_each_note_on() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        recorder.record(message(
            100,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 10,
                value: 0,
            },
        ));
        recorder.record(message(
            110,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 74,
                value: 0,
            },
        ));
        recorder.record(message(
            120,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 71,
                value: 127,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        ));
        recorder.record(message(
            250,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 74,
                value: 127,
            },
        ));
        recorder.record(message(
            275,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 71,
                value: 0,
            },
        ));
        recorder.record(message(
            300,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 10,
                value: 127,
            },
        ));
        recorder.record(message(
            400,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        ));
        recorder.record(message(
            500,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 62,
                velocity: 90,
            },
        ));
        recorder.record(message(
            600,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 62,
                velocity: 0,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(700));
        assert_eq!(result.notes.len(), 2);
        assert_eq!(result.notes[0].pan, 0);
        assert_eq!(result.notes[0].mod_x, 0);
        assert_eq!(result.notes[0].mod_y, 255);
        assert_eq!(result.notes[1].pan, 128);
        assert_eq!(result.notes[1].mod_x, 255);
        assert_eq!(result.notes[1].mod_y, 0);
    }

    #[test]
    fn sustain_pedal_defers_recorded_note_off_until_pedal_release() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        let mut sustain_channels = [false; 16];
        sustain_channels[0] = true;
        recorder.set_pedal_state(sustain_channels, [false; 16]);
        recorder.record(message(
            100,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100,
            },
        ));
        recorder.record(message(
            100,
            MidiInputEvent::NoteOn {
                channel: 1,
                key: 64,
                velocity: 90,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 88,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOff {
                channel: 1,
                key: 64,
                velocity: 70,
            },
        ));
        assert_eq!(recorder.notes.len(), 1);
        assert_eq!(recorder.notes[0].key, 64);

        recorder.record(message(
            300,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 64,
                value: 63,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(400));
        let sustained = result
            .notes
            .iter()
            .find(|note| note.key == 60)
            .expect("the sustained note should be recorded");
        assert_eq!(sustained.length, 39);
        assert_eq!(
            sustained.release,
            midi_release_velocity_to_fl_note_release(88)
        );
        let other_channel = result
            .notes
            .iter()
            .find(|note| note.key == 64)
            .expect("the note on the other channel should finish immediately");
        assert_eq!(other_channel.length, 19);
    }

    #[test]
    fn sostenuto_captures_pedal_down_notes_and_composes_with_sustain() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        recorder.record(message(
            100,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100,
            },
        ));
        recorder.record(message(
            150,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 66,
                value: 127,
            },
        ));
        recorder.record(message(
            175,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 62,
                velocity: 90,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 62,
                velocity: 70,
            },
        ));
        recorder.record(message(
            210,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 64,
                value: 127,
            },
        ));
        recorder.record(message(
            220,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 88,
            },
        ));
        recorder.record(message(
            250,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 66,
                value: 0,
            },
        ));
        recorder.record(message(
            300,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 64,
                value: 0,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(350));
        let captured = result
            .notes
            .iter()
            .find(|note| note.key == 60)
            .expect("note held at pedal-down should be recorded");
        let uncaptured = result
            .notes
            .iter()
            .find(|note| note.key == 62)
            .expect("note started after pedal-down should be recorded");
        assert_eq!(captured.length, 39);
        assert_eq!(
            captured.release,
            midi_release_velocity_to_fl_note_release(88)
        );
        assert_eq!(uncaptured.length, 4);
        assert_eq!(
            uncaptured.release,
            midi_release_velocity_to_fl_note_release(70)
        );
    }

    #[test]
    fn stopping_recording_finishes_notes_deferred_by_sustain() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        recorder.record(message(
            50,
            MidiInputEvent::ControlChange {
                channel: 0,
                controller: 64,
                value: 64,
            },
        ));
        recorder.record(message(
            100,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 100,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 88,
            },
        ));

        let result = recorder.finish(Some(1_300_000), started_at + Duration::from_millis(300));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].length, 39);
        assert_eq!(
            result.notes[0].release,
            midi_release_velocity_to_fl_note_release(88)
        );
    }

    #[test]
    fn records_pitch_bend_at_each_note_on_using_rpn_sensitivity() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        let message = |elapsed_ms, event| ReceivedMidiMessage {
            timestamp_micros: 1_000_000 + elapsed_ms * 1_000,
            received_at: started_at + Duration::from_millis(elapsed_ms),
            event,
        };
        for (elapsed_ms, controller, value) in
            [(100, 101, 0), (110, 100, 0), (120, 6, 4), (130, 38, 0)]
        {
            recorder.record(message(
                elapsed_ms,
                MidiInputEvent::ControlChange {
                    channel: 0,
                    controller,
                    value,
                },
            ));
        }
        recorder.record(message(
            150,
            MidiInputEvent::PitchBend {
                channel: 0,
                value: 8191,
            },
        ));
        recorder.record(message(
            200,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        ));
        recorder.record(message(
            300,
            MidiInputEvent::PitchBend {
                channel: 0,
                value: -8192,
            },
        ));
        recorder.record(message(
            400,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        ));
        recorder.record(message(
            500,
            MidiInputEvent::NoteOn {
                channel: 0,
                key: 62,
                velocity: 90,
            },
        ));
        recorder.record(message(
            600,
            MidiInputEvent::NoteOff {
                channel: 0,
                key: 62,
                velocity: 0,
            },
        ));

        let result = recorder.finish(None, started_at + Duration::from_millis(700));
        assert_eq!(result.notes.len(), 2);
        assert_eq!(result.notes[0].fine_pitch, 160);
        assert_eq!(result.notes[1].fine_pitch, 80);
    }

    #[test]
    fn pitch_bend_uses_two_semitone_default_sensitivity() {
        let mut state = MidiInputChannelState::default();
        state.update(MidiInputEvent::PitchBend {
            channel: 0,
            value: 8191,
        });
        assert_eq!(state.note_fine_pitch(), 140);

        state.update(MidiInputEvent::PitchBend {
            channel: 0,
            value: -8192,
        });
        assert_eq!(state.note_fine_pitch(), 100);
    }

    #[test]
    fn reset_all_controllers_centers_pitch_bend_and_restores_its_default_range() {
        let mut state = MidiInputChannelState::default();
        for (controller, value) in [(7, 63), (10, 100), (11, 48), (71, 0), (74, 127)] {
            state.update(MidiInputEvent::ControlChange {
                channel: 0,
                controller,
                value,
            });
        }
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 101,
            value: 0,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 100,
            value: 0,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 6,
            value: 12,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 38,
            value: 50,
        });
        state.update(MidiInputEvent::PitchBend {
            channel: 0,
            value: 8191,
        });
        assert_eq!(state.raw_pitch_bend_value(), 16_383);

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 121,
            value: 0,
        });

        assert_eq!(state.raw_pan(), 64);
        assert_eq!(state.raw_channel_volume(), 127);
        assert_eq!(state.raw_expression(), 127);
        assert_eq!(state.raw_mod_x(), 64);
        assert_eq!(state.raw_mod_y(), 64);
        assert_eq!(state.raw_pitch_bend_value(), 8192);
        assert_eq!(state.pitch_bend_offset_cents(), 0);
        assert_eq!(state.note_fine_pitch(), 120);
        assert_eq!(state.raw_pitch_bend_sensitivity_semitones(), 2);
        assert_eq!(state.raw_pitch_bend_sensitivity_cents(), 0);

        // Reset also nulls the selected RPN, so later Data Entry cannot change bend range.
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 6,
            value: 7,
        });
        assert_eq!(state.raw_pitch_bend_sensitivity_semitones(), 2);
    }

    #[test]
    fn channel_volume_and_expression_reset_to_live_defaults() {
        let mut state = MidiInputChannelState::default();
        assert_eq!(state.raw_channel_volume(), 127);
        assert_eq!(state.raw_expression(), 127);

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 7,
            value: 63,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 11,
            value: 48,
        });
        assert_eq!(state.raw_channel_volume(), 63);
        assert_eq!(state.raw_expression(), 48);

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 121,
            value: 0,
        });
        assert_eq!(state.raw_channel_volume(), 127);
        assert_eq!(state.raw_expression(), 127);
    }

    #[test]
    fn pan_volume_and_expression_pairs_receive_lsb_as_14_bit_values() {
        let mut state = MidiInputChannelState::default();
        assert!(!state.pan_lsb_received());
        assert!(!state.channel_volume_lsb_received());
        assert!(!state.expression_lsb_received());

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 10,
            value: 64,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 7,
            value: 64,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 11,
            value: 32,
        });
        assert_eq!(state.raw_pan(), 64);
        assert_eq!(state.raw_channel_volume(), 64);
        assert_eq!(state.raw_expression(), 32);
        assert!(!state.pan_lsb_received());
        assert!(!state.channel_volume_lsb_received());
        assert!(!state.expression_lsb_received());

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 42,
            value: 1,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 39,
            value: 1,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 43,
            value: 65,
        });
        assert_eq!(state.raw_pan_14bit(), (64 << 7) | 1);
        assert_eq!(state.raw_channel_volume_14bit(), (64 << 7) | 1);
        assert_eq!(state.raw_expression_14bit(), (32 << 7) | 65);
        assert!(state.pan_lsb_received());
        assert!(state.channel_volume_lsb_received());
        assert!(state.expression_lsb_received());

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 10,
            value: 63,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 7,
            value: 63,
        });
        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 11,
            value: 31,
        });
        assert_eq!(state.raw_pan_14bit(), (63 << 7) | 1);
        assert_eq!(state.raw_channel_volume_14bit(), (63 << 7) | 1);
        assert_eq!(state.raw_expression_14bit(), (31 << 7) | 65);

        state.update(MidiInputEvent::ControlChange {
            channel: 0,
            controller: 121,
            value: 0,
        });
        assert!(!state.pan_lsb_received());
        assert!(!state.channel_volume_lsb_received());
        assert!(!state.expression_lsb_received());
        assert_eq!(state.raw_pan(), 64);
        assert_eq!(state.raw_pan_14bit(), 8192);
        assert_eq!(state.raw_channel_volume(), 127);
        assert_eq!(state.raw_expression(), 127);
    }

    #[test]
    fn midi_release_velocity_maps_to_fl_note_release_range() {
        assert_eq!(midi_release_velocity_to_fl_note_release(0), 0);
        assert_eq!(midi_release_velocity_to_fl_note_release(64), 65);
        assert_eq!(midi_release_velocity_to_fl_note_release(127), 128);
        assert_eq!(midi_release_velocity_to_fl_note_release(255), 128);
    }

    #[test]
    fn fixed_velocity_override_replaces_incoming_note_on_velocity() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        recorder.record_with_velocity_override(
            ReceivedMidiMessage {
                timestamp_micros: 1_000_000,
                received_at: started_at + Duration::from_millis(200),
                event: MidiInputEvent::NoteOn {
                    channel: 0,
                    key: 60,
                    velocity: 17,
                },
            },
            Some(100),
        );
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_300_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(Some(1_300_000), started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].velocity, 100);
    }

    #[test]
    fn positive_timing_offset_moves_recorded_notes_earlier() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 100,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid timing correction should create a recorder");

        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_250_000,
            received_at: started_at + Duration::from_millis(250),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 29);
        assert_eq!(result.notes[0].length, 48);
    }

    #[test]
    fn closes_held_notes_when_recording_stops() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 110_000,
            received_at: started_at + Duration::from_millis(100),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 48,
                velocity: 80,
            },
        });
        let result = recorder.finish(None, started_at + Duration::from_millis(600));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].release, 64);
        assert_eq!(result.notes[0].length, 96);
    }

    #[test]
    fn quantizes_recorded_note_start_and_end_to_the_nearest_grid() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: Some(24),
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing and snap should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_135_417,
            received_at: started_at + Duration::from_micros(135_417),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_364_583,
            received_at: started_at + Duration::from_micros(364_583),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 24);
        assert_eq!(result.notes[0].length, 48);
    }

    #[test]
    fn quantizes_recorded_notes_to_a_triplet_grid() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 0,
                snap_ticks: Some(32),
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid triplet timing should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_150_000,
            received_at: started_at + Duration::from_millis(150),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_350_000,
            received_at: started_at + Duration::from_millis(350),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 32);
        assert_eq!(result.notes[0].length, 32);
    }

    #[test]
    fn rejects_a_zero_length_recording_snap() {
        assert!(matches!(
            MidiPatternRecorder::new(
                MidiPatternRecorderSettings {
                    pattern_id: 1,
                    channel_id: 2,
                    project_path: None,
                    ppq: 96,
                    tempo_bpm: 120.0,
                    start_tick: 0,
                    snap_ticks: Some(0),
                    timing_offset_ms: 0,
                    song_timeline_start: None,
                },
                Instant::now(),
            ),
            Err("MIDI recording snap requires a non-zero tick value")
        ));
    }

    #[test]
    fn offsets_recorded_notes_from_the_selected_pattern_playhead_position() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 192,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: None,
            },
            started_at,
        )
        .expect("valid project timing and playhead should create a recorder");
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_200_000,
            received_at: started_at + Duration::from_millis(200),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 230);
        assert_eq!(result.notes[0].length, 58);
    }

    #[test]
    fn records_against_playlist_timeline_while_preserving_pattern_offset() {
        let started_at = Instant::now();
        let mut recorder = MidiPatternRecorder::new(
            MidiPatternRecorderSettings {
                pattern_id: 1,
                channel_id: 2,
                project_path: None,
                ppq: 96,
                tempo_bpm: 120.0,
                start_tick: 24,
                snap_ticks: None,
                timing_offset_ms: 0,
                song_timeline_start: Some((
                    PlaylistTempoMap::constant(96, 120.0).expect("valid constant tempo"),
                    96.0,
                )),
            },
            started_at,
        )
        .expect("valid Playlist timing should create a recorder");

        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_250_000,
            received_at: started_at + Duration::from_millis(250),
            event: MidiInputEvent::NoteOn {
                channel: 0,
                key: 60,
                velocity: 90,
            },
        });
        recorder.record(ReceivedMidiMessage {
            timestamp_micros: 1_500_000,
            received_at: started_at + Duration::from_millis(500),
            event: MidiInputEvent::NoteOff {
                channel: 0,
                key: 60,
                velocity: 0,
            },
        });

        let result = recorder.finish(None, started_at + Duration::from_millis(500));
        assert_eq!(result.notes.len(), 1);
        assert_eq!(result.notes[0].position, 72);
        assert_eq!(result.notes[0].length, 48);
    }

    #[test]
    fn full_thru_queue_advances_generation_to_invalidate_stale_events() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let enabled = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));
        let dropped_messages = Arc::new(AtomicU64::new(0));
        let queue = MidiThruInputQueue::new(
            sender,
            enabled,
            Arc::clone(&generation),
            Arc::clone(&dropped_messages),
        );

        queue.try_send(MidiInputEvent::TimingClock);
        queue.try_send(MidiInputEvent::TimingClock);

        assert_eq!(receiver.try_recv().unwrap().generation, 0);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
        assert_eq!(dropped_messages.load(Ordering::Relaxed), 1);
    }
}

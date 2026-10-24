use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::midi_input::{
    MIDI_THRU_INPUT_QUEUE_CAPACITY, MidiInputEvent, MidiInputEvent::*, MidiThruInputMessage,
    MidiThruInputPayload, MidiThruInputQueue, MidiTransportCommand,
};
use crate::midi_output::ConnectedMidiOutput;

const MIDI_THRU_NOTE_STACK_LIMIT: u8 = 16;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MidiThruSettings {
    pub output_channel: Option<u8>,
    pub pitch_class_mask: Option<u16>,
}

pub struct MidiThruWorker {
    input_queue: MidiThruInputQueue,
    control_sender: Sender<ControlCommand>,
    error_receiver: Receiver<String>,
    generation: Arc<AtomicU64>,
    settings: Arc<Mutex<MidiThruSettings>>,
    thread: Option<JoinHandle<()>>,
}

enum ControlCommand {
    Enable {
        generation: u64,
        output: ConnectedMidiOutput,
        acknowledgement: SyncSender<Option<String>>,
    },
    Disable {
        generation: u64,
        acknowledgement: SyncSender<Option<String>>,
    },
    Shutdown {
        acknowledgement: SyncSender<Option<String>>,
    },
}

impl MidiThruWorker {
    pub fn new() -> Self {
        let (input_sender, input_receiver) = mpsc::sync_channel(MIDI_THRU_INPUT_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = mpsc::channel();
        let (error_sender, error_receiver) = mpsc::channel();
        let enabled = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let dropped_messages = Arc::new(AtomicU64::new(0));
        let settings = Arc::new(Mutex::new(MidiThruSettings::default()));
        let input_queue = MidiThruInputQueue::new(
            input_sender.clone(),
            Arc::clone(&enabled),
            Arc::clone(&generation),
            Arc::clone(&dropped_messages),
        );
        let worker_generation = Arc::clone(&generation);
        let worker_enabled = Arc::clone(&enabled);
        let worker_dropped_messages = Arc::clone(&dropped_messages);
        let worker_settings = Arc::clone(&settings);
        let worker = thread::Builder::new()
            .name("MIDI Thru output".to_owned())
            .spawn(move || {
                run_worker(
                    input_receiver,
                    control_receiver,
                    error_sender,
                    worker_generation,
                    worker_enabled,
                    worker_dropped_messages,
                    worker_settings,
                )
            })
            .expect("could not start MIDI Thru worker");

        Self {
            input_queue,
            control_sender,
            error_receiver,
            generation,
            settings,
            thread: Some(worker),
        }
    }

    pub fn input_queue(&self) -> MidiThruInputQueue {
        self.input_queue.clone()
    }

    pub fn set_settings(&self, settings: MidiThruSettings) {
        *self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }

    pub fn enable(&self, output: ConnectedMidiOutput) -> Result<(), String> {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.input_queue_enabled().store(true, Ordering::Release);
        self.send_control(|acknowledgement| ControlCommand::Enable {
            generation,
            output,
            acknowledgement,
        })
    }

    pub fn disable(&self) -> Result<(), String> {
        self.input_queue_enabled().store(false, Ordering::Release);
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.send_control(|acknowledgement| ControlCommand::Disable {
            generation,
            acknowledgement,
        })
    }

    pub fn dropped_count(&self) -> u64 {
        self.input_queue.dropped_count()
    }

    pub fn take_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        while let Ok(error) = self.error_receiver.try_recv() {
            errors.push(error);
        }
        errors
    }

    fn input_queue_enabled(&self) -> &AtomicBool {
        // The queue and worker deliberately share only the state needed by the
        // MIDI input callback. Keep its atomic private to the queue itself.
        self.input_queue.enabled()
    }

    fn send_control(
        &self,
        command: impl FnOnce(SyncSender<Option<String>>) -> ControlCommand,
    ) -> Result<(), String> {
        let (acknowledgement_sender, acknowledgement_receiver) = mpsc::sync_channel(0);
        self.control_sender
            .send(command(acknowledgement_sender))
            .map_err(|_| "MIDI Thru worker stopped unexpectedly".to_owned())?;
        match acknowledgement_receiver.recv() {
            Ok(Some(error)) => Err(error),
            Ok(None) => Ok(()),
            Err(_) => Err("MIDI Thru worker stopped unexpectedly".to_owned()),
        }
    }
}

impl Drop for MidiThruWorker {
    fn drop(&mut self) {
        self.input_queue_enabled().store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        let (acknowledgement_sender, acknowledgement_receiver) = mpsc::sync_channel(0);
        if self
            .control_sender
            .send(ControlCommand::Shutdown {
                acknowledgement: acknowledgement_sender,
            })
            .is_ok()
        {
            let _ = acknowledgement_receiver.recv();
        }
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    input_receiver: Receiver<MidiThruInputMessage>,
    control_receiver: Receiver<ControlCommand>,
    error_sender: Sender<String>,
    generation_atomic: Arc<AtomicU64>,
    enabled_atomic: Arc<AtomicBool>,
    dropped_messages: Arc<AtomicU64>,
    settings: Arc<Mutex<MidiThruSettings>>,
) {
    let mut state = MidiThruWorkerState {
        generation: generation_atomic.load(Ordering::Acquire),
        ..MidiThruWorkerState::default()
    };
    let mut reported_dropped_messages = dropped_messages.load(Ordering::Relaxed);

    loop {
        loop {
            match control_receiver.try_recv() {
                Ok(ControlCommand::Enable {
                    generation: next_generation,
                    output: next_output,
                    acknowledgement,
                }) => {
                    let cleanup_error = state.processor.release_active(state.output.as_mut());
                    state.output = Some(next_output);
                    state.generation = next_generation;
                    state.enabled = true;
                    let _ = acknowledgement.send(cleanup_error);
                }
                Ok(ControlCommand::Disable {
                    generation: next_generation,
                    acknowledgement,
                }) => {
                    let cleanup_error = state.processor.release_active(state.output.as_mut());
                    state.output = None;
                    state.generation = next_generation;
                    state.enabled = false;
                    let _ = acknowledgement.send(cleanup_error);
                }
                Ok(ControlCommand::Shutdown { acknowledgement }) => {
                    let cleanup_error = state.processor.release_active(state.output.as_mut());
                    let _ = acknowledgement.send(cleanup_error);
                    return;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        let latest_generation = generation_atomic.load(Ordering::Acquire);
        if latest_generation != state.generation {
            state.generation = latest_generation;
            if state.enabled
                && let Some(error) = state.processor.release_active(state.output.as_mut())
            {
                state.report_error(error, &generation_atomic, &enabled_atomic, &error_sender);
            }
        }

        let dropped = dropped_messages.load(Ordering::Relaxed);
        if dropped != reported_dropped_messages {
            reported_dropped_messages = dropped;
            if state.enabled
                && let Some(error) = state.processor.release_active(state.output.as_mut())
            {
                state.report_error(error, &generation_atomic, &enabled_atomic, &error_sender);
            }
        }

        match input_receiver.recv_timeout(Duration::from_millis(2)) {
            Ok(message) => {
                if !state.enabled || message.generation != state.generation {
                    continue;
                }
                let current_settings = *settings
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let output = state
                    .output
                    .as_mut()
                    .expect("enabled MIDI Thru has an output");
                let result = match message.payload {
                    MidiThruInputPayload::Event(event) => {
                        state.processor.process(event, current_settings, output)
                    }
                    MidiThruInputPayload::SystemExclusive(bytes) => {
                        state.processor.process_system_exclusive(&bytes, output)
                    }
                };
                if let Err(error) = result {
                    state.report_error(error, &generation_atomic, &enabled_atomic, &error_sender);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = state.processor.release_active(state.output.as_mut());
                return;
            }
        }
    }
}

#[derive(Default)]
struct MidiThruWorkerState {
    processor: MidiThruProcessor,
    output: Option<ConnectedMidiOutput>,
    generation: u64,
    enabled: bool,
}

impl MidiThruWorkerState {
    fn report_error(
        &mut self,
        error: String,
        generation_atomic: &AtomicU64,
        enabled_atomic: &AtomicBool,
        error_sender: &Sender<String>,
    ) {
        let cleanup_error = self.processor.release_active(self.output.as_mut());
        self.output.take();
        self.enabled = false;
        enabled_atomic.store(false, Ordering::Release);
        self.generation = generation_atomic
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let message = cleanup_error.map_or(error.clone(), |cleanup| {
            format!("{error}; MIDI Thru note cleanup failed: {cleanup}")
        });
        let _ = error_sender.send(message);
    }
}

trait MidiThruOutput {
    fn send(&mut self, message: &[u8]) -> Result<(), String>;
}

impl MidiThruOutput for ConnectedMidiOutput {
    fn send(&mut self, message: &[u8]) -> Result<(), String> {
        ConnectedMidiOutput::send(self, message)
    }
}

#[derive(Default)]
struct MidiThruProcessor {
    active_notes: BTreeMap<(u8, u8), u8>,
    note_mapping: BTreeMap<(u8, u8), VecDeque<(u8, u8)>>,
    sustain_channels: BTreeSet<u8>,
}

impl MidiThruProcessor {
    fn process_system_exclusive(
        &mut self,
        bytes: &[u8],
        output: &mut impl MidiThruOutput,
    ) -> Result<(), String> {
        output.send(bytes)
    }

    fn process(
        &mut self,
        event: MidiInputEvent,
        settings: MidiThruSettings,
        output: &mut impl MidiThruOutput,
    ) -> Result<(), String> {
        let input_channel = match event {
            NoteOn { channel, .. }
            | NoteOff { channel, .. }
            | PolyphonicPressure { channel, .. }
            | ControlChange { channel, .. }
            | ProgramChange { channel, .. }
            | ChannelPressure { channel, .. }
            | PitchBend { channel, .. } => Some(channel & 0x0F),
            _ => None,
        };
        let output_channel =
            input_channel.map(|channel| settings.output_channel.unwrap_or(channel) & 0x0F);
        let output_event = match event {
            NoteOn {
                channel,
                key,
                velocity,
            } => NoteOn {
                channel: output_channel.unwrap_or(channel),
                key: settings
                    .pitch_class_mask
                    .map_or(key, |mask| snap_key_to_pitch_classes(key, mask)),
                velocity,
            },
            NoteOff {
                channel,
                key,
                velocity,
            } => {
                let (mapped_channel, mapped_key) = self
                    .note_mapping
                    .get(&(channel & 0x0F, key.min(127)))
                    .and_then(VecDeque::front)
                    .copied()
                    .unwrap_or((output_channel.unwrap_or(channel), key));
                NoteOff {
                    channel: mapped_channel,
                    key: mapped_key,
                    velocity,
                }
            }
            PolyphonicPressure {
                channel,
                key,
                pressure,
            } => PolyphonicPressure {
                channel: output_channel.unwrap_or(channel),
                key,
                pressure,
            },
            ControlChange {
                channel,
                controller,
                value,
            } => ControlChange {
                channel: output_channel.unwrap_or(channel),
                controller,
                value,
            },
            ProgramChange { channel, program } => ProgramChange {
                channel: output_channel.unwrap_or(channel),
                program,
            },
            ChannelPressure { channel, pressure } => ChannelPressure {
                channel: output_channel.unwrap_or(channel),
                pressure,
            },
            PitchBend { channel, value } => PitchBend {
                channel: output_channel.unwrap_or(channel),
                value,
            },
            event => event,
        };
        let Some((bytes, length)) = midi_input_thru_message(output_event) else {
            return Ok(());
        };
        output.send(&bytes[..length])?;

        match (event, output_event) {
            (
                NoteOn { channel, key, .. },
                NoteOn {
                    channel: output_channel,
                    key: output_key,
                    ..
                },
            ) => {
                let count = self
                    .active_notes
                    .entry((output_channel & 0x0F, output_key.min(127)))
                    .or_default();
                *count = count.saturating_add(1).min(MIDI_THRU_NOTE_STACK_LIMIT);
                let mapping = self
                    .note_mapping
                    .entry((channel & 0x0F, key.min(127)))
                    .or_default();
                if mapping.len() < usize::from(MIDI_THRU_NOTE_STACK_LIMIT) {
                    mapping.push_back((output_channel & 0x0F, output_key.min(127)));
                }
            }
            (
                NoteOff { channel, key, .. },
                NoteOff {
                    channel: output_channel,
                    key: output_key,
                    ..
                },
            ) => {
                let source_note = (channel & 0x0F, key.min(127));
                let remove_mapping =
                    self.note_mapping
                        .get_mut(&source_note)
                        .is_some_and(|mapping| {
                            mapping.pop_front();
                            mapping.is_empty()
                        });
                if remove_mapping {
                    self.note_mapping.remove(&source_note);
                }
                let note = (output_channel & 0x0F, output_key.min(127));
                let remove = self.active_notes.get_mut(&note).is_some_and(|count| {
                    if *count <= 1 {
                        true
                    } else {
                        *count -= 1;
                        false
                    }
                });
                if remove {
                    self.active_notes.remove(&note);
                }
            }
            (
                ControlChange {
                    controller: 64,
                    value,
                    ..
                },
                ControlChange {
                    channel: output_channel,
                    ..
                },
            ) => {
                let channel = output_channel & 0x0F;
                if value >= 64 {
                    self.sustain_channels.insert(channel);
                } else {
                    self.sustain_channels.remove(&channel);
                }
            }
            (
                ControlChange {
                    controller: 120 | 123..=127,
                    ..
                },
                ControlChange {
                    channel: output_channel,
                    ..
                },
            ) => {
                let channel = output_channel & 0x0F;
                self.active_notes
                    .retain(|(note_channel, _), _| *note_channel != channel);
                self.note_mapping.retain(|_, mapping| {
                    mapping.retain(|(note_channel, _)| *note_channel != channel);
                    !mapping.is_empty()
                });
            }
            (
                ControlChange {
                    controller: 121, ..
                },
                ControlChange {
                    channel: output_channel,
                    ..
                },
            ) => {
                self.sustain_channels.remove(&(output_channel & 0x0F));
            }
            _ => {}
        }
        Ok(())
    }

    fn release_active<O: MidiThruOutput>(&mut self, output: Option<&mut O>) -> Option<String> {
        let active_notes = std::mem::take(&mut self.active_notes);
        self.note_mapping.clear();
        let sustain_channels = std::mem::take(&mut self.sustain_channels);
        let output = output?;
        let mut first_error = None;
        for channel in sustain_channels {
            let sustain_off = [0xB0 | (channel & 0x0F), 64, 0];
            if let Err(error) = output.send(&sustain_off)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        for ((channel, key), count) in active_notes {
            let note_off = [0x80 | (channel & 0x0F), key.min(127), 0];
            for _ in 0..count {
                if let Err(error) = output.send(&note_off)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }
        first_error
    }
}

fn snap_key_to_pitch_classes(key: u8, pitch_class_mask: u16) -> u8 {
    (0..=127_u8)
        .filter(|candidate| pitch_class_mask & (1 << (candidate % 12)) != 0)
        .min_by_key(|candidate| (candidate.abs_diff(key), *candidate))
        .unwrap_or(key)
}

fn midi_input_thru_message(event: MidiInputEvent) -> Option<([u8; 3], usize)> {
    let (bytes, length) = match event {
        NoteOn {
            channel,
            key,
            velocity,
        } => (
            [0x90 | (channel & 0x0F), key.min(127), velocity.min(127)],
            3,
        ),
        NoteOff {
            channel,
            key,
            velocity,
        } => (
            [0x80 | (channel & 0x0F), key.min(127), velocity.min(127)],
            3,
        ),
        PolyphonicPressure {
            channel,
            key,
            pressure,
        } => (
            [0xA0 | (channel & 0x0F), key.min(127), pressure.min(127)],
            3,
        ),
        ControlChange {
            channel,
            controller,
            value,
        } => (
            [0xB0 | (channel & 0x0F), controller.min(127), value.min(127)],
            3,
        ),
        ProgramChange { channel, program } => ([0xC0 | (channel & 0x0F), program.min(127), 0], 2),
        ChannelPressure { channel, pressure } => {
            ([0xD0 | (channel & 0x0F), pressure.min(127), 0], 2)
        }
        PitchBend { channel, value } => {
            let value = (i32::from(value) + 8192).clamp(0, 0x3FFF) as u16;
            (
                [
                    0xE0 | (channel & 0x0F),
                    (value & 0x7F) as u8,
                    ((value >> 7) & 0x7F) as u8,
                ],
                3,
            )
        }
        TimingClock => ([0xF8, 0, 0], 1),
        SongPosition { sixteenth_notes } => {
            let position = sixteenth_notes.min(0x3FFF);
            (
                [
                    0xF2,
                    (position & 0x7F) as u8,
                    ((position >> 7) & 0x7F) as u8,
                ],
                3,
            )
        }
        Transport(MidiTransportCommand::Start) => ([0xFA, 0, 0], 1),
        Transport(MidiTransportCommand::Continue) => ([0xFB, 0, 0], 1),
        Transport(MidiTransportCommand::Stop) => ([0xFC, 0, 0], 1),
        Transport(
            MidiTransportCommand::Pause
            | MidiTransportCommand::RecordStart
            | MidiTransportCommand::RecordStop
            | MidiTransportCommand::RecordToggle
            | MidiTransportCommand::TogglePlayback
            | MidiTransportCommand::ToggleLoopMode
            | MidiTransportCommand::SetSongPosition(_)
            | MidiTransportCommand::SetSongPositionTicks(_)
            | MidiTransportCommand::SetSongPositionTimeUs(_)
            | MidiTransportCommand::MoveSongPosition { .. }
            | MidiTransportCommand::MarkerJumpJog(_)
            | MidiTransportCommand::MarkerSelJog(_),
        ) => return None,
    };
    Some((bytes, length))
}

#[cfg(test)]
mod tests {
    use super::{MidiThruOutput, MidiThruProcessor, MidiThruSettings};
    use crate::midi_input::MidiInputEvent;

    #[derive(Default)]
    struct TestOutput(Vec<Vec<u8>>);

    impl MidiThruOutput for TestOutput {
        fn send(&mut self, message: &[u8]) -> Result<(), String> {
            self.0.push(message.to_vec());
            Ok(())
        }
    }

    #[test]
    fn note_off_keeps_the_note_on_mapping_after_settings_change() {
        let mut processor = MidiThruProcessor::default();
        let mut output = TestOutput::default();
        processor
            .process(
                MidiInputEvent::NoteOn {
                    channel: 0,
                    key: 61,
                    velocity: 100,
                },
                MidiThruSettings {
                    output_channel: Some(2),
                    pitch_class_mask: Some(1),
                },
                &mut output,
            )
            .unwrap();
        processor
            .process(
                MidiInputEvent::NoteOff {
                    channel: 0,
                    key: 61,
                    velocity: 7,
                },
                MidiThruSettings {
                    output_channel: Some(4),
                    pitch_class_mask: Some(1 << 2),
                },
                &mut output,
            )
            .unwrap();

        assert_eq!(output.0, vec![vec![0x92, 60, 100], vec![0x82, 60, 7]]);
    }

    #[test]
    fn release_active_sends_sustain_off_and_all_duplicate_note_offs() {
        let mut processor = MidiThruProcessor::default();
        let mut output = TestOutput::default();
        for _ in 0..2 {
            processor
                .process(
                    MidiInputEvent::NoteOn {
                        channel: 1,
                        key: 64,
                        velocity: 96,
                    },
                    MidiThruSettings::default(),
                    &mut output,
                )
                .unwrap();
        }
        processor
            .process(
                MidiInputEvent::ControlChange {
                    channel: 1,
                    controller: 64,
                    value: 127,
                },
                MidiThruSettings::default(),
                &mut output,
            )
            .unwrap();
        output.0.clear();

        assert_eq!(processor.release_active(Some(&mut output)), None);
        assert_eq!(
            output.0,
            vec![vec![0xB1, 64, 0], vec![0x81, 64, 0], vec![0x81, 64, 0]]
        );
        assert!(processor.active_notes.is_empty());
        assert!(processor.note_mapping.is_empty());
    }

    #[test]
    fn all_notes_off_controller_clears_tracked_notes() {
        let mut processor = MidiThruProcessor::default();
        let mut output = TestOutput::default();
        for event in [
            MidiInputEvent::NoteOn {
                channel: 3,
                key: 60,
                velocity: 100,
            },
            MidiInputEvent::ControlChange {
                channel: 3,
                controller: 123,
                value: 0,
            },
        ] {
            processor
                .process(event, MidiThruSettings::default(), &mut output)
                .unwrap();
        }
        output.0.clear();

        assert_eq!(processor.release_active(Some(&mut output)), None);
        assert!(output.0.is_empty());
    }

    #[test]
    fn scale_snap_uses_lower_pitch_when_two_notes_are_equally_close() {
        assert_eq!(
            super::snap_key_to_pitch_classes(61, (1 << 0) | (1 << 2)),
            60
        );
    }

    #[test]
    fn system_exclusive_is_forwarded_byte_for_byte() {
        let mut processor = MidiThruProcessor::default();
        let mut output = TestOutput::default();
        let message = [0xF0, 0x7D, 0x01, 0x02, 0xF7];

        processor
            .process_system_exclusive(&message, &mut output)
            .unwrap();

        assert_eq!(output.0, vec![message.to_vec()]);
        assert!(processor.active_notes.is_empty());
        assert!(processor.note_mapping.is_empty());
    }
}

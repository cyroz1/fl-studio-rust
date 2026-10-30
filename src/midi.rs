use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::{
    ChannelMidiOutSettings, FlpDocument, PatternNote, PlaylistClip, PlaylistClipTarget,
    automation_segment_value_with_tension, normalized_automation_point_value,
};

const MTHD: &[u8; 4] = b"MThd";
const MTRK: &[u8; 4] = b"MTrk";
const MAX_VLQ: u64 = 0x0FFF_FFFF;
const MAX_EXPORTED_NOTES: usize = 1_000_000;
const MAX_EXPORTED_CONTROLLER_POINTS: usize = 1_000_000;
const MAX_EXPORTED_TEMPO_EVENTS: usize = 1_000_000;
const MAX_EXPORTED_MARKERS: usize = 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiFile {
    format: u16,
    division: u16,
    tracks: Vec<MidiTrack>,
    original: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiTrack {
    events: Vec<MidiEvent>,
    end_tick: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiEvent {
    delta_ticks: u32,
    absolute_tick: u64,
    kind: MidiEventKind,
    wire_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiNote {
    channel: u8,
    key: u8,
    velocity: u8,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    pitch_bend_offset_cents: i16,
    channel_coarse_tuning: i8,
    channel_fine_tuning_cents: i16,
    start_tick: u64,
    end_tick: Option<u64>,
    release_velocity: Option<u8>,
}

/// One MIDI Control Change message with its source channel, controller number, value, and tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiControlChange {
    channel: u8,
    controller: u8,
    value: u8,
    tick: u64,
}

impl MidiControlChange {
    /// Zero-based MIDI channel in the range 0–15.
    pub fn channel(self) -> u8 {
        self.channel
    }

    /// Standard MIDI Control Change number in the range 0–127.
    pub fn controller(self) -> u8 {
        self.controller
    }

    /// Standard MIDI Control Change value in the range 0–127.
    pub fn value(self) -> u8 {
        self.value
    }

    /// Absolute position in the source track's MIDI ticks.
    pub fn tick(self) -> u64 {
        self.tick
    }
}

/// Options for interpreting MIDI notes and aligning imported events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiImportOptions {
    /// Treat MIDI Note On events with velocity zero as notes. By default, MIDI
    /// defines these events as Note Off messages.
    pub zero_velocity_note_ons_as_notes: bool,
    /// Remove leading empty time before the first imported note. Batch imports
    /// share one start across tracks; independent Format-2 sequences align separately.
    pub realign_events: bool,
    /// Include these MIDI channels, with bit 0 representing MIDI channel 1.
    /// The default mask includes all 16 channels.
    pub midi_channel_mask: u16,
    /// Assign newly created MIDI-track Sampler channels to the first unassigned Mixer inserts.
    /// This option has no effect when MIDI notes are imported into existing channels.
    pub set_mixer_tracks_for_new_channels: bool,
}

impl Default for MidiImportOptions {
    fn default() -> Self {
        Self {
            zero_velocity_note_ons_as_notes: false,
            realign_events: false,
            midi_channel_mask: u16::MAX,
            set_mixer_tracks_for_new_channels: false,
        }
    }
}

#[derive(Clone, Copy)]
struct MidiChannelNoteState {
    pan: u8,
    mod_x: u8,
    mod_y: u8,
    pitch_bend_value: u16,
    pitch_bend_sensitivity_semitones: u8,
    pitch_bend_sensitivity_cents: u8,
    rpn_msb: u8,
    rpn_lsb: u8,
    rpn_selected: bool,
    channel_coarse_tuning: u8,
    channel_fine_tuning: u16,
    sustain_pedal_down: bool,
    sostenuto_pedal_down: bool,
}

impl Default for MidiChannelNoteState {
    fn default() -> Self {
        Self {
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            pitch_bend_value: 8192,
            pitch_bend_sensitivity_semitones: 2,
            pitch_bend_sensitivity_cents: 0,
            rpn_msb: 127,
            rpn_lsb: 127,
            rpn_selected: false,
            channel_coarse_tuning: 64,
            channel_fine_tuning: 8192,
            sustain_pedal_down: false,
            sostenuto_pedal_down: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiTempoEvent {
    tick: u64,
    microseconds_per_quarter: u32,
}

/// The real-time clock encoded by an SMPTE MIDI time division.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiSmpteTimeBase {
    frames_per_second_numerator: u32,
    frames_per_second_denominator: u32,
    ticks_per_frame: u8,
}

impl MidiSmpteTimeBase {
    pub fn frames_per_second(self) -> f64 {
        f64::from(self.frames_per_second_numerator) / f64::from(self.frames_per_second_denominator)
    }

    pub fn ticks_per_frame(self) -> u8 {
        self.ticks_per_frame
    }
}

/// Selects how FL Studio channels are assigned to MIDI channels during export.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MidiChannelMapping {
    /// Preserve each note's stored MIDI channel. A recognized MIDI Out setting overrides it
    /// when that channel does not route MIDI by note color.
    #[default]
    PreserveNoteChannels,
    /// Assign each distinct FL Studio channel its own MIDI channel, wrapping after 16.
    AssignProjectChannels,
}

/// Selects the Standard MIDI File track layout used for export.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MidiExportFileFormat {
    /// Merge conductor and channel events into one track.
    Format0,
    /// Keep a conductor track and separate channel tracks.
    #[default]
    Format1,
}

/// A note to serialize into a Standard MIDI File.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiExportNote {
    pub channel: u8,
    pub key: u8,
    pub velocity: u8,
    /// MIDI Note Off velocity in the range 0–127.
    pub release_velocity: u8,
    /// MIDI channel pan value in the range 0–127.
    pub pan: u8,
    /// MIDI CC 74 value for FL Studio note Mod X (filter cutoff / brightness).
    pub mod_x: u8,
    /// MIDI CC 71 value for FL Studio note Mod Y (filter resonance / timbre).
    pub mod_y: u8,
    pub start_tick: u64,
    pub end_tick: u64,
    /// Fine pitch from equal temperament, expressed in cents from -1200 to +1200.
    pub fine_pitch_offset_cents: i16,
}

/// A named MIDI track representing one FL Studio channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiExportTrack {
    pub name: String,
    pub project_channel_id: Option<u16>,
    pub end_tick: u64,
    pub notes: Vec<MidiExportNote>,
}

struct TimedMidiBytes {
    tick: u64,
    order: u8,
    bytes: Vec<u8>,
}

struct MidiExportMarker {
    tick: u64,
    name: String,
    time_signature: Option<(u8, u8)>,
}

#[derive(Clone, Copy)]
struct PendingMidiExportNote {
    track_index: usize,
    note_index: usize,
    preferred_channel: u8,
    start_tick: u64,
    end_tick: u64,
    fine_pitch_offset_cents: i16,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
}

#[derive(Clone, Copy)]
struct AssignedMidiExportNote {
    track_index: usize,
    note_index: usize,
    channel: u8,
    start_tick: u64,
    end_tick: u64,
    fine_pitch_offset_cents: i16,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
}

#[derive(Clone, Copy)]
struct ActiveMidiExportNote {
    end_tick: u64,
    track_index: usize,
    fine_pitch_offset_cents: i16,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
}

#[derive(Clone, Copy)]
struct EndedMidiExportNote {
    end_tick: u64,
    track_index: usize,
    fine_pitch_offset_cents: i16,
    pan: u8,
    mod_x: u8,
    mod_y: u8,
}

struct MidiExportOptions<'a> {
    file_format: MidiExportFileFormat,
    ppq: u16,
    tempo_bpm: f64,
    time_signature: Option<(u8, u8)>,
    channel_mapping: MidiChannelMapping,
    project_channel_order: &'a [u16],
    tempo_events: &'a [(u64, f64)],
    markers: &'a [MidiExportMarker],
    auxiliary_track: Option<MidiExportAuxiliaryTrack>,
}

struct MidiExportAuxiliaryTrack {
    name: String,
    end_tick: u64,
    events: Vec<TimedMidiBytes>,
}

struct MidiExportSongOptions {
    file_format: MidiExportFileFormat,
    auxiliary_track: Option<MidiExportAuxiliaryTrack>,
}

impl MidiExportTrack {
    pub fn new(name: impl Into<String>, project_channel_id: Option<u16>) -> Self {
        Self {
            name: name.into(),
            project_channel_id,
            end_tick: 0,
            notes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiEventKind {
    ChannelVoice { status: u8, data: Vec<u8> },
    Meta { meta_type: u8, data: Vec<u8> },
    SysEx { status: u8, data: Vec<u8> },
    System { status: u8, data: Vec<u8> },
}

impl MidiFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, MidiError> {
        if bytes.len() < 8 {
            return Err(MidiError::UnexpectedEof {
                offset: bytes.len(),
                context: "MThd chunk header",
            });
        }
        require_tag(bytes, 0, MTHD)?;
        let header_length = read_u32_be(bytes, 4, "MThd length")? as usize;
        if header_length < 6 {
            return Err(MidiError::InvalidHeaderLength(header_length as u32));
        }
        let header_end = 8usize
            .checked_add(header_length)
            .ok_or(MidiError::LengthOverflow)?;
        if header_end > bytes.len() {
            return Err(MidiError::UnexpectedEof {
                offset: bytes.len(),
                context: "MThd contents",
            });
        }

        let format = read_u16_be(bytes, 8, "MIDI format")?;
        let track_count = read_u16_be(bytes, 10, "MIDI track count")?;
        let division = read_u16_be(bytes, 12, "MIDI time division")?;
        let mut cursor = header_end;
        let mut tracks = Vec::with_capacity(usize::from(track_count));

        while tracks.len() < usize::from(track_count) {
            if cursor >= bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: cursor,
                    context: "MTrk chunk",
                });
            }
            if cursor.checked_add(8).ok_or(MidiError::LengthOverflow)? > bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: bytes.len(),
                    context: "MIDI chunk header",
                });
            }

            let tag = &bytes[cursor..cursor + 4];
            let chunk_length = read_u32_be(bytes, cursor + 4, "MIDI chunk length")? as usize;
            let chunk_start = cursor + 8;
            let chunk_end = chunk_start
                .checked_add(chunk_length)
                .ok_or(MidiError::LengthOverflow)?;
            if chunk_end > bytes.len() {
                return Err(MidiError::UnexpectedEof {
                    offset: bytes.len(),
                    context: "MIDI chunk contents",
                });
            }

            cursor = chunk_end;
            if tag == MTRK {
                tracks.push(parse_track(&bytes[chunk_start..chunk_end], chunk_start)?);
            }
        }

        Ok(Self {
            format,
            division,
            tracks,
            original: bytes.to_vec(),
        })
    }

    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn division(&self) -> u16 {
        self.division
    }

    pub fn ticks_per_quarter_note(&self) -> Option<u16> {
        (self.division & 0x8000 == 0).then_some(self.division)
    }

    /// Returns the SMPTE frame clock when the encoded frame code is supported.
    /// The -29 code uses the SMPTE drop-frame rate of 30000/1001 frames per second.
    pub fn smpte_time_base(&self) -> Option<MidiSmpteTimeBase> {
        if self.division & 0x8000 == 0 {
            return None;
        }
        let frame_code = ((self.division >> 8) as u8) as i8;
        let ticks_per_frame = (self.division & 0x00FF) as u8;
        if ticks_per_frame == 0 {
            return None;
        }
        let (frames_per_second_numerator, frames_per_second_denominator) = match frame_code {
            -24 => (24, 1),
            -25 => (25, 1),
            -29 => (30_000, 1_001),
            -30 => (30, 1),
            _ => return None,
        };
        Some(MidiSmpteTimeBase {
            frames_per_second_numerator,
            frames_per_second_denominator,
            ticks_per_frame,
        })
    }

    /// Converts an absolute MIDI tick into elapsed microseconds.
    ///
    /// PPQ files use their tempo map, with `default_microseconds_per_quarter` before
    /// the first tempo event. Format-2 files use only the selected track's tempo
    /// events. SMPTE files use their encoded real-time frame clock and ignore tempo
    /// events, as required by their time division.
    pub fn elapsed_microseconds_at_tick(
        &self,
        tick: u64,
        selected_track: usize,
        default_microseconds_per_quarter: f64,
    ) -> Result<f64, MidiError> {
        if self.ticks_per_quarter_note().is_some() {
            let ppq = self.division;
            if ppq == 0 {
                return Err(MidiError::InvalidTimeDivision(self.division));
            }
            if !default_microseconds_per_quarter.is_finite()
                || default_microseconds_per_quarter <= 0.0
            {
                return Err(MidiError::InvalidTimeDivision(self.division));
            }
            if selected_track >= self.tracks.len() {
                return Err(MidiError::InvalidTrackIndex(selected_track));
            }

            let mut tempo_events = Vec::new();
            for (track_index, track) in self.tracks.iter().enumerate() {
                if self.format == 2 && track_index != selected_track {
                    continue;
                }
                for (event_index, event) in track.events.iter().enumerate() {
                    if let MidiEventKind::Meta {
                        meta_type: 0x51,
                        data,
                    } = &event.kind
                        && data.len() == 3
                    {
                        let microseconds_per_quarter = (u32::from(data[0]) << 16)
                            | (u32::from(data[1]) << 8)
                            | u32::from(data[2]);
                        if microseconds_per_quarter != 0 {
                            tempo_events.push((
                                event.absolute_tick,
                                track_index,
                                event_index,
                                f64::from(microseconds_per_quarter),
                            ));
                        }
                    }
                }
            }
            tempo_events.sort_by_key(|(event_tick, track_index, event_index, _)| {
                (*event_tick, *track_index, *event_index)
            });

            let mut elapsed_microseconds = 0.0;
            let mut previous_tick = 0u64;
            let mut current_tempo = default_microseconds_per_quarter;
            for (tempo_tick, _, _, microseconds_per_quarter) in tempo_events {
                if tempo_tick > tick {
                    break;
                }
                elapsed_microseconds +=
                    (tempo_tick - previous_tick) as f64 * current_tempo / f64::from(ppq);
                previous_tick = tempo_tick;
                current_tempo = microseconds_per_quarter;
            }
            elapsed_microseconds += (tick - previous_tick) as f64 * current_tempo / f64::from(ppq);
            if !elapsed_microseconds.is_finite() {
                return Err(MidiError::LengthOverflow);
            }
            Ok(elapsed_microseconds)
        } else if let Some(time_base) = self.smpte_time_base() {
            let ticks_per_second = f64::from(time_base.frames_per_second_numerator)
                * f64::from(time_base.ticks_per_frame)
                / f64::from(time_base.frames_per_second_denominator);
            let elapsed_microseconds = tick as f64 * 1_000_000.0 / ticks_per_second;
            if !elapsed_microseconds.is_finite() {
                return Err(MidiError::LengthOverflow);
            }
            Ok(elapsed_microseconds)
        } else {
            Err(MidiError::InvalidTimeDivision(self.division))
        }
    }

    pub fn tracks(&self) -> &[MidiTrack] {
        &self.tracks
    }

    /// Returns the untouched Standard MIDI File bytes.
    pub fn encode_lossless(&self) -> Vec<u8> {
        self.original.clone()
    }

    /// Exports one FL Studio pattern to a format-1 MIDI file.
    pub fn encode_project_pattern(
        document: &FlpDocument,
        pattern_id: u16,
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_pattern_with_format(
            document,
            pattern_id,
            channel_mapping,
            MidiExportFileFormat::Format1,
        )
    }

    /// Exports one FL Studio pattern using the requested Standard MIDI File layout.
    pub fn encode_project_pattern_with_format(
        document: &FlpDocument,
        pattern_id: u16,
        channel_mapping: MidiChannelMapping,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        let patterns = document
            .patterns()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(MidiError::InvalidExport(
                "the requested pattern does not exist",
            ))?;
        let mut tracks = tracks_for_notes(document, &pattern.notes)?;
        if tracks.is_empty() {
            tracks.push(MidiExportTrack::new(
                pattern
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .map_or_else(|| format!("Pattern {pattern_id}"), str::to_owned),
                None,
            ));
        }
        Self::encode_project_export_tracks_with_format(
            document,
            &tracks,
            channel_mapping,
            file_format,
        )
    }

    /// Exports one raw Pattern controller stream as a MIDI Control Change track.
    /// The raw stream bytes do not identify a MIDI controller or channel, so both
    /// mappings are explicit. Controller values are clamped to 0.0–1.0 and mapped
    /// linearly to MIDI's 0–127 range; Pattern ticks use the project's PPQ.
    pub fn encode_project_pattern_controller_stream(
        document: &FlpDocument,
        pattern_id: u16,
        raw_stream: (u8, u8),
        midi_controller: u8,
        midi_channel: u8,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_pattern_controller_stream_with_format(
            document,
            pattern_id,
            raw_stream,
            midi_controller,
            midi_channel,
            MidiExportFileFormat::Format1,
        )
    }

    /// Exports one raw Pattern controller stream using the requested MIDI file layout.
    pub fn encode_project_pattern_controller_stream_with_format(
        document: &FlpDocument,
        pattern_id: u16,
        raw_stream: (u8, u8),
        midi_controller: u8,
        midi_channel: u8,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        if midi_controller > 127 {
            return Err(MidiError::InvalidExport(
                "MIDI Control Change number must be from 0 through 127",
            ));
        }
        if midi_channel > 15 {
            return Err(MidiError::InvalidExport(
                "MIDI channel must be from 0 through 15",
            ));
        }
        let ppq = document.header().ppq();
        if ppq == 0 || ppq & 0x8000 != 0 {
            return Err(MidiError::InvalidExport(
                "project PPQ must be in the range 1..=32767",
            ));
        }
        let patterns = document
            .patterns()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(MidiError::InvalidExport(
                "the requested pattern does not exist",
            ))?;
        let controller_points = pattern
            .controllers
            .iter()
            .filter(|point| (point.channel, point.flags) == raw_stream)
            .collect::<Vec<_>>();
        if controller_points.is_empty() {
            return Err(MidiError::InvalidExport(
                "the requested raw Pattern controller stream does not exist",
            ));
        }
        if controller_points.len() > MAX_EXPORTED_CONTROLLER_POINTS {
            return Err(MidiError::InvalidExport(
                "the raw Pattern controller stream has more than one million points",
            ));
        }
        if controller_points
            .iter()
            .any(|point| !point.value().is_finite())
        {
            return Err(MidiError::InvalidExport(
                "raw Pattern controller values must be finite",
            ));
        }

        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
        let tempo = midi_tempo_microseconds_per_quarter(tempo_bpm)?.to_be_bytes();
        let time_signature = document.metadata().time_signature();
        if let Some((numerator, denominator)) = time_signature
            && (numerator == 0 || denominator == 0 || !denominator.is_power_of_two())
        {
            return Err(MidiError::InvalidExport(
                "time signature must have a positive numerator and power-of-two denominator",
            ));
        }

        let mut output = Vec::new();
        output.extend_from_slice(MTHD);
        output.extend_from_slice(&6u32.to_be_bytes());
        let (smf_format, track_count) = match file_format {
            MidiExportFileFormat::Format0 => (0u16, 1u16),
            MidiExportFileFormat::Format1 => (1u16, 2u16),
        };
        output.extend_from_slice(&smf_format.to_be_bytes());
        output.extend_from_slice(&track_count.to_be_bytes());
        output.extend_from_slice(&ppq.to_be_bytes());

        let mut conductor_events = vec![TimedMidiBytes {
            tick: 0,
            order: 0,
            bytes: meta_event_bytes(
                0x03,
                match file_format {
                    MidiExportFileFormat::Format0 => b"FL Studio export" as &[u8],
                    MidiExportFileFormat::Format1 => b"Tempo and meter",
                },
            )?,
        }];
        conductor_events.push(TimedMidiBytes {
            tick: 0,
            order: 1,
            bytes: meta_event_bytes(0x51, &tempo[1..])?,
        });
        if let Some((numerator, denominator)) = time_signature {
            let signature = midi_time_signature_bytes(numerator, denominator)?;
            conductor_events.push(TimedMidiBytes {
                tick: 0,
                order: 2,
                bytes: meta_event_bytes(0x58, &signature)?,
            });
        }
        let mut controller_events = vec![TimedMidiBytes {
            tick: 0,
            order: 0,
            bytes: meta_event_bytes(
                match file_format {
                    MidiExportFileFormat::Format0 => 0x01,
                    MidiExportFileFormat::Format1 => 0x03,
                },
                format!(
                    "Pattern {pattern_id} controller {} / 0x{:02X}",
                    raw_stream.0, raw_stream.1
                )
                .as_bytes(),
            )?,
        }];
        for point in controller_points {
            let value = (point.value().clamp(0.0, 1.0) * 127.0).round() as u8;
            controller_events.push(TimedMidiBytes {
                tick: u64::from(point.position),
                order: 4,
                bytes: vec![0xB0 | midi_channel, midi_controller, value],
            });
        }
        match file_format {
            MidiExportFileFormat::Format0 => {
                let mut events = conductor_events;
                events.extend(controller_events);
                events.sort_by_key(|event| (event.tick, event.order));
                let mut track = Vec::new();
                let mut previous_tick = 0;
                for event in events {
                    write_delta(&mut track, event.tick.saturating_sub(previous_tick))?;
                    track.extend_from_slice(&event.bytes);
                    previous_tick = event.tick;
                }
                write_end_of_track(&mut track, 0)?;
                append_track_chunk(&mut output, track)?;
            }
            MidiExportFileFormat::Format1 => {
                conductor_events.sort_by_key(|event| (event.tick, event.order));
                let mut conductor = Vec::new();
                let mut previous_tick = 0;
                for event in conductor_events {
                    write_delta(&mut conductor, event.tick.saturating_sub(previous_tick))?;
                    conductor.extend_from_slice(&event.bytes);
                    previous_tick = event.tick;
                }
                write_end_of_track(&mut conductor, 0)?;
                append_track_chunk(&mut output, conductor)?;

                controller_events.sort_by_key(|event| (event.tick, event.order));
                let mut controller_track = Vec::new();
                let mut previous_tick = 0;
                for event in controller_events {
                    write_delta(
                        &mut controller_track,
                        event.tick.saturating_sub(previous_tick),
                    )?;
                    controller_track.extend_from_slice(&event.bytes);
                    previous_tick = event.tick;
                }
                write_end_of_track(&mut controller_track, 0)?;
                append_track_chunk(&mut output, controller_track)?;
            }
        }
        Ok(output)
    }

    /// Exports one raw Pattern controller stream across every matching Pattern Clip in an
    /// arrangement as a MIDI Control Change track. The raw stream bytes do not identify a
    /// MIDI controller or channel, so both mappings are explicit. Pattern-relative points
    /// follow each clip's repeat length and are clipped at the Playlist clip boundary. If a
    /// Pattern has no saved or note-inferred length, its clip span is used for one pass.
    pub fn encode_project_song_controller_stream(
        document: &FlpDocument,
        arrangement_id: u16,
        raw_stream: (u8, u8),
        midi_controller: u8,
        midi_channel: u8,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_controller_stream_with_format(
            document,
            arrangement_id,
            raw_stream,
            midi_controller,
            midi_channel,
            MidiExportFileFormat::Format1,
        )
    }

    /// Exports one arrangement's raw Pattern controller stream using the requested MIDI layout.
    pub fn encode_project_song_controller_stream_with_format(
        document: &FlpDocument,
        arrangement_id: u16,
        raw_stream: (u8, u8),
        midi_controller: u8,
        midi_channel: u8,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        if midi_controller > 127 {
            return Err(MidiError::InvalidExport(
                "MIDI Control Change number must be from 0 through 127",
            ));
        }
        if midi_channel > 15 {
            return Err(MidiError::InvalidExport(
                "MIDI channel must be from 0 through 15",
            ));
        }

        let arrangements = document
            .arrangements()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let arrangement = arrangements
            .iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or(MidiError::InvalidExport(
                "the requested Playlist arrangement does not exist",
            ))?;
        let patterns = document
            .patterns()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let patterns_by_id: BTreeMap<_, _> = patterns
            .iter()
            .map(|pattern| (pattern.id, pattern))
            .collect();
        let mut controller_events = Vec::new();
        let mut song_end_tick = 0u64;
        let mut exported_point_count = 0usize;
        let ppq = document.header().ppq();

        for clip in &arrangement.clips {
            if clip.is_muted() || clip.track_index.is_none() || clip.length_ticks == 0 {
                continue;
            }
            let clip_start = u64::from(clip.position_ticks);
            let clip_length = u64::from(clip.length_ticks);
            let clip_end = clip_start
                .checked_add(clip_length)
                .ok_or(MidiError::LengthOverflow)?;
            song_end_tick = song_end_tick.max(clip_end);

            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                continue;
            };
            let Some(pattern) = patterns_by_id.get(&id).copied() else {
                continue;
            };
            if !clip.has_default_scale() {
                return Err(MidiError::InvalidExport(
                    "scaled Playlist Pattern Clips cannot be exported yet",
                ));
            }

            let controller_points = pattern
                .controllers
                .iter()
                .filter(|point| (point.channel, point.flags) == raw_stream)
                .collect::<Vec<_>>();
            if controller_points.is_empty() {
                continue;
            }
            let (source_start_tick, source_end_tick) = clip
                .recognized_pattern_source_window_ticks()
                .ok_or(MidiError::InvalidExport(
                    "Playlist Pattern source windows must be recognized tick windows matching the clip length",
                ))?;
            let source_start_tick = u64::from(source_start_tick);
            let source_end_tick = u64::from(source_end_tick);
            if controller_points
                .iter()
                .any(|point| !point.value().is_finite())
            {
                return Err(MidiError::InvalidExport(
                    "raw Pattern controller values must be finite",
                ));
            }

            let inferred_note_length = pattern
                .notes
                .iter()
                .filter(|note| note.length > 0)
                .map(|note| u64::from(note.position) + u64::from(note.length))
                .max()
                .unwrap_or(0);
            let loop_length = pattern
                .length_ticks
                .map(u64::from)
                .filter(|length| *length > 0)
                .unwrap_or_else(|| {
                    inferred_pattern_loop_length(
                        inferred_note_length,
                        clip_length,
                        ppq,
                        document.metadata().time_signature(),
                    )
                });
            if loop_length == 0 {
                continue;
            }
            let controller_points = controller_points
                .into_iter()
                .filter(|point| u64::from(point.position) < loop_length)
                .collect::<Vec<_>>();
            if controller_points.is_empty() {
                continue;
            }

            let first_point_tick = controller_points
                .iter()
                .map(|point| u64::from(point.position))
                .max()
                .unwrap_or(0);
            let first_repetition = source_start_tick.saturating_sub(first_point_tick) / loop_length;
            let last_repetition = (source_end_tick - 1) / loop_length;
            let mut previous_point = None::<(u64, u64, usize, f64)>;
            for repetition in first_repetition..=last_repetition {
                let repeat_start = repetition
                    .checked_mul(loop_length)
                    .ok_or(MidiError::LengthOverflow)?;
                for (point_index, point) in controller_points.iter().enumerate() {
                    let point_position = u64::from(point.position);
                    let source_tick = repeat_start
                        .checked_add(point_position)
                        .ok_or(MidiError::LengthOverflow)?;
                    if source_tick < source_start_tick {
                        let candidate = (
                            source_tick,
                            repetition,
                            point_index,
                            f64::from(point.value()),
                        );
                        if previous_point.is_none_or(|previous| {
                            (candidate.0, candidate.1, candidate.2)
                                > (previous.0, previous.1, previous.2)
                        }) {
                            previous_point = Some(candidate);
                        }
                        continue;
                    }
                    if source_tick >= source_end_tick {
                        continue;
                    }
                    exported_point_count = exported_point_count.saturating_add(1);
                    if exported_point_count > MAX_EXPORTED_CONTROLLER_POINTS {
                        return Err(MidiError::InvalidExport(
                            "the arrangement expands to more than one million controller points",
                        ));
                    }
                    controller_events.push(TimedMidiBytes {
                        tick: clip_start
                            .checked_add(source_tick - source_start_tick)
                            .ok_or(MidiError::LengthOverflow)?,
                        order: 4,
                        bytes: vec![
                            0xB0 | midi_channel,
                            midi_controller,
                            (point.value().clamp(0.0, 1.0) * 127.0).round() as u8,
                        ],
                    });
                }
            }
            if source_start_tick > 0
                && let Some((_, _, _, normalized_value)) = previous_point
            {
                exported_point_count = exported_point_count.saturating_add(1);
                if exported_point_count > MAX_EXPORTED_CONTROLLER_POINTS {
                    return Err(MidiError::InvalidExport(
                        "the arrangement expands to more than one million controller points",
                    ));
                }
                controller_events.push(TimedMidiBytes {
                    tick: clip_start,
                    order: 3,
                    bytes: vec![
                        0xB0 | midi_channel,
                        midi_controller,
                        (normalized_value.clamp(0.0, 1.0) * 127.0).round() as u8,
                    ],
                });
            }
        }

        if controller_events.is_empty() {
            return Err(MidiError::InvalidExport(
                "the requested raw Pattern controller stream has no points in the arrangement",
            ));
        }

        let auxiliary_track = MidiExportAuxiliaryTrack {
            name: format!(
                "Arrangement {arrangement_id} raw controller {} / 0x{:02X} as CC {midi_controller} channel {}",
                raw_stream.0,
                raw_stream.1,
                midi_channel + 1
            ),
            end_tick: song_end_tick,
            events: controller_events,
        };
        Self::encode_project_song_with_live_filters_and_format_and_auxiliary_track(
            document,
            arrangement_id,
            MidiChannelMapping::PreserveNoteChannels,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
            MidiExportSongOptions {
                file_format,
                auxiliary_track: Some(auxiliary_track),
            },
        )
    }

    /// Exports pattern clips in one FL Studio arrangement to a format-1 MIDI file.
    /// Audio clips and non-pattern clip targets are omitted.
    pub fn encode_project_song(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_with_format(
            document,
            arrangement_id,
            channel_mapping,
            MidiExportFileFormat::Format1,
        )
    }

    /// Exports one Playlist arrangement using the requested Standard MIDI File layout.
    pub fn encode_project_song_with_format(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_with_live_filters_and_format(
            document,
            arrangement_id,
            channel_mapping,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
            file_format,
        )
    }

    /// Exports a Playlist arrangement while omitting notes from selected project channels.
    /// This is used by live MIDI output to honor the Channel Rack enable state without
    /// changing the behavior of explicit MIDI file exports.
    pub fn encode_project_song_excluding_channels(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
        excluded_project_channels: &BTreeSet<u16>,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_with_live_filters(
            document,
            arrangement_id,
            channel_mapping,
            excluded_project_channels,
            &BTreeSet::new(),
            None,
        )
    }

    /// Exports an arrangement for live MIDI output with project mute and solo filters.
    /// These filters are separate from explicit MIDI file export behavior.
    pub fn encode_project_song_with_live_filters(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
        excluded_project_channels: &BTreeSet<u16>,
        disabled_playlist_track_ids: &BTreeSet<u32>,
        soloed_playlist_track_range: Option<(u32, u32)>,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_with_live_filters_and_format(
            document,
            arrangement_id,
            channel_mapping,
            excluded_project_channels,
            disabled_playlist_track_ids,
            soloed_playlist_track_range,
            MidiExportFileFormat::Format1,
        )
    }

    /// Exports an arrangement with live filters and a selected Standard MIDI File layout.
    pub fn encode_project_song_with_live_filters_and_format(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
        excluded_project_channels: &BTreeSet<u16>,
        disabled_playlist_track_ids: &BTreeSet<u32>,
        soloed_playlist_track_range: Option<(u32, u32)>,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_song_with_live_filters_and_format_and_auxiliary_track(
            document,
            arrangement_id,
            channel_mapping,
            excluded_project_channels,
            disabled_playlist_track_ids,
            soloed_playlist_track_range,
            MidiExportSongOptions {
                file_format,
                auxiliary_track: None,
            },
        )
    }

    fn encode_project_song_with_live_filters_and_format_and_auxiliary_track(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
        excluded_project_channels: &BTreeSet<u16>,
        disabled_playlist_track_ids: &BTreeSet<u32>,
        soloed_playlist_track_range: Option<(u32, u32)>,
        options: MidiExportSongOptions,
    ) -> Result<Vec<u8>, MidiError> {
        let MidiExportSongOptions {
            file_format,
            auxiliary_track,
        } = options;
        let arrangements = document
            .arrangements()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let arrangement = arrangements
            .iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or(MidiError::InvalidExport(
                "the requested Playlist arrangement does not exist",
            ))?;
        let patterns = document
            .patterns()
            .map_err(|error| MidiError::Project(error.to_string()))?;
        let patterns_by_id: BTreeMap<_, _> = patterns
            .iter()
            .map(|pattern| (pattern.id, pattern))
            .collect();
        let midi_out_settings = channel_midi_out_settings(document);
        let mut notes_by_channel = BTreeMap::<u16, Vec<MidiExportNote>>::new();
        let mut song_end_tick = 0u64;
        let mut exported_note_count = 0usize;

        for clip in &arrangement.clips {
            if clip.is_muted() || clip.track_index.is_none() || clip.length_ticks == 0 {
                continue;
            }
            let playlist_track_id = clip.playlist_track_id();
            if let Some((first_track, last_track)) = soloed_playlist_track_range {
                if !playlist_track_id
                    .is_some_and(|track_id| (first_track..=last_track).contains(&track_id))
                {
                    continue;
                }
            } else if playlist_track_id
                .is_some_and(|track_id| disabled_playlist_track_ids.contains(&track_id))
            {
                continue;
            }
            let clip_end = u64::from(clip.position_ticks)
                .checked_add(u64::from(clip.length_ticks))
                .ok_or(MidiError::LengthOverflow)?;
            song_end_tick = song_end_tick.max(clip_end);
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                continue;
            };
            let Some(pattern) = patterns_by_id.get(&id).copied() else {
                continue;
            };
            if !clip.has_default_scale() {
                return Err(MidiError::InvalidExport(
                    "scaled Playlist Pattern Clips cannot be exported yet",
                ));
            }
            let (source_start_tick, source_end_tick) = clip
                .recognized_pattern_source_window_ticks()
                .ok_or(MidiError::InvalidExport(
                    "Playlist Pattern source windows must be recognized tick windows matching the clip length",
                ))?;
            let source_start_tick = u64::from(source_start_tick);
            let source_end_tick = u64::from(source_end_tick);
            let clip_length = u64::from(clip.length_ticks);
            if clip_length == 0 {
                continue;
            }
            let inferred_length = pattern
                .notes
                .iter()
                .filter(|note| note.length > 0)
                .map(|note| u64::from(note.position) + u64::from(note.length))
                .max()
                .unwrap_or(0);
            let loop_length = pattern
                .length_ticks
                .map(u64::from)
                .filter(|length| *length > 0)
                .unwrap_or_else(|| {
                    inferred_pattern_loop_length(
                        inferred_length,
                        clip_length,
                        document.header().ppq(),
                        document.metadata().time_signature(),
                    )
                });
            if loop_length == 0 {
                continue;
            }
            let clip_start = u64::from(clip.position_ticks);
            let play_truncated_notes = document
                .project_settings()
                .is_none_or(|settings| settings.play_truncated_notes_in_clips);
            let first_repetition = pattern
                .notes
                .iter()
                .filter(|note| {
                    !note.is_muted()
                        && !excluded_project_channels.contains(&note.channel_id)
                        && u64::from(note.position) < source_end_tick
                })
                .map(|note| {
                    first_pattern_note_repetition_for_source_window(
                        note,
                        source_start_tick,
                        loop_length,
                        play_truncated_notes,
                    )
                })
                .min();
            let last_repetition = (source_end_tick - 1) / loop_length;
            let first_repetition = first_repetition.unwrap_or_else(|| last_repetition + 1);
            for repetition in first_repetition..=last_repetition {
                let repeat_start = repetition
                    .checked_mul(loop_length)
                    .ok_or(MidiError::LengthOverflow)?;
                for note in &pattern.notes {
                    if note.is_muted() || excluded_project_channels.contains(&note.channel_id) {
                        continue;
                    }
                    let source_note_start = repeat_start
                        .checked_add(u64::from(note.position))
                        .ok_or(MidiError::LengthOverflow)?;
                    if source_note_start >= source_end_tick {
                        continue;
                    }
                    let source_note_end = source_note_start
                        .checked_add(u64::from(note.length))
                        .ok_or(MidiError::LengthOverflow)?;
                    let starts_before_source_window = source_note_start < source_start_tick;
                    if starts_before_source_window
                        && (!play_truncated_notes || source_note_end <= source_start_tick)
                    {
                        continue;
                    }
                    let relative_start =
                        source_note_start.max(source_start_tick) - source_start_tick;
                    exported_note_count = exported_note_count.saturating_add(1);
                    if exported_note_count > MAX_EXPORTED_NOTES {
                        return Err(MidiError::InvalidExport(
                            "the arrangement expands to more than one million MIDI notes",
                        ));
                    }
                    let start_tick = clip_start
                        .checked_add(relative_start)
                        .ok_or(MidiError::LengthOverflow)?;
                    let relative_end = source_note_end
                        .checked_sub(source_start_tick)
                        .ok_or(MidiError::LengthOverflow)?;
                    let stop_tick = clip_start
                        .checked_add(relative_end)
                        .ok_or(MidiError::LengthOverflow)?
                        .min(clip_end);
                    notes_by_channel
                        .entry(note.channel_id)
                        .or_default()
                        .push(export_note(
                            note,
                            start_tick,
                            stop_tick,
                            midi_out_settings.get(&note.channel_id).copied(),
                        ));
                }
            }
        }

        let mut tracks = tracks_for_grouped_notes(document, notes_by_channel)?;
        if tracks.is_empty() {
            tracks.push(MidiExportTrack::new("Song", None));
        }
        for track in &mut tracks {
            track.end_tick = song_end_tick;
        }
        let tempo_events = song_tempo_events(document, &arrangement.clips)?;
        let mut markers = Vec::with_capacity(arrangement.time_markers.len());
        for marker in &arrangement.time_markers {
            let time_signature = if marker.is_signature() {
                Some((
                    marker.numerator().ok_or(MidiError::InvalidExport(
                        "a Playlist signature marker has no numerator",
                    ))?,
                    marker.denominator().ok_or(MidiError::InvalidExport(
                        "a Playlist signature marker has no denominator",
                    ))?,
                ))
            } else {
                None
            };
            markers.push(MidiExportMarker {
                tick: u64::from(marker.position_ticks()),
                name: marker.name().unwrap_or_default().to_owned(),
                time_signature,
            });
        }
        Self::encode_project_export_tracks_with_tempo_events_and_markers_and_format(
            document,
            &tracks,
            channel_mapping,
            &tempo_events,
            &markers,
            file_format,
            auxiliary_track,
        )
    }

    /// Serializes named channel tracks and a conductor track to Standard MIDI File format 1.
    pub fn encode_export_tracks(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_export_tracks_with_format(
            tracks,
            ppq,
            tempo_bpm,
            time_signature,
            channel_mapping,
            MidiExportFileFormat::Format1,
        )
    }

    /// Serializes named channel tracks using the requested Standard MIDI File layout.
    pub fn encode_export_tracks_with_format(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_export_tracks_with_channel_order_and_format(
            tracks,
            ppq,
            tempo_bpm,
            time_signature,
            channel_mapping,
            &[],
            file_format,
        )
    }

    fn encode_project_export_tracks_with_format(
        document: &FlpDocument,
        tracks: &[MidiExportTrack],
        channel_mapping: MidiChannelMapping,
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_project_export_tracks_with_tempo_events_and_markers_and_format(
            document,
            tracks,
            channel_mapping,
            &[],
            &[],
            file_format,
            None,
        )
    }

    fn encode_project_export_tracks_with_tempo_events_and_markers_and_format(
        document: &FlpDocument,
        tracks: &[MidiExportTrack],
        channel_mapping: MidiChannelMapping,
        tempo_events: &[(u64, f64)],
        markers: &[MidiExportMarker],
        file_format: MidiExportFileFormat,
        auxiliary_track: Option<MidiExportAuxiliaryTrack>,
    ) -> Result<Vec<u8>, MidiError> {
        let channel_order = document
            .channels()
            .into_iter()
            .map(|channel| channel.id())
            .collect::<Vec<_>>();
        Self::encode_export_tracks_with_channel_order_and_tempo_events(
            tracks,
            MidiExportOptions {
                file_format,
                ppq: document.header().ppq(),
                tempo_bpm: document.metadata().tempo_bpm().unwrap_or(140.0),
                time_signature: document.metadata().time_signature(),
                channel_mapping,
                project_channel_order: &channel_order,
                tempo_events,
                markers,
                auxiliary_track,
            },
        )
    }

    #[cfg(test)]
    fn encode_export_tracks_with_channel_order(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
        project_channel_order: &[u16],
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_export_tracks_with_channel_order_and_format(
            tracks,
            ppq,
            tempo_bpm,
            time_signature,
            channel_mapping,
            project_channel_order,
            MidiExportFileFormat::Format1,
        )
    }

    fn encode_export_tracks_with_channel_order_and_format(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
        project_channel_order: &[u16],
        file_format: MidiExportFileFormat,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_export_tracks_with_channel_order_and_tempo_events(
            tracks,
            MidiExportOptions {
                file_format,
                ppq,
                tempo_bpm,
                time_signature,
                channel_mapping,
                project_channel_order,
                tempo_events: &[],
                markers: &[],
                auxiliary_track: None,
            },
        )
    }

    fn encode_export_tracks_with_channel_order_and_tempo_events(
        tracks: &[MidiExportTrack],
        options: MidiExportOptions<'_>,
    ) -> Result<Vec<u8>, MidiError> {
        let MidiExportOptions {
            file_format,
            ppq,
            tempo_bpm,
            time_signature,
            channel_mapping,
            project_channel_order,
            tempo_events,
            markers,
            auxiliary_track,
        } = options;
        if ppq == 0 || ppq & 0x8000 != 0 {
            return Err(MidiError::InvalidExport(
                "project PPQ must be in the range 1..=32767",
            ));
        }
        midi_tempo_microseconds_per_quarter(tempo_bpm)?;
        if let Some((numerator, denominator)) = time_signature
            && (numerator == 0 || denominator == 0 || !denominator.is_power_of_two())
        {
            return Err(MidiError::InvalidExport(
                "time signature must have a positive numerator and power-of-two denominator",
            ));
        }

        let fallback_track;
        let tracks = if tracks.is_empty() {
            fallback_track = [MidiExportTrack::new("MIDI", None)];
            &fallback_track[..]
        } else {
            tracks
        };
        let auxiliary_end_tick = auxiliary_track.as_ref().map_or(0, |track| track.end_tick);
        let (smf_format, track_count) = match file_format {
            MidiExportFileFormat::Format0 => (0_u16, 1_u16),
            MidiExportFileFormat::Format1 => (
                1_u16,
                u16::try_from(
                    tracks
                        .len()
                        .checked_add(1)
                        .and_then(|count| count.checked_add(usize::from(auxiliary_track.is_some())))
                        .ok_or(MidiError::LengthOverflow)?,
                )
                .map_err(|_| MidiError::LengthOverflow)?,
            ),
        };
        let mut ordered_channel_ids = Vec::new();
        let mut seen_channels = std::collections::BTreeSet::new();
        for channel_id in project_channel_order.iter().copied().chain(
            tracks
                .iter()
                .filter_map(|track| track.project_channel_id)
                .collect::<std::collections::BTreeSet<_>>(),
        ) {
            if seen_channels.insert(channel_id) {
                ordered_channel_ids.push(channel_id);
            }
        }
        let channel_indices: BTreeMap<_, _> = ordered_channel_ids
            .into_iter()
            .enumerate()
            .map(|(index, channel_id)| (channel_id, (index % 16) as u8))
            .collect();
        let assigned_notes =
            assign_export_note_channels(tracks, channel_mapping, &channel_indices)?;

        let mut output = Vec::new();
        output.extend_from_slice(MTHD);
        output.extend_from_slice(&6u32.to_be_bytes());
        output.extend_from_slice(&smf_format.to_be_bytes());
        output.extend_from_slice(&track_count.to_be_bytes());
        output.extend_from_slice(&ppq.to_be_bytes());

        let mut conductor_events = vec![TimedMidiBytes {
            tick: 0,
            order: 0,
            bytes: meta_event_bytes(
                0x03,
                match file_format {
                    MidiExportFileFormat::Format0 => &b"FL Studio export"[..],
                    MidiExportFileFormat::Format1 => &b"Tempo and meter"[..],
                },
            )?,
        }];
        if let Some((numerator, denominator)) = time_signature {
            let signature = midi_time_signature_bytes(numerator, denominator)?;
            conductor_events.push(TimedMidiBytes {
                tick: 0,
                order: 1,
                bytes: meta_event_bytes(0x58, &signature)?,
            });
        }
        let mut ordered_tempo_events = Vec::with_capacity(tempo_events.len().saturating_add(1));
        ordered_tempo_events.push((0, tempo_bpm));
        ordered_tempo_events.extend_from_slice(tempo_events);
        ordered_tempo_events.sort_by_key(|(tick, _)| *tick);
        for (tick, bpm) in ordered_tempo_events {
            let tempo_bytes = midi_tempo_microseconds_per_quarter(bpm)?.to_be_bytes();
            conductor_events.push(TimedMidiBytes {
                tick,
                order: 2,
                bytes: meta_event_bytes(0x51, &tempo_bytes[1..])?,
            });
        }
        if markers.len() > MAX_EXPORTED_MARKERS {
            return Err(MidiError::InvalidExport(
                "the arrangement has more than one million time markers",
            ));
        }
        for marker in markers {
            conductor_events.push(TimedMidiBytes {
                tick: marker.tick,
                order: 3,
                bytes: meta_event_bytes(0x06, marker.name.as_bytes())?,
            });
            if let Some((numerator, denominator)) = marker.time_signature {
                let signature = midi_time_signature_bytes(numerator, denominator)?;
                conductor_events.push(TimedMidiBytes {
                    tick: marker.tick,
                    order: 4,
                    bytes: meta_event_bytes(0x58, &signature)?,
                });
            }
        }
        conductor_events.sort_by_key(|event| (event.tick, event.order));

        let mut events_by_track = tracks
            .iter()
            .map(|_| Vec::<TimedMidiBytes>::new())
            .collect::<Vec<_>>();
        for (track_index, track) in tracks.iter().enumerate() {
            let events = &mut events_by_track[track_index];
            events.push(TimedMidiBytes {
                tick: 0,
                order: 0,
                bytes: meta_event_bytes(
                    match file_format {
                        MidiExportFileFormat::Format0 => 0x01,
                        MidiExportFileFormat::Format1 => 0x03,
                    },
                    track.name.as_bytes(),
                )?,
            });
        }

        let mut configured_pitch_channels = std::collections::BTreeSet::new();
        for assigned in assigned_notes.iter().flatten() {
            if assigned.fine_pitch_offset_cents != 0 {
                configured_pitch_channels.insert((assigned.track_index, assigned.channel));
            }
        }
        for (track_index, channel) in configured_pitch_channels {
            for bytes in pitch_bend_range_setup_messages(channel) {
                events_by_track[track_index].push(TimedMidiBytes {
                    tick: 0,
                    order: 1,
                    bytes: bytes.to_vec(),
                });
            }
        }

        for track_notes in &assigned_notes {
            for assigned in track_notes {
                let note = &tracks[assigned.track_index].notes[assigned.note_index];
                let events = &mut events_by_track[assigned.track_index];
                let velocity = note.velocity.min(127);
                events.push(TimedMidiBytes {
                    tick: assigned.start_tick,
                    order: 4,
                    bytes: vec![0xB0 | assigned.channel, 10, assigned.pan],
                });
                events.push(TimedMidiBytes {
                    tick: assigned.start_tick,
                    order: 4,
                    bytes: vec![0xB0 | assigned.channel, 74, assigned.mod_x],
                });
                events.push(TimedMidiBytes {
                    tick: assigned.start_tick,
                    order: 4,
                    bytes: vec![0xB0 | assigned.channel, 71, assigned.mod_y],
                });
                events.push(TimedMidiBytes {
                    tick: assigned.start_tick,
                    order: 5,
                    bytes: vec![0x90 | assigned.channel, note.key, velocity],
                });
                events.push(TimedMidiBytes {
                    tick: assigned.end_tick,
                    order: 2,
                    bytes: vec![
                        0x80 | assigned.channel,
                        note.key,
                        note.release_velocity.min(127),
                    ],
                });
                if assigned.fine_pitch_offset_cents != 0 {
                    events.push(TimedMidiBytes {
                        tick: assigned.start_tick,
                        order: 4,
                        bytes: pitch_bend_bytes(assigned.channel, assigned.fine_pitch_offset_cents),
                    });
                }
            }
        }
        for (track_index, channel, tick) in pitch_bend_reset_events(&assigned_notes) {
            events_by_track[track_index].push(TimedMidiBytes {
                tick,
                order: 3,
                bytes: pitch_bend_bytes(channel, 0),
            });
        }

        match file_format {
            MidiExportFileFormat::Format0 => {
                let mut events = conductor_events;
                for track_events in events_by_track {
                    events.extend(track_events);
                }
                if let Some(auxiliary_track) = auxiliary_track {
                    events.push(TimedMidiBytes {
                        tick: 0,
                        order: 0,
                        bytes: meta_event_bytes(0x01, auxiliary_track.name.as_bytes())?,
                    });
                    events.extend(auxiliary_track.events);
                }
                events.sort_by_key(|event| (event.tick, event.order));
                let mut track_bytes = Vec::new();
                let mut previous_tick = 0_u64;
                for event in events {
                    write_delta(&mut track_bytes, event.tick.saturating_sub(previous_tick))?;
                    track_bytes.extend_from_slice(&event.bytes);
                    previous_tick = event.tick;
                }
                let end_tick = tracks
                    .iter()
                    .map(|track| track.end_tick)
                    .max()
                    .unwrap_or(0)
                    .max(auxiliary_end_tick);
                write_end_of_track(
                    &mut track_bytes,
                    end_tick.max(previous_tick).saturating_sub(previous_tick),
                )?;
                append_track_chunk(&mut output, track_bytes)?;
            }
            MidiExportFileFormat::Format1 => {
                let mut conductor = Vec::new();
                let mut previous_tick = 0_u64;
                for event in conductor_events {
                    write_delta(&mut conductor, event.tick.saturating_sub(previous_tick))?;
                    conductor.extend_from_slice(&event.bytes);
                    previous_tick = event.tick;
                }
                write_end_of_track(&mut conductor, 0)?;
                append_track_chunk(&mut output, conductor)?;

                for (track_index, track) in tracks.iter().enumerate() {
                    let mut events = std::mem::take(&mut events_by_track[track_index]);
                    events.sort_by_key(|event| (event.tick, event.order));
                    let mut track_bytes = Vec::new();
                    let mut previous_tick = 0_u64;
                    for event in events {
                        write_delta(&mut track_bytes, event.tick.saturating_sub(previous_tick))?;
                        track_bytes.extend_from_slice(&event.bytes);
                        previous_tick = event.tick;
                    }
                    write_end_of_track(
                        &mut track_bytes,
                        track
                            .end_tick
                            .max(previous_tick)
                            .saturating_sub(previous_tick),
                    )?;
                    append_track_chunk(&mut output, track_bytes)?;
                }
                if let Some(auxiliary_track) = auxiliary_track {
                    let mut events = vec![TimedMidiBytes {
                        tick: 0,
                        order: 0,
                        bytes: meta_event_bytes(0x03, auxiliary_track.name.as_bytes())?,
                    }];
                    events.extend(auxiliary_track.events);
                    events.sort_by_key(|event| (event.tick, event.order));
                    let mut track_bytes = Vec::new();
                    let mut previous_tick = 0_u64;
                    for event in events {
                        write_delta(&mut track_bytes, event.tick.saturating_sub(previous_tick))?;
                        track_bytes.extend_from_slice(&event.bytes);
                        previous_tick = event.tick;
                    }
                    write_end_of_track(
                        &mut track_bytes,
                        auxiliary_end_tick.saturating_sub(previous_tick),
                    )?;
                    append_track_chunk(&mut output, track_bytes)?;
                }
            }
        }
        Ok(output)
    }
}

fn assign_export_note_channels(
    tracks: &[MidiExportTrack],
    channel_mapping: MidiChannelMapping,
    channel_indices: &BTreeMap<u16, u8>,
) -> Result<Vec<Vec<AssignedMidiExportNote>>, MidiError> {
    let mut pending = Vec::new();
    for (track_index, track) in tracks.iter().enumerate() {
        let assigned_channel = track
            .project_channel_id
            .and_then(|id| channel_indices.get(&id).copied())
            .unwrap_or(0);
        for (note_index, note) in track.notes.iter().enumerate() {
            if note.key > 127 {
                return Err(MidiError::InvalidExport(
                    "a note key exceeds the MIDI range 0..=127",
                ));
            }
            if !(-1200..=1200).contains(&note.fine_pitch_offset_cents) {
                return Err(MidiError::InvalidExport(
                    "a note fine-pitch offset exceeds the MIDI pitch-bend range of +/-1200 cents",
                ));
            }
            pending.push(PendingMidiExportNote {
                track_index,
                note_index,
                preferred_channel: match channel_mapping {
                    MidiChannelMapping::PreserveNoteChannels => note.channel & 0x0F,
                    MidiChannelMapping::AssignProjectChannels => assigned_channel,
                },
                start_tick: note.start_tick,
                end_tick: note.end_tick.max(note.start_tick.saturating_add(1)),
                fine_pitch_offset_cents: note.fine_pitch_offset_cents,
                pan: note.pan.min(127),
                mod_x: note.mod_x.min(127),
                mod_y: note.mod_y.min(127),
            });
        }
    }
    pending.sort_by_key(|note| (note.start_tick, note.track_index, note.note_index));
    let reserved_channels = pending
        .iter()
        .map(|note| note.preferred_channel)
        .collect::<std::collections::BTreeSet<_>>();

    let mut active = (0..16)
        .map(|_| Vec::<ActiveMidiExportNote>::new())
        .collect::<Vec<_>>();
    let mut last_ended = vec![Vec::<EndedMidiExportNote>::new(); 16];
    let mut assigned = vec![Vec::new(); tracks.len()];

    for note in pending {
        for channel in 0..16 {
            let mut still_active = Vec::with_capacity(active[channel].len());
            for voice in active[channel].drain(..) {
                if voice.end_tick <= note.start_tick {
                    match last_ended[channel]
                        .first()
                        .map(|previous| previous.end_tick)
                    {
                        Some(previous_end) if voice.end_tick > previous_end => {
                            last_ended[channel].clear();
                            last_ended[channel].push(EndedMidiExportNote {
                                end_tick: voice.end_tick,
                                track_index: voice.track_index,
                                fine_pitch_offset_cents: voice.fine_pitch_offset_cents,
                                pan: voice.pan,
                                mod_x: voice.mod_x,
                                mod_y: voice.mod_y,
                            });
                        }
                        Some(previous_end) if voice.end_tick == previous_end => {
                            last_ended[channel].push(EndedMidiExportNote {
                                end_tick: voice.end_tick,
                                track_index: voice.track_index,
                                fine_pitch_offset_cents: voice.fine_pitch_offset_cents,
                                pan: voice.pan,
                                mod_x: voice.mod_x,
                                mod_y: voice.mod_y,
                            });
                        }
                        None => last_ended[channel].push(EndedMidiExportNote {
                            end_tick: voice.end_tick,
                            track_index: voice.track_index,
                            fine_pitch_offset_cents: voice.fine_pitch_offset_cents,
                            pan: voice.pan,
                            mod_x: voice.mod_x,
                            mod_y: voice.mod_y,
                        }),
                        _ => {}
                    }
                } else {
                    still_active.push(voice);
                }
            }
            active[channel] = still_active;
        }

        let channel_can_carry_note = |channel: usize| {
            let active_pitch_matches = active[channel]
                .iter()
                .all(|voice| voice.fine_pitch_offset_cents == note.fine_pitch_offset_cents);
            let active_pan_matches = active[channel].iter().all(|voice| voice.pan == note.pan);
            let active_mod_x_matches = active[channel]
                .iter()
                .all(|voice| voice.mod_x == note.mod_x);
            let active_mod_y_matches = active[channel]
                .iter()
                .all(|voice| voice.mod_y == note.mod_y);
            let channel_state_change_is_ordered = last_ended[channel].iter().all(|previous| {
                previous.end_tick != note.start_tick
                    || (previous.fine_pitch_offset_cents == note.fine_pitch_offset_cents
                        && previous.pan == note.pan
                        && previous.mod_x == note.mod_x
                        && previous.mod_y == note.mod_y)
                    || previous.track_index == note.track_index
            });
            active_pitch_matches
                && active_pan_matches
                && active_mod_x_matches
                && active_mod_y_matches
                && (!active[channel].is_empty() || channel_state_change_is_ordered)
        };

        let preferred_channel = usize::from(note.preferred_channel);
        let channel = if channel_can_carry_note(preferred_channel) {
            preferred_channel
        } else if let Some(channel) = (0..16).find(|channel| {
            !reserved_channels.contains(&(*channel as u8))
                && active[*channel].is_empty()
                && channel_can_carry_note(*channel)
        }) {
            channel
        } else {
            return Err(MidiError::InvalidExport(
                "simultaneous notes with independent pan, modulation, or pitch need a free MIDI channel, but none is available under this channel mapping",
            ));
        };

        active[channel].push(ActiveMidiExportNote {
            end_tick: note.end_tick,
            track_index: note.track_index,
            fine_pitch_offset_cents: note.fine_pitch_offset_cents,
            pan: note.pan,
            mod_x: note.mod_x,
            mod_y: note.mod_y,
        });
        assigned[note.track_index].push(AssignedMidiExportNote {
            track_index: note.track_index,
            note_index: note.note_index,
            channel: channel as u8,
            start_tick: note.start_tick,
            end_tick: note.end_tick,
            fine_pitch_offset_cents: note.fine_pitch_offset_cents,
            pan: note.pan,
            mod_x: note.mod_x,
            mod_y: note.mod_y,
        });
    }

    Ok(assigned)
}

fn pitch_bend_reset_events(assigned: &[Vec<AssignedMidiExportNote>]) -> Vec<(usize, u8, u64)> {
    let mut notes_by_channel = (0..16)
        .map(|_| Vec::<AssignedMidiExportNote>::new())
        .collect::<Vec<_>>();
    for note in assigned.iter().flatten() {
        notes_by_channel[usize::from(note.channel)].push(*note);
    }

    let mut resets = Vec::new();
    for (channel, mut notes) in notes_by_channel.into_iter().enumerate() {
        notes.sort_by_key(|note| (note.start_tick, note.end_tick, note.track_index));
        let mut run = None::<(i16, u64, usize)>;
        for note in notes {
            if let Some((pitch, end_tick, track_index)) = run
                && pitch == note.fine_pitch_offset_cents
                && note.start_tick <= end_tick
            {
                run = Some((
                    pitch,
                    end_tick.max(note.end_tick),
                    if note.end_tick >= end_tick {
                        note.track_index
                    } else {
                        track_index
                    },
                ));
                continue;
            }
            if let Some((pitch, end_tick, track_index)) = run.take()
                && pitch != 0
            {
                resets.push((track_index, channel as u8, end_tick));
            }
            run = Some((
                note.fine_pitch_offset_cents,
                note.end_tick,
                note.track_index,
            ));
        }
        if let Some((pitch, end_tick, track_index)) = run
            && pitch != 0
        {
            resets.push((track_index, channel as u8, end_tick));
        }
    }
    resets
}

fn pitch_bend_range_setup_messages(channel: u8) -> [[u8; 3]; 6] {
    let status = 0xB0 | channel;
    [
        [status, 101, 0],
        [status, 100, 0],
        [status, 6, 12],
        [status, 38, 0],
        [status, 101, 127],
        [status, 100, 127],
    ]
}

fn pitch_bend_bytes(channel: u8, offset_cents: i16) -> Vec<u8> {
    let bend = midi_pitch_bend_value(offset_cents);
    vec![
        0xE0 | channel,
        (bend & 0x7F) as u8,
        ((bend >> 7) & 0x7F) as u8,
    ]
}

fn midi_pitch_bend_value(offset_cents: i16) -> u16 {
    let cents = i64::from(offset_cents.clamp(-1200, 1200));
    let scale = if cents < 0 { 8192 } else { 8191 };
    let magnitude = (cents.abs() * scale + 600) / 1200;
    (8192 + if cents < 0 { -magnitude } else { magnitude }) as u16
}

impl MidiTrack {
    pub fn events(&self) -> &[MidiEvent] {
        &self.events
    }

    pub fn end_tick(&self) -> u64 {
        self.end_tick
    }

    pub fn name(&self) -> Option<String> {
        self.events.iter().find_map(|event| match &event.kind {
            MidiEventKind::Meta {
                meta_type: 0x03,
                data,
            } => Some(String::from_utf8_lossy(data).into_owned()),
            _ => None,
        })
    }

    pub fn tempo_bpm(&self) -> Option<f64> {
        self.tempo_events()
            .first()
            .copied()
            .map(MidiTempoEvent::bpm)
    }

    /// Returns every tempo event in file order, including repeated values.
    pub fn tempo_events(&self) -> Vec<MidiTempoEvent> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                MidiEventKind::Meta {
                    meta_type: 0x51,
                    data,
                } if data.len() == 3 => Some(MidiTempoEvent {
                    tick: event.absolute_tick,
                    microseconds_per_quarter: (u32::from(data[0]) << 16)
                        | (u32::from(data[1]) << 8)
                        | u32::from(data[2]),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn note_event_counts(&self) -> (usize, usize) {
        let mut note_on = 0usize;
        let mut note_off = 0usize;
        for event in &self.events {
            if let MidiEventKind::ChannelVoice { status, data } = &event.kind {
                if data.len() < 2 {
                    continue;
                }
                match status & 0xF0 {
                    0x90 if data[1] != 0 => note_on += 1,
                    0x80 | 0x90 => note_off += 1,
                    _ => {}
                }
            }
        }
        (note_on, note_off)
    }

    /// Returns valid MIDI Control Change messages in file order.
    pub fn control_changes(&self) -> Vec<MidiControlChange> {
        self.events
            .iter()
            .filter_map(|event| {
                let MidiEventKind::ChannelVoice { status, data } = &event.kind else {
                    return None;
                };
                if status & 0xF0 != 0xB0 || data.len() < 2 || data[0] > 127 || data[1] > 127 {
                    return None;
                }
                Some(MidiControlChange {
                    channel: status & 0x0F,
                    controller: data[0],
                    value: data[1],
                    tick: event.absolute_tick,
                })
            })
            .collect()
    }

    /// Pairs note-on and note-off messages in channel/key order. Notes without an
    /// ending message remain present with `end_tick == None`. The current MIDI
    /// CC 10 pan, CC 74 Mod X, CC 71 Mod Y, and pitch bend are snapshotted when
    /// each note starts. Missing CC values use MIDI center 64; a track without
    /// RPN 0 pitch sensitivity assumes a two-semitone range.
    pub fn notes(&self) -> Vec<MidiNote> {
        self.notes_with_options(MidiImportOptions::default())
    }

    /// Pairs note events using the supplied velocity-zero and channel-selection options.
    /// CC 64 sustain defers each note's end tick until pedal-up on the same channel.
    /// CC 66 sostenuto captures notes that are active when the pedal goes down and
    /// defers their ends until pedal-up. Both pedals can hold a note together, while
    /// retaining the release velocity from the original Note Off. CC 120 closes all
    /// active and sustained notes immediately; CC 123–127 release active notes and
    /// defer their end ticks while either pedal holds them.
    /// `realign_events` is applied by document import methods because aligning
    /// several tracks requires a shared project-time origin.
    pub fn notes_with_options(&self, options: MidiImportOptions) -> Vec<MidiNote> {
        let mut notes: Vec<MidiNote> = Vec::new();
        let mut active: BTreeMap<(u8, u8), VecDeque<usize>> = BTreeMap::new();
        let mut channel_state = BTreeMap::<u8, MidiChannelNoteState>::new();
        let mut deferred_note_offs = BTreeMap::<u8, Vec<usize>>::new();
        let mut sostenuto_captured_notes = BTreeMap::<u8, BTreeSet<usize>>::new();

        fn finish_unheld_notes(
            channel: u8,
            end_tick: u64,
            state: MidiChannelNoteState,
            notes: &mut [MidiNote],
            deferred_note_offs: &mut BTreeMap<u8, Vec<usize>>,
            sostenuto_captured_notes: &BTreeMap<u8, BTreeSet<usize>>,
        ) {
            let Some(deferred) = deferred_note_offs.remove(&channel) else {
                return;
            };
            let captured = sostenuto_captured_notes.get(&channel);
            let mut still_held = Vec::new();
            for note_index in deferred {
                let held_by_sostenuto = state.sostenuto_pedal_down
                    && captured.is_some_and(|captured| captured.contains(&note_index));
                if state.sustain_pedal_down || held_by_sostenuto {
                    still_held.push(note_index);
                } else {
                    notes[note_index].end_tick = Some(end_tick);
                }
            }
            if !still_held.is_empty() {
                deferred_note_offs.insert(channel, still_held);
            }
        }

        for event in &self.events {
            let MidiEventKind::ChannelVoice { status, data } = &event.kind else {
                continue;
            };
            if data.len() < 2 {
                continue;
            }

            let channel = status & 0x0F;
            if options.midi_channel_mask & (1u16 << u32::from(channel)) == 0 {
                continue;
            }
            let key = data[0];
            match status & 0xF0 {
                0xB0 => {
                    let state = channel_state.entry(channel).or_default();
                    match key {
                        10 => state.pan = data[1],
                        74 => state.mod_x = data[1].min(127),
                        71 => state.mod_y = data[1].min(127),
                        64 => {
                            let was_down = state.sustain_pedal_down;
                            state.sustain_pedal_down = data[1] >= 64;
                            let state = *state;
                            if was_down && !state.sustain_pedal_down {
                                finish_unheld_notes(
                                    channel,
                                    event.absolute_tick,
                                    state,
                                    &mut notes,
                                    &mut deferred_note_offs,
                                    &sostenuto_captured_notes,
                                );
                            }
                        }
                        66 => {
                            let was_down = state.sostenuto_pedal_down;
                            state.sostenuto_pedal_down = data[1] >= 64;
                            let is_down = state.sostenuto_pedal_down;
                            let state = *state;
                            if !was_down && is_down {
                                let captured = sostenuto_captured_notes.entry(channel).or_default();
                                for ((active_channel, _), queue) in &active {
                                    if *active_channel == channel {
                                        captured.extend(queue.iter().copied());
                                    }
                                }
                            } else if was_down && !is_down {
                                sostenuto_captured_notes.remove(&channel);
                                finish_unheld_notes(
                                    channel,
                                    event.absolute_tick,
                                    state,
                                    &mut notes,
                                    &mut deferred_note_offs,
                                    &sostenuto_captured_notes,
                                );
                            }
                        }
                        120 => {
                            let mut note_indices = Vec::new();
                            for ((active_channel, _), queue) in &mut active {
                                if *active_channel == channel {
                                    note_indices.extend(queue.drain(..));
                                }
                            }
                            active.retain(|(active_channel, _), _| *active_channel != channel);
                            sostenuto_captured_notes.remove(&channel);
                            if let Some(sustained) = deferred_note_offs.remove(&channel) {
                                note_indices.extend(sustained);
                            }
                            for note_index in note_indices {
                                notes[note_index].end_tick = Some(event.absolute_tick);
                            }
                        }
                        121 => {
                            let sensitivity_semitones = state.pitch_bend_sensitivity_semitones;
                            let sensitivity_cents = state.pitch_bend_sensitivity_cents;
                            let coarse_tuning = state.channel_coarse_tuning;
                            let fine_tuning = state.channel_fine_tuning;
                            *state = MidiChannelNoteState::default();
                            state.pitch_bend_sensitivity_semitones = sensitivity_semitones;
                            state.pitch_bend_sensitivity_cents = sensitivity_cents;
                            state.channel_coarse_tuning = coarse_tuning;
                            state.channel_fine_tuning = fine_tuning;
                            sostenuto_captured_notes.remove(&channel);
                            if let Some(note_indices) = deferred_note_offs.remove(&channel) {
                                for note_index in note_indices {
                                    notes[note_index].end_tick = Some(event.absolute_tick);
                                }
                            }
                        }
                        123..=127 => {
                            let mut note_indices = Vec::new();
                            for ((active_channel, _), queue) in &mut active {
                                if *active_channel == channel {
                                    note_indices.extend(queue.drain(..));
                                }
                            }
                            active.retain(|(active_channel, _), _| *active_channel != channel);
                            let state = *state;
                            let captured = sostenuto_captured_notes.get(&channel);
                            for note_index in note_indices {
                                if state.sustain_pedal_down
                                    || (state.sostenuto_pedal_down
                                        && captured.is_some_and(|set| set.contains(&note_index)))
                                {
                                    deferred_note_offs
                                        .entry(channel)
                                        .or_default()
                                        .push(note_index);
                                } else {
                                    notes[note_index].end_tick = Some(event.absolute_tick);
                                }
                            }
                        }
                        99 | 98 => state.rpn_selected = false,
                        101 => {
                            state.rpn_msb = data[1];
                            state.rpn_selected = true;
                        }
                        100 => {
                            state.rpn_lsb = data[1];
                            state.rpn_selected = true;
                        }
                        6 if state.rpn_selected && (state.rpn_msb, state.rpn_lsb) == (0, 0) => {
                            state.pitch_bend_sensitivity_semitones = data[1];
                        }
                        38 if state.rpn_selected && (state.rpn_msb, state.rpn_lsb) == (0, 0) => {
                            state.pitch_bend_sensitivity_cents = data[1];
                        }
                        6 if state.rpn_selected && (state.rpn_msb, state.rpn_lsb) == (0, 1) => {
                            state.channel_fine_tuning =
                                (u16::from(data[1]) << 7) | (state.channel_fine_tuning & 0x7f);
                        }
                        38 if state.rpn_selected && (state.rpn_msb, state.rpn_lsb) == (0, 1) => {
                            state.channel_fine_tuning =
                                (state.channel_fine_tuning & 0x3f80) | u16::from(data[1]);
                        }
                        6 if state.rpn_selected && (state.rpn_msb, state.rpn_lsb) == (0, 2) => {
                            state.channel_coarse_tuning = data[1];
                        }
                        _ => {}
                    }
                }
                0xE0 => {
                    let state = channel_state.entry(channel).or_default();
                    state.pitch_bend_value = u16::from(data[0]) | (u16::from(data[1]) << 7);
                }
                0x90 if data[1] != 0 || options.zero_velocity_note_ons_as_notes => {
                    let state = channel_state.get(&channel).copied().unwrap_or_default();
                    let note_index = notes.len();
                    notes.push(MidiNote {
                        channel,
                        key,
                        velocity: data[1],
                        pan: state.pan,
                        mod_x: state.mod_x,
                        mod_y: state.mod_y,
                        pitch_bend_offset_cents: midi_pitch_bend_unclamped_cents(
                            state.pitch_bend_value,
                            state.pitch_bend_sensitivity_semitones,
                            state.pitch_bend_sensitivity_cents,
                        ),
                        channel_coarse_tuning: state.channel_coarse_tuning as i8 - 64,
                        channel_fine_tuning_cents: (((i32::from(state.channel_fine_tuning) - 8192)
                            * 100) as f64
                            / 8192.0)
                            .round() as i16,
                        start_tick: event.absolute_tick,
                        end_tick: None,
                        release_velocity: None,
                    });
                    active
                        .entry((channel, key))
                        .or_default()
                        .push_back(note_index);
                }
                0x80 | 0x90 => {
                    let note_key = (channel, key);
                    let mut remove_key = false;
                    if let Some(queue) = active.get_mut(&note_key) {
                        if let Some(note_index) = queue.pop_front() {
                            notes[note_index].release_velocity = Some(data[1]);
                            let state = channel_state.get(&channel).copied().unwrap_or_default();
                            let held_by_sostenuto = state.sostenuto_pedal_down
                                && sostenuto_captured_notes
                                    .get(&channel)
                                    .is_some_and(|captured| captured.contains(&note_index));
                            if state.sustain_pedal_down || held_by_sostenuto {
                                deferred_note_offs
                                    .entry(channel)
                                    .or_default()
                                    .push(note_index);
                            } else {
                                notes[note_index].end_tick = Some(event.absolute_tick);
                            }
                        }
                        remove_key = queue.is_empty();
                    }
                    if remove_key {
                        active.remove(&note_key);
                    }
                }
                _ => {}
            }
        }
        notes
    }
}

/// Approximate a MIDI pitch offset using an FL key and 10-cent fine-pitch field.
pub fn midi_offset_to_fl_pitch(source_key: u8, total_cents: i32) -> (u16, u8) {
    let total_cents = total_cents.clamp(-25600, 25600);
    let key_shift = if total_cents > 1200 {
        (total_cents - 1200 + 99) / 100
    } else if total_cents < -1200 {
        -((-1200 - total_cents + 99) / 100)
    } else {
        0
    };
    let key = (i32::from(source_key) + key_shift).clamp(0, 127);
    let cents = total_cents - (key - i32::from(source_key)) * 100;
    (
        key as u16,
        midi_pitch_bend_cents_to_fl_note_fine_pitch(cents.clamp(-1200, 1200) as i16),
    )
}

impl MidiNote {
    pub fn channel(&self) -> u8 {
        self.channel
    }

    pub fn key(&self) -> u8 {
        self.key
    }

    /// FL note key and fine pitch approximating the channel tuning at note-on.
    pub fn tuned_fl_pitch(&self) -> (u16, u8) {
        let total_cents = i32::from(self.channel_coarse_tuning) * 100
            + i32::from(self.channel_fine_tuning_cents)
            + i32::from(self.pitch_bend_offset_cents);
        midi_offset_to_fl_pitch(self.key, total_cents)
    }

    pub fn velocity(&self) -> u8 {
        self.velocity
    }

    /// MIDI CC 10 pan value (0–127) active when the note started.
    pub fn pan(&self) -> u8 {
        self.pan
    }
    /// MIDI CC 74 value active when the note started.
    pub fn mod_x(&self) -> u8 {
        self.mod_x
    }

    /// MIDI CC 71 value active when the note started.
    pub fn mod_y(&self) -> u8 {
        self.mod_y
    }

    /// Pitch bend offset in cents active when the note started.
    pub fn pitch_bend_offset_cents(&self) -> i16 {
        self.pitch_bend_offset_cents.clamp(-1200, 1200)
    }

    pub fn start_tick(&self) -> u64 {
        self.start_tick
    }

    pub fn end_tick(&self) -> Option<u64> {
        self.end_tick
    }

    pub fn duration_ticks(&self) -> Option<u64> {
        self.end_tick.map(|end| end.saturating_sub(self.start_tick))
    }

    pub fn release_velocity(&self) -> Option<u8> {
        self.release_velocity
    }
}

/// Maps a MIDI release velocity (0–127) to FL Studio's note release range (0–128).
pub fn midi_release_velocity_to_fl_note_release(velocity: u8) -> u8 {
    let velocity = u16::from(velocity.min(127));
    ((velocity * 128 + 63) / 127) as u8
}

/// Maps FL Studio's note release range (0–128) to a MIDI Note Off velocity (0–127).
pub fn fl_note_release_to_midi_velocity(release: u8) -> u8 {
    let release = u16::from(release.min(128));
    ((release * 127 + 64) / 128) as u8
}

/// Maps FL Studio's 0–128 centered note pan to the 0–127 MIDI CC 10 range.
pub fn fl_note_pan_to_midi_value(pan: u8) -> u8 {
    ((u16::from(pan.min(128)) * 127 + 64) / 128) as u8
}

/// Maps MIDI CC 10 pan (0–127) to FL Studio's 0–128 note-pan range.
pub fn midi_pan_to_fl_note_pan(pan: u8) -> u8 {
    let pan = u16::from(pan.min(127));
    if pan <= 64 {
        pan as u8
    } else {
        ((pan * 128 + 63) / 127) as u8
    }
}

/// Converts a 14-bit MIDI pitch-bend value to cents using its RPN 0 sensitivity.
pub fn midi_pitch_bend_offset_cents(value: u16, semitones: u8, cents: u8) -> i16 {
    midi_pitch_bend_unclamped_cents(value, semitones, cents).clamp(-1200, 1200)
}

/// Calculate bend cents before limiting them to the FL fine-pitch range.
pub fn midi_pitch_bend_unclamped_cents(value: u16, semitones: u8, cents: u8) -> i16 {
    let bend = i32::from(value.min(0x3FFF)) - 8192;
    if bend == 0 {
        return 0;
    }
    let range_cents = i64::from(semitones) * 100 + i64::from(cents);
    let scale = if bend < 0 { 8192 } else { 8191 };
    let magnitude = (i64::from(bend.abs()) * range_cents + i64::from(scale) / 2) / i64::from(scale);
    let offset = if bend < 0 { -magnitude } else { magnitude };
    offset as i16
}

/// Maps a MIDI pitch-bend offset in cents to FL Studio's 10-cent note-fine-pitch steps.
pub fn midi_pitch_bend_cents_to_fl_note_fine_pitch(cents: i16) -> u8 {
    let cents = i32::from(cents).clamp(-1200, 1200);
    let steps = if cents < 0 {
        -((-cents + 5) / 10)
    } else {
        (cents + 5) / 10
    };
    (120 + steps) as u8
}

impl MidiTempoEvent {
    pub fn tick(self) -> u64 {
        self.tick
    }

    pub fn microseconds_per_quarter(self) -> u32 {
        self.microseconds_per_quarter
    }

    pub fn bpm(self) -> f64 {
        if self.microseconds_per_quarter == 0 {
            0.0
        } else {
            60_000_000.0 / f64::from(self.microseconds_per_quarter)
        }
    }
}

/// The native exporter emits tempo-control curves over FL's observed 60–180 BPM range.
/// This mapping is inferred from the installed FL Studio reference project and needs
/// more oracle coverage for other control ranges and non-linear point shapes.
fn tempo_automation_bpm(value: f64) -> Result<f64, MidiError> {
    let value = normalized_automation_point_value(value).unwrap_or(value);
    let bpm = 60.0 + value * 120.0;
    midi_tempo_microseconds_per_quarter(bpm)?;
    Ok(bpm)
}

fn midi_tempo_microseconds_per_quarter(bpm: f64) -> Result<u32, MidiError> {
    if !bpm.is_finite() || bpm <= 0.0 {
        return Err(MidiError::InvalidExport(
            "tempo must be positive and finite",
        ));
    }
    let micros_per_quarter = (60_000_000.0 / bpm).round();
    if !(1.0..=16_777_215.0).contains(&micros_per_quarter) {
        return Err(MidiError::InvalidExport(
            "tempo is outside the range representable by Standard MIDI Files",
        ));
    }
    Ok(micros_per_quarter as u32)
}

fn midi_time_signature_bytes(numerator: u8, denominator: u8) -> Result<[u8; 4], MidiError> {
    if numerator == 0 || denominator == 0 || !denominator.is_power_of_two() {
        return Err(MidiError::InvalidExport(
            "time signatures must have a positive numerator and power-of-two denominator",
        ));
    }
    Ok([numerator, denominator.trailing_zeros() as u8, 24, 8])
}

fn inferred_pattern_loop_length(
    inferred_length: u64,
    clip_length: u64,
    ppq: u16,
    time_signature: Option<(u8, u8)>,
) -> u64 {
    if inferred_length == 0 {
        return clip_length;
    }

    // The last note-off only bounds an automatic pattern's length; observed FL clips
    // repeat on the project measure grid instead of at that exact tick.
    let (numerator, denominator) = time_signature.unwrap_or((4, 4));
    if denominator == 0 {
        return inferred_length;
    }
    let measure_numerator = u64::from(ppq) * 4 * u64::from(numerator);
    let measure_ticks = measure_numerator.div_ceil(u64::from(denominator));
    if measure_ticks == 0 || clip_length < measure_ticks {
        return inferred_length;
    }

    inferred_length
        .div_ceil(measure_ticks)
        .checked_mul(measure_ticks)
        .unwrap_or(inferred_length)
}

pub(crate) fn song_tempo_events(
    document: &FlpDocument,
    clips: &[PlaylistClip],
) -> Result<Vec<(u64, f64)>, MidiError> {
    let automation_channels = document
        .automation_channels()
        .map_err(|error| MidiError::Project(error.to_string()))?;
    let ppq = document.header().ppq();
    let base_tempo = document.metadata().tempo_bpm().unwrap_or(140.0);
    let mut events = Vec::new();

    for clip in clips {
        if clip.is_muted() || clip.track_index.is_none() || clip.length_ticks == 0 {
            continue;
        }
        let PlaylistClipTarget::Channel { id } = clip.target() else {
            continue;
        };
        let Some(channel) = automation_channels.iter().find(|channel| {
            channel.channel_id() == id
                && channel
                    .display_name()
                    .is_some_and(|name| name.trim().eq_ignore_ascii_case("tempo"))
        }) else {
            continue;
        };
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            return Err(MidiError::InvalidExport(
                "scaled Playlist tempo automation Clips cannot be exported yet",
            ));
        }
        if channel.points().is_empty() {
            continue;
        }

        let (source_start_tick, source_end_tick, explicit_source_window) =
            match clip.source_window_ticks() {
                Some((0, 0)) | None => (0, u64::from(clip.length_ticks), false),
                Some((start, end)) if end.checked_sub(start) == Some(clip.length_ticks) => {
                    (u64::from(start), u64::from(end), true)
                }
                Some(_) => {
                    return Err(MidiError::InvalidExport(
                        "Playlist tempo automation source windows must match their clip length",
                    ));
                }
            };

        let mut source_points = channel
            .points()
            .iter()
            .map(|point| {
                Ok((
                    automation_point_tick(point.position_beats(), ppq)?,
                    point.value(),
                    point.tension(),
                ))
            })
            .collect::<Result<Vec<_>, MidiError>>()?;
        source_points.sort_by_key(|(tick, _, _)| *tick);
        source_points.dedup_by(|current, previous| {
            if current.0 == previous.0 {
                previous.1 = current.1;
                previous.2 = current.2;
                true
            } else {
                false
            }
        });
        let mut points = Vec::new();
        if source_start_tick > 0
            && !source_points
                .iter()
                .any(|(tick, _, _)| *tick == source_start_tick)
            && let Some((from_tick, from_value, _)) = source_points
                .iter()
                .rev()
                .find(|(tick, _, _)| *tick < source_start_tick)
        {
            let next = source_points
                .iter()
                .find(|(tick, _, _)| *tick > source_start_tick);
            let boundary_value = next.map_or(*from_value, |(to_tick, to_value, to_tension)| {
                let fraction =
                    (source_start_tick - from_tick) as f64 / (*to_tick - from_tick) as f64;
                automation_segment_value_with_tension(*from_value, *to_value, fraction, *to_tension)
            });
            points.push((0, boundary_value, 0.0));
        }
        if explicit_source_window {
            points.extend(
                source_points
                    .iter()
                    .copied()
                    .filter(|(tick, _, _)| *tick >= source_start_tick && *tick < source_end_tick)
                    .map(|(tick, value, tension)| (tick - source_start_tick, value, tension)),
            );
            if let Some((value, tension)) =
                automation_value_at_tick(&source_points, source_end_tick)
            {
                points.push((source_end_tick - source_start_tick, value, tension));
            }
        } else {
            // Legacy clips have no decoded source window. Keep the original
            // point sequence, including the first point beyond the visible
            // end, so the final in-clip tempo ramp can be sampled faithfully.
            points.extend(source_points.iter().copied());
        }
        if points.is_empty() {
            continue;
        }

        let clip_start = u64::from(clip.position_ticks);
        let clip_end = clip_start
            .checked_add(u64::from(clip.length_ticks))
            .ok_or(MidiError::LengthOverflow)?;
        let first_point_tick = clip_start
            .checked_add(points[0].0)
            .ok_or(MidiError::LengthOverflow)?;
        if first_point_tick < clip_end {
            events.push((first_point_tick, tempo_automation_bpm(points[0].1)?));
        }

        let mut clip_event_count = 0usize;
        for pair in points.windows(2) {
            let from = &pair[0];
            let to = &pair[1];
            if from.1 == to.1 {
                continue;
            }

            let segment_start = clip_start
                .checked_add(from.0)
                .ok_or(MidiError::LengthOverflow)?;
            let segment_end = clip_start
                .checked_add(to.0)
                .ok_or(MidiError::LengthOverflow)?;
            if segment_start >= clip_end || segment_end <= segment_start {
                continue;
            }
            let sample_end = segment_end.min(clip_end);
            let span = (segment_end - segment_start) as f64;
            let first_sample_tick = segment_start.saturating_sub(1);
            for tick in first_sample_tick..sample_end {
                let elapsed = tick.saturating_add(1).saturating_sub(segment_start) as f64;
                let fraction = (elapsed / span).clamp(0.0, 1.0);
                let value = automation_segment_value_with_tension(from.1, to.1, fraction, to.2);
                events.push((tick, tempo_automation_bpm(value)?));
                clip_event_count = clip_event_count.saturating_add(1);
                if clip_event_count > MAX_EXPORTED_TEMPO_EVENTS {
                    return Err(MidiError::InvalidExport(
                        "tempo automation expands to more than one million MIDI events",
                    ));
                }
            }
        }

        let reset_tick = clip_end.checked_add(1).ok_or(MidiError::LengthOverflow)?;
        events.push((reset_tick, base_tempo));
    }

    events.sort_by_key(|(tick, _)| *tick);
    events.dedup_by(|current, previous| {
        current.0 == previous.0 && (current.1 - previous.1).abs() <= 1e-9
    });
    if events.len() > MAX_EXPORTED_TEMPO_EVENTS {
        return Err(MidiError::InvalidExport(
            "tempo automation expands to more than one million MIDI events",
        ));
    }
    Ok(events)
}

fn automation_point_tick(position_beats: f64, ppq: u16) -> Result<u64, MidiError> {
    let ticks = position_beats * f64::from(ppq);
    if !ticks.is_finite() || ticks < 0.0 || ticks >= u64::MAX as f64 {
        return Err(MidiError::InvalidExport(
            "tempo automation positions must be finite, non-negative, and representable",
        ));
    }
    Ok(ticks.round() as u64)
}

fn automation_value_at_tick(points: &[(u64, f64, f32)], tick: u64) -> Option<(f64, f32)> {
    let previous = points
        .iter()
        .rev()
        .find(|(point_tick, _, _)| *point_tick <= tick)?;
    if previous.0 == tick {
        return Some((previous.1, previous.2));
    }
    points
        .iter()
        .find(|(point_tick, _, _)| *point_tick > tick)
        .map_or(
            Some((previous.1, previous.2)),
            |(next_tick, next_value, next_tension)| {
                let fraction = (tick - previous.0) as f64 / (*next_tick - previous.0) as f64;
                Some((
                    automation_segment_value_with_tension(
                        previous.1,
                        *next_value,
                        fraction,
                        *next_tension,
                    ),
                    *next_tension,
                ))
            },
        )
}

impl MidiEvent {
    pub fn delta_ticks(&self) -> u32 {
        self.delta_ticks
    }

    pub fn absolute_tick(&self) -> u64 {
        self.absolute_tick
    }

    pub fn kind(&self) -> &MidiEventKind {
        &self.kind
    }

    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiError {
    UnexpectedEof {
        offset: usize,
        context: &'static str,
    },
    InvalidTag {
        offset: usize,
        expected: &'static str,
    },
    InvalidHeaderLength(u32),
    InvalidTimeDivision(u16),
    InvalidTrackIndex(usize),
    InvalidVlq {
        offset: usize,
    },
    InvalidEvent {
        offset: usize,
        detail: &'static str,
    },
    Project(String),
    InvalidExport(&'static str),
    LengthOverflow,
}

impl fmt::Display for MidiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { offset, context } => {
                write!(
                    formatter,
                    "unexpected end of MIDI file at byte {offset} while reading {context}"
                )
            }
            Self::InvalidTag { offset, expected } => {
                write!(
                    formatter,
                    "invalid MIDI chunk at byte {offset}; expected {expected}"
                )
            }
            Self::InvalidHeaderLength(length) => {
                write!(
                    formatter,
                    "invalid MThd length {length}; expected at least 6"
                )
            }
            Self::InvalidTimeDivision(division) => {
                write!(
                    formatter,
                    "unsupported or invalid MIDI time division 0x{division:04X}"
                )
            }
            Self::InvalidTrackIndex(index) => {
                write!(formatter, "MIDI track index {index} does not exist")
            }
            Self::InvalidVlq { offset } => {
                write!(
                    formatter,
                    "invalid or overlong MIDI variable-length quantity at byte {offset}"
                )
            }
            Self::InvalidEvent { offset, detail } => {
                write!(formatter, "invalid MIDI event at byte {offset}: {detail}")
            }
            Self::Project(error) => {
                write!(formatter, "could not read FL Studio project data: {error}")
            }
            Self::InvalidExport(detail) => write!(formatter, "could not export MIDI: {detail}"),
            Self::LengthOverflow => write!(formatter, "MIDI length exceeds supported size"),
        }
    }
}

impl std::error::Error for MidiError {}

fn first_pattern_note_repetition_for_source_window(
    note: &PatternNote,
    source_start_tick: u64,
    loop_length: u64,
    play_truncated_notes: bool,
) -> u64 {
    let note_start = u64::from(note.position);
    if note.length == 0 || !play_truncated_notes {
        if source_start_tick <= note_start {
            0
        } else {
            (source_start_tick - note_start).div_ceil(loop_length)
        }
    } else {
        let note_end = note_start + u64::from(note.length);
        if note_end > source_start_tick {
            0
        } else {
            (source_start_tick - note_end) / loop_length + 1
        }
    }
}

fn tracks_for_notes(
    document: &FlpDocument,
    notes: &[PatternNote],
) -> Result<Vec<MidiExportTrack>, MidiError> {
    if notes.len() > MAX_EXPORTED_NOTES {
        return Err(MidiError::InvalidExport(
            "the pattern contains more than one million MIDI notes",
        ));
    }
    let midi_out_settings = channel_midi_out_settings(document);
    let mut grouped = BTreeMap::<u16, Vec<MidiExportNote>>::new();
    for note in notes {
        if note.is_muted() {
            continue;
        }
        let track_notes = grouped.entry(note.channel_id).or_default();
        if track_notes.len() >= MAX_EXPORTED_NOTES {
            return Err(MidiError::InvalidExport(
                "the pattern expands to more than one million MIDI notes",
            ));
        }
        let end_tick = u64::from(note.position) + u64::from(note.length);
        track_notes.push(export_note(
            note,
            u64::from(note.position),
            end_tick,
            midi_out_settings.get(&note.channel_id).copied(),
        ));
    }
    tracks_for_grouped_notes(document, grouped)
}

fn channel_midi_out_settings(document: &FlpDocument) -> BTreeMap<u16, ChannelMidiOutSettings> {
    document
        .channels()
        .into_iter()
        .filter_map(|channel| {
            channel
                .midi_out_settings()
                .copied()
                .map(|settings| (channel.id(), settings))
        })
        .collect()
}

fn tracks_for_grouped_notes(
    document: &FlpDocument,
    grouped: BTreeMap<u16, Vec<MidiExportNote>>,
) -> Result<Vec<MidiExportTrack>, MidiError> {
    let channels: BTreeMap<_, _> = document
        .channels()
        .into_iter()
        .map(|channel| (channel.id(), channel.display_name().map(str::to_owned)))
        .collect();
    Ok(grouped
        .into_iter()
        .map(|(channel_id, notes)| {
            let name = channels
                .get(&channel_id)
                .and_then(Option::as_deref)
                .filter(|name| !name.is_empty())
                .map_or_else(|| format!("Channel {channel_id}"), str::to_owned);
            let mut track = MidiExportTrack::new(name, Some(channel_id));
            track.end_tick = notes.iter().map(|note| note.end_tick).max().unwrap_or(0);
            track.notes = notes;
            track
        })
        .collect())
}

fn export_note(
    note: &PatternNote,
    start_tick: u64,
    end_tick: u64,
    midi_out_settings: Option<ChannelMidiOutSettings>,
) -> MidiExportNote {
    MidiExportNote {
        channel: midi_out_settings
            .filter(|settings| !settings.use_note_color_channel())
            .map_or_else(
                || note.midi_channel_index(),
                |settings| settings.midi_channel() - 1,
            ),
        key: u8::try_from(note.key).unwrap_or(u8::MAX),
        velocity: note.velocity,
        release_velocity: fl_note_release_to_midi_velocity(note.release),
        pan: fl_note_pan_to_midi_value(note.pan),
        mod_x: fl_note_modulation_to_midi_value(note.mod_x),
        mod_y: fl_note_modulation_to_midi_value(note.mod_y),
        start_tick,
        end_tick,
        fine_pitch_offset_cents: note.fine_pitch_offset_cents(),
    }
}

/// Maps FL Studio's 0–255 note modulation range to a MIDI controller's 0–127 range.
pub fn fl_note_modulation_to_midi_value(value: u8) -> u8 {
    ((u16::from(value) * 127 + 127) / 255) as u8
}

/// Maps MIDI CC 71/74 values to FL Studio's 0–255 note Mod X/Y range.
pub fn midi_modulation_to_fl_note_value(value: u8) -> u8 {
    let value = value.min(127);
    if value <= 64 {
        ((u16::from(value) * 128 + 32) / 64) as u8
    } else {
        (128 + (u16::from(value - 64) * 127 + 31) / 63) as u8
    }
}

fn meta_event_bytes(meta_type: u8, data: &[u8]) -> Result<Vec<u8>, MidiError> {
    let length = u32::try_from(data.len()).map_err(|_| MidiError::LengthOverflow)?;
    if u64::from(length) > MAX_VLQ {
        return Err(MidiError::LengthOverflow);
    }
    let mut bytes = Vec::with_capacity(data.len().saturating_add(6));
    bytes.push(0xFF);
    bytes.push(meta_type);
    write_vlq(&mut bytes, length);
    bytes.extend_from_slice(data);
    Ok(bytes)
}

fn write_end_of_track(output: &mut Vec<u8>, delta_ticks: u64) -> Result<(), MidiError> {
    write_delta(output, delta_ticks)?;
    output.extend_from_slice(&[0xFF, 0x2F, 0x00]);
    Ok(())
}

fn write_delta(output: &mut Vec<u8>, mut delta_ticks: u64) -> Result<(), MidiError> {
    while delta_ticks > MAX_VLQ {
        write_vlq(output, MAX_VLQ as u32);
        // A sequencer-specific empty meta event advances the track when a delta exceeds
        // the four-byte VLQ limit. It carries no musical event or project data.
        output.extend_from_slice(&[0xFF, 0x7F, 0x00]);
        delta_ticks -= MAX_VLQ;
    }
    write_vlq(output, delta_ticks as u32);
    Ok(())
}

fn write_vlq(output: &mut Vec<u8>, value: u32) {
    let mut buffer = [0u8; 4];
    let mut cursor = buffer.len() - 1;
    buffer[cursor] = (value & 0x7F) as u8;
    let mut value = value >> 7;
    while value > 0 {
        cursor -= 1;
        buffer[cursor] = ((value & 0x7F) as u8) | 0x80;
        value >>= 7;
    }
    output.extend_from_slice(&buffer[cursor..]);
}

fn append_track_chunk(output: &mut Vec<u8>, track_data: Vec<u8>) -> Result<(), MidiError> {
    let length = u32::try_from(track_data.len()).map_err(|_| MidiError::LengthOverflow)?;
    output.extend_from_slice(MTRK);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(&track_data);
    Ok(())
}

fn parse_track(bytes: &[u8], absolute_start: usize) -> Result<MidiTrack, MidiError> {
    let mut cursor = 0usize;
    let mut absolute_tick = 0u64;
    let mut running_status = None;
    let mut events = Vec::new();

    while cursor < bytes.len() {
        let event_start = cursor;
        let (delta_ticks, after_delta) = read_vlq(bytes, cursor, absolute_start)?;
        cursor = after_delta;
        absolute_tick = absolute_tick
            .checked_add(u64::from(delta_ticks))
            .ok_or(MidiError::LengthOverflow)?;

        let Some(&first_byte) = bytes.get(cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "MIDI event status",
            });
        };

        let (status, first_data) = if first_byte & 0x80 != 0 {
            cursor += 1;
            (first_byte, None)
        } else {
            let Some(status) = running_status else {
                return Err(MidiError::InvalidEvent {
                    offset: absolute_start + cursor,
                    detail: "data byte encountered without running status",
                });
            };
            (status, Some(first_byte))
        };

        let kind = match status {
            0x80..=0xEF => {
                running_status = Some(status);
                let data_length = channel_data_length(status);
                let data =
                    read_data_bytes(bytes, &mut cursor, data_length, first_data, absolute_start)?;
                MidiEventKind::ChannelVoice { status, data }
            }
            0xF0 | 0xF7 => {
                running_status = None;
                let (length, after_length) = read_vlq(bytes, cursor, absolute_start)?;
                cursor = after_length;
                let data =
                    read_payload(bytes, &mut cursor, length, absolute_start, "SysEx payload")?;
                MidiEventKind::SysEx { status, data }
            }
            0xFF => {
                let Some(&meta_type) = bytes.get(cursor) else {
                    return Err(MidiError::UnexpectedEof {
                        offset: absolute_start + cursor,
                        context: "MIDI meta type",
                    });
                };
                cursor += 1;
                let (length, after_length) = read_vlq(bytes, cursor, absolute_start)?;
                cursor = after_length;
                let data = read_payload(
                    bytes,
                    &mut cursor,
                    length,
                    absolute_start,
                    "MIDI meta payload",
                )?;
                MidiEventKind::Meta { meta_type, data }
            }
            0xF1 | 0xF2 | 0xF3 | 0xF6 | 0xF8..=0xFE => {
                if status < 0xF8 {
                    running_status = None;
                }
                let data_length = system_data_length(status);
                let data =
                    read_data_bytes(bytes, &mut cursor, data_length, first_data, absolute_start)?;
                MidiEventKind::System { status, data }
            }
            _ => {
                return Err(MidiError::InvalidEvent {
                    offset: absolute_start + event_start,
                    detail: "unsupported MIDI system status",
                });
            }
        };

        events.push(MidiEvent {
            delta_ticks,
            absolute_tick,
            kind,
            wire_bytes: bytes[event_start..cursor].to_vec(),
        });
    }

    Ok(MidiTrack {
        events,
        end_tick: absolute_tick,
    })
}

fn channel_data_length(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

fn system_data_length(status: u8) -> usize {
    match status {
        0xF1 | 0xF3 => 1,
        0xF2 => 2,
        _ => 0,
    }
}

fn read_data_bytes(
    bytes: &[u8],
    cursor: &mut usize,
    length: usize,
    first_data: Option<u8>,
    absolute_start: usize,
) -> Result<Vec<u8>, MidiError> {
    let mut data = Vec::with_capacity(length);
    if let Some(first) = first_data {
        if length == 0 {
            return Err(MidiError::InvalidEvent {
                offset: absolute_start + *cursor,
                detail: "running status supplied data for a zero-length event",
            });
        }
        data.push(first);
        *cursor += 1;
    }
    while data.len() < length {
        let Some(&byte) = bytes.get(*cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + *cursor,
                context: "MIDI event data",
            });
        };
        if byte & 0x80 != 0 {
            return Err(MidiError::InvalidEvent {
                offset: absolute_start + *cursor,
                detail: "status byte encountered where event data was expected",
            });
        }
        data.push(byte);
        *cursor += 1;
    }
    Ok(data)
}

fn read_payload(
    bytes: &[u8],
    cursor: &mut usize,
    length: u32,
    absolute_start: usize,
    context: &'static str,
) -> Result<Vec<u8>, MidiError> {
    let length = usize::try_from(length).map_err(|_| MidiError::LengthOverflow)?;
    let end = cursor
        .checked_add(length)
        .ok_or(MidiError::LengthOverflow)?;
    let Some(payload) = bytes.get(*cursor..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: absolute_start + bytes.len(),
            context,
        });
    };
    *cursor = end;
    Ok(payload.to_vec())
}

fn read_vlq(bytes: &[u8], start: usize, absolute_start: usize) -> Result<(u32, usize), MidiError> {
    let mut value = 0u32;
    let mut cursor = start;
    for _ in 0..4 {
        let Some(&byte) = bytes.get(cursor) else {
            return Err(MidiError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "MIDI variable-length quantity",
            });
        };
        cursor += 1;
        value = (value << 7) | u32::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }
    Err(MidiError::InvalidVlq {
        offset: absolute_start + start,
    })
}

fn require_tag(bytes: &[u8], offset: usize, expected: &'static [u8; 4]) -> Result<(), MidiError> {
    let end = offset.checked_add(4).ok_or(MidiError::LengthOverflow)?;
    let Some(tag) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context: "MIDI chunk tag",
        });
    };
    if tag != expected {
        return Err(MidiError::InvalidTag {
            offset,
            expected: if expected == MTHD { "MThd" } else { "MTrk" },
        });
    }
    Ok(())
}

fn read_u16_be(bytes: &[u8], offset: usize, context: &'static str) -> Result<u16, MidiError> {
    let end = offset.checked_add(2).ok_or(MidiError::LengthOverflow)?;
    let Some(value) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32_be(bytes: &[u8], offset: usize, context: &'static str) -> Result<u32, MidiError> {
    let end = offset.checked_add(4).ok_or(MidiError::LengthOverflow)?;
    let Some(value) = bytes.get(offset..end) else {
        return Err(MidiError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

#[cfg(test)]
mod tests {
    use super::{
        MidiChannelMapping, MidiError, MidiExportNote, MidiExportTrack, MidiFile, MidiImportOptions,
    };

    fn midi_fixture(track: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"MThd");
        bytes.extend_from_slice(&6u32.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&480u16.to_be_bytes());
        bytes.extend_from_slice(b"MTrk");
        bytes.extend_from_slice(&(track.len() as u32).to_be_bytes());
        bytes.extend_from_slice(track);
        bytes
    }

    #[test]
    fn pairs_notes_across_running_status_and_preserves_original_bytes() {
        let track = [
            0x00, 0x90, 0x3C, 0x64, // Note on at tick 0.
            0x81, 0x70, 0x3C, 0x00, // Running-status note off at tick 240.
            0x00, 0xFF, 0x2F, 0x00, // End of track.
        ];
        let input = midi_fixture(&track);

        let midi = MidiFile::parse(&input).expect("fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(midi.division(), 480);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key(), 60);
        assert_eq!(notes[0].velocity(), 100);
        assert_eq!(notes[0].pan(), 64);
        assert_eq!(notes[0].pitch_bend_offset_cents(), 0);
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(240));
        assert_eq!(notes[0].duration_ticks(), Some(240));
        assert_eq!(midi.encode_lossless(), input);
    }

    #[test]
    fn midi_import_defers_note_off_until_sustain_pedal_up_per_channel() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0x00, 0x90, 0x3C, 0x64, // Channel 1 note on at tick 0.
            0x00, 0x91, 0x3C, 0x64, // Channel 2 note on at tick 0.
            0x0A, 0xB0, 64, 127, // Channel 1 sustain on at tick 10.
            0x0A, 0x80, 0x3C, 33, // Channel 1 note off at tick 20.
            0x00, 0x81, 0x3C, 44, // Channel 2 note off at tick 20.
            0x0A, 0xB0, 64, 63, // Channel 1 sustain off at tick 30.
            0x00, 0xFF, 0x2F, 0x00,
        ]))
        .expect("sustain fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].channel(), 0);
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(30));
        assert_eq!(notes[0].release_velocity(), Some(33));
        assert_eq!(notes[1].channel(), 1);
        assert_eq!(notes[1].start_tick(), 0);
        assert_eq!(notes[1].end_tick(), Some(20));
        assert_eq!(notes[1].release_velocity(), Some(44));
    }

    #[test]
    fn midi_import_sostenuto_holds_only_notes_already_active_when_pressed() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0x00, 0x90, 0x3C, 0x64, // C4 starts at tick 0.
            0x0A, 0xB0, 66, 127, // Sostenuto down at tick 10; captures C4.
            0x0A, 0x90, 0x3E, 0x5A, // D4 starts at tick 20; not captured.
            0x0A, 0x80, 0x3C, 31, // C4 released at tick 30, but remains held.
            0x0A, 0x80, 0x3E, 41, // D4 released at tick 40 and ends immediately.
            0x0A, 0xB0, 66, 63, // Sostenuto up at tick 50.
            0x00, 0xFF, 0x2F, 0x00,
        ]))
        .expect("sostenuto fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].key(), 60);
        assert_eq!(notes[0].end_tick(), Some(50));
        assert_eq!(notes[0].release_velocity(), Some(31));
        assert_eq!(notes[1].key(), 62);
        assert_eq!(notes[1].end_tick(), Some(40));
        assert_eq!(notes[1].release_velocity(), Some(41));
    }

    #[test]
    fn midi_import_sustain_and_sostenuto_hold_notes_until_both_are_released() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0x00, 0x90, 0x3C, 0x64, // C4 starts at tick 0.
            0x05, 0xB0, 66, 127, // Sostenuto down at tick 5.
            0x05, 0xB0, 64, 127, // Sustain down at tick 10.
            0x05, 0xB0, 123, 0, // All Notes Off at tick 15; both pedals hold C4.
            0x05, 0xB0, 66, 0, // Sostenuto up at tick 20; sustain still holds C4.
            0x0A, 0xB0, 64, 0, // Sustain up at tick 30; C4 ends here.
            0x00, 0xFF, 0x2F, 0x00,
        ]))
        .expect("combined pedal fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key(), 60);
        assert_eq!(notes[0].end_tick(), Some(30));
        assert_eq!(notes[0].release_velocity(), None);
    }

    #[test]
    fn midi_import_reset_all_controllers_releases_sustain_and_resets_note_state() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0x00, 0xB0, 64, 127, // Sustain on at tick 0.
            0x00, 0x90, 0x3C, 0x64, // C4 on at tick 0.
            0x0A, 0x80, 0x3C, 33, // C4 off at tick 10, held by sustain.
            0x00, 0xB0, 10, 127, // Set pan right before reset.
            0x00, 0xB0, 74, 127, // Set Mod X high before reset.
            0x00, 0xB0, 71, 0, // Set Mod Y low before reset.
            0x00, 0xE0, 0, 0, // Set pitch bend down before reset.
            0x0A, 0xB0, 121, 0, // Reset all controllers at tick 20.
            0x00, 0x90, 0x3E, 0x64, // D4 on at tick 20.
            0x0A, 0x80, 0x3E, 44, // D4 off at tick 30.
            0x00, 0xFF, 0x2F, 0x00,
        ]))
        .expect("controller reset fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].end_tick(), Some(20));
        assert_eq!(notes[0].release_velocity(), Some(33));
        assert_eq!(notes[1].end_tick(), Some(30));
        assert_eq!(notes[1].release_velocity(), Some(44));
        assert_eq!(notes[1].pan(), 64);
        assert_eq!(notes[1].mod_x(), 64);
        assert_eq!(notes[1].mod_y(), 64);
        assert_eq!(notes[1].pitch_bend_offset_cents(), 0);
    }

    #[test]
    fn midi_import_reset_retains_rpn_values_and_clears_selection() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0, 0xB0, 101, 0, 0, 0xB0, 100, 0, 0, 0xB0, 6, 12, 0, 0xB0, 100, 2, 0, 0xB0, 6,
            66, // +2 semitones channel tuning.
            0, 0xB0, 121, 0, 0, 0xB0, 6, 80, // Reset clears selection, not tuning.
            0, 0xE0, 127, 127, 0, 0x90, 60, 100, 10, 0x80, 60, 64, 0, 0xFF, 0x2F, 0,
        ]))
        .unwrap();
        let notes = midi.tracks()[0].notes();
        assert_eq!(notes[0].pitch_bend_offset_cents(), 1200);
        assert_eq!(notes[0].tuned_fl_pitch(), (62, 240));
    }

    #[test]
    fn midi_import_combines_wide_bend_and_tuning_before_clamping() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0, 0xB0, 101, 0, 0, 0xB0, 100, 0, 0, 0xB0, 6, 24, // Bend range +/−24 semitones.
            0, 0xB0, 100, 2, 0, 0xB0, 6, 52, // Coarse tuning −12 semitones.
            0, 0xE0, 127, 127, 0, 0x90, 60, 100, 10, 0x80, 60, 64, 0, 0xFF, 0x2F, 0,
        ]))
        .unwrap();
        let note = &midi.tracks()[0].notes()[0];
        assert_eq!(note.pitch_bend_offset_cents(), 1200); // Existing API remains bounded.
        assert_eq!(note.tuned_fl_pitch(), (60, 240));
    }

    #[test]
    fn midi_import_snapshots_channel_tuning_and_bend_per_note() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0, 0xB0, 101, 0, 0, 0xB0, 100, 2, 0, 0xB0, 6, 66, // +2 semitones.
            0, 0xB0, 100, 1, 0, 0xB0, 6, 80, 0, 0xB0, 38, 0, // +25 cents.
            0, 0xE0, 0, 96, // +100 cents with default bend range.
            0, 0x90, 60, 100, 0, 0x91, 60, 100, 10, 0x80, 60, 64, 0, 0x81, 60, 64, 0, 0xB0, 121, 0,
            0, 0x90, 60, 100, 10, 0x80, 60, 64, 0, 0xFF, 0x2F, 0,
        ]))
        .expect("fixture should parse");
        let notes = midi.tracks()[0].notes();
        assert_eq!(notes[0].tuned_fl_pitch(), (60, 153));
        assert_eq!(notes[1].tuned_fl_pitch(), (60, 120));
        assert_eq!(notes[2].tuned_fl_pitch(), (60, 143));
        let mut extreme = notes[1].clone();
        extreme.channel_coarse_tuning = 20;
        assert_eq!(extreme.tuned_fl_pitch(), (68, 240));
        extreme.key = 127;
        assert_eq!(extreme.tuned_fl_pitch(), (127, 240));
        extreme.key = 0;
        extreme.channel_coarse_tuning = -20;
        assert_eq!(extreme.tuned_fl_pitch(), (0, 0));
    }

    #[test]
    fn midi_import_nrpn_data_entry_does_not_change_pitch_bend_range() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0, 0xB0, 101, 0, 0, 0xB0, 100, 0, // Select pitch-bend sensitivity.
            0, 0xB0, 6, 12, // Set twelve semitones.
            0, 0xB0, 99, 1, 0, 0xB0, 98, 2, // Select an NRPN.
            0, 0xB0, 6, 3, 0, 0xB0, 38, 50, // Data belongs to the NRPN.
            0, 0xE0, 0x7F, 0x7F, 0, 0x90, 60, 100, 10, 0x80, 60, 64, 0, 0xB0, 101, 0, 0, 0xB0, 100,
            0, // Reselect the RPN.
            0, 0xB0, 6, 2, 0, 0x90, 62, 100, 10, 0x80, 62, 64, 0, 0xFF, 0x2F, 0,
        ]))
        .expect("fixture should parse");
        let notes = midi.tracks()[0].notes();
        assert_eq!(notes[0].pitch_bend_offset_cents(), 1200);
        assert_eq!(notes[1].pitch_bend_offset_cents(), 200);
    }

    #[test]
    fn midi_import_all_sound_off_closes_active_and_sustained_notes_per_channel() {
        let midi = MidiFile::parse(&midi_fixture(&[
            0x00, 0xB0, 64, 127, // Channel 1 sustain on at tick 0.
            0x00, 0x90, 0x3C, 0x64, // Channel 1 C4 on at tick 0.
            0x00, 0x91, 0x3C, 0x64, // Channel 2 C4 on at tick 0.
            0x0A, 0x80, 0x3C, 33, // Channel 1 C4 off at tick 10, held by sustain.
            0x00, 0x90, 0x3E, 0x5A, // Channel 1 D4 on at tick 10.
            0x0A, 0xB0, 120, 0, // All Sound Off on channel 1 at tick 20.
            0x0A, 0x81, 0x3C, 44, // Channel 2 C4 remains active until tick 30.
            0x00, 0x90, 0x40, 0x50, // Channel 1 E4 on at tick 30.
            0x0A, 0x80, 0x40, 55, // Channel 1 E4 off at tick 40, held by sustain.
            0x0A, 0xB0, 64, 0, // Channel 1 sustain off at tick 50.
            0x00, 0xFF, 0x2F, 0x00,
        ]))
        .expect("All Sound Off fixture should parse");
        let notes = midi.tracks()[0].notes();

        assert_eq!(notes.len(), 4);
        assert_eq!(notes[0].channel(), 0);
        assert_eq!(notes[0].end_tick(), Some(20));
        assert_eq!(notes[0].release_velocity(), Some(33));
        assert_eq!(notes[1].channel(), 1);
        assert_eq!(notes[1].end_tick(), Some(30));
        assert_eq!(notes[1].release_velocity(), Some(44));
        assert_eq!(notes[2].end_tick(), Some(20));
        assert_eq!(notes[2].release_velocity(), None);
        assert_eq!(notes[3].end_tick(), Some(50));
        assert_eq!(notes[3].release_velocity(), Some(55));
    }

    #[test]
    fn midi_import_all_notes_off_controllers_respect_sustain() {
        for (controller, sustain_down) in (123..=127).zip([false, true, false, true, false]) {
            let mut events = vec![0x00, 0x90, 0x3C, 0x64, 0x00, 0x91, 0x3C, 0x64];
            if sustain_down {
                events.extend([0x05, 0xB0, 64, 127]);
            }
            events.extend([if sustain_down { 0x05 } else { 0x0A }, 0xB0, controller, 0]);
            if sustain_down {
                events.extend([0x0A, 0xB0, 64, 0]);
            }
            events.extend([
                if sustain_down { 0x0A } else { 0x14 },
                0x81,
                0x3C,
                44,
                0x00,
                0xFF,
                0x2F,
                0x00,
            ]);

            let midi = MidiFile::parse(&midi_fixture(&events))
                .expect("All Notes Off controller fixture should parse");
            let notes = midi.tracks()[0].notes();
            let expected_end = if sustain_down { 20 } else { 10 };

            assert_eq!(notes.len(), 2, "controller {controller}");
            assert_eq!(notes[0].channel(), 0, "controller {controller}");
            assert_eq!(
                notes[0].end_tick(),
                Some(expected_end),
                "controller {controller}"
            );
            assert_eq!(notes[0].release_velocity(), None, "controller {controller}");
            assert_eq!(notes[1].channel(), 1, "controller {controller}");
            assert_eq!(notes[1].end_tick(), Some(30), "controller {controller}");
            assert_eq!(
                notes[1].release_velocity(),
                Some(44),
                "controller {controller}"
            );
        }
    }

    #[test]
    fn zero_velocity_note_on_is_an_off_by_default_and_an_on_when_requested() {
        let input = midi_fixture(&[
            0x00, 0x90, 0x3C, 0x00, // Velocity-zero Note On at tick 0.
            0x60, 0x80, 0x3C, 0x21, // Note Off at tick 96.
            0x00, 0xFF, 0x2F, 0x00,
        ]);
        let midi = MidiFile::parse(&input).expect("fixture should parse");
        let track = &midi.tracks()[0];

        assert!(track.notes().is_empty());

        let notes = track.notes_with_options(MidiImportOptions {
            zero_velocity_note_ons_as_notes: true,
            ..MidiImportOptions::default()
        });
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].key(), 60);
        assert_eq!(notes[0].velocity(), 0);
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(96));
        assert_eq!(notes[0].release_velocity(), Some(33));
    }

    #[test]
    fn midi_import_channel_mask_filters_note_channels() {
        let input = midi_fixture(&[
            0x00, 0x90, 0x3C, 0x64, // Channel 1, C4.
            0x00, 0x91, 0x43, 0x50, // Channel 2, G4.
            0x60, 0x80, 0x3C, 0x00, // Channel 1 note off at tick 96.
            0x00, 0x81, 0x43, 0x00, // Channel 2 note off at tick 96.
            0x00, 0xFF, 0x2F, 0x00,
        ]);
        let midi = MidiFile::parse(&input).expect("fixture should parse");
        let track = &midi.tracks()[0];

        assert_eq!(track.notes().len(), 2);

        let notes = track.notes_with_options(MidiImportOptions {
            midi_channel_mask: 1u16 << 1,
            ..MidiImportOptions::default()
        });
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].channel(), 1);
        assert_eq!(notes[0].key(), 67);
        assert_eq!(notes[0].end_tick(), Some(96));
    }

    #[test]
    fn fl_note_release_maps_to_midi_note_off_velocity() {
        assert_eq!(super::fl_note_release_to_midi_velocity(0), 0);
        assert_eq!(super::fl_note_release_to_midi_velocity(64), 64);
        assert_eq!(super::fl_note_release_to_midi_velocity(65), 64);
        assert_eq!(super::fl_note_release_to_midi_velocity(128), 127);
        assert_eq!(super::fl_note_release_to_midi_velocity(255), 127);
    }

    #[test]
    fn fl_note_pan_maps_to_midi_cc10_range_with_center_preserved() {
        assert_eq!(super::fl_note_pan_to_midi_value(0), 0);
        assert_eq!(super::fl_note_pan_to_midi_value(64), 64);
        assert_eq!(super::fl_note_pan_to_midi_value(128), 127);
        assert_eq!(super::fl_note_pan_to_midi_value(255), 127);
        assert_eq!(super::midi_pan_to_fl_note_pan(0), 0);
        assert_eq!(super::midi_pan_to_fl_note_pan(64), 64);
        assert_eq!(super::midi_pan_to_fl_note_pan(65), 66);
        assert_eq!(super::midi_pan_to_fl_note_pan(126), 127);
        assert_eq!(super::midi_pan_to_fl_note_pan(127), 128);
        assert_eq!(super::midi_pan_to_fl_note_pan(255), 128);
    }

    #[test]
    fn fl_note_modulation_maps_to_midi_controller_range() {
        assert_eq!(super::fl_note_modulation_to_midi_value(0), 0);
        assert_eq!(super::fl_note_modulation_to_midi_value(128), 64);
        assert_eq!(super::fl_note_modulation_to_midi_value(255), 127);
        assert_eq!(super::midi_modulation_to_fl_note_value(0), 0);
        assert_eq!(super::midi_modulation_to_fl_note_value(64), 128);
        assert_eq!(super::midi_modulation_to_fl_note_value(127), 255);
    }

    #[test]
    fn notes_snapshot_modulation_controllers_at_note_on() {
        let input = midi_fixture(&[
            0x00, 0xB0, 74, 127, // Mod X / brightness.
            0x00, 0xB0, 71, 0, // Mod Y / timbre.
            0x00, 0x90, 60, 100, // Note on.
            0x60, 0xB0, 74, 0, // Later Mod X change does not affect the held note.
            0x00, 0x80, 60, 64, // Note off.
            0x00, 0xFF, 0x2F, 0x00,
        ]);
        let midi = MidiFile::parse(&input).expect("fixture should parse");
        let note = midi.tracks()[0].notes().remove(0);

        assert_eq!(note.mod_x(), 127);
        assert_eq!(note.mod_y(), 0);
        assert_eq!(note.end_tick(), Some(96));
    }

    #[test]
    fn midi_pitch_bend_cents_map_to_fl_fine_pitch_steps() {
        assert_eq!(super::midi_pitch_bend_cents_to_fl_note_fine_pitch(-1200), 0);
        assert_eq!(super::midi_pitch_bend_cents_to_fl_note_fine_pitch(-5), 119);
        assert_eq!(super::midi_pitch_bend_cents_to_fl_note_fine_pitch(0), 120);
        assert_eq!(super::midi_pitch_bend_cents_to_fl_note_fine_pitch(5), 121);
        assert_eq!(super::midi_pitch_bend_cents_to_fl_note_fine_pitch(600), 180);
        assert_eq!(
            super::midi_pitch_bend_cents_to_fl_note_fine_pitch(1200),
            240
        );
        assert_eq!(
            super::midi_pitch_bend_cents_to_fl_note_fine_pitch(2000),
            240
        );
    }

    #[test]
    fn rejects_overlong_delta_time_varints() {
        let input = midi_fixture(&[0x81, 0x80, 0x80, 0x80]);

        let error = MidiFile::parse(&input).expect_err("overlong delta must fail");

        assert!(matches!(error, MidiError::InvalidVlq { .. }));
    }

    #[test]
    fn exports_format_one_tracks_with_tempo_meter_and_channel_mapping() {
        let mut piano = MidiExportTrack::new("Piano", Some(14));
        piano.end_tick = 480;
        piano.notes.push(MidiExportNote {
            channel: 7,
            key: 60,
            velocity: 96,
            release_velocity: 127,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 240,
            fine_pitch_offset_cents: 0,
        });
        let mut bass = MidiExportTrack::new("Bass", Some(27));
        bass.end_tick = 480;
        bass.notes.push(MidiExportNote {
            channel: 7,
            key: 36,
            velocity: 128,
            release_velocity: 32,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 120,
            end_tick: 480,
            fine_pitch_offset_cents: 0,
        });

        let bytes = MidiFile::encode_export_tracks(
            &[piano, bass],
            480,
            120.0,
            Some((3, 4)),
            MidiChannelMapping::AssignProjectChannels,
        )
        .expect("valid export data should serialize");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");

        assert_eq!(midi.format(), 1);
        assert_eq!(midi.division(), 480);
        assert_eq!(midi.tracks().len(), 3);
        assert_eq!(midi.tracks()[0].tempo_bpm(), Some(120.0));
        assert_eq!(midi.tracks()[1].name().as_deref(), Some("Piano"));
        assert_eq!(midi.tracks()[2].name().as_deref(), Some("Bass"));
        let piano_note = midi.tracks()[1].notes().remove(0);
        let bass_note = midi.tracks()[2].notes().remove(0);
        assert_eq!(piano_note.channel(), 0);
        assert_eq!(piano_note.key(), 60);
        assert_eq!(piano_note.velocity(), 96);
        assert_eq!(piano_note.end_tick(), Some(240));
        assert_eq!(piano_note.release_velocity(), Some(127));
        assert_eq!(bass_note.channel(), 1);
        assert_eq!(bass_note.velocity(), 127);
        assert_eq!(bass_note.end_tick(), Some(480));
        assert_eq!(bass_note.release_velocity(), Some(32));
        assert!(midi.tracks()[0].events().iter().any(|event| {
            matches!(event.kind(), super::MidiEventKind::Meta { meta_type: 0x58, data } if data == &[3, 2, 24, 8])
        }));
    }

    #[test]
    fn overlapping_notes_with_different_pans_use_a_spare_midi_channel() {
        let mut track = MidiExportTrack::new("Pan", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 0,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 90,
            release_velocity: 0,
            pan: 127,
            mod_x: 64,
            mod_y: 64,
            start_tick: 12,
            end_tick: 36,
            fine_pitch_offset_cents: 0,
        });

        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            120.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("overlapping pan values should use an available channel");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let track = &midi.tracks()[1];
        let notes = track.notes();
        assert_eq!(notes[0].channel(), 0);
        assert_eq!(notes[1].channel(), 1);

        let events_at_second_onset = track
            .events()
            .iter()
            .filter(|event| event.absolute_tick() == 12)
            .collect::<Vec<_>>();
        assert!(matches!(
            events_at_second_onset[0].kind(),
            super::MidiEventKind::ChannelVoice { status, data }
                if *status == 0xB1 && data == &[10, 127]
        ));
        assert!(matches!(
            events_at_second_onset[1].kind(),
            super::MidiEventKind::ChannelVoice { status, data }
                if *status == 0xB1 && data == &[74, 64]
        ));
        assert!(matches!(
            events_at_second_onset[2].kind(),
            super::MidiEventKind::ChannelVoice { status, data }
                if *status == 0xB1 && data == &[71, 64]
        ));
        assert!(matches!(
            events_at_second_onset[3].kind(),
            super::MidiEventKind::ChannelVoice { status, data }
                if *status == 0x91 && data == &[64, 90]
        ));
    }

    #[test]
    fn exports_note_mod_x_and_mod_y_as_midi_controllers() {
        let mut track = MidiExportTrack::new("Modulation", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 91,
            mod_y: 37,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });

        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            120.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("valid note modulation should serialize");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let events = midi.tracks()[1]
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                super::MidiEventKind::ChannelVoice { status, data }
                    if *status == 0xB0 && data.len() == 2 =>
                {
                    Some((data[0], data[1]))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(events, [(10, 64), (74, 91), (71, 37)]);
    }

    #[test]
    fn overlapping_notes_with_different_modulation_use_a_spare_midi_channel() {
        let mut track = MidiExportTrack::new("Modulation", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 20,
            mod_y: 30,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 100,
            mod_y: 110,
            start_tick: 12,
            end_tick: 36,
            fine_pitch_offset_cents: 0,
        });

        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            120.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("overlapping note modulation should use an available channel");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let notes = midi.tracks()[1].notes();
        assert_eq!(notes[0].channel(), 0);
        assert_eq!(notes[1].channel(), 1);
        assert!(midi.tracks()[1].events().iter().any(|event| {
            event.absolute_tick() == 12
                && matches!(event.kind(), super::MidiEventKind::ChannelVoice { status, data }
                    if *status == 0xB1 && data == &[74, 100])
        }));
        assert!(midi.tracks()[1].events().iter().any(|event| {
            event.absolute_tick() == 12
                && matches!(event.kind(), super::MidiEventKind::ChannelVoice { status, data }
                    if *status == 0xB1 && data == &[71, 110])
        }));
    }

    #[test]
    fn exports_large_note_positions_using_vlq_safe_meta_events() {
        let mut track = MidiExportTrack::new("Long timeline", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: u64::from(super::MAX_VLQ as u32) + 17,
            end_tick: u64::from(super::MAX_VLQ as u32) + 200,
            fine_pitch_offset_cents: 0,
        });
        let bytes = MidiFile::encode_export_tracks(
            &[track],
            960,
            90.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("large timeline should serialize");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let note = midi.tracks()[1].notes().remove(0);
        assert_eq!(note.start_tick(), u64::from(super::MAX_VLQ as u32) + 17);
        assert_eq!(
            note.end_tick(),
            Some(u64::from(super::MAX_VLQ as u32) + 200)
        );
    }

    #[test]
    fn stored_channel_export_uses_only_the_midi_channel_nibble() {
        let mut track = MidiExportTrack::new("Legacy channel", Some(0));
        track.notes.push(MidiExportNote {
            channel: 32,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            140.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("the stored channel should be normalized to a MIDI status nibble");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");

        assert_eq!(midi.tracks()[1].notes()[0].channel(), 0);
    }

    #[test]
    fn channel_mapping_respects_empty_project_channels() {
        let mut piano = MidiExportTrack::new("Piano", Some(14));
        piano.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        let mut bass = MidiExportTrack::new("Bass", Some(27));
        bass.notes.push(MidiExportNote {
            channel: 0,
            key: 36,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        let bytes = MidiFile::encode_export_tracks_with_channel_order(
            &[piano, bass],
            96,
            140.0,
            None,
            MidiChannelMapping::AssignProjectChannels,
            &[7, 14, 27],
        )
        .expect("project channel order should serialize");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");

        assert_eq!(midi.tracks()[1].notes()[0].channel(), 1);
        assert_eq!(midi.tracks()[2].notes()[0].channel(), 2);
    }

    #[test]
    fn exports_note_fine_pitch_with_pitch_bend_and_a_twelve_semitone_range() {
        let mut track = MidiExportTrack::new("Fine pitch", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 100,
        });
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        let mut other_track = MidiExportTrack::new("Other mapped channel", Some(1));
        other_track.notes.push(MidiExportNote {
            channel: 1,
            key: 48,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 0,
        });
        let bytes = MidiFile::encode_export_tracks(
            &[track, other_track],
            96,
            140.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("independent fine pitches should be assigned separate MIDI channels");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let midi_track = &midi.tracks()[1];
        let notes = midi_track.notes();
        let bent_channel = notes
            .iter()
            .find(|note| note.key() == 60)
            .expect("fine-pitched note should be present")
            .channel();
        let plain_channel = notes
            .iter()
            .find(|note| note.key() == 64)
            .expect("unpitched note should be present")
            .channel();
        assert_ne!(bent_channel, plain_channel);
        assert_eq!(plain_channel, 2);
        assert_eq!(midi.tracks()[2].notes()[0].channel(), 1);

        let bend_events = midi_track
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                super::MidiEventKind::ChannelVoice { status, data }
                    if status & 0xF0 == 0xE0 && status & 0x0F == bent_channel =>
                {
                    Some((
                        event.absolute_tick(),
                        u16::from(data[0]) | (u16::from(data[1]) << 7),
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bend_events,
            vec![(0, super::midi_pitch_bend_value(100)), (24, 8192),]
        );

        let range_setup = midi_track
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                super::MidiEventKind::ChannelVoice { status, data }
                    if status & 0xF0 == 0xB0
                        && status & 0x0F == bent_channel
                        && matches!(data[0], 6 | 38 | 100 | 101) =>
                {
                    Some([data[0], data[1]])
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            range_setup,
            vec![[101, 0], [100, 0], [6, 12], [38, 0], [101, 127], [100, 127],]
        );
    }

    #[test]
    fn overlapping_notes_with_the_same_fine_pitch_share_a_bend_until_the_last_note_ends() {
        let mut track = MidiExportTrack::new("Overlapping notes", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 100,
        });
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 12,
            end_tick: 36,
            fine_pitch_offset_cents: 100,
        });
        let bytes = MidiFile::encode_export_tracks(
            &[track],
            96,
            140.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("matching bends can share a MIDI channel");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let midi_track = &midi.tracks()[1];
        let notes = midi_track.notes();
        assert_eq!(notes[0].channel(), notes[1].channel());

        let bend_events = midi_track
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                super::MidiEventKind::ChannelVoice { status, data }
                    if status & 0xF0 == 0xE0 && status & 0x0F == notes[0].channel() =>
                {
                    Some((
                        event.absolute_tick(),
                        u16::from(data[0]) | (u16::from(data[1]) << 7),
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bend_events,
            vec![
                (0, super::midi_pitch_bend_value(100)),
                (12, super::midi_pitch_bend_value(100)),
                (36, 8192),
            ]
        );
    }

    #[test]
    fn uses_a_spare_channel_when_bend_changes_at_a_cross_track_boundary() {
        let mut first = MidiExportTrack::new("Bent part", Some(0));
        first.notes.push(MidiExportNote {
            channel: 0,
            key: 60,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 24,
            fine_pitch_offset_cents: 100,
        });
        let mut second = MidiExportTrack::new("Plain part", Some(1));
        second.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 24,
            end_tick: 48,
            fine_pitch_offset_cents: 0,
        });
        let bytes = MidiFile::encode_export_tracks(
            &[first, second],
            96,
            140.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect("a spare channel should keep separate track events ordered");
        let midi = MidiFile::parse(&bytes).expect("encoded MIDI should parse");
        let bent_channel = midi.tracks()[1].notes()[0].channel();
        let plain_channel = midi.tracks()[2].notes()[0].channel();
        assert_ne!(bent_channel, plain_channel);
        assert_eq!(bent_channel, 0);
        assert_eq!(plain_channel, 1);
    }

    #[test]
    fn rejects_fine_pitch_polyphony_that_exhausts_all_midi_channels() {
        let mut track = MidiExportTrack::new("Too many pitches", Some(0));
        for index in 0..17 {
            track.notes.push(MidiExportNote {
                channel: (index % 16) as u8,
                key: 48 + index as u8,
                velocity: 100,
                release_velocity: 0,
                pan: 64,
                mod_x: 64,
                mod_y: 64,
                start_tick: 0,
                end_tick: 24,
                fine_pitch_offset_cents: (index as i16 - 8) * 100,
            });
        }
        let error = MidiFile::encode_export_tracks(
            &[track],
            96,
            140.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect_err("17 independent simultaneous tunings exceed MIDI 1.0 channel capacity");
        assert!(matches!(error, MidiError::InvalidExport(_)));
    }

    #[test]
    fn rejects_unrepresentable_midi_export_values() {
        let mut track = MidiExportTrack::new("Invalid", Some(0));
        track.notes.push(MidiExportNote {
            channel: 16,
            key: 128,
            velocity: 100,
            release_velocity: 0,
            pan: 64,
            mod_x: 64,
            mod_y: 64,
            start_tick: 0,
            end_tick: 1,
            fine_pitch_offset_cents: 0,
        });
        let error = MidiFile::encode_export_tracks(
            &[track],
            480,
            120.0,
            None,
            MidiChannelMapping::PreserveNoteChannels,
        )
        .expect_err("out-of-range key must be rejected");
        assert!(matches!(error, MidiError::InvalidExport(_)));
    }
}

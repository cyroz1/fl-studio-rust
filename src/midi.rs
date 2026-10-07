use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use crate::{FlpDocument, PatternNote, PlaylistClipTarget};

const MTHD: &[u8; 4] = b"MThd";
const MTRK: &[u8; 4] = b"MTrk";
const MAX_VLQ: u64 = 0x0FFF_FFFF;
const MAX_EXPORTED_NOTES: usize = 1_000_000;

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
    start_tick: u64,
    end_tick: Option<u64>,
    release_velocity: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiTempoEvent {
    tick: u64,
    microseconds_per_quarter: u32,
}

/// Selects how FL Studio channels are assigned to MIDI channels during export.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MidiChannelMapping {
    /// Preserve the low four bits of each note's stored MIDI channel.
    #[default]
    PreserveNoteChannels,
    /// Assign each distinct FL Studio channel its own MIDI channel, wrapping after 16.
    AssignProjectChannels,
}

/// A note to serialize into a Standard MIDI File.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiExportNote {
    pub channel: u8,
    pub key: u8,
    pub velocity: u8,
    pub start_tick: u64,
    pub end_tick: u64,
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
        Self::encode_project_export_tracks(document, &tracks, channel_mapping)
    }

    /// Exports pattern clips in one FL Studio arrangement to a format-1 MIDI file.
    /// Audio clips and non-pattern clip targets are omitted.
    pub fn encode_project_song(
        document: &FlpDocument,
        arrangement_id: u16,
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
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
        let mut notes_by_channel = BTreeMap::<u16, Vec<MidiExportNote>>::new();
        let mut song_end_tick = 0u64;
        let mut exported_note_count = 0usize;

        for clip in &arrangement.clips {
            if clip.track_index.is_none() || clip.length_ticks == 0 {
                continue;
            }
            let PlaylistClipTarget::Pattern { id } = clip.target() else {
                continue;
            };
            let Some(pattern) = patterns_by_id.get(&id).copied() else {
                continue;
            };
            let clip_end = u64::from(clip.position_ticks)
                .checked_add(u64::from(clip.length_ticks))
                .ok_or(MidiError::LengthOverflow)?;
            song_end_tick = song_end_tick.max(clip_end);
            if clip
                .scale
                .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
            {
                return Err(MidiError::InvalidExport(
                    "scaled Playlist Pattern Clips cannot be exported yet",
                ));
            }
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
                .unwrap_or(if inferred_length > 0 {
                    inferred_length
                } else {
                    clip_length
                });
            if loop_length == 0 {
                continue;
            }
            let clip_start = u64::from(clip.position_ticks);
            let repetitions = clip_length.div_ceil(loop_length);
            for repetition in 0..repetitions {
                let repeat_start = repetition
                    .checked_mul(loop_length)
                    .ok_or(MidiError::LengthOverflow)?;
                for note in &pattern.notes {
                    let relative_start = repeat_start
                        .checked_add(u64::from(note.position))
                        .ok_or(MidiError::LengthOverflow)?;
                    if relative_start >= clip_length {
                        continue;
                    }
                    exported_note_count = exported_note_count.saturating_add(1);
                    if exported_note_count > MAX_EXPORTED_NOTES {
                        return Err(MidiError::InvalidExport(
                            "the arrangement expands to more than one million MIDI notes",
                        ));
                    }
                    let start_tick = clip_start
                        .checked_add(relative_start)
                        .ok_or(MidiError::LengthOverflow)?;
                    let stop_tick = start_tick
                        .checked_add(u64::from(note.length))
                        .ok_or(MidiError::LengthOverflow)?
                        .min(clip_end);
                    notes_by_channel
                        .entry(note.channel_id)
                        .or_default()
                        .push(export_note(note, start_tick, stop_tick));
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
        Self::encode_project_export_tracks(document, &tracks, channel_mapping)
    }

    /// Serializes named channel tracks and a conductor track to Standard MIDI File format 1.
    pub fn encode_export_tracks(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
        Self::encode_export_tracks_with_channel_order(
            tracks,
            ppq,
            tempo_bpm,
            time_signature,
            channel_mapping,
            &[],
        )
    }

    fn encode_project_export_tracks(
        document: &FlpDocument,
        tracks: &[MidiExportTrack],
        channel_mapping: MidiChannelMapping,
    ) -> Result<Vec<u8>, MidiError> {
        let channel_order = document
            .channels()
            .into_iter()
            .map(|channel| channel.id())
            .collect::<Vec<_>>();
        Self::encode_export_tracks_with_channel_order(
            tracks,
            document.header().ppq(),
            document.metadata().tempo_bpm().unwrap_or(140.0),
            document.metadata().time_signature(),
            channel_mapping,
            &channel_order,
        )
    }

    fn encode_export_tracks_with_channel_order(
        tracks: &[MidiExportTrack],
        ppq: u16,
        tempo_bpm: f64,
        time_signature: Option<(u8, u8)>,
        channel_mapping: MidiChannelMapping,
        project_channel_order: &[u16],
    ) -> Result<Vec<u8>, MidiError> {
        if ppq == 0 || ppq & 0x8000 != 0 {
            return Err(MidiError::InvalidExport(
                "project PPQ must be in the range 1..=32767",
            ));
        }
        if !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
            return Err(MidiError::InvalidExport(
                "tempo must be positive and finite",
            ));
        }
        let micros_per_quarter = (60_000_000.0 / tempo_bpm).round();
        if !(1.0..=16_777_215.0).contains(&micros_per_quarter) {
            return Err(MidiError::InvalidExport(
                "tempo is outside the range representable by Standard MIDI Files",
            ));
        }
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
        let track_count = tracks
            .len()
            .checked_add(1)
            .ok_or(MidiError::LengthOverflow)?;
        let track_count = u16::try_from(track_count).map_err(|_| MidiError::LengthOverflow)?;
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

        let mut output = Vec::new();
        output.extend_from_slice(MTHD);
        output.extend_from_slice(&6u32.to_be_bytes());
        output.extend_from_slice(&1u16.to_be_bytes());
        output.extend_from_slice(&track_count.to_be_bytes());
        output.extend_from_slice(&ppq.to_be_bytes());

        let mut conductor = Vec::new();
        push_meta_event(&mut conductor, 0, 0x03, b"Tempo and meter")?;
        let tempo_bytes = (micros_per_quarter as u32).to_be_bytes();
        push_meta_event(&mut conductor, 0, 0x51, &tempo_bytes[1..])?;
        if let Some((numerator, denominator)) = time_signature {
            let exponent = denominator.trailing_zeros() as u8;
            push_meta_event(&mut conductor, 0, 0x58, &[numerator, exponent, 24, 8])?;
        }
        write_end_of_track(&mut conductor, 0)?;
        append_track_chunk(&mut output, conductor)?;

        for track in tracks {
            let mut events = Vec::<TimedMidiBytes>::new();
            events.push(TimedMidiBytes {
                tick: 0,
                order: 0,
                bytes: meta_event_bytes(0x03, track.name.as_bytes())?,
            });
            let assigned_channel = track
                .project_channel_id
                .and_then(|id| channel_indices.get(&id).copied())
                .unwrap_or(0);
            for note in &track.notes {
                if note.key > 127 {
                    return Err(MidiError::InvalidExport(
                        "a note key exceeds the MIDI range 0..=127",
                    ));
                }
                let channel = match channel_mapping {
                    MidiChannelMapping::PreserveNoteChannels => note.channel & 0x0F,
                    MidiChannelMapping::AssignProjectChannels => assigned_channel,
                };
                let start_tick = note.start_tick;
                let end_tick = note.end_tick.max(start_tick.saturating_add(1));
                let velocity = note.velocity.min(127);
                events.push(TimedMidiBytes {
                    tick: start_tick,
                    order: 2,
                    bytes: vec![0x90 | channel, note.key, velocity],
                });
                events.push(TimedMidiBytes {
                    tick: end_tick,
                    order: 1,
                    bytes: vec![0x80 | channel, note.key, 0],
                });
            }
            events.sort_by_key(|event| (event.tick, event.order));
            let mut track_bytes = Vec::new();
            let mut previous_tick = 0u64;
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
        Ok(output)
    }
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

    /// Pairs note-on and note-off messages in channel/key order. Notes without an
    /// ending message remain present with `end_tick == None`.
    pub fn notes(&self) -> Vec<MidiNote> {
        let mut notes = Vec::new();
        let mut active: BTreeMap<(u8, u8), VecDeque<usize>> = BTreeMap::new();

        for event in &self.events {
            let MidiEventKind::ChannelVoice { status, data } = &event.kind else {
                continue;
            };
            if data.len() < 2 {
                continue;
            }

            let channel = status & 0x0F;
            let key = data[0];
            match status & 0xF0 {
                0x90 if data[1] != 0 => {
                    let note_index = notes.len();
                    notes.push(MidiNote {
                        channel,
                        key,
                        velocity: data[1],
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
                            notes[note_index].end_tick = Some(event.absolute_tick);
                            notes[note_index].release_velocity = Some(data[1]);
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

impl MidiNote {
    pub fn channel(&self) -> u8 {
        self.channel
    }

    pub fn key(&self) -> u8 {
        self.key
    }

    pub fn velocity(&self) -> u8 {
        self.velocity
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

fn tracks_for_notes(
    document: &FlpDocument,
    notes: &[PatternNote],
) -> Result<Vec<MidiExportTrack>, MidiError> {
    if notes.len() > MAX_EXPORTED_NOTES {
        return Err(MidiError::InvalidExport(
            "the pattern contains more than one million MIDI notes",
        ));
    }
    let mut grouped = BTreeMap::<u16, Vec<MidiExportNote>>::new();
    for note in notes {
        let track_notes = grouped.entry(note.channel_id).or_default();
        if track_notes.len() >= MAX_EXPORTED_NOTES {
            return Err(MidiError::InvalidExport(
                "the pattern expands to more than one million MIDI notes",
            ));
        }
        let end_tick = u64::from(note.position) + u64::from(note.length);
        track_notes.push(export_note(note, u64::from(note.position), end_tick));
    }
    tracks_for_grouped_notes(document, grouped)
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

fn export_note(note: &PatternNote, start_tick: u64, end_tick: u64) -> MidiExportNote {
    MidiExportNote {
        channel: note.midi_channel,
        key: u8::try_from(note.key).unwrap_or(u8::MAX),
        velocity: note.velocity,
        start_tick,
        end_tick,
    }
}

fn push_meta_event(
    output: &mut Vec<u8>,
    delta_ticks: u64,
    meta_type: u8,
    data: &[u8],
) -> Result<(), MidiError> {
    write_delta(output, delta_ticks)?;
    output.extend_from_slice(&meta_event_bytes(meta_type, data)?);
    Ok(())
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
    use super::{MidiChannelMapping, MidiError, MidiExportNote, MidiExportTrack, MidiFile};

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
        assert_eq!(notes[0].start_tick(), 0);
        assert_eq!(notes[0].end_tick(), Some(240));
        assert_eq!(notes[0].duration_ticks(), Some(240));
        assert_eq!(midi.encode_lossless(), input);
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
            start_tick: 0,
            end_tick: 240,
        });
        let mut bass = MidiExportTrack::new("Bass", Some(27));
        bass.end_tick = 480;
        bass.notes.push(MidiExportNote {
            channel: 7,
            key: 36,
            velocity: 128,
            start_tick: 120,
            end_tick: 480,
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
        assert_eq!(bass_note.channel(), 1);
        assert_eq!(bass_note.velocity(), 127);
        assert_eq!(bass_note.end_tick(), Some(480));
        assert!(midi.tracks()[0].events().iter().any(|event| {
            matches!(event.kind(), super::MidiEventKind::Meta { meta_type: 0x58, data } if data == &[3, 2, 24, 8])
        }));
    }

    #[test]
    fn exports_large_note_positions_using_vlq_safe_meta_events() {
        let mut track = MidiExportTrack::new("Long timeline", Some(0));
        track.notes.push(MidiExportNote {
            channel: 0,
            key: 64,
            velocity: 100,
            start_tick: u64::from(super::MAX_VLQ as u32) + 17,
            end_tick: u64::from(super::MAX_VLQ as u32) + 200,
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
            start_tick: 0,
            end_tick: 24,
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
            start_tick: 0,
            end_tick: 24,
        });
        let mut bass = MidiExportTrack::new("Bass", Some(27));
        bass.notes.push(MidiExportNote {
            channel: 0,
            key: 36,
            velocity: 100,
            start_tick: 0,
            end_tick: 24,
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
    fn rejects_unrepresentable_midi_export_values() {
        let mut track = MidiExportTrack::new("Invalid", Some(0));
        track.notes.push(MidiExportNote {
            channel: 16,
            key: 128,
            velocity: 100,
            start_tick: 0,
            end_tick: 1,
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

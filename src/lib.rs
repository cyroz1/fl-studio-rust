use std::collections::HashMap;
use std::fmt;

pub mod midi;
pub mod plugins;
pub mod vst3;

const FLHD: &[u8; 4] = b"FLhd";
const FLDT: &[u8; 4] = b"FLdt";
const MIN_HEADER_CONTENT_LENGTH: usize = 6;
const FLP_NOTE_RECORD_SIZE: usize = 24;
const FLP_PLAYLIST_RECORD_SIZES: [usize; 3] = [80, 60, 32];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpHeader {
    format: u16,
    legacy_channel_count: u16,
    ppq: u16,
    extension: Vec<u8>,
}

impl FlpHeader {
    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn legacy_channel_count(&self) -> u16 {
        self.legacy_channel_count
    }

    pub fn ppq(&self) -> u16 {
        self.ppq
    }

    pub fn extension(&self) -> &[u8] {
        &self.extension
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PayloadEncoding {
    Byte,
    Word,
    Dword,
    Data { length_prefix: Vec<u8> },
    FixedThreeBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpEvent {
    opcode: u8,
    payload: Vec<u8>,
    encoding: PayloadEncoding,
    wire_bytes: Vec<u8>,
    file_offset: usize,
}

impl FlpEvent {
    pub fn opcode(&self) -> u8 {
        self.opcode
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn encoding(&self) -> &PayloadEncoding {
        &self.encoding
    }

    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire_bytes
    }

    pub fn file_offset(&self) -> usize {
        self.file_offset
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlpDocument {
    header: FlpHeader,
    events: Vec<FlpEvent>,
    project_version: Option<String>,
    metadata: ProjectMetadata,
    trailing_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectMetadata {
    tempo_milli_bpm: Option<u32>,
    time_signature: Option<(u8, u8)>,
    build_number: Option<u32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChannelSummary {
    id: u16,
    kind: Option<u8>,
    enabled: Option<bool>,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    first_event_index: usize,
    end_event_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelPluginState {
    channel_id: u16,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    wrapper_payload: Option<Vec<u8>>,
    data_payload: Vec<u8>,
    data_event_index: usize,
    vst_metadata: Option<VstPluginStateMetadata>,
}

/// Identity fields embedded in an FLP VST plug-in state event.
///
/// The complete event remains available through [`ChannelPluginState::data_payload`];
/// this structure only decodes the length-prefixed identity envelope around its state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VstPluginStateMetadata {
    format_marker: u32,
    plugin_info: Option<Vec<u8>>,
    fourcc: Option<String>,
    guid: Option<Vec<u8>>,
    name: Option<String>,
    path: Option<String>,
    vendor: Option<String>,
    state_bytes: Option<usize>,
}

impl ChannelPluginState {
    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Opaque `0xD4` wrapper metadata, when present.
    pub fn wrapper_payload(&self) -> Option<&[u8]> {
        self.wrapper_payload.as_deref()
    }

    /// Opaque `0xD5` per-instance plug-in data.
    pub fn data_payload(&self) -> &[u8] {
        &self.data_payload
    }

    pub fn data_event_index(&self) -> usize {
        self.data_event_index
    }

    /// VST identity metadata when the `0xD5` event has a recognized VST envelope.
    pub fn vst_metadata(&self) -> Option<&VstPluginStateMetadata> {
        self.vst_metadata.as_ref()
    }
}

impl VstPluginStateMetadata {
    /// FL's VST wrapper serialization marker, observed as 8, 10, and 12.
    pub fn format_marker(&self) -> u32 {
        self.format_marker
    }

    /// Opaque 16-byte plug-in information field, when present.
    pub fn plugin_info(&self) -> Option<&[u8]> {
        self.plugin_info.as_deref()
    }

    /// VST2 four-character identifier, when present.
    pub fn fourcc(&self) -> Option<&str> {
        self.fourcc.as_deref()
    }

    /// Raw 16-byte GUID stored in the FLP envelope.
    pub fn guid(&self) -> Option<&[u8]> {
        self.guid.as_deref()
    }

    /// VST factory name stored in the FLP envelope.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Original plug-in binary path stored in the FLP envelope.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Plug-in vendor stored in the FLP envelope.
    pub fn vendor(&self) -> Option<&str> {
        self.vendor.as_deref()
    }

    /// Size of the nested plug-in state field, without copying the state bytes.
    pub fn state_bytes(&self) -> Option<usize> {
        self.state_bytes
    }

    /// VST3-style 32-digit class UID converted to the format used by the host API.
    pub fn class_uid(&self) -> Option<String> {
        let guid: [u8; 16] = self.guid.as_deref()?.try_into().ok()?;
        Some(format!(
            "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{}",
            guid[3],
            guid[2],
            guid[1],
            guid[0],
            guid[5],
            guid[4],
            guid[7],
            guid[6],
            guid[8..]
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ))
    }
}

impl ChannelSummary {
    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn kind(&self) -> Option<u8> {
        self.kind
    }

    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Pattern {
    pub id: u16,
    pub name: Option<String>,
    pub length_ticks: Option<u32>,
    pub notes: Vec<PatternNote>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNote {
    pub position: u32,
    pub flags: u16,
    pub channel_id: u16,
    pub length: u32,
    pub key: u16,
    pub group: u16,
    pub fine_pitch: u8,
    pub reserved: u8,
    pub release: u8,
    pub midi_channel: u8,
    pub pan: u8,
    pub velocity: u8,
    pub mod_x: u8,
    pub mod_y: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNoteEdit {
    pub position: Option<u32>,
    pub flags: Option<u16>,
    pub length: Option<u32>,
    pub key: Option<u16>,
    pub group: Option<u16>,
    pub fine_pitch: Option<u8>,
    pub release: Option<u8>,
    pub midi_channel: Option<u8>,
    pub pan: Option<u8>,
    pub velocity: Option<u8>,
    pub mod_x: Option<u8>,
    pub mod_y: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arrangement {
    pub id: u16,
    pub name: Option<String>,
    pub clips: Vec<PlaylistClip>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTrack {
    /// One-based Playlist track identifier stored in the `0xEE` record.
    pub id: u32,
    /// The adjacent `0x2B` byte is retained without assigning it a meaning.
    pub state_byte: Option<u8>,
    pub name: Option<String>,
    /// Raw 70-byte `0xEE` state payload.
    pub state_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistClip {
    pub position_ticks: u32,
    pub pattern_base: u16,
    pub item_index: u16,
    pub length_ticks: u32,
    pub raw_track_index: u16,
    pub track_index: Option<u16>,
    pub group: u16,
    pub unknown_word: u16,
    pub item_flags: u16,
    pub header_bytes: [u8; 4],
    pub start_offset: f32,
    pub end_offset: f32,
    pub clip_id: Option<u32>,
    pub reserved: Vec<u8>,
    pub scale: Option<f64>,
    pub trailing_bytes: Vec<u8>,
    pub record_size: usize,
    source_event_index: usize,
    source_record_index: usize,
}

impl PlaylistClip {
    pub fn playlist_track_id(&self) -> Option<u32> {
        self.track_index
            .and_then(|track_index| u32::from(track_index).checked_add(1))
    }

    pub fn target(&self) -> PlaylistClipTarget {
        if self.item_index >= self.pattern_base {
            PlaylistClipTarget::Pattern {
                id: self.item_index - self.pattern_base,
            }
        } else {
            PlaylistClipTarget::Channel {
                id: self.item_index,
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistClipTarget {
    Pattern { id: u16 },
    Channel { id: u16 },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaylistClipEdit {
    pub position_ticks: Option<u32>,
    pub length_ticks: Option<u32>,
    pub item_index: Option<u16>,
    pub raw_track_index: Option<u16>,
    pub group: Option<u16>,
    pub item_flags: Option<u16>,
    pub start_offset: Option<f32>,
    pub end_offset: Option<f32>,
    pub scale: Option<f64>,
}

impl PatternNote {
    fn decode(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), FLP_NOTE_RECORD_SIZE);
        Self {
            position: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            flags: u16::from_le_bytes([bytes[4], bytes[5]]),
            channel_id: u16::from_le_bytes([bytes[6], bytes[7]]),
            length: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            key: u16::from_le_bytes([bytes[12], bytes[13]]),
            group: u16::from_le_bytes([bytes[14], bytes[15]]),
            fine_pitch: bytes[16],
            reserved: bytes[17],
            release: bytes[18],
            midi_channel: bytes[19],
            pan: bytes[20],
            velocity: bytes[21],
            mod_x: bytes[22],
            mod_y: bytes[23],
        }
    }

    fn encode_into(&self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), FLP_NOTE_RECORD_SIZE);
        bytes[0..4].copy_from_slice(&self.position.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.channel_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.length.to_le_bytes());
        bytes[12..14].copy_from_slice(&self.key.to_le_bytes());
        bytes[14..16].copy_from_slice(&self.group.to_le_bytes());
        bytes[16] = self.fine_pitch;
        bytes[17] = self.reserved;
        bytes[18] = self.release;
        bytes[19] = self.midi_channel;
        bytes[20] = self.pan;
        bytes[21] = self.velocity;
        bytes[22] = self.mod_x;
        bytes[23] = self.mod_y;
    }

    fn apply(&mut self, edit: PatternNoteEdit) {
        if let Some(value) = edit.position {
            self.position = value;
        }
        if let Some(value) = edit.flags {
            self.flags = value;
        }
        if let Some(value) = edit.length {
            self.length = value;
        }
        if let Some(value) = edit.key {
            self.key = value;
        }
        if let Some(value) = edit.group {
            self.group = value;
        }
        if let Some(value) = edit.fine_pitch {
            self.fine_pitch = value;
        }
        if let Some(value) = edit.release {
            self.release = value;
        }
        if let Some(value) = edit.midi_channel {
            self.midi_channel = value;
        }
        if let Some(value) = edit.pan {
            self.pan = value;
        }
        if let Some(value) = edit.velocity {
            self.velocity = value;
        }
        if let Some(value) = edit.mod_x {
            self.mod_x = value;
        }
        if let Some(value) = edit.mod_y {
            self.mod_y = value;
        }
    }
}

impl ProjectMetadata {
    /// Returns the tempo as an integer number of thousandths of a beat per minute.
    pub fn tempo_milli_bpm(&self) -> Option<u32> {
        self.tempo_milli_bpm
    }

    pub fn tempo_bpm(&self) -> Option<f64> {
        self.tempo_milli_bpm
            .map(|milli_bpm| f64::from(milli_bpm) / 1000.0)
    }

    pub fn time_signature(&self) -> Option<(u8, u8)> {
        self.time_signature
    }

    pub fn build_number(&self) -> Option<u32> {
        self.build_number
    }
}

impl FlpDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self, FlpError> {
        if bytes.len() < 8 {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLhd chunk header",
            });
        }
        require_magic(bytes, 0, FLHD)?;

        let header_content_length = read_u32(bytes, 4, "FLhd length")? as usize;
        if header_content_length < MIN_HEADER_CONTENT_LENGTH {
            return Err(FlpError::InvalidHeaderLength(header_content_length as u32));
        }
        let data_chunk_offset = 8usize
            .checked_add(header_content_length)
            .ok_or(FlpError::LengthOverflow)?;
        if data_chunk_offset > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLhd contents",
            });
        }
        if data_chunk_offset < 14 {
            return Err(FlpError::InvalidHeaderLength(header_content_length as u32));
        }

        let header = FlpHeader {
            format: read_u16(bytes, 8, "FLhd format")?,
            legacy_channel_count: read_u16(bytes, 10, "FLhd channel count")?,
            ppq: read_u16(bytes, 12, "FLhd PPQ")?,
            extension: bytes[14..data_chunk_offset].to_vec(),
        };

        let event_start = data_chunk_offset
            .checked_add(8)
            .ok_or(FlpError::LengthOverflow)?;
        if event_start > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLdt chunk header",
            });
        }
        require_magic(bytes, data_chunk_offset, FLDT)?;
        let data_length = read_u32(bytes, data_chunk_offset + 4, "FLdt length")? as usize;
        let event_end = event_start
            .checked_add(data_length)
            .ok_or(FlpError::LengthOverflow)?;
        if event_end > bytes.len() {
            return Err(FlpError::UnexpectedEof {
                offset: bytes.len(),
                context: "FLdt event stream",
            });
        }

        let stream = &bytes[event_start..event_end];
        let project_version = detect_project_version(stream);
        let modern_ac_event = project_version
            .as_deref()
            .and_then(|value| value.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 25);

        let mut events = Vec::new();
        let mut cursor = 0usize;
        while cursor < stream.len() {
            let (event, next_cursor) = parse_event(stream, cursor, event_start, modern_ac_event)?;
            if next_cursor <= cursor {
                return Err(FlpError::InvalidEvent {
                    offset: event_start + cursor,
                    detail: "event parser did not advance",
                });
            }
            events.push(event);
            cursor = next_cursor;
        }

        let metadata = read_project_metadata(&events);

        Ok(Self {
            header,
            events,
            project_version,
            metadata,
            trailing_bytes: bytes[event_end..].to_vec(),
        })
    }

    pub fn header(&self) -> &FlpHeader {
        &self.header
    }

    pub fn events(&self) -> &[FlpEvent] {
        &self.events
    }

    /// Returns channel summaries from the stream's `0x40` channel markers.
    /// Unrecognized channel data remains available in `events()`.
    pub fn channels(&self) -> Vec<ChannelSummary> {
        let mut channels = Vec::new();
        let mut current: Option<ChannelSummary> = None;

        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode == 0x40 && event.payload.len() == 2 {
                if let Some(mut channel) = current.take() {
                    channel.end_event_index = event_index;
                    channels.push(channel);
                }
                current = Some(ChannelSummary {
                    id: u16::from_le_bytes([event.payload[0], event.payload[1]]),
                    first_event_index: event_index,
                    end_event_index: self.events.len(),
                    ..ChannelSummary::default()
                });
                continue;
            }

            if event.opcode == 0x62 && current.is_some() {
                if let Some(mut channel) = current.take() {
                    channel.end_event_index = event_index;
                    channels.push(channel);
                }
                break;
            }

            let Some(channel) = current.as_mut() else {
                continue;
            };
            match event.opcode {
                0x00 if event.payload.len() == 1 => {
                    channel.enabled = Some(event.payload[0] != 0);
                }
                0x15 if event.payload.len() == 1 => channel.kind = Some(event.payload[0]),
                0xC9 if channel.plugin_identifier.is_none() => {
                    channel.plugin_identifier =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                0xCB if channel.display_name.is_none() => {
                    channel.display_name =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                0xC0 if channel.display_name.is_none()
                    && uses_legacy_string_encoding(self.project_version.as_deref()) =>
                {
                    channel.display_name =
                        decode_project_string(&event.payload, self.project_version.as_deref())
                            .filter(|value| !value.is_empty());
                }
                _ => {}
            }
        }

        if let Some(mut channel) = current {
            channel.end_event_index = self.events.len();
            channels.push(channel);
        }
        channels
    }

    /// Returns per-channel plug-in wrapper and data payloads when a `0xD5` event exists.
    /// The raw bytes are retained exactly; recognized VST envelopes also expose identity metadata.
    pub fn channel_plugin_states(&self) -> Vec<ChannelPluginState> {
        self.channels()
            .into_iter()
            .filter_map(|channel| {
                let mut wrapper_payload = None;
                let mut data_payload = None;
                let mut data_event_index = None;
                for index in channel.event_range() {
                    let event = &self.events[index];
                    match event.opcode {
                        0xD4 if wrapper_payload.is_none() => {
                            wrapper_payload = Some(event.payload.clone());
                        }
                        0xD5 if data_payload.is_none() => {
                            data_payload = Some(event.payload.clone());
                            data_event_index = Some(index);
                        }
                        _ => {}
                    }
                }
                let data_payload = data_payload?;
                Some(ChannelPluginState {
                    channel_id: channel.id,
                    plugin_identifier: channel.plugin_identifier,
                    display_name: channel.display_name,
                    wrapper_payload,
                    vst_metadata: parse_vst_plugin_state_metadata(&data_payload),
                    data_payload,
                    data_event_index: data_event_index?,
                })
            })
            .collect()
    }

    /// Returns pattern metadata and note records from the version-specific score event.
    pub fn patterns(&self) -> Result<Vec<Pattern>, FlpError> {
        let mut patterns = Vec::<Pattern>::new();
        let mut pattern_indices = HashMap::<u16, usize>::new();
        let mut current_pattern = None;
        let mut event_index = 0usize;

        while event_index < self.events.len() {
            let event = &self.events[event_index];
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    let pattern_id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    let pattern_index = *pattern_indices.entry(pattern_id).or_insert_with(|| {
                        let index = patterns.len();
                        patterns.push(Pattern {
                            id: pattern_id,
                            ..Pattern::default()
                        });
                        index
                    });
                    current_pattern = Some(pattern_index);

                    if let Some(notes_event) = self.events.get(event_index + 1)
                        && Self::is_pattern_note_event(notes_event)
                    {
                        if notes_event.payload.len() % FLP_NOTE_RECORD_SIZE != 0 {
                            return Err(FlpError::InvalidEvent {
                                offset: notes_event.file_offset,
                                detail: "pattern note payload is not a whole number of 24-byte records",
                            });
                        }
                        patterns[pattern_index].notes.extend(
                            notes_event
                                .payload
                                .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                                .0
                                .iter()
                                .map(|record| PatternNote::decode(record)),
                        );
                        event_index += 1;
                    }
                }
                0x40 | 0x62 | 0x63 => current_pattern = None,
                0xC1 => {
                    if let Some(pattern_index) = current_pattern
                        && patterns[pattern_index].name.is_none()
                    {
                        patterns[pattern_index].name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                    }
                }
                0xA4 if event.payload.len() == 4 => {
                    if let Some(pattern_index) = current_pattern {
                        patterns[pattern_index].length_ticks = Some(u32::from_le_bytes([
                            event.payload[0],
                            event.payload[1],
                            event.payload[2],
                            event.payload[3],
                        ]));
                    }
                }
                _ => {}
            }
            event_index += 1;
        }
        patterns.sort_by_key(|pattern| pattern.id);
        Ok(patterns)
    }

    /// Returns playlist arrangements and their stored clip records.
    /// The source events remain available unchanged through `events()`.
    pub fn arrangements(&self) -> Result<Vec<Arrangement>, FlpError> {
        let mut arrangements = Vec::<Arrangement>::new();
        let mut current_arrangement = None;
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);

        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x63 if event.payload.len() == 2 => {
                    let id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    arrangements.push(Arrangement {
                        id,
                        ..Arrangement::default()
                    });
                    current_arrangement = Some(arrangements.len() - 1);
                }
                0x62 if has_arrangement_markers => current_arrangement = None,
                0xF1 => {
                    if let Some(arrangement_index) = current_arrangement
                        && arrangements[arrangement_index].name.is_none()
                    {
                        arrangements[arrangement_index].name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                    }
                }
                0xE9 => {
                    let arrangement_index = if has_arrangement_markers {
                        let Some(arrangement_index) = current_arrangement else {
                            continue;
                        };
                        arrangement_index
                    } else {
                        if arrangements.is_empty() {
                            arrangements.push(Arrangement::default());
                        }
                        0
                    };
                    let record_size = playlist_clip_record_size(
                        self.project_version.as_deref(),
                        event.payload.len(),
                        event.file_offset,
                    )?;
                    if event.payload.len() % record_size != 0 {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "playlist clip payload is not a whole number of supported records",
                        });
                    }
                    arrangements[arrangement_index].clips.extend(
                        event.payload.chunks_exact(record_size).enumerate().map(
                            |(record_index, record)| {
                                decode_playlist_clip(record, event_index, record_index)
                            },
                        ),
                    );
                }
                _ => {}
            }
        }
        Ok(arrangements)
    }

    /// Returns Playlist tracks from the observed `0xEE`, `0x2B`, `0xEF` event sequence.
    pub fn playlist_tracks(&self) -> Vec<PlaylistTrack> {
        let mut tracks = Vec::new();
        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0xEE || event.payload.len() != 70 {
                continue;
            }
            let id = u32::from_le_bytes([
                event.payload[0],
                event.payload[1],
                event.payload[2],
                event.payload[3],
            ]);
            let next_event = self.events.get(event_index + 1);
            let state_byte = next_event
                .filter(|next| next.opcode == 0x2B && next.payload.len() == 1)
                .map(|next| next.payload[0]);
            let name_event_index = event_index + if state_byte.is_some() { 2 } else { 1 };
            let name = self
                .events
                .get(name_event_index)
                .filter(|name_event| name_event.opcode == 0xEF)
                .and_then(|name_event| {
                    decode_project_string(&name_event.payload, self.project_version.as_deref())
                })
                .filter(|value| !value.is_empty());
            tracks.push(PlaylistTrack {
                id,
                state_byte,
                name,
                state_bytes: event.payload.clone(),
            });
        }
        tracks
    }

    /// Edits fields with established playlist record offsets while preserving every other byte.
    /// `clip_index` is zero-based in the selected arrangement's stored clip order.
    pub fn edit_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        edit: PlaylistClipEdit,
    ) -> Result<(), FlpError> {
        let arrangements = self.arrangements()?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested arrangement id is ambiguous",
            ));
        }
        let Some(clip) = arrangement.clips.get(clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested playlist clip does not exist",
            ));
        };
        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let record_size = clip.record_size;

        if edit.scale.is_some() && record_size < 80 {
            return Err(FlpError::UnsupportedEdit(
                "this playlist clip record has no established scale field",
            ));
        }

        let mut writes = Vec::<(usize, Vec<u8>)>::new();
        if let Some(value) = edit.position_ticks {
            writes.push((record_start, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.item_index {
            writes.push((record_start + 6, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.length_ticks {
            writes.push((record_start + 8, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.raw_track_index {
            writes.push((record_start + 12, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.group {
            writes.push((record_start + 14, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.item_flags {
            writes.push((record_start + 18, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.start_offset {
            writes.push((record_start + 24, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.end_offset {
            writes.push((record_start + 28, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.scale {
            writes.push((record_start + 64, value.to_le_bytes().to_vec()));
        }
        for (payload_offset, bytes) in writes {
            write_event_payload_bytes(&mut self.events[event_index], payload_offset, &bytes)?;
        }
        Ok(())
    }

    /// Edits selected fields of one existing note without changing the event's wire length.
    /// `note_index` is zero-based within the selected channel's notes in that pattern.
    pub fn edit_pattern_note(
        &mut self,
        pattern_id: u16,
        channel_id: u16,
        note_index: usize,
        edit: PatternNoteEdit,
    ) -> Result<(), FlpError> {
        let mut channel_note_index = 0usize;
        let mut event_index = 0usize;
        while event_index < self.events.len() {
            let marker = &self.events[event_index];
            if marker.opcode != 0x41 || marker.payload.len() != 2 {
                event_index += 1;
                continue;
            }
            let marker_id = u16::from_le_bytes([marker.payload[0], marker.payload[1]]);
            let notes_index = event_index + 1;
            let Some(notes_event) = self.events.get(notes_index) else {
                break;
            };
            if marker_id != pattern_id || !Self::is_pattern_note_event(notes_event) {
                event_index += 1;
                continue;
            }
            if notes_event.payload.len() % FLP_NOTE_RECORD_SIZE != 0 {
                return Err(FlpError::InvalidEvent {
                    offset: notes_event.file_offset,
                    detail: "pattern note payload is not a whole number of 24-byte records",
                });
            }

            for (record_index, record) in notes_event
                .payload
                .as_chunks::<FLP_NOTE_RECORD_SIZE>()
                .0
                .iter()
                .enumerate()
            {
                let mut note = PatternNote::decode(record);
                if note.channel_id != channel_id {
                    continue;
                }
                if channel_note_index != note_index {
                    channel_note_index += 1;
                    continue;
                }
                note.apply(edit);
                let byte_offset = record_index * FLP_NOTE_RECORD_SIZE;
                let event = &mut self.events[notes_index];
                let payload_end = byte_offset + FLP_NOTE_RECORD_SIZE;
                note.encode_into(&mut event.payload[byte_offset..payload_end]);
                let prefix_length = match &event.encoding {
                    PayloadEncoding::Data { length_prefix } => length_prefix.len(),
                    _ => {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "pattern note event does not have a data payload",
                        });
                    }
                };
                let wire_start = 1 + prefix_length + byte_offset;
                let wire_end = wire_start + FLP_NOTE_RECORD_SIZE;
                event.wire_bytes[wire_start..wire_end]
                    .copy_from_slice(&event.payload[byte_offset..payload_end]);
                return Ok(());
            }
            event_index += 2;
        }
        Err(FlpError::UnsupportedEdit(
            "the requested pattern, channel, or note index does not exist",
        ))
    }

    pub fn project_version(&self) -> Option<&str> {
        self.project_version.as_deref()
    }

    pub fn metadata(&self) -> &ProjectMetadata {
        &self.metadata
    }

    /// Changes an existing four-byte `0x9C` tempo event without reserializing other events.
    pub fn set_tempo_milli_bpm(&mut self, milli_bpm: u32) -> Result<(), FlpError> {
        let event_index = self.events.iter().position(|event| {
            event.opcode == 0x9C
                && event.payload.len() == 4
                && event.encoding == PayloadEncoding::Dword
        });
        let Some(event_index) = event_index else {
            return Err(FlpError::UnsupportedEdit(
                "no recognized 0x9C tempo event exists in this file",
            ));
        };

        let encoded_value = milli_bpm.to_le_bytes();
        let event = &mut self.events[event_index];
        let Some(wire_payload) = event.wire_bytes.get_mut(1..5) else {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "0x9C event does not contain a four-byte payload",
            });
        };
        wire_payload.copy_from_slice(&encoded_value);
        event.payload.copy_from_slice(&encoded_value);
        self.metadata = read_project_metadata(&self.events);
        Ok(())
    }

    /// Renames a channel through its verified UTF-16LE `0xCB` display-name event.
    /// Unedited event bytes are retained; the edited event is resized if needed.
    pub fn set_channel_name(&mut self, channel_id: u16, name: &str) -> Result<(), FlpError> {
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "channel names cannot contain an embedded NUL character",
            ));
        }

        let channels = self.channels();
        let mut matching = channels.iter().filter(|channel| channel.id == channel_id);
        let Some(channel) = matching.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(channel_id));
        }

        let event_index = channel
            .event_range()
            .find(|index| self.events[*index].opcode == 0xCB)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected channel has no recognized 0xCB display-name event",
            ))?;
        let event = &self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected channel's 0xCB event is not a length-prefixed data event",
            ));
        }

        let old_payload = &event.payload;
        let suffix_start = old_payload
            .as_chunks::<2>()
            .0
            .iter()
            .position(|pair| u16::from_le_bytes([pair[0], pair[1]]) == 0)
            .map_or(old_payload.len(), |unit_index| (unit_index + 1) * 2);
        let mut replacement_payload = Vec::with_capacity(name.len().saturating_mul(2) + 2);
        for unit in name.encode_utf16() {
            replacement_payload.extend_from_slice(&unit.to_le_bytes());
        }
        replacement_payload.extend_from_slice(&[0, 0]);
        replacement_payload.extend_from_slice(&old_payload[suffix_start..]);

        let payload_length =
            u32::try_from(replacement_payload.len()).map_err(|_| FlpError::LengthOverflow)?;
        let length_prefix = encode_leb128(payload_length);
        let mut wire_bytes =
            Vec::with_capacity(1 + length_prefix.len() + replacement_payload.len());
        wire_bytes.push(0xCB);
        wire_bytes.extend_from_slice(&length_prefix);
        wire_bytes.extend_from_slice(&replacement_payload);

        let event = &mut self.events[event_index];
        event.payload = replacement_payload;
        event.encoding = PayloadEncoding::Data { length_prefix };
        event.wire_bytes = wire_bytes;
        self.refresh_event_offsets()?;
        Ok(())
    }

    pub fn trailing_bytes(&self) -> &[u8] {
        &self.trailing_bytes
    }

    pub fn opcode_counts(&self) -> [usize; 256] {
        let mut counts = [0usize; 256];
        for event in &self.events {
            counts[event.opcode as usize] += 1;
        }
        counts
    }

    /// Rebuilds the two-chunk container while keeping each event's original wire bytes.
    pub fn encode_lossless(&self) -> Result<Vec<u8>, FlpError> {
        let header_content_length = MIN_HEADER_CONTENT_LENGTH
            .checked_add(self.header.extension.len())
            .ok_or(FlpError::LengthOverflow)?;
        let header_length_u32 =
            u32::try_from(header_content_length).map_err(|_| FlpError::LengthOverflow)?;

        let event_stream_length = self.events.iter().try_fold(0usize, |total, event| {
            total
                .checked_add(event.wire_bytes.len())
                .ok_or(FlpError::LengthOverflow)
        })?;
        let event_length_u32 =
            u32::try_from(event_stream_length).map_err(|_| FlpError::LengthOverflow)?;

        let output_capacity = 8usize
            .checked_add(header_content_length)
            .and_then(|size| size.checked_add(8))
            .and_then(|size| size.checked_add(event_stream_length))
            .and_then(|size| size.checked_add(self.trailing_bytes.len()))
            .ok_or(FlpError::LengthOverflow)?;
        let mut output = Vec::with_capacity(output_capacity);
        output.extend_from_slice(FLHD);
        output.extend_from_slice(&header_length_u32.to_le_bytes());
        output.extend_from_slice(&self.header.format.to_le_bytes());
        output.extend_from_slice(&self.header.legacy_channel_count.to_le_bytes());
        output.extend_from_slice(&self.header.ppq.to_le_bytes());
        output.extend_from_slice(&self.header.extension);
        output.extend_from_slice(FLDT);
        output.extend_from_slice(&event_length_u32.to_le_bytes());
        for event in &self.events {
            output.extend_from_slice(&event.wire_bytes);
        }
        output.extend_from_slice(&self.trailing_bytes);
        Ok(output)
    }

    fn refresh_event_offsets(&mut self) -> Result<(), FlpError> {
        let header_content_length = MIN_HEADER_CONTENT_LENGTH
            .checked_add(self.header.extension.len())
            .ok_or(FlpError::LengthOverflow)?;
        let mut offset = 8usize
            .checked_add(header_content_length)
            .and_then(|value| value.checked_add(8))
            .ok_or(FlpError::LengthOverflow)?;
        for event in &mut self.events {
            event.file_offset = offset;
            offset = offset
                .checked_add(event.wire_bytes.len())
                .ok_or(FlpError::LengthOverflow)?;
        }
        Ok(())
    }

    fn is_pattern_note_event(event: &FlpEvent) -> bool {
        matches!(event.opcode, 0xD0 | 0xE0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlpError {
    UnexpectedEof {
        offset: usize,
        context: &'static str,
    },
    BadMagic {
        offset: usize,
        expected: &'static str,
    },
    InvalidHeaderLength(u32),
    MalformedVarint {
        offset: usize,
    },
    LengthOverflow,
    InvalidEvent {
        offset: usize,
        detail: &'static str,
    },
    UnsupportedEdit(&'static str),
    ChannelNotFound(u16),
    AmbiguousChannelId(u16),
}

impl fmt::Display for FlpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof { offset, context } => {
                write!(
                    formatter,
                    "unexpected end of file at byte {offset} while reading {context}"
                )
            }
            Self::BadMagic { offset, expected } => {
                write!(
                    formatter,
                    "invalid chunk marker at byte {offset}; expected {expected}"
                )
            }
            Self::InvalidHeaderLength(length) => {
                write!(
                    formatter,
                    "invalid FLhd content length {length}; expected at least 6"
                )
            }
            Self::MalformedVarint { offset } => {
                write!(
                    formatter,
                    "invalid or overlong event length at byte {offset}"
                )
            }
            Self::LengthOverflow => write!(formatter, "FLP length exceeds supported size"),
            Self::InvalidEvent { offset, detail } => {
                write!(formatter, "invalid event at byte {offset}: {detail}")
            }
            Self::UnsupportedEdit(detail) => write!(formatter, "cannot edit FLP field: {detail}"),
            Self::ChannelNotFound(channel_id) => {
                write!(
                    formatter,
                    "cannot edit FLP field: channel id {channel_id} was not found"
                )
            }
            Self::AmbiguousChannelId(channel_id) => write!(
                formatter,
                "cannot edit FLP field: channel id {channel_id} is not unique"
            ),
        }
    }
}

impl std::error::Error for FlpError {}

fn playlist_clip_record_size(
    project_version: Option<&str>,
    payload_length: usize,
    file_offset: usize,
) -> Result<usize, FlpError> {
    if let Some(major) = project_version
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
    {
        let expected = if major >= 25 {
            80
        } else if major >= 21 {
            60
        } else {
            32
        };
        if payload_length == 0 || payload_length.is_multiple_of(expected) {
            return Ok(expected);
        }
        let alternatives = FLP_PLAYLIST_RECORD_SIZES
            .into_iter()
            .filter(|size| payload_length.is_multiple_of(*size))
            .collect::<Vec<_>>();
        if alternatives.len() == 1 {
            return Ok(alternatives[0]);
        }
        return Err(FlpError::InvalidEvent {
            offset: file_offset,
            detail: "playlist clip payload size does not match the project version",
        });
    }

    if payload_length == 0 {
        return Ok(80);
    }
    let candidates = FLP_PLAYLIST_RECORD_SIZES
        .into_iter()
        .filter(|size| payload_length.is_multiple_of(*size))
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [record_size] => Ok(*record_size),
        [] => Err(FlpError::InvalidEvent {
            offset: file_offset,
            detail: "playlist clip payload is not divisible by a supported record size",
        }),
        _ => Err(FlpError::InvalidEvent {
            offset: file_offset,
            detail: "playlist clip record size is ambiguous without project version metadata",
        }),
    }
}

fn decode_playlist_clip(
    record: &[u8],
    source_event_index: usize,
    source_record_index: usize,
) -> PlaylistClip {
    debug_assert!(FLP_PLAYLIST_RECORD_SIZES.contains(&record.len()));
    let u16_at = |offset: usize| u16::from_le_bytes([record[offset], record[offset + 1]]);
    let u32_at = |offset: usize| {
        u32::from_le_bytes([
            record[offset],
            record[offset + 1],
            record[offset + 2],
            record[offset + 3],
        ])
    };
    let f32_at = |offset: usize| f32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
    let record_size = record.len();
    let clip_id = (record_size >= 60).then(|| u32_at(32));
    let reserved = match record_size {
        80 => record[36..64].to_vec(),
        60 => record[36..60].to_vec(),
        _ => Vec::new(),
    };
    let scale = (record_size >= 80).then(|| f64::from_le_bytes(record[64..72].try_into().unwrap()));
    let trailing_bytes = if record_size >= 80 {
        record[72..80].to_vec()
    } else {
        Vec::new()
    };

    PlaylistClip {
        position_ticks: u32_at(0),
        pattern_base: u16_at(4),
        item_index: u16_at(6),
        length_ticks: u32_at(8),
        raw_track_index: u16_at(12),
        track_index: 499u16.checked_sub(u16_at(12)),
        group: u16_at(14),
        unknown_word: u16_at(16),
        item_flags: u16_at(18),
        header_bytes: [record[20], record[21], record[22], record[23]],
        start_offset: f32_at(24),
        end_offset: f32_at(28),
        clip_id,
        reserved,
        scale,
        trailing_bytes,
        record_size,
        source_event_index,
        source_record_index,
    }
}

fn write_event_payload_bytes(
    event: &mut FlpEvent,
    payload_offset: usize,
    bytes: &[u8],
) -> Result<(), FlpError> {
    let payload_end = payload_offset
        .checked_add(bytes.len())
        .ok_or(FlpError::LengthOverflow)?;
    if event.payload.get(payload_offset..payload_end).is_none() {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "edited playlist field exceeds its source event",
        });
    }
    let prefix_length = match &event.encoding {
        PayloadEncoding::Data { length_prefix } => length_prefix.len(),
        _ => {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "playlist clip event does not have a data payload",
            });
        }
    };
    let wire_start = 1usize
        .checked_add(prefix_length)
        .and_then(|value| value.checked_add(payload_offset))
        .ok_or(FlpError::LengthOverflow)?;
    let wire_end = wire_start
        .checked_add(bytes.len())
        .ok_or(FlpError::LengthOverflow)?;
    if event.wire_bytes.get(wire_start..wire_end).is_none() {
        return Err(FlpError::InvalidEvent {
            offset: event.file_offset,
            detail: "playlist clip wire bytes do not match the decoded payload",
        });
    }
    event.payload[payload_offset..payload_end].copy_from_slice(bytes);
    event.wire_bytes[wire_start..wire_end].copy_from_slice(bytes);
    Ok(())
}

fn require_magic(bytes: &[u8], offset: usize, expected: &'static [u8; 4]) -> Result<(), FlpError> {
    let end = offset.checked_add(4).ok_or(FlpError::LengthOverflow)?;
    let Some(actual) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context: "chunk marker",
        });
    };
    if actual != expected {
        return Err(FlpError::BadMagic {
            offset,
            expected: if expected == FLHD { "FLhd" } else { "FLdt" },
        });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize, context: &'static str) -> Result<u16, FlpError> {
    let end = offset.checked_add(2).ok_or(FlpError::LengthOverflow)?;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize, context: &'static str) -> Result<u32, FlpError> {
    let end = offset.checked_add(4).ok_or(FlpError::LengthOverflow)?;
    let Some(slice) = bytes.get(offset..end) else {
        return Err(FlpError::UnexpectedEof {
            offset: bytes.len(),
            context,
        });
    };
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn detect_project_version(stream: &[u8]) -> Option<String> {
    let mut cursor = 0usize;
    for _ in 0..64 {
        let (event, next_cursor) = parse_event(stream, cursor, 0, false).ok()?;
        if event.opcode == 0xC7 {
            if let Some(version) = ascii_version(&event.payload) {
                return Some(version);
            }
        } else if event.opcode == 0xC0
            && let Some(version) = utf16_banner_version(&event.payload)
        {
            return Some(version);
        }
        cursor = next_cursor;
        if cursor >= stream.len() {
            return None;
        }
    }
    None
}

fn ascii_version(payload: &[u8]) -> Option<String> {
    let text = payload
        .split(|byte| *byte == 0)
        .next()
        .and_then(|part| std::str::from_utf8(part).ok())?;
    let valid = text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && text.contains('.')
        && text
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit());
    valid.then(|| text.to_owned())
}

fn decode_utf16_z(payload: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(payload.len() / 2);
    for pair in payload.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16(&units).ok()
}

fn project_string_version(version: Option<&str>) -> Option<(u32, u32)> {
    version.and_then(|version| {
        let mut parts = version.split('.');
        Some((
            parts.next()?.parse::<u32>().ok()?,
            parts.next()?.parse::<u32>().ok()?,
        ))
    })
}

fn uses_legacy_string_encoding(version: Option<&str>) -> bool {
    project_string_version(version)
        .is_some_and(|(major, minor)| major < 11 || (major == 11 && minor < 5))
}

fn decode_project_string(payload: &[u8], version: Option<&str>) -> Option<String> {
    let encoding_is_utf16 = project_string_version(version)
        .map(|(major, minor)| major > 11 || (major == 11 && minor >= 5))
        .unwrap_or_else(|| {
            payload
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|pair| pair[1] == 0)
                .count()
                >= 2
        });

    if encoding_is_utf16 {
        decode_utf16_z(payload)
    } else {
        decode_windows_1252_z(payload)
    }
}

fn decode_windows_1252_z(payload: &[u8]) -> Option<String> {
    let text = payload.split(|byte| *byte == 0).next()?;
    Some(text.iter().copied().map(windows_1252_char).collect())
}

fn windows_1252_char(byte: u8) -> char {
    const EXTENDED: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    if (0x80..=0x9F).contains(&byte) {
        EXTENDED[usize::from(byte - 0x80)]
    } else {
        char::from(byte)
    }
}

fn utf16_banner_version(payload: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(payload.len() / 2);
    for pair in payload.as_chunks::<2>().0 {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let text = String::from_utf16(&units).ok()?;
    if !text.starts_with("FL Studio") {
        return None;
    }
    let start = text
        .char_indices()
        .find_map(|(index, ch)| ch.is_ascii_digit().then_some(index))?;
    let suffix = &text[start..];
    let version: String = suffix
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    (version.contains('.') && version.bytes().next().is_some_and(|b| b.is_ascii_digit()))
        .then_some(version)
}

fn read_project_metadata(events: &[FlpEvent]) -> ProjectMetadata {
    let mut modern_tempo = None;
    let mut legacy_coarse_tempo = None;
    let mut legacy_fine_tempo = 0u32;
    let mut numerator = None;
    let mut denominator = None;
    let mut build_number = None;

    for event in events {
        match event.opcode {
            0x11 if event.payload.len() == 1 => numerator = Some(event.payload[0]),
            0x12 if event.payload.len() == 1 => denominator = Some(event.payload[0]),
            0x42 if event.payload.len() == 2 => {
                legacy_coarse_tempo = Some(u32::from(u16::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                ])));
            }
            0x5D if event.payload.len() == 2 => {
                legacy_fine_tempo =
                    u32::from(u16::from_le_bytes([event.payload[0], event.payload[1]]));
            }
            0x9C if event.payload.len() == 4 => {
                modern_tempo = Some(u32::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                    event.payload[2],
                    event.payload[3],
                ]));
            }
            0x9F if event.payload.len() == 4 => {
                build_number = Some(u32::from_le_bytes([
                    event.payload[0],
                    event.payload[1],
                    event.payload[2],
                    event.payload[3],
                ]));
            }
            _ => {}
        }
    }

    let time_signature = numerator.zip(denominator);
    let tempo_milli_bpm = modern_tempo.or_else(|| {
        legacy_coarse_tempo.map(|coarse| {
            coarse
                .saturating_mul(1000)
                .saturating_add(legacy_fine_tempo)
        })
    });

    ProjectMetadata {
        tempo_milli_bpm,
        time_signature,
        build_number,
    }
}

fn parse_event(
    stream: &[u8],
    start: usize,
    absolute_start: usize,
    modern_ac_event: bool,
) -> Result<(FlpEvent, usize), FlpError> {
    let Some(&opcode) = stream.get(start) else {
        return Err(FlpError::UnexpectedEof {
            offset: absolute_start + start,
            context: "event opcode",
        });
    };

    let (encoding, payload_start, payload_length) = if opcode == 0xAC && modern_ac_event {
        (PayloadEncoding::FixedThreeBytes, start + 1, 3usize)
    } else if opcode <= 0x3F {
        (PayloadEncoding::Byte, start + 1, 1usize)
    } else if opcode <= 0x7F {
        (PayloadEncoding::Word, start + 1, 2usize)
    } else if opcode <= 0xBF {
        (PayloadEncoding::Dword, start + 1, 4usize)
    } else {
        let (length, prefix_end) = read_leb128(stream, start + 1, absolute_start)?;
        let prefix = stream[start + 1..prefix_end].to_vec();
        (
            PayloadEncoding::Data {
                length_prefix: prefix,
            },
            prefix_end,
            usize::try_from(length).map_err(|_| FlpError::LengthOverflow)?,
        )
    };

    let event_end = payload_start
        .checked_add(payload_length)
        .ok_or(FlpError::LengthOverflow)?;
    if event_end > stream.len() {
        return Err(FlpError::UnexpectedEof {
            offset: absolute_start + stream.len(),
            context: "event payload",
        });
    }

    Ok((
        FlpEvent {
            opcode,
            payload: stream[payload_start..event_end].to_vec(),
            encoding,
            wire_bytes: stream[start..event_end].to_vec(),
            file_offset: absolute_start + start,
        },
        event_end,
    ))
}

fn read_leb128(
    bytes: &[u8],
    start: usize,
    absolute_start: usize,
) -> Result<(u32, usize), FlpError> {
    let mut value = 0u32;
    let mut cursor = start;
    for shift in (0..35).step_by(7) {
        let Some(&byte) = bytes.get(cursor) else {
            return Err(FlpError::UnexpectedEof {
                offset: absolute_start + cursor,
                context: "event length",
            });
        };
        cursor += 1;
        if shift == 28 && byte & 0xF0 != 0 {
            return Err(FlpError::MalformedVarint {
                offset: absolute_start + start,
            });
        }
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }
    Err(FlpError::MalformedVarint {
        offset: absolute_start + start,
    })
}

fn encode_leb128(mut value: u32) -> Vec<u8> {
    let mut output = Vec::with_capacity(5);
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return output;
        }
    }
}

fn parse_vst_plugin_state_metadata(payload: &[u8]) -> Option<VstPluginStateMetadata> {
    let marker = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?);
    if !matches!(marker, 8 | 10 | 12) {
        return None;
    }

    let mut metadata = VstPluginStateMetadata {
        format_marker: marker,
        ..VstPluginStateMetadata::default()
    };
    let mut cursor = 4usize;
    let mut field_count = 0usize;
    while cursor < payload.len() {
        let id_end = cursor.checked_add(4)?;
        let length_end = cursor.checked_add(12)?;
        let id = u32::from_le_bytes(payload.get(cursor..id_end)?.try_into().ok()?);
        let length = u64::from_le_bytes(payload.get(id_end..length_end)?.try_into().ok()?);
        let length = usize::try_from(length).ok()?;
        let data_end = length_end.checked_add(length)?;
        let data = payload.get(length_end..data_end)?;
        field_count += 1;

        match id {
            50 if metadata.plugin_info.is_none() => metadata.plugin_info = Some(data.to_vec()),
            51 if metadata.fourcc.is_none() => metadata.fourcc = decode_vst_text(data),
            52 if metadata.guid.is_none() => metadata.guid = Some(data.to_vec()),
            53 if metadata.state_bytes.is_none() => metadata.state_bytes = Some(data.len()),
            54 if metadata.name.is_none() => metadata.name = decode_vst_text(data),
            55 if metadata.path.is_none() => metadata.path = decode_vst_text(data),
            56 if metadata.vendor.is_none() => metadata.vendor = decode_vst_text(data),
            _ => {}
        }
        cursor = data_end;
    }

    (field_count > 0).then_some(metadata)
}

fn decode_vst_text(bytes: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(bytes)
        .ok()?
        .trim_end_matches('\0')
        .to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{FlpDocument, FlpError, PayloadEncoding, parse_vst_plugin_state_metadata};

    fn flp_fixture(event_stream: &[u8], header_extension: &[u8], trailing: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FLhd");
        bytes.extend_from_slice(&(6u32 + header_extension.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&96u16.to_le_bytes());
        bytes.extend_from_slice(header_extension);
        bytes.extend_from_slice(b"FLdt");
        bytes.extend_from_slice(&(event_stream.len() as u32).to_le_bytes());
        bytes.extend_from_slice(event_stream);
        bytes.extend_from_slice(trailing);
        bytes
    }

    fn append_vst_field(payload: &mut Vec<u8>, id: u32, data: &[u8]) {
        payload.extend_from_slice(&id.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(data);
    }

    #[test]
    fn decodes_vst_identity_fields_and_class_uid_from_d5_envelope() {
        let mut payload = 12u32.to_le_bytes().to_vec();
        append_vst_field(&mut payload, 50, &[0; 16]);
        append_vst_field(
            &mut payload,
            52,
            &[
                0xDF, 0x55, 0x47, 0x32, 0xDF, 0x8F, 0x88, 0x47, 0xB4, 0xCE, 0x5B, 0x70, 0xA8, 0x03,
                0x7E, 0xC4,
            ],
        );
        append_vst_field(&mut payload, 54, b"ZENOLOGY");
        append_vst_field(
            &mut payload,
            55,
            b"/Library/Audio/Plug-Ins/VST3/Roland/ZENOLOGY.vst3",
        );
        append_vst_field(&mut payload, 56, b"Roland Cloud");
        append_vst_field(&mut payload, 53, &[1, 2, 3, 4]);
        append_vst_field(&mut payload, 999, &[5, 6]);

        let metadata = parse_vst_plugin_state_metadata(&payload)
            .expect("the VST envelope should be recognized");

        assert_eq!(metadata.format_marker(), 12);
        assert_eq!(metadata.name(), Some("ZENOLOGY"));
        assert_eq!(metadata.vendor(), Some("Roland Cloud"));
        assert_eq!(
            metadata.path(),
            Some("/Library/Audio/Plug-Ins/VST3/Roland/ZENOLOGY.vst3")
        );
        assert_eq!(metadata.state_bytes(), Some(4));
        assert_eq!(
            metadata.class_uid().as_deref(),
            Some("324755DF8FDF4788B4CE5B70A8037EC4")
        );
    }

    #[test]
    fn ignores_unsupported_or_truncated_vst_envelopes() {
        assert!(parse_vst_plugin_state_metadata(&42u32.to_le_bytes()).is_none());

        let mut truncated = 12u32.to_le_bytes().to_vec();
        truncated.extend_from_slice(&54u32.to_le_bytes());
        truncated.extend_from_slice(&8u64.to_le_bytes());
        truncated.push(b'X');
        assert!(parse_vst_plugin_state_metadata(&truncated).is_none());
    }

    #[test]
    fn lossless_roundtrip_preserves_unknown_event_bytes_and_container_extensions() {
        let event_stream = [0xFF, 0x82, 0x00, 0xA1, 0xB2];
        let header_extension = [0xC3, 0xD4];
        let trailing = [0xE5, 0xF6];
        let input = flp_fixture(&event_stream, &header_extension, &trailing);

        let document = FlpDocument::parse(&input).expect("fixture should parse");

        assert_eq!(document.header().ppq(), 96);
        assert_eq!(document.header().extension(), &header_extension);
        assert_eq!(document.events().len(), 1);
        assert_eq!(document.events()[0].payload(), &[0xA1, 0xB2]);
        assert_eq!(
            document.events()[0].encoding(),
            &PayloadEncoding::Data {
                length_prefix: vec![0x82, 0x00]
            }
        );
        assert_eq!(document.trailing_bytes(), &trailing);
        assert_eq!(document.encode_lossless().unwrap(), input);
    }

    #[test]
    fn invalid_flp_magic_is_reported() {
        let error = FlpDocument::parse(b"not-an-flp").expect_err("bad magic must fail");
        assert!(matches!(error, FlpError::BadMagic { .. }));
    }
}

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

pub mod audio;
pub mod media;
pub mod midi;
pub mod plugins;
pub mod project_package;
pub mod sample_render;
pub mod vst3;

const FLHD: &[u8; 4] = b"FLhd";
const FLDT: &[u8; 4] = b"FLdt";
const MIN_HEADER_CONTENT_LENGTH: usize = 6;
const FLP_NOTE_RECORD_SIZE: usize = 24;
const FLP_PATTERN_CONTROLLER_RECORD_SIZE: usize = 12;
const FLP_PLAYLIST_RECORD_SIZES: [usize; 4] = [88, 80, 60, 32];
const MAX_MERGED_PATTERN_NOTES: usize = 2_000_000;
const FLP_AUTOMATION_COUNT_OFFSET: usize = 17;
const FLP_AUTOMATION_POINTS_OFFSET: usize = 21;
const FLP_AUTOMATION_POINT_SIZE: usize = 24;
const TIME_MARKER_SIGNATURE_BIT: u32 = 0x0800_0000;
const TIME_MARKER_TICK_MASK: u32 = 0x07FF_FFFF;
const PROJECT_INFO_STRING_EVENTS: [u8; 5] = [0xC2, 0xCE, 0xCF, 0xC3, 0xC5];

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

    fn replace_data_payload(&mut self, payload: Vec<u8>) -> Result<(), FlpError> {
        if !matches!(self.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a length-prefixed data payload",
            });
        }
        let payload_length = u32::try_from(payload.len()).map_err(|_| FlpError::LengthOverflow)?;
        let original_payload_length = self.payload.len();
        let length_prefix = match &self.encoding {
            PayloadEncoding::Data { length_prefix } if payload.len() == original_payload_length => {
                length_prefix.clone()
            }
            PayloadEncoding::Data { .. } => encode_leb128(payload_length),
            _ => unreachable!("data encoding was checked above"),
        };
        let mut wire_bytes = Vec::with_capacity(1 + length_prefix.len() + payload.len());
        wire_bytes.push(self.opcode);
        wire_bytes.extend_from_slice(&length_prefix);
        wire_bytes.extend_from_slice(&payload);
        self.payload = payload;
        self.encoding = PayloadEncoding::Data { length_prefix };
        self.wire_bytes = wire_bytes;
        Ok(())
    }

    fn new_data(opcode: u8, payload: Vec<u8>) -> Result<Self, FlpError> {
        let mut event = Self {
            opcode,
            payload: Vec::new(),
            encoding: PayloadEncoding::Data {
                length_prefix: encode_leb128(0),
            },
            wire_bytes: Vec::new(),
            file_offset: 0,
        };
        event.replace_data_payload(payload)?;
        Ok(event)
    }

    fn new_word(opcode: u8, value: u16) -> Self {
        let payload = value.to_le_bytes().to_vec();
        let mut wire_bytes = Vec::with_capacity(3);
        wire_bytes.push(opcode);
        wire_bytes.extend_from_slice(&payload);
        Self {
            opcode,
            payload,
            encoding: PayloadEncoding::Word,
            wire_bytes,
            file_offset: 0,
        }
    }

    fn new_dword(opcode: u8, value: u32) -> Self {
        let payload = value.to_le_bytes().to_vec();
        let mut wire_bytes = Vec::with_capacity(5);
        wire_bytes.push(opcode);
        wire_bytes.extend_from_slice(&payload);
        Self {
            opcode,
            payload,
            encoding: PayloadEncoding::Dword,
            wire_bytes,
            file_offset: 0,
        }
    }

    fn new_byte(opcode: u8, value: u8) -> Self {
        Self {
            opcode,
            payload: vec![value],
            encoding: PayloadEncoding::Byte,
            wire_bytes: vec![opcode, value],
            file_offset: 0,
        }
    }

    fn replace_byte_payload(&mut self, value: u8) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Byte || self.payload.len() != 1 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a one-byte payload",
            });
        }
        let Some(wire_payload) = self.wire_bytes.get_mut(1) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "byte event does not contain its payload",
            });
        };
        *wire_payload = value;
        self.payload[0] = value;
        Ok(())
    }

    fn replace_word_payload(&mut self, value: u16) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Word || self.payload.len() != 2 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a two-byte payload",
            });
        }
        let encoded = value.to_le_bytes();
        let Some(wire_payload) = self.wire_bytes.get_mut(1..3) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "word event does not contain its payload",
            });
        };
        wire_payload.copy_from_slice(&encoded);
        self.payload.copy_from_slice(&encoded);
        Ok(())
    }

    fn replace_dword_payload(&mut self, value: u32) -> Result<(), FlpError> {
        if self.encoding != PayloadEncoding::Dword || self.payload.len() != 4 {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "event does not have a four-byte payload",
            });
        }
        let encoded = value.to_le_bytes();
        let Some(wire_payload) = self.wire_bytes.get_mut(1..5) else {
            return Err(FlpError::InvalidEvent {
                offset: self.file_offset,
                detail: "dword event does not contain its payload",
            });
        };
        wire_payload.copy_from_slice(&encoded);
        self.payload.copy_from_slice(&encoded);
        Ok(())
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewProjectMidiImport {
    pub project: FlpDocument,
    pub imported_notes: usize,
    pub destinations: Vec<(u16, u16)>,
}

#[derive(Clone, Copy)]
struct MidiTrackImportSettings {
    import_time_signatures: bool,
    options: midi::MidiImportOptions,
    realign_origin_tick: Option<u64>,
}

/// The state-file variant identified by the FL Studio `FLhd` format field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FstPresetKind {
    AutomationState,
    ChannelState,
    NativePluginState,
    VstGeneratorState,
    VstEffectState,
    MixerInsertState,
    UnknownFormat(u16),
}

impl FstPresetKind {
    pub fn from_format(format: u16) -> Self {
        match format {
            24 => Self::AutomationState,
            32 => Self::ChannelState,
            48 => Self::NativePluginState,
            49 => Self::VstGeneratorState,
            50 => Self::VstEffectState,
            64 => Self::MixerInsertState,
            other => Self::UnknownFormat(other),
        }
    }
}

impl fmt::Display for FstPresetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AutomationState => formatter.write_str("automation state"),
            Self::ChannelState => formatter.write_str("channel state"),
            Self::NativePluginState => formatter.write_str("native plug-in state"),
            Self::VstGeneratorState => formatter.write_str("VST generator state"),
            Self::VstEffectState => formatter.write_str("VST effect state"),
            Self::MixerInsertState => formatter.write_str("Mixer insert state"),
            Self::UnknownFormat(format) => write!(formatter, "unknown state format ({format})"),
        }
    }
}

/// A lossless FL Studio state preset with its container variant identified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FstPreset {
    kind: FstPresetKind,
    document: FlpDocument,
}

impl FstPreset {
    pub fn parse(bytes: &[u8]) -> Result<Self, FlpError> {
        let document = FlpDocument::parse(bytes)?;
        let kind = FstPresetKind::from_format(document.header().format());
        Ok(Self { kind, document })
    }

    pub fn kind(&self) -> FstPresetKind {
        self.kind
    }

    pub fn document(&self) -> &FlpDocument {
        &self.document
    }

    /// Finds a supported VST3 channel state in a saved channel preset that matches `target`.
    ///
    /// Only format-32 channel states using FL's marker-12 VST3 wrapper are accepted. Matching
    /// the class UID before exposing the state prevents a preset for another instrument from
    /// being restored into the target channel.
    pub fn matching_vst3_channel_state(
        &self,
        target: &ChannelPluginState,
    ) -> Option<ChannelPluginState> {
        if self.kind != FstPresetKind::ChannelState {
            return None;
        }
        let target_metadata = target.vst_metadata()?;
        if target_metadata.format_marker() != 12
            || target_metadata.fourcc().is_some()
            || target.vst_state_bytes().is_none()
        {
            return None;
        }
        let target_uid = target_metadata.class_uid()?;

        self.document
            .channel_plugin_states()
            .into_iter()
            .find(|state| {
                let Some(metadata) = state.vst_metadata() else {
                    return false;
                };
                metadata.format_marker() == 12
                    && metadata.fourcc().is_none()
                    && state.vst_state_bytes().is_some()
                    && metadata
                        .class_uid()
                        .is_some_and(|uid| uid.eq_ignore_ascii_case(&target_uid))
            })
    }

    pub fn into_document(self) -> FlpDocument {
        self.document
    }

    pub fn encode_lossless(&self) -> Result<Vec<u8>, FlpError> {
        self.document.encode_lossless()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectMetadata {
    tempo_milli_bpm: Option<u32>,
    time_signature: Option<(u8, u8)>,
    global_swing_mix: Option<u8>,
    pan_law_raw: Option<u8>,
    master_pitch_cents: Option<i16>,
    build_number: Option<u32>,
    title: Option<String>,
    author: Option<String>,
    comments: Option<String>,
    genre: Option<String>,
    web_link: Option<String>,
}

/// Fields that can be changed in FL Studio's Project Info dialog.
///
/// A `None` field is left unchanged. `Some("")` clears that field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectInfoEdit {
    pub title: Option<String>,
    pub author: Option<String>,
    pub comments: Option<String>,
    pub genre: Option<String>,
    pub web_link: Option<String>,
}

/// Project settings verified against FL Studio 26.1.6 saves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettings {
    pub play_truncated_notes_in_clips: bool,
    pub fast_declick_for_cut_groups: bool,
}

/// Fields that can be changed in the supported Project settings subset.
///
/// A `None` field is left unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettingsEdit {
    pub play_truncated_notes_in_clips: Option<bool>,
    pub fast_declick_for_cut_groups: Option<bool>,
    /// Raw project pan-law value from the global `0x17` byte event.
    pub pan_law_raw: Option<u8>,
    /// Project time-signature numerator and denominator from global `0x11`/`0x12` events.
    pub time_signature: Option<(u8, u8)>,
    /// Master pitch in cents from the global signed-word `0x50` event.
    pub master_pitch_cents: Option<i16>,
}

/// A known FL channel kind, while retaining unrecognized raw values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelType {
    Sampler,
    Native,
    Layer,
    Instrument,
    Automation,
    Unknown(u8),
}

/// Channel Rack ordering used by [`FlpDocument::sort_channels`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelSortOrder {
    Color,
    MixerTrack,
    Name,
    Type,
}

impl ChannelType {
    pub fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Sampler,
            2 => Self::Native,
            3 => Self::Layer,
            4 => Self::Instrument,
            5 => Self::Automation,
            other => Self::Unknown(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            Self::Sampler => 0,
            Self::Native => 2,
            Self::Layer => 3,
            Self::Instrument => 4,
            Self::Automation => 5,
            Self::Unknown(value) => value,
        }
    }
}

/// The ordered target represented by one Sampler Envelope/LFO (`0xDA`) record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(usize)]
pub enum SamplerEnvelopeLfoTarget {
    Panning = 0,
    Volume = 1,
    ModX = 2,
    ModY = 3,
    Pitch = 4,
}

impl SamplerEnvelopeLfoTarget {
    pub const fn all() -> [Self; 5] {
        [
            Self::Panning,
            Self::Volume,
            Self::ModX,
            Self::ModY,
            Self::Pitch,
        ]
    }

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Panning => "Panning",
            Self::Volume => "Volume",
            Self::ModX => "Mod X",
            Self::ModY => "Mod Y",
            Self::Pitch => "Pitch",
        }
    }

    fn from_index(index: usize) -> Option<Self> {
        Self::all().get(index).copied()
    }
}

/// Decoded raw fields from one of the Sampler's five ordered `0xDA` records.
///
/// The record order is Panning, Volume, Mod X, Mod Y, and Pitch. Time and
/// level values remain in their native raw units; audio modulation behavior
/// and display curves are separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerEnvelopeLfo {
    target: SamplerEnvelopeLfoTarget,
    flags_raw: i32,
    enabled: bool,
    predelay_raw: Option<i32>,
    attack_raw: Option<i32>,
    hold_raw: Option<i32>,
    decay_raw: Option<i32>,
    sustain_raw: Option<i32>,
    release_raw: Option<i32>,
    amount_raw: Option<i32>,
    attack_tension_raw: Option<i32>,
    decay_tension_raw: Option<i32>,
    release_tension_raw: Option<i32>,
    lfo_predelay_raw: Option<u32>,
    lfo_attack_raw: Option<u32>,
    lfo_amount_raw: Option<i32>,
    lfo_speed_raw: Option<u32>,
    lfo_shape_raw: Option<i32>,
}

impl SamplerEnvelopeLfo {
    pub fn target(&self) -> SamplerEnvelopeLfoTarget {
        self.target
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn tempo_sync_enabled(&self) -> bool {
        self.flags_raw & 1 != 0
    }

    pub fn lfo_tempo_sync_enabled(&self) -> bool {
        self.flags_raw & (1 << 1) != 0
    }

    pub fn lfo_phase_retrigger_enabled(&self) -> bool {
        self.flags_raw & (1 << 5) != 0
    }

    pub fn flags_raw(&self) -> i32 {
        self.flags_raw
    }

    pub fn predelay_raw(&self) -> Option<i32> {
        self.predelay_raw
    }

    pub fn attack_raw(&self) -> Option<i32> {
        self.attack_raw
    }

    pub fn hold_raw(&self) -> Option<i32> {
        self.hold_raw
    }

    pub fn decay_raw(&self) -> Option<i32> {
        self.decay_raw
    }

    pub fn sustain_raw(&self) -> Option<i32> {
        self.sustain_raw
    }

    pub fn release_raw(&self) -> Option<i32> {
        self.release_raw
    }

    pub fn amount_raw(&self) -> Option<i32> {
        self.amount_raw
    }

    pub fn attack_tension_raw(&self) -> Option<i32> {
        self.attack_tension_raw
    }

    pub fn decay_tension_raw(&self) -> Option<i32> {
        self.decay_tension_raw
    }

    pub fn release_tension_raw(&self) -> Option<i32> {
        self.release_tension_raw
    }

    pub fn lfo_predelay_raw(&self) -> Option<u32> {
        self.lfo_predelay_raw
    }

    pub fn lfo_attack_raw(&self) -> Option<u32> {
        self.lfo_attack_raw
    }

    pub fn lfo_amount_raw(&self) -> Option<i32> {
        self.lfo_amount_raw
    }

    pub fn lfo_speed_raw(&self) -> Option<u32> {
        self.lfo_speed_raw
    }

    /// Raw LFO shape code: 0 Sine, 1 Triangle, or 2 Pulse.
    pub fn lfo_shape_raw(&self) -> Option<i32> {
        self.lfo_shape_raw
    }
}

/// Backwards-compatible name for the Volume target record.
pub type SamplerVolumeEnvelope = SamplerEnvelopeLfo;

/// Lossless edits to recognized Sampler Envelope/LFO fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SamplerEnvelopeLfoEdit {
    pub enabled: Option<bool>,
    pub tempo_sync: Option<bool>,
    pub predelay_raw: Option<i32>,
    pub attack_raw: Option<i32>,
    pub hold_raw: Option<i32>,
    pub decay_raw: Option<i32>,
    pub sustain_raw: Option<i32>,
    pub release_raw: Option<i32>,
    pub amount_raw: Option<i32>,
    pub attack_tension_raw: Option<i32>,
    pub decay_tension_raw: Option<i32>,
    pub release_tension_raw: Option<i32>,
    pub lfo_tempo_sync: Option<bool>,
    pub lfo_phase_retrigger: Option<bool>,
    pub lfo_predelay_raw: Option<u32>,
    pub lfo_attack_raw: Option<u32>,
    pub lfo_amount_raw: Option<i32>,
    pub lfo_speed_raw: Option<u32>,
    pub lfo_shape_raw: Option<i32>,
}

/// Backwards-compatible edit type for the Volume target.
pub type SamplerVolumeEnvelopeEdit = SamplerEnvelopeLfoEdit;

fn decode_sampler_envelope_lfo(
    payload: &[u8],
    target: SamplerEnvelopeLfoTarget,
) -> Option<SamplerEnvelopeLfo> {
    let i32_at = |offset: usize| {
        Some(i32::from_le_bytes(
            payload
                .get(offset..offset.checked_add(4)?)?
                .try_into()
                .ok()?,
        ))
    };
    let u32_at = |offset: usize| {
        Some(u32::from_le_bytes(
            payload
                .get(offset..offset.checked_add(4)?)?
                .try_into()
                .ok()?,
        ))
    };
    Some(SamplerEnvelopeLfo {
        target,
        flags_raw: i32_at(0)?,
        enabled: i32_at(4)? != 0,
        predelay_raw: i32_at(8),
        attack_raw: i32_at(12),
        hold_raw: i32_at(16),
        decay_raw: i32_at(20),
        sustain_raw: i32_at(24),
        release_raw: i32_at(28),
        amount_raw: i32_at(32),
        attack_tension_raw: i32_at(56),
        decay_tension_raw: i32_at(60),
        release_tension_raw: i32_at(64),
        lfo_predelay_raw: u32_at(36),
        lfo_attack_raw: u32_at(40),
        lfo_amount_raw: i32_at(44),
        lfo_speed_raw: u32_at(48),
        lfo_shape_raw: i32_at(52),
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChannelSummary {
    id: u16,
    kind: Option<u8>,
    enabled: Option<bool>,
    zipped: bool,
    color: Option<u32>,
    mixer_track: Option<i8>,
    sampler_fx_flags: Option<u16>,
    sampler_fade_out_raw: Option<u16>,
    sampler_fade_in_raw: Option<u16>,
    sampler_envelope_lfos: [Option<SamplerEnvelopeLfo>; 5],
    sampler_max_polyphony_raw: Option<u32>,
    sampler_mono_enabled: Option<bool>,
    channel_cut_groups: Option<(u16, u16)>,
    envelope_lfo_events_seen: u8,
    sampler_flags: Option<u32>,
    sampler_root_note: Option<u32>,
    keyboard_fine_tune_raw: Option<i32>,
    keyboard_main_pitch: Option<bool>,
    keyboard_add_root: Option<bool>,
    keyboard_key_region: Option<(u32, u32)>,
    layer_fade_raw: Option<u32>,
    stretch_settings: Option<ChannelStretchSettings>,
    ping_pong_loop: bool,
    swing_mix: Option<u16>,
    group_number: Option<i32>,
    volume: Option<u32>,
    pan: Option<i32>,
    levels_editable: bool,
    volume_priority: u8,
    pan_priority: u8,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    sample_path: Option<String>,
    layer_children: Vec<u16>,
    layer_flags: Option<u32>,
    level_adjustments: Vec<ChannelLevelAdjustments>,
    first_event_index: usize,
    end_event_index: usize,
}

/// Raw fields decoded from one FLP `0xD5` channel Level Adjusts event.
///
/// The stored units and playback effect are not yet interpreted. The complete
/// event payload remains available unchanged through [`FlpDocument::events`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelLevelAdjustments {
    pan: i32,
    volume: u32,
    unknown: u32,
    mod_x: i32,
    mod_y: i32,
}

/// Raw fields that can be changed on one existing channel `0xD5` Level Adjusts event.
/// Fields set to `None` remain unchanged; the unknown field and trailing payload bytes are
/// always preserved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChannelLevelAdjustmentsEdit {
    pub pan_raw: Option<i32>,
    pub volume_raw: Option<u32>,
    pub mod_x_raw: Option<i32>,
    pub mod_y_raw: Option<i32>,
}

/// Raw time-stretch settings from the channel Parameters (`0xC7`) payload.
///
/// The four values are retained in FL Studio's stored units. Their field offsets and
/// stretch-mode codes follow the independently documented PyFLP channel layout; this
/// project does not yet apply them during audio playback or rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelStretchSettings {
    time_raw: u32,
    pitch_raw: i32,
    multiplier_raw: i32,
    mode_raw: i32,
}

/// Raw time-stretch fields that can be changed on an existing channel Parameters event.
/// Fields set to `None` remain unchanged. The edit requires one unambiguous Parameters
/// event containing all four fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChannelStretchSettingsEdit {
    pub time_raw: Option<u32>,
    pub pitch_raw: Option<i32>,
    pub multiplier_raw: Option<i32>,
    pub mode_raw: Option<i32>,
}

/// Known FL Studio sample time-stretch modes stored in Channel Parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelStretchMode {
    Stretch,
    Resample,
    Elastique3Generic,
    Elastique3Mono,
    SliceStretch,
    SliceMap,
    Auto,
    Elastique2Generic,
    Elastique2Transient,
    Elastique2Mono,
    Elastique2Speech,
    Unknown(i32),
}

impl ChannelStretchMode {
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            -1 => Self::Stretch,
            0 => Self::Resample,
            1 => Self::Elastique3Generic,
            2 => Self::Elastique3Mono,
            3 => Self::SliceStretch,
            4 => Self::SliceMap,
            5 => Self::Auto,
            6 => Self::Elastique2Generic,
            7 => Self::Elastique2Transient,
            8 => Self::Elastique2Mono,
            9 => Self::Elastique2Speech,
            other => Self::Unknown(other),
        }
    }

    pub fn raw(self) -> i32 {
        match self {
            Self::Stretch => -1,
            Self::Resample => 0,
            Self::Elastique3Generic => 1,
            Self::Elastique3Mono => 2,
            Self::SliceStretch => 3,
            Self::SliceMap => 4,
            Self::Auto => 5,
            Self::Elastique2Generic => 6,
            Self::Elastique2Transient => 7,
            Self::Elastique2Mono => 8,
            Self::Elastique2Speech => 9,
            Self::Unknown(raw) => raw,
        }
    }
}

impl ChannelLevelAdjustments {
    fn decode(payload: &[u8]) -> Option<Self> {
        (payload.len() >= 20).then(|| Self {
            pan: i32::from_le_bytes(payload[0..4].try_into().expect("four-byte pan field")),
            volume: u32::from_le_bytes(payload[4..8].try_into().expect("four-byte volume field")),
            unknown: u32::from_le_bytes(
                payload[8..12].try_into().expect("four-byte unknown field"),
            ),
            mod_x: i32::from_le_bytes(payload[12..16].try_into().expect("four-byte Mod X field")),
            mod_y: i32::from_le_bytes(payload[16..20].try_into().expect("four-byte Mod Y field")),
        })
    }

    pub fn pan_raw(&self) -> i32 {
        self.pan
    }

    pub fn volume_raw(&self) -> u32 {
        self.volume
    }

    pub fn unknown_raw(&self) -> u32 {
        self.unknown
    }

    pub fn mod_x_raw(&self) -> i32 {
        self.mod_x
    }

    pub fn mod_y_raw(&self) -> i32 {
        self.mod_y
    }
}

impl ChannelStretchSettings {
    pub fn time_raw(&self) -> u32 {
        self.time_raw
    }

    pub fn pitch_raw(&self) -> i32 {
        self.pitch_raw
    }

    pub fn multiplier_raw(&self) -> i32 {
        self.multiplier_raw
    }

    pub fn mode_raw(&self) -> i32 {
        self.mode_raw
    }

    pub fn mode(&self) -> ChannelStretchMode {
        ChannelStretchMode::from_raw(self.mode_raw)
    }
}

/// A named Channel Rack display filter from the project-level `0xE7` events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelGroupSummary {
    index: i32,
    name: Option<String>,
}

impl ChannelGroupSummary {
    /// Zero-based group index referenced by channel `0x91` events.
    pub fn index(&self) -> i32 {
        self.index
    }

    /// Group name, if its text event could be decoded.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
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
/// this structure decodes the length-prefixed identity fields and state-field byte range.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VstPluginStateMetadata {
    format_marker: u32,
    plugin_info: Option<Vec<u8>>,
    fourcc: Option<String>,
    guid: Option<Vec<u8>>,
    name: Option<String>,
    path: Option<String>,
    vendor: Option<String>,
    state_data_range: Option<std::ops::Range<usize>>,
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

    /// Nested state bytes from the recognized VST field 53, when present.
    /// The returned slice points into the original `0xD5` data payload.
    pub fn vst_state_bytes(&self) -> Option<&[u8]> {
        let range = self.vst_metadata.as_ref()?.state_data_range.as_ref()?;
        self.data_payload.get(range.clone())
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
        self.state_data_range
            .as_ref()
            .map(|range| range.end - range.start)
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

    pub fn channel_type(&self) -> Option<ChannelType> {
        self.kind.map(ChannelType::from_raw)
    }

    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Whether FL Studio saved this channel in compact (zipped) Channel Rack mode.
    /// A missing `0x0F` event defaults to false.
    pub fn zipped(&self) -> bool {
        self.zipped
    }

    /// Whether a kind-0 Sampler's saved Reverse option is enabled.
    ///
    /// The Reverse bit is bit 1 of the channel's `0x46` FX flags word. Unknown
    /// flag bits remain untouched in the original event stream.
    pub fn sample_reversed(&self) -> bool {
        self.kind == Some(0)
            && self
                .sampler_fx_flags
                .is_some_and(|flags| flags & (1 << 1) != 0)
    }

    /// Saved raw Sampler precomputed Fade Out value (`0x4B`), when present.
    /// FL Studio documents a 0..=1024 range. Supported Sampler rendering applies a provisional
    /// linear-amplitude fade across the source sample; exact native curve comparison is open.
    pub fn sampler_fade_out_raw(&self) -> Option<u16> {
        (self.kind == Some(0))
            .then_some(self.sampler_fade_out_raw)
            .flatten()
    }

    /// Saved raw Sampler precomputed Fade In value (`0x4C`), when present.
    /// FL Studio documents a 0..=1024 range. Supported Sampler rendering applies a provisional
    /// linear-amplitude fade across the source sample; exact native curve comparison is open.
    pub fn sampler_fade_in_raw(&self) -> Option<u16> {
        (self.kind == Some(0))
            .then_some(self.sampler_fade_in_raw)
            .flatten()
    }

    /// Whether the kind-0 Sampler's Volume envelope is enabled, when its saved `0xDA`
    /// envelope records include the Volume record. FL Studio plays a sample through to its
    /// natural end after note-off when this envelope is disabled.
    pub fn sampler_volume_envelope_enabled(&self) -> Option<bool> {
        self.sampler_envelope_lfo(SamplerEnvelopeLfoTarget::Volume)
            .map(|record| record.enabled())
    }

    /// Saved raw PAHDSR/LFO fields from one ordered Sampler `0xDA` record.
    pub fn sampler_envelope_lfo(
        &self,
        target: SamplerEnvelopeLfoTarget,
    ) -> Option<SamplerEnvelopeLfo> {
        (self.kind == Some(0))
            .then_some(self.sampler_envelope_lfos[target.index()])
            .flatten()
    }

    /// The five ordered Sampler Envelope/LFO records, retaining missing or truncated records as
    /// `None` entries so the target indices stay aligned with the FL event order.
    pub fn sampler_envelope_lfos(&self) -> Option<[Option<SamplerEnvelopeLfo>; 5]> {
        (self.kind == Some(0)).then_some(self.sampler_envelope_lfos)
    }

    /// Saved raw fields from the Sampler's Volume-target `0xDA` record.
    pub fn sampler_volume_envelope(&self) -> Option<SamplerVolumeEnvelope> {
        self.sampler_envelope_lfo(SamplerEnvelopeLfoTarget::Volume)
    }

    /// Saved raw Sampler Max Polyphony value from the channel's `0xDD` event, when present.
    /// Playback currently treats zero as unrestricted; this sentinel still needs native-save
    /// confirmation.
    pub fn sampler_max_polyphony_raw(&self) -> Option<u32> {
        (self.kind == Some(0))
            .then_some(self.sampler_max_polyphony_raw)
            .flatten()
    }

    /// Whether the Sampler's Mono flag is set in the channel's `0xDD` event, when present.
    pub fn sampler_mono_enabled(&self) -> Option<bool> {
        (self.kind == Some(0))
            .then_some(self.sampler_mono_enabled)
            .flatten()
    }

    /// Saved `(Cut, Cut by)` channel group numbers from event `0x84`, when present.
    pub fn channel_cut_groups(&self) -> Option<(u16, u16)> {
        self.channel_cut_groups
    }

    /// Whether a kind-0 Sampler uses embedded sample loop points.
    ///
    /// The `UsesLoopPoints` option is bit 3 of the `0x8F` sampler-flags dword.
    pub fn sampler_uses_loop_points(&self) -> bool {
        self.kind == Some(0)
            && self
                .sampler_flags
                .is_some_and(|flags| flags & (1 << 3) != 0)
    }

    /// Whether a kind-0 Sampler has its Ping-pong loop option enabled (`0x14`).
    pub fn sampler_ping_pong_loop_enabled(&self) -> bool {
        self.kind == Some(0) && self.ping_pong_loop
    }

    /// Saved root note for a kind-0 Sampler, when the raw value is a MIDI key number.
    pub fn sampler_root_key(&self) -> Option<u16> {
        if self.kind != Some(0) {
            return None;
        }
        self.sampler_root_note
            .filter(|note| *note <= 127)
            .map(|note| note as u16)
    }

    /// Saved channel Fine Tune value (`0x8E`) in cents when it falls in FL Studio's documented
    /// -100..=100 range. The raw value remains available even when it is outside that range.
    pub fn keyboard_fine_tune_raw(&self) -> Option<i32> {
        self.keyboard_fine_tune_raw
    }

    /// Channel Fine Tune in cents for Sampler, Native instrument, and Audio Clip channels.
    pub fn keyboard_fine_tune_cents(&self) -> Option<i32> {
        if !matches!(self.kind, Some(0 | 2 | 4)) {
            return None;
        }
        self.keyboard_fine_tune_raw
            .filter(|cents| (-100..=100).contains(cents))
    }

    /// Inclusive playable MIDI key range from the channel Parameters event.
    /// Missing or invalid ranges are treated as unrestricted.
    pub fn keyboard_key_region(&self) -> Option<(u16, u16)> {
        let (low, high) = self.keyboard_key_region?;
        (low <= high && high <= 127).then_some((low as u16, high as u16))
    }

    /// Whether this channel responds to the project's Master pitch control.
    /// Missing Parameters data defaults to disabled.
    pub fn keyboard_main_pitch_enabled(&self) -> bool {
        self.keyboard_main_pitch.unwrap_or(false)
    }

    /// Whether the channel uses its root key to transpose note input where supported.
    /// Missing Parameters data defaults to disabled.
    pub fn keyboard_add_to_key_enabled(&self) -> bool {
        self.keyboard_add_root.unwrap_or(false)
    }

    /// Raw per-channel stretching values from the unique, sufficiently long Parameters event.
    /// Returns `None` when no decoded event contains all four fields.
    pub fn stretch_settings(&self) -> Option<ChannelStretchSettings> {
        self.stretch_settings
    }

    /// Returns the project's pitch offset for this channel, or zero when the channel is not
    /// enabled for Master pitch or the stored project value is outside FL Studio's range.
    pub fn master_pitch_offset_cents(&self, project_master_pitch_cents: Option<i16>) -> i16 {
        if !self.keyboard_main_pitch_enabled() {
            return 0;
        }
        project_master_pitch_cents
            .filter(|cents| (-1200..=1200).contains(cents))
            .unwrap_or_default()
    }

    /// Effective continuous pitch offset for renderers that honor Add to Key.
    /// When Add to Key is enabled, the project pitch is represented by a note
    /// transposition instead of continuous tuning.
    pub fn master_pitch_render_offset_cents(&self, project_master_pitch_cents: Option<i16>) -> i16 {
        if self.keyboard_add_to_key_enabled() {
            0
        } else {
            self.master_pitch_offset_cents(project_master_pitch_cents)
        }
    }

    /// Semitone note transposition for renderers that honor Add to Key.
    /// Fractional semitone project pitches round to the nearest MIDI note.
    pub fn master_pitch_note_offset_semitones(
        &self,
        project_master_pitch_cents: Option<i16>,
    ) -> i16 {
        if !self.keyboard_add_to_key_enabled() {
            return 0;
        }
        let cents = self.master_pitch_offset_cents(project_master_pitch_cents);
        if cents >= 0 {
            (cents + 50) / 100
        } else {
            (cents - 50) / 100
        }
    }

    /// Raw four-byte channel color value, in little-endian RGBA byte order.
    pub fn color(&self) -> Option<[u8; 4]> {
        self.color.map(u32::to_le_bytes)
    }

    /// Raw signed Mixer track assignment from the channel's one-byte `0x16` event.
    pub fn mixer_track(&self) -> Option<i8> {
        self.mixer_track
    }

    /// Zero-based Channel Rack display group from the signed `0x91` event.
    /// Missing or negative values represent an unassigned channel.
    pub fn group_number(&self) -> Option<i32> {
        self.group_number
    }

    /// Raw FL channel volume value, in the project's 0..=12800 control range.
    pub fn volume(&self) -> Option<u32> {
        self.volume
    }

    /// Raw FL channel pan value, in the project's 0..=12800 control range.
    pub fn pan(&self) -> Option<i32> {
        self.pan
    }

    /// Raw fields from each recognized 20-byte core in a channel `0xD5`
    /// Level Adjusts data event. A channel can contain multiple matching
    /// events, so callers should not assume there is exactly one.
    pub fn level_adjustments(&self) -> &[ChannelLevelAdjustments] {
        &self.level_adjustments
    }

    /// Raw per-channel swing mix from the `0x61` word event.
    /// A missing event uses FL Studio's default of 128 (100%).
    pub fn swing_mix_raw(&self) -> Option<u16> {
        self.swing_mix
    }

    /// Per-channel swing mix in FL Studio's 0..=128 range, defaulting to 128 (100%).
    pub fn swing_mix(&self) -> u16 {
        self.swing_mix.unwrap_or(128)
    }

    /// Whether this channel has a recognized modern levels event that can be edited losslessly.
    pub fn levels_editable(&self) -> bool {
        self.levels_editable
    }

    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Sample source path decoded from a sample-bearing channel's `0xC4` string event.
    pub fn sample_path(&self) -> Option<&str> {
        self.sample_path.as_deref()
    }

    /// Channel IDs referenced by a Layer channel's repeated `0x5E` events.
    pub fn layer_child_ids(&self) -> Option<&[u16]> {
        (self.channel_type() == Some(ChannelType::Layer)).then_some(&self.layer_children)
    }

    /// Raw Layer flags from the `0x90` dword event.
    pub fn layer_flags(&self) -> Option<u32> {
        (self.channel_type() == Some(ChannelType::Layer))
            .then_some(self.layer_flags)
            .flatten()
    }

    /// Observed Layer flag bit 0, named `Random` by independent format research.
    pub fn layer_random_enabled(&self) -> Option<bool> {
        self.layer_flags().map(|flags| flags & 1 != 0)
    }

    /// Observed Layer flag bit 1, named `Crossfade` by independent format research.
    pub fn layer_crossfade_enabled(&self) -> Option<bool> {
        self.layer_flags().map(|flags| flags & 2 != 0)
    }

    /// Saved Layer Fade control from Channel Parameters (`0xC7`) bytes 88â€“91.
    ///
    /// The independently documented field range is 0â€“256. Invalid and absent
    /// values return `None`; non-Layer channels never expose this field.
    pub fn layer_fade_raw(&self) -> Option<u16> {
        if self.channel_type() != Some(ChannelType::Layer) {
            return None;
        }
        self.layer_fade_raw
            .filter(|value| *value <= 256)
            .map(|value| value as u16)
    }

    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

/// Resolves a note at a channel to the enabled Sampler or instrument channels
/// reached through any Layer children.
///
/// Layer Random uses a stable per-note pseudo-random choice in this runtime so
/// Sampler and VST3 paths can share a routing seed. FL Studio's native random
/// sequence is not known, so rendered choices may differ from FL Studio.
pub struct ChannelNoteRouter {
    channels_by_id: HashMap<u16, ChannelSummary>,
}

struct ChannelRouteState {
    note_seed: u64,
    key: u16,
    active_layers: HashSet<u16>,
    seen_targets: HashSet<u16>,
    targets: Vec<(u16, f32)>,
}

impl ChannelNoteRouter {
    /// Builds a router from a project's channel summaries.
    pub fn new(channels: impl IntoIterator<Item = ChannelSummary>) -> Self {
        Self {
            channels_by_id: channels
                .into_iter()
                .map(|channel| (channel.id(), channel))
                .collect(),
        }
    }

    /// Returns targets for a note after applying the source and child key regions.
    /// Layer Random chooses from the children that accept this note.
    pub fn targets_for_note(&self, source_channel_id: u16, note_seed: u64, key: u16) -> Vec<u16> {
        self.targets_for_note_with_gain(source_channel_id, note_seed, key)
            .into_iter()
            .map(|(channel_id, _)| channel_id)
            .collect()
    }

    /// Returns Sampler route gains for the saved Layer crossfade position.
    /// Non-crossfaded routes have gain 1.0. Crossfade is applied only when all
    /// active child subtrees lead exclusively to built-in Sampler channels;
    /// VST3 routes retain gain 1.
    pub fn targets_for_note_with_gain(
        &self,
        source_channel_id: u16,
        note_seed: u64,
        key: u16,
    ) -> Vec<(u16, f32)> {
        let mut state = ChannelRouteState {
            note_seed,
            key,
            active_layers: HashSet::new(),
            seen_targets: HashSet::new(),
            targets: Vec::new(),
        };
        self.route_channel(source_channel_id, 1.0, &mut state);
        state.targets
    }

    fn route_channel(&self, channel_id: u16, route_gain: f32, state: &mut ChannelRouteState) {
        let Some(channel) = self.channels_by_id.get(&channel_id) else {
            return;
        };
        if channel.enabled == Some(false) || !channel_accepts_key(channel, state.key) {
            return;
        }
        if channel.channel_type() != Some(ChannelType::Layer) {
            if state.seen_targets.insert(channel_id) {
                state.targets.push((channel_id, route_gain));
            }
            return;
        }
        if !state.active_layers.insert(channel_id) {
            return;
        }

        let mut seen_children = HashSet::new();
        let children = channel
            .layer_children
            .iter()
            .copied()
            .filter(|child_id| seen_children.insert(*child_id))
            .filter(|child_id| {
                self.channels_by_id.get(child_id).is_some_and(|child| {
                    child.enabled != Some(false)
                        && (child.channel_type() != Some(ChannelType::Layer)
                            || !state.active_layers.contains(child_id))
                        && channel_accepts_key(child, state.key)
                })
            })
            .collect::<Vec<_>>();
        if channel.layer_random_enabled() == Some(true) && !children.is_empty() {
            let mut random =
                state.note_seed ^ (u64::from(channel_id) << 32) ^ 0x9E37_79B9_7F4A_7C15;
            random = (random ^ (random >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            random = (random ^ (random >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            random ^= random >> 31;
            self.route_channel(
                children[(random as usize) % children.len()],
                route_gain,
                state,
            );
        } else if channel.layer_crossfade_enabled() == Some(true)
            && channel.layer_fade_raw().is_some()
            && !children.is_empty()
            && children.iter().all(|child_id| {
                let mut active_layers = state.active_layers.clone();
                self.layer_subtree_targets_only_samplers(*child_id, state.key, &mut active_layers)
            })
        {
            let fade = f32::from(channel.layer_fade_raw().unwrap_or_default()) / 256.0;
            let position = fade * (children.len().saturating_sub(1) as f32);
            let lower_index = position.floor() as usize;
            let upper_fraction = position - lower_index as f32;
            for (index, child_id) in children.into_iter().enumerate() {
                let child_gain = if index == lower_index {
                    1.0 - upper_fraction
                } else if index == lower_index + 1 {
                    upper_fraction
                } else {
                    0.0
                };
                if child_gain > 0.0 {
                    self.route_channel(child_id, route_gain * child_gain, state);
                }
            }
        } else {
            for child_id in children {
                self.route_channel(child_id, route_gain, state);
            }
        }
        state.active_layers.remove(&channel_id);
    }

    fn layer_subtree_targets_only_samplers(
        &self,
        channel_id: u16,
        key: u16,
        active_layers: &mut HashSet<u16>,
    ) -> bool {
        let Some(channel) = self.channels_by_id.get(&channel_id) else {
            return false;
        };
        if channel.enabled == Some(false) || !channel_accepts_key(channel, key) {
            return false;
        }
        match channel.channel_type() {
            Some(ChannelType::Sampler) => true,
            Some(ChannelType::Layer) => {
                if !active_layers.insert(channel_id) {
                    return false;
                }
                let mut seen_children = HashSet::new();
                let children = channel
                    .layer_children
                    .iter()
                    .copied()
                    .filter(|child_id| seen_children.insert(*child_id))
                    .filter(|child_id| {
                        self.channels_by_id.get(child_id).is_some_and(|child| {
                            child.enabled != Some(false)
                                && (child.channel_type() != Some(ChannelType::Layer)
                                    || !active_layers.contains(child_id))
                                && channel_accepts_key(child, key)
                        })
                    })
                    .collect::<Vec<_>>();
                let supports_crossfade = !children.is_empty()
                    && children.iter().all(|child_id| {
                        self.layer_subtree_targets_only_samplers(*child_id, key, active_layers)
                    });
                active_layers.remove(&channel_id);
                supports_crossfade
            }
            _ => false,
        }
    }
}

fn channel_accepts_key(channel: &ChannelSummary, key: u16) -> bool {
    channel
        .keyboard_key_region()
        .is_none_or(|(low, high)| (low..=high).contains(&key))
}

/// One point from the `0xEA` payload of a kind-5 automation channel.
///
/// Positions are cumulative beats from the start of the automation clip. The
/// four trailing bytes are retained as opaque point state for forward
/// compatibility.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationPoint {
    position_beats: f64,
    value: f64,
    tension: f32,
    trailing_bytes: [u8; 4],
}

impl AutomationPoint {
    pub fn position_beats(&self) -> f64 {
        self.position_beats
    }

    /// Normalized automation value, usually within 0..=1.
    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn tension(&self) -> f32 {
        self.tension
    }

    /// Opaque four-byte point state preserved from the source project.
    pub fn trailing_bytes(&self) -> [u8; 4] {
        self.trailing_bytes
    }
}

/// Decoded automation points attached to one type-5 Channel Rack channel.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationChannel {
    channel_id: u16,
    display_name: Option<String>,
    points: Vec<AutomationPoint>,
    data_event_index: Option<usize>,
}

impl AutomationChannel {
    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    /// Source `0xEA` event index, or `None` when the channel has no point blob.
    pub fn data_event_index(&self) -> Option<usize> {
        self.data_event_index
    }
}

/// Fields that can be changed on an existing automation point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomationPointEdit {
    pub position_beats: Option<f64>,
    pub value: Option<f64>,
    pub tension: Option<f32>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Pattern {
    pub id: u16,
    pub name: Option<String>,
    pub length_ticks: Option<u32>,
    pub notes: Vec<PatternNote>,
    pub controllers: Vec<PatternController>,
    pub time_markers: Vec<TimeMarker>,
}

/// Raw controller point record stored in a Pattern's `0xDF` event.
///
/// The channel and flags bytes are retained without interpretation. `value_bits` keeps the
/// original IEEE-754 representation, including unusual or non-finite values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PatternController {
    pub position: u32,
    pub reserved: [u8; 2],
    pub channel: u8,
    pub flags: u8,
    pub value_bits: u32,
}

impl PatternController {
    /// Decodes the stored float value while leaving its original bits available in `value_bits`.
    pub fn value(&self) -> f32 {
        f32::from_bits(self.value_bits)
    }

    fn decode(record: &[u8; FLP_PATTERN_CONTROLLER_RECORD_SIZE]) -> Self {
        Self {
            position: u32::from_le_bytes(record[..4].try_into().expect("four-byte position")),
            reserved: [record[4], record[5]],
            channel: record[6],
            flags: record[7],
            value_bits: u32::from_le_bytes(record[8..12].try_into().expect("four-byte value")),
        }
    }

    fn encode(self) -> [u8; FLP_PATTERN_CONTROLLER_RECORD_SIZE] {
        let mut record = [0; FLP_PATTERN_CONTROLLER_RECORD_SIZE];
        record[..4].copy_from_slice(&self.position.to_le_bytes());
        record[4..6].copy_from_slice(&self.reserved);
        record[6] = self.channel;
        record[7] = self.flags;
        record[8..12].copy_from_slice(&self.value_bits.to_le_bytes());
        record
    }
}

/// Fields that can be changed on an existing Pattern controller point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatternControllerEdit {
    pub position: Option<u32>,
    pub value: Option<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

/// FL Studio's observed flags word for ordinary score notes.
pub const PATTERN_NOTE_DEFAULT_FLAGS: u16 = 0x4000;

/// FL Studio's observed release value for a new score note.
pub const PATTERN_NOTE_DEFAULT_RELEASE: u8 = 64;

/// FL Studio score-note flag for slide notes.
pub const PATTERN_NOTE_SLIDE_FLAG: u16 = 1 << 3;

/// Candidate FL Studio score-note flag for muted notes.
///
/// Image-Line documents the note mute behavior but not its FLP bit assignment;
/// this low-bit mapping is inferred and still needs comparison with a native save.
pub const PATTERN_NOTE_MUTED_FLAG: u16 = 1 << 0;

/// Candidate Playlist item flag for an individually muted clip.
///
/// An independent FLP parser corroborates the bit mapping. Image-Line's manual
/// documents clip muting but not its binary representation.
pub const PLAYLIST_CLIP_MUTED_FLAG: u16 = 0x2000;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PatternNoteEdit {
    pub position: Option<u32>,
    pub flags: Option<u16>,
    pub slide: Option<bool>,
    pub muted: Option<bool>,
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

/// Direction used by the Riff Machine's built-in arpeggiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpeggioDirection {
    Up,
    Down,
    UpDown,
}

/// Pitch traversal used by the Piano roll Arpeggiator's Range Pattern setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpeggioRangePattern {
    /// Repeat the range from low to high.
    Up,
    /// Repeat the range from high to low.
    Down,
    /// Traverse low to high, then back down without repeating the endpoints.
    UpDown,
    /// Traverse low to high and high to low, repeating both turning notes.
    Alternate,
}

/// Event that ends a repeated Piano roll arpeggio pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpeggioSyncMode {
    /// Apply one arpeggio pass without repeating it.
    Time,
    /// Repeat until every note in the source chord has ended.
    Block,
    /// Repeat until the first note in the source chord ends.
    Chord,
}

/// Direction used when a limited pitch needs to move onto a selected scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitSnapDirection {
    Up,
    Down,
    Alternate,
}

/// Direction used to rotate note starts by one snap unit within their measures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternNoteRotation {
    Left,
    Right,
}

/// Timing and velocity settings for the Piano roll Strum operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StrumOptions {
    pub spread_ticks: u32,
    pub descending: bool,
    /// Positive values lower successive note velocities; negative values raise them.
    pub velocity_amount: i16,
    /// Curves the velocity change from gradual to accelerated/decelerated.
    pub velocity_tension_percent: i16,
}

impl Default for StrumOptions {
    fn default() -> Self {
        Self {
            spread_ticks: 120,
            descending: false,
            velocity_amount: 0,
            velocity_tension_percent: 0,
        }
    }
}

/// Pitch range, wrap mode, and optional scale settings for the Piano roll Limit operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitNoteOptions<'a> {
    pub minimum_key: u16,
    pub maximum_key: u16,
    pub wrap_to_bottom: bool,
    pub scale_root: Option<u8>,
    pub scale_intervals: Option<&'a [u8]>,
    pub snap_direction: LimitSnapDirection,
}

/// Parameters for replacing simultaneous notes with a gated arpeggio sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpeggioOptions {
    pub step_ticks: u32,
    pub range_octaves: u8,
    pub gate_percent: u8,
    pub range_pattern: ArpeggioRangePattern,
    pub sync: ArpeggioSyncMode,
}

/// Timing mode for the Riff Machine's Groove stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiffMachineQuantizeMode {
    LeaveDuration,
    LeaveEnd,
    QuantizeDuration,
    QuantizeEnd,
}

/// Note-duration behavior for Piano roll quantization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantizeNoteMode {
    LeaveDuration,
    LeaveEnd,
    QuantizeDuration,
    QuantizeEnd,
}

/// Settings for Piano roll quantization of note starts and durations.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantizeNoteOptions {
    pub grid_ticks: u32,
    pub start_strength: f64,
    /// Snap only features within this fraction of half a grid step.
    pub sensitivity: f64,
    pub duration_strength: f64,
    pub swing: f64,
    pub mode: QuantizeNoteMode,
}

impl Default for QuantizeNoteOptions {
    fn default() -> Self {
        Self {
            grid_ticks: 24,
            start_strength: 1.0,
            sensitivity: 1.0,
            duration_strength: 0.0,
            swing: 0.0,
            mode: QuantizeNoteMode::LeaveDuration,
        }
    }
}

/// Settings for a seeded, scale-based Riff Machine pass over an existing note progression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiffMachineOptions<'a> {
    pub scale_root: u8,
    pub scale_intervals: &'a [u8],
    pub minimum_key: u16,
    pub maximum_key: u16,
    pub wrap_to_bottom: bool,
    pub snap_direction: LimitSnapDirection,
    pub step_ticks: u32,
    pub range_octaves: u8,
    pub gate_percent: u8,
    pub direction: ArpeggioDirection,
    /// Reverse the generated riff's note time order, or pitch order when preserving onsets.
    pub mirror_horizontal: bool,
    /// Keep generated onsets fixed and reverse the pitch sequence during horizontal mirroring.
    pub preserve_start_times: bool,
    /// Reflect generated pitches around the midpoint of their current pitch range.
    pub mirror_vertical: bool,
    /// Optional Piano roll snap grid for the Groove stage; `None` means snap is disabled.
    pub groove_snap_ticks: Option<u32>,
    /// Mix from original starts toward quantized starts, from 0 through 100 percent.
    pub groove_start_percent: u8,
    /// Quantize only features within this fraction of half a snap step, from 0 through 100.
    pub groove_sensitivity_percent: u8,
    /// Mix note durations or end positions toward their quantized values.
    pub groove_duration_percent: u8,
    pub groove_quantize_mode: RiffMachineQuantizeMode,
    /// Mix seeded offsets into note panning, from 0 through 100 percent.
    pub pan_variation_percent: u8,
    pub length_multiplier_percent: u8,
    pub velocity_variation_percent: u8,
    /// Mix seeded offsets into release and modulation levels, from 0 through 100 percent.
    pub release_variation_percent: u8,
    pub mod_x_variation_percent: u8,
    pub mod_y_variation_percent: u8,
    /// Randomly shift generated pitches by up to this many semitones before scale fitting.
    pub pitch_variation_semitones: u8,
    /// Reset velocity, pan, release, and modulation values before applying offsets.
    pub reset_levels: bool,
    /// Randomize levels above and below their baseline when enabled.
    pub bipolar_levels: bool,
    pub seed: u64,
}

impl<'a> Default for RiffMachineOptions<'a> {
    fn default() -> Self {
        Self {
            scale_root: 0,
            scale_intervals: &[0, 2, 4, 5, 7, 9, 11],
            minimum_key: 36,
            maximum_key: 83,
            wrap_to_bottom: false,
            snap_direction: LimitSnapDirection::Up,
            step_ticks: 24,
            range_octaves: 1,
            gate_percent: 80,
            direction: ArpeggioDirection::Up,
            mirror_horizontal: false,
            preserve_start_times: false,
            mirror_vertical: false,
            groove_snap_ticks: Some(24),
            groove_start_percent: 0,
            groove_sensitivity_percent: 50,
            groove_duration_percent: 0,
            groove_quantize_mode: RiffMachineQuantizeMode::LeaveDuration,
            pan_variation_percent: 0,
            length_multiplier_percent: 100,
            velocity_variation_percent: 10,
            release_variation_percent: 0,
            mod_x_variation_percent: 0,
            mod_y_variation_percent: 0,
            pitch_variation_semitones: 0,
            reset_levels: false,
            bipolar_levels: true,
            seed: 1,
        }
    }
}

fn riff_machine_groove_note_timing(
    note: &PatternNote,
    snap_ticks: u32,
    options: RiffMachineOptions<'_>,
) -> Result<(u32, u32), FlpError> {
    if snap_ticks == 0 {
        return Err(FlpError::UnsupportedEdit(
            "Riff Machine Groove needs an enabled Piano roll snap grid",
        ));
    }
    let grid = u64::from(snap_ticks);
    let original_start = u64::from(note.position);
    let original_length = u64::from(note.length);
    let original_end = original_start
        .checked_add(original_length)
        .ok_or(FlpError::LengthOverflow)?;

    let quantize = |tick: u64| (tick.saturating_add(grid / 2) / grid).saturating_mul(grid);
    let near_grid = |tick: u64, snapped: u64| {
        tick.abs_diff(snapped).saturating_mul(200)
            <= grid.saturating_mul(u64::from(options.groove_sensitivity_percent))
    };
    let mix = |original: u64, target: u64, amount: u8| {
        let delta = target as i64 - original as i64;
        (original as i64 + delta * i64::from(amount) / 100).max(0) as u64
    };

    let snapped_start = quantize(original_start);
    let target_start = if near_grid(original_start, snapped_start) {
        snapped_start
    } else {
        original_start
    };
    let mut start = mix(original_start, target_start, options.groove_start_percent);
    let snapped_end = quantize(original_end);
    let target_end = if near_grid(original_end, snapped_end) {
        snapped_end
    } else {
        original_end
    };
    let length = match options.groove_quantize_mode {
        RiffMachineQuantizeMode::LeaveDuration => original_length,
        RiffMachineQuantizeMode::LeaveEnd => {
            start = start.min(original_end.saturating_sub(1));
            original_end.saturating_sub(start)
        }
        RiffMachineQuantizeMode::QuantizeDuration => {
            let target_length = target_end.saturating_sub(target_start).max(1);
            mix(
                original_length,
                target_length,
                options.groove_duration_percent,
            )
            .max(1)
        }
        RiffMachineQuantizeMode::QuantizeEnd => {
            let end = mix(original_end, target_end, options.groove_duration_percent);
            start = start.min(end.saturating_sub(1));
            end.saturating_sub(start)
        }
    };
    let start = u32::try_from(start).map_err(|_| FlpError::LengthOverflow)?;
    let length =
        u32::try_from(length.min(u64::from(u32::MAX))).map_err(|_| FlpError::LengthOverflow)?;
    Ok((start, length.max(1)))
}

/// Parameters for seeded note-level and pitch randomization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomizerOptions {
    pub seed: u64,
    pub velocity_amount_percent: i16,
    pub pan_amount_percent: i16,
    pub release_amount_percent: i16,
    pub mod_x_amount_percent: i16,
    pub mod_y_amount_percent: i16,
    pub pitch_range_semitones: u8,
    pub bipolar: bool,
    pub reset_levels: bool,
}

/// Chord tones used to constrain seeded random note generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomNoteChordMap {
    Major,
    Minor,
    Diminished,
    Augmented,
    Suspended2,
    Suspended4,
    Power,
    Dominant7,
    Major7,
    Minor7,
    Major6,
    Minor6,
    Add9,
    MinorAdd9,
    Diminished7,
    HalfDiminished7,
    MinorMajor7,
    Dominant7Flat5,
    Dominant7Sharp5,
    Dominant7Flat9,
    Dominant7Sharp9,
    Dominant9,
    Major9,
    Minor9,
    Dominant11,
    Minor11,
    Dominant13,
    Major7Add13,
}

impl RandomNoteChordMap {
    pub const ALL: [Self; 28] = [
        Self::Major,
        Self::Minor,
        Self::Diminished,
        Self::Augmented,
        Self::Suspended2,
        Self::Suspended4,
        Self::Power,
        Self::Dominant7,
        Self::Major7,
        Self::Minor7,
        Self::Major6,
        Self::Minor6,
        Self::Add9,
        Self::MinorAdd9,
        Self::Diminished7,
        Self::HalfDiminished7,
        Self::MinorMajor7,
        Self::Dominant7Flat5,
        Self::Dominant7Sharp5,
        Self::Dominant7Flat9,
        Self::Dominant7Sharp9,
        Self::Dominant9,
        Self::Major9,
        Self::Minor9,
        Self::Dominant11,
        Self::Minor11,
        Self::Dominant13,
        Self::Major7Add13,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Major => "Major",
            Self::Minor => "Minor",
            Self::Diminished => "Diminished",
            Self::Augmented => "Augmented",
            Self::Suspended2 => "Suspended 2",
            Self::Suspended4 => "Suspended 4",
            Self::Power => "Power fifth",
            Self::Dominant7 => "Dominant 7",
            Self::Major7 => "Major 7",
            Self::Minor7 => "Minor 7",
            Self::Major6 => "Major 6",
            Self::Minor6 => "Minor 6",
            Self::Add9 => "Add 9",
            Self::MinorAdd9 => "Minor add 9",
            Self::Diminished7 => "Diminished 7",
            Self::HalfDiminished7 => "Half-diminished 7",
            Self::MinorMajor7 => "Minor-major 7",
            Self::Dominant7Flat5 => "Dominant 7 flat 5",
            Self::Dominant7Sharp5 => "Dominant 7 sharp 5",
            Self::Dominant7Flat9 => "Dominant 7 flat 9",
            Self::Dominant7Sharp9 => "Dominant 7 sharp 9",
            Self::Dominant9 => "Dominant 9",
            Self::Major9 => "Major 9",
            Self::Minor9 => "Minor 9",
            Self::Dominant11 => "Dominant 11",
            Self::Minor11 => "Minor 11",
            Self::Dominant13 => "Dominant 13",
            Self::Major7Add13 => "Major 7 add 13",
        }
    }

    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Diminished => "diminished",
            Self::Augmented => "augmented",
            Self::Suspended2 => "sus2",
            Self::Suspended4 => "sus4",
            Self::Power => "power",
            Self::Dominant7 => "dominant7",
            Self::Major7 => "major7",
            Self::Minor7 => "minor7",
            Self::Major6 => "major6",
            Self::Minor6 => "minor6",
            Self::Add9 => "add9",
            Self::MinorAdd9 => "minor-add9",
            Self::Diminished7 => "diminished7",
            Self::HalfDiminished7 => "half-diminished7",
            Self::MinorMajor7 => "minor-major7",
            Self::Dominant7Flat5 => "dominant7-flat5",
            Self::Dominant7Sharp5 => "dominant7-sharp5",
            Self::Dominant7Flat9 => "dominant7-flat9",
            Self::Dominant7Sharp9 => "dominant7-sharp9",
            Self::Dominant9 => "dominant9",
            Self::Major9 => "major9",
            Self::Minor9 => "minor9",
            Self::Dominant11 => "dominant11",
            Self::Minor11 => "minor11",
            Self::Dominant13 => "dominant13",
            Self::Major7Add13 => "major7-add13",
        }
    }

    pub fn intervals(self) -> &'static [u8] {
        match self {
            Self::Major => &[0, 4, 7],
            Self::Minor => &[0, 3, 7],
            Self::Diminished => &[0, 3, 6],
            Self::Augmented => &[0, 4, 8],
            Self::Suspended2 => &[0, 2, 7],
            Self::Suspended4 => &[0, 5, 7],
            Self::Power => &[0, 7, 12],
            Self::Dominant7 => &[0, 4, 7, 10],
            Self::Major7 => &[0, 4, 7, 11],
            Self::Minor7 => &[0, 3, 7, 10],
            Self::Major6 => &[0, 4, 7, 9],
            Self::Minor6 => &[0, 3, 7, 9],
            Self::Add9 => &[0, 4, 7, 14],
            Self::MinorAdd9 => &[0, 3, 7, 14],
            Self::Diminished7 => &[0, 3, 6, 9],
            Self::HalfDiminished7 => &[0, 3, 6, 10],
            Self::MinorMajor7 => &[0, 3, 7, 11],
            Self::Dominant7Flat5 => &[0, 4, 6, 10],
            Self::Dominant7Sharp5 => &[0, 4, 8, 10],
            Self::Dominant7Flat9 => &[0, 4, 7, 10, 13],
            Self::Dominant7Sharp9 => &[0, 4, 7, 10, 15],
            Self::Dominant9 => &[0, 4, 7, 10, 14],
            Self::Major9 => &[0, 4, 7, 11, 14],
            Self::Minor9 => &[0, 3, 7, 10, 14],
            Self::Dominant11 => &[0, 4, 7, 10, 14, 17],
            Self::Minor11 => &[0, 3, 7, 10, 14, 17],
            Self::Dominant13 => &[0, 4, 7, 10, 14, 17, 21],
            Self::Major7Add13 => &[0, 4, 7, 11, 21],
        }
    }
}

/// Parameters for generating seeded notes on a bounded Piano roll grid range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomNoteGeneratorOptions {
    pub seed: u64,
    pub start_tick: u32,
    /// Exclusive end of the generated range.
    pub end_tick: u32,
    pub step_ticks: u32,
    pub note_length_ticks: u32,
    pub density_percent: u8,
    /// Number of distinct pitches to generate at each occupied grid position.
    pub notes_per_step: u8,
    /// Maximum seeded note-length variation, from -100% (shorter) to +100% (longer).
    pub length_variation_percent: i16,
    /// Merge touching or overlapping generated notes that have the same pitch.
    pub merge_repeated_notes: bool,
    /// Base octave for an octave-bounded pitch pool. Set with `range_octaves`.
    pub base_octave: Option<u8>,
    /// Number of octaves above the base octave included in the pitch pool.
    pub range_octaves: Option<u8>,
    /// Optional chord map. Its pitch classes are combined with any selected scale.
    pub chord_map: Option<RandomNoteChordMap>,
    /// Chord-map root pitch class, 0 = C through 11 = B.
    pub chord_root: u8,
    pub minimum_key: u16,
    pub maximum_key: u16,
}

/// Timing and note-level variation for a seeded Humanize pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HumanizerOptions {
    /// Maximum symmetric timing offset, in project ticks.
    pub timing_range_ticks: u32,
    /// Symmetric variation as a percentage of the full velocity range.
    pub velocity_variation_percent: u8,
    /// Symmetric variation as a percentage of the full pan range.
    pub pan_variation_percent: u8,
    /// Symmetric variation as a percentage of the full release range.
    pub release_variation_percent: u8,
    /// Symmetric variation as a percentage of the full Mod X range.
    pub mod_x_variation_percent: u8,
    /// Symmetric variation as a percentage of the full Mod Y range.
    pub mod_y_variation_percent: u8,
}

/// Parameters for adding and optionally grouping Flam strokes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlamOptions {
    pub stroke_ticks: u32,
    pub velocity: u8,
    pub before: bool,
    pub group_notes: bool,
}

impl Default for FlamOptions {
    fn default() -> Self {
        Self {
            stroke_ticks: 12,
            velocity: 80,
            before: false,
            group_notes: true,
        }
    }
}

/// Parameters for scaling note velocity with a pivot and logarithmic tension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScaleLevelsOptions {
    pub center_percent: i16,
    pub tension_percent: i16,
    pub multiplier_percent: u16,
    pub offset_percent: i16,
}

impl Default for ScaleLevelsOptions {
    fn default() -> Self {
        Self {
            center_percent: 0,
            tension_percent: 0,
            multiplier_percent: 100,
            offset_percent: 0,
        }
    }
}

/// Parameters for scaling note lengths with an optional seeded variation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArticulateOptions {
    pub multiplier_percent: u8,
    pub variation_percent: u8,
    pub seed: u64,
    pub use_original_lengths: bool,
    pub chop_chords: bool,
}

/// Parameters for the Piano roll Claw Machine's periodic note gate and timing slew.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClawMachineOptions {
    pub period_ticks: u32,
    pub trash_every: u8,
    pub time_distortion_percent: i16,
    pub remove_short_notes: bool,
    pub stretch_to_compensate: bool,
}

impl Default for ClawMachineOptions {
    fn default() -> Self {
        Self {
            period_ticks: 384,
            trash_every: 4,
            time_distortion_percent: 0,
            remove_short_notes: false,
            stretch_to_compensate: false,
        }
    }
}

impl Default for ArticulateOptions {
    fn default() -> Self {
        Self {
            multiplier_percent: 100,
            variation_percent: 0,
            seed: 1,
            use_original_lengths: true,
            chop_chords: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Arrangement {
    pub id: u16,
    pub name: Option<String>,
    pub clips: Vec<PlaylistClip>,
    pub time_markers: Vec<TimeMarker>,
}

/// A Playlist marker or time-signature marker stored in an arrangement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimeMarker {
    raw_position: u32,
    numerator: Option<u8>,
    denominator: Option<u8>,
    name: Option<String>,
    source_events: TimeMarkerSourceEvents,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TimeMarkerSourceEvents {
    position: Option<usize>,
    numerator: Option<usize>,
    denominator: Option<usize>,
    name: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimeMarkerEdit {
    pub position_ticks: Option<u32>,
    pub is_signature: Option<bool>,
    pub numerator: Option<u8>,
    pub denominator: Option<u8>,
    pub name: Option<String>,
}

impl TimeMarker {
    /// Original little-endian `0x94` value, including all type/flag bits.
    pub fn raw_position(&self) -> u32 {
        self.raw_position
    }

    /// Tick position with the documented signature-kind bit removed.
    /// The raw dword remains available through `raw_position()` for unknown high bits.
    pub fn position_ticks(&self) -> u32 {
        self.raw_position & TIME_MARKER_TICK_MASK
    }

    /// True when bit 27 marks this record as a time-signature marker.
    pub fn is_signature(&self) -> bool {
        self.raw_position & TIME_MARKER_SIGNATURE_BIT != 0
    }

    pub fn numerator(&self) -> Option<u8> {
        self.numerator
    }

    pub fn denominator(&self) -> Option<u8> {
        self.denominator
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

#[derive(Clone, Copy)]
enum TimeMarkerTarget {
    Arrangement {
        arrangement_index: usize,
        marker_index: usize,
    },
    Pending {
        marker_index: usize,
    },
}

fn time_marker_at_mut<'a>(
    target: TimeMarkerTarget,
    arrangements: &'a mut [Arrangement],
    pending: &'a mut [TimeMarker],
) -> Option<&'a mut TimeMarker> {
    match target {
        TimeMarkerTarget::Arrangement {
            arrangement_index,
            marker_index,
        } => arrangements
            .get_mut(arrangement_index)?
            .time_markers
            .get_mut(marker_index),
        TimeMarkerTarget::Pending { marker_index } => pending.get_mut(marker_index),
    }
}

fn next_randomizer_value(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    value.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn randomizer_offset(state: &mut u64, range: i32, negative: bool, bipolar: bool) -> i32 {
    if range == 0 {
        return 0;
    }
    if bipolar {
        let width = u64::try_from(range * 2 + 1).expect("randomizer range is bounded");
        (next_randomizer_value(state) % width) as i32 - range
    } else {
        let width = u64::try_from(range + 1).expect("randomizer range is bounded");
        let value = (next_randomizer_value(state) % width) as i32;
        if negative { -value } else { value }
    }
}

fn randomizer_pitch_in_scale(
    state: &mut u64,
    key: u16,
    range: u8,
    bipolar: bool,
    scale_root: u8,
    scale_intervals: &[u8],
) -> u16 {
    let key = i32::from(key);
    let range = i32::from(range);
    let lower = if bipolar { key - range } else { key }.max(0);
    let upper = (key + range).min(131);
    let candidates = (lower..=upper)
        .filter(|candidate| note_pitch_in_scale(*candidate, scale_root, scale_intervals))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return key as u16;
    }
    let choice = (next_randomizer_value(state) % candidates.len() as u64) as usize;
    candidates[choice] as u16
}

fn note_index_in_scope(note_index: usize, selected_indices: Option<&HashSet<usize>>) -> bool {
    selected_indices.is_none_or(|indices| indices.contains(&note_index))
}

fn pattern_measure_bounds(
    ppq: u16,
    initial_signature: Option<(u8, u8)>,
    signature_changes: &[(u32, u8, u8)],
    tick: u32,
) -> (u32, u64) {
    let mut signature = initial_signature.unwrap_or((4, 4));
    signature.0 = signature.0.max(1);
    signature.1 = signature.1.max(1);
    let mut segment_start = 0u32;
    let mut change_index = 0usize;

    while let Some((position, _, _)) = signature_changes.get(change_index).copied() {
        if position > tick {
            let bar_ticks = pattern_bar_ticks(ppq, signature);
            let elapsed = tick - segment_start;
            let bar_start =
                u64::from(segment_start) + u64::from(elapsed / bar_ticks) * u64::from(bar_ticks);
            let bar_end = (bar_start + u64::from(bar_ticks)).min(u64::from(position));
            return (bar_start as u32, bar_end);
        }
        if position > segment_start {
            segment_start = position;
        }
        while signature_changes
            .get(change_index)
            .is_some_and(|(change_position, _, _)| *change_position == position)
        {
            let (_, numerator, denominator) = signature_changes[change_index];
            signature = (numerator.max(1), denominator.max(1));
            change_index += 1;
        }
    }

    let bar_ticks = pattern_bar_ticks(ppq, signature);
    let elapsed = tick - segment_start;
    let bar_start =
        u64::from(segment_start) + u64::from(elapsed / bar_ticks) * u64::from(bar_ticks);
    (bar_start as u32, bar_start + u64::from(bar_ticks))
}

fn pattern_bar_ticks(ppq: u16, signature: (u8, u8)) -> u32 {
    (u32::from(ppq.max(1))
        .saturating_mul(u32::from(signature.0.max(1)))
        .saturating_mul(4)
        / u32::from(signature.1.max(1)))
    .max(1)
}

fn note_pitch_in_scale(key: i32, scale_root: u8, scale_intervals: &[u8]) -> bool {
    let pitch_class = (key.rem_euclid(12) - i32::from(scale_root)).rem_euclid(12) as u8;
    scale_intervals.contains(&pitch_class)
}

fn claw_warp_position(
    position: u64,
    origin: u64,
    period_ticks: u32,
    distortion_percent: i16,
) -> Result<u32, FlpError> {
    let period = u64::from(period_ticks);
    let relative = position.saturating_sub(origin);
    let cycle_start = origin + (relative / period) * period;
    let phase = relative % period;
    let normalized = phase as f64 / period as f64;
    let exponent = 1.0 + 2.0 * (f64::from(distortion_percent.abs()) / 100.0);
    let warped = if distortion_percent >= 0 {
        normalized.powf(exponent)
    } else {
        1.0 - (1.0 - normalized).powf(exponent)
    };
    let warped_phase = (warped * period as f64)
        .round()
        .clamp(0.0, (period - 1) as f64) as u64;
    let warped_position = cycle_start + warped_phase;
    u32::try_from(warped_position).map_err(|_| FlpError::LengthOverflow)
}

fn fold_note_pitch_to_limit(key: i32, minimum: i32, maximum: i32, wrap_to_bottom: bool) -> i32 {
    if wrap_to_bottom {
        return (minimum + (key - minimum).rem_euclid(12)).min(maximum);
    }

    let mut key = key;
    while key > maximum && key - 12 >= minimum {
        key -= 12;
    }
    while key < minimum && key + 12 <= maximum {
        key += 12;
    }
    if key < minimum || key > maximum {
        if (key - minimum).abs() <= (key - maximum).abs() {
            minimum
        } else {
            maximum
        }
    } else {
        key
    }
}

fn snap_note_pitch_to_scale(
    key: i32,
    minimum: i32,
    maximum: i32,
    scale_root: u8,
    scale_intervals: &[u8],
    snap_up: bool,
) -> i32 {
    let in_scale = |candidate| note_pitch_in_scale(candidate, scale_root, scale_intervals);
    if snap_up {
        (key..=maximum)
            .find(|candidate| in_scale(*candidate))
            .or_else(|| (minimum..key).rev().find(|candidate| in_scale(*candidate)))
            .expect("the validated key range contains at least one scale pitch")
    } else {
        (minimum..=key)
            .rev()
            .find(|candidate| in_scale(*candidate))
            .or_else(|| (key..=maximum).find(|candidate| in_scale(*candidate)))
            .expect("the validated key range contains at least one scale pitch")
    }
}

fn riff_triad_keys(
    key: u16,
    minimum_key: u16,
    maximum_key: u16,
    scale_root: u8,
    scale_intervals: &[u8],
    wrap_to_bottom: bool,
    snap_up: bool,
) -> Result<Vec<u16>, FlpError> {
    let minimum = i32::from(minimum_key);
    let maximum = i32::from(maximum_key);
    let folded = fold_note_pitch_to_limit(i32::from(key), minimum, maximum, wrap_to_bottom);
    let root_key = snap_note_pitch_to_scale(
        folded,
        minimum,
        maximum,
        scale_root,
        scale_intervals,
        snap_up,
    );
    let root_interval = (root_key.rem_euclid(12) - i32::from(scale_root)).rem_euclid(12) as u8;
    let root_degree = scale_intervals
        .iter()
        .position(|interval| *interval == root_interval)
        .ok_or(FlpError::UnsupportedEdit(
            "the Riff Machine root does not belong to the selected scale",
        ))?;

    let mut pitches = Vec::with_capacity(3);
    for degree_offset in [0usize, 2, 4] {
        let degree = root_degree + degree_offset;
        let octave = degree / scale_intervals.len();
        let interval = i32::from(scale_intervals[degree % scale_intervals.len()]);
        let root_interval = i32::from(scale_intervals[root_degree]);
        let pitch =
            root_key + i32::try_from(octave).map_err(|_| FlpError::LengthOverflow)? * 12 + interval
                - root_interval;
        let pitch = fold_note_pitch_to_limit(pitch, minimum, maximum, wrap_to_bottom);
        let pitch =
            snap_note_pitch_to_scale(pitch, minimum, maximum, scale_root, scale_intervals, true);
        let pitch = u16::try_from(pitch).map_err(|_| FlpError::LengthOverflow)?;
        if !pitches.contains(&pitch) {
            pitches.push(pitch);
        }
    }
    pitches.sort_unstable();
    Ok(pitches)
}

fn scale_note_level(level: u8, options: ScaleLevelsOptions) -> u8 {
    let pivot = f64::from(options.center_percent) * 1.27;
    let distance = f64::from(level) - pivot;
    let distance_scale = pivot.abs().max((127.0 - pivot).abs()).max(1.0);
    let normalized_distance = (distance.abs() / distance_scale).clamp(0.0, 1.0);
    let tension = f64::from(options.tension_percent) / 100.0 * std::f64::consts::LN_10;
    let curved_distance = if tension == 0.0 {
        distance.abs()
    } else {
        let curved = ((1.0 + (tension.exp() - 1.0) * normalized_distance).ln()) / tension;
        curved * distance_scale
    };
    let tensioned = pivot + distance.signum() * curved_distance;
    let multiplier = f64::from(options.multiplier_percent) / 100.0;
    let offset = f64::from(options.offset_percent) * 1.27;
    (pivot + (tensioned - pivot) * multiplier + offset)
        .round()
        .clamp(0.0, 127.0) as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistTrackSync {
    Off,
    QuarterBeat,
    HalfBeat,
    Beat,
    TwoBeats,
    FourBeats,
    Auto,
}

impl PlaylistTrackSync {
    fn raw(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::QuarterBeat => 1,
            Self::HalfBeat => 2,
            Self::Beat => 3,
            Self::TwoBeats => 4,
            Self::FourBeats => 5,
            Self::Auto => 6,
        }
    }

    fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::QuarterBeat),
            2 => Some(Self::HalfBeat),
            3 => Some(Self::Beat),
            4 => Some(Self::TwoBeats),
            5 => Some(Self::FourBeats),
            6 => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Saved Performance Mode behavior for each press of a Playlist clip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistTrackPress {
    Retrigger,
    HoldStop,
    HoldMotion,
    Latch,
}

impl PlaylistTrackPress {
    fn raw(self) -> i32 {
        match self {
            Self::Retrigger => 0,
            Self::HoldStop => 1,
            Self::HoldMotion => 2,
            Self::Latch => 3,
        }
    }

    fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Retrigger),
            1 => Some(Self::HoldStop),
            2 => Some(Self::HoldMotion),
            3 => Some(Self::Latch),
            _ => None,
        }
    }
}

/// Saved Performance Mode rule for selecting the next clip on a Playlist track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistTrackMotion {
    Stay,
    OneShot,
    MarchWrap,
    MarchStay,
    MarchStop,
    Random,
    ExclusiveRandom,
}

impl PlaylistTrackMotion {
    fn raw(self) -> i32 {
        match self {
            Self::Stay => 0,
            Self::OneShot => 1,
            Self::MarchWrap => 2,
            Self::MarchStay => 3,
            Self::MarchStop => 4,
            Self::Random => 5,
            Self::ExclusiveRandom => 6,
        }
    }

    fn from_raw(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Stay),
            1 => Some(Self::OneShot),
            2 => Some(Self::MarchWrap),
            3 => Some(Self::MarchStay),
            4 => Some(Self::MarchStop),
            5 => Some(Self::Random),
            6 => Some(Self::ExclusiveRandom),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTrack {
    /// One-based Playlist track identifier stored in the `0xEE` record.
    pub id: u32,
    /// The adjacent `0x2B` byte is retained without assigning it a meaning.
    pub state_byte: Option<u8>,
    pub name: Option<String>,
    /// Track enabled state from byte 12 of the `0xEE` state record. Disabled tracks are muted.
    pub enabled: Option<bool>,
    /// Whether this track is grouped with the Playlist track directly above it.
    pub grouped: Option<bool>,
    /// Saved Performance Mode Press behavior for clips on this track.
    pub press: Option<PlaylistTrackPress>,
    /// Saved Performance Mode Motion behavior for clips on this track.
    pub motion: Option<PlaylistTrackMotion>,
    /// Trigger timing for Performance Mode clips on this track.
    pub trigger_sync: Option<PlaylistTrackSync>,
    /// Whether Performance Mode queues triggered clips for this track.
    pub queued: Option<bool>,
    /// Position within the clip used when a Performance Mode clip starts on this track.
    pub position_sync: Option<PlaylistTrackSync>,
    /// Whether a slightly missed trigger boundary launches immediately on this track.
    pub tolerant: Option<bool>,
    /// Raw 70-byte `0xEE` state payload.
    pub state_bytes: Vec<u8>,
}

/// Fields that can be changed on an existing Playlist track without rewriting its other state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaylistTrackEdit {
    pub enabled: Option<bool>,
    pub grouped: Option<bool>,
    pub press: Option<PlaylistTrackPress>,
    pub motion: Option<PlaylistTrackMotion>,
    pub trigger_sync: Option<PlaylistTrackSync>,
    pub queued: Option<bool>,
    pub position_sync: Option<PlaylistTrackSync>,
    pub tolerant: Option<bool>,
}

/// Mixer insert fields recognized from the observed `0x9A`, `0x93`, `0x95`
/// sequence. The source events remain byte-exact in [`FlpDocument::events`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerInsertSummary {
    ordinal: usize,
    input_raw: i32,
    output_raw: i32,
    color_raw: u32,
    icon_raw: Option<i16>,
    name: Option<String>,
    flags: Option<u32>,
    flags_event_index: Option<usize>,
    first_event_index: usize,
    end_event_index: usize,
}

/// Read-only description of one Mixer effect slot bounded by its closing marker.
/// The original events and plug-in payload stay lossless in [`FlpDocument::events`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerEffectSlotSummary {
    insert_ordinal: usize,
    slot_index: u16,
    plugin_identifier: Option<String>,
    display_name: Option<String>,
    state_event_count: usize,
    first_event_index: usize,
    end_event_index: usize,
}

/// Summary of one route-volume parameter and its paired saved routing flag, when available.
/// The parameter ID and destination mapping remain raw because their visible Mixer-track
/// numbering has not been confirmed against a controlled native save.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerSendSummary {
    insert_ordinal: usize,
    route_parameter_id: u8,
    value: i32,
    enabled: Option<bool>,
    routing_flag_location: Option<(usize, usize)>,
    event_index: usize,
    record_index: usize,
}

impl MixerEffectSlotSummary {
    /// Zero-based insert containing this effect slot.
    pub fn insert_ordinal(&self) -> usize {
        self.insert_ordinal
    }

    /// Zero-based effect-slot index in the insert.
    pub fn slot_index(&self) -> u16 {
        self.slot_index
    }

    /// Plug-in class string from the slot's unique `0xC9` event, when recognized.
    pub fn plugin_identifier(&self) -> Option<&str> {
        self.plugin_identifier.as_deref()
    }

    /// User-facing plug-in name from the slot's unique `0xCB` event, when recognized.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Number of opaque `0xD5` state events in this slot.
    pub fn state_event_count(&self) -> usize {
        self.state_event_count
    }

    /// Events assigned to this slot, excluding its closing marker when present.
    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

impl MixerSendSummary {
    /// Candidate zero-based source insert matched from the Mixer parameter record.
    pub fn insert_ordinal(&self) -> usize {
        self.insert_ordinal
    }

    /// Raw Mixer parameter ID in the send-level range 64â€“191.
    pub fn route_parameter_id(&self) -> u8 {
        self.route_parameter_id
    }

    /// Raw signed send-level value.
    pub fn value(&self) -> i32 {
        self.value
    }

    /// Saved on/off state from the corresponding per-insert routing byte, when present.
    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Event and payload-byte location for an unambiguous saved routing flag.
    pub fn routing_flag_location(&self) -> Option<(usize, usize)> {
        self.routing_flag_location
    }

    /// Event and record location of the source route-volume parameter.
    pub fn parameter_location(&self) -> (usize, usize) {
        (self.event_index, self.record_index)
    }
}

impl MixerInsertSummary {
    /// Zero-based order among Mixer insert records recognized in the event stream.
    pub fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Raw signed value carried by the insert's `0x9A` event.
    pub fn input_raw(&self) -> i32 {
        self.input_raw
    }

    /// Raw signed value carried by the insert's `0x93` event.
    pub fn output_raw(&self) -> i32 {
        self.output_raw
    }

    /// Raw four-byte value carried by the insert's `0x95` event.
    pub fn color_raw(&self) -> u32 {
        self.color_raw
    }

    /// Raw signed icon value carried by the insert's `0x5F` event, when present.
    pub fn icon_raw(&self) -> Option<i16> {
        self.icon_raw
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Raw insert flag bitmask from the version-specific Mixer state event.
    pub fn flags(&self) -> Option<u32> {
        self.flags
    }

    /// Whether this Mixer track is enabled. Disabled tracks are muted.
    pub fn enabled(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 3) != 0)
    }

    /// Whether this Mixer track is soloed.
    pub fn soloed(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 12) != 0)
    }

    /// Whether the track reverses signal polarity.
    pub fn polarity_reversed(&self) -> Option<bool> {
        self.flags.map(|flags| flags & 1 != 0)
    }

    /// Whether the track swaps its left and right channels.
    pub fn swap_left_right(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 1) != 0)
    }

    /// Whether effects processing is enabled for this Mixer track.
    pub fn effects_enabled(&self) -> Option<bool> {
        self.flags.map(|flags| flags & (1 << 2) != 0)
    }

    /// Event range associated with this insert record, ending before the next recognized record.
    pub fn event_range(&self) -> std::ops::Range<usize> {
        self.first_event_index..self.end_event_index
    }
}

/// Conservative direct-route Mixer state for mute, solo, and candidate fader values.
/// Unknown and context-dependent routes are left untouched.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MixerRouteAudibility {
    known_tracks: HashSet<i8>,
    disabled_tracks: HashSet<i8>,
    soloed_insert_tracks: HashSet<i8>,
    source_transforms: HashMap<i8, MixerInsertSignalTransform>,
    source_fader_volume_raw: HashMap<i8, i32>,
    source_signal_flags: HashMap<i8, (bool, bool)>,
    source_enabled: HashMap<i8, bool>,
    send_routes_by_source: HashMap<i8, Vec<MixerSendRouteEdge>>,
    complete_send_sources: HashSet<i8>,
    master_fader_raw: Option<i32>,
    master_pan_raw: Option<i32>,
    master_stereo_separation_raw: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MixerSendRouteEdge {
    destination_track: i8,
    level_raw: i32,
    enabled: bool,
    live_control: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MixerInsertSignalTransform {
    polarity_reversed: bool,
    swap_left_right: bool,
    fader_volume_raw: Option<i32>,
    mixer_pan_raw: Option<i32>,
    stereo_separation_raw: Option<i32>,
    mixer_pan_law_raw: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MixerSourceRouteTransform {
    matrix: [f32; 4],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MixerLiveSendTargetKey {
    pub(crate) source_track: usize,
    pub(crate) destination_track: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MixerLiveSendGain {
    pub(crate) target_index: Option<usize>,
    pub(crate) saved_gain: f32,
    pub(crate) enabled_target_index: Option<usize>,
    pub(crate) saved_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MixerLiveInsertEnabledGain {
    pub(crate) target_index: Option<usize>,
    pub(crate) saved_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MixerLiveSoloState {
    pub(crate) any_soloed: bool,
    pub(crate) soloed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MixerLiveSendOutputPath {
    pub(crate) transform: MixerSourceRouteTransform,
    pub(crate) send_gains: Vec<MixerLiveSendGain>,
    pub(crate) insert_enabled_gains: Vec<MixerLiveInsertEnabledGain>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MixerLiveSendMeterPath {
    pub(crate) track: usize,
    pub(crate) transform: MixerSourceRouteTransform,
    pub(crate) send_gains: Vec<MixerLiveSendGain>,
    pub(crate) insert_enabled_gains: Vec<MixerLiveInsertEnabledGain>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MixerSourceRoutePlan {
    pub(crate) output_transform: MixerSourceRouteTransform,
    pub(crate) insert_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MixerLiveSourceRoutePlan {
    pub(crate) track: usize,
    pub(crate) source_transform: MixerSourceRouteTransform,
    pub(crate) saved_fader_gain: f32,
    pub(crate) saved_pan_raw: Option<i32>,
    pub(crate) saved_stereo_separation_raw: Option<i32>,
    pub(crate) saved_signal_flags: Option<(bool, bool)>,
    pub(crate) saved_enabled: Option<bool>,
    pub(crate) pan_law_raw: Option<u8>,
    pub(crate) output_transform: MixerSourceRouteTransform,
    pub(crate) insert_meter_transforms: Vec<(usize, MixerSourceRouteTransform)>,
    pub(crate) live_send_target_keys: Vec<MixerLiveSendTargetKey>,
    pub(crate) saved_live_send_gains: Vec<f32>,
    pub(crate) saved_live_send_enabled: Vec<bool>,
    pub(crate) live_send_output_solo_tracks: Vec<Vec<usize>>,
    pub(crate) saved_live_send_output_solo_gains: Vec<f32>,
    pub(crate) live_send_meter_solo_tracks: Vec<Vec<usize>>,
    pub(crate) saved_live_send_meter_solo_gains: Vec<f32>,
    pub(crate) live_insert_enabled_tracks: Vec<usize>,
    pub(crate) saved_live_insert_enabled: Vec<bool>,
    pub(crate) live_send_output_paths: Option<Vec<MixerLiveSendOutputPath>>,
    pub(crate) live_send_meter_paths: Vec<MixerLiveSendMeterPath>,
    pub(crate) live_solo: Option<MixerLiveSoloState>,
}

struct MixerLiveSourceRouteData {
    route_plan: MixerSourceRoutePlan,
    output_paths: Option<Vec<MixerLiveSendOutputPath>>,
    meter_paths: Vec<MixerLiveSendMeterPath>,
    target_keys: Vec<MixerLiveSendTargetKey>,
    saved_target_gains: Vec<f32>,
    saved_target_enabled: Vec<bool>,
    output_solo_tracks: Vec<Vec<usize>>,
    saved_output_solo_gains: Vec<f32>,
    meter_solo_tracks: Vec<Vec<usize>>,
    saved_meter_solo_gains: Vec<f32>,
    target_insert_enabled_tracks: Vec<usize>,
    saved_target_insert_enabled: Vec<bool>,
}

impl Default for MixerSourceRouteTransform {
    fn default() -> Self {
        Self {
            matrix: [1.0, 0.0, 0.0, 1.0],
        }
    }
}

impl MixerSourceRouteTransform {
    const fn zero() -> Self {
        Self { matrix: [0.0; 4] }
    }

    pub(crate) fn from_insert_transform(transform: MixerInsertSignalTransform) -> Self {
        let mut left_basis = [1.0, 0.0];
        let mut right_basis = [0.0, 1.0];
        transform.apply_frame(&mut left_basis);
        transform.apply_frame(&mut right_basis);
        Self {
            matrix: [left_basis[0], right_basis[0], left_basis[1], right_basis[1]],
        }
    }

    fn then(self, next: Self) -> Self {
        let [a, b, c, d] = self.matrix;
        let [e, f, g, h] = next.matrix;
        Self {
            matrix: [e * a + f * c, e * b + f * d, g * a + h * c, g * b + h * d],
        }
    }

    fn scaled(self, gain: f32) -> Self {
        Self {
            matrix: self.matrix.map(|value| value * gain),
        }
    }

    fn add(&mut self, other: Self) {
        for (value, addition) in self.matrix.iter_mut().zip(other.matrix) {
            *value += addition;
        }
    }

    pub(crate) fn apply_frame(self, frame: &mut [f32; 2]) {
        let input = *frame;
        frame[0] = self.matrix[0] * input[0] + self.matrix[1] * input[1];
        frame[1] = self.matrix[2] * input[0] + self.matrix[3] * input[1];
    }
}

impl MixerInsertSignalTransform {
    pub(crate) const fn new(polarity_reversed: bool, swap_left_right: bool) -> Self {
        Self {
            polarity_reversed,
            swap_left_right,
            fader_volume_raw: None,
            mixer_pan_raw: None,
            stereo_separation_raw: None,
            mixer_pan_law_raw: None,
        }
    }

    pub(crate) const fn with_signal_flags(
        mut self,
        polarity_reversed: bool,
        swap_left_right: bool,
    ) -> Self {
        self.polarity_reversed = polarity_reversed;
        self.swap_left_right = swap_left_right;
        self
    }

    pub(crate) const fn with_fader_volume_raw(mut self, volume_raw: Option<i32>) -> Self {
        self.fader_volume_raw = volume_raw;
        self
    }

    pub(crate) const fn with_mixer_pan_raw(mut self, pan_raw: Option<i32>) -> Self {
        self.mixer_pan_raw = pan_raw;
        self
    }

    pub(crate) const fn with_stereo_separation_raw(mut self, separation_raw: Option<i32>) -> Self {
        self.stereo_separation_raw = separation_raw;
        self
    }

    pub(crate) const fn with_mixer_pan_law_raw(mut self, pan_law_raw: Option<u8>) -> Self {
        self.mixer_pan_law_raw = pan_law_raw;
        self
    }

    pub(crate) fn apply_frame(self, frame: &mut [f32; 2]) {
        if self.swap_left_right {
            frame.swap(0, 1);
        }
        if self.polarity_reversed {
            frame[0] = -frame[0];
            frame[1] = -frame[1];
        }
        if let Some(separation_raw) = self.stereo_separation_raw {
            apply_mixer_stereo_separation(frame, separation_raw);
        }
        if let Some(pan_raw) = self.mixer_pan_raw {
            apply_mixer_pan(frame, pan_raw, self.mixer_pan_law_raw);
        }
        if let Some(volume_raw) = self.fader_volume_raw {
            let gain = mixer_fader_gain(volume_raw);
            frame[0] *= gain;
            frame[1] *= gain;
        }
    }
}

impl MixerRouteAudibility {
    pub(crate) fn from_document(document: &FlpDocument) -> Self {
        let parameters = document.mixer_parameter_records().unwrap_or_default();
        let mut routing = Self::from_inserts_with_parameters(
            document.mixer_inserts(),
            &parameters,
            document.metadata().pan_law_raw(),
        );
        routing.add_send_summaries(document.mixer_send_summaries().unwrap_or_default());
        routing
    }

    fn add_send_summaries(&mut self, sends: impl IntoIterator<Item = MixerSendSummary>) {
        let sends = sends.into_iter().collect::<Vec<_>>();
        let mut live_control_counts = HashMap::<(i8, i8), usize>::new();
        for send in &sends {
            if send.enabled().is_none() || mixer_send_gain_raw(send.value()).is_none() {
                continue;
            }
            let Ok(source_track) = i8::try_from(send.insert_ordinal()) else {
                continue;
            };
            let Some(destination_ordinal) = send.route_parameter_id().checked_sub(64) else {
                continue;
            };
            let Ok(destination_track) = i8::try_from(destination_ordinal) else {
                continue;
            };
            if self.known_tracks.contains(&source_track)
                && self.known_tracks.contains(&destination_track)
            {
                *live_control_counts
                    .entry((source_track, destination_track))
                    .or_default() += 1;
            }
        }

        let mut source_validity = HashMap::<i8, bool>::new();
        for send in sends {
            let Ok(source_track) = i8::try_from(send.insert_ordinal()) else {
                continue;
            };
            if !self.known_tracks.contains(&source_track) {
                continue;
            }
            let valid = source_validity.entry(source_track).or_insert(true);
            let Some(enabled) = send.enabled() else {
                *valid = false;
                continue;
            };
            if !enabled {
                let Some(destination_ordinal) = send.route_parameter_id().checked_sub(64) else {
                    continue;
                };
                let Ok(destination_track) = i8::try_from(destination_ordinal) else {
                    continue;
                };
                if !self.known_tracks.contains(&destination_track)
                    || mixer_send_gain_raw(send.value()).is_none()
                {
                    continue;
                }
                self.send_routes_by_source
                    .entry(source_track)
                    .or_default()
                    .push(MixerSendRouteEdge {
                        destination_track,
                        level_raw: send.value(),
                        enabled: false,
                        live_control: live_control_counts.get(&(source_track, destination_track))
                            == Some(&1),
                    });
                continue;
            }
            let Some(destination_ordinal) = send.route_parameter_id().checked_sub(64) else {
                *valid = false;
                continue;
            };
            let Ok(destination_track) = i8::try_from(destination_ordinal) else {
                *valid = false;
                continue;
            };
            if !self.known_tracks.contains(&destination_track)
                || mixer_send_gain_raw(send.value()).is_none()
            {
                *valid = false;
                continue;
            }
            self.send_routes_by_source
                .entry(source_track)
                .or_default()
                .push(MixerSendRouteEdge {
                    destination_track,
                    level_raw: send.value(),
                    enabled,
                    live_control: live_control_counts.get(&(source_track, destination_track))
                        == Some(&1),
                });
        }
        for (source_track, valid) in source_validity {
            if valid {
                self.complete_send_sources.insert(source_track);
            } else {
                self.send_routes_by_source.remove(&source_track);
            }
        }
    }

    fn from_inserts_with_parameters(
        inserts: impl IntoIterator<Item = MixerInsertSummary>,
        parameters: &[MixerParameterRecord],
        pan_law_raw: Option<u8>,
    ) -> Self {
        let mut routing = Self::default();
        for insert in inserts {
            let Ok(track) = i8::try_from(insert.ordinal()) else {
                continue;
            };
            routing.known_tracks.insert(track);
            let fader_volume_raw = candidate_mixer_fader_volume(parameters, insert.ordinal());
            let mixer_pan_raw = candidate_mixer_pan(parameters, insert.ordinal());
            let stereo_separation_raw =
                candidate_mixer_stereo_separation(parameters, insert.ordinal());
            let mixer_pan_law_raw = mixer_pan_raw.and(pan_law_raw);
            // Master is the final destination for every normally routed source. Its
            // saved flags and candidate controls are applied to the completed mix instead.
            if track == 0 {
                routing.master_fader_raw = fader_volume_raw;
                routing.master_pan_raw = mixer_pan_raw;
                routing.master_stereo_separation_raw = stereo_separation_raw;
                continue;
            }
            if let Some(fader_volume_raw) = fader_volume_raw {
                routing
                    .source_fader_volume_raw
                    .insert(track, fader_volume_raw);
            }
            // Missing flags already mean an insert is active for static routing. Record that
            // effective default too, so a Mute edit can attach a live smoother during playback.
            let enabled = insert.enabled().unwrap_or(true);
            routing.source_enabled.insert(track, enabled);
            if !enabled {
                routing.disabled_tracks.insert(track);
            }
            if insert.soloed() == Some(true) {
                routing.soloed_insert_tracks.insert(track);
            }
            if let (Some(polarity_reversed), Some(swap_left_right)) =
                (insert.polarity_reversed(), insert.swap_left_right())
            {
                routing
                    .source_signal_flags
                    .insert(track, (polarity_reversed, swap_left_right));
            }
            let transform = MixerInsertSignalTransform::new(
                insert.polarity_reversed() == Some(true),
                insert.swap_left_right() == Some(true),
            )
            .with_fader_volume_raw(fader_volume_raw)
            .with_mixer_pan_raw(mixer_pan_raw)
            .with_stereo_separation_raw(stereo_separation_raw)
            .with_mixer_pan_law_raw(mixer_pan_law_raw);
            if transform != MixerInsertSignalTransform::default() {
                routing.source_transforms.insert(track, transform);
            }
        }
        routing
    }

    /// `None` is FL Studio's default Master route; `-1` means context-dependent
    /// Current insert, so it is kept audible until that context is understood.
    pub(crate) fn allows_channel(&self, route: Option<i8>) -> bool {
        let track = match route {
            None => 0,
            Some(track) if track < 0 => return true,
            Some(track) => track,
        };
        if track != 0 && !self.known_tracks.contains(&track) {
            return true;
        }
        if self.disabled_tracks.contains(&track) {
            return false;
        }
        if self.soloed_insert_tracks.is_empty() {
            return true;
        }
        if track == 0 {
            return false;
        }
        if self.complete_send_sources.contains(&track) {
            self.route_can_reach_soloed_insert(track, &mut HashSet::new())
        } else {
            self.soloed_insert_tracks.contains(&track)
        }
    }

    /// Tests the static solo policy while allowing the source insert's own mute to be controlled
    /// by the live source smoother.
    pub(crate) fn allows_live_source_channel(&self, route: Option<i8>) -> bool {
        let Some(_source_track) =
            route.filter(|track| *track > 0 && self.known_tracks.contains(track))
        else {
            return self.allows_channel(route);
        };
        // Keep every recognized source available so direct routes and complete live send paths
        // can respond when the solo set changes during playback.
        true
    }

    fn route_can_reach_soloed_insert(&self, track: i8, visited: &mut HashSet<i8>) -> bool {
        self.route_can_reach_soloed_insert_with_source_mute_override(track, visited, None, false)
    }

    fn route_can_reach_soloed_insert_with_source_mute_override(
        &self,
        track: i8,
        visited: &mut HashSet<i8>,
        source_mute_override: Option<i8>,
        include_live_zero_sends: bool,
    ) -> bool {
        if (self.disabled_tracks.contains(&track)
            && source_mute_override != Some(track)
            && !include_live_zero_sends)
            || !visited.insert(track)
        {
            return false;
        }
        if self.soloed_insert_tracks.contains(&track) {
            visited.remove(&track);
            return true;
        }
        let reaches_solo = self
            .send_routes_by_source
            .get(&track)
            .into_iter()
            .flatten()
            .any(|route| {
                ((route.enabled && mixer_send_gain_raw(route.level_raw).unwrap_or_default() > 0.0)
                    || (include_live_zero_sends && route.live_control))
                    && self.route_can_reach_soloed_insert_with_source_mute_override(
                        route.destination_track,
                        visited,
                        source_mute_override,
                        include_live_zero_sends,
                    )
            });
        visited.remove(&track);
        reaches_solo
    }

    /// Returns the supported phase, stereo, candidate pan, and fader transform for a route.
    /// Master processing is applied once to the final mix, and unknown routes are unchanged.
    pub(crate) fn transform_for_channel(&self, route: Option<i8>) -> MixerInsertSignalTransform {
        route
            .filter(|track| *track > 0 && self.known_tracks.contains(track))
            .and_then(|track| self.source_transforms.get(&track).copied())
            .unwrap_or_default()
    }

    /// Splits a source insert's static signal controls from the transforms on its routed paths.
    /// The source fader, pan, stereo separation, phase, and channel swap can then follow live
    /// targets before routes split.
    pub(crate) fn live_source_route_plan_for_channel(
        &self,
        route: Option<i8>,
    ) -> Option<MixerLiveSourceRoutePlan> {
        let track = route.filter(|track| *track > 0 && self.known_tracks.contains(track))?;
        let live_direct_solo = !self.complete_send_sources.contains(&track);
        let live_send_solo = self.complete_send_sources.contains(&track);
        let source_transform = self
            .source_transforms
            .get(&track)
            .copied()
            .unwrap_or_default();
        let saved_fader_raw = self.source_fader_volume_raw.get(&track).copied();
        let saved_signal_flags = self.source_signal_flags.get(&track).copied();
        let saved_enabled = self.source_enabled.get(&track).copied();
        let has_live_send_controls = self
            .send_routes_by_source
            .get(&track)
            .is_some_and(|routes| routes.iter().any(|route| route.live_control));
        if saved_fader_raw.is_none()
            && source_transform.mixer_pan_raw.is_none()
            && source_transform.stereo_separation_raw.is_none()
            && saved_signal_flags.is_none()
            && saved_enabled.is_none()
            && !has_live_send_controls
            && !live_direct_solo
            && !live_send_solo
        {
            return None;
        }

        let source_transform = if saved_signal_flags.is_some() {
            source_transform.with_signal_flags(false, false)
        } else {
            source_transform
        };
        let source_transform_without_live_controls =
            MixerSourceRouteTransform::from_insert_transform(
                source_transform
                    .with_fader_volume_raw(None)
                    .with_mixer_pan_raw(None)
                    .with_stereo_separation_raw(None),
            );
        let mut downstream_routes = self.clone();
        downstream_routes.disabled_tracks.remove(&track);
        if live_direct_solo {
            downstream_routes.soloed_insert_tracks.clear();
        }
        downstream_routes
            .source_transforms
            .insert(track, MixerInsertSignalTransform::default());
        let live_route_data = downstream_routes.live_source_route_data(track);

        Some(MixerLiveSourceRoutePlan {
            track: track as usize,
            source_transform: source_transform_without_live_controls,
            saved_fader_gain: saved_fader_raw.map_or(1.0, mixer_fader_gain),
            saved_pan_raw: source_transform.mixer_pan_raw,
            saved_stereo_separation_raw: source_transform.stereo_separation_raw,
            saved_signal_flags,
            saved_enabled,
            pan_law_raw: source_transform.mixer_pan_law_raw,
            output_transform: live_route_data.route_plan.output_transform,
            insert_meter_transforms: live_route_data.route_plan.insert_meter_transforms,
            live_send_target_keys: live_route_data.target_keys,
            saved_live_send_gains: live_route_data.saved_target_gains,
            saved_live_send_enabled: live_route_data.saved_target_enabled,
            live_send_output_solo_tracks: live_route_data.output_solo_tracks,
            saved_live_send_output_solo_gains: live_route_data.saved_output_solo_gains,
            live_send_meter_solo_tracks: live_route_data.meter_solo_tracks,
            saved_live_send_meter_solo_gains: live_route_data.saved_meter_solo_gains,
            live_insert_enabled_tracks: live_route_data.target_insert_enabled_tracks,
            saved_live_insert_enabled: live_route_data.saved_target_insert_enabled,
            live_send_output_paths: live_route_data.output_paths,
            live_send_meter_paths: live_route_data.meter_paths,
            live_solo: live_direct_solo.then_some(MixerLiveSoloState {
                any_soloed: !self.soloed_insert_tracks.is_empty(),
                soloed: self.soloed_insert_tracks.contains(&track),
            }),
        })
    }

    fn live_source_route_data(&self, source_track: i8) -> MixerLiveSourceRouteData {
        if !self.complete_send_sources.contains(&source_track) {
            return MixerLiveSourceRouteData {
                route_plan: self.source_route_plan_for_channel(Some(source_track)),
                output_paths: None,
                meter_paths: Vec::new(),
                target_keys: Vec::new(),
                saved_target_gains: Vec::new(),
                saved_target_enabled: Vec::new(),
                output_solo_tracks: Vec::new(),
                saved_output_solo_gains: Vec::new(),
                meter_solo_tracks: Vec::new(),
                saved_meter_solo_gains: Vec::new(),
                target_insert_enabled_tracks: Vec::new(),
                saved_target_insert_enabled: Vec::new(),
            };
        }

        let mut output_paths = Vec::new();
        let mut meter_paths = Vec::new();
        let mut send_gains = Vec::new();
        let mut insert_enabled_gains = Vec::new();
        let mut target_indices = HashMap::new();
        let mut insert_enabled_target_indices = HashMap::new();
        let mut target_keys = Vec::new();
        let mut saved_target_gains = Vec::new();
        let mut saved_target_enabled = Vec::new();
        let mut output_solo_tracks = Vec::new();
        let mut saved_output_solo_gains = Vec::new();
        let mut meter_solo_tracks = Vec::new();
        let mut saved_meter_solo_gains = Vec::new();
        let mut target_insert_enabled_tracks = Vec::new();
        let mut saved_target_insert_enabled = Vec::new();
        self.collect_live_send_paths(
            source_track,
            MixerSourceRouteTransform::default(),
            &mut HashSet::new(),
            &mut send_gains,
            &mut insert_enabled_gains,
            &mut target_indices,
            &mut insert_enabled_target_indices,
            &mut target_keys,
            &mut saved_target_gains,
            &mut saved_target_enabled,
            &mut Vec::new(),
            &mut output_solo_tracks,
            &mut saved_output_solo_gains,
            &mut meter_solo_tracks,
            &mut saved_meter_solo_gains,
            &mut target_insert_enabled_tracks,
            &mut saved_target_insert_enabled,
            &mut output_paths,
            &mut meter_paths,
        );

        let mut output_transform = MixerSourceRouteTransform::zero();
        for (index, path) in output_paths.iter().enumerate() {
            output_transform.add(path.transform.scaled(
                saved_live_route_path_gain(&path.send_gains, &path.insert_enabled_gains)
                    * saved_output_solo_gains[index],
            ));
        }
        let mut insert_meter_transforms = BTreeMap::new();
        for (index, path) in meter_paths.iter().enumerate() {
            insert_meter_transforms
                .entry(path.track)
                .or_insert_with(MixerSourceRouteTransform::zero)
                .add(path.transform.scaled(
                    saved_live_route_path_gain(&path.send_gains, &path.insert_enabled_gains)
                        * saved_meter_solo_gains[index],
                ));
        }
        MixerLiveSourceRouteData {
            route_plan: MixerSourceRoutePlan {
                output_transform,
                insert_meter_transforms: insert_meter_transforms.into_iter().collect(),
            },
            output_paths: Some(output_paths),
            meter_paths,
            target_keys,
            saved_target_gains,
            saved_target_enabled,
            output_solo_tracks,
            saved_output_solo_gains,
            meter_solo_tracks,
            saved_meter_solo_gains,
            target_insert_enabled_tracks,
            saved_target_insert_enabled,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_live_send_paths(
        &self,
        track: i8,
        path: MixerSourceRouteTransform,
        visited: &mut HashSet<i8>,
        send_gains: &mut Vec<MixerLiveSendGain>,
        insert_enabled_gains: &mut Vec<MixerLiveInsertEnabledGain>,
        target_indices: &mut HashMap<MixerLiveSendTargetKey, usize>,
        insert_enabled_target_indices: &mut HashMap<usize, usize>,
        target_keys: &mut Vec<MixerLiveSendTargetKey>,
        saved_target_gains: &mut Vec<f32>,
        saved_target_enabled: &mut Vec<bool>,
        solo_path_tracks: &mut Vec<usize>,
        output_solo_tracks: &mut Vec<Vec<usize>>,
        saved_output_solo_gains: &mut Vec<f32>,
        meter_solo_tracks: &mut Vec<Vec<usize>>,
        saved_meter_solo_gains: &mut Vec<f32>,
        target_insert_enabled_tracks: &mut Vec<usize>,
        saved_target_insert_enabled: &mut Vec<bool>,
        output_paths: &mut Vec<MixerLiveSendOutputPath>,
        meter_paths: &mut Vec<MixerLiveSendMeterPath>,
    ) {
        if track == 0 {
            let solo_tracks = solo_path_tracks.clone();
            output_paths.push(MixerLiveSendOutputPath {
                transform: path,
                send_gains: send_gains.clone(),
                insert_enabled_gains: insert_enabled_gains.clone(),
            });
            saved_output_solo_gains.push(self.saved_solo_gain_for_path(&solo_tracks));
            output_solo_tracks.push(solo_tracks);
            return;
        }
        let is_source_track = visited.is_empty();
        if !visited.insert(track) {
            return;
        }
        solo_path_tracks.push(track as usize);

        let pushed_insert_enabled = !is_source_track;
        if pushed_insert_enabled {
            let saved_enabled = self.source_enabled.get(&track).copied().unwrap_or(true);
            let target_index = *insert_enabled_target_indices
                .entry(track as usize)
                .or_insert_with(|| {
                    let index = target_insert_enabled_tracks.len();
                    target_insert_enabled_tracks.push(track as usize);
                    saved_target_insert_enabled.push(saved_enabled);
                    index
                });
            insert_enabled_gains.push(MixerLiveInsertEnabledGain {
                target_index: Some(target_index),
                saved_enabled,
            });
        }

        let track_transform = self
            .source_transforms
            .get(&track)
            .copied()
            .unwrap_or_default();
        let path = path.then(MixerSourceRouteTransform::from_insert_transform(
            track_transform,
        ));
        let mut meter_solos = solo_path_tracks.clone();
        self.append_live_solo_reachable_tracks(track, &mut HashSet::new(), &mut meter_solos);
        saved_meter_solo_gains.push(self.saved_solo_gain_for_path(&meter_solos));
        meter_solo_tracks.push(meter_solos);
        meter_paths.push(MixerLiveSendMeterPath {
            track: track as usize,
            transform: path,
            send_gains: send_gains.clone(),
            insert_enabled_gains: insert_enabled_gains.clone(),
        });

        if self.complete_send_sources.contains(&track) {
            if let Some(routes) = self.send_routes_by_source.get(&track) {
                for route in routes {
                    if !route.enabled && !route.live_control {
                        continue;
                    }
                    let saved_gain = mixer_send_gain_raw(route.level_raw).unwrap_or_default();
                    let key = MixerLiveSendTargetKey {
                        source_track: track as usize,
                        destination_track: route.destination_track as usize,
                    };
                    let target_index = if route.live_control {
                        Some(*target_indices.entry(key).or_insert_with(|| {
                            let index = target_keys.len();
                            target_keys.push(key);
                            saved_target_gains.push(saved_gain);
                            saved_target_enabled.push(route.enabled);
                            index
                        }))
                    } else {
                        None
                    };
                    send_gains.push(MixerLiveSendGain {
                        target_index,
                        saved_gain,
                        enabled_target_index: target_index,
                        saved_enabled: route.enabled,
                    });
                    self.collect_live_send_paths(
                        route.destination_track,
                        path,
                        visited,
                        send_gains,
                        insert_enabled_gains,
                        target_indices,
                        insert_enabled_target_indices,
                        target_keys,
                        saved_target_gains,
                        saved_target_enabled,
                        solo_path_tracks,
                        output_solo_tracks,
                        saved_output_solo_gains,
                        meter_solo_tracks,
                        saved_meter_solo_gains,
                        target_insert_enabled_tracks,
                        saved_target_insert_enabled,
                        output_paths,
                        meter_paths,
                    );
                    send_gains.pop();
                }
            }
        } else {
            // Rows without a complete routing map retain FL Studio's default Master path.
            let solo_tracks = solo_path_tracks.clone();
            output_paths.push(MixerLiveSendOutputPath {
                transform: path,
                send_gains: send_gains.clone(),
                insert_enabled_gains: insert_enabled_gains.clone(),
            });
            saved_output_solo_gains.push(self.saved_solo_gain_for_path(&solo_tracks));
            output_solo_tracks.push(solo_tracks);
        }
        if pushed_insert_enabled {
            insert_enabled_gains.pop();
        }
        solo_path_tracks.pop();
        visited.remove(&track);
    }

    fn saved_solo_gain_for_path(&self, path_tracks: &[usize]) -> f32 {
        if self.soloed_insert_tracks.is_empty()
            || path_tracks.iter().any(|track| {
                i8::try_from(*track)
                    .ok()
                    .is_some_and(|track| self.soloed_insert_tracks.contains(&track))
            })
        {
            1.0
        } else {
            0.0
        }
    }

    fn append_live_solo_reachable_tracks(
        &self,
        track: i8,
        visited: &mut HashSet<i8>,
        reachable_tracks: &mut Vec<usize>,
    ) {
        if track <= 0 || !visited.insert(track) {
            return;
        }
        if !reachable_tracks.contains(&(track as usize)) {
            reachable_tracks.push(track as usize);
        }
        if self.complete_send_sources.contains(&track)
            && let Some(routes) = self.send_routes_by_source.get(&track)
        {
            for route in routes {
                if route.enabled || route.live_control {
                    self.append_live_solo_reachable_tracks(
                        route.destination_track,
                        visited,
                        reachable_tracks,
                    );
                }
            }
        }
        visited.remove(&track);
    }

    /// Returns the final pre-Master route plus each insert's summed pre-effect meter path.
    pub(crate) fn source_route_plan_for_channel(&self, route: Option<i8>) -> MixerSourceRoutePlan {
        let Some(track) = route.filter(|track| *track > 0 && self.known_tracks.contains(track))
        else {
            return MixerSourceRoutePlan {
                output_transform: MixerSourceRouteTransform::default(),
                insert_meter_transforms: Vec::new(),
            };
        };
        if !self.complete_send_sources.contains(&track) {
            let transform = MixerSourceRouteTransform::from_insert_transform(
                self.transform_for_channel(Some(track)),
            );
            return MixerSourceRoutePlan {
                output_transform: transform,
                insert_meter_transforms: vec![(track as usize, transform)],
            };
        }

        let mut output = MixerSourceRouteTransform::zero();
        let mut insert_meter_transforms = BTreeMap::new();
        self.accumulate_send_paths(
            track,
            MixerSourceRouteTransform::default(),
            &mut HashSet::new(),
            false,
            &mut output,
            &mut insert_meter_transforms,
        );
        MixerSourceRoutePlan {
            output_transform: output,
            insert_meter_transforms: insert_meter_transforms.into_iter().collect(),
        }
    }

    fn accumulate_send_paths(
        &self,
        track: i8,
        path: MixerSourceRouteTransform,
        visited: &mut HashSet<i8>,
        reached_solo: bool,
        output: &mut MixerSourceRouteTransform,
        insert_meter_transforms: &mut BTreeMap<usize, MixerSourceRouteTransform>,
    ) {
        if track == 0 {
            if self.soloed_insert_tracks.is_empty() || reached_solo {
                output.add(path);
            }
            return;
        }
        if self.disabled_tracks.contains(&track) || !visited.insert(track) {
            return;
        }

        let reached_solo = reached_solo || self.soloed_insert_tracks.contains(&track);
        let track_transform = self
            .source_transforms
            .get(&track)
            .copied()
            .unwrap_or_default();
        let path = path.then(MixerSourceRouteTransform::from_insert_transform(
            track_transform,
        ));
        if self.soloed_insert_tracks.is_empty()
            || reached_solo
            || self.route_can_reach_soloed_insert(track, &mut HashSet::new())
        {
            insert_meter_transforms
                .entry(track as usize)
                .or_insert_with(MixerSourceRouteTransform::zero)
                .add(path);
        }
        if self.complete_send_sources.contains(&track) {
            if let Some(routes) = self.send_routes_by_source.get(&track) {
                for route in routes {
                    if !route.enabled {
                        continue;
                    }
                    let gain = mixer_send_gain_raw(route.level_raw).unwrap_or_default();
                    self.accumulate_send_paths(
                        route.destination_track,
                        path.scaled(gain),
                        visited,
                        reached_solo,
                        output,
                        insert_meter_transforms,
                    );
                }
            }
        } else if self.soloed_insert_tracks.is_empty() || reached_solo {
            // Rows without a complete routing map retain FL Studio's default Master path.
            output.add(path);
        }
        visited.remove(&track);
    }

    pub(crate) fn master_fader_raw(&self) -> Option<i32> {
        self.master_fader_raw
    }

    pub(crate) fn master_pan_raw(&self) -> Option<i32> {
        self.master_pan_raw
    }

    pub(crate) fn master_stereo_separation_raw(&self) -> Option<i32> {
        self.master_stereo_separation_raw
    }
}

fn mixer_send_gain_raw(value: i32) -> Option<f32> {
    (0..=16_000)
        .contains(&value)
        .then_some(value as f32 / 12_800.0)
}

fn saved_live_route_path_gain(
    send_gains: &[MixerLiveSendGain],
    insert_enabled_gains: &[MixerLiveInsertEnabledGain],
) -> f32 {
    let send_gain = send_gains
        .iter()
        .map(|send_gain| send_gain.saved_gain * if send_gain.saved_enabled { 1.0 } else { 0.0 })
        .product::<f32>();
    insert_enabled_gains.iter().fold(send_gain, |gain, insert| {
        gain * if insert.saved_enabled { 1.0 } else { 0.0 }
    })
}

fn candidate_mixer_fader_volume(
    parameters: &[MixerParameterRecord],
    insert_ordinal: usize,
) -> Option<i32> {
    let mut matches = parameters.iter().filter(|record| {
        record.kind() == MixerParameterKind::Volume
            && record.candidate_insert_ordinal() == Some(insert_ordinal)
            && record.target_scope_raw() == 0
    });
    let record = matches.next()?;
    if matches.next().is_some() || !(0..=16_000).contains(&record.value()) {
        return None;
    }
    Some(record.value())
}

fn mixer_fader_gain(volume_raw: i32) -> f32 {
    volume_raw.clamp(0, 16_000) as f32 / 10_000.0
}

fn candidate_mixer_pan(parameters: &[MixerParameterRecord], insert_ordinal: usize) -> Option<i32> {
    let mut matches = parameters.iter().filter(|record| {
        record.kind() == MixerParameterKind::Pan
            && record.candidate_insert_ordinal() == Some(insert_ordinal)
            && record.target_scope_raw() == 0
    });
    let record = matches.next()?;
    if matches.next().is_some() || !(-6_400..=6_400).contains(&record.value()) {
        return None;
    }
    Some(record.value())
}

fn candidate_mixer_stereo_separation(
    parameters: &[MixerParameterRecord],
    insert_ordinal: usize,
) -> Option<i32> {
    let mut matches = parameters.iter().filter(|record| {
        record.kind() == MixerParameterKind::StereoSeparation
            && record.candidate_insert_ordinal() == Some(insert_ordinal)
            && record.target_scope_raw() == 0
    });
    let record = matches.next()?;
    if matches.next().is_some() || !(-64..=64).contains(&record.value()) {
        return None;
    }
    Some(record.value())
}

fn apply_mixer_stereo_separation(frame: &mut [f32; 2], separation_raw: i32) {
    apply_mixer_stereo_separation_raw_f32(frame, separation_raw as f32);
}

pub(crate) fn apply_mixer_stereo_separation_raw_f32(frame: &mut [f32; 2], separation_raw: f32) {
    let separation_raw = if separation_raw.is_finite() {
        separation_raw.clamp(-64.0, 64.0)
    } else {
        0.0
    };
    let mid = (frame[0] + frame[1]) * 0.5;
    let side = (frame[0] - frame[1]) * 0.5;
    let side_gain = 1.0 - separation_raw / 64.0;
    frame[0] = mid + side * side_gain;
    frame[1] = mid - side * side_gain;
}

pub(crate) fn apply_mixer_signal_flags_f32(frame: &mut [f32; 2], polarity_mix: f32, swap_mix: f32) {
    let polarity_mix = if polarity_mix.is_finite() {
        polarity_mix.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let swap_mix = if swap_mix.is_finite() {
        swap_mix.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let input = *frame;
    let polarity_gain = 1.0 - 2.0 * polarity_mix;
    frame[0] = (input[0] + (input[1] - input[0]) * swap_mix) * polarity_gain;
    frame[1] = (input[1] + (input[0] - input[1]) * swap_mix) * polarity_gain;
}

fn apply_mixer_pan(frame: &mut [f32; 2], pan_raw: i32, pan_law_raw: Option<u8>) {
    apply_mixer_pan_raw_f32(frame, pan_raw as f32, pan_law_raw);
}

pub(crate) fn apply_mixer_pan_raw_f32(frame: &mut [f32; 2], pan_raw: f32, pan_law_raw: Option<u8>) {
    let pan = if pan_raw.is_finite() {
        pan_raw.clamp(-6_400.0, 6_400.0) / 6_400.0
    } else {
        0.0
    };
    let left = frame[0];
    let right = frame[1];
    if pan < 0.0 {
        let mix = -pan;
        frame[0] = left + right * mix;
        frame[1] = right * (1.0 - mix);
    } else {
        frame[0] = left * (1.0 - pan);
        frame[1] = right + left * pan;
    }
    if pan_law_raw != Some(2) {
        // Use the maximum side gain from the existing equal-power mono curve as the
        // overall Circular compensation for this stereo crossmix; native Mixer coefficients
        // still need comparison against FL Studio output.
        let circular_gain = ((1.0 - pan.abs()) * std::f32::consts::FRAC_PI_4).cos();
        frame[0] *= circular_gain;
        frame[1] *= circular_gain;
    }
}

/// Recognized parameter-ID interpretations for Mixer `0xE1` records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameterKind {
    SlotEnabled,
    SlotMix,
    RouteVolume,
    Volume,
    Pan,
    StereoSeparation,
    LowEqGain,
    MidEqGain,
    HighEqGain,
    LowEqFrequency,
    MidEqFrequency,
    HighEqFrequency,
    LowEqQ,
    MidEqQ,
    HighEqQ,
    Unknown(u8),
}

/// One 12-byte Mixer parameter record from a `0xE1` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerParameterRecord {
    event_index: usize,
    record_index: usize,
    prefix: [u8; 4],
    parameter_id: u8,
    reserved: u8,
    channel_data: u16,
    value: i32,
}

/// Fields that can be toggled in an existing Mixer insert flag record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MixerInsertEdit {
    pub enabled: Option<bool>,
    pub soloed: Option<bool>,
    pub polarity_reversed: Option<bool>,
    pub swap_left_right: Option<bool>,
    pub effects_enabled: Option<bool>,
}

impl MixerParameterRecord {
    pub fn event_index(&self) -> usize {
        self.event_index
    }

    pub fn record_index(&self) -> usize {
        self.record_index
    }

    /// Four leading bytes retained without assigning them a meaning.
    pub fn prefix(&self) -> [u8; 4] {
        self.prefix
    }

    pub fn parameter_id(&self) -> u8 {
        self.parameter_id
    }

    /// Interprets parameter IDs recognized by an independent FLP parser.
    pub fn kind(&self) -> MixerParameterKind {
        match self.parameter_id {
            0 => MixerParameterKind::SlotEnabled,
            1 => MixerParameterKind::SlotMix,
            64..=191 => MixerParameterKind::RouteVolume,
            192 => MixerParameterKind::Volume,
            193 => MixerParameterKind::Pan,
            194 => MixerParameterKind::StereoSeparation,
            208 => MixerParameterKind::LowEqGain,
            209 => MixerParameterKind::MidEqGain,
            210 => MixerParameterKind::HighEqGain,
            216 => MixerParameterKind::LowEqFrequency,
            217 => MixerParameterKind::MidEqFrequency,
            218 => MixerParameterKind::HighEqFrequency,
            224 => MixerParameterKind::LowEqQ,
            225 => MixerParameterKind::MidEqQ,
            226 => MixerParameterKind::HighEqQ,
            value => MixerParameterKind::Unknown(value),
        }
    }

    /// Candidate target index extracted from bits 6â€“12 of `channel_data`.
    /// Its relationship to visible Mixer insert numbering still needs broader validation.
    pub fn target_index(&self) -> u8 {
        ((self.channel_data >> 6) & 0x7F) as u8
    }

    /// Corpus-derived insert ordinal candidate for this target index.
    ///
    /// Across the inspected named Mixer inserts, target 64 corresponded to the first
    /// insert and subsequent targets advanced in order. Projects with missing or
    /// ambiguous parameter records still require a separate mapping check.
    pub fn candidate_insert_ordinal(&self) -> Option<usize> {
        self.target_index().checked_sub(64).map(usize::from)
    }

    /// Candidate slot index extracted from bits 0â€“5 of `channel_data`.
    pub fn slot_index(&self) -> u8 {
        (self.channel_data & 0x3F) as u8
    }

    /// Uninterpreted top three bits of `channel_data`.
    pub fn target_scope_raw(&self) -> u8 {
        (self.channel_data >> 13) as u8
    }

    pub fn reserved(&self) -> u8 {
        self.reserved
    }

    /// Raw little-endian word retained without assigning its bit fields a meaning.
    pub fn channel_data(&self) -> u16 {
        self.channel_data
    }

    /// Raw signed parameter value.
    pub fn value(&self) -> i32 {
        self.value
    }
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
    pub muted: bool,
    pub header_bytes: [u8; 4],
    pub start_offset: f32,
    pub end_offset: f32,
    pub source_window_ticks: Option<(u32, u32)>,
    pub clip_id: Option<u32>,
    pub reserved: Vec<u8>,
    pub scale: Option<f64>,
    pub trailing_bytes: Vec<u8>,
    pub record_size: usize,
    source_event_index: usize,
    source_record_index: usize,
}

impl PlaylistClip {
    /// Whether this Playlist clip's individual mute flag is set.
    pub fn is_muted(&self) -> bool {
        self.muted
    }

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

    /// Returns a decoded legacy source window in milliseconds. The 88-byte
    /// record stores its source window as ticks instead. A `-1..-1` legacy
    /// window means the full source and returns `None` here.
    pub fn source_window_milliseconds(&self) -> Option<(f64, f64)> {
        if self.start_offset.is_finite()
            && self.end_offset.is_finite()
            && self.start_offset >= 0.0
            && self.end_offset > self.start_offset
        {
            return Some((f64::from(self.start_offset), f64::from(self.end_offset)));
        }
        None
    }

    /// Returns the FL Studio 2026 source cut window for an 88-byte record, in ticks.
    pub fn source_window_ticks(&self) -> Option<(u32, u32)> {
        self.source_window_ticks
    }

    pub fn uses_full_source_window(&self) -> bool {
        if self.record_size == 88 {
            return match self.target() {
                PlaylistClipTarget::Pattern { .. } => {
                    self.source_window_ticks == Some((0, self.length_ticks))
                }
                PlaylistClipTarget::Channel { .. } => true,
            };
        }
        self.start_offset == -1.0 && self.end_offset == -1.0
    }

    /// Whether this clip's timeline length can be changed using decoded fields.
    pub fn can_resize_timeline_length(&self) -> bool {
        if !self.has_opaque_record_tail() {
            return true;
        }
        matches!(self.target(), PlaylistClipTarget::Pattern { .. })
            && self.source_window_ticks == Some((0, self.length_ticks))
    }

    /// The 88-byte Playlist record tail is preserved verbatim except for the
    /// decoded tick source window and candidate mute flag; other fields remain
    /// opaque.
    pub fn has_opaque_record_tail(&self) -> bool {
        self.record_size == 88
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistClipTarget {
    Pattern { id: u16 },
    Channel { id: u16 },
}

/// An opaque, lossless copy of one Playlist clip record for later insertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistClipClipboard {
    raw_record: Vec<u8>,
}

/// Lossless clipboard for a set of Playlist clips with their relative layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistClipSelectionClipboard {
    clips: Vec<(PlaylistClipClipboard, u32, u16)>,
    origin_position_ticks: u32,
    origin_raw_track_index: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaylistClipEdit {
    pub position_ticks: Option<u32>,
    pub length_ticks: Option<u32>,
    pub item_index: Option<u16>,
    pub raw_track_index: Option<u16>,
    pub group: Option<u16>,
    pub item_flags: Option<u16>,
    pub muted: Option<bool>,
    pub start_offset: Option<f32>,
    pub end_offset: Option<f32>,
    /// Candidate source-window tick bounds for one 88-byte Audio Clip record.
    pub source_window_ticks: Option<(u32, u32)>,
    pub scale: Option<f64>,
}

impl PatternNote {
    /// Whether this note carries FL Studio's slide-note flag.
    pub fn is_slide_note(&self) -> bool {
        self.flags & PATTERN_NOTE_SLIDE_FLAG != 0
    }

    /// Whether this note carries the candidate FL Studio mute flag.
    pub fn is_muted(&self) -> bool {
        self.flags & PATTERN_NOTE_MUTED_FLAG != 0
    }

    /// The note's fine-pitch offset in cents.
    ///
    /// FL Studio stores this as an unsigned byte centered at 120. Values 0 through 240 map to
    /// -1200 through +1200 cents in ten-cent steps. Out-of-range raw values are retained for
    /// lossless round-tripping but ignored during playback.
    pub fn fine_pitch_offset_cents(&self) -> i16 {
        if self.fine_pitch > 240 {
            return 0;
        }
        (i16::from(self.fine_pitch) - 120) * 10
    }

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
        if let Some(value) = edit.slide {
            if value {
                self.flags |= PATTERN_NOTE_SLIDE_FLAG;
            } else {
                self.flags &= !PATTERN_NOTE_SLIDE_FLAG;
            }
        }
        if let Some(value) = edit.muted {
            if value {
                self.flags |= PATTERN_NOTE_MUTED_FLAG;
            } else {
                self.flags &= !PATTERN_NOTE_MUTED_FLAG;
            }
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

impl Default for PatternNote {
    fn default() -> Self {
        Self {
            position: 0,
            flags: PATTERN_NOTE_DEFAULT_FLAGS,
            channel_id: 0,
            length: 0,
            key: 60,
            group: 0,
            fine_pitch: 120,
            reserved: 0,
            release: PATTERN_NOTE_DEFAULT_RELEASE,
            midi_channel: 0,
            pan: 64,
            velocity: 100,
            mod_x: 128,
            mod_y: 128,
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

    /// Raw global Channel Rack swing mix from the project-level `0x0B` byte event.
    pub fn global_swing_mix_raw(&self) -> Option<u8> {
        self.global_swing_mix
    }

    /// Global Channel Rack swing mix in FL's 0..=128 range; missing events default to zero.
    pub fn global_swing_mix(&self) -> u8 {
        self.global_swing_mix.unwrap_or(0)
    }

    /// Raw project pan-law value from the global `0x17` byte event.
    /// FL Studio uses 0 for Circular (the default) and 2 for Triangular.
    pub fn pan_law_raw(&self) -> Option<u8> {
        self.pan_law_raw
    }

    /// Project Master pitch in cents from its global signed-word `0x50` event.
    /// An absent event uses FL Studio's zero-cent default.
    pub fn master_pitch_cents(&self) -> Option<i16> {
        self.master_pitch_cents
    }

    pub fn build_number(&self) -> Option<u32> {
        self.build_number
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    pub fn comments(&self) -> Option<&str> {
        self.comments.as_deref()
    }

    pub fn genre(&self) -> Option<&str> {
        self.genre.as_deref()
    }

    pub fn web_link(&self) -> Option<&str> {
        self.web_link.as_deref()
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

        let metadata = read_project_metadata(&events, project_version.as_deref());

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
                0x80 if event.encoding == PayloadEncoding::Dword && event.payload.len() == 4 => {
                    channel.color = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x00 if event.payload.len() == 1 => {
                    channel.enabled = Some(event.payload[0] != 0);
                }
                0x0F if event.payload.len() == 1 => {
                    channel.zipped = event.payload[0] != 0;
                }
                0x14 if event.payload.len() == 1 => {
                    channel.ping_pong_loop = event.payload[0] != 0;
                }
                0x46 if event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                    && channel.sampler_fx_flags.is_none() =>
                {
                    channel.sampler_fx_flags =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x4B if event.encoding == PayloadEncoding::Word && event.payload.len() == 2 => {
                    channel.sampler_fade_out_raw.get_or_insert_with(|| {
                        u16::from_le_bytes([event.payload[0], event.payload[1]])
                    });
                }
                0x4C if event.encoding == PayloadEncoding::Word && event.payload.len() == 2 => {
                    channel.sampler_fade_in_raw.get_or_insert_with(|| {
                        u16::from_le_bytes([event.payload[0], event.payload[1]])
                    });
                }
                0xDA if matches!(event.encoding, PayloadEncoding::Data { .. }) => {
                    // PyFLP documents five ordered 0xDA records (Panning, Volume,
                    // Mod X, Mod Y, Pitch).
                    let target_index = usize::from(channel.envelope_lfo_events_seen);
                    if channel.kind == Some(ChannelType::Sampler.raw())
                        && let Some(target) = SamplerEnvelopeLfoTarget::from_index(target_index)
                    {
                        channel.sampler_envelope_lfos[target_index] =
                            decode_sampler_envelope_lfo(&event.payload, target);
                    }
                    channel.envelope_lfo_events_seen =
                        channel.envelope_lfo_events_seen.saturating_add(1);
                }
                0xDD if matches!(event.encoding, PayloadEncoding::Data { .. }) => {
                    // PyFLP maps 0xDD to Max Polyphony (u32), Slide (u32), and
                    // flags (u8), with Mono stored in bit 0 of flags.
                    if event.payload.len() >= 4 && channel.sampler_max_polyphony_raw.is_none() {
                        channel.sampler_max_polyphony_raw = Some(u32::from_le_bytes(
                            event.payload[..4]
                                .try_into()
                                .expect("the polyphony maximum has four bytes"),
                        ));
                    }
                    if event.payload.len() >= 9 && channel.sampler_mono_enabled.is_none() {
                        channel.sampler_mono_enabled = Some(event.payload[8] & 1 != 0);
                    }
                }
                0x84 if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.channel_cut_groups.is_none() =>
                {
                    channel.channel_cut_groups = Some((
                        u16::from_le_bytes([event.payload[0], event.payload[1]]),
                        u16::from_le_bytes([event.payload[2], event.payload[3]]),
                    ));
                }
                0x8F if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.sampler_flags.is_none() =>
                {
                    channel.sampler_flags = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x87 if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.sampler_root_note.is_none() =>
                {
                    channel.sampler_root_note = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x8E if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.keyboard_fine_tune_raw.is_none() =>
                {
                    channel.keyboard_fine_tune_raw = Some(i32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0xC7 if matches!(event.encoding, PayloadEncoding::Data { .. })
                    && ascii_version(&event.payload).is_none() =>
                {
                    if event.payload.len() >= 116 && channel.stretch_settings.is_none() {
                        channel.stretch_settings = Some(ChannelStretchSettings {
                            time_raw: u32::from_le_bytes(
                                event.payload[100..104]
                                    .try_into()
                                    .expect("the stretch-time field has four bytes"),
                            ),
                            pitch_raw: i32::from_le_bytes(
                                event.payload[104..108]
                                    .try_into()
                                    .expect("the stretch-pitch field has four bytes"),
                            ),
                            multiplier_raw: i32::from_le_bytes(
                                event.payload[108..112]
                                    .try_into()
                                    .expect("the stretch-multiplier field has four bytes"),
                            ),
                            mode_raw: i32::from_le_bytes(
                                event.payload[112..116]
                                    .try_into()
                                    .expect("the stretch-mode field has four bytes"),
                            ),
                        });
                    }
                    if event.payload.len() >= 76 && channel.keyboard_key_region.is_none() {
                        let low = u32::from_le_bytes(
                            event.payload[68..72]
                                .try_into()
                                .expect("the key-region low bound has four payload bytes"),
                        );
                        let high = u32::from_le_bytes(
                            event.payload[72..76]
                                .try_into()
                                .expect("the key-region high bound has four payload bytes"),
                        );
                        channel.keyboard_key_region = Some((low, high));
                    }
                    if channel.channel_type() == Some(ChannelType::Layer)
                        && event.payload.len() >= 92
                        && channel.layer_fade_raw.is_none()
                    {
                        channel.layer_fade_raw = Some(u32::from_le_bytes(
                            event.payload[88..92]
                                .try_into()
                                .expect("the Layer Fade field has four payload bytes"),
                        ));
                    }
                    if event.payload.len() >= 12 && channel.keyboard_main_pitch.is_none() {
                        channel.keyboard_main_pitch = Some(event.payload[11] != 0);
                    }
                    if event.payload.len() >= 64 && channel.keyboard_add_root.is_none() {
                        channel.keyboard_add_root = Some(event.payload[63] != 0);
                    }
                }
                0xD5 if matches!(event.encoding, PayloadEncoding::Data { .. }) => {
                    if let Some(level_adjustments) = ChannelLevelAdjustments::decode(&event.payload)
                    {
                        channel.level_adjustments.push(level_adjustments);
                    }
                }
                0x16 if event.encoding == PayloadEncoding::Byte && event.payload.len() == 1 => {
                    channel.mixer_track = Some(event.payload[0] as i8);
                }
                0x61 if event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                    && channel.swing_mix.is_none() =>
                {
                    channel.swing_mix =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x91 if event.encoding == PayloadEncoding::Dword
                    && event.payload.len() == 4
                    && channel.group_number.is_none() =>
                {
                    channel.group_number = Some(i32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                0x15 if event.payload.len() == 1 => channel.kind = Some(event.payload[0]),
                0x5E if event.payload.len() == 2 => {
                    channel
                        .layer_children
                        .push(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
                0x90 if event.payload.len() == 4 => {
                    channel.layer_flags = Some(u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword event has four payload bytes"),
                    ));
                }
                // FL 25+ stores channel pan and volume at the start of the 0xDB
                // Levels data event. Keep legacy byte/word values as fallbacks.
                0xDB if event.payload.len() >= 8 => {
                    channel.levels_editable = true;
                    if channel.pan_priority < 3 {
                        channel.pan = Some(i32::from_le_bytes(
                            event.payload[0..4]
                                .try_into()
                                .expect("the 0xDB pan field has four bytes"),
                        ));
                        channel.pan_priority = 3;
                    }
                    if channel.volume_priority < 3 {
                        channel.volume = Some(u32::from_le_bytes(
                            event.payload[4..8]
                                .try_into()
                                .expect("the 0xDB volume field has four bytes"),
                        ));
                        channel.volume_priority = 3;
                    }
                }
                0x48 if event.payload.len() == 2 && channel.volume_priority < 2 => {
                    channel.volume = Some(u32::from(u16::from_le_bytes([
                        event.payload[0],
                        event.payload[1],
                    ])));
                    channel.volume_priority = 2;
                }
                0x49 if event.payload.len() == 2 && channel.pan_priority < 2 => {
                    channel.pan = Some(i32::from(u16::from_le_bytes([
                        event.payload[0],
                        event.payload[1],
                    ])));
                    channel.pan_priority = 2;
                }
                0x02 if event.payload.len() == 1 && channel.volume_priority < 1 => {
                    channel.volume = Some(u32::from(event.payload[0]));
                    channel.volume_priority = 1;
                }
                0x03 if event.payload.len() == 1 && channel.pan_priority < 1 => {
                    channel.pan = Some(i32::from(event.payload[0]));
                    channel.pan_priority = 1;
                }
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
                // Sampler channels (kind 0) and sample-backed audio channels (kind 4)
                // both use `0xC4` for the source path. Other channel kinds can contain
                // unrelated data in their event range, so keep the scope check narrow.
                0xC4 if matches!(channel.kind, Some(0 | 4)) && channel.sample_path.is_none() => {
                    channel.sample_path =
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

    /// Returns Channel Rack display groups declared before the first channel marker.
    /// The same `0xE7` opcode is also used elsewhere in the project stream, so only the
    /// project header section is treated as display-group metadata.
    pub fn channel_groups(&self) -> Vec<ChannelGroupSummary> {
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        self.events[..first_channel]
            .iter()
            .filter(|event| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .enumerate()
            .map(|(index, event)| ChannelGroupSummary {
                index: i32::try_from(index).unwrap_or(i32::MAX),
                name: decode_project_string(&event.payload, self.project_version.as_deref())
                    .filter(|name| !name.is_empty()),
            })
            .collect()
    }

    fn project_string_encoding(&self) -> Option<bool> {
        let utf16_from_version = project_string_version(self.project_version.as_deref())
            .map(|(major, minor)| major > 11 || (major == 11 && minor >= 5));
        utf16_from_version.or_else(|| {
            let first_channel = self
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(self.events.len());
            self.events.iter().enumerate().find_map(|(index, event)| {
                let is_project_string = matches!(
                    event.opcode,
                    0xC1 | 0xC2 | 0xC3 | 0xC4 | 0xC5 | 0xCB | 0xCE | 0xCF
                ) || (event.opcode == 0xE7 && index < first_channel);
                (is_project_string && matches!(event.encoding, PayloadEncoding::Data { .. }))
                    .then(|| project_string_is_utf16(&event.payload, None))
            })
        })
    }

    /// Creates a Channel Rack display group when `name` is new and assigns every listed channel
    /// to it. Existing groups are matched by exact name. All edits are applied atomically while
    /// preserving unrelated event bytes.
    pub fn group_channels(&mut self, channel_ids: &[u16], name: &str) -> Result<i32, FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        if channel_ids.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "at least one Channel Rack channel must be selected",
            ));
        }

        let mut selected_ids = BTreeSet::new();
        let channels = self.channels();
        for channel_id in channel_ids {
            if !selected_ids.insert(*channel_id) {
                return Err(FlpError::UnsupportedEdit(
                    "a Channel Rack channel was selected more than once",
                ));
            }
            let mut matching = channels
                .iter()
                .filter(|channel| channel.id() == *channel_id);
            if matching.next().is_none() {
                return Err(FlpError::ChannelNotFound(*channel_id));
            }
            if matching.next().is_some() {
                return Err(FlpError::AmbiguousChannelId(*channel_id));
            }
        }

        let groups = self.channel_groups();
        let matching_groups = groups
            .iter()
            .filter(|group| group.name() == Some(name))
            .map(ChannelGroupSummary::index)
            .collect::<Vec<_>>();
        if matching_groups.len() > 1 {
            return Err(FlpError::UnsupportedEdit(
                "multiple Channel Rack groups have the requested name",
            ));
        }
        let (group_index, create_group) = if let Some(index) = matching_groups.first() {
            (*index, false)
        } else {
            (
                i32::try_from(groups.len()).map_err(|_| {
                    FlpError::UnsupportedEdit("the project has too many Channel Rack groups")
                })?,
                true,
            )
        };

        let mut candidate = self.clone();
        if create_group {
            let utf16 = self
                .project_string_encoding()
                .ok_or(FlpError::UnsupportedEdit(
                    "the project's string encoding cannot be inferred for a new Channel Rack group",
                ))?;
            let name_payload = encode_project_string(name, utf16)?;
            let group_event = FlpEvent::new_data(0xE7, name_payload)?;
            let first_channel = candidate
                .events
                .iter()
                .position(|event| event.opcode == 0x40)
                .unwrap_or(candidate.events.len());
            let insertion_index = candidate.events[..first_channel]
                .iter()
                .rposition(|event| {
                    event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
                })
                .map_or(first_channel, |index| index + 1);
            candidate.events.insert(insertion_index, group_event);
        }

        let mut selected_channels = candidate
            .channels()
            .into_iter()
            .filter(|channel| selected_ids.contains(&channel.id()))
            .collect::<Vec<_>>();
        selected_channels.sort_by_key(|channel| std::cmp::Reverse(channel.first_event_index));
        let mut changed = create_group;
        let group_number_payload = u32::from_le_bytes(group_index.to_le_bytes());
        for channel in selected_channels {
            let mut group_events = channel
                .event_range()
                .filter(|index| candidate.events[*index].opcode == 0x91);
            let existing_index = group_events.next();
            if group_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "a selected channel has multiple display-group events",
                ));
            }
            if let Some(event_index) = existing_index {
                let event = &candidate.events[event_index];
                if event.encoding != PayloadEncoding::Dword || event.payload.len() != 4 {
                    return Err(FlpError::UnsupportedEdit(
                        "a selected channel's display-group event is not a dword",
                    ));
                }
                let current_index = i32::from_le_bytes(
                    event.payload[..4]
                        .try_into()
                        .expect("a dword event has four payload bytes"),
                );
                if current_index != group_index {
                    candidate.events[event_index].replace_dword_payload(group_number_payload)?;
                    changed = true;
                }
            } else {
                candidate.events.insert(
                    channel.event_range().end,
                    FlpEvent::new_dword(0x91, group_number_payload),
                );
                changed = true;
            }
        }
        if changed {
            candidate.refresh_event_offsets()?;
            *self = candidate;
        }
        Ok(group_index)
    }

    /// Adds an empty Channel Rack display group and returns its zero-based index.
    pub fn add_channel_group(&mut self, name: &str) -> Result<i32, FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        let groups = self.channel_groups();
        if groups.iter().any(|group| group.name() == Some(name)) {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group already has the requested name",
            ));
        }
        let group_index = i32::try_from(groups.len()).map_err(|_| {
            FlpError::UnsupportedEdit("the project has too many Channel Rack groups")
        })?;
        let utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a new Channel Rack group",
            ))?;
        let group_event = FlpEvent::new_data(0xE7, encode_project_string(name, utf16)?)?;
        let mut candidate = self.clone();
        let first_channel = candidate
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(candidate.events.len());
        let insertion_index = candidate.events[..first_channel]
            .iter()
            .rposition(|event| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .map_or(first_channel, |index| index + 1);
        candidate.events.insert(insertion_index, group_event);
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(group_index)
    }

    /// Renames a Channel Rack display group without changing its channel assignments.
    pub fn rename_channel_group(&mut self, group_index: i32, name: &str) -> Result<(), FlpError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group needs a non-empty name without embedded NUL characters",
            ));
        }
        let groups = self.channel_groups();
        let Some(group) = groups.iter().find(|group| group.index() == group_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ));
        };
        if group.name() == Some(name) {
            return Ok(());
        }
        if groups
            .iter()
            .any(|other| other.index() != group_index && other.name() == Some(name))
        {
            return Err(FlpError::UnsupportedEdit(
                "a Channel Rack group already has the requested name",
            ));
        }
        let utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a renamed Channel Rack group",
            ))?;
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let group_event_index = self.events[..first_channel]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .nth(usize::try_from(group_index).map_err(|_| {
                FlpError::UnsupportedEdit("the requested Channel Rack group does not exist")
            })?)
            .map(|(index, _)| index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ))?;
        let mut candidate = self.clone();
        candidate.events[group_event_index]
            .replace_data_payload(encode_project_string(name, utf16)?)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Deletes a Channel Rack group, unassigns its channels, and shifts higher group indexes down.
    pub fn delete_channel_group(&mut self, group_index: i32) -> Result<(), FlpError> {
        if !self
            .channel_groups()
            .iter()
            .any(|group| group.index() == group_index)
        {
            return Err(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ));
        }
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let group_event_index = self.events[..first_channel]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .nth(usize::try_from(group_index).map_err(|_| {
                FlpError::UnsupportedEdit("the requested Channel Rack group does not exist")
            })?)
            .map(|(index, _)| index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Channel Rack group does not exist",
            ))?;

        let mut candidate = self.clone();
        candidate.events.remove(group_event_index);
        let mut channels = candidate.channels();
        channels.sort_by_key(|channel| std::cmp::Reverse(channel.first_event_index));
        for channel in channels {
            let mut group_events = channel
                .event_range()
                .filter(|index| candidate.events[*index].opcode == 0x91);
            let existing_index = group_events.next();
            if group_events.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "a channel has multiple display-group events",
                ));
            }
            let Some(event_index) = existing_index else {
                continue;
            };
            let event = &candidate.events[event_index];
            if event.encoding != PayloadEncoding::Dword || event.payload.len() != 4 {
                return Err(FlpError::UnsupportedEdit(
                    "a channel's display-group event is not a dword",
                ));
            }
            let current_index = i32::from_le_bytes(
                event.payload[..4]
                    .try_into()
                    .expect("a dword event has four payload bytes"),
            );
            if current_index == group_index {
                candidate.events.remove(event_index);
            } else if current_index > group_index {
                candidate.events[event_index].replace_dword_payload((current_index - 1) as u32)?;
            }
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Returns the points found in each type-5 automation channel's `0xEA` blob.
    /// The full event remains byte-exact in `events()`; the 17-byte header,
    /// per-point trailing bytes, and any era-specific trailer are not discarded.
    pub fn automation_channels(&self) -> Result<Vec<AutomationChannel>, FlpError> {
        self.channels()
            .into_iter()
            .filter(|channel| channel.kind == Some(5))
            .map(|channel| {
                let event_index = channel
                    .event_range()
                    .find(|index| self.events[*index].opcode == 0xEA);
                let points = match event_index {
                    Some(index) => decode_automation_points(&self.events[index])?,
                    None => Vec::new(),
                };
                Ok(AutomationChannel {
                    channel_id: channel.id,
                    display_name: channel.display_name,
                    points,
                    data_event_index: event_index,
                })
            })
            .collect()
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

    /// Replaces field 53 of a marker-12 VST3 channel state while retaining its
    /// wrapper metadata, identity fields, and every unrelated field byte-for-byte.
    pub fn replace_vst3_channel_state_bytes(
        &mut self,
        channel_id: u16,
        nested_state: &[u8],
    ) -> Result<(), FlpError> {
        let mut candidate = self.clone();
        let channel = candidate
            .channels()
            .into_iter()
            .find(|channel| channel.id() == channel_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the VST3 state channel does not exist",
            ))?;
        let data_events = channel
            .event_range()
            .filter(|index| candidate.events[*index].opcode == 0xD5)
            .collect::<Vec<_>>();
        let [data_event_index] = data_events.as_slice() else {
            return Err(FlpError::UnsupportedEdit(
                "the channel must contain exactly one VST plug-in data event",
            ));
        };
        let state = candidate
            .channel_plugin_states()
            .into_iter()
            .find(|state| state.channel_id() == channel_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the channel VST plug-in data event could not be decoded",
            ))?;
        let Some(metadata) = state.vst_metadata() else {
            return Err(FlpError::UnsupportedEdit(
                "the channel has an unsupported VST plug-in state envelope",
            ));
        };
        if metadata.format_marker() != 12 || metadata.fourcc().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "VST3 state write-back requires a marker-12 VST3 envelope",
            ));
        }
        if metadata.class_uid().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the channel VST3 state has no valid class UID",
            ));
        }
        let replacement_payload = replace_vst_state_field(
            state.data_payload(),
            metadata.state_data_range.as_ref(),
            nested_state,
        )?;
        candidate.events[*data_event_index].replace_data_payload(replacement_payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Returns pattern metadata, note records, and raw controller points.
    pub fn patterns(&self) -> Result<Vec<Pattern>, FlpError> {
        let mut patterns = Vec::<Pattern>::new();
        let mut pattern_indices = HashMap::<u16, usize>::new();
        let mut current_pattern = None;
        let mut current_time_marker = None;
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
                    current_time_marker = None;

                    if let Some(notes_event) = self.events.get(event_index + 1)
                        && Self::is_pattern_note_event(notes_event)
                    {
                        if !notes_event
                            .payload
                            .len()
                            .is_multiple_of(FLP_NOTE_RECORD_SIZE)
                        {
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
                0x40 | 0x62 | 0x63 => {
                    current_pattern = None;
                    current_time_marker = None;
                }
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
                0xDF => {
                    if let Some(pattern_index) = current_pattern {
                        if !event
                            .payload
                            .len()
                            .is_multiple_of(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
                        {
                            return Err(FlpError::InvalidEvent {
                                offset: event.file_offset,
                                detail: "pattern controller payload is not a whole number of 12-byte records",
                            });
                        }
                        patterns[pattern_index].controllers.extend(
                            event
                                .payload
                                .as_chunks::<FLP_PATTERN_CONTROLLER_RECORD_SIZE>()
                                .0
                                .iter()
                                .map(|record| PatternController {
                                    position: u32::from_le_bytes(
                                        record[..4]
                                            .try_into()
                                            .expect("a Pattern controller position has four bytes"),
                                    ),
                                    reserved: [record[4], record[5]],
                                    channel: record[6],
                                    flags: record[7],
                                    value_bits: u32::from_le_bytes(
                                        record[8..12]
                                            .try_into()
                                            .expect("a Pattern controller value has four bytes"),
                                    ),
                                }),
                        );
                    }
                }
                0x94 if event.payload.len() == 4 => {
                    current_time_marker = current_pattern.map(|pattern_index| {
                        let marker_index = patterns[pattern_index].time_markers.len();
                        patterns[pattern_index].time_markers.push(TimeMarker {
                            raw_position: u32::from_le_bytes(
                                event.payload[..4]
                                    .try_into()
                                    .expect("a dword time-marker event has four bytes"),
                            ),
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        (pattern_index, marker_index)
                    });
                }
                0x21 if event.payload.len() == 1 => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.numerator = Some(event.payload[0]);
                        marker.source_events.numerator = Some(event_index);
                    }
                }
                0x22 if event.payload.len() == 1 => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.denominator = Some(event.payload[0]);
                        marker.source_events.denominator = Some(event_index);
                    }
                }
                0xCD => {
                    if let Some((pattern_index, marker_index)) = current_time_marker
                        && let Some(marker) = patterns
                            .get_mut(pattern_index)
                            .and_then(|pattern| pattern.time_markers.get_mut(marker_index))
                    {
                        marker.name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                        marker.source_events.name = Some(event_index);
                    }
                    current_time_marker = None;
                }
                _ => {}
            }
            event_index += 1;
        }
        patterns.sort_by_key(|pattern| pattern.id);
        Ok(patterns)
    }

    /// Creates one raw point in a new controller stream, inserting a `0xDF` event if needed.
    /// The new point starts with zeroed reserved bytes; channel and flags are kept uninterpreted.
    pub fn create_pattern_controller_stream(
        &mut self,
        pattern_id: u16,
        channel: u8,
        flags: u8,
        position: u32,
        value: f32,
    ) -> Result<usize, FlpError> {
        if !value.is_finite() {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }

        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        if pattern
            .controllers
            .iter()
            .any(|controller| (controller.channel, controller.flags) == (channel, flags))
        {
            return Err(FlpError::UnsupportedEdit(
                "the requested raw Pattern controller stream already exists",
            ));
        }

        let mut matching_markers = self.events.iter().enumerate().filter_map(|(index, event)| {
            (event.opcode == 0x41
                && event.payload.len() == 2
                && event.payload.as_slice() == pattern_id.to_le_bytes())
            .then_some(index)
        });
        let marker_index = matching_markers.next().ok_or(FlpError::UnsupportedEdit(
            "the requested Pattern has no source marker",
        ))?;
        if matching_markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern ID is ambiguous",
            ));
        }

        let region_start = marker_index + 1;
        let region_end = self.events[region_start..]
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.payload.len() == 2)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .map_or(self.events.len(), |offset| region_start + offset);
        let new_index = pattern.controllers.len();
        let controller = PatternController {
            position,
            reserved: [0; 2],
            channel,
            flags,
            value_bits: value.to_bits(),
        };

        let mut candidate = self.clone();
        if let Some(event_index) = (region_start..region_end)
            .rev()
            .find(|index| candidate.events[*index].opcode == 0xDF)
        {
            let mut payload = candidate.events[event_index].payload.clone();
            payload
                .len()
                .checked_add(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
                .ok_or(FlpError::LengthOverflow)?;
            payload.extend_from_slice(&controller.encode());
            candidate.events[event_index].replace_data_payload(payload)?;
        } else {
            candidate.events.insert(
                region_end,
                FlpEvent::new_data(0xDF, controller.encode().to_vec())?,
            );
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_index)
    }

    /// Edits position or value in one raw Pattern controller record without changing its length.
    /// Controller channel and flags bytes remain untouched because their target semantics are
    /// still unknown.
    pub fn edit_pattern_controller(
        &mut self,
        pattern_id: u16,
        controller_index: usize,
        edit: PatternControllerEdit,
    ) -> Result<(), FlpError> {
        if edit.value.is_some_and(|value| !value.is_finite()) {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }
        if edit.position.is_none() && edit.value.is_none() {
            return Ok(());
        }

        let (event_index, record_offset, _) =
            self.pattern_controller_record_location(pattern_id, controller_index)?;

        let mut candidate = self.clone();
        if let Some(position) = edit.position {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                record_offset,
                &position.to_le_bytes(),
            )?;
        }
        if let Some(value) = edit.value {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                record_offset + 8,
                &value.to_bits().to_le_bytes(),
            )?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Adds a raw Pattern controller point by copying an existing point's uninterpreted fields.
    /// Reserved, channel, and flags bytes are copied exactly; only position and value change.
    pub fn duplicate_pattern_controller(
        &mut self,
        pattern_id: u16,
        template_controller_index: usize,
        position: u32,
        value: f32,
    ) -> Result<usize, FlpError> {
        if !value.is_finite() {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }
        let (event_index, _, mut controller) =
            self.pattern_controller_record_location(pattern_id, template_controller_index)?;
        let new_index = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .map(|pattern| pattern.controllers.len())
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        controller.position = position;
        controller.value_bits = value.to_bits();

        let mut candidate = self.clone();
        let mut payload = candidate.events[event_index].payload.clone();
        payload
            .len()
            .checked_add(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        payload.extend_from_slice(&controller.encode());
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_index)
    }

    /// Draws raw Pattern controller points into one existing channel/flags stream.
    /// Points at existing ticks update that point's value; new ticks copy the first point's
    /// opaque record fields. All edits are committed together so a drawn stroke is atomic.
    pub fn draw_pattern_controller_points(
        &mut self,
        pattern_id: u16,
        stream: (u8, u8),
        points: &[(u32, f32)],
    ) -> Result<usize, FlpError> {
        if points.iter().any(|(_, value)| !value.is_finite()) {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller values must be finite",
            ));
        }
        if points.is_empty() {
            return Ok(0);
        }

        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let template_index = pattern
            .controllers
            .iter()
            .position(|controller| (controller.channel, controller.flags) == stream)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested raw Pattern controller stream does not exist",
            ))?;
        let (template_event_index, _, template) =
            self.pattern_controller_record_location(pattern_id, template_index)?;

        let mut existing_by_position = BTreeMap::new();
        for (index, controller) in pattern.controllers.iter().enumerate() {
            if (controller.channel, controller.flags) == stream {
                existing_by_position
                    .entry(controller.position)
                    .or_insert(index);
            }
        }

        let mut value_updates = BTreeMap::new();
        let mut additions = Vec::<PatternController>::new();
        let mut addition_by_position = BTreeMap::<u32, usize>::new();
        for (position, value) in points {
            if let Some(index) = existing_by_position.get(position).copied() {
                let (event_index, record_offset, _) =
                    self.pattern_controller_record_location(pattern_id, index)?;
                value_updates.insert((event_index, record_offset + 8), value.to_bits());
            } else if let Some(index) = addition_by_position.get(position).copied() {
                additions[index].value_bits = value.to_bits();
            } else {
                let mut controller = template;
                controller.position = *position;
                controller.value_bits = value.to_bits();
                addition_by_position.insert(*position, additions.len());
                additions.push(controller);
            }
        }

        let added_count = additions.len();
        let mut candidate = self.clone();
        for ((event_index, value_offset), value_bits) in value_updates {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                value_offset,
                &value_bits.to_le_bytes(),
            )?;
        }
        if !additions.is_empty() {
            let mut payload = candidate.events[template_event_index].payload.clone();
            for controller in additions {
                payload
                    .len()
                    .checked_add(FLP_PATTERN_CONTROLLER_RECORD_SIZE)
                    .ok_or(FlpError::LengthOverflow)?;
                payload.extend_from_slice(&controller.encode());
            }
            candidate.events[template_event_index].replace_data_payload(payload)?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(added_count)
    }

    /// Makes existing points in a raw Pattern controller range follow a straight line between
    /// the first and last selected points. Positions and opaque record fields remain unchanged.
    pub fn linearize_pattern_controller_range(
        &mut self,
        pattern_id: u16,
        stream: (u8, u8),
        start_tick: u32,
        end_tick: u32,
    ) -> Result<usize, FlpError> {
        self.interpolate_pattern_controller_range(pattern_id, stream, start_tick, end_tick, false)
    }

    /// Applies a cubic smoothstep spline between the endpoint values of an existing raw Pattern
    /// controller range. Positions and opaque record fields remain unchanged.
    pub fn spline_pattern_controller_range(
        &mut self,
        pattern_id: u16,
        stream: (u8, u8),
        start_tick: u32,
        end_tick: u32,
    ) -> Result<usize, FlpError> {
        self.interpolate_pattern_controller_range(pattern_id, stream, start_tick, end_tick, true)
    }

    fn interpolate_pattern_controller_range(
        &mut self,
        pattern_id: u16,
        stream: (u8, u8),
        start_tick: u32,
        end_tick: u32,
        spline: bool,
    ) -> Result<usize, FlpError> {
        let (start_tick, end_tick) = if start_tick <= end_tick {
            (start_tick, end_tick)
        } else {
            (end_tick, start_tick)
        };
        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let mut selected = pattern
            .controllers
            .iter()
            .enumerate()
            .filter(|(_, controller)| {
                (controller.channel, controller.flags) == stream
                    && (start_tick..=end_tick).contains(&controller.position)
            })
            .map(|(index, controller)| (index, *controller))
            .collect::<Vec<_>>();
        selected.sort_by_key(|(_, controller)| controller.position);

        let Some((_, first)) = selected.first().copied() else {
            return Ok(0);
        };
        let Some((_, last)) = selected.last().copied() else {
            return Ok(0);
        };
        if selected.len() < 3 || first.position == last.position {
            return Ok(0);
        }
        if !first.value().is_finite() || !last.value().is_finite() {
            return Err(FlpError::UnsupportedEdit(
                "Pattern controller interpolation needs finite endpoint values",
            ));
        }

        let tick_span = f64::from(last.position - first.position);
        let first_value = f64::from(first.value());
        let last_value = f64::from(last.value());
        let mut value_updates = BTreeMap::new();
        for (index, controller) in selected.iter().skip(1).take(selected.len() - 2) {
            let fraction = f64::from(controller.position - first.position) / tick_span;
            let fraction = if spline {
                fraction * fraction * (3.0 - 2.0 * fraction)
            } else {
                fraction
            };
            let value = (first_value + (last_value - first_value) * fraction) as f32;
            if controller.value_bits != value.to_bits() {
                let (event_index, record_offset, _) =
                    self.pattern_controller_record_location(pattern_id, *index)?;
                value_updates.insert((event_index, record_offset + 8), value.to_bits());
            }
        }
        let changed_count = value_updates.len();
        if changed_count == 0 {
            return Ok(0);
        }

        let mut candidate = self.clone();
        for ((event_index, value_offset), value_bits) in value_updates {
            write_event_payload_bytes(
                &mut candidate.events[event_index],
                value_offset,
                &value_bits.to_le_bytes(),
            )?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(changed_count)
    }

    /// Removes one raw Pattern controller point by its zero-based Pattern index.
    pub fn delete_pattern_controller(
        &mut self,
        pattern_id: u16,
        controller_index: usize,
    ) -> Result<(), FlpError> {
        let (event_index, record_offset, _) =
            self.pattern_controller_record_location(pattern_id, controller_index)?;
        let mut candidate = self.clone();
        let mut payload = candidate.events[event_index].payload.clone();
        payload.drain(record_offset..record_offset + FLP_PATTERN_CONTROLLER_RECORD_SIZE);
        if payload.is_empty() {
            candidate.events.remove(event_index);
        } else {
            candidate.events[event_index].replace_data_payload(payload)?;
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Removes points in one raw Pattern controller stream and inclusive tick range.
    /// Records from other streams and unrelated event bytes remain unchanged.
    pub fn delete_pattern_controller_range(
        &mut self,
        pattern_id: u16,
        stream: (u8, u8),
        start_tick: u32,
        end_tick: u32,
    ) -> Result<usize, FlpError> {
        let (start_tick, end_tick) = if start_tick <= end_tick {
            (start_tick, end_tick)
        } else {
            (end_tick, start_tick)
        };
        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let mut offsets_by_event = BTreeMap::<usize, Vec<usize>>::new();
        for (controller_index, controller) in pattern.controllers.iter().enumerate() {
            if (controller.channel, controller.flags) == stream
                && (start_tick..=end_tick).contains(&controller.position)
            {
                let (event_index, record_offset, _) =
                    self.pattern_controller_record_location(pattern_id, controller_index)?;
                offsets_by_event
                    .entry(event_index)
                    .or_default()
                    .push(record_offset);
            }
        }
        let removed_count = offsets_by_event.values().map(Vec::len).sum::<usize>();
        if removed_count == 0 {
            return Ok(0);
        }

        let mut candidate = self.clone();
        for (event_index, mut record_offsets) in offsets_by_event.into_iter().rev() {
            record_offsets.sort_unstable();
            let mut payload = candidate.events[event_index].payload.clone();
            for record_offset in record_offsets.into_iter().rev() {
                payload.drain(record_offset..record_offset + FLP_PATTERN_CONTROLLER_RECORD_SIZE);
            }
            if payload.is_empty() {
                candidate.events.remove(event_index);
            } else {
                candidate.events[event_index].replace_data_payload(payload)?;
            }
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(removed_count)
    }

    fn pattern_controller_record_location(
        &self,
        pattern_id: u16,
        controller_index: usize,
    ) -> Result<(usize, usize, PatternController), FlpError> {
        let mut current_pattern = None;
        let mut matching_pattern_markers = 0usize;
        let mut next_controller_index = 0usize;
        let mut target = None;
        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    current_pattern =
                        Some(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                    if current_pattern == Some(pattern_id) {
                        matching_pattern_markers += 1;
                    }
                }
                0x40 | 0x62 | 0x63 => current_pattern = None,
                0xDF if current_pattern == Some(pattern_id) => {
                    let (records, remainder) = event
                        .payload
                        .as_chunks::<FLP_PATTERN_CONTROLLER_RECORD_SIZE>();
                    if !remainder.is_empty() {
                        return Err(FlpError::InvalidEvent {
                            offset: event.file_offset,
                            detail: "pattern controller payload is not a whole number of 12-byte records",
                        });
                    }
                    let next_controller_end = next_controller_index.saturating_add(records.len());
                    if (next_controller_index..next_controller_end).contains(&controller_index) {
                        let record_index = controller_index - next_controller_index;
                        target = Some((
                            event_index,
                            record_index * FLP_PATTERN_CONTROLLER_RECORD_SIZE,
                            PatternController::decode(&records[record_index]),
                        ));
                    }
                    next_controller_index = next_controller_index.saturating_add(records.len());
                }
                _ => {}
            }
        }
        if matching_pattern_markers == 0 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ));
        }
        if matching_pattern_markers > 1 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern ID is ambiguous",
            ));
        }
        target.ok_or(FlpError::UnsupportedEdit(
            "the requested Pattern or controller index does not exist",
        ))
    }

    /// Sets or creates a time-signature marker within one Pattern.
    /// Pattern markers are scoped by the `0x41` Pattern ID events and remain separate from
    /// project defaults and Playlist arrangement markers.
    pub fn set_pattern_time_signature(
        &mut self,
        pattern_id: u16,
        position_ticks: u32,
        numerator: u8,
        denominator: u8,
    ) -> Result<usize, FlpError> {
        if numerator == 0 || denominator == 0 {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }
        if position_ticks > TIME_MARKER_TICK_MASK {
            return Err(FlpError::UnsupportedEdit(
                "pattern time-signature positions must fit in the low 27 position bits",
            ));
        }

        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let matching_markers = pattern
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature() && marker.position_ticks() == position_ticks)
            .collect::<Vec<_>>();
        if matching_markers.len() > 1 {
            return Err(FlpError::UnsupportedEdit(
                "the Pattern has ambiguous signatures at the requested position",
            ));
        }

        let mut candidate = self.clone();
        if let Some(marker) = matching_markers.first() {
            let position_event_index =
                marker
                    .source_events
                    .position
                    .ok_or(FlpError::UnsupportedEdit(
                        "the selected Pattern time signature has no source position event",
                    ))?;
            let mut insertion_index = position_event_index + 1;
            for event_index in [
                marker.source_events.numerator,
                marker.source_events.denominator,
            ]
            .into_iter()
            .flatten()
            {
                insertion_index = insertion_index.max(event_index + 1);
            }
            if let Some(name_event_index) = marker.source_events.name {
                insertion_index = insertion_index.min(name_event_index);
            }

            let mut insertions = Vec::<(usize, u8, FlpEvent)>::new();
            for (value, event_index, opcode, rank) in [
                (numerator, marker.source_events.numerator, 0x21, 0),
                (denominator, marker.source_events.denominator, 0x22, 1),
            ] {
                if let Some(event_index) = event_index {
                    candidate.events[event_index].replace_byte_payload(value)?;
                } else {
                    insertions.push((insertion_index, rank, FlpEvent::new_byte(opcode, value)));
                }
            }
            insertions
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
            for (event_index, _, event) in insertions {
                candidate.events.insert(event_index, event);
            }
        } else {
            let marker_index = candidate
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.opcode == 0x41
                        && event.payload.len() == 2
                        && event.payload.as_slice() == pattern_id.to_le_bytes()
                })
                .map(|(index, _)| index)
                .next_back()
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested Pattern has no source marker",
                ))?;
            let region_start = marker_index + 1;
            let region_end = candidate.events[region_start..]
                .iter()
                .position(|event| {
                    event.opcode == 0x41 || matches!(event.opcode, 0x40 | 0x62 | 0x63)
                })
                .map_or(candidate.events.len(), |offset| region_start + offset);
            let later_signature = pattern
                .time_markers
                .iter()
                .filter(|marker| marker.is_signature() && marker.position_ticks() > position_ticks)
                .filter_map(|marker| marker.source_events.position)
                .find(|event_index| *event_index >= region_start && *event_index < region_end);
            let last_marker_end = pattern
                .time_markers
                .iter()
                .flat_map(|marker| {
                    [
                        marker.source_events.position,
                        marker.source_events.numerator,
                        marker.source_events.denominator,
                        marker.source_events.name,
                    ]
                })
                .flatten()
                .filter(|event_index| *event_index >= region_start && *event_index < region_end)
                .max()
                .map(|event_index| event_index + 1);
            let insertion_index = later_signature.or(last_marker_end).unwrap_or(region_end);
            let raw_position = position_ticks | TIME_MARKER_SIGNATURE_BIT;
            candidate.events.splice(
                insertion_index..insertion_index,
                [
                    FlpEvent::new_dword(0x94, raw_position),
                    FlpEvent::new_byte(0x21, numerator),
                    FlpEvent::new_byte(0x22, denominator),
                ],
            );
        }
        candidate.refresh_event_offsets()?;
        let marker_index = candidate
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .and_then(|pattern| {
                pattern.time_markers.iter().position(|marker| {
                    marker.is_signature() && marker.position_ticks() == position_ticks
                })
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the updated Pattern time signature could not be decoded",
            ))?;
        *self = candidate;
        Ok(marker_index)
    }

    /// Deletes one Pattern time-signature marker at the requested tick.
    pub fn delete_pattern_time_signature(
        &mut self,
        pattern_id: u16,
        position_ticks: u32,
    ) -> Result<(), FlpError> {
        let patterns = self.patterns()?;
        let pattern = patterns
            .iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Pattern does not exist",
            ))?;
        let matching_markers = pattern
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature() && marker.position_ticks() == position_ticks)
            .collect::<Vec<_>>();
        if matching_markers.len() != 1 {
            return Err(FlpError::UnsupportedEdit(if matching_markers.is_empty() {
                "the requested Pattern time signature does not exist"
            } else {
                "the Pattern has ambiguous signatures at the requested position"
            }));
        }
        let marker = matching_markers[0];
        let source_event_indices = [
            marker.source_events.position,
            marker.source_events.numerator,
            marker.source_events.denominator,
            marker.source_events.name,
        ]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
        if source_event_indices.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Pattern time signature has no source events",
            ));
        }
        let mut candidate = self.clone();
        for event_index in source_event_indices.into_iter().rev() {
            candidate.events.remove(event_index);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Renames a recognized Pattern while preserving its other events and string suffix bytes.
    pub fn set_pattern_name(&mut self, pattern_id: u16, name: &str) -> Result<(), FlpError> {
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "pattern names cannot contain an embedded NUL character",
            ));
        }
        if !self
            .patterns()?
            .iter()
            .any(|pattern| pattern.id == pattern_id)
        {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ));
        }

        let mut markers = self.events.iter().enumerate().filter(|(_, event)| {
            event.opcode == 0x41
                && event.encoding == PayloadEncoding::Word
                && event.payload.as_slice() == pattern_id.to_le_bytes()
        });
        let Some((marker_index, _)) = markers.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern marker does not exist",
            ));
        };
        if markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }
        let region_end = self.events[marker_index + 1..]
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.encoding == PayloadEncoding::Word)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .map_or(self.events.len(), |offset| marker_index + 1 + offset);
        let name_events = (marker_index + 1..region_end)
            .filter(|index| self.events[*index].opcode == 0xC1)
            .collect::<Vec<_>>();
        if name_events.len() > 1 {
            return Err(FlpError::UnsupportedEdit(
                "the pattern has multiple name events",
            ));
        }
        let uses_utf16 = self
            .project_string_encoding()
            .ok_or(FlpError::UnsupportedEdit(
                "the project's string encoding cannot be inferred for a pattern name",
            ))?;

        let mut candidate = self.clone();
        if let Some(name_event_index) = name_events.first().copied() {
            if !matches!(
                candidate.events[name_event_index].encoding,
                PayloadEncoding::Data { .. }
            ) {
                return Err(FlpError::UnsupportedEdit(
                    "the pattern name is not a length-prefixed string",
                ));
            }
            let replacement = replace_project_string_payload(
                &candidate.events[name_event_index].payload,
                name,
                uses_utf16,
            )?;
            candidate.events[name_event_index].replace_data_payload(replacement)?;
        } else {
            let payload = encode_project_string(name, uses_utf16)?;
            let insert_index = marker_index
                + 1
                + usize::from(
                    self.events
                        .get(marker_index + 1)
                        .is_some_and(Self::is_pattern_note_event),
                );
            candidate
                .events
                .insert(insert_index, FlpEvent::new_data(0xC1, payload)?);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Deletes an unreferenced Pattern and its complete event region.
    ///
    /// Pattern Clips that still target this Pattern prevent deletion. Projects whose
    /// arrangement clip layout cannot be decoded are also rejected so unknown references
    /// cannot be orphaned.
    pub fn delete_pattern(&mut self, pattern_id: u16) -> Result<(), FlpError> {
        let patterns = self.patterns()?;
        if !patterns.iter().any(|pattern| pattern.id == pattern_id) {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ));
        }

        let mut markers = self.events.iter().enumerate().filter(|(_, event)| {
            event.opcode == 0x41
                && event.encoding == PayloadEncoding::Word
                && event.payload.as_slice() == pattern_id.to_le_bytes()
        });
        let Some((marker_index, _)) = markers.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern marker does not exist",
            ));
        };
        if markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }

        let arrangements = self.arrangements()?;
        if arrangements.iter().any(|arrangement| {
            arrangement.clips.iter().any(|clip| {
                matches!(clip.target(), PlaylistClipTarget::Pattern { id } if id == pattern_id)
            })
        }) {
            return Err(FlpError::UnsupportedEdit(
                "the pattern is still referenced by a Playlist Clip",
            ));
        }

        let region_end = self.events[marker_index + 1..]
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.encoding == PayloadEncoding::Word)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .map_or(self.events.len(), |offset| marker_index + 1 + offset);

        let mut candidate = self.clone();
        candidate.events.drain(marker_index..region_end);
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Creates an empty pattern using the note-event encoding already present in the project.
    ///
    /// Projects without a uniquely recognized pattern-note encoding are rejected rather than
    /// guessing between version-specific score event layouts.
    pub fn create_pattern(&mut self) -> Result<u16, FlpError> {
        let mut score_regions = Vec::new();
        let mut note_opcode = None;
        let mut conflicting_opcodes = false;

        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0x41 || event.payload.len() != 2 {
                continue;
            }
            if let Some(notes_event) = self.events.get(event_index + 1)
                && Self::is_pattern_note_event(notes_event)
            {
                if event.encoding != PayloadEncoding::Word
                    || !matches!(notes_event.encoding, PayloadEncoding::Data { .. })
                    || !notes_event
                        .payload
                        .len()
                        .is_multiple_of(FLP_NOTE_RECORD_SIZE)
                {
                    return Err(FlpError::UnsupportedEdit(
                        "the project's pattern note event layout is not recognized",
                    ));
                }
                let section_start = self.events[..event_index]
                    .iter()
                    .rposition(|candidate| matches!(candidate.opcode, 0x40 | 0x62 | 0x63))
                    .map_or(0, |boundary| boundary + 1);
                let section_end = self.events[event_index + 1..]
                    .iter()
                    .position(|candidate| matches!(candidate.opcode, 0x40 | 0x62 | 0x63))
                    .map_or(self.events.len(), |offset| event_index + 1 + offset);
                score_regions.push((section_start, section_end));

                match note_opcode {
                    None => note_opcode = Some(notes_event.opcode),
                    Some(opcode) if opcode != notes_event.opcode => {
                        conflicting_opcodes = true;
                    }
                    _ => {}
                }
            }
        }

        if conflicting_opcodes || note_opcode.is_none() {
            return Err(FlpError::UnsupportedEdit(
                "cannot infer a unique pattern note-event encoding for this project",
            ));
        }
        let Some((_, insert_index)) = score_regions.iter().copied().max_by_key(|range| range.0)
        else {
            return Err(FlpError::UnsupportedEdit(
                "the project has no recognized pattern marker to extend",
            ));
        };
        let mut pattern_ids = HashSet::new();
        for (region_start, region_end) in score_regions {
            for event in &self.events[region_start..region_end] {
                if event.opcode == 0x41
                    && event.encoding == PayloadEncoding::Word
                    && event.payload.len() == 2
                {
                    pattern_ids.insert(u16::from_le_bytes([event.payload[0], event.payload[1]]));
                }
            }
        }
        let Some(highest_pattern_id) = pattern_ids.iter().max().copied() else {
            return Err(FlpError::UnsupportedEdit(
                "the recognized score section contains no pattern IDs",
            ));
        };

        let new_id = highest_pattern_id
            .checked_add(1)
            .ok_or(FlpError::LengthOverflow)?;

        let mut candidate = self.clone();
        candidate
            .events
            .insert(insert_index, FlpEvent::new_word(0x41, new_id));
        candidate.events.insert(
            insert_index + 1,
            FlpEvent::new_data(
                note_opcode.expect("unique opcode checked above"),
                Vec::new(),
            )?,
        );
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_id)
    }

    /// Duplicates a pattern's note records, raw `0xDF` controller events, recognized time-marker
    /// events, name, and explicit length into a new pattern. Unknown events and all existing
    /// pattern bytes remain untouched.
    pub fn duplicate_pattern(&mut self, pattern_id: u16) -> Result<u16, FlpError> {
        let source_pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ))?;
        let mut matching_markers = self.events.iter().enumerate().filter(|(_, event)| {
            event.opcode == 0x41
                && event.payload.len() == 2
                && event.payload.as_slice() == pattern_id.to_le_bytes()
        });
        let Some((source_marker_index, _)) = matching_markers.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern marker does not exist",
            ));
        };
        if matching_markers.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }
        let source_note_payload = self
            .events
            .get(source_marker_index + 1)
            .filter(|event| Self::is_pattern_note_event(event))
            .map(|event| event.payload.clone());

        let source_region = &self.events[source_marker_index + 1..];
        let source_region_length = source_region
            .iter()
            .position(|event| {
                (event.opcode == 0x41 && event.payload.len() == 2)
                    || matches!(event.opcode, 0x40 | 0x62 | 0x63)
            })
            .unwrap_or(source_region.len());
        let source_region = &source_region[..source_region_length];
        let time_marker_event_indices = source_pattern
            .time_markers
            .iter()
            .flat_map(|marker| {
                [
                    marker.source_events.position,
                    marker.source_events.numerator,
                    marker.source_events.denominator,
                    marker.source_events.name,
                ]
                .into_iter()
                .flatten()
            })
            .collect::<HashSet<_>>();
        let name_event_index = source_pattern.name.as_ref().and_then(|_| {
            source_region.iter().position(|event| {
                event.opcode == 0xC1
                    && decode_project_string(&event.payload, self.project_version.as_deref())
                        .is_some_and(|name| !name.is_empty())
            })
        });
        let length_event_index = source_pattern.length_ticks.and_then(|_| {
            source_region
                .iter()
                .rposition(|event| event.opcode == 0xA4 && event.payload.len() == 4)
        });
        let duplicate_auxiliary_events = source_region
            .iter()
            .enumerate()
            .filter(|(index, event)| {
                let source_event_index = source_marker_index + 1 + *index;
                event.opcode == 0xDF
                    || time_marker_event_indices.contains(&source_event_index)
                    || Some(*index) == name_event_index
                    || Some(*index) == length_event_index
            })
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();

        let mut candidate = self.clone();
        let new_pattern_id = candidate.create_pattern()?;
        let new_marker_index = candidate
            .events
            .iter()
            .position(|event| {
                event.opcode == 0x41
                    && event.payload.len() == 2
                    && event.payload.as_slice() == new_pattern_id.to_le_bytes()
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the duplicate pattern marker could not be found",
            ))?;
        let new_note_event_index = new_marker_index
            .checked_add(1)
            .filter(|index| *index < candidate.events.len())
            .ok_or(FlpError::UnsupportedEdit(
                "the duplicate pattern note event could not be found",
            ))?;
        if !Self::is_pattern_note_event(&candidate.events[new_note_event_index]) {
            return Err(FlpError::UnsupportedEdit(
                "the duplicate pattern note event could not be found",
            ));
        }
        if let Some(payload) = source_note_payload {
            candidate.events[new_note_event_index].replace_data_payload(payload)?;
        }
        if !duplicate_auxiliary_events.is_empty() {
            let insert_index = new_marker_index
                .checked_add(2)
                .filter(|index| *index <= candidate.events.len())
                .ok_or(FlpError::LengthOverflow)?;
            candidate
                .events
                .splice(insert_index..insert_index, duplicate_auxiliary_events);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_pattern_id)
    }

    /// Returns playlist arrangements and their stored clip records.
    /// The source events remain available unchanged through `events()`.
    pub fn arrangements(&self) -> Result<Vec<Arrangement>, FlpError> {
        self.arrangements_impl(true)
    }

    /// Returns the exact stored bytes for one Playlist clip record.
    ///
    /// This is useful when comparing native project saves to identify fields
    /// that are not decoded yet. The bytes are borrowed from the original
    /// `0xE9` event payload and are never interpreted or rewritten here.
    pub fn playlist_clip_record_bytes(
        &self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<&[u8], FlpError> {
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
        let event_index = clip.source_event_index;
        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event = self.events.get(event_index).ok_or(FlpError::InvalidEvent {
            offset: 0,
            detail: "the Playlist clip source event is unavailable",
        })?;
        event
            .payload
            .get(record_start..record_end)
            .ok_or(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "the Playlist clip record is outside its source event",
            })
    }

    /// Returns flattened time markers without decoding Playlist clip records.
    /// This lets callers inspect marker data even when a project's clip layout
    /// is not yet supported by `arrangements()`.
    pub fn time_markers(&self) -> Result<Vec<(u16, TimeMarker)>, FlpError> {
        let arrangements = self.arrangements_impl(false)?;
        let mut markers = Vec::new();
        for arrangement in arrangements {
            markers.extend(
                arrangement
                    .time_markers
                    .into_iter()
                    .map(|marker| (arrangement.id, marker)),
            );
        }
        Ok(markers)
    }

    /// Edits the stored fields of one Playlist time marker while retaining its raw flags
    /// and all unrelated events. Marker indexes are zero-based within the arrangement.
    pub fn edit_time_marker(
        &mut self,
        arrangement_id: u16,
        marker_index: usize,
        edit: TimeMarkerEdit,
    ) -> Result<(), FlpError> {
        if edit.name.as_deref().is_some_and(|name| name.contains('\0')) {
            return Err(FlpError::UnsupportedEdit(
                "time marker names cannot contain an embedded NUL character",
            ));
        }
        if edit.numerator == Some(0) || edit.denominator == Some(0) {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }
        if edit
            .position_ticks
            .is_some_and(|position| position > TIME_MARKER_TICK_MASK)
        {
            return Err(FlpError::UnsupportedEdit(
                "time marker positions must fit in the low 27 position bits",
            ));
        }

        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let marker =
            arrangement
                .time_markers
                .get(marker_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested time marker does not exist",
                ))?;
        let source_events = marker.source_events.clone();
        let position_event_index = source_events.position.ok_or(FlpError::UnsupportedEdit(
            "the selected time marker has no source position event",
        ))?;

        let mut raw_position = marker.raw_position;
        if let Some(position_ticks) = edit.position_ticks {
            raw_position = (raw_position & !TIME_MARKER_TICK_MASK) | position_ticks;
        }
        if let Some(is_signature) = edit.is_signature {
            if is_signature {
                raw_position |= TIME_MARKER_SIGNATURE_BIT;
            } else {
                raw_position &= !TIME_MARKER_SIGNATURE_BIT;
            }
        }

        let is_signature = raw_position & TIME_MARKER_SIGNATURE_BIT != 0;
        let numerator = edit.numerator.or(marker.numerator);
        let denominator = edit.denominator.or(marker.denominator);
        if (edit.numerator.is_some() || edit.denominator.is_some()) && !is_signature {
            return Err(FlpError::UnsupportedEdit(
                "numerator and denominator can only be edited on a time-signature marker",
            ));
        }
        if (edit.numerator.is_some() || edit.denominator.is_some())
            && (numerator.is_none() || denominator.is_none())
        {
            return Err(FlpError::UnsupportedEdit(
                "editing a time signature requires both numerator and denominator values",
            ));
        }
        if edit.is_signature == Some(true)
            && !marker.is_signature()
            && (numerator.is_none() || denominator.is_none())
        {
            return Err(FlpError::UnsupportedEdit(
                "converting a marker to a time signature requires numerator and denominator values",
            ));
        }

        let mut candidate = self.clone();
        candidate.events[position_event_index].replace_dword_payload(raw_position)?;

        let mut insertion_index = position_event_index + 1;
        for event_index in [source_events.numerator, source_events.denominator]
            .into_iter()
            .flatten()
        {
            insertion_index = insertion_index.max(event_index + 1);
        }
        if let Some(name_event_index) = source_events.name {
            insertion_index = insertion_index.min(name_event_index);
        }

        let mut insertions = Vec::<(usize, u8, FlpEvent)>::new();
        for (value, event_index, opcode, rank) in [
            (edit.numerator, source_events.numerator, 0x21, 0),
            (edit.denominator, source_events.denominator, 0x22, 1),
        ] {
            let Some(value) = value else {
                continue;
            };
            if let Some(event_index) = event_index {
                candidate.events[event_index].replace_byte_payload(value)?;
            } else {
                insertions.push((insertion_index, rank, FlpEvent::new_byte(opcode, value)));
            }
        }

        if let Some(name) = edit.name {
            if let Some(name_event_index) = source_events.name {
                let event = &candidate.events[name_event_index];
                if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                    return Err(FlpError::UnsupportedEdit(
                        "the selected time marker name is not a length-prefixed data event",
                    ));
                }
                let utf16 =
                    project_string_is_utf16(&event.payload, candidate.project_version.as_deref());
                let payload = replace_project_string_payload(&event.payload, &name, utf16)?;
                candidate.events[name_event_index].replace_data_payload(payload)?;
            } else {
                let payload = encode_project_string(&name, candidate.project_strings_use_utf16())?;
                insertions.push((insertion_index, 2, FlpEvent::new_data(0xCD, payload)?));
            }
        }

        insertions.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        for (event_index, _, event) in insertions {
            candidate.events.insert(event_index, event);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Creates a Playlist time or signature marker in an arrangement.
    /// Marker fields use the same validation and wire representation as marker edits.
    pub fn create_time_marker(
        &mut self,
        arrangement_id: u16,
        edit: TimeMarkerEdit,
    ) -> Result<usize, FlpError> {
        if edit.name.as_deref().is_some_and(|name| name.contains('\0')) {
            return Err(FlpError::UnsupportedEdit(
                "time marker names cannot contain an embedded NUL character",
            ));
        }
        if edit
            .position_ticks
            .is_some_and(|position| position > TIME_MARKER_TICK_MASK)
        {
            return Err(FlpError::UnsupportedEdit(
                "time marker positions must fit in the low 27 position bits",
            ));
        }
        if edit.numerator == Some(0) || edit.denominator == Some(0) {
            return Err(FlpError::UnsupportedEdit(
                "time signature numerator and denominator must be positive",
            ));
        }

        let is_signature = edit.is_signature.unwrap_or(false);
        if is_signature {
            if edit.numerator.is_none() || edit.denominator.is_none() {
                return Err(FlpError::UnsupportedEdit(
                    "creating a time-signature marker requires numerator and denominator values",
                ));
            }
        } else if edit.numerator.is_some() || edit.denominator.is_some() {
            return Err(FlpError::UnsupportedEdit(
                "numerator and denominator can only be set on a time-signature marker",
            ));
        }

        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let arrangement = matching_arrangements.next();
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
        if arrangement.is_none() && (has_arrangement_markers || arrangement_id != 0) {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        }

        let (region_start, region_end) = if has_arrangement_markers {
            let start = self
                .events
                .iter()
                .position(|event| {
                    event.opcode == 0x63
                        && event.payload.len() == 2
                        && u16::from_le_bytes([event.payload[0], event.payload[1]])
                            == arrangement_id
                })
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested Playlist arrangement does not exist",
                ))?;
            let region_start = start + 1;
            let region_end = self.events[region_start..]
                .iter()
                .position(|event| {
                    event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
                })
                .map_or(self.events.len(), |offset| region_start + offset);
            (region_start, region_end)
        } else {
            let region_end = self
                .events
                .iter()
                .rposition(|event| event.opcode == 0x62)
                .unwrap_or(self.events.len());
            (0, region_end)
        };

        let position_ticks = edit.position_ticks.unwrap_or(0);
        let markers =
            arrangement.map_or(&[][..], |arrangement| arrangement.time_markers.as_slice());
        let later_marker = markers
            .iter()
            .filter(|marker| marker.position_ticks() > position_ticks)
            .filter_map(|marker| marker.source_events.position)
            .find(|event_index| *event_index >= region_start && *event_index < region_end);
        let last_marker_end = markers
            .iter()
            .filter_map(|marker| {
                [
                    marker.source_events.position,
                    marker.source_events.numerator,
                    marker.source_events.denominator,
                    marker.source_events.name,
                ]
                .into_iter()
                .flatten()
                .max()
            })
            .map(|event_index| event_index + 1)
            .filter(|event_index| *event_index >= region_start && *event_index <= region_end);
        let first_clip_event = self.events[region_start..region_end]
            .iter()
            .position(|event| event.opcode == 0xE9)
            .map(|offset| region_start + offset);
        let insertion_index = later_marker
            .or_else(|| last_marker_end.max())
            .or(first_clip_event)
            .unwrap_or(region_end);

        let mut raw_position = position_ticks;
        if is_signature {
            raw_position |= TIME_MARKER_SIGNATURE_BIT;
        }
        let mut inserted = vec![FlpEvent::new_dword(0x94, raw_position)];
        if is_signature {
            inserted.push(FlpEvent::new_byte(
                0x21,
                edit.numerator.expect("signature numerator validated"),
            ));
            inserted.push(FlpEvent::new_byte(
                0x22,
                edit.denominator.expect("signature denominator validated"),
            ));
        }
        if let Some(name) = edit.name {
            let payload = encode_project_string(&name, self.project_strings_use_utf16())?;
            inserted.push(FlpEvent::new_data(0xCD, payload)?);
        }

        let mut candidate = self.clone();
        for (offset, event) in inserted.into_iter().enumerate() {
            candidate.events.insert(insertion_index + offset, event);
        }
        candidate.refresh_event_offsets()?;
        let marker_index = candidate
            .arrangements_impl(false)?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .and_then(|arrangement| {
                arrangement
                    .time_markers
                    .iter()
                    .position(|marker| marker.source_events.position == Some(insertion_index))
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the new Playlist marker could not be resolved after insertion",
            ))?;
        *self = candidate;
        Ok(marker_index)
    }

    /// Removes one marker's recognized source events while preserving unrelated events.
    pub fn delete_time_marker(
        &mut self,
        arrangement_id: u16,
        marker_index: usize,
    ) -> Result<(), FlpError> {
        let arrangements = self.arrangements_impl(false)?;
        let mut matching_arrangements = arrangements
            .iter()
            .filter(|arrangement| arrangement.id == arrangement_id);
        let Some(arrangement) = matching_arrangements.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement does not exist",
            ));
        };
        if matching_arrangements.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist arrangement id is ambiguous",
            ));
        }
        let marker =
            arrangement
                .time_markers
                .get(marker_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the requested time marker does not exist",
                ))?;
        let mut source_event_indices = [
            marker.source_events.position,
            marker.source_events.numerator,
            marker.source_events.denominator,
            marker.source_events.name,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !source_event_indices.iter().any(|index| {
            self.events
                .get(*index)
                .is_some_and(|event| event.opcode == 0x94)
        }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected time marker has no source position event",
            ));
        }
        source_event_indices.sort_unstable();
        source_event_indices.dedup();

        let mut candidate = self.clone();
        for event_index in source_event_indices.into_iter().rev() {
            candidate.events.remove(event_index);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    fn arrangements_impl(&self, include_clips: bool) -> Result<Vec<Arrangement>, FlpError> {
        let mut arrangements = Vec::<Arrangement>::new();
        let mut current_arrangement = None;
        let mut current_time_marker = None;
        let mut current_pattern = false;
        let mut pending_time_markers = Vec::new();
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);

        for (event_index, event) in self.events.iter().enumerate() {
            match event.opcode {
                0x41 if event.payload.len() == 2 => {
                    current_pattern = true;
                    current_time_marker = None;
                }
                0x40 => {
                    current_pattern = false;
                    current_time_marker = None;
                }
                0x63 if event.payload.len() == 2 => {
                    current_pattern = false;
                    let id = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    arrangements.push(Arrangement {
                        id,
                        ..Arrangement::default()
                    });
                    current_arrangement = Some(arrangements.len() - 1);
                    if !pending_time_markers.is_empty() {
                        let arrangement_index = arrangements.len() - 1;
                        let marker_start = arrangements[arrangement_index].time_markers.len();
                        let pending_count = pending_time_markers.len();
                        arrangements[arrangement_index]
                            .time_markers
                            .append(&mut pending_time_markers);
                        current_time_marker = match current_time_marker {
                            Some(TimeMarkerTarget::Pending { marker_index })
                                if marker_index < pending_count =>
                            {
                                Some(TimeMarkerTarget::Arrangement {
                                    arrangement_index,
                                    marker_index: marker_start + marker_index,
                                })
                            }
                            _ => None,
                        };
                    } else {
                        current_time_marker = None;
                    }
                }
                0x62 => {
                    current_pattern = false;
                    if has_arrangement_markers {
                        current_arrangement = None;
                    }
                    current_time_marker = None;
                }
                0xF1 => {
                    if let Some(arrangement_index) = current_arrangement
                        && arrangements[arrangement_index].name.is_none()
                    {
                        arrangements[arrangement_index].name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                    }
                }
                0x94 if event.payload.len() == 4 && !current_pattern => {
                    let raw_position = u32::from_le_bytes(
                        event.payload[..4]
                            .try_into()
                            .expect("a dword time-marker event has four bytes"),
                    );
                    if let Some(arrangement_index) = current_arrangement {
                        let marker_index = arrangements[arrangement_index].time_markers.len();
                        arrangements[arrangement_index]
                            .time_markers
                            .push(TimeMarker {
                                raw_position,
                                source_events: TimeMarkerSourceEvents {
                                    position: Some(event_index),
                                    ..TimeMarkerSourceEvents::default()
                                },
                                ..TimeMarker::default()
                            });
                        current_time_marker = Some(TimeMarkerTarget::Arrangement {
                            arrangement_index,
                            marker_index,
                        });
                    } else if has_arrangement_markers {
                        let marker_index = pending_time_markers.len();
                        pending_time_markers.push(TimeMarker {
                            raw_position,
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        current_time_marker = Some(TimeMarkerTarget::Pending { marker_index });
                    } else {
                        if arrangements.is_empty() {
                            arrangements.push(Arrangement::default());
                        }
                        current_arrangement = Some(0);
                        let marker_index = arrangements[0].time_markers.len();
                        arrangements[0].time_markers.push(TimeMarker {
                            raw_position,
                            source_events: TimeMarkerSourceEvents {
                                position: Some(event_index),
                                ..TimeMarkerSourceEvents::default()
                            },
                            ..TimeMarker::default()
                        });
                        current_time_marker = Some(TimeMarkerTarget::Arrangement {
                            arrangement_index: 0,
                            marker_index,
                        });
                    }
                }
                0x21 if event.payload.len() == 1 => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.numerator = Some(event.payload[0]);
                        marker.source_events.numerator = Some(event_index);
                    }
                }
                0x22 if event.payload.len() == 1 => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.denominator = Some(event.payload[0]);
                        marker.source_events.denominator = Some(event_index);
                    }
                }
                0xCD => {
                    if let Some(marker) = current_time_marker.and_then(|target| {
                        time_marker_at_mut(target, &mut arrangements, &mut pending_time_markers)
                    }) {
                        marker.name =
                            decode_project_string(&event.payload, self.project_version.as_deref())
                                .filter(|name| !name.is_empty());
                        marker.source_events.name = Some(event_index);
                    }
                    current_time_marker = None;
                }
                0xE9 if !include_clips => {}
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
                        &event.payload,
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
        if !pending_time_markers.is_empty() && !has_arrangement_markers {
            if arrangements.is_empty() {
                arrangements.push(Arrangement::default());
            }
            arrangements[0]
                .time_markers
                .append(&mut pending_time_markers);
        }
        Ok(arrangements)
    }

    /// Returns Playlist tracks from the observed `0xEE`, `0x2B`, `0xEF` event sequence.
    pub fn playlist_tracks(&self) -> Vec<PlaylistTrack> {
        self.events
            .iter()
            .enumerate()
            .filter_map(|(event_index, _)| self.playlist_track_at(event_index, self.events.len()))
            .collect()
    }

    /// Returns Playlist tracks from one arrangement's track-state block.
    /// Projects without arrangement markers have one implicit arrangement (ID 0).
    pub fn playlist_tracks_in_arrangement(&self, arrangement_id: u16) -> Vec<PlaylistTrack> {
        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
        if !has_arrangement_markers {
            return if arrangement_id == 0 {
                self.playlist_tracks()
            } else {
                Vec::new()
            };
        }

        let Some(marker_index) = self.events.iter().position(|event| {
            event.opcode == 0x63
                && event.payload.len() == 2
                && u16::from_le_bytes([event.payload[0], event.payload[1]]) == arrangement_id
        }) else {
            return Vec::new();
        };
        let start = marker_index + 1;
        let end = self.events[start..]
            .iter()
            .position(|event| {
                event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
            })
            .map_or(self.events.len(), |offset| start + offset);

        (start..end)
            .filter_map(|event_index| self.playlist_track_at(event_index, end))
            .collect()
    }

    fn playlist_track_at(&self, event_index: usize, region_end: usize) -> Option<PlaylistTrack> {
        let event = self.events.get(event_index)?;
        if event.opcode != 0xEE || event.payload.len() != 70 {
            return None;
        }
        let id = u32::from_le_bytes(event.payload[..4].try_into().ok()?);
        let next_event = self.events.get(event_index + 1);
        let state_byte = next_event
            .filter(|next| next.opcode == 0x2B && next.payload.len() == 1)
            .map(|next| next.payload[0]);
        let name_event_index = event_index + if state_byte.is_some() { 2 } else { 1 };
        let name = (name_event_index < region_end)
            .then(|| self.events.get(name_event_index))
            .flatten()
            .filter(|name_event| name_event.opcode == 0xEF)
            .and_then(|name_event| {
                decode_project_string(&name_event.payload, self.project_version.as_deref())
            })
            .filter(|value| !value.is_empty());
        let motion = i32::from_le_bytes(event.payload[22..26].try_into().ok()?);
        let press = i32::from_le_bytes(event.payload[26..30].try_into().ok()?);
        let trigger_sync = i32::from_le_bytes(event.payload[30..34].try_into().ok()?);
        let queued = match i32::from_le_bytes(event.payload[34..38].try_into().ok()?) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        let position_sync = i32::from_le_bytes(event.payload[42..46].try_into().ok()?);
        let tolerant = match i32::from_le_bytes(event.payload[38..42].try_into().ok()?) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        Some(PlaylistTrack {
            id,
            state_byte,
            name,
            enabled: match event.payload[12] {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            grouped: match event.payload[46] {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            press: PlaylistTrackPress::from_raw(press),
            motion: PlaylistTrackMotion::from_raw(motion),
            trigger_sync: PlaylistTrackSync::from_raw(trigger_sync),
            queued,
            position_sync: PlaylistTrackSync::from_raw(position_sync),
            tolerant,
            state_bytes: event.payload.clone(),
        })
    }

    /// Edits observed mute, grouping, Press, Motion, Trigger Sync, Queue, Position Sync, or Tolerant fields.
    /// This unscoped form reports an error when the track ID occurs in multiple arrangements.
    pub fn edit_playlist_track(
        &mut self,
        track_id: u32,
        edit: PlaylistTrackEdit,
    ) -> Result<(), FlpError> {
        Self::validate_playlist_track_edit(track_id, edit)?;
        if edit.enabled.is_none()
            && edit.grouped.is_none()
            && edit.press.is_none()
            && edit.motion.is_none()
            && edit.trigger_sync.is_none()
            && edit.queued.is_none()
            && edit.position_sync.is_none()
            && edit.tolerant.is_none()
        {
            return Ok(());
        }
        let event_index = self.playlist_track_event_index(None, track_id)?;
        self.write_playlist_track_edit(event_index, edit)
    }

    /// Edits one Playlist track in a specific arrangement while preserving all other state bytes.
    pub fn edit_playlist_track_in_arrangement(
        &mut self,
        arrangement_id: u16,
        track_id: u32,
        edit: PlaylistTrackEdit,
    ) -> Result<(), FlpError> {
        Self::validate_playlist_track_edit(track_id, edit)?;
        if edit.enabled.is_none()
            && edit.grouped.is_none()
            && edit.press.is_none()
            && edit.motion.is_none()
            && edit.trigger_sync.is_none()
            && edit.queued.is_none()
            && edit.position_sync.is_none()
            && edit.tolerant.is_none()
        {
            return Ok(());
        }
        let event_index = self.playlist_track_event_index(Some(arrangement_id), track_id)?;
        self.write_playlist_track_edit(event_index, edit)
    }

    fn validate_playlist_track_edit(
        track_id: u32,
        edit: PlaylistTrackEdit,
    ) -> Result<(), FlpError> {
        if track_id == 0 || track_id > 500 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track id must be between 1 and 500",
            ));
        }
        if edit.grouped == Some(true) && track_id == 1 {
            return Err(FlpError::UnsupportedEdit(
                "the first Playlist track cannot be grouped with a track above it",
            ));
        }
        Ok(())
    }

    fn playlist_track_event_index(
        &self,
        arrangement_id: Option<u16>,
        track_id: u32,
    ) -> Result<usize, FlpError> {
        let (start, end) = if let Some(arrangement_id) = arrangement_id {
            let has_arrangement_markers = self
                .events
                .iter()
                .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
            if !has_arrangement_markers {
                if arrangement_id != 0 {
                    return Err(FlpError::UnsupportedEdit(
                        "the requested Playlist arrangement does not exist",
                    ));
                }
                (0, self.events.len())
            } else {
                let Some(marker_index) = self.events.iter().position(|event| {
                    event.opcode == 0x63
                        && event.payload.len() == 2
                        && u16::from_le_bytes([event.payload[0], event.payload[1]])
                            == arrangement_id
                }) else {
                    return Err(FlpError::UnsupportedEdit(
                        "the requested Playlist arrangement does not exist",
                    ));
                };
                let start = marker_index + 1;
                let end = self.events[start..]
                    .iter()
                    .position(|event| {
                        event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
                    })
                    .map_or(self.events.len(), |offset| start + offset);
                (start, end)
            }
        } else {
            (0, self.events.len())
        };
        let mut matching_events = (start..end).filter(|index| {
            let event = &self.events[*index];
            event.opcode == 0xEE
                && event.payload.len() == 70
                && u32::from_le_bytes(event.payload[..4].try_into().unwrap()) == track_id
        });
        let Some(event_index) = matching_events.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track does not exist",
            ));
        };
        if matching_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track id is ambiguous",
            ));
        }
        Ok(event_index)
    }

    fn write_playlist_track_edit(
        &mut self,
        event_index: usize,
        edit: PlaylistTrackEdit,
    ) -> Result<(), FlpError> {
        if let Some(enabled) = edit.enabled {
            write_event_payload_bytes(&mut self.events[event_index], 12, &[u8::from(enabled)])?;
        }
        if let Some(grouped) = edit.grouped {
            write_event_payload_bytes(&mut self.events[event_index], 46, &[u8::from(grouped)])?;
        }
        if let Some(motion) = edit.motion {
            write_event_payload_bytes(
                &mut self.events[event_index],
                22,
                &motion.raw().to_le_bytes(),
            )?;
        }
        if let Some(press) = edit.press {
            write_event_payload_bytes(
                &mut self.events[event_index],
                26,
                &press.raw().to_le_bytes(),
            )?;
        }
        if let Some(trigger_sync) = edit.trigger_sync {
            write_event_payload_bytes(
                &mut self.events[event_index],
                30,
                &trigger_sync.raw().to_le_bytes(),
            )?;
        }
        if let Some(queued) = edit.queued {
            write_event_payload_bytes(
                &mut self.events[event_index],
                34,
                &i32::from(queued).to_le_bytes(),
            )?;
        }
        if let Some(position_sync) = edit.position_sync {
            write_event_payload_bytes(
                &mut self.events[event_index],
                42,
                &position_sync.raw().to_le_bytes(),
            )?;
        }
        if let Some(tolerant) = edit.tolerant {
            write_event_payload_bytes(
                &mut self.events[event_index],
                38,
                &i32::from(tolerant).to_le_bytes(),
            )?;
        }
        Ok(())
    }

    /// Renames an existing Playlist track while preserving its other events and state bytes.
    pub fn rename_playlist_track(&mut self, track_id: u32, name: &str) -> Result<(), FlpError> {
        if track_id == 0 || track_id > 500 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track id must be between 1 and 500",
            ));
        }
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track names cannot contain a null character",
            ));
        }

        let mut candidate = self.clone();
        let mut matching_events =
            candidate
                .events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    if event.opcode != 0xEE || event.payload.len() != 70 {
                        return None;
                    }
                    let id = u32::from_le_bytes([
                        event.payload[0],
                        event.payload[1],
                        event.payload[2],
                        event.payload[3],
                    ]);
                    (id == track_id).then_some(index)
                });
        let Some(event_index) = matching_events.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track does not exist",
            ));
        };
        if matching_events.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Playlist track id is ambiguous",
            ));
        }

        let has_state_byte = candidate
            .events
            .get(event_index + 1)
            .is_some_and(|event| event.opcode == 0x2B && event.encoding == PayloadEncoding::Byte);
        let name_event_index = event_index + if has_state_byte { 2 } else { 1 };
        if candidate
            .events
            .get(name_event_index)
            .is_some_and(|event| event.opcode == 0xEF)
        {
            let event = &candidate.events[name_event_index];
            if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                return Err(FlpError::UnsupportedEdit(
                    "the Playlist track name is not a length-prefixed string",
                ));
            }
            let replacement = replace_project_string_payload(
                &event.payload,
                name,
                candidate.project_strings_use_utf16(),
            )?;
            candidate.events[name_event_index].replace_data_payload(replacement)?;
        } else {
            let payload = encode_project_string(name, candidate.project_strings_use_utf16())?;
            candidate
                .events
                .insert(name_event_index, FlpEvent::new_data(0xEF, payload)?);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Returns Mixer insert summaries for the observed adjacent `0x9A`, `0x93`, `0x95`
    /// record signature. This is intentionally read-only; the unparsed insert and effect data
    /// remains available byte-for-byte through `events()`.
    pub fn mixer_inserts(&self) -> Vec<MixerInsertSummary> {
        let flag_opcodes: &[u8] = match self
            .project_version
            .as_deref()
            .and_then(|version| version.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
        {
            Some(major) if major >= 25 => &[0xEC],
            Some(_) => &[0xDC],
            None => &[0xEC, 0xDC],
        };
        let starts = self
            .events
            .windows(3)
            .enumerate()
            .filter_map(|(index, window)| {
                (window[0].opcode == 0x9A
                    && window[0].payload.len() == 4
                    && window[1].opcode == 0x93
                    && window[1].payload.len() == 4
                    && window[2].opcode == 0x95
                    && window[2].payload.len() == 4)
                    .then_some(index)
            })
            .collect::<Vec<_>>();

        starts
            .iter()
            .enumerate()
            .map(|(ordinal, &start)| {
                let end = starts
                    .get(ordinal + 1)
                    .copied()
                    .unwrap_or(self.events.len());
                let insert_events = &self.events[start..end];
                let read_i32 = |offset: usize| {
                    i32::from_le_bytes(
                        self.events[start + offset].payload[..4]
                            .try_into()
                            .expect("the Mixer record signature checked four-byte fields"),
                    )
                };
                let color_raw = u32::from_le_bytes(
                    self.events[start + 2].payload[..4]
                        .try_into()
                        .expect("the Mixer record signature checked four-byte fields"),
                );
                let icon_raw = insert_events
                    .iter()
                    .find(|event| event.opcode == 0x5F && event.payload.len() == 2)
                    .map(|event| i16::from_le_bytes([event.payload[0], event.payload[1]]));
                let name = insert_events
                    .iter()
                    .find(|event| event.opcode == 0xCC)
                    .and_then(|event| {
                        decode_project_string(&event.payload, self.project_version.as_deref())
                    })
                    .filter(|value| !value.is_empty());
                let mut flags_records = insert_events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| {
                        flag_opcodes.contains(&event.opcode) && event.payload.len() == 12
                    })
                    .map(|(offset, event)| {
                        (
                            start + offset,
                            u32::from_le_bytes(
                                event.payload[4..8]
                                    .try_into()
                                    .expect("Mixer insert flags have four bytes"),
                            ),
                        )
                    });
                let flags_record = flags_records
                    .next()
                    .filter(|_| flags_records.next().is_none());

                MixerInsertSummary {
                    ordinal,
                    input_raw: read_i32(0),
                    output_raw: read_i32(1),
                    color_raw,
                    icon_raw,
                    name,
                    flags: flags_record.map(|(_, flags)| flags),
                    flags_event_index: flags_record.map(|(event_index, _)| event_index),
                    first_event_index: start,
                    end_event_index: end,
                }
            })
            .collect()
    }

    /// Reads the ten Mixer effect slots for inserts whose `0x62` boundaries appear in
    /// zero-through-nine order. The first boundary precedes slot zero's data; boundaries one
    /// through nine close slots zero through eight, and the following `0x93` closes slot nine
    /// when present. A complete boundary set is required so an unrelated legacy `0x62` marker
    /// cannot be mistaken for an FX slot. Plug-in state remains opaque and lossless.
    pub fn mixer_effect_slots(&self) -> Vec<MixerEffectSlotSummary> {
        let mut slots = Vec::new();
        for insert in self.mixer_inserts() {
            let first_slot_event = insert.first_event_index + 3;
            let mut next_boundary_index = 0_u16;
            let mut slot_start = first_slot_event;
            let mut insert_slots = Vec::new();
            let mut first_boundary_seen = false;

            let summarize_slot = |slot_index, start, end| {
                let slot_events = &self.events[start..end];
                let plugin_identifier = unique_project_string_in_events(
                    slot_events,
                    0xC9,
                    self.project_version.as_deref(),
                );
                let display_name = unique_project_string_in_events(
                    slot_events,
                    0xCB,
                    self.project_version.as_deref(),
                );
                let state_event_count = slot_events
                    .iter()
                    .filter(|event| event.opcode == 0xD5)
                    .count();
                MixerEffectSlotSummary {
                    insert_ordinal: insert.ordinal,
                    slot_index,
                    plugin_identifier,
                    display_name,
                    state_event_count,
                    first_event_index: start,
                    end_event_index: end,
                }
            };

            for boundary_index in first_slot_event..insert.end_event_index {
                let boundary = &self.events[boundary_index];
                if boundary.opcode != 0x62
                    || boundary.encoding != PayloadEncoding::Word
                    || boundary.payload.len() != 2
                {
                    continue;
                }

                let slot_index = u16::from_le_bytes([boundary.payload[0], boundary.payload[1]]);
                if slot_index != next_boundary_index {
                    insert_slots.clear();
                    break;
                }

                if !first_boundary_seen {
                    first_boundary_seen = true;
                    next_boundary_index += 1;
                    continue;
                }

                insert_slots.push(summarize_slot(slot_index - 1, slot_start, boundary_index));
                slot_start = boundary_index + 1;
                next_boundary_index += 1;
                if next_boundary_index == 10 {
                    break;
                }
            }

            if next_boundary_index == 10 {
                let last_slot_end = (slot_start..insert.end_event_index)
                    .find(|&event_index| {
                        self.events[event_index].opcode == 0x93
                            && self.events[event_index].encoding == PayloadEncoding::Dword
                            && self.events[event_index].payload.len() == 4
                    })
                    .unwrap_or(insert.end_event_index);
                insert_slots.push(summarize_slot(9, slot_start, last_slot_end));
                slots.extend(insert_slots);
            }
        }
        slots
    }

    /// Edits known bits in one existing Mixer insert flag event while preserving
    /// unrecognized bits, reserved bytes, and all other source events.
    pub fn edit_mixer_insert_flags(
        &mut self,
        insert_ordinal: usize,
        edit: MixerInsertEdit,
    ) -> Result<(), FlpError> {
        let insert = self
            .mixer_inserts()
            .into_iter()
            .find(|insert| insert.ordinal() == insert_ordinal)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert does not exist",
            ))?;
        let Some(mut flags) = insert.flags else {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer insert has no unique recognized flags event",
            ));
        };
        let event_index = insert.flags_event_index.ok_or(FlpError::UnsupportedEdit(
            "the requested Mixer insert has no unique recognized flags event",
        ))?;
        for (bit, value) in [
            (3, edit.enabled),
            (12, edit.soloed),
            (0, edit.polarity_reversed),
            (1, edit.swap_left_right),
            (2, edit.effects_enabled),
        ] {
            if let Some(value) = value {
                let mask = 1_u32 << bit;
                if value {
                    flags |= mask;
                } else {
                    flags &= !mask;
                }
            }
        }
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert flags event does not exist",
            ))?;
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) || event.payload.len() != 12 {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer insert flags event has an unsupported layout",
            ));
        }
        write_event_payload_bytes(&mut self.events[event_index], 4, &flags.to_le_bytes())
    }

    /// Renames a recognized Mixer insert through its existing `0xCC` name event.
    /// The event's string encoding, terminator convention, and bytes after the
    /// terminator are retained; every other event remains untouched.
    pub fn set_mixer_insert_name(
        &mut self,
        insert_ordinal: usize,
        name: &str,
    ) -> Result<(), FlpError> {
        if name.contains('\0') {
            return Err(FlpError::UnsupportedEdit(
                "Mixer insert names cannot contain an embedded NUL character",
            ));
        }

        let insert = self
            .mixer_inserts()
            .into_iter()
            .find(|insert| insert.ordinal() == insert_ordinal)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer insert does not exist",
            ))?;
        let event_index = insert
            .event_range()
            .find(|index| self.events[*index].opcode == 0xCC)
            .ok_or(FlpError::UnsupportedEdit(
                "the selected Mixer insert has no recognized 0xCC name event",
            ))?;
        let event = &self.events[event_index];
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the selected Mixer insert's 0xCC event is not a length-prefixed data event",
            ));
        }

        let old_payload = &event.payload;
        let utf16 = project_string_is_utf16(old_payload, self.project_version.as_deref());
        let (suffix_start, had_terminator) = if utf16 {
            old_payload
                .as_chunks::<2>()
                .0
                .iter()
                .position(|pair| u16::from_le_bytes(*pair) == 0)
                .map_or((old_payload.len(), false), |unit_index| {
                    ((unit_index + 1) * 2, true)
                })
        } else {
            old_payload
                .iter()
                .position(|byte| *byte == 0)
                .map_or((old_payload.len(), false), |byte_index| {
                    (byte_index + 1, true)
                })
        };

        let mut replacement_payload = Vec::new();
        if utf16 {
            for unit in name.encode_utf16() {
                replacement_payload.extend_from_slice(&unit.to_le_bytes());
            }
            if had_terminator {
                replacement_payload.extend_from_slice(&[0, 0]);
            }
        } else {
            for character in name.chars() {
                let byte = windows_1252_byte(character).ok_or(FlpError::UnsupportedEdit(
                    "the name contains a character unavailable in the project's legacy encoding",
                ))?;
                replacement_payload.push(byte);
            }
            if had_terminator {
                replacement_payload.push(0);
            }
        }
        replacement_payload.extend_from_slice(&old_payload[suffix_start..]);

        self.events[event_index].replace_data_payload(replacement_payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Decodes only the fixed 12-byte framing observed in Mixer `0xE1` parameter records.
    /// Unknown fields remain raw, and a different record size is reported instead of guessed.
    pub fn mixer_parameter_records(&self) -> Result<Vec<MixerParameterRecord>, FlpError> {
        const RECORD_SIZE: usize = 12;
        let mut records = Vec::new();
        for (event_index, event) in self.events.iter().enumerate() {
            if event.opcode != 0xE1 {
                continue;
            }
            if !event.payload.len().is_multiple_of(RECORD_SIZE) {
                return Err(FlpError::InvalidEvent {
                    offset: event.file_offset,
                    detail: "Mixer parameter payload is not a whole number of 12-byte records",
                });
            }
            for (record_index, bytes) in event
                .payload
                .as_chunks::<RECORD_SIZE>()
                .0
                .iter()
                .enumerate()
            {
                records.push(MixerParameterRecord {
                    event_index,
                    record_index,
                    prefix: bytes[..4]
                        .try_into()
                        .expect("the Mixer parameter record has four prefix bytes"),
                    parameter_id: bytes[4],
                    reserved: bytes[5],
                    channel_data: u16::from_le_bytes([bytes[6], bytes[7]]),
                    value: i32::from_le_bytes(
                        bytes[8..12]
                            .try_into()
                            .expect("the Mixer parameter record has a four-byte value"),
                    ),
                });
            }
        }
        Ok(records)
    }

    /// Returns saved Mixer send-level records with a paired routing flag when available.
    /// Newer FLP projects store a project-wide `0xE7` routing byte stream; older projects
    /// store insert-scoped `0xEB` arrays. Parameter IDs and candidate source-track numbering
    /// remain unverified mappings.
    pub fn mixer_send_summaries(&self) -> Result<Vec<MixerSendSummary>, FlpError> {
        let inserts = self.mixer_inserts();
        let parameters = self.mixer_parameter_records()?;
        let first_channel = self
            .events
            .iter()
            .position(|event| event.opcode == 0x40)
            .unwrap_or(self.events.len());
        let project_routing_flags = self.events[first_channel..]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
            })
            .flat_map(|(offset, event)| {
                let event_index = first_channel + offset;
                event
                    .payload
                    .iter()
                    .copied()
                    .enumerate()
                    .map(move |(flag_index, flag)| (event_index, flag_index, flag != 0))
            })
            .collect::<Vec<_>>();
        let has_project_routing_stream = self.events[first_channel..].iter().any(|event| {
            event.opcode == 0xE7 && matches!(event.encoding, PayloadEncoding::Data { .. })
        });
        let mut seen_route_parameters = HashMap::<usize, HashSet<u8>>::new();
        let mut ambiguous_inserts = HashSet::new();
        for record in parameters
            .iter()
            .filter(|record| record.kind() == MixerParameterKind::RouteVolume)
        {
            if let Some(insert_ordinal) = record.candidate_insert_ordinal() {
                let parameter_ids = seen_route_parameters.entry(insert_ordinal).or_default();
                if !parameter_ids.insert(record.parameter_id()) {
                    ambiguous_inserts.insert(insert_ordinal);
                }
            }
        }
        let routing_flags_by_insert = inserts
            .iter()
            .map(|insert| {
                let insert_range = insert.event_range();
                let routing_flags = self.events[insert_range.clone()]
                    .iter()
                    .enumerate()
                    .find(|(_, event)| {
                        event.opcode == 0xEB
                            && matches!(event.encoding, PayloadEncoding::Data { .. })
                    })
                    .map(|(offset, event)| (insert_range.start + offset, event.payload.clone()));
                (insert.ordinal(), routing_flags)
            })
            .collect::<HashMap<_, _>>();
        let mut route_indices_by_insert = HashMap::<usize, usize>::new();
        let mut project_route_index = 0usize;
        let mut summaries = Vec::new();

        for record in parameters
            .iter()
            .filter(|record| record.kind() == MixerParameterKind::RouteVolume)
        {
            let Some(insert_ordinal) = record.candidate_insert_ordinal() else {
                continue;
            };
            let (enabled, routing_flag_location) = if has_project_routing_stream {
                let paired_flag = project_routing_flags.get(project_route_index).copied();
                project_route_index += 1;
                if ambiguous_inserts.contains(&insert_ordinal) {
                    (None, None)
                } else {
                    paired_flag.map_or((None, None), |(event_index, flag_index, enabled)| {
                        (Some(enabled), Some((event_index, flag_index)))
                    })
                }
            } else {
                let route_index = route_indices_by_insert.entry(insert_ordinal).or_default();
                let paired_flag = routing_flags_by_insert
                    .get(&insert_ordinal)
                    .and_then(Option::as_ref)
                    .and_then(|(event_index, flags)| {
                        flags
                            .get(*route_index)
                            .map(|flag| (*event_index, *route_index, *flag != 0))
                    });
                *route_index += 1;
                if ambiguous_inserts.contains(&insert_ordinal) {
                    (None, None)
                } else {
                    paired_flag.map_or((None, None), |(event_index, flag_index, enabled)| {
                        (Some(enabled), Some((event_index, flag_index)))
                    })
                }
            };
            summaries.push(MixerSendSummary {
                insert_ordinal,
                route_parameter_id: record.parameter_id(),
                value: record.value(),
                enabled,
                routing_flag_location,
                event_index: record.event_index(),
                record_index: record.record_index(),
            });
        }

        Ok(summaries)
    }

    /// Changes the signed value in one existing 12-byte Mixer parameter record.
    /// The record prefix, ID, reserved byte, target word, and all other records remain intact.
    pub fn set_mixer_parameter_record_value(
        &mut self,
        event_index: usize,
        record_index: usize,
        value: i32,
    ) -> Result<(), FlpError> {
        const RECORD_SIZE: usize = 12;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer parameter event does not exist",
            ))?;
        if event.opcode != 0xE1 {
            return Err(FlpError::UnsupportedEdit(
                "the requested event is not a Mixer parameter event",
            ));
        }
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Mixer parameter event is not a length-prefixed data event",
            ));
        }
        if !event.payload.len().is_multiple_of(RECORD_SIZE) {
            return Err(FlpError::InvalidEvent {
                offset: event.file_offset,
                detail: "Mixer parameter payload is not a whole number of 12-byte records",
            });
        }
        let record_start = record_index
            .checked_mul(RECORD_SIZE)
            .ok_or(FlpError::LengthOverflow)?;
        let value_start = record_start
            .checked_add(8)
            .ok_or(FlpError::LengthOverflow)?;
        let value_end = value_start.checked_add(4).ok_or(FlpError::LengthOverflow)?;
        if value_end > event.payload.len() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Mixer parameter record does not exist",
            ));
        }

        let mut payload = event.payload.clone();
        payload[value_start..value_end].copy_from_slice(&value.to_le_bytes());
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Toggles one existing Mixer routing flag without changing other bytes.
    pub fn set_mixer_routing_flag(
        &mut self,
        event_index: usize,
        routing_flag_index: usize,
        enabled: bool,
    ) -> Result<(), FlpError> {
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Mixer routing event does not exist",
            ))?;
        let is_project_routing_event = event.opcode == 0xE7
            && self.events[..event_index]
                .iter()
                .any(|previous| previous.opcode == 0x40);
        if event.opcode != 0xEB && !is_project_routing_event {
            return Err(FlpError::UnsupportedEdit(
                "the requested event is not a Mixer routing event",
            ));
        }
        if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Mixer routing event is not a length-prefixed data event",
            ));
        }
        write_event_payload_bytes(
            &mut self.events[event_index],
            routing_flag_index,
            &[u8::from(enabled)],
        )?;
        self.refresh_event_offsets()?;
        Ok(())
    }

    /// Edits fields with established playlist record offsets while preserving every other byte.
    /// `clip_index` is zero-based in the selected arrangement's stored clip order. Candidate
    /// source-window ticks can be edited for 88-byte Audio Clips.
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
        if edit.start_offset.is_some_and(|value| !value.is_finite())
            || edit.end_offset.is_some_and(|value| !value.is_finite())
            || edit.scale.is_some_and(|value| {
                !value.is_finite() || (clip.record_size == 60 && !(value as f32).is_finite())
            })
        {
            return Err(FlpError::UnsupportedEdit(
                "playlist clip offsets and scale must fit finite stored numbers",
            ));
        }
        if edit
            .source_window_ticks
            .is_some_and(|(start, end)| start >= end)
        {
            return Err(FlpError::UnsupportedEdit(
                "Audio Clip source end tick must be after its start tick",
            ));
        }
        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let record_size = clip.record_size;

        if clip.has_opaque_record_tail()
            && (edit.item_flags.is_some()
                || edit.start_offset.is_some()
                || edit.end_offset.is_some()
                || edit.scale.is_some())
        {
            return Err(FlpError::UnsupportedEdit(
                "this Playlist record version keeps clip flags, source offsets, and scale in opaque fields",
            ));
        }
        let mut edits_audio_clip_window = false;
        if let Some((start_tick, end_tick)) = edit.source_window_ticks {
            if record_size != 88 {
                return Err(FlpError::UnsupportedEdit(
                    "source-window tick editing is only available for 88-byte Playlist records",
                ));
            }
            if edit.item_index.is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "edit the Audio Clip source window separately from its target",
                ));
            }
            let is_audio_clip = match clip.target() {
                PlaylistClipTarget::Channel { id } => {
                    let channels = self.channels();
                    let mut matching = channels.iter().filter(|channel| channel.id() == id);
                    matching
                        .next()
                        .is_some_and(|channel| channel.kind() == Some(4))
                        && matching.next().is_none()
                }
                PlaylistClipTarget::Pattern { .. } => false,
            };
            if !is_audio_clip {
                return Err(FlpError::UnsupportedEdit(
                    "source-window tick editing applies to 88-byte Audio Clips",
                ));
            }
            if edit
                .length_ticks
                .is_some_and(|length| end_tick.checked_sub(start_tick) != Some(length))
            {
                return Err(FlpError::UnsupportedEdit(
                    "an Audio Clip's source tick span must match its edited timeline length",
                ));
            }
            edits_audio_clip_window = true;
            debug_assert!(start_tick < end_tick);
        }
        if edit.scale.is_some() && !matches!(record_size, 60 | 80) {
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
            if (!clip.can_resize_timeline_length() && !edits_audio_clip_window)
                || edit
                    .item_index
                    .is_some_and(|item_index| item_index < clip.pattern_base)
            {
                return Err(FlpError::UnsupportedEdit(
                    "this Playlist clip does not have a recognized full source window for resizing",
                ));
            }
            writes.push((record_start + 8, value.to_le_bytes().to_vec()));
            if record_size == 88 && edit.source_window_ticks.is_none() {
                writes.push((record_start + 28, value.to_le_bytes().to_vec()));
            }
        }
        if let Some(value) = edit.raw_track_index {
            writes.push((record_start + 12, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.group {
            writes.push((record_start + 14, value.to_le_bytes().to_vec()));
        }
        if edit.item_flags.is_some() || edit.muted.is_some() {
            let flags_offset = record_start
                .checked_add(18)
                .ok_or(FlpError::LengthOverflow)?;
            let flags_end = flags_offset
                .checked_add(2)
                .ok_or(FlpError::LengthOverflow)?;
            let stored_flags = self.events[event_index]
                .payload
                .get(flags_offset..flags_end)
                .ok_or(FlpError::UnsupportedEdit(
                    "the Playlist clip flags field is unavailable",
                ))?;
            let mut flags = edit
                .item_flags
                .unwrap_or_else(|| u16::from_le_bytes([stored_flags[0], stored_flags[1]]));
            if let Some(muted) = edit.muted {
                if muted {
                    flags |= PLAYLIST_CLIP_MUTED_FLAG;
                } else {
                    flags &= !PLAYLIST_CLIP_MUTED_FLAG;
                }
            }
            writes.push((flags_offset, flags.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.start_offset {
            writes.push((record_start + 24, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.end_offset {
            writes.push((record_start + 28, value.to_le_bytes().to_vec()));
        }
        if let Some((start_tick, end_tick)) = edit.source_window_ticks {
            writes.push((record_start + 24, start_tick.to_le_bytes().to_vec()));
            writes.push((record_start + 28, end_tick.to_le_bytes().to_vec()));
        }
        if let Some(value) = edit.scale {
            if record_size == 60 {
                writes.push((record_start + 52, (value as f32).to_le_bytes().to_vec()));
            } else {
                writes.push((record_start + 64, value.to_le_bytes().to_vec()));
            }
        }
        for (payload_offset, bytes) in writes {
            write_event_payload_bytes(&mut self.events[event_index], payload_offset, &bytes)?;
        }
        Ok(())
    }

    /// Makes one Playlist Audio Clip use its own copied kind-4 channel record.
    ///
    /// The sample path and all opaque channel events are copied byte-for-byte. Only the new
    /// channel marker and the selected clip's item index change. The source channel and every
    /// other Playlist clip remain attached to their original channel.
    pub fn make_playlist_audio_clip_unique(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<u16, FlpError> {
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
        let PlaylistClipTarget::Channel { id: source_id } = clip.target() else {
            return Err(FlpError::UnsupportedEdit(
                "Make unique applies to Playlist Audio Clips",
            ));
        };

        let channels = self.channels();
        let mut matching_channels = channels.iter().filter(|channel| channel.id() == source_id);
        let Some(source_channel) = matching_channels.next() else {
            return Err(FlpError::ChannelNotFound(source_id));
        };
        if matching_channels.next().is_some() {
            return Err(FlpError::AmbiguousChannelId(source_id));
        }
        if source_channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "Make unique applies to kind-4 Audio Clip channels",
            ));
        }

        let source_range = source_channel.event_range();
        let Some(source_marker) = self.events.get(source_range.start) else {
            return Err(FlpError::UnsupportedEdit(
                "the Audio Clip channel has no recognized channel marker",
            ));
        };
        if source_marker.opcode != 0x40
            || source_marker.encoding != PayloadEncoding::Word
            || source_marker.payload.len() != 2
            || u16::from_le_bytes([source_marker.payload[0], source_marker.payload[1]]) != source_id
        {
            return Err(FlpError::UnsupportedEdit(
                "the Audio Clip channel marker is not a recognized word event",
            ));
        }

        let record_start = clip
            .source_record_index
            .checked_mul(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let source_clip_event =
            self.events
                .get(clip.source_event_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the Playlist Audio Clip record event no longer exists",
                ))?;
        if source_clip_event.opcode != 0xE9
            || !matches!(source_clip_event.encoding, PayloadEncoding::Data { .. })
            || clip.record_size == 0
            || record_end > source_clip_event.payload.len()
            || !source_clip_event
                .payload
                .len()
                .is_multiple_of(clip.record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip record is not recognized",
            ));
        }

        let used_ids = channels
            .iter()
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        let new_id = used_ids
            .iter()
            .copied()
            .filter(|id| *id < clip.pattern_base)
            .max()
            .and_then(|id| id.checked_add(1))
            .filter(|id| *id < clip.pattern_base && !used_ids.contains(id))
            .or_else(|| (0..clip.pattern_base).find(|id| !used_ids.contains(id)))
            .ok_or(FlpError::UnsupportedEdit(
                "the project has no unused channel ID below this clip's pattern base",
            ))?;
        let insertion_index = channels
            .last()
            .map(|channel| channel.event_range().end)
            .unwrap_or(self.events.len());
        if insertion_index > self.events.len() {
            return Err(FlpError::UnsupportedEdit(
                "Channel Rack event ranges are inconsistent",
            ));
        }
        let channel_count = u16::try_from(channels.len().saturating_add(1)).map_err(|_| {
            FlpError::UnsupportedEdit("the project has too many channels to make one unique")
        })?;

        let mut copied_events = self.events[source_range].to_vec();
        copied_events[0] = FlpEvent::new_word(0x40, new_id);
        let mut candidate = self.clone();
        write_event_payload_bytes(
            candidate
                .events
                .get_mut(clip.source_event_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the Playlist Audio Clip record event no longer exists",
                ))?,
            record_start + 6,
            &new_id.to_le_bytes(),
        )?;
        candidate
            .events
            .splice(insertion_index..insertion_index, copied_events);
        candidate.header.legacy_channel_count = channel_count;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(new_id)
    }

    /// Creates a Pattern Clip from a recognized Playlist record in the target arrangement.
    /// For an empty arrangement, a Pattern Clip record from another arrangement is reused.
    /// Position, item index, length, track, and (when present) clip ID are assigned; other
    /// record bytes come from the template and remain opaque.
    pub fn create_playlist_pattern_clip(
        &mut self,
        arrangement_id: u16,
        pattern_id: u16,
        position_ticks: u32,
        length_ticks: u32,
        track_index: u16,
    ) -> Result<usize, FlpError> {
        if length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Pattern Clip length must be greater than zero",
            ));
        }
        if track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let patterns = self.patterns()?;
        let mut matching_patterns = patterns.iter().filter(|pattern| pattern.id == pattern_id);
        if matching_patterns.next().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern does not exist",
            ));
        }
        if matching_patterns.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the requested pattern id is ambiguous",
            ));
        }

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
        let template_in_arrangement = arrangement
            .clips
            .iter()
            .enumerate()
            .find(|(_, clip)| matches!(clip.target(), PlaylistClipTarget::Pattern { .. }))
            .or_else(|| arrangement.clips.iter().enumerate().next());
        let item_index = template_in_arrangement
            .map(|(_, template)| template.pattern_base)
            .or_else(|| {
                arrangements
                    .iter()
                    .flat_map(|arrangement| arrangement.clips.iter())
                    .find(|clip| matches!(clip.target(), PlaylistClipTarget::Pattern { .. }))
                    .map(|template| template.pattern_base)
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the project has no recognized Pattern Clip record to use as a template",
            ))?
            .checked_add(pattern_id)
            .ok_or(FlpError::LengthOverflow)?;

        if template_in_arrangement.is_none() {
            let template = arrangements
                .iter()
                .flat_map(|arrangement| arrangement.clips.iter())
                .find(|clip| matches!(clip.target(), PlaylistClipTarget::Pattern { .. }))
                .expect("a Pattern Clip template was checked above");
            let record_size = template.record_size;
            let record_start = template
                .source_record_index
                .checked_mul(record_size)
                .ok_or(FlpError::LengthOverflow)?;
            let record_end = record_start
                .checked_add(record_size)
                .ok_or(FlpError::LengthOverflow)?;
            let event =
                self.events
                    .get(template.source_event_index)
                    .ok_or(FlpError::UnsupportedEdit(
                        "the Playlist clip template event no longer exists",
                    ))?;
            if event.opcode != 0xE9
                || !matches!(event.encoding, PayloadEncoding::Data { .. })
                || record_end > event.payload.len()
                || !event.payload.len().is_multiple_of(record_size)
            {
                return Err(FlpError::UnsupportedEdit(
                    "the Playlist Pattern Clip template record is not recognized",
                ));
            }
            let mut record = event.payload[record_start..record_end].to_vec();
            record[6..8].copy_from_slice(&item_index.to_le_bytes());
            initialize_new_playlist_clip_length(&mut record, length_ticks);
            let clipboard = PlaylistClipClipboard { raw_record: record };
            return self.paste_playlist_clip(
                arrangement_id,
                &clipboard,
                position_ticks,
                499u16 - track_index,
            );
        }

        let (template_index, template) =
            template_in_arrangement.expect("a clip in the target arrangement was checked above");

        let record_size = template.record_size;
        let record_start = template
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = template.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip template event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template is not in a length-prefixed data event",
            ));
        }
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip template does not fit its event payload",
            ));
        }

        let raw_track_index = 499u16 - track_index;
        let mut record = event.payload[record_start..record_end].to_vec();
        record[0..4].copy_from_slice(&position_ticks.to_le_bytes());
        record[6..8].copy_from_slice(&item_index.to_le_bytes());
        initialize_new_playlist_clip_length(&mut record, length_ticks);
        record[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        if playlist_record_has_clip_id(record_size) {
            record[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, record);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(template_index.saturating_add(1))
    }

    /// Creates an Audio Clip by copying a recognized project Playlist clip record.
    ///
    /// The requested channel must be an Audio Clip channel. Position, channel target, length,
    /// track, and (when present) clip ID are assigned; all other record bytes remain copied from
    /// the template. An Audio Clip record in another arrangement can seed an empty arrangement.
    pub fn create_playlist_audio_clip(
        &mut self,
        arrangement_id: u16,
        channel_id: u16,
        position_ticks: u32,
        length_ticks: u32,
        track_index: u16,
    ) -> Result<usize, FlpError> {
        if length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clip length must be greater than zero",
            ));
        }
        if track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let channels = self.channels();
        let mut matching_channels = channels.iter().filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the requested channel is not one unambiguous Audio Clip channel",
            ));
        }
        if channel.sample_path().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Audio Clip channel has no recognized sample path",
            ));
        }

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
        let channel_ids = channels
            .iter()
            .filter(|channel| channel.kind() == Some(4))
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        let template = arrangement
            .clips
            .iter()
            .find(|clip| {
                matches!(clip.target(), PlaylistClipTarget::Channel { id } if id == channel_id)
            })
            .or_else(|| {
                arrangement.clips.iter().find(|clip| {
                    matches!(clip.target(), PlaylistClipTarget::Channel { id } if channel_ids.contains(&id))
                })
            })
            .or_else(|| {
                arrangements
                    .iter()
                    .flat_map(|candidate| candidate.clips.iter())
                    .find(|clip| {
                        matches!(clip.target(), PlaylistClipTarget::Channel { id } if id == channel_id)
                    })
            })
            .or_else(|| {
                arrangements
                    .iter()
                    .flat_map(|candidate| candidate.clips.iter())
                    .find(|clip| {
                        matches!(clip.target(), PlaylistClipTarget::Channel { id } if channel_ids.contains(&id))
                    })
            });
        let Some(template) = template else {
            return Err(FlpError::UnsupportedEdit(
                "the project has no recognized Audio Clip record to use as a template",
            ));
        };
        if channel_id >= template.pattern_base {
            return Err(FlpError::UnsupportedEdit(
                "the Audio Clip channel ID does not fit the template's channel target range",
            ));
        }

        let record_size = template.record_size;
        let record_start = template
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record =
            {
                let event = self.events.get(template.source_event_index).ok_or(
                    FlpError::UnsupportedEdit("the Playlist clip template event no longer exists"),
                )?;
                if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                    return Err(FlpError::UnsupportedEdit(
                        "the Playlist clip template is not in a length-prefixed data event",
                    ));
                }
                if record_end > event.payload.len()
                    || !event.payload.len().is_multiple_of(record_size)
                {
                    return Err(FlpError::UnsupportedEdit(
                        "the Playlist clip template does not fit its event payload",
                    ));
                }
                let mut record = event.payload[record_start..record_end].to_vec();
                record[6..8].copy_from_slice(&channel_id.to_le_bytes());
                initialize_new_playlist_clip_length(&mut record, length_ticks);
                record
            };

        let clipboard = PlaylistClipClipboard { raw_record: record };
        self.paste_playlist_clip(
            arrangement_id,
            &clipboard,
            position_ticks,
            499u16 - track_index,
        )
    }

    /// Creates a Playlist Automation Clip from a recognized Automation Clip record.
    ///
    /// The target must be a type-5 channel with an existing `0xEA` point blob. The complete
    /// Playlist record is copied from an Automation Clip in this or another arrangement; only
    /// its position, channel target, length, track, and clip ID (when present) are changed. For
    /// 88-byte records, the established source window is initialized to `[0, length]`.
    pub fn create_playlist_automation_clip(
        &mut self,
        arrangement_id: u16,
        channel_id: u16,
        position_ticks: u32,
        length_ticks: u32,
        track_index: u16,
    ) -> Result<usize, FlpError> {
        if length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Automation Clip length must be greater than zero",
            ));
        }
        if track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let channels = self.channels();
        let mut matching_channels = channels.iter().filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::ChannelNotFound(channel_id));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(5) {
            return Err(FlpError::UnsupportedEdit(
                "the requested channel is not one unambiguous Automation channel",
            ));
        }
        let automation_channels = self.automation_channels()?;
        let automation_channel = automation_channels
            .iter()
            .find(|automation| automation.channel_id() == channel_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the requested Automation channel has no decoded point data",
            ))?;
        if automation_channel.data_event_index().is_none() {
            return Err(FlpError::UnsupportedEdit(
                "the requested Automation channel has no recognized 0xEA point blob",
            ));
        }

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

        let automation_channel_ids = automation_channels
            .iter()
            .filter(|channel| channel.data_event_index().is_some())
            .map(AutomationChannel::channel_id)
            .collect::<HashSet<_>>();
        let is_automation_clip = |clip: &&PlaylistClip| matches!(clip.target(), PlaylistClipTarget::Channel { id } if automation_channel_ids.contains(&id));
        let template = arrangement
            .clips
            .iter()
            .find(|clip| matches!(clip.target(), PlaylistClipTarget::Channel { id } if id == channel_id))
            .or_else(|| arrangement.clips.iter().find(is_automation_clip))
            .or_else(|| {
                arrangements
                    .iter()
                    .flat_map(|candidate| candidate.clips.iter())
                    .find(|clip| {
                        matches!(clip.target(), PlaylistClipTarget::Channel { id } if id == channel_id)
                    })
            })
            .or_else(|| {
                arrangements
                    .iter()
                    .flat_map(|candidate| candidate.clips.iter())
                    .find(is_automation_clip)
            })
            .ok_or(FlpError::UnsupportedEdit(
                "the project has no recognized Automation Clip record to use as a template",
            ))?;
        if channel_id >= template.pattern_base {
            return Err(FlpError::UnsupportedEdit(
                "the Automation channel ID does not fit the template's channel target range",
            ));
        }

        let record_size = template.record_size;
        let record_start = template
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event =
            self.events
                .get(template.source_event_index)
                .ok_or(FlpError::UnsupportedEdit(
                    "the Playlist Automation Clip template event no longer exists",
                ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Automation Clip template is not in a length-prefixed data event",
            ));
        }
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Automation Clip template does not fit its event payload",
            ));
        }

        let mut record = event.payload[record_start..record_end].to_vec();
        record[6..8].copy_from_slice(&channel_id.to_le_bytes());
        initialize_new_playlist_clip_length(&mut record, length_ticks);
        let clipboard = PlaylistClipClipboard { raw_record: record };
        self.paste_playlist_clip(
            arrangement_id,
            &clipboard,
            position_ticks,
            499u16 - track_index,
        )
    }

    /// Removes one Playlist clip's complete stored record without rewriting its neighbors.
    pub fn delete_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
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

        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip is not in a length-prefixed data event",
            ));
        }
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip record does not fit its event payload",
            ));
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.drain(record_start..record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(())
    }

    /// Copies a complete Playlist clip record for a later lossless paste.
    pub fn copy_playlist_clip(
        &self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<PlaylistClipClipboard, FlpError> {
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
        let record_end = record_start
            .checked_add(clip.record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event = self
            .events
            .get(clip.source_event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist clip event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip is not in a length-prefixed data event",
            ));
        }
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(clip.record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip record does not fit its event payload",
            ));
        }

        Ok(PlaylistClipClipboard {
            raw_record: event.payload[record_start..record_end].to_vec(),
        })
    }

    /// Copies selected Playlist clips as complete records while retaining their relative timing
    /// and track spacing.
    pub fn copy_playlist_clip_selection(
        &self,
        arrangement_id: u16,
        clip_indices: &[usize],
    ) -> Result<PlaylistClipSelectionClipboard, FlpError> {
        if clip_indices.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "at least one Playlist clip must be selected",
            ));
        }

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

        let mut clip_indices = clip_indices.to_vec();
        clip_indices.sort_unstable();
        clip_indices.dedup();
        let clips = clip_indices
            .iter()
            .map(|index| {
                arrangement
                    .clips
                    .get(*index)
                    .ok_or(FlpError::UnsupportedEdit(
                        "a selected Playlist clip does not exist",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let origin_position_ticks =
            clips
                .iter()
                .map(|clip| clip.position_ticks)
                .min()
                .ok_or(FlpError::UnsupportedEdit(
                    "at least one Playlist clip must be selected",
                ))?;
        let origin_raw_track_index = clips.iter().map(|clip| clip.raw_track_index).max().ok_or(
            FlpError::UnsupportedEdit("at least one Playlist clip must be selected"),
        )?;
        if clips.iter().any(|clip| clip.raw_track_index > 499) {
            return Err(FlpError::UnsupportedEdit(
                "a selected Playlist clip has an unsupported track index",
            ));
        }

        let clips = clip_indices
            .iter()
            .zip(clips)
            .map(|(index, clip)| {
                Ok((
                    self.copy_playlist_clip(arrangement_id, *index)?,
                    clip.position_ticks,
                    clip.raw_track_index,
                ))
            })
            .collect::<Result<Vec<_>, FlpError>>()?;

        Ok(PlaylistClipSelectionClipboard {
            clips,
            origin_position_ticks,
            origin_raw_track_index,
        })
    }

    /// Pastes a copied clip record into an arrangement, updating its position, track, and clip ID
    /// when present. A paste into an empty arrangement can reuse the copied ID.
    /// The complete record is appended to the arrangement's final clip event, or a new clip event
    /// is created at the end of the arrangement when it has no clip event yet.
    pub fn paste_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clipboard: &PlaylistClipClipboard,
        position_ticks: u32,
        raw_track_index: u16,
    ) -> Result<usize, FlpError> {
        let record_size = clipboard.raw_record.len();
        if !FLP_PLAYLIST_RECORD_SIZES.contains(&record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the copied Playlist clip has an unsupported record size",
            ));
        }
        if raw_track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

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
        let insertion_clip_index = arrangement.clips.len();
        let established_record_size = arrangement.clips.last().map(|clip| clip.record_size);

        let has_arrangement_markers = self
            .events
            .iter()
            .any(|event| event.opcode == 0x63 && event.payload.len() == 2);
        let (region_start, region_end) = if has_arrangement_markers {
            let mut marker_indices = self
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.opcode == 0x63 && event.payload.len() == 2)
                .filter(|(_, event)| {
                    u16::from_le_bytes([event.payload[0], event.payload[1]]) == arrangement_id
                })
                .map(|(index, _)| index);
            let Some(marker_index) = marker_indices.next() else {
                return Err(FlpError::UnsupportedEdit(
                    "the requested arrangement does not exist",
                ));
            };
            if marker_indices.next().is_some() {
                return Err(FlpError::UnsupportedEdit(
                    "the requested arrangement id is ambiguous",
                ));
            }
            let region_end = self
                .events
                .iter()
                .enumerate()
                .skip(marker_index + 1)
                .find(|(_, event)| {
                    event.opcode == 0x62 || (event.opcode == 0x63 && event.payload.len() == 2)
                })
                .map_or(self.events.len(), |(index, _)| index);
            (marker_index + 1, region_end)
        } else {
            (0, self.events.len())
        };
        let target_event_index = self.events[region_start..region_end]
            .iter()
            .rposition(|event| event.opcode == 0xE9)
            .map(|offset| region_start + offset);

        let mut record = clipboard.raw_record.clone();
        record[0..4].copy_from_slice(&position_ticks.to_le_bytes());
        record[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        if playlist_record_has_clip_id(record_size) {
            let preferred_id = u32::from_le_bytes(record[32..36].try_into().unwrap());
            record[32..36].copy_from_slice(
                &next_playlist_clip_id(arrangement, Some(preferred_id))?.to_le_bytes(),
            );
        }

        let mut candidate = self.clone();
        if let Some(event_index) = target_event_index {
            let event = &candidate.events[event_index];
            if !matches!(event.encoding, PayloadEncoding::Data { .. }) {
                return Err(FlpError::UnsupportedEdit(
                    "the target Playlist clip event is not length-prefixed data",
                ));
            }
            let target_record_size = if let Some(record_size) = established_record_size {
                record_size
            } else if event.payload.is_empty() {
                record_size
            } else {
                playlist_clip_record_size(
                    candidate.project_version.as_deref(),
                    &event.payload,
                    event.file_offset,
                )?
            };
            if target_record_size != record_size {
                return Err(FlpError::UnsupportedEdit(
                    "the copied clip record layout does not match the target arrangement",
                ));
            }
            if !event.payload.len().is_multiple_of(record_size) {
                return Err(FlpError::InvalidEvent {
                    offset: event.file_offset,
                    detail: "playlist clip payload is not a whole number of supported records",
                });
            }
            let mut payload = event.payload.clone();
            payload.extend_from_slice(&record);
            candidate.events[event_index].replace_data_payload(payload)?;
        } else {
            let event = FlpEvent::new_data(0xE9, record)?;
            candidate.events.insert(region_end, event);
        }
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(insertion_clip_index)
    }

    /// Pastes a copied Playlist selection as complete records, preserving its relative timing
    /// and track spacing. All clips are inserted transactionally.
    pub fn paste_playlist_clip_selection(
        &mut self,
        arrangement_id: u16,
        clipboard: &PlaylistClipSelectionClipboard,
        position_ticks: u32,
        raw_track_index: u16,
    ) -> Result<Vec<usize>, FlpError> {
        if clipboard.clips.is_empty() {
            return Err(FlpError::UnsupportedEdit(
                "the copied Playlist selection is empty",
            ));
        }
        if raw_track_index > 499 {
            return Err(FlpError::UnsupportedEdit(
                "Playlist track index must be between 0 and 499",
            ));
        }

        let mut candidate = self.clone();
        let mut inserted_indices = Vec::with_capacity(clipboard.clips.len());
        for (clip, source_position, source_track_index) in &clipboard.clips {
            let position_offset = source_position
                .checked_sub(clipboard.origin_position_ticks)
                .ok_or(FlpError::LengthOverflow)?;
            let pasted_position = position_ticks
                .checked_add(position_offset)
                .ok_or(FlpError::LengthOverflow)?;
            let track_offset =
                i32::from(*source_track_index) - i32::from(clipboard.origin_raw_track_index);
            let pasted_track = i32::from(raw_track_index) + track_offset;
            if !(0..=499).contains(&pasted_track) {
                return Err(FlpError::UnsupportedEdit(
                    "the copied Playlist selection does not fit on the target tracks",
                ));
            }
            inserted_indices.push(candidate.paste_playlist_clip(
                arrangement_id,
                clip,
                pasted_position,
                pasted_track as u16,
            )?);
        }

        *self = candidate;
        Ok(inserted_indices)
    }

    /// Duplicates one clip's complete stored record into the same Playlist event, assigning a new
    /// ID when the record layout contains one. The duplicate is inserted immediately after the
    /// source record.
    pub fn duplicate_playlist_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        position_ticks: Option<u32>,
        raw_track_index: Option<u16>,
    ) -> Result<usize, FlpError> {
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
        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the source Playlist event no longer exists",
            ))?;
        if event.opcode != 0xE9 || !matches!(event.encoding, PayloadEncoding::Data { .. }) {
            return Err(FlpError::UnsupportedEdit(
                "the source Playlist clip is not in a length-prefixed data event",
            ));
        }
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        if record_end > event.payload.len() || !event.payload.len().is_multiple_of(record_size) {
            return Err(FlpError::UnsupportedEdit(
                "the source Playlist record does not fit its event payload",
            ));
        }
        let mut duplicate = event.payload[record_start..record_end].to_vec();
        let position =
            position_ticks.unwrap_or_else(|| clip.position_ticks.saturating_add(clip.length_ticks));
        duplicate[..4].copy_from_slice(&position.to_le_bytes());
        if let Some(raw_track_index) = raw_track_index {
            duplicate[12..14].copy_from_slice(&raw_track_index.to_le_bytes());
        }
        if playlist_record_has_clip_id(record_size) {
            duplicate[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }
        let mut payload = event.payload.clone();
        payload.splice(record_end..record_end, duplicate);
        self.events[event_index].replace_data_payload(payload)?;
        self.refresh_event_offsets()?;
        Ok(clip_index.saturating_add(1))
    }

    /// Splits one Audio Clip at a Playlist tick while preserving its source window.
    /// Legacy records need source offsets that map linearly to timeline time; an 88-byte record
    /// needs an explicit tick window whose span matches the clip length. `full_source_length_ms`
    /// is needed only for legacy clips whose two offsets are the `-1` sentinel. The right-hand
    /// clip gets a new ID when the record layout contains one and is inserted after the source.
    pub fn split_playlist_audio_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        split_position_ticks: u32,
        full_source_length_ms: Option<f32>,
    ) -> Result<usize, FlpError> {
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
        let PlaylistClipTarget::Channel { id: channel_id } = clip.target() else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Audio Clips can be split",
            ));
        };
        let channel_summaries = self.channels();
        let mut matching_channels = channel_summaries
            .iter()
            .filter(|channel| channel.id() == channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip channel does not exist",
            ));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clip does not target one unambiguous Audio Clip channel",
            ));
        }
        let relative_split_ticks = split_position_ticks.checked_sub(clip.position_ticks);
        let Some(relative_split_ticks) =
            relative_split_ticks.filter(|ticks| *ticks > 0 && *ticks < clip.length_ticks)
        else {
            return Err(FlpError::UnsupportedEdit(
                "the split point must fall inside the Playlist Audio Clip",
            ));
        };
        let tempo_channel_ids = channel_summaries
            .iter()
            .filter(|channel| {
                channel.kind() == Some(5)
                    && channel
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks <= split_position_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "splitting an Audio Clip across Playlist tempo automation is unsupported",
            ));
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || scale <= 0.0 || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clips with a non-default or invalid scale cannot be split",
            ));
        }

        let record_size = clip.record_size;
        let (source_start_bytes, split_source_bytes, source_end_bytes) = if record_size == 88 {
            let Some((source_start_tick, source_end_tick)) = clip.source_window_ticks() else {
                return Err(FlpError::UnsupportedEdit(
                    "the Audio Clip has no decoded source tick window",
                ));
            };
            if source_end_tick <= source_start_tick
                || source_end_tick.checked_sub(source_start_tick) != Some(clip.length_ticks)
            {
                return Err(FlpError::UnsupportedEdit(
                    "splitting an 88-byte Audio Clip requires a source tick window matching its timeline length",
                ));
            }
            let split_source_tick = source_start_tick
                .checked_add(relative_split_ticks)
                .ok_or(FlpError::LengthOverflow)?;
            (
                source_start_tick.to_le_bytes(),
                split_source_tick.to_le_bytes(),
                source_end_tick.to_le_bytes(),
            )
        } else {
            let (source_start_ms, source_end_ms) =
                if clip.start_offset == -1.0 && clip.end_offset == -1.0 {
                    let Some(source_length_ms) = full_source_length_ms else {
                        return Err(FlpError::UnsupportedEdit(
                            "splitting a full-source Audio Clip requires its sample length",
                        ));
                    };
                    if !source_length_ms.is_finite() || source_length_ms <= 0.0 {
                        return Err(FlpError::UnsupportedEdit(
                            "the full sample length must be a positive number of milliseconds",
                        ));
                    }
                    (0.0, source_length_ms)
                } else if clip.start_offset.is_finite()
                    && clip.end_offset.is_finite()
                    && clip.start_offset >= 0.0
                    && clip.end_offset > clip.start_offset
                {
                    (clip.start_offset, clip.end_offset)
                } else {
                    return Err(FlpError::UnsupportedEdit(
                        "the Audio Clip has an unsupported source window",
                    ));
                };

            let ppq = f64::from(self.header.ppq);
            let tempo_bpm = self.metadata.tempo_bpm().unwrap_or(140.0);
            if ppq == 0.0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 {
                return Err(FlpError::UnsupportedEdit(
                    "splitting an Audio Clip requires a valid project tempo and PPQ",
                ));
            }
            let elapsed_source_ms = f64::from(relative_split_ticks) * 60_000.0 / (ppq * tempo_bpm);
            let split_source_ms = (f64::from(source_start_ms) + elapsed_source_ms) as f32;
            if !split_source_ms.is_finite()
                || split_source_ms <= source_start_ms
                || split_source_ms >= source_end_ms
            {
                return Err(FlpError::UnsupportedEdit(
                    "the split point must fall inside the Audio Clip's available source audio",
                ));
            }
            (
                source_start_ms.to_le_bytes(),
                split_source_ms.to_le_bytes(),
                source_end_ms.to_le_bytes(),
            )
        };

        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip record does not fit its event payload",
            ));
        }

        let left_length = relative_split_ticks;
        let right_position = split_position_ticks;
        let right_length = clip.length_ticks - relative_split_ticks;
        let mut left_record = event.payload[record_start..record_end].to_vec();
        let mut right_record = left_record.clone();
        left_record[8..12].copy_from_slice(&left_length.to_le_bytes());
        left_record[24..28].copy_from_slice(&source_start_bytes);
        left_record[28..32].copy_from_slice(&split_source_bytes);
        right_record[0..4].copy_from_slice(&right_position.to_le_bytes());
        right_record[8..12].copy_from_slice(&right_length.to_le_bytes());
        right_record[24..28].copy_from_slice(&split_source_bytes);
        right_record[28..32].copy_from_slice(&source_end_bytes);
        if playlist_record_has_clip_id(record_size) {
            right_record[32..36]
                .copy_from_slice(&next_playlist_clip_id(arrangement, None)?.to_le_bytes());
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload.splice(record_start..record_end, left_record);
        let right_record_start = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        payload.splice(right_record_start..right_record_start, right_record);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;
        Ok(clip_index.saturating_add(1))
    }

    /// Joins two contiguous Audio Clips that play adjacent source windows.
    /// Legacy records require millisecond windows; 88-byte records require explicit tick windows
    /// whose spans match the timeline lengths. The left record and ID are kept; other opaque
    /// fields must match byte-for-byte.
    pub fn join_adjacent_playlist_audio_clips(
        &mut self,
        arrangement_id: u16,
        left_clip_index: usize,
        right_clip_index: usize,
    ) -> Result<usize, FlpError> {
        if left_clip_index == right_clip_index {
            return Err(FlpError::UnsupportedEdit(
                "joining requires two different Playlist Audio Clips",
            ));
        }
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
        let Some(left_clip) = arrangement.clips.get(left_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the left Playlist Audio Clip does not exist",
            ));
        };
        let Some(right_clip) = arrangement.clips.get(right_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the right Playlist Audio Clip does not exist",
            ));
        };
        let (
            PlaylistClipTarget::Channel {
                id: left_channel_id,
            },
            PlaylistClipTarget::Channel {
                id: right_channel_id,
            },
        ) = (left_clip.target(), right_clip.target())
        else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Audio Clips can be joined",
            ));
        };
        if left_channel_id != right_channel_id {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must use the same sample channel",
            ));
        }
        let channels = self.channels();
        let mut matching_channels = channels
            .iter()
            .filter(|channel| channel.id() == left_channel_id);
        let Some(channel) = matching_channels.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip channel does not exist",
            ));
        };
        if matching_channels.next().is_some() || channel.kind() != Some(4) {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist clips do not target one unambiguous Audio Clip channel",
            ));
        }
        if left_clip.raw_track_index != right_clip.raw_track_index {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must be on the same Playlist track",
            ));
        }
        if left_clip.record_size != right_clip.record_size
            || left_clip.source_event_index != right_clip.source_event_index
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must use the same Playlist record event and layout",
            ));
        }
        let record_size = left_clip.record_size;
        if left_clip.pattern_base != right_clip.pattern_base
            || left_clip.item_index != right_clip.item_index
            || left_clip.track_index != right_clip.track_index
            || left_clip.group != right_clip.group
            || left_clip.unknown_word != right_clip.unknown_word
            || left_clip.item_flags != right_clip.item_flags
            || left_clip.header_bytes != right_clip.header_bytes
            || left_clip.scale != right_clip.scale
            || left_clip.trailing_bytes != right_clip.trailing_bytes
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have matching clip properties",
            ));
        }
        if !left_clip.has_opaque_record_tail() && left_clip.reserved != right_clip.reserved {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have matching clip properties",
            ));
        }
        let default_scale = |clip: &PlaylistClip| {
            clip.scale
                .is_none_or(|scale| scale.is_finite() && (scale - 1.0).abs() <= 1e-9)
        };
        if !default_scale(left_clip) || !default_scale(right_clip) {
            return Err(FlpError::UnsupportedEdit(
                "Playlist Audio Clips with a non-default or invalid scale cannot be joined",
            ));
        }
        let left_end_ticks = left_clip
            .position_ticks
            .checked_add(left_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        if left_clip.length_ticks == 0 || right_clip.length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must have nonzero timeline lengths",
            ));
        }
        if left_end_ticks != right_clip.position_ticks {
            return Err(FlpError::UnsupportedEdit(
                "joined Audio Clips must touch on the Playlist timeline",
            ));
        }
        let modern_source_end_tick = if record_size == 88 {
            let (
                Some((left_source_start, left_source_end)),
                Some((right_source_start, right_source_end)),
            ) = (
                left_clip.source_window_ticks(),
                right_clip.source_window_ticks(),
            )
            else {
                return Err(FlpError::UnsupportedEdit(
                    "joined 88-byte Audio Clips need decoded source tick windows",
                ));
            };
            if left_source_end.checked_sub(left_source_start) != Some(left_clip.length_ticks)
                || right_source_end.checked_sub(right_source_start) != Some(right_clip.length_ticks)
                || left_source_end != right_source_start
            {
                return Err(FlpError::UnsupportedEdit(
                    "joined 88-byte Audio Clips need matching timeline and contiguous source tick windows",
                ));
            }
            Some(right_source_end)
        } else {
            if !left_clip.start_offset.is_finite()
                || !left_clip.end_offset.is_finite()
                || !right_clip.start_offset.is_finite()
                || !right_clip.end_offset.is_finite()
                || left_clip.start_offset < 0.0
                || right_clip.start_offset < 0.0
                || left_clip.end_offset <= left_clip.start_offset
                || right_clip.end_offset <= right_clip.start_offset
                || left_clip.end_offset.to_bits() != right_clip.start_offset.to_bits()
            {
                return Err(FlpError::UnsupportedEdit(
                    "joined Audio Clips must have contiguous, supported sample windows",
                ));
            }
            None
        };
        let merged_end_ticks = right_clip
            .position_ticks
            .checked_add(right_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let merged_length_ticks = merged_end_ticks
            .checked_sub(left_clip.position_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let tempo_channel_ids = channels
            .iter()
            .filter(|candidate| {
                candidate.kind() == Some(5)
                    && candidate
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < merged_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > left_clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining Audio Clips across Playlist tempo automation is unsupported",
            ));
        }

        let left_record_start = left_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let left_record_end = left_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_start = right_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_end = right_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = left_clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || left_record_end > event.payload.len()
            || right_record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip records do not fit their event payload",
            ));
        }
        if left_record_start == right_record_start {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Audio Clip records are not distinct",
            ));
        }
        if record_size == 88 {
            let mut left_comparison = event.payload[left_record_start..left_record_end].to_vec();
            let mut right_comparison = event.payload[right_record_start..right_record_end].to_vec();
            for range in [0..4, 8..12, 24..32] {
                left_comparison[range.clone()].fill(0);
                right_comparison[range].fill(0);
            }
            if left_comparison != right_comparison {
                return Err(FlpError::UnsupportedEdit(
                    "joined 88-byte Audio Clips must match outside position, length, and source windows",
                ));
            }
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload[left_record_start + 8..left_record_start + 12]
            .copy_from_slice(&merged_length_ticks.to_le_bytes());
        if let Some(source_end_tick) = modern_source_end_tick {
            payload[left_record_start + 28..left_record_start + 32]
                .copy_from_slice(&source_end_tick.to_le_bytes());
        } else {
            payload[left_record_start + 28..left_record_start + 32]
                .copy_from_slice(&right_clip.end_offset.to_le_bytes());
        }
        payload.drain(right_record_start..right_record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;

        Ok(if right_clip_index < left_clip_index {
            left_clip_index - 1
        } else {
            left_clip_index
        })
    }

    /// Joins adjacent Pattern Clips that repeat the same pattern without changing its playback.
    /// The left record is kept and extended; the right record is removed. The left clip length
    /// must end on a pattern repeat boundary, and notes may not cross that boundary. Clip IDs
    /// may differ and the left clip's ID is kept.
    pub fn join_adjacent_playlist_pattern_clips(
        &mut self,
        arrangement_id: u16,
        left_clip_index: usize,
        right_clip_index: usize,
    ) -> Result<usize, FlpError> {
        if left_clip_index == right_clip_index {
            return Err(FlpError::UnsupportedEdit(
                "joining requires two different Playlist Pattern Clips",
            ));
        }
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
        let Some(left_clip) = arrangement.clips.get(left_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the left Playlist Pattern Clip does not exist",
            ));
        };
        let Some(right_clip) = arrangement.clips.get(right_clip_index) else {
            return Err(FlpError::UnsupportedEdit(
                "the right Playlist Pattern Clip does not exist",
            ));
        };
        let (
            PlaylistClipTarget::Pattern {
                id: left_pattern_id,
            },
            PlaylistClipTarget::Pattern {
                id: right_pattern_id,
            },
        ) = (left_clip.target(), right_clip.target())
        else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Pattern Clips can be joined",
            ));
        };
        if left_pattern_id != right_pattern_id {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must reference the same pattern",
            ));
        }
        if left_clip.raw_track_index != right_clip.raw_track_index {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must be on the same Playlist track",
            ));
        }
        if left_clip.record_size != right_clip.record_size
            || left_clip.source_event_index != right_clip.source_event_index
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must use the same Playlist record event and layout",
            ));
        }
        if left_clip.has_opaque_record_tail() || right_clip.has_opaque_record_tail() {
            return Err(FlpError::UnsupportedEdit(
                "joining this Playlist record version requires decoding its opaque tail",
            ));
        }
        if left_clip.pattern_base != right_clip.pattern_base
            || left_clip.item_index != right_clip.item_index
            || left_clip.track_index != right_clip.track_index
            || left_clip.group != right_clip.group
            || left_clip.unknown_word != right_clip.unknown_word
            || left_clip.item_flags != right_clip.item_flags
            || left_clip.header_bytes != right_clip.header_bytes
            || left_clip.start_offset.to_bits() != right_clip.start_offset.to_bits()
            || left_clip.end_offset.to_bits() != right_clip.end_offset.to_bits()
            || left_clip.reserved != right_clip.reserved
            || left_clip.scale.map(f64::to_bits) != right_clip.scale.map(f64::to_bits)
            || left_clip.trailing_bytes != right_clip.trailing_bytes
        {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must have matching clip properties",
            ));
        }
        if left_clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Pattern Clips with a non-default or invalid scale cannot be joined",
            ));
        }
        if left_clip.length_ticks == 0 || right_clip.length_ticks == 0 {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must have nonzero timeline lengths",
            ));
        }
        let left_end_ticks = left_clip
            .position_ticks
            .checked_add(left_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        if left_end_ticks != right_clip.position_ticks {
            return Err(FlpError::UnsupportedEdit(
                "joined Pattern Clips must touch on the Playlist timeline",
            ));
        }
        let merged_end_ticks = right_clip
            .position_ticks
            .checked_add(right_clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let merged_length_ticks = merged_end_ticks
            .checked_sub(left_clip.position_ticks)
            .ok_or(FlpError::LengthOverflow)?;

        let pattern = self
            .patterns()?
            .into_iter()
            .find(|pattern| pattern.id == left_pattern_id)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip pattern does not exist",
            ))?;
        let inferred_length = pattern
            .notes
            .iter()
            .filter(|note| note.length > 0)
            .map(|note| u64::from(note.position) + u64::from(note.length))
            .max()
            .unwrap_or(0);
        let repeat_length = pattern
            .length_ticks
            .map(u64::from)
            .filter(|length| *length > 0)
            .or_else(|| (inferred_length > 0).then_some(inferred_length))
            .ok_or(FlpError::UnsupportedEdit(
                "joining Pattern Clips requires a known nonzero pattern repeat length",
            ))?;
        if !u64::from(left_clip.length_ticks).is_multiple_of(repeat_length) {
            return Err(FlpError::UnsupportedEdit(
                "the left Pattern Clip must end on a pattern repeat boundary",
            ));
        }
        if pattern.notes.iter().any(|note| {
            u64::from(note.position) >= repeat_length
                || (note.length > 0
                    && u64::from(note.position) % repeat_length + u64::from(note.length)
                        > repeat_length)
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining these Pattern Clips would let a note cross the clip boundary",
            ));
        }

        let channels = self.channels();
        let tempo_channel_ids = channels
            .iter()
            .filter(|candidate| {
                candidate.kind() == Some(5)
                    && candidate
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < merged_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > left_clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "joining Pattern Clips across Playlist tempo automation is unsupported",
            ));
        }

        let record_size = left_clip.record_size;
        let left_record_start = left_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let left_record_end = left_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_start = right_clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let right_record_end = right_record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = left_clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || left_record_end > event.payload.len()
            || right_record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip records do not fit their event payload",
            ));
        }
        if left_record_start == right_record_start {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip records are not distinct",
            ));
        }

        let mut candidate = self.clone();
        let mut payload = event.payload.clone();
        payload[left_record_start + 8..left_record_start + 12]
            .copy_from_slice(&merged_length_ticks.to_le_bytes());
        payload.drain(right_record_start..right_record_end);
        candidate.events[event_index].replace_data_payload(payload)?;
        candidate.refresh_event_offsets()?;
        *self = candidate;

        Ok(if right_clip_index < left_clip_index {
            left_clip_index - 1
        } else {
            left_clip_index
        })
    }

    /// Splits a Pattern Clip at a pattern repeat boundary without changing playback.
    /// The existing clip is shortened and a copy of its record is inserted after it.
    /// Full-window 88-byte Pattern Clips are supported; non-default scale, notes that
    /// cross a repeat boundary, and Playlist tempo automation within the clip are rejected.
    pub fn split_playlist_pattern_clip(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
        split_position_ticks: u32,
    ) -> Result<usize, FlpError> {
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
                "the requested Playlist clip does not exist",
            ));
        };
        let PlaylistClipTarget::Pattern { id: pattern_id } = clip.target() else {
            return Err(FlpError::UnsupportedEdit(
                "only Playlist Pattern Clips can be split",
            ));
        };
        if !clip.can_resize_timeline_length() {
            return Err(FlpError::UnsupportedEdit(
                "this Pattern Clip does not have a recognized full source window for splitting",
            ));
        }
        if clip
            .scale
            .is_some_and(|scale| !scale.is_finite() || (scale - 1.0).abs() > 1e-9)
        {
            return Err(FlpError::UnsupportedEdit(
                "Pattern Clips with a non-default or invalid scale cannot be split",
            ));
        }
        let relative_split_ticks = split_position_ticks.checked_sub(clip.position_ticks);
        let Some(relative_split_ticks) =
            relative_split_ticks.filter(|ticks| *ticks > 0 && *ticks < clip.length_ticks)
        else {
            return Err(FlpError::UnsupportedEdit(
                "the split point must fall inside the Playlist Pattern Clip",
            ));
        };

        let mut matching_patterns = self
            .patterns()?
            .into_iter()
            .filter(|pattern| pattern.id == pattern_id);
        let Some(pattern) = matching_patterns.next() else {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip pattern does not exist",
            ));
        };
        if matching_patterns.next().is_some() {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip pattern is ambiguous",
            ));
        }
        let inferred_length = pattern
            .notes
            .iter()
            .filter(|note| note.length > 0)
            .map(|note| u64::from(note.position) + u64::from(note.length))
            .max()
            .unwrap_or(0);
        let repeat_length = pattern
            .length_ticks
            .map(u64::from)
            .filter(|length| *length > 0)
            .or_else(|| (inferred_length > 0).then_some(inferred_length))
            .ok_or(FlpError::UnsupportedEdit(
                "splitting Pattern Clips requires a known nonzero pattern repeat length",
            ))?;
        if !u64::from(relative_split_ticks).is_multiple_of(repeat_length) {
            return Err(FlpError::UnsupportedEdit(
                "the split point must fall on a pattern repeat boundary",
            ));
        }
        if pattern.notes.iter().any(|note| {
            u64::from(note.position) >= repeat_length
                || (note.length > 0
                    && u64::from(note.position) % repeat_length + u64::from(note.length)
                        > repeat_length)
        }) {
            return Err(FlpError::UnsupportedEdit(
                "splitting this Pattern Clip would let a note cross a repeat boundary",
            ));
        }

        let clip_end_ticks = clip
            .position_ticks
            .checked_add(clip.length_ticks)
            .ok_or(FlpError::LengthOverflow)?;
        let channels = self.channels();
        let tempo_channel_ids = channels
            .iter()
            .filter(|channel| {
                channel.kind() == Some(5)
                    && channel
                        .display_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("TEMPO"))
            })
            .map(ChannelSummary::id)
            .collect::<HashSet<_>>();
        if arrangement.clips.iter().any(|candidate| {
            let PlaylistClipTarget::Channel { id } = candidate.target() else {
                return false;
            };
            tempo_channel_ids.contains(&id)
                && candidate.position_ticks < clip_end_ticks
                && candidate
                    .position_ticks
                    .saturating_add(candidate.length_ticks)
                    > clip.position_ticks
        }) {
            return Err(FlpError::UnsupportedEdit(
                "splitting Pattern Clips across Playlist tempo automation is unsupported",
            ));
        }

        let record_size = clip.record_size;
        let record_start = clip
            .source_record_index
            .checked_mul(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let record_end = record_start
            .checked_add(record_size)
            .ok_or(FlpError::LengthOverflow)?;
        let event_index = clip.source_event_index;
        let event = self
            .events
            .get(event_index)
            .ok_or(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip event no longer exists",
            ))?;
        if event.opcode != 0xE9
            || !matches!(event.encoding, PayloadEncoding::Data { .. })
            || record_end > event.payload.len()
            || !event.payload.len().is_multiple_of(record_size)
        {
            return Err(FlpError::UnsupportedEdit(
                "the Playlist Pattern Clip record does not fit its event payload",
            ));
        }

        let left_length = relative_split_ticks;
        let right_length = clip.length_ticks - relative_split_ticks;
        let mut left_record = event.payload[record_start..record_end].to_vec();
        let mut right_record = left_record.clone();
        left_record[8..12].copy_from_slice(&left_length.to_le_bytes());
        right_record[0..4].copy_from_slice(&split_position_ticks.to_le_bytes());
        right_record[8..12].copy_from_slice(&right_length.to_le_bytes());
        if record_size == 88 {
            left_record[28..32].copy_from_slice(&left_length.to_le_bytes());
  ë¾¸ç¦òµë(š+myÖÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2Â2ÂÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—5÷–ÆöB“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂe³„ÒÂe³„#%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBæFVÆWFU÷Æ–Æ—7Eö6Æ—ƒ2Â"’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢Fö7VÖVç@¢æFVÆWFU÷Æ–Æ—7Eö6Æ—ƒ2Â¢æW‡V7B‚'F†Rf—'7B6Æ—6†÷VÆB&R&VÖ÷f&ÆR"“°¢ÆWB'&ævVÖVçG2ÒFö7VÖVç@¢æ'&ævVÖVçG2‚¢æW‡V7B‚'F†RÆ–Æ—7B6†÷VÆBFV6öFRgFW"FVÆWF–öâ"“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â“°¢76W'EöW€¢'&ævVÖVçG5³Òæ6Æ—5³ÒçF&vWB‚’À¢Æ–Æ—7D6Æ—F&vWC£¥GFW&â²–C¢‚Ð¢“°¢ÆWB6Æ—öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢76W'EöW†6Æ—öWfVçBç–ÆöB‚’Â6V6öæB“° ¢Fö7VÖVç@¢æFVÆWFU÷Æ–Æ—7Eö6Æ—ƒ2Â¢æW‡V7B‚'F†Rf–æÂ6Æ—6†÷VÆB&R&VÖ÷f&ÆR"“°¢76W'B†Fö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚•³Òæ6Æ—2æ—5öV×G’‚’“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†RV×G’Æ–Æ—7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâ7Æ—G5÷Vç7G&WF6†VE÷Æ–Æ—7EöVF–õö6Æ—5öæE÷&W6W'fW5÷F†U÷6÷W&6U÷v–æF÷r‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³3"âã3eÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cóãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB&–v‡Eö6Æ—ÒFö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂæöæR¢æW‡V7B‚&7Æ—B–ç6–FRF†Rf–Æ&ÆR6÷W&6RVF–ò6†÷VÆB7V66VVB"“°¢76W'EöW‡&–v‡Eö6Æ—Â“° ¢ÆWB6Æ—2ÒfFö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚•³Òæ6Æ—3°¢76W'EöW†6Æ—2æÆVâ‚’Â"“°¢76W'EöW†6Æ—5³Òç÷6—F–öå÷F–6·2Â“°¢76W'EöW†6Æ—5³ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW†6Æ—5³Òç÷6—F–öå÷F–6·2Â“b“°¢76W'EöW†6Æ—5³ÒæÆVæwF…÷F–6·2ÂCB“°¢76W'EöW†6Æ—5³Òæ6Æ—ö–BÂ6öÖRƒ’“°¢76W'EöW†6Æ—5³Òæ6Æ—ö–BÂ6öÖRƒ"’“°¢76W'EöW†6Æ—5³Òç7F'Eööfg6WBÂã“°¢76W'EöW†6Æ—5³ÒæVæEööfg6WBÂóã“°¢76W'B‚†6Æ—5³ÒæVæEööfg6WBÒC#‚ãSsCB’æ'2‚’Âã“°¢76W'EöW†6Æ—5³Òç7F'Eööfg6WBÂ6Æ—5³ÒæVæEööfg6WB“° ¢ÆWB×WBW‡V7FVEöÆVgBÒ6Æ—°¢W‡V7FVEöÆVgE³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVEöÆVgE³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚f6Æ—5³ÒæVæEööfg6WBçFõöÆUö'—FW2‚’“°¢ÆWB×WBW‡V7FVE÷&–v‡BÒ6Æ—°¢W‡V7FVE÷&–v‡E³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&–v‡E³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cCGS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&–v‡E³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚f6Æ—5³ÒæVæEööfg6WBçFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&–v‡E³3"âã3eÒæ6÷•ög&öÕ÷6Æ–6R‚c'S3"çFõöÆUö'—FW2‚’“°¢ÆWB6Æ—öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7B6Æ—WfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢ÆWB×WBW‡V7FVE÷–ÆöBÒW‡V7FVEöÆVgBçFõ÷fV2‚“°¢W‡V7FVE÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fW‡V7FVE÷&–v‡B“°¢76W'EöW†6Æ—öWfVçBç–ÆöB‚’ÂW‡V7FVE÷–ÆöB“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R7Æ—B&ö¦V7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R7Æ—B&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâ¦ö–ç5öF¦6VçE÷Æ–Æ—7EöVF–õö6Æ—5ö&6µ÷Fõ÷F†V—%ö÷&–v–æÅ÷&V6÷&B‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cóãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂæöæR¢æW‡V7B‚'F†R6×ÆR6†÷VÆB7Æ—B–çFòGvò6öçF–wV÷W26Æ—2"“°¢ÆWB¦ö–æVEö–æFW‚ÒFö7VÖVç@¢æ¦ö–åöF¦6VçE÷Æ–Æ—7EöVF–õö6Æ—2ƒ2ÂÂ¢æW‡V7B‚'F†R6öçF–wV÷W2VF–ò6VvÖVçG26†÷VÆB¦ö–â"“° ¢76W'EöW†¦ö–æVEö–æFW‚Â“°¢ÆWB'&ævVÖVçG2ÒFö7VÖVç@¢æ'&ævVÖVçG2‚¢æW‡V7B‚'F†R¦ö–æVB'&ævVÖVçB6†÷VÆBFV6öFR"“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â#C“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òç7F'Eööfg6WBÂã“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæVæEööfg6WBÂóã“°¢ÆWB6Æ—öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢76W'EöW†6Æ—öWfVçBç–ÆöB‚’Â6Æ—“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R¦ö–æVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R¦ö–æVB&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâ¦ö–ç5öF¦6VçE÷Æ–Æ—7E÷GFW&åö6Æ—5÷v†Vå÷&WVEö&÷VæF&–W5öÖF6‚‚’°¢ÆWB×WBFö7VÖVçBÒF¦6VçE÷GFW&åö6Æ—öf—‡GW&Rƒ“"Â“bÂÂC‚“°¢ÆWB¦ö–æVEö–æFW‚ÒFö7VÖVç@¢æ¦ö–åöF¦6VçE÷Æ–Æ—7E÷GFW&åö6Æ—2ƒ2ÂÂ¢æW‡V7B‚&Æ–væVBGFW&â6Æ—2F†B6†&RGFW&â6†÷VÆB¦ö–â"“° ¢76W'EöW†¦ö–æVEö–æFW‚Â“°¢ÆWB'&ævVÖVçG2ÒFö7VÖVç@¢æ'&ævVÖVçG2‚¢æW‡V7B‚'F†R¦ö–æVB'&ævVÖVçB6†÷VÆBFV6öFR"“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òç÷6—F–öå÷F–6·2Â“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â#ƒ‚“°¢76W'EöW€¢'&ævVÖVçG5³Òæ6Æ—5³ÒçF&vWB‚’À¢Æ–Æ—7D6Æ—F&vWC£¥GFW&â²–C¢rÐ¢“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òæ6Æ—ö–BÂ6öÖRƒ’“° ¢ÆWB6Æ—öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7B6Æ—WfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢76W'EöW†6Æ—öWfVçBç–ÆöB‚’æÆVâ‚’Âƒ“°¢76W'EöW€¢S3#£¦g&öÕöÆUö'—FW2†6Æ—öWfVçBç–ÆöB‚•³‚âã%ÒçG'•ö–çFò‚’çVçw&‚’’À¢#ƒ€¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R¦ö–æVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R¦ö–æVB&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâ7Æ—G5÷Æ–Æ—7E÷GFW&åö6Æ—öE÷&WVEö&÷VæF'•öæE÷&W6W'fW5÷&V6÷&G2‚’°¢ÆWB×WBFö7VÖVçBÒF¦6VçE÷GFW&åö6Æ—öf—‡GW&Rƒ“"Â“bÂÂC‚“°¢ÆWB÷&–v–æÅöÆVgBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&R&W6VçB"¢ç–ÆöB‚•²âãƒÐ¢çFõ÷fV2‚“° ¢ÆWB&–v‡Eö6Æ—ö–æFW‚ÒFö7VÖVç@¢ç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂ“b¢æW‡V7B‚&7Æ—BöâF†RGFW&â&WVB&÷VæF'’6†÷VÆB7V66VVB"“° ¢76W'EöW‡&–v‡Eö6Æ—ö–æFW‚Â“°¢ÆWB'&ævVÖVçG2ÒFö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â2“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òç÷6—F–öå÷F–6·2Â“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òæ6Æ—ö–BÂ6öÖRƒ’“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òç÷6—F–öå÷F–6·2Â“b“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òæ6Æ—ö–BÂ6öÖRƒ2’“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³%Òç÷6—F–öå÷F–6·2Â“"“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³%ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³%Òæ6Æ—ö–BÂ6öÖRƒ"’“° ¢ÆWB6Æ—÷–ÆöBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&VÖ–â&W6VçB"¢ç–ÆöB‚“°¢ÆWB×WBW‡V7FVEöÆVgBÒ÷&–v–æÅöÆVgBæ6ÆöæR‚“°¢W‡V7FVEöÆVgE³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢ÆWB×WBW‡V7FVE÷&–v‡BÒ÷&–v–æÅöÆVgC°¢W‡V7FVE÷&–v‡E³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&–v‡E³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&–v‡E³3"âã3eÒæ6÷•ög&öÕ÷6Æ–6R‚c7S3"çFõöÆUö'—FW2‚’“°¢76W'EöW‚f6Æ—÷–ÆöE²âãƒÒÂW‡V7FVEöÆVgB“°¢76W'EöW‚f6Æ—÷–ÆöE³ƒâãcÒÂW‡V7FVE÷&–v‡B“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R7Æ—B&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâ7Æ—G5ögVÆÅ÷v–æF÷uöÖöFW&å÷GFW&åö6Æ—öæE÷WFFW5ö&÷F…÷6÷W&6U÷v–æF÷w2‚’°¢ÆWB×WBFö7VÖVçBÒÖöFW&å÷GFW&åö6Æ—öf—‡GW&Rƒ“"ÂÂÂC‚“° ¢ÆWB&–v‡Eö6Æ—ö–æFW‚ÒFö7VÖVç@¢ç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂ“b¢æW‡V7B‚&gVÆÂ×v–æF÷rƒ‚Ö'—FRGFW&â6Æ—6†÷VÆB7Æ—B"“° ¢76W'EöW‡&–v‡Eö6Æ—ö–æFW‚Â“°¢ÆWB'&ævVÖVçG2ÒFö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â"“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW€¢'&ævVÖVçG5³Òæ6Æ—5³Òç6÷W&6U÷v–æF÷u÷F–6·2‚’À¢6öÖR‚ƒÂ“b’¢“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³Òç÷6—F–öå÷F–6·2Â“b“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—5³ÒæÆVæwF…÷F–6·2Â“b“°¢76W'EöW€¢'&ævVÖVçG5³Òæ6Æ—5³Òç6÷W&6U÷v–æF÷u÷F–6·2‚’À¢6öÖR‚ƒÂ“b’¢“° ¢ÆWB6Æ—÷–ÆöBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&VÖ–â&W6VçB"¢ç–ÆöB‚“°¢76W'EöW†6Æ—÷–ÆöBæÆVâ‚’Âsb“°¢76W'EöW‚f6Æ—÷–ÆöE³#Bâã3%ÒÂe³ÂÂÂÂ“bÂÂÂÒ“°¢76W'EöW‚f6Æ—÷–ÆöE³ƒ‚²#Bâãƒ‚²3%ÒÂe³ÂÂÂÂ“bÂÂÂÒ“°¢76W'EöW‚f6Æ—÷–ÆöE³3"âãƒ…ÒÂf6Æ—÷–ÆöE³ƒ‚²3"âåÒ“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R7Æ—B&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7E÷GFW&åö6Æ—÷7Æ—E÷&V¦V7G5÷Vç6fUö&÷VæF&–W5öFöÖ–6ÆÇ’‚’°¢ÆWB×WBæöå÷&WVEö&÷VæF'’ÒF¦6VçE÷GFW&åö6Æ—öf—‡GW&Rƒ“"Â“bÂÂC‚“°¢ÆWB&Vf÷&U÷7Æ—BÒæöå÷&WVEö&÷VæF'’æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢æöå÷&WVEö&÷VæF'¢ç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂC‚¢æ—5öW'"‚¢“°¢76W'EöW†æöå÷&WVEö&÷VæF'’æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&U÷7Æ—B“° ¢ÆWB×WB7&÷76–æuöæ÷FRÒF¦6VçE÷GFW&åö6Æ—öf—‡GW&Rƒ“"Â“bÂƒÂ#B“°¢ÆWB&Vf÷&U÷7Æ—BÒ7&÷76–æuöæ÷FRæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B†7&÷76–æuöæ÷FRç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂ“b’æ—5öW'"‚’“°¢76W'EöW†7&÷76–æuöæ÷FRæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&U÷7Æ—B“° ¢ÆWB×WB'F–ÅöÖöFW&å÷v–æF÷rÒÖöFW&å÷GFW&åö6Æ—öf—‡GW&Rƒ“"Â#BÂÂC‚“°¢ÆWB&Vf÷&U÷7Æ—BÒ'F–ÅöÖöFW&å÷v–æF÷ræVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢'F–ÅöÖöFW&å÷v–æF÷p¢ç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂ“b¢æ—5öW'"‚¢“°¢76W'EöW€¢'F–ÅöÖöFW&å÷v–æF÷ræVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢&Vf÷&U÷7Æ—@¢“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7E÷GFW&åö6Æ—÷7Æ—E÷&V¦V7G5÷FV×õöWFöÖF–öåöFöÖ–6ÆÇ’‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂ#EÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„EÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c“gS3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂRÂƒC‚ÂÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â%DTÕò"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÂƒc2Â2ÂÒ“° ¢ÆWB×WBGFW&åö6Æ—Ò³Sƒ²ƒÓ°¢GFW&åö6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cƒSwSbçFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“'S3"çFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³3"âã3eÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢GFW&åö6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBFV×õö6Æ—Ò³Sƒ²ƒÓ°¢FV×õö6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c#S3"çFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#GS3"çFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“‡SbçFõöÆUö'—FW2‚’“°¢ÆWB×WB6Æ—÷–ÆöBÒGFW&åö6Æ—çFõ÷fV2‚“°¢6Æ—÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gFV×õö6Æ—“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—÷–ÆöB“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBç7Æ—E÷Æ–Æ—7E÷GFW&åö6Æ—ƒ2ÂÂ“b’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7E÷GFW&åö6Æ—ö¦ö–å÷&V¦V7G5÷Vç6fUö&÷VæF&–W5öFöÖ–6ÆÇ’‚’°¢f÷"†ÆVgEöÆVæwF‚Âæ÷FU÷÷6—F–öâÂæ÷FUöÆVæwF‚’–â²ƒCBÂÂC‚’Âƒ“"ÂƒÂ#B’Âƒ“"Â“bÂ•Ð¢°¢ÆWB×WBFö7VÖVçBÐ¢F¦6VçE÷GFW&åö6Æ—öf—‡GW&R†ÆVgEöÆVæwF‚Â“bÂæ÷FU÷÷6—F–öâÂæ÷FUöÆVæwF‚“°¢ÆWB&Vf÷&Uö¦ö–âÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R÷&–v–æÂ&ö¦V7B6†÷VÆBVæ6öFR"“° ¢76W'B€¢Fö7VÖVç@¢æ¦ö–åöF¦6VçE÷Æ–Æ—7E÷GFW&åö6Æ—2ƒ2ÂÂ¢æ—5öW'"‚¢“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R&ö¦V7B6†÷VÆB7F–ÆÂVæ6öFRgFW"&V¦V7FVB¦ö–â"’À¢&Vf÷&Uö¦ö–à¢“°¢Ð¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7EöVF–õö6Æ—ö¦ö–å÷&V¦V7G5öF—66öçF–çV÷W5÷6÷W&6U÷v–æF÷w5öFöÖ–6ÆÇ’‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cóãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂæöæR¢æW‡V7B‚'F†R6×ÆR6†÷VÆB7Æ—B–çFòGvò6Æ—2"“°¢Fö7VÖVç@¢æVF—E÷Æ–Æ—7Eö6Æ—€¢2À¢À¢Æ–Æ—7D6Æ—VF—B°¢7F'Eööfg6WC¢6öÖRƒC3ã’À¢âåÆ–Æ—7D6Æ—VF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'F†R&–v‡B6÷W&6Röfg6WB6†÷VÆB&RVF—F&ÆR"“°¢ÆWB&Vf÷&Uö¦ö–âÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R7Æ—B&ö¦V7B6†÷VÆBVæ6öFR"“° ¢76W'B€¢Fö7VÖVç@¢æ¦ö–åöF¦6VçE÷Æ–Æ—7EöVF–õö6Æ—2ƒ2ÂÂ¢æ—5öW'"‚¢“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R&ö¦V7B6†÷VÆB7F–ÆÂVæ6öFRgFW"&V¦V7FVB¦ö–â"’À¢&Vf÷&Uö¦ö–à¢“°¢Ð ¢5·FW7EÐ¢fâ6Æ—5÷Æ–Æ—7EöVF–õö6Æ—÷6÷W&6U÷v—F†÷WEöÖ÷f–æuö—G5÷F–ÖVÆ–æUö&÷VæG2‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c#S3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c3ƒGS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚c“ãc3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂÓãÂóã¢æW‡V7B‚'F†Rv–æF÷r6†÷VÆB6Æ—FòF†R6×ÆR7F'B"“°¢Fö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ#ãÂóã¢æW‡V7B‚'F†Rv–æF÷r6†÷VÆB6Æ—&–v‡Bv†–ÆR7F––ær–âF†R6×ÆR"“° ¢ÆWB6Æ—2ÒfFö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚•³Òæ6Æ—3°¢76W'EöW†6Æ—2æÆVâ‚’Â“°¢76W'EöW†6Æ—5³Òç÷6—F–öå÷F–6·2Â#“°¢76W'EöW†6Æ—5³ÒæÆVæwF…÷F–6·2Â3ƒB“°¢76W'EöW†6Æ—5³Òç7F'Eööfg6WBÂ#ã“°¢76W'EöW†6Æ—5³ÒæVæEööfg6WBÂóã“°¢ÆWB×WBW‡V7FVEö6Æ—Ò6Æ—°¢W‡V7FVEö6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚c#ãc3"çFõöÆUö'—FW2‚’“°¢W‡V7FVEö6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cóãc3"çFõöÆUö'—FW2‚’“°¢ÆWB6Æ—öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æW‡V7B‚'F†RÆ–Æ—7BWfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢76W'EöW†6Æ—öWfVçBç–ÆöB‚’ÂW‡V7FVEö6Æ—“° ¢ÆWB&Vf÷&U÷&V¦V7FVE÷6Æ—ÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R6Æ—VB&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'B€¢Fö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂãÂóã¢æ—5öW'"‚¢“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R&ö¦V7B6†÷VÆBVæ6öFRgFW"&V¦V7FVB6Æ—"’À¢&Vf÷&U÷&V¦V7FVE÷6Æ— ¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R6Æ—VB&ö¦V7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R6Æ—VB&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7EöVF–õö6Æ—÷6Æ—÷&V¦V7G5ögVÆÅ÷6÷W&6U÷v—F†÷WE÷&ööÕöFöÖ–6ÆÇ’‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c3ƒGS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Óãc3"’çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Óãc3"’çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Fö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂãÂóã¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7EöVF–õö6Æ—÷7Æ—E÷&V¦V7G5÷Vç7W÷'FVEöVF—G5öFöÖ–6ÆÇ’‚’°¢ÆWB×WB6Æ—Ò³„S²ƒÓ°¢6Æ—³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Óãc3"’çFõöÆUö'—FW2‚’“°¢6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Óãc3"’çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂæöæR’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢ÆWB×WBgVÆÅ÷6÷W&6UöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†RgVÆÂ×6÷W&6R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢gVÆÅ÷6÷W&6UöFö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂ6öÖRƒóã’¢æW‡V7B‚&7Æ—B6†÷VÆB7V66VVBv†VâF†R6×ÆRGW&F–öâ—27WÆ–VB"“°¢ÆWBgVÆÅ÷6÷W&6Uö6Æ—2ÒfgVÆÅ÷6÷W&6UöFö7VÖVçBæ'&ævVÖVçG2‚’çVçw&‚•³Òæ6Æ—3°¢76W'EöW†gVÆÅ÷6÷W&6Uö6Æ—5³Òç7F'Eööfg6WBÂã“°¢76W'B‚†gVÆÅ÷6÷W&6Uö6Æ—5³ÒæVæEööfg6WBÒC#‚ãSsCB’æ'2‚’Âã“°¢76W'EöW€¢gVÆÅ÷6÷W&6Uö6Æ—5³Òç7F'Eööfg6WBÀ¢gVÆÅ÷6÷W&6Uö6Æ—5³ÒæVæEööfg6W@¢“°¢76W'EöW†gVÆÅ÷6÷W&6Uö6Æ—5³ÒæVæEööfg6WBÂóã“°¢76W'B€¢Fö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂ6öÖRƒ#ã’¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢76W'B€¢Fö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ#CÂ6öÖRƒóã’¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãVccBçFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÂƒc"ÂÂÂƒc2Â2ÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWB66ÆVEö–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WB66ÆVEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g66ÆVEö–çWB’æW‡V7B‚'F†R66ÆVB&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢66ÆVEöFö7VÖVç@¢ç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂ6öÖRƒóã’¢æ—5öW'"‚¢“°¢76W'EöW‡66ÆVEöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â66ÆVEö–çWB“°¢76W'B€¢66ÆVEöFö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂãÂóã¢æ—5öW'"‚¢“°¢76W'EöW‡66ÆVEöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â66ÆVEö–çWB“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7EöVF–õö6Æ—÷7Æ—E÷&V¦V7G5÷FV×õöWFöÖF–öåöFöÖ–6ÆÇ’‚’°¢ÆWB×WBVF–õö6Æ—Ò³„S²ƒÓ°¢VF–õö6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c—SbçFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢VF–õö6Æ—³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cóãc3"çFõöÆUö'—FW2‚’“°¢VF–õö6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“° ¢ÆWB×WBFV×õö6Æ—Ò³„S²ƒÓ°¢FV×õö6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#CS3"çFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“‡SbçFõöÆUö'—FW2‚’“°¢FV×õö6Æ—³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢ÆWB×WB6Æ—5÷–ÆöBÒVF–õö6Æ—çFõ÷fV2‚“°¢6Æ—5÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gFV×õö6Æ—“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂ"##bããÃ"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ’ÂÂƒRÂBÂƒC‚Â’ÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂRÂƒC‚ÂÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â%DTÕò"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÂƒc2Â2ÂÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—5÷–ÆöB“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBç7Æ—E÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂ“bÂæöæR’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢76W'B€¢Fö7VÖVç@¢ç6Æ—÷Æ–Æ—7EöVF–õö6Æ—ƒ2ÂÂãÂóã¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7Eö6Æ—ö6Æ—&ö&E÷7W'f—fW5ö7WEöæE÷7FW5ö–çFõöåöV×G•ö'&ævVÖVçB‚’°¢ÆWB×WB6÷W&6U÷&V6÷&BÒ³„S²ƒÓ°¢6÷W&6U÷&V6÷&E³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚có“#S3"çFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cƒSwSbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“cS3"çFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6÷W&6U÷&V6÷&E³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂ"##bããÃ"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2Â2ÂÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âg6÷W&6U÷&V6÷&B“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÂƒc2ÂBÂÂƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„Â„$%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'F†R&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6Æ—&ö&BÒFö7VÖVç@¢æ6÷•÷Æ–Æ—7Eö6Æ—ƒ2Â¢æW‡V7B‚'F†Rf—'7B6Æ—6†÷VÆB6÷’Æ÷76ÆW76Ç’"“°¢76W'EöW†6Æ—&ö&Bç&u÷&V6÷&BÂ6÷W&6U÷&V6÷&B“°¢Fö7VÖVç@¢æFVÆWFU÷Æ–Æ—7Eö6Æ—ƒ2Â¢æW‡V7B‚'F†R6÷W&6R6Æ—6†÷VÆB&R7WBg&öÒ—G2'&ævVÖVçB"“°¢ÆWB–ç6W'FVEö–æFW‚ÒFö7VÖVç@¢ç7FU÷Æ–Æ—7Eö6Æ—ƒBÂf6Æ—&ö&BÂ5óƒCÂC“R¢æW‡V7B‚'F†R6Æ—6†÷VÆB7FR–çFòF†RV×G’'&ævVÖVçB"“° ¢ÆWB'&ævVÖVçG2ÒFö7VÖVç@¢æ'&ævVÖVçG2‚¢æW‡V7B‚&&÷F‚'&ævVÖVçG26†÷VÆBFV6öFRgFW"7FR"“°¢76W'B†'&ævVÖVçG5³Òæ6Æ—2æ—5öV×G’‚’“°¢76W'EöW†–ç6W'FVEö–æFW‚Â“°¢76W'EöW†'&ævVÖVçG5³Òæ6Æ—2æÆVâ‚’Â“°¢ÆWB7FVBÒf'&ævVÖVçG5³Òæ6Æ—5³Ó°¢76W'EöW‡7FVBç÷6—F–öå÷F–6·2Â5óƒC“°¢76W'EöW‡7FVBç&u÷G&6µö–æFW‚ÂC“R“°¢76W'EöW‡7FVBçG&6µö–æFW‚Â6öÖRƒB’“°¢76W'EöW‡7FVBçF&vWB‚’ÂÆ–Æ—7D6Æ—F&vWC£¥GFW&â²–C¢rÒ“°¢76W'EöW€¢7FVBæ6Æ—ö–BÀ¢6öÖR‡S3#£¦g&öÕöÆUö'—FW2€¢6÷W&6U÷&V6÷&E³3"âã3eÒçG'•ö–çFò‚’çVçw&‚¢’¢“° ¢ÆWB×WBW‡V7FVE÷&V6÷&BÒ6÷W&6U÷&V6÷&C°¢W‡V7FVE÷&V6÷&E³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c5óƒCS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&V6÷&E³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“WSbçFõöÆUö'—FW2‚’“°¢ÆWB6Æ—öWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†6Æ—öWfVçG2æÆVâ‚’Â"“°¢76W'B†6Æ—öWfVçG5³Òç–ÆöB‚’æ—5öV×G’‚’“°¢76W'EöW†6Æ—öWfVçG5³Òç–ÆöB‚’ÂW‡V7FVE÷&V6÷&B“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆB'6Rv–â"“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7Eö6Æ—÷7FU÷&V¦V7G5ööF–ffW&VçE÷&V6÷&EöÆ–÷WEöFöÖ–6ÆÇ’‚’°¢ÆWB×WB6÷W&6U÷&V6÷&BÒ³„S²ƒÓ°¢6÷W&6U÷&V6÷&E³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cƒSwSbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“cS3"çFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢6÷W&6U÷&V6÷&E³#Bâã#…Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢6÷W&6U÷&V6÷&E³#‚âã3%Òæ6÷•ög&öÕ÷6Æ–6R‚cãc3"çFõöÆUö'—FW2‚’“°¢ÆWB×WB6÷W&6UöWfVçG2ÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6÷W&6UöWfVçG2Â„3rÂ"##bããÃ"“°¢6÷W&6UöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2Â2ÂÒ“°¢VæEöFFöWfVçB‚f×WB6÷W&6UöWfVçG2Â„S’Âg6÷W&6U÷&V6÷&B“°¢ÆWB6÷W&6RÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚g6÷W&6UöWfVçG2ÂeµÒÂeµÒ’¢æW‡V7B‚'F†R6÷W&6Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6Æ—&ö&BÒ6÷W&6P¢æ6÷•÷Æ–Æ—7Eö6Æ—ƒ2Â¢æW‡V7B‚'F†R6÷W&6R6Æ—6†÷VÆB6÷’"“° ¢ÆWB×WBF&vWE÷&V6÷&BÒ³ƒT²3%Ó°¢F&vWE÷&V6÷&E³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSSbçFõöÆUö'—FW2‚’“°¢F&vWE÷&V6÷&E³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cƒS‡SbçFõöÆUö'—FW2‚’“°¢F&vWE÷&V6÷&E³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c“cS3"çFõöÆUö'—FW2‚’“°¢F&vWE÷&V6÷&E³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢F&vWE÷&V6÷&E³#âã#EÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒCÂƒcBÂƒƒÂƒƒÒ“°¢ÆWB×WBF&vWEöWfVçG2ÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBF&vWEöWfVçG2Â„3rÂ"##ããÃ"“°¢F&vWEöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂBÂÒ“°¢VæEöFFöWfVçB‚f×WBF&vWEöWfVçG2Â„S’ÂgF&vWE÷&V6÷&B“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚gF&vWEöWfVçG2ÂeµÒÂe³„45Ò“°¢ÆWB×WBF&vWBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†RF&vWBf—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢F&vW@¢ç7FU÷Æ–Æ—7Eö6Æ—ƒBÂf6Æ—&ö&BÂó“#ÂC“‚¢æ—5öW'"‚¢“°¢76W'EöW€¢F&vWBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚'F&vWB6†÷VÆBVæ6öFR"’À¢÷&–v–æÂÀ¢&&V¦V7FVB7FR×W7BÆVfRWfW'’&ö¦V7B'—FRVæ6†ævVB ¢“°¢Ð ¢5·FW7EÐ¢fâÆ–Æ—7E÷&V6÷&E÷6—¦Uö¶VW5÷fW'6–öåöfÆÆ&6µöæE÷&V¦V7G5÷vVµöÖ&–wV—G’‚’°¢ÆWBÖ&–wV÷W2Ò³Sƒ²3#Ó°¢76W'EöW€¢7WW#£§Æ–Æ—7Eö6Æ—÷&V6÷&E÷6—¦R…6öÖR‚##ãã"’ÂfÖ&–wV÷W2Â’çVçw&‚’À¢3 ¢“°¢76W'B‡7WW#£§Æ–Æ—7Eö6Æ—÷&V6÷&E÷6—¦R…6öÖR‚##Bã"ã"’ÂfÖ&–wV÷W2Â’æ—5öW'"‚’“°¢76W'B‡7WW#£§Æ–Æ—7Eö6Æ—÷&V6÷&E÷6—¦R„æöæRÂfÖ&–wV÷W2Â’æ—5öW'"‚’“°¢Ð ¢fâÖ–F•öf—‡GW&R‡G&6³¢e·S…ÒÂF—f—6–öã¢Sb’ÓâfV3ÇSƒâ°¢ÆWB×WB'—FW2ÒfV3£¦æWr‚“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕF†B"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cgS3"çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cSbçFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cSbçFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚fF—f—6–öâçFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕG&²"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚b‡G&6²æÆVâ‚’2S3"’çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‡G&6²“°¢'—FW0¢Ð ¢fâÖ–F•÷G&6·5öf—‡GW&R‡G&6·3¢e²e·S…ÕÒÂf÷&ÖC¢SbÂF—f—6–öã¢Sb’ÓâfV3ÇSƒâ°¢ÆWB×WB'—FW2ÒfV3£¦æWr‚“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕF†B"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cgS3"çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚ff÷&ÖBçFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚b‡G&6·2æÆVâ‚’2Sb’çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚fF—f—6–öâçFõö&Uö'—FW2‚’“°¢f÷"G&6²–âG&6·2°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕG&²"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚b‡G&6²æÆVâ‚’2S3"’çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‡G&6²“°¢Ð¢'—FW0¢Ð ¢fâÖ–F•öf—‡GW&UöæÖVE÷G&6·2‡G&6·3¢e²‚g7G"Âe·S…Ò•ÒÂF—f—6–öã¢Sb’ÓâfV3ÇSƒâ°¢ÆWB×WB'—FW2ÒfV3£¦æWr‚“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕF†B"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cgS3"çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚cSbçFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚b‡G&6·2æÆVâ‚’2Sb’çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚fF—f—6–öâçFõö&Uö'—FW2‚’“°¢f÷"†æÖRÂWfVçG2’–âG&6·2°¢ÆWBæÖUö'—FW2ÒæÖRæ5ö'—FW2‚“°¢ÆWB×WBG&6²ÒfV2³ƒÂ„dbÂƒ2ÂæÖUö'—FW2æÆVâ‚’2S…Ó°¢G&6²æW‡FVæEög&öÕ÷6Æ–6R†æÖUö'—FW2“°¢G&6²æW‡FVæEög&öÕ÷6Æ–6R†WfVçG2“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R†"$ÕG&²"“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚b‡G&6²æÆVâ‚’2S3"’çFõö&Uö'—FW2‚’“°¢'—FW2æW‡FVæEög&öÕ÷6Æ–6R‚gG&6²“°¢Ð¢'—FW0¢Ð ¢5·FW7EÐ¢fâFV6öFW5öö'6W'fVEöÖ—†W%ö–ç6W'Eöf–VÆG5÷v—F†÷WE÷&Ww&—F–æu÷6÷W&6UöWfVçG2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢f÷"†–çWBÂ÷WGWBÂ6öÆ÷"Â–6öâÂæÖR’–â°¢‚Óö“3"Âö“3"Âƒ5óc„7S3"ÂsUö“bÂ$´”4²"’À¢‚Óö“3"ÂÓö“3"ÂƒEó#s“'S3"Âseö“bÂ%DôÕ2"’À¢Ò°¢WfVçE÷7G&VÒçW6‚ƒƒ”“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f–çWBçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f÷WGWBçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f6öÆ÷"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒTb“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f–6öâçFõöÆUö'—FW2‚’“°¢ÆWBæÖRÒæÖP¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„42ÂfæÖR“°¢Ð ¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB–ç6W'G2ÒFö7VÖVçBæÖ—†W%ö–ç6W'G2‚“° ¢76W'EöW†–ç6W'G2æÆVâ‚’Â"“°¢76W'EöW†–ç6W'G5³Òæ÷&F–æÂ‚’Â“°¢76W'EöW†–ç6W'G5³ÒææÖR‚’Â6öÖR‚$´”4²"’“°¢76W'EöW†–ç6W'G5³Òæ–çWE÷&r‚’ÂÓ“°¢76W'EöW†–ç6W'G5³Òæ÷WGWE÷&r‚’Â“°¢76W'EöW†–ç6W'G5³Òæ6öÆ÷%÷&r‚’Âƒ5óc„2“°¢76W'EöW†–ç6W'G5³Òæ–6öå÷&r‚’Â6öÖRƒsR’“°¢76W'EöW†–ç6W'G5³ÒæWfVçE÷&ævR‚’ÂâãR“°¢76W'EöW†–ç6W'G5³ÒææÖR‚’Â6öÖR‚%DôÕ2"’“°¢76W'EöW†–ç6W'G5³Òæ÷WGWE÷&r‚’ÂÓ“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&Æ÷76ÆW72Væ6öF–ær6†÷VÆB7V66VVB"’À¢÷&–v–æÀ¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5ö6ö×ÆWFUöÖ—†W%öVffV7E÷6Æ÷Eö&÷VæF&–W5÷v—F†÷WE÷&Ww&—F–æu÷6Æ÷E÷7FFR‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒ”Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c÷S3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒc"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c÷SbçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3’Â"$g'V—G’&ÖWG&–2U%Ã"“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„4"Â"$G'VÒUÃ"“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂe³„Â„#"Â„35Ò“°¢f÷"6Æ÷Eö–æFW‚–â÷Sbâã°¢WfVçE÷7G&VÒçW6‚ƒƒc"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚g6Æ÷Eö–æFW‚çFõöÆUö'—FW2‚’“°¢Ð¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3’Â"&÷WG6–FRF†R–ç6W'EÃ"“° ¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6Æ÷G2ÒFö7VÖVçBæÖ—†W%öVffV7E÷6Æ÷G2‚“° ¢76W'EöW‡6Æ÷G2æÆVâ‚’Â“°¢76W'EöW‡6Æ÷G5³Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6Æ÷G5³Òç6Æ÷Eö–æFW‚‚’Â“°¢76W'EöW‡6Æ÷G5³ÒçÇVv–åö–FVçF–f–W"‚’Â6öÖR‚$g'V—G’&ÖWG&–2U""’“°¢76W'EöW‡6Æ÷G5³ÒæF—7Æ•öæÖR‚’Â6öÖR‚$G'VÒU"’“°¢76W'EöW‡6Æ÷G5³Òç7FFUöWfVçEö6÷VçB‚’Â“°¢76W'EöW‡6Æ÷G5³ÒæWfVçE÷&ævR‚’Â2âãr“°¢76W'EöW‡6Æ÷G5³Òç6Æ÷Eö–æFW‚‚’Â“°¢76W'EöW‡6Æ÷G5³ÒçÇVv–åö–FVçF–f–W"‚’ÂæöæR“°¢76W'EöW‡6Æ÷G5³ÒæF—7Æ•öæÖR‚’ÂæöæR“°¢76W'EöW‡6Æ÷G5³Òç7FFUöWfVçEö6÷VçB‚’Â“°¢76W'EöW‡6Æ÷G5³•Òç6Æ÷Eö–æFW‚‚’Â’“°¢76W'EöW‡6Æ÷G5³•ÒçÇVv–åö–FVçF–f–W"‚’ÂæöæR“°¢76W'EöW‡6Æ÷G5³•ÒæWfVçE÷&ævR‚’Âbâãb“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&Æ÷76ÆW72Væ6öF–ær6†÷VÆB7V66VVB"’À¢÷&–v–æÀ¢“° ¢ÆWB×WB–æ6ö×ÆWFRÒfV2³ƒ”Ó°¢–æ6ö×ÆWFRæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢–æ6ö×ÆWFRçW6‚ƒƒ“2“°¢–æ6ö×ÆWFRæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢–æ6ö×ÆWFRçW6‚ƒƒ“R“°¢–æ6ö×ÆWFRæW‡FVæEög&öÕ÷6Æ–6R‚c÷S3"çFõöÆUö'—FW2‚’“°¢–æ6ö×ÆWFRæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–æ6ö×ÆWFUöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚f–æ6ö×ÆWFRÂeµÒÂeµÒ’’æW‡V7B‚&f—‡GW&R'6W2"“°¢76W'B†–æ6ö×ÆWFUöFö7VÖVçBæÖ—†W%öVffV7E÷6Æ÷G2‚’æ—5öV×G’‚’“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%ö–ç6W'EöfÆw5öFV6öFUöæEöVF—Eö¶æ÷våö&—G5öÆ÷76ÆW76Ç’‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒ”Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c÷S3"çFõöÆUö'—FW2‚’“°¢ÆWBfÆw2ÒƒƒóU÷S3#°¢ÆWB×WBfÆw5÷–ÆöBÒfV2³„Â„$"Â„42Â„DEÓ°¢fÆw5÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚ffÆw2çFõöÆUö'—FW2‚’“°¢fÆw5÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂƒ#"Âƒ32ÂƒCEÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„T2ÂffÆw5÷–ÆöB“° ¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB–ç6W'BÒfFö7VÖVçBæÖ—†W%ö–ç6W'G2‚•³Ó°¢76W'EöW†–ç6W'BæfÆw2‚’Â6öÖR†fÆw2’“°¢76W'EöW†–ç6W'BæVæ&ÆVB‚’Â6öÖR†fÇ6R’“°¢76W'EöW†–ç6W'Bç6öÆöVB‚’Â6öÖR†fÇ6R’“°¢76W'EöW†–ç6W'BçöÆ&—G•÷&WfW'6VB‚’Â6öÖR‡G'VR’“°¢76W'EöW†–ç6W'Bç7vöÆVgE÷&–v‡B‚’Â6öÖR†fÇ6R’“°¢76W'EöW†–ç6W'BæVffV7G5öVæ&ÆVB‚’Â6öÖR‡G'VR’“° ¢ÆWBfÆw5öWfVçEö–æFW‚Ò–ç6W'BæfÆw5öWfVçEö–æFW‚æW‡V7B‚&fÆw2WfVçB—2&V6övæ—¦VB"“°¢ÆWB÷&–v–æÅöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢Fö7VÖVç@¢æVF—EöÖ—†W%ö–ç6W'EöfÆw2€¢À¢Ö—†W$–ç6W'DVF—B°¢Væ&ÆVC¢6öÖR‡G'VR’À¢6öÆöVC¢6öÖR‡G'VR’À¢öÆ&—G•÷&WfW'6VC¢6öÖR†fÇ6R’À¢7vöÆVgE÷&–v‡C¢6öÖR‡G'VR’À¢VffV7G5öVæ&ÆVC¢æöæRÀ¢ÒÀ¢¢æW‡V7B‚&¶æ÷vâÖ—†W"fÆw26†÷VÆB&RVF—F&ÆR"“° ¢ÆWB–ç6W'BÒfFö7VÖVçBæÖ—†W%ö–ç6W'G2‚•³Ó°¢76W'EöW†–ç6W'BæfÆw2‚’Â6öÖRƒƒƒóR’“°¢76W'EöW†–ç6W'BæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†–ç6W'Bç6öÆöVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†–ç6W'BçöÆ&—G•÷&WfW'6VB‚’Â6öÖR†fÇ6R’“°¢76W'EöW†–ç6W'Bç7vöÆVgE÷&–v‡B‚’Â6öÖR‡G'VR’“°¢76W'EöW†–ç6W'BæVffV7G5öVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢ÆWBVF—FVE÷–ÆöBÒFö7VÖVçBæWfVçG2‚•¶fÆw5öWfVçEö–æFW…Òç–ÆöB‚“°¢76W'EöW‚fVF—FVE÷–ÆöE²âãEÒÂe³„Â„$"Â„42Â„DEÒ“°¢76W'EöW‚fVF—FVE÷–ÆöE³Bâã…ÒÂcƒƒóU÷S3"çFõöÆUö'—FW2‚’“°¢76W'EöW‚fVF—FVE÷–ÆöE³‚âåÒÂe³ƒÂƒ#"Âƒ32ÂƒCEÒ“°¢f÷"†–æFW‚ÂWfVçB’–âFö7VÖVçBæWfVçG2‚’æ—FW"‚’æVçVÖW&FR‚’°¢–b–æFW‚ÒfÆw5öWfVçEö–æFW‚°¢76W'EöW†WfVçBçv—&Uö'—FW2‚’Â÷&–v–æÅöWfVçG5¶–æFW…Ò“°¢Ð¢Ð ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVBfÆw26†÷VÆBVæ6öFRÆ÷76ÆW76Ç’"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBæÖ—†W%ö–ç6W'G2‚•³ÒæfÆw2‚’Â6öÖRƒƒƒóR’“° ¢ÆWB×WBÆVv7•÷7G&VÒÒfV2³ƒ”Ó°¢ÆVv7•÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢ÆVv7•÷7G&VÒçW6‚ƒƒ“2“°¢ÆVv7•÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢ÆVv7•÷7G&VÒçW6‚ƒƒ“R“°¢ÆVv7•÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c÷S3"çFõöÆUö'—FW2‚’“°¢ÆWB×WBÆVv7•öfÆw2ÒfV2³²EÓ°¢ÆVv7•öfÆw2æW‡FVæEög&öÕ÷6Æ–6R‚bƒ÷S3"ÃÂ2’çFõöÆUö'—FW2‚’“°¢ÆVv7•öfÆw2æW‡FVæEög&öÕ÷6Æ–6R‚e³²EÒ“°¢VæEöFFöWfVçB‚f×WBÆVv7•÷7G&VÒÂ„D2ÂfÆVv7•öfÆw2“°¢ÆWB×WBÆVv7’ÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fÆVv7•÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'F†RÆVv7’f—‡GW&R6†÷VÆB'6R"“°¢ÆVv7’ç&ö¦V7E÷fW'6–öâÒ6öÖR‚##Bã"ã"çFõö÷væVB‚’“°¢76W'EöW†ÆVv7’æÖ—†W%ö–ç6W'G2‚•³ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷&÷WFUöVF–&–Æ—G•öÆ–W5ö–ç6W'Eö×WFUöæE÷6öÆõö6öç6W'fF—fVÇ’‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÂÂfÆw7ÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢À¢÷WGWE÷&s¢À¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†fÆw2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢°¢–ç6W'BƒÂƒÃÂ2’ÂƒÃÂ"’ÂÂƒÃÂ’’À¢–ç6W'BƒÂƒÃÂ2’ÂƒÃÂ"’ÂÂƒÃÂ’’À¢–ç6W'Bƒ"Â’ÂòòF—6&ÆVB–ç6W'Bà¢ÒÀ¢eµÒÀ¢æöæRÀ¢“° ¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ„æöæR’“°¢76W'B‡&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ"’’“°¢76W'B‡&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖR‚Ó’’“°¢76W'B‡&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒr’’“°¢ÆWB×WB&÷WFVEög&ÖRÒ³ã#RÂÓãUÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WB&÷WFVEög&ÖR“°¢76W'EöW‡&÷WFVEög&ÖRÂ³ãRÂÓã#UÒ“°¢ÆWB×WBÖ7FW%ög&ÖRÒ³ã#RÂÓãUÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ„æöæR¢æÇ•ög&ÖR‚f×WBÖ7FW%ög&ÖR“°¢76W'EöW†Ö7FW%ög&ÖRÂ³ã#RÂÓãUÒ“° ¢ÆWBÖ7FW%÷6öÆòÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'BƒÂƒÃÂ2’ÂƒÃÂ"’’Â–ç6W'BƒÂÃÂ2•ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢76W'B†Ö7FW%÷6öÆòæÆÆ÷w5ö6†ææVÂ„æöæR’“°¢76W'B†Ö7FW%÷6öÆòæÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷6VæEöVF–õ÷F‡5ö6öÖ&–æU÷÷7EöfFW%÷&÷WFW5öæE÷6öÆõö–çWG2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÂÂfÆw7ÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†fÆw2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB6VæBÐ¢Ç6÷W&6U÷G&6³¢W6—¦RÂFW7F–æF–öå÷G&6³¢S‚ÂÆWfVÅ÷&rÂVæ&ÆVGÂÖ—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢6÷W&6U÷G&6²À¢&÷WFU÷&ÖWFW%ö–C¢cB²FW7F–æF–öå÷G&6²À¢fÇVS¢ÆWfVÅ÷&rÀ¢Væ&ÆVC¢6öÖR†Væ&ÆVB’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢Ó°¢ÆWBVæ&ÆVBÒÃÂ3°¢ÆWB×WB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'BƒÂVæ&ÆVB’Â–ç6W'BƒÂVæ&ÆVB’Â–ç6W'Bƒ"ÂVæ&ÆVB•ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…°¢6VæBƒÂÂ%óƒÂG'VR’À¢6VæBƒÂ"ÂeóCÂG'VR’À¢6VæBƒ"ÂÂ%óƒÂG'VR’À¢Ò“° ¢ÆWB×WBg&ÖRÒ³ãÂÓãUÓ°¢ÆWB&÷WFU÷ÆâÒ&÷WF–ærç6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’“°¢&÷WFU÷Æâæ÷WGWE÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãRÂÓãsUÒ“°¢76W'EöW€¢&÷WFU÷Æâæ–ç6W'EöÖWFW%÷G&ç6f÷&×2À¢fV2°¢ƒÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’’À¢ƒ"ÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’ç66ÆVBƒãR’’À¢Ð¢“° ¢ÆWB×WB6öÆõ÷&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢°¢–ç6W'BƒÂVæ&ÆVB’À¢–ç6W'BƒÂVæ&ÆVB’À¢–ç6W'Bƒ"ÂVæ&ÆVBÂƒÃÂ"’’À¢ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢6öÆõ÷&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…·6VæBƒÂ"Â%óƒÂG'VR’Â6VæBƒ"ÂÂ%óƒÂG'VR•Ò“°¢76W'B‡6öÆõ÷&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢ÆWB×WB6öÆõög&ÖRÒ³ã#RÂãUÓ°¢6öÆõ÷&÷WF–æp¢ç6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æ÷WGWE÷G&ç6f÷&Ð¢æÇ•ög&ÖR‚f×WB6öÆõög&ÖR“°¢76W'EöW‡6öÆõög&ÖRÂ³ã#RÂãUÒ“°¢76W'B‚6öÆõ÷&÷WF–æræÆÆ÷w5ö6†ææVÂ„æöæR’“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷Æå÷7Æ—G5÷åöæEöfFW%ö&Vf÷&U÷6VæEö'&æ6†W2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖRƒÃÂ2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&ÖWFW"ÒÇ&ÖWFW%ö–C¢S‚ÂfÇVWÂÖ—†W%&ÖWFW%&V6÷&B°¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢W6—¦S£¦g&öÒ‡&ÖWFW%ö–BÓÒ“2’À¢&Vf—ƒ¢³²EÒÀ¢&ÖWFW%ö–BÀ¢&W6W'fVC¢À¢6†ææVÅöFF¢cRÃÂbÀ¢fÇVRÀ¢Ó°¢ÆWB6VæBÒÇ6÷W&6U÷G&6³¢W6—¦RÂFW7F–æF–öå÷G&6³¢S‚ÂÆWfVÅ÷&wÂÖ—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢6÷W&6U÷G&6²À¢&÷WFU÷&ÖWFW%ö–C¢cB²FW7F–æF–öå÷G&6²À¢fÇVS¢ÆWfVÅ÷&rÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢Ó°¢ÆWB&ÖWFW'2Ò°¢&ÖWFW"ƒ“"ÂUó’À¢&ÖWFW"ƒ“2ÂÓ5ó#’À¢&ÖWFW"ƒ“BÂ3"’À¢Ó°¢ÆWB×WB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ’Â–ç6W'Bƒ"•ÒÀ¢g&ÖWFW'2À¢6öÖRƒ"’À¢“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…·6VæBƒÂÂ%óƒ’Â6VæBƒÂ"ÂeóC’Â6VæBƒ"ÂÂ%óƒ•Ò“° ¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'&V6övæ—¦VB6÷W&6RâæBfFW"v—F‚6ö×ÆWFR6VæBw&‚"“°¢76W'EöW‡ÆâçG&6²Â“°¢76W'EöW‡Æâç6fVEöfFW%öv–âÂãR“°¢76W'EöW‡Æâç6fVE÷å÷&rÂ6öÖR‚Ó5ó#’“°¢76W'EöW‡Æâç6fVE÷7FW&Võ÷6W&F–öå÷&rÂ6öÖRƒ3"’“°¢76W'EöW‡ÆâçåöÆu÷&rÂ6öÖRƒ"’“° ¢ÆWB×WB6÷W&6U÷6–FRÒ³ãÂÓãÓ°¢Æâç6÷W&6U÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WB6÷W&6U÷6–FR“°¢76W'EöW‡6÷W&6U÷6–FRÂ³ãÂÓãÒ“° ¢ÆWB×WB6÷W&6U÷7FvRÒ³ãÂãÓ°¢Æâç6÷W&6U÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WB6÷W&6U÷7FvR“°¢7WW#£¦Ç•öÖ—†W%÷7FW&Võ÷6W&F–öâ€¢f×WB6÷W&6U÷7FvRÀ¢Æâç6fVE÷7FW&Võ÷6W&F–öå÷&rçVçw&‚’À¢“°¢76W'EöW‡6÷W&6U÷7FvRÂ³ãÂãÒ“°¢7WW#£¦Ç•öÖ—†W%÷å÷&uöc3"‚f×WB6÷W&6U÷7FvRÂÓ5ó#ãÂÆâçåöÆu÷&r“°¢6÷W&6U÷7FvU³Ò£ÒÆâç6fVEöfFW%öv–ã°¢6÷W&6U÷7FvU³Ò£ÒÆâç6fVEöfFW%öv–ã° ¢ÆWB×WB÷WGWBÒ6÷W&6U÷7FvS°¢Æâæ÷WGWE÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WB÷WGWB“°¢76W'EöW†÷WGWBÂ³ã#RÂã3sUÒ“°¢76W'EöW€¢Æâæ–ç6W'EöÖWFW%÷G&ç6f÷&×2À¢fV2°¢ƒÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’’À¢ƒ"ÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’ç66ÆVBƒãR’’À¢Ð¢“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷ÆåöW‡G&7G5÷6–væÅöfÆw5öf÷%÷6Öö÷F†–ær‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÂÂfÆw7ÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†fÆw2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'BƒÂÃÂ2’Â–ç6W'BƒÂƒÃÂ2’ÂÂƒÃÂ’•ÒÀ¢eµÒÀ¢æöæRÀ¢“° ¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'6fVB6–væÂfÆw26†÷VÆB&öGV6RÆ—fR6÷W&6RÆâ"“°¢76W'EöW‡Æâç6fVE÷6–væÅöfÆw2Â6öÖR‚‡G'VRÂG'VR’’“°¢ÆWB×WBg&ÖRÒ³ãÂã#UÓ°¢Æâç6÷W&6U÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂã#UÒ“°¢7WW#£¦Ç•öÖ—†W%÷6–væÅöfÆw5öc3"‚f×WBg&ÖRÂãÂã“°¢76W'EöW†g&ÖRÂ²Óã#RÂÓãÒ“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷ÆåöW‡G&7G5öVæ&ÆVE÷7FFUöf÷%÷6Öö÷F†–ær‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÂÂfÆw7ÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†fÆw2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'BƒÂÃÂ2’Â–ç6W'BƒÂ•ÒÀ¢eµÒÀ¢æöæRÀ¢“° ¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢76W'B‡&÷WF–æræÆÆ÷w5öÆ—fU÷6÷W&6Uö6†ææVÂ…6öÖRƒ’’“°¢òòöffÆ–æR÷7FF–2&÷WF–ær&V¦V7G2F†R6fVBÖ×WFVB6÷W&6R&Vf÷&R&VæFW&–ærÂv†–ÆRÆ—fP¢òò&÷WF–ær¶VW2—Bf–Æ&ÆR6òF†RW"×6÷W&6R6Öö÷F†W"6â×WFR÷"Væ×WFR—Bà¢76W'EöW€¢&÷WF–æp¢ç6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æ÷WGWE÷G&ç6f÷&ÒÀ¢Ö—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚¢“°¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'6fVBVæ&ÆVB7FFR6†÷VÆB&öGV6RÆ—fR6÷W&6RÆâ"“°¢76W'EöW‡Æâç6fVEöVæ&ÆVBÂ6öÖR†fÇ6R’“°¢ÆWB×WBg&ÖRÒ³ãÂã#UÓ°¢Æâæ÷WGWE÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂã#UÒ“°¢Ð ¢5·FW7EÐ¢fâÆ—fU÷6öÆõö¶VW5öF—&V7E÷6÷W&6W5öf–Æ&ÆUöf÷%÷'VçF–ÖU÷6Öö÷F†–ær‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÂÂ6öÆöVGÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR‚ƒÃÂ2’Â–b6öÆöVB²ÃÂ"ÒVÇ6R²Ò’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'BƒÂfÇ6R’Â–ç6W'BƒÂG'VR’Â–ç6W'Bƒ"ÂfÇ6R•ÒÀ¢eµÒÀ¢æöæRÀ¢“° ¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ"’’“°¢76W'B‡&÷WF–æræÆÆ÷w5öÆ—fU÷6÷W&6Uö6†ææVÂ…6öÖRƒ"’’“°¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ"’¢æW‡V7B‚&F—&V7BÖ—†W"&÷WFW2æVVBÆ—fR6öÆò6öçG&öÇ2"“°¢76W'EöW€¢ÆâæÆ—fU÷6öÆòÀ¢6öÖR‡7WW#£¤Ö—†W$Æ—fU6öÆõ7FFR°¢ç•÷6öÆöVC¢G'VRÀ¢6öÆöVC¢fÇ6RÀ¢Ò¢“°¢76W'EöW‡Æâæ÷WGWE÷G&ç6f÷&ÒÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷ÆåöFVfVÇG5öÖ—76–æuöVæ&ÆVEöfÆw5÷Fõö7F—fR‚’°¢ÆWB–ç6W'BÒÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÃ¢À¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢æöæRÀ¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2…¶–ç6W'EÒÂeµÒÂæöæR“° ¢76W'B‡&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚&â–ç6W'Bv—F‚FVfVÇBÖVæ&ÆVB7FFR6†÷VÆB7W÷'BÆ—fR×WFRVF—G2"“°¢76W'EöW‡Æâç6fVEöVæ&ÆVBÂ6öÖR‡G'VR’“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷Æåö¶VW5öæW7FVE÷6VæEöv–ç5öG–æÖ–2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR‚ƒÃÂ2’Â–b÷&F–æÂÓÒ"²ÃÂ"ÒVÇ6R²Ò’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB×WB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ’Â–ç6W'Bƒ"•ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…°¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cbÀ¢fÇVS¢eóCÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢"À¢&÷WFU÷&ÖWFW%ö–C¢cBÀ¢fÇVS¢%óƒÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ò“°¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚&6VæB×&÷WFVB6÷W&6R6†÷VÆB&V6V—fRÆ—fR&÷WFRÆâ"“°¢76W'EöW€¢ÆâæÆ—fU÷6VæE÷F&vWEö¶W—2À¢fV2°¢Ö—†W$Æ—fU6VæEF&vWD¶W’°¢6÷W&6U÷G&6³¢À¢FW7F–æF–öå÷G&6³¢"À¢ÒÀ¢Ö—†W$Æ—fU6VæEF&vWD¶W’°¢6÷W&6U÷G&6³¢"À¢FW7F–æF–öå÷G&6³¢À¢ÒÀ¢Ð¢“°¢76W'EöW‡Æâç6fVEöÆ—fU÷6VæEöv–ç2ÂfV2³ãRÂãÒ“°¢76W'EöW‡ÆâæÆ—fU÷6öÆòÂæöæR“°¢76W'EöW‡ÆâæÆ—fU÷6VæEö÷WGWE÷6öÆõ÷G&6·2ÂfV2·fV2³Â%ÕÒ“°¢76W'EöW‡Æâç6fVEöÆ—fU÷6VæEö÷WGWE÷6öÆõöv–ç2ÂfV2³ãÒ“°¢76W'EöW€¢ÆâæÆ—fU÷6VæEöÖWFW%÷6öÆõ÷G&6·2À¢fV2·fV2³Â%ÒÂfV2³Â%ÕÐ¢“°¢76W'EöW‡Æâç6fVEöÆ—fU÷6VæEöÖWFW%÷6öÆõöv–ç2ÂfV2³ãÂãÒ“°¢76W'EöW‡ÆâæÆ—fUö–ç6W'EöVæ&ÆVE÷G&6·2ÂfV2³%Ò“°¢76W'EöW‡Æâç6fVEöÆ—fUö–ç6W'EöVæ&ÆVBÂfV2·G'VUÒ“°¢76W'EöW€¢Æâæ÷WGWE÷G&ç6f÷&ÒÀ¢Ö—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’ç66ÆVBƒãR¢“°¢76W'EöW€¢Æâæ–ç6W'EöÖWFW%÷G&ç6f÷&×2À¢fV2°¢ƒÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’’À¢ƒ"ÂÖ—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’ç66ÆVBƒãR’’À¢Ð¢“°¢76W'EöW€¢ÆâæÆ—fU÷6VæEö÷WGWE÷F‡2À¢6öÖR‡fV2´Ö—†W$Æ—fU6VæD÷WGWEF‚°¢G&ç6f÷&Ó¢Ö—†W%6÷W&6U&÷WFUG&ç6f÷&Ó£¦FVfVÇB‚’À¢6VæEöv–ç3¢fV2°¢Ö—†W$Æ—fU6VæDv–â°¢F&vWEö–æFWƒ¢6öÖRƒ’À¢6fVEöv–ã¢ãRÀ¢Væ&ÆVE÷F&vWEö–æFWƒ¢6öÖRƒ’À¢6fVEöVæ&ÆVC¢G'VRÀ¢ÒÀ¢Ö—†W$Æ—fU6VæDv–â°¢F&vWEö–æFWƒ¢6öÖRƒ’À¢6fVEöv–ã¢ãÀ¢Væ&ÆVE÷F&vWEö–æFWƒ¢6öÖRƒ’À¢6fVEöVæ&ÆVC¢G'VRÀ¢ÒÀ¢ÒÀ¢–ç6W'EöVæ&ÆVEöv–ç3¢fV2´Ö—†W$Æ—fT–ç6W'DVæ&ÆVDv–â°¢F&vWEö–æFWƒ¢6öÖRƒ’À¢6fVEöVæ&ÆVC¢G'VRÀ¢ÕÒÀ¢ÕÒ¢“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6VæEöFW7F–æF–öåö×WFU÷&VÖ–ç5÷FövvÆV&ÆU÷F‡&÷Vv…öæW7FVE÷&÷WFW2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†ÖF6‚÷&F–æÂ°¢"ÓâÀ¢2ÓâƒÃÂ2’ÂƒÃÂ"’À¢òÓâÃÂ2À¢Ò’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB×WB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ’Â–ç6W'Bƒ"’Â–ç6W'Bƒ2•ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…°¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cbÀ¢fÇVS¢%óƒÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢"À¢&÷WFU÷&ÖWFW%ö–C¢crÀ¢fÇVS¢%óƒÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ò“° ¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢76W'B‡&÷WF–æræÆÆ÷w5öÆ—fU÷6÷W&6Uö6†ææVÂ…6öÖRƒ’’“°¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'F†R6÷W&6R&÷WFR6†÷VÆB&VÖ–âf–Æ&ÆRFòF†RÆ—fRÖ—†W""“°¢76W'EöW‡ÆâæÆ—fUö–ç6W'EöVæ&ÆVE÷G&6·2ÂfV2³"Â5Ò“°¢76W'EöW‡Æâç6fVEöÆ—fUö–ç6W'EöVæ&ÆVBÂfV2¶fÇ6RÂG'VUÒ“°¢76W'EöW‡ÆâæÆ—fU÷6VæE÷F&vWEö¶W—2æÆVâ‚’Â"“°¢ÆWBF‡2ÒÆà¢æÆ—fU÷6VæEö÷WGWE÷F‡0¢æ5÷&Vb‚¢æW‡V7B‚&6ö×ÆWFR6VæB&÷WFR6†÷VÆB†fRG–æÖ–2÷WGWBF‡2"“°¢76W'EöW‡F‡2æÆVâ‚’Â“°¢76W'EöW‡F‡5³Òæ–ç6W'EöVæ&ÆVEöv–ç2æÆVâ‚’Â"“°¢76W'EöW‡F‡5³Òæ–ç6W'EöVæ&ÆVEöv–ç5³ÒçF&vWEö–æFW‚Â6öÖRƒ’“°¢76W'B‚F‡5³Òæ–ç6W'EöVæ&ÆVEöv–ç5³Òç6fVEöVæ&ÆVB“°¢76W'EöW‡F‡5³Òæ–ç6W'EöVæ&ÆVEöv–ç5³ÒçF&vWEö–æFW‚Â6öÖRƒ’“°¢76W'B‡F‡5³Òæ–ç6W'EöVæ&ÆVEöv–ç5³Òç6fVEöVæ&ÆVB“°¢ÆWB×WBg&ÖRÒ³ãÂã#UÓ°¢Æâæ÷WGWE÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂãÒ“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6÷W&6U÷&÷WFU÷Æåö¶VW5öF—6&ÆVE÷Væ—VU÷6VæG5÷FövvÆV&ÆR‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR†–b÷&F–æÂÓÒ"°¢ƒÃÂ2’ÂƒÃÂ"¢ÒVÇ6R°¢ÃÂ0¢Ò’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB×WB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ’Â–ç6W'Bƒ"•ÒÀ¢eµÒÀ¢æöæRÀ¢“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…°¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cbÀ¢fÇVS¢eóCÀ¢Væ&ÆVC¢6öÖR†fÇ6R’À¢&÷WF–æuöfÆuöÆö6F–öã¢6öÖR‚ƒÂ’’À¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢"À¢&÷WFU÷&ÖWFW%ö–C¢cBÀ¢fÇVS¢%óƒÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢6öÖR‚ƒÂ’’À¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÒÀ¢Ò“°¢76W'B‚&÷WF–æræÆÆ÷w5ö6†ææVÂ…6öÖRƒ’’“°¢76W'B‡&÷WF–æræÆÆ÷w5öÆ—fU÷6÷W&6Uö6†ææVÂ…6öÖRƒ’’“° ¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚&Væ—VVÇ’—&VBF—6&ÆVB6VæB6†÷VÆB&VÖ–âÆ—fR×FövvÆV&ÆR"“°¢76W'EöW‡Æâç6fVEöÆ—fU÷6VæEöv–ç2ÂfV2³ãRÂãÒ“°¢76W'EöW‡Æâç6fVEöÆ—fU÷6VæEöVæ&ÆVBÂfV2¶fÇ6RÂG'VUÒ“°¢76W'EöW€¢ÆâæÆ—fU÷6VæE÷F&vWEö¶W—2À¢fV2°¢Ö—†W$Æ—fU6VæEF&vWD¶W’°¢6÷W&6U÷G&6³¢À¢FW7F–æF–öå÷G&6³¢"À¢ÒÀ¢Ö—†W$Æ—fU6VæEF&vWD¶W’°¢6÷W&6U÷G&6³¢"À¢FW7F–æF–öå÷G&6³¢À¢ÒÀ¢Ð¢“°¢ÆWB×WBg&ÖRÒ³ãÂã#UÓ°¢Æâæ÷WGWE÷G&ç6f÷&ÒæÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂãÒ“°¢Ð ¢5·FW7EÐ¢fâÆ—fUöÖ—†W%÷6VæE÷&÷WFU÷Æå÷v÷&·5÷v—F†÷WEö–ç6W'E÷7FFUöfÆw2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢æöæRÀ¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB×WB&÷WF–ærÐ¢Ö—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2…¶–ç6W'Bƒ’Â–ç6W'Bƒ•ÒÂeµÒÂæöæR“°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…´Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cBÀ¢fÇVS¢eóCÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÕÒ“° ¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚&â7F—fR6VæBÆWfVÂ6†÷VÆB†fRÆ—fR6÷W&6R&÷WFRÆâ"“°¢76W'EöW€¢ÆâæÆ—fU÷6VæE÷F&vWEö¶W—2À¢fV2´Ö—†W$Æ—fU6VæEF&vWD¶W’°¢6÷W&6U÷G&6³¢À¢FW7F–æF–öå÷G&6³¢À¢ÕÐ¢“°¢76W'EöW‡Æâç6fVEöVæ&ÆVBÂ6öÖR‡G'VR’“°¢Ð ¢5·FW7EÐ¢fâGWÆ–6FUöÆ—fUöÖ—†W%÷6VæE÷—'5öFõöæ÷E÷6†&UööG–æÖ–5÷F&vWB‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢ÓÀ¢÷WGWE÷&s¢ÓÀ¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖRƒÃÂ2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB×WB&÷WF–ærÐ¢Ö—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2…¶–ç6W'Bƒ’Â–ç6W'Bƒ•ÒÂeµÒÂæöæR“°¢ÆWB6VæBÒÇ&V6÷&Eö–æFW‡ÂÖ—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cBÀ¢fÇVS¢eóCÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFW‚À¢Ó°¢&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…·6VæBƒ’Â6VæBƒ•Ò“° ¢ÆWBÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'F†R–ç6W'B†2Æ—fR×WFR6öçG&öÂ"“°¢76W'B‡ÆâæÆ—fU÷6VæE÷F&vWEö¶W—2æ—5öV×G’‚’“°¢ÆWBF‡2ÒÆà¢æÆ—fU÷6VæEö÷WGWE÷F‡0¢æW‡V7B‚'F†R6fVB&÷WFRÖ—26ö×ÆWFR"“°¢76W'EöW‡F‡2æÆVâ‚’Â"“°¢76W'B€¢F‡0¢æ—FW"‚¢æÆÂ‡ÇF‡ÂF‚ç6VæEöv–ç5³ÒçF&vWEö–æFW‚æ—5öæöæR‚’¢“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷&÷WFUöVF–&–Æ—G•öÆ–W5ööæÇ•÷Væ—VU÷¦W&õ÷66÷UöfFW%ö6æF–FFW2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢À¢÷WGWE÷&s¢À¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖR‚ƒÃÂ2’ÂƒÃÂ"’’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&ÖWFW"Ð¢Æ–ç6W'Eö÷&F–æÃ¢SbÂ66÷S¢SbÂ&ÖWFW%ö–C¢S‚ÂfÇVWÂÖ—†W%&ÖWFW%&V6÷&B°¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢&Vf—ƒ¢³²EÒÀ¢&ÖWFW%ö–BÀ¢&W6W'fVC¢À¢6†ææVÅöFF¢‚ƒcB²–ç6W'Eö÷&F–æÂ’ÃÂb’Â‡66÷RÃÂ2’À¢fÇVRÀ¢Ó°¢ÆWB&ÖWFW'2Ò°¢&ÖWFW"ƒÂÂ“"Â%óS’À¢&ÖWFW"ƒÂÂ“2Â5ó#’À¢&ÖWFW"ƒÂÂ“"ÂUó’À¢&ÖWFW"ƒÂÂ“2ÂÓ5ó#’À¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ’Â–ç6W'Bƒ"•ÒÀ¢g&ÖWFW'2À¢6öÖRƒ"’À¢“° ¢76W'EöW‡&÷WF–æræÖ7FW%öfFW%÷&r‚’Â6öÖRƒ%óS’“°¢76W'EöW‡&÷WF–æræÖ7FW%÷å÷&r‚’Â6öÖRƒ5ó#’“°¢ÆWBÆ—fU÷&÷WFU÷ÆâÒ&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æW‡V7B‚'F†RF—&V7B–ç6W'BfFW"6†÷VÆBW‡÷6RÆ—fRG&ç6f÷&Ò"“°¢76W'EöW†Æ—fU÷&÷WFU÷ÆâçG&6²Â“°¢76W'EöW†Æ—fU÷&÷WFU÷Æâç6fVEöfFW%öv–âÂãR“°¢76W'EöW†Æ—fU÷&÷WFU÷Æâç6fVE÷å÷&rÂ6öÖR‚Ó5ó#’“°¢ÆWB×WBÆ—fUög&ÖRÒ³ãÂ"ãÓ°¢Æ—fU÷&÷WFU÷Æà¢ç6÷W&6U÷G&ç6f÷&Ð¢æÇ•ög&ÖR‚f×WBÆ—fUög&ÖR“°¢7WW#£¦Ç•öÖ—†W%÷å÷&uöc3"€¢f×WBÆ—fUög&ÖRÀ¢Æ—fU÷&÷WFU÷Æâç6fVE÷å÷&rçVçw&‚’2c3"À¢Æ—fU÷&÷WFU÷ÆâçåöÆu÷&rÀ¢“°¢Æ—fUög&ÖU³Ò£ÒÆ—fU÷&÷WFU÷Æâç6fVEöfFW%öv–ã°¢Æ—fUög&ÖU³Ò£ÒÆ—fU÷&÷WFU÷Æâç6fVEöfFW%öv–ã°¢Æ—fU÷&÷WFU÷Æà¢æ÷WGWE÷G&ç6f÷&Ð¢æÇ•ög&ÖR‚f×WBÆ—fUög&ÖR“°¢76W'EöW†Æ—fUög&ÖRÂ³ãÂãUÒ“°¢ÆWB×WB–ç6W'Eög&ÖRÒ³ãÂ"ãÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WB–ç6W'Eög&ÖR“°¢76W'EöW†–ç6W'Eög&ÖRÂ³ãÂãUÒ“°¢ÆWB×WBVæÖF6†VEög&ÖRÒ³ãÂ"ãÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ"’¢æÇ•ög&ÖR‚f×WBVæÖF6†VEög&ÖR“°¢76W'EöW‡VæÖF6†VEög&ÖRÂ³ãÂ"ãÒ“° ¢ÆWB×WB6VæE÷&÷WF–ærÒ&÷WF–æræ6ÆöæR‚“°¢6VæE÷&÷WF–æræFE÷6VæE÷7VÖÖ&–W2…´Ö—†W%6VæE7VÖÖ'’°¢–ç6W'Eö÷&F–æÃ¢À¢&÷WFU÷&ÖWFW%ö–C¢cBÀ¢fÇVS¢%óƒÀ¢Væ&ÆVC¢6öÖR‡G'VR’À¢&÷WF–æuöfÆuöÆö6F–öã¢æöæRÀ¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢ÕÒ“°¢76W'B€¢6VæE÷&÷WF–æp¢æÆ—fU÷6÷W&6U÷&÷WFU÷Æåöf÷%ö6†ææVÂ…6öÖRƒ’¢æ—5÷6öÖR‚¢“° ¢f÷"–çfÆ–B–â°¢fV2·&ÖWFW"ƒÂÂ“"ÂUó’Â&ÖWFW"ƒÂÂ“"Âeó•ÒÀ¢fV2·&ÖWFW"ƒÂÂ“"ÂUó•ÒÀ¢fV2·&ÖWFW"ƒÂÂ“"Âeó•ÒÀ¢Ò°¢ÆWB&÷WF–ærÐ¢Ö—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2…¶–ç6W'Bƒ•ÒÂf–çfÆ–BÂ6öÖRƒ"’“°¢ÆWB×WBg&ÖRÒ³ãÂ"ãÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂ"ãÒ“°¢Ð ¢f÷"–çfÆ–B–â°¢fV2·&ÖWFW"ƒÂÂ“2ÂÓ5ó#’Â&ÖWFW"ƒÂÂ“2Â5ó#•ÒÀ¢fV2·&ÖWFW"ƒÂÂ“2ÂÓ5ó#•ÒÀ¢fV2·&ÖWFW"ƒÂÂ“2ÂeóC•ÒÀ¢Ò°¢ÆWB&÷WF–ærÐ¢Ö—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2…¶–ç6W'Bƒ•ÒÂf–çfÆ–BÂ6öÖRƒ"’“°¢ÆWB×WBg&ÖRÒ³ãÂ"ãÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBg&ÖR“°¢76W'EöW†g&ÖRÂ³ãÂ"ãÒ“°¢Ð¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷7FW&Võ÷6W&F–öå÷W6W5÷Væ—VUö6æF–FFU÷&V6÷&G2‚’°¢ÆWB–ç6W'BÒÆ÷&F–æÇÂÖ—†W$–ç6W'E7VÖÖ'’°¢÷&F–æÂÀ¢–çWE÷&s¢À¢÷WGWE÷&s¢À¢6öÆ÷%÷&s¢À¢–6öå÷&s¢æöæRÀ¢æÖS¢æöæRÀ¢fÆw3¢6öÖRƒÃÂ2’À¢fÆw5öWfVçEö–æFWƒ¢æöæRÀ¢f—'7EöWfVçEö–æFWƒ¢À¢VæEöWfVçEö–æFWƒ¢À¢Ó°¢ÆWB&ÖWFW"ÒÆ–ç6W'Eö÷&F–æÃ¢SbÂ66÷S¢SbÂfÇVWÂÖ—†W%&ÖWFW%&V6÷&B°¢WfVçEö–æFWƒ¢À¢&V6÷&Eö–æFWƒ¢À¢&Vf—ƒ¢³²EÒÀ¢&ÖWFW%ö–C¢“BÀ¢&W6W'fVC¢À¢6†ææVÅöFF¢‚ƒcB²–ç6W'Eö÷&F–æÂ’ÃÂb’Â‡66÷RÃÂ2’À¢fÇVRÀ¢Ó°¢ÆWB&÷WF–ærÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ’Â–ç6W'Bƒ•ÒÀ¢e·&ÖWFW"ƒÂÂÓcB’Â&ÖWFW"ƒÂÂcB•ÒÀ¢æöæRÀ¢“°¢76W'EöW‡&÷WF–æræÖ7FW%÷7FW&Võ÷6W&F–öå÷&r‚’Â6öÖR‚ÓcB’“°¢ÆWB×WBÖöæõö–ç6W'BÒ³ãÂÓãÓ°¢&÷WF–æp¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBÖöæõö–ç6W'B“°¢76W'EöW†Ööæõö–ç6W'BÂ³ãÂãÒ“° ¢ÆWBÖ&–wV÷W2ÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ•ÒÀ¢e·&ÖWFW"ƒÂÂcB’Â&ÖWFW"ƒÂÂÓcB•ÒÀ¢æöæRÀ¢“°¢ÆWB×WBVæ6†ævVBÒ³ãÂÓãÓ°¢Ö&–wV÷W0¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBVæ6†ævVB“°¢76W'EöW‡Væ6†ævVBÂ³ãÂÓãÒ“° ¢ÆWB66÷VBÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ•ÒÀ¢e·&ÖWFW"ƒÂÂcB•ÒÀ¢æöæRÀ¢“°¢ÆWB×WBVæ6†ævVBÒ³ãÂÓãÓ°¢66÷V@¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBVæ6†ævVB“°¢76W'EöW‡Væ6†ævVBÂ³ãÂÓãÒ“° ¢ÆWB÷WEööe÷&ævRÒÖ—†W%&÷WFTVF–&–Æ—G“£¦g&öÕö–ç6W'G5÷v—F…÷&ÖWFW'2€¢¶–ç6W'Bƒ•ÒÀ¢e·&ÖWFW"ƒÂÂcR•ÒÀ¢æöæRÀ¢“°¢ÆWB×WBVæ6†ævVBÒ³ãÂÓãÓ°¢÷WEööe÷&ævP¢çG&ç6f÷&Õöf÷%ö6†ææVÂ…6öÖRƒ’¢æÇ•ög&ÖR‚f×WBVæ6†ævVB“°¢76W'EöW‡Væ6†ævVBÂ³ãÂÓãÒ“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷7FW&Võ÷6W&F–öå÷&W6W'fW5öÖ–EöæEöF§W7G5÷6–FR‚’°¢ÆWB×WBVæ6†ævVBÒ³ãsRÂã#UÓ°¢Ç•öÖ—†W%÷7FW&Võ÷6W&F–öâ‚f×WBVæ6†ævVBÂ“°¢76W'EöW‡Væ6†ævVBÂ³ãsRÂã#UÒ“° ¢ÆWB×WBÖöæòÒ³ãsRÂã#UÓ°¢Ç•öÖ—†W%÷7FW&Võ÷6W&F–öâ‚f×WBÖöæòÂcB“°¢76W'EöW†ÖöæòÂ³ãRÂãUÒ“° ¢ÆWB×WBv–FVæVBÒ³ãsRÂã#UÓ°¢Ç•öÖ—†W%÷7FW&Võ÷6W&F–öâ‚f×WBv–FVæVBÂÓcB“°¢76W'EöW‡v–FVæVBÂ³ãÂãÒ“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷å÷W6W5÷&ö¦V7Eö6—&7VÆ%öæE÷G&–æwVÆ%öÆw2‚’°¢ÆWB×WB6—&7VÆ"Ò³ãÂ"ãÓ°¢Ç•öÖ—†W%÷â‚f×WB6—&7VÆ"ÂÂæöæR“°¢ÆWB6VçFW%öv–âÒ7FC£¦c3#£¦6öç7G3£¤e$5óõ5%Eó#°¢76W'B‚†6—&7VÆ%³ÒÒ6VçFW%öv–â’æ'2‚’ÂãRÓb“°¢76W'B‚†6—&7VÆ%³ÒÒ"ã¢6VçFW%öv–â’æ'2‚’ÂãRÓb“° ¢ÆWB×WBG&–æwVÆ"Ò³ãÂ"ãÓ°¢Ç•öÖ—†W%÷â‚f×WBG&–æwVÆ"ÂÂ6öÖRƒ"’“°¢76W'EöW‡G&–æwVÆ"Â³ãÂ"ãÒ“° ¢ÆWB×WB†&EöÆVgBÒ³ãÂ"ãÓ°¢Ç•öÖ—†W%÷â‚f×WB†&EöÆVgBÂÓeóCÂæöæR“°¢76W'EöW††&EöÆVgBÂ³2ãÂãÒ“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%ö–ç6W'E÷&VæÖU÷&W6W'fW5÷WFce÷FW&Ö–æF÷%÷7Vff—…öæEö÷F†W%öWfVçG2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒ”Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cƒ5óc„7S3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒTb“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚csUö“bçFõöÆUö'—FW2‚’“°¢ÆWB×WBæÖU÷–ÆöBÒ$´”4² ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢æÖU÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ÂÂ„Â„$%Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„42ÂfæÖU÷–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S‚Âe³ƒSUÒ“° ¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢Fö7VÖVç@¢ç6WEöÖ—†W%ö–ç6W'EöæÖRƒÂ$´”4²	ù*R"¢æW‡V7B‚%UDbÓbæÖW26†÷VÆB7W÷'BVæ–6öFR"“° ¢ÆWB–ç6W'BÒfFö7VÖVçBæÖ—†W%ö–ç6W'G2‚•³Ó°¢76W'EöW†–ç6W'BææÖR‚’Â6öÖR‚$´”4²	ù*R"’“°¢ÆWBæÖUöWfVçEö–æFW‚Ò–ç6W'@¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„42¢æW‡V7B‚'F†R–ç6W'B6†÷VÆB&WF–â—G2æÖRWfVçB"“°¢ÆWB×WBW‡V7FVE÷–ÆöBÒ$´”4²	ù*R ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢W‡V7FVE÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ÂÂ„Â„$%Ò“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•¶æÖUöWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷–Æö@¢“°¢f÷"†–æFW‚ÂWfVçB’–âFö7VÖVçBæWfVçG2‚’æ—FW"‚’æVçVÖW&FR‚’°¢–b–æFW‚ÒæÖUöWfVçEö–æFW‚°¢76W'EöW†WfVçBçv—&Uö'—FW2‚’Â÷&–v–æÅöWfVçG5¶–æFW…Ò“°¢Ð¢Ð¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R&VæÖVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB¢æW‡V7B‚'F†R&VæÖVB&ö¦V7B6†÷VÆB'6R"¢æÖ—†W%ö–ç6W'G2‚•³Ð¢ææÖR‚’À¢6öÖR‚$´”4²	ù*R"¢“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%ö–ç6W'E÷&VæÖU÷W6W5öÆVv7•öVæ6öF–æuöæE÷&V¦V7G5÷Vç&W&W6VçF&ÆU÷FW‡B‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒ”Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cƒ5óc„7S3"çFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„42Â"$´”4µÃ"“° ¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVçBç&ö¦V7E÷fW'6–öâÒ6öÖR‚#ã"çFõö÷væVB‚’“°¢Fö7VÖVç@¢ç6WEöÖ—†W%ö–ç6W'EöæÖRƒÂ$6l:’"¢æW‡V7B‚'&W&W6VçF&ÆRÆVv7’FW‡B6†÷VÆBVæ6öFR"“°¢76W'EöW†Fö7VÖVçBæÖ—†W%ö–ç6W'G2‚•³ÒææÖR‚’Â6öÖR‚$6l:’"’“°¢ÆWBæÖUöWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„42¢æW‡V7B‚'F†RæÖRWfVçB6†÷VÆB&VÖ–â"“°¢76W'EöW†æÖUöWfVçBç–ÆöB‚’Â"$6eÇ„S•Ã"“° ¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÖ—†W%ö–ç6W'EöæÖRƒÂ$6l:’	úX"’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW€¢Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"’À¢&Vf÷&RÀ¢&f–ÆVB&VæÖW26†÷VÆBæ÷B'F–ÆÇ’×WFFRF†RæÖRWfVçB ¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öf—†VEöÖ—†W%÷&ÖWFW%÷&V6÷&G5öæE÷&V¦V7G5÷VæÖVE÷&V6÷&E÷6—¦W2‚’°¢ÆWB×WB–ÆöBÒfV2³„Â„$"Â„42Â„DBÂ“"ÂÓ°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚cƒ#5÷SbçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c%óƒö“3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂƒ#"Âƒ32ÂƒCBÂ“2ÂuÒ“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚cƒCScu÷SbçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚b‚ÓcCö“3"’çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒSRÂƒcbÂƒsrÂƒƒ‚Â“"ÂÒ“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚bƒcE÷SbÃÂb’çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c%óƒö“3"çFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SÂg–ÆöB“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'F†Rf—†VB×6—¦RÖ—†W"&ÖWFW"WfVçB6†÷VÆB'6R"“° ¢ÆWB&V6÷&G2ÒFö7VÖVç@¢æÖ—†W%÷&ÖWFW%÷&V6÷&G2‚¢æW‡V7B‚#"Ö'—FR&V6÷&G26†÷VÆBFV6öFR"“°¢76W'EöW‡&V6÷&G2æÆVâ‚’Â2“°¢76W'EöW‡&V6÷&G5³Òç&Vf—‚‚’Â³„Â„$"Â„42Â„DEÒ“°¢76W'EöW‡&V6÷&G5³Òç&ÖWFW%ö–B‚’Â“"“°¢76W'EöW‡&V6÷&G5³Òæ¶–æB‚’ÂÖ—†W%&ÖWFW$¶–æC£¥föÇVÖR“°¢76W'EöW‡&V6÷&G5³Òç&W6W'fVB‚’Â“°¢76W'EöW‡&V6÷&G5³Òæ6†ææVÅöFF‚’Âƒ#2“°¢76W'EöW‡&V6÷&G5³ÒçF&vWEö–æFW‚‚’ÂB“°¢76W'EöW‡&V6÷&G5³Òç6Æ÷Eö–æFW‚‚’Âƒ#2“°¢76W'EöW‡&V6÷&G5³ÒçF&vWE÷66÷U÷&r‚’Â“°¢76W'EöW‡&V6÷&G5³ÒçfÇVR‚’Â%óƒ“°¢76W'EöW‡&V6÷&G5³Òç&ÖWFW%ö–B‚’Â“2“°¢76W'EöW‡&V6÷&G5³Òæ¶–æB‚’ÂÖ—†W%&ÖWFW$¶–æC£¥â“°¢76W'EöW‡&V6÷&G5³Òç&W6W'fVB‚’Âr“°¢76W'EöW‡&V6÷&G5³Òæ6†ææVÅöFF‚’ÂƒCScr“°¢76W'EöW‡&V6÷&G5³ÒçF&vWEö–æFW‚‚’Â#“°¢76W'EöW‡&V6÷&G5³Òç6Æ÷Eö–æFW‚‚’Âƒ#r“°¢76W'EöW‡&V6÷&G5³ÒçF&vWE÷66÷U÷&r‚’Â"“°¢76W'EöW‡&V6÷&G5³ÒçfÇVR‚’ÂÓcC“°¢76W'EöW‡&V6÷&G5³%ÒçF&vWEö–æFW‚‚’ÂcB“°¢76W'EöW‡&V6÷&G5³%Òæ6æF–FFUö–ç6W'Eö÷&F–æÂ‚’Â6öÖRƒ’“° ¢ÆWB×WBÖÆf÷&ÖVE÷7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBÖÆf÷&ÖVE÷7G&VÒÂ„SÂe³„Ò“°¢ÆWBÖÆf÷&ÖVBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fÖÆf÷&ÖVE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'Væ¶æ÷vâ&ÖWFW"–ÆöG2&VÖ–â'6V&ÆR"“°¢76W'B†ÖF6†W2€¢ÖÆf÷&ÖVBæÖ—†W%÷&ÖWFW%÷&V6÷&G2‚’À¢W'"„fÇW'&÷#£¤–çfÆ–DWfVçB²ââÒ¢’“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷6VæE÷7VÖÖ&–W5÷—%÷&÷WFUöÆWfVÇ5÷v—F…ö–ç6W'E÷&÷WF–æuöfÆw2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢f÷"6öÆ÷"–â³ƒ5óc„7S3"ÂƒEó#s“%Ò°¢WfVçE÷7G&VÒçW6‚ƒƒ”“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f6öÆ÷"çFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SrÂe³ƒCÂƒC%Ò“°¢ÆWBfÆw2Ò–b6öÆ÷"ÓÒƒ5óc„2°¢e³ÂÕ²âåÐ¢ÒVÇ6R°¢e³Õ²âåÐ¢Ó°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„T"ÂfÆw2“°¢Ð ¢ÆWB×WB&ÖWFW%÷–ÆöBÒfV3£¦æWr‚“°¢f÷"‡6÷W&6Uö–ç6W'BÂ&÷WFUö–BÂfÇVR’–â²ƒcE÷SbÂce÷S‚ÂÓ#Uö“3"’ÂƒcBÂcrÂSö“3"•Ò°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³„Â„$"Â„42Â„DEÒ“°¢&ÖWFW%÷–ÆöBçW6‚‡&÷WFUö–B“°¢&ÖWFW%÷–ÆöBçW6‚ƒ“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚b‡6÷W&6Uö–ç6W'BÃÂb’çFõöÆUö'—FW2‚’“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢Ð¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂƒ#"Âƒ32ÂƒCEÒ“°¢&ÖWFW%÷–ÆöBçW6‚ƒc‚“°¢&ÖWFW%÷–ÆöBçW6‚ƒ“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚bƒcU÷SbÃÂb’çFõöÆUö'—FW2‚’“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚csSö“3"çFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SÂg&ÖWFW%÷–ÆöB“° ¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'F†RÖ—†W"&÷WF–ærf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6VæG2ÒFö7VÖVç@¢æÖ—†W%÷6VæE÷7VÖÖ&–W2‚¢æW‡V7B‚'&V6övæ—¦VBÖ—†W"&ÖWFW"&V6÷&G26†÷VÆBFV6öFR"“°¢76W'EöW‡6VæG2æÆVâ‚’Â2“°¢76W'EöW‡6VæG5³Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6VæG5³Òç&÷WFU÷&ÖWFW%ö–B‚’Âcb“°¢76W'EöW‡6VæG5³ÒçfÇVR‚’ÂÓ#R“°¢76W'EöW‡6VæG5³ÒæVæ&ÆVB‚’Â6öÖR†fÇ6R’“°¢76W'EöW‡6VæG5³Òç&÷WF–æuöfÆuöÆö6F–öâ‚’Â6öÖR‚ƒBÂ’’“°¢76W'EöW‡6VæG5³Òç&÷WFU÷&ÖWFW%ö–B‚’Âcr“°¢76W'EöW‡6VæG5³ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW‡6VæG5³Òç&÷WF–æuöfÆuöÆö6F–öâ‚’Â6öÖR‚ƒBÂ’’“°¢76W'EöW‡6VæG5³%Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6VæG5³%Òç&÷WFU÷&ÖWFW%ö–B‚’Âc‚“°¢76W'EöW‡6VæG5³%ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW‡6VæG5³%Òç&÷WF–æuöfÆuöÆö6F–öâ‚’Â6öÖR‚ƒ’Â’’“°¢76W'B‡6VæG2æ—FW"‚’æÆÂ‡Ç6VæGÂ6VæBç&ÖWFW%öÆö6F–öâ‚’ãÓÒ’“° ¢ÆWBF—7Æ•öw&÷Wö'—FW2ÒFö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢Fö7VÖVç@¢ç6WEöÖ—†W%÷&÷WF–æuöfÆrƒBÂÂG'VR¢æW‡V7B‚&—&VB&÷WF–ærfÆr6†÷VÆB&RVF—F&ÆR"“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’ÂF—7Æ•öw&÷Wö'—FW2“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³EÒç–ÆöB‚’Âe³÷S‚ÂÒ“°¢76W'EöW€¢Fö7VÖVçBæÖ—†W%÷6VæE÷7VÖÖ&–W2‚’çVçw&‚•³ÒæVæ&ÆVB‚’À¢6öÖR‡G'VR¢“° ¢ÆWB&Vf÷&Uö–çfÆ–EöVF—BÒFö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWB&Vf÷&Uö–çfÆ–E÷&÷WFRÒFö7VÖVçBæWfVçG2‚•³EÒçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÖ—†W%÷&÷WF–æuöfÆrƒ2ÂÂfÇ6R’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÖ—†W%÷&÷WF–æuöfÆrƒBÂ"ÂfÇ6R’À¢W'"„fÇW'&÷#£¤–çfÆ–DWfVçB²ââÒ¢’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’Â&Vf÷&Uö–çfÆ–EöVF—B“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³EÒçv—&Uö'—FW2‚’Â&Vf÷&Uö–çfÆ–E÷&÷WFR“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R6†ævVBÖ—†W"&÷WFR6†÷VÆBVæ6öFRÆ÷76ÆW76Ç’"“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R6†ævVBÖ—†W"&÷WFR'6W2"“°¢76W'EöW€¢&÷VæE÷G&—æÖ—†W%÷6VæE÷7VÖÖ&–W2‚’çVçw&‚•³ÒæVæ&ÆVB‚’À¢6öÖR‡G'VR¢“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷6VæE÷7VÖÖ&–W5÷—%÷7Æ—E÷&ö¦V7E÷&÷WF–æu÷7G&VÕö–å÷&ÖWFW%ö÷&FW"‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SrÂ"$G'V×5Ã"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SrÂe³Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SrÂe³ÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢f÷"6öÆ÷"–â³ƒ5óc„7S3"ÂƒEó#s“%Ò°¢WfVçE÷7G&VÒçW6‚ƒƒ”“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“R“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f6öÆ÷"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒ“2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óö“3"’çFõöÆUö'—FW2‚’“°¢Ð ¢ÆWB×WB&ÖWFW%÷–ÆöBÒfV3£¦æWr‚“°¢f÷"‡6÷W&6Uö–ç6W'BÂ&÷WFUö–BÂfÇVR’–â°¢ƒcE÷SbÂce÷S‚Â%óƒö“3"’À¢ƒcBÂcrÂeóC’À¢ƒcRÂcbÂ•óc’À¢Ò°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³„Â„$"Â„42Â„DEÒ“°¢&ÖWFW%÷–ÆöBçW6‚‡&÷WFUö–B“°¢&ÖWFW%÷–ÆöBçW6‚ƒ“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚b‡6÷W&6Uö–ç6W'BÃÂb’çFõöÆUö'—FW2‚’“°¢&ÖWFW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢Ð¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SÂg&ÖWFW%÷–ÆöB“° ¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'F†R7Æ—BÖ—†W"&÷WF–ærf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6VæG2ÒFö7VÖVç@¢æÖ—†W%÷6VæE÷7VÖÖ&–W2‚¢æW‡V7B‚$Ö—†W"&ÖWFW"&V6÷&G26†÷VÆBFV6öFR"“°¢76W'EöW‡6VæG2æÆVâ‚’Â2“°¢76W'EöW‡6VæG5³Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6VæG5³ÒæVæ&ÆVB‚’Â6öÖR†fÇ6R’“°¢76W'EöW‡6VæG5³Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6VæG5³ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW‡6VæG5³%Òæ–ç6W'Eö÷&F–æÂ‚’Â“°¢76W'EöW‡6VæG5³%ÒæVæ&ÆVB‚’Â6öÖR†fÇ6R’“° ¢ÆWBf—'7E÷&÷WF–æuöWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„SrbbWfVçBç–ÆöB‚’ÓÒ³Ò¢æW‡V7B‚'F†Rf—'7B7Æ—B&÷WF–ærWfVçB6†÷VÆB&R&W6VçB"“°¢76W'EöW€¢6VæG5³Òç&÷WF–æuöfÆuöÆö6F–öâ‚’À¢6öÖR‚†f—'7E÷&÷WF–æuöWfVçBÂ’¢“°¢Fö7VÖVç@¢ç6WEöÖ—†W%÷&÷WF–æuöfÆr†f—'7E÷&÷WF–æuöWfVçBÂÂG'VR¢æW‡V7B‚'F†R&ö¦V7B&÷WF–ær'—FR6†÷VÆB&RVF—F&ÆR"“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•¶f—'7E÷&÷WF–æuöWfVçEÒç–ÆöB‚’Âe³Ò“°¢76W'EöW€¢Fö7VÖVçBæÖ—†W%÷6VæE÷7VÖÖ&–W2‚’çVçw&‚•³ÒæVæ&ÆVB‚’À¢6öÖR‡G'VR¢“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVB&ö¦V7B&÷WF–ær6†÷VÆBVæ6öFRÆ÷76ÆW76Ç’"“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&÷WF–ær6†÷VÆB&W'6R"“°¢76W'EöW€¢&÷VæE÷G&—æÖ—†W%÷6VæE÷7VÖÖ&–W2‚’çVçw&‚•³ÒæVæ&ÆVB‚’À¢6öÖR‡G'VR¢“°¢Ð ¢5·FW7EÐ¢fâVF—F–æuöÖ—†W%÷&ÖWFW%÷fÇVU÷&W6W'fW5÷&V6÷&EöæEöWfVçEö'—FW2‚’°¢ÆWB×WB–ÆöBÒfV2³„Â„$"Â„42Â„DBÂ“"ÂÓ°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚cƒC÷SbçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c%óƒö“3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂƒ#"Âƒ32ÂƒCBÂ“2ÂuÒ“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚cƒƒ÷SbçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚b‚ÓcCö“3"’çFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„SÂƒ“‚ÂƒÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚g–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S‚Âe³„RÂƒTÒ“° ¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWBWfVçEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„S¢æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB6öçF–âÖ—†W"&ÖWFW'2"“°¢ÆWB÷F†W%öWfVçEö–æFW‚ÒWfVçEö–æFW‚²°¢ÆWB÷F†W%öWfVçBÒFö7VÖVçBæWfVçG2‚•¶÷F†W%öWfVçEö–æFW…Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWB×WBW‡V7FVEöÖ—†W%öWfVçBÒFö7VÖVçBæWfVçG2‚•¶WfVçEö–æFW…Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢W‡V7FVEöÖ—†W%öWfVçE³âãUÒæ6÷•ög&öÕ÷6Æ–6R‚cóö“3"çFõöÆUö'—FW2‚’“°¢Fö7VÖVç@¢ç6WEöÖ—†W%÷&ÖWFW%÷&V6÷&E÷fÇVR†WfVçEö–æFW‚ÂÂó¢æW‡V7B‚&âW†—7F–ær&ÖWFW"fÇVR6†÷VÆB&RVF—F&ÆR"“° ¢ÆWB&V6÷&G2ÒFö7VÖVç@¢æÖ—†W%÷&ÖWFW%÷&V6÷&G2‚¢æW‡V7B‚'F†R&ÖWFW"&V6÷&G26†÷VÆB&VÖ–âfÆ–B"“°¢76W'EöW‡&V6÷&G5³ÒçfÇVR‚’Âó“°¢76W'EöW‡&V6÷&G5³ÒçfÇVR‚’ÂÓeóC“°¢76W'EöW‡&V6÷&G5³Òç&Vf—‚‚’Â³„Â„$"Â„42Â„DEÒ“°¢76W'EöW‡&V6÷&G5³Òæ6†ææVÅöFF‚’ÂƒC“°¢76W'EöW‡&V6÷&G5³Òç&Vf—‚‚’Â³ƒÂƒ#"Âƒ32ÂƒCEÒ“°¢76W'EöW‡&V6÷&G5³Òæ6†ææVÅöFF‚’Âƒƒ“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•¶WfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢W‡V7FVEöÖ—†W%öWfVç@¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•¶WfVçEö–æFW…ÒæVæ6öF–ær‚’À¢e–ÆöDVæ6öF–æs£¤FF°¢ÆVæwF…÷&Vf—ƒ¢fV2³ƒ“‚ÂƒÐ¢Ð¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•¶÷F†W%öWfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢÷F†W%öWfVç@¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&÷VæE÷G&—æÖ—†W%÷&ÖWFW%÷&V6÷&G2‚’çVçw&‚•³ÒçfÇVR‚’À¢ó ¢“°¢Ð ¢5·FW7EÐ¢fâÖ—†W%÷&ÖWFW%÷fÇVUöVF—E÷&V¦V7G5ö–çfÆ–E÷F&vWG5÷v—F†÷WEö×WFF–öâ‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒÂƒÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„SÂe³²%Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'F†Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÖ—†W%÷&ÖWFW%÷&V6÷&E÷fÇVRƒÂÂ#2’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÖ—†W%÷&ÖWFW%÷&V6÷&E÷fÇVRƒÂÂ#2’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW€¢Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"’À¢&Vf÷&P¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷g7Eö–FVçF—G•öf–VÆG5öæEö6Æ75÷V–Eög&öÕöCUöVçfVÆ÷R‚’°¢ÆWB×WB–ÆöBÒ'S3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂSÂe³²eÒ“°¢VæE÷g7Eöf–VÆB€¢f×WB–ÆöBÀ¢S"À¢e°¢„DbÂƒSRÂƒCrÂƒ3"Â„DbÂƒ„bÂƒƒ‚ÂƒCrÂ„#BÂ„4RÂƒT"ÂƒsÂ„‚Âƒ2À¢ƒtRÂ„3BÀ¢ÒÀ¢“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂSBÂ"%¤TäôÄôu’"“°¢VæE÷g7Eöf–VÆB€¢f×WB–ÆöBÀ¢SRÀ¢""ôÆ–'&'’ôVF–òõÇVrÔ–ç2õe5C2õ&öÆæBõ¤TäôÄôu’çg7C2"À¢“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂSbÂ"%&öÆæB6Æ÷VB"“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂS2Âe³Â"Â2ÂEÒ“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂ““’Âe³RÂeÒ“° ¢ÆWBÖWFFFÒ'6U÷g7E÷ÇVv–å÷7FFUöÖWFFF‚g–ÆöB¢æW‡V7B‚'F†Re5BVçfVÆ÷R6†÷VÆB&R&V6övæ—¦VB"“° ¢76W'EöW†ÖWFFFæf÷&ÖEöÖ&¶W"‚’Â"“°¢76W'EöW†ÖWFFFææÖR‚’Â6öÖR‚%¤TäôÄôu’"’“°¢76W'EöW†ÖWFFFçfVæF÷"‚’Â6öÖR‚%&öÆæB6Æ÷VB"’“°¢76W'EöW€¢ÖWFFFçF‚‚’À¢6öÖR‚"ôÆ–'&'’ôVF–òõÇVrÔ–ç2õe5C2õ&öÆæBõ¤TäôÄôu’çg7C2"¢“°¢76W'EöW†ÖWFFFç7FFUö'—FW2‚’Â6öÖRƒB’“°¢76W'EöW€¢ÖWFFFæ6Æ75÷V–B‚’æ5öFW&Vb‚’À¢6öÖR‚#3#CsSTDc„dDcCsƒ„#D4ST#sƒ3tT3B"¢“°¢ÆWB7FFRÒ7WW#£¤6†ææVÅÇVv–å7FFR°¢6†ææVÅö–C¢À¢ÇVv–åö–FVçF–f–W#¢æöæRÀ¢F—7Æ•öæÖS¢æöæRÀ¢w&W%÷–ÆöC¢æöæRÀ¢FF÷–ÆöC¢–ÆöBæ6ÆöæR‚’À¢FFöWfVçEö–æFWƒ¢À¢g7EöÖWFFF¢6öÖR†ÖWFFF’À¢Ó°¢76W'EöW‡7FFRçg7E÷7FFUö'—FW2‚’Â6öÖR‚e³Â"Â2ÂEÕ²âåÒ’“°¢Ð ¢fâÖ&¶W%ó%÷g7C5÷7FFUöf—‡GW&R‚’ÓâfV3ÇSƒâ°¢ÆWB×WBæW7FVBÒS3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚c7S3"çFõöÆUö'—FW2‚’“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚c'ScBçFõöÆUö'—FW2‚’“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂƒ#Ò“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚c'S3"çFõöÆUö'—FW2‚’“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚cScBçFõöÆUö'—FW2‚’“°¢æW7FVBçW6‚ƒƒSR“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚cGS3"çFõöÆUö'—FW2‚’“°¢æW7FVBæW‡FVæEög&öÕ÷6Æ–6R‚cScBçFõöÆUö'—FW2‚’“°¢æW7FVBçW6‚ƒ„“° ¢ÆWB×WB–ÆöBÒ'S3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂSÂe³²eÒ“°¢VæE÷g7Eöf–VÆB€¢f×WB–ÆöBÀ¢S"À¢e°¢„DbÂƒSRÂƒCrÂƒ3"Â„DbÂƒ„bÂƒƒ‚ÂƒCrÂ„#BÂ„4RÂƒT"ÂƒsÂ„‚Âƒ2À¢ƒtRÂ„3BÀ¢ÒÀ¢“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂS2ÂfæW7FVB“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂSBÂ"%FW7Be5C2"“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂ““’Âe³ƒ“’Âƒ“…Ò“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ%Ó°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CBÂe³„CBÂ„UÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂg–ÆöB“°¢fÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„CÂ„C%Ò¢Ð ¢5·FW7EÐ¢fâ6†ææVÅ÷&W6WEöÖF6†W5ööæÇ•÷F†U÷6ÖUöÖ&¶W%ó%÷g7C5ö6Æ72‚’°¢ÆWB&ö¦V7Eö'—FW2ÒÖ&¶W%ó%÷g7C5÷7FFUöf—‡GW&R‚“°¢ÆWBF&vWEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'F&vWB&ö¦V7B6†÷VÆB'6R"“°¢ÆWBF&vWE÷7FFRÒF&vWEöFö7VÖVçBæ6†ææVÅ÷ÇVv–å÷7FFW2‚•³Òæ6ÆöæR‚“° ¢ÆWB×WB&W6WEö'—FW2Ò&ö¦V7Eö'—FW3°¢&W6WEö'—FW5³‚âãÒæ6÷•ög&öÕ÷6Æ–6R‚c3%÷SbçFõöÆUö'—FW2‚’“°¢ÆWB&W6WBÒg7E&W6WC£§'6R‚g&W6WEö'—FW2’æW‡V7B‚&6†ææVÂ&W6WB6†÷VÆB'6R"“°¢76W'EöW‡&W6WBæ¶–æB‚’Âg7E&W6WD¶–æC£¤6†ææVÅ7FFR“° ¢ÆWBÖF6†–æu÷7FFRÒ&W6W@¢æÖF6†–æu÷g7C5ö6†ææVÅ÷7FFR‚gF&vWE÷7FFR¢æW‡V7B‚&ÖF6†–ærÖ&¶W"Ó"e5C27FFR6†÷VÆB&Rf÷VæB"“°¢76W'EöW€¢ÖF6†–æu÷7FFP¢çg7EöÖWFFF‚¢ææE÷F†Vâ‡ÆÖWFFFÂÖWFFFæ6Æ75÷V–B‚’’À¢F&vWE÷7FFP¢çg7EöÖWFFF‚¢ææE÷F†Vâ‡ÆÖWFFFÂÖWFFFæ6Æ75÷V–B‚’¢“°¢76W'EöW€¢ÖF6†–æu÷7FFRçg7E÷7FFUö'—FW2‚’À¢F&vWE÷7FFRçg7E÷7FFUö'—FW2‚¢“° ¢ÆWB×WBF–ffW&VçEö6Æ75÷7FFRÒF&vWE÷7FFRæ6ÆöæR‚“°¢F–ffW&VçEö6Æ75÷7FFP¢çg7EöÖWFFF¢æ5ö×WB‚¢æW‡V7B‚&f—‡GW&R6†÷VÆB6öçF–âe5BÖWFFF"¢æwV–BÒ6öÖR‡fV2³²eÒ“°¢76W'B€¢&W6W@¢æÖF6†–æu÷g7C5ö6†ææVÅ÷7FFR‚fF–ffW&VçEö6Æ75÷7FFR¢æ—5öæöæR‚¢“° ¢ÆWB×WBVç7W÷'FVEöÖ&¶W%÷7FFRÒF&vWE÷7FFRæ6ÆöæR‚“°¢Vç7W÷'FVEöÖ&¶W%÷7FFP¢çg7EöÖWFFF¢æ5ö×WB‚¢æW‡V7B‚&f—‡GW&R6†÷VÆB6öçF–âe5BÖWFFF"¢æf÷&ÖEöÖ&¶W"Òƒ°¢76W'B€¢&W6W@¢æÖF6†–æu÷g7C5ö6†ææVÅ÷7FFR‚gVç7W÷'FVEöÖ&¶W%÷7FFR¢æ—5öæöæR‚¢“° ¢ÆWB×WBw&öæuö¶–æEö'—FW2Ò&W6WEö'—FW3°¢w&öæuö¶–æEö'—FW5³‚âãÒæ6÷•ög&öÕ÷6Æ–6R‚cC•÷SbçFõöÆUö'—FW2‚’“°¢ÆWBw&öæuö¶–æBÐ¢g7E&W6WC£§'6R‚gw&öæuö¶–æEö'—FW2’æW‡V7B‚%e5BvVæW&F÷"&W6WB6†÷VÆB'6R"“°¢76W'EöW‡w&öæuö¶–æBæ¶–æB‚’Âg7E&W6WD¶–æC£¥g7DvVæW&F÷%7FFR“°¢76W'B€¢w&öæuö¶–æ@¢æÖF6†–æu÷g7C5ö6†ææVÅ÷7FFR‚gF&vWE÷7FFR¢æ—5öæöæR‚¢“°¢Ð ¢5·FW7EÐ¢fâ&WÆ6W5÷g7C5ö6†ææVÅ÷7FFUöæE÷&W6W'fW5öÆÅö÷F†W%ö'—FW2‚’°¢ÆWB–çWBÒÖ&¶W%ó%÷g7C5÷7FFUöf—‡GW&R‚“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅ÷7FFRÒFö7VÖVç@¢æ6†ææVÅ÷ÇVv–å÷7FFW2‚¢æ–çFõö—FW"‚¢ææW‡B‚¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fRe5B7FFR"“°¢ÆWB÷&–v–æÅ÷–ÆöBÒ÷&–v–æÅ÷7FFRæFF÷–ÆöB‚’çFõ÷fV2‚“°¢ÆWB÷&–v–æÅ÷w&W"Ò÷&–v–æÅ÷7FFRçw&W%÷–ÆöB‚’æÖƒÅ·S…Óã£§Fõ÷fV2“°¢ÆWB÷&–v–æÅöWfVçG2ÒFö7VÖVçBæWfVçG2‚’æÆVâ‚“°¢ÆWB×WB&WÆ6VÖVçBÒS3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚c7S3"çFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚c7ScBçFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒ#Âƒ#"Âƒ#5Ò“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚c'S3"çFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚cScBçFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBçW6‚ƒƒSR“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚cGS3"çFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚c'ScBçFõöÆUö'—FW2‚’“°¢&WÆ6VÖVçBæW‡FVæEög&öÕ÷6Æ–6R‚e³„Â„%Ò“° ¢Fö7VÖVç@¢ç&WÆ6U÷g7C5ö6†ææVÅ÷7FFUö'—FW2ƒrÂg&WÆ6VÖVçB¢æW‡V7B‚&Ö&¶W"Ó"e5C27FFR6†÷VÆB&R&WÆ6V&ÆR"“°¢ÆWB7FFRÒFö7VÖVç@¢æ6†ææVÅ÷ÇVv–å÷7FFW2‚¢æ–çFõö—FW"‚¢ææW‡B‚¢æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB&WF–âe5B7FFR"“°¢76W'EöW‡7FFRçg7E÷7FFUö'—FW2‚’Â6öÖR‡&WÆ6VÖVçBæ5÷6Æ–6R‚’’“°¢76W'EöW‡7FFRçw&W%÷–ÆöB‚’Â÷&–v–æÅ÷w&W"æ5öFW&Vb‚’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚’æÆVâ‚’Â÷&–v–æÅöWfVçG2“° ¢ÆWBWFFVE÷–ÆöBÒ7FFRæFF÷–ÆöB‚“°¢ÆWB÷&–v–æÅ÷&ævRÒ÷&–v–æÅ÷7FFP¢çg7EöÖWFFF‚¢çVçw&‚¢ç7FFUöFF÷&ævP¢æ6ÆöæR‚¢çVçw&‚“°¢ÆWBWFFVEöÖWFFFÒ7FFP¢çg7EöÖWFFF‚¢æW‡V7B‚'WFFVBVçfVÆ÷R6†÷VÆB&VÖ–â&V6övæ—¦&ÆR"“°¢76W'EöW‡WFFVEöÖWFFFææÖR‚’Â6öÖR‚%FW7Be5C2"’“°¢76W'EöW€¢WFFVEöÖWFFFæ6Æ75÷V–B‚’À¢÷&–v–æÅ÷7FFRçg7EöÖWFFF‚’çVçw&‚’æ6Æ75÷V–B‚¢“°¢ÆWBWFFVE÷&ævRÒWFFVEöÖWFFFç7FFUöFF÷&ævRæ5÷&Vb‚’çVçw&‚“°¢ÆWB÷&–v–æÅöf–VÆE÷7F'BÒ÷&–v–æÅ÷&ævRç7F'BÒ#°¢ÆWBWFFVEöf–VÆE÷7F'BÒWFFVE÷&ævRç7F'BÒ#°¢76W'EöW€¢gWFFVE÷–ÆöE²âçWFFVEöf–VÆE÷7F'EÒÀ¢f÷&–v–æÅ÷–ÆöE²âæ÷&–v–æÅöf–VÆE÷7F'EÐ¢“°¢76W'EöW€¢gWFFVE÷–ÆöE·WFFVE÷&ævRæVæBâåÒÀ¢f÷&–v–æÅ÷–ÆöE¶÷&–v–æÅ÷&ævRæVæBâåÐ¢“°¢ÆWB÷&–v–æÅ÷Væ¶æ÷våöf–VÆBÒ÷&–v–æÅ÷–Æö@¢çv–æF÷w2ƒB¢ç÷6—F–öâ‡Æ'—FW7Â'—FW2ÓÒ““—S3"çFõöÆUö'—FW2‚’¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fRâVæ¶æ÷vâf–VÆB"“°¢ÆWBWFFVE÷Væ¶æ÷våöf–VÆBÒWFFVE÷–Æö@¢çv–æF÷w2ƒB¢ç÷6—F–öâ‡Æ'—FW7Â'—FW2ÓÒ““—S3"çFõöÆUö'—FW2‚’¢æW‡V7B‚'Væ¶æ÷vâf–VÆB6†÷VÆB&R&WF–æVB"“°¢76W'EöW€¢WFFVE÷–ÆöE·WFFVE÷Væ¶æ÷våöf–VÆBâåÒÀ¢÷&–v–æÅ÷–ÆöE¶÷&–v–æÅ÷Væ¶æ÷våöf–VÆBâåÐ¢“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB&W'6R"“°¢76W'EöW€¢&W'6VBæ6†ææVÅ÷ÇVv–å÷7FFW2‚•³Òçg7E÷7FFUö'—FW2‚’À¢6öÖR‡&WÆ6VÖVçBæ5÷6Æ–6R‚’¢“°¢76W'EöW‡&W'6VBçG&–Æ–æuö'—FW2‚’Âe³„CÂ„C%Ò“°¢Ð ¢5·FW7EÐ¢fâg7C5ö6†ææVÅ÷7FFU÷&WÆ6VÖVçE÷&V¦V7G5öÖ&–wV÷W5öf–VÆG5÷v—F†÷WEö×WFF–öâ‚’°¢ÆWB×WB–çWBÒÖ&¶W%ó%÷g7C5÷7FFUöf—‡GW&R‚“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Fö7VÖVçBç&WÆ6U÷g7C5ö6†ææVÅ÷7FFUö'—FW2ƒ“’Âe³Ò’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“° ¢ÆWB×WB–ÆöBÒFö7VÖVçBæ6†ææVÅ÷ÇVv–å÷7FFW2‚•³ÒæFF÷–ÆöB‚’çFõ÷fV2‚“°¢VæE÷g7Eöf–VÆB‚f×WB–ÆöBÂS2Âe³ƒceÒ“°¢ÆWB×WBGWÆ–6FU÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ%Ó°¢VæEöFFöWfVçB‚f×WBGWÆ–6FU÷7G&VÒÂ„CRÂg–ÆöB“°¢–çWBÒfÇöf—‡GW&R‚fGWÆ–6FU÷7G&VÒÂeµÒÂeµÒ“°¢Fö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&GWÆ–6FRÖf–VÆBf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Fö7VÖVçBç&WÆ6U÷g7C5ö6†ææVÅ÷7FFUö'—FW2ƒrÂe³Ò’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ–væ÷&W5÷Vç7W÷'FVEö÷%÷G'Væ6FVE÷g7EöVçfVÆ÷W2‚’°¢76W'B‡'6U÷g7E÷ÇVv–å÷7FFUöÖWFFF‚cC'S3"çFõöÆUö'—FW2‚’’æ—5öæöæR‚’“° ¢ÆWB×WBG'Væ6FVBÒ'S3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢G'Væ6FVBæW‡FVæEög&öÕ÷6Æ–6R‚cSGS3"çFõöÆUö'—FW2‚’“°¢G'Væ6FVBæW‡FVæEög&öÕ÷6Æ–6R‚c‡ScBçFõöÆUö'—FW2‚’“°¢G'Væ6FVBçW6‚†"u‚r“°¢76W'B‡'6U÷g7E÷ÇVv–å÷7FFUöÖWFFF‚gG'Væ6FVB’æ—5öæöæR‚’“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%öæEöVF–õö6†ææVÅ÷6×ÆU÷F‡5öÆ÷76ÆW76Ç’‚’°¢ÆWBF‚Ò""TdÅ7GVF–ôf7F÷'”FFUÄFFÅF6†W5Å6÷VæG5Çfö–6Rçvb#°¢f÷"¶–æB–â³ÂEÒ°¢ÆWB–çWBÒ6†ææVÅ÷v—F…÷6×ÆU÷F…öf—‡GW&R†¶–æBÂF‚“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“° ¢76W'EöW†6†ææVÇ2æÆVâ‚’Â“°¢76W'EöW†6†ææVÇ5³Òç6×ÆU÷F‚‚’Â6öÖR‡F‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%÷föÇVÖUöVçfVÆ÷UöVæ&ÆUöfÆuög&öÕö÷&FW&VEöVçfVÆ÷U÷&V6÷&G2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢òò„D&V6÷&G2&R÷&FW&VBææ–ærÂföÇVÖRÂÖöB‚ÂÖöB’Â—F6‚à¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„DÂ‚ÂÂÂÂÂÂÂÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„DÂ‚ÂÂÂÂÂÂÂÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%÷föÇVÖUöVçfVÆ÷UöVæ&ÆVB‚’À¢6öÖR‡G'VR¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢ÆWB×WBVF–õö6†ææVÂÒWfVçE÷7G&VÓ°¢VF–õö6†ææVÅ³EÒÒC°¢ÆWBVF–õö–çWBÒfÇöf—‡GW&R‚fVF–õö6†ææVÂÂeµÒÂeµÒ“°¢ÆWBVF–õöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fVF–õö–çWB’æW‡V7B‚&VF–ò6Æ—6†÷VÆB'6R"“°¢76W'EöW€¢VF–õöFö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%÷föÇVÖUöVçfVÆ÷UöVæ&ÆVB‚’À¢æöæP¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%÷föÇVÖUöVçfVÆ÷U÷&uöf–VÆG2‚’°¢ÆWB×WBföÇVÖRÒfV2³÷Sƒ²seÓ°¢föÇVÖU³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒ&%ö“3"çFõöÆUö'—FW2‚’“°¢föÇVÖU³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢f÷"†öfg6WBÂfÇVR’–â°¢ƒ…÷W6—¦RÂö“3"’À¢ƒ"Â#"’À¢ƒbÂ32’À¢ƒ#ÂCB’À¢ƒ#BÂcB’À¢ƒ#‚ÂSR’À¢ƒ3"ÂÓ#’À¢ƒSbÂÓcB’À¢ƒcÂ’À¢ƒcBÂ#’À¢Ò°¢föÇVÖU¶öfg6WBâæöfg6WB²EÒæ6÷•ög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢Ð¢föÇVÖU³3bâãSeÒæ6÷•ög&öÕ÷6Æ–6R‚e³ƒV²#Ò“°¢föÇVÖU³s"âãseÒæ6÷•ög&öÕ÷6Æ–6R‚e³„Â„#"Â„32Â„CEÒ“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂe³²s%Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂgföÇVÖR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÂÒFö7VÖVçBæ6†ææVÇ2‚’ç&VÖ÷fRƒ“°¢ÆWBVçfVÆ÷RÒ6†ææVÀ¢ç6×ÆW%÷föÇVÖUöVçfVÆ÷R‚¢æW‡V7B‚%föÇVÖRVçfVÆ÷R6†÷VÆB&RFV6öFVB"“° ¢76W'EöW†VçfVÆ÷RæfÆw5÷&r‚’Âƒ&"“°¢76W'B†VçfVÆ÷RæVæ&ÆVB‚’“°¢76W'B†VçfVÆ÷RçFV×õ÷7–æ5öVæ&ÆVB‚’“°¢76W'EöW†VçfVÆ÷Rç&VFVÆ•÷&r‚’Â6öÖRƒ’“°¢76W'EöW†VçfVÆ÷RæGF6µ÷&r‚’Â6öÖRƒ#"’“°¢76W'EöW†VçfVÆ÷Ræ†öÆE÷&r‚’Â6öÖRƒ32’“°¢76W'EöW†VçfVÆ÷RæFV6•÷&r‚’Â6öÖRƒCB’“°¢76W'EöW†VçfVÆ÷Rç7W7F–å÷&r‚’Â6öÖRƒcB’“°¢76W'EöW†VçfVÆ÷Rç&VÆV6U÷&r‚’Â6öÖRƒSR’“°¢76W'EöW†VçfVÆ÷RæÖ÷VçE÷&r‚’Â6öÖR‚Ó#’“°¢76W'EöW†VçfVÆ÷RæGF6µ÷FVç6–öå÷&r‚’Â6öÖR‚ÓcB’“°¢76W'EöW†VçfVÆ÷RæFV6•÷FVç6–öå÷&r‚’Â6öÖRƒ’“°¢76W'EöW†VçfVÆ÷Rç&VÆV6U÷FVç6–öå÷&r‚’Â6öÖRƒ#’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâ6×ÆW%÷föÇVÖUöVçfVÆ÷UöVF—E÷&W6W'fW5öÆfõöæE÷G&–Æ–æuö'—FW2‚’°¢ÆWB×WBföÇVÖRÒfV2³÷Sƒ²seÓ°¢föÇVÖU³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒ&%ö“3"çFõöÆUö'—FW2‚’“°¢föÇVÖU³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢f÷"†öfg6WBÂfÇVR’–âƒ3bâãSb’ç7FWö'’ƒB’ç¦—ƒö“3"âãÓR’°¢föÇVÖU¶öfg6WBâæöfg6WB²EÒæ6÷•ög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢Ð¢föÇVÖU³s"âãseÒæ6÷•ög&öÕ÷6Æ–6R‚e³„Â„#"Â„32Â„CEÒ“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂe³²s%Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂgföÇVÖR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWBföÇVÖUöWfVçEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æVçVÖW&FR‚¢æf–ÇFW"‡Â…òÂWfVçB—ÂWfVçBæ÷6öFR‚’ÓÒ„D¢æçF‚ƒ¢æÖ‡Â†–æFW‚Âò—Â–æFW‚¢æW‡V7B‚%föÇVÖRVçfVÆ÷RWfVçB6†÷VÆBW†—7B"“°¢ÆWB&Vf÷&U÷–ÆöBÒFö7VÖVçBæWfVçG2‚•·föÇVÖUöWfVçEö–æFW…Òç–ÆöB‚’çFõ÷fV2‚“° ¢Fö7VÖVç@¢æVF—E÷6×ÆW%÷föÇVÖUöVçfVÆ÷R€¢rÀ¢6×ÆW%föÇVÖTVçfVÆ÷TVF—B°¢Væ&ÆVC¢6öÖR†fÇ6R’À¢FV×õ÷7–æ3¢6öÖR†fÇ6R’À¢GF6µ÷&s¢6öÖRƒó#3B’À¢GF6µ÷FVç6–öå÷&s¢6öÖRƒC"’À¢âå6×ÆW%föÇVÖTVçfVÆ÷TVF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'&V6övæ—¦VBf–VÆG26†÷VÆB&RVF—F&ÆR"“° ¢ÆWB×WBW‡V7FVE÷–ÆöBÒ&Vf÷&U÷–ÆöC°¢W‡V7FVE÷–ÆöE³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒ&ö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷–ÆöE³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷–ÆöE³"âãeÒæ6÷•ög&öÕ÷6Æ–6R‚có#3Eö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷–ÆöE³SbâãcÒæ6÷•ög&öÕ÷6Æ–6R‚cC%ö“3"çFõöÆUö'—FW2‚’“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·föÇVÖUöWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷–Æö@¢“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB'6R"“°¢ÆWBVçfVÆ÷RÒ&W'6VBæ6†ææVÇ2‚•³Ð¢ç6×ÆW%÷föÇVÖUöVçfVÆ÷R‚¢æW‡V7B‚&VF—FVBVçfVÆ÷R6†÷VÆB&RFV6öFVB"“°¢76W'B‚VçfVÆ÷RæVæ&ÆVB‚’“°¢76W'B‚VçfVÆ÷RçFV×õ÷7–æ5öVæ&ÆVB‚’“°¢76W'EöW†VçfVÆ÷RæGF6µ÷&r‚’Â6öÖRƒó#3B’“°¢76W'EöW†VçfVÆ÷RæGF6µ÷FVç6–öå÷&r‚’Â6öÖRƒC"’“°¢76W'EöW€¢&W'6VBæWfVçG2‚•·föÇVÖUöWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷–Æö@¢“°¢Ð ¢5·FW7EÐ¢fâ6×ÆW%÷föÇVÖUöVçfVÆ÷Uö–çfÆ–EöVF—Eö—5öFöÖ–2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂe³²s%Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂe³²cEÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—E÷6×ÆW%÷föÇVÖUöVçfVÆ÷R€¢rÀ¢6×ÆW%föÇVÖTVçfVÆ÷TVF—B°¢GF6µ÷&s¢6öÖRƒ“’’À¢âå6×ÆW%föÇVÖTVçfVÆ÷TVF—C£¦FVfVÇB‚¢Ð¢’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—E÷6×ÆW%÷föÇVÖUöVçfVÆ÷R€¢rÀ¢6×ÆW%föÇVÖTVçfVÆ÷TVF—B°¢&VÆV6U÷FVç6–öå÷&s¢6öÖRƒ’À¢âå6×ÆW%föÇVÖTVçfVÆ÷TVF—C£¦FVfVÇB‚¢Ð¢’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öÆÅöf—fU÷6×ÆW%öVçfVÆ÷UöÆfõ÷F&vWG5ö–å÷&V6÷&Eö÷&FW"‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢f÷"†–æFW‚ÂF&vWB’–â6×ÆW$VçfVÆ÷TÆfõF&vWC£¦ÆÂ‚’æ–çFõö—FW"‚’æVçVÖW&FR‚’°¢ÆWB×WB–ÆöBÒfV2³÷Sƒ²s%Ó°¢ÆWBfÆw2Ò†–æFW‚2“3"’ÃÂ°¢–ÆöE³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚ffÆw2çFõöÆUö'—FW2‚’“°¢–ÆöE³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚b†–æFW‚2“3"’çFõöÆUö'—FW2‚’“°¢–ÆöE³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚bƒ²–æFW‚2“3"’çFõöÆUö'—FW2‚’“°¢–ÆöE³CBâãC…Òæ6÷•ög&öÕ÷6Æ–6R‚b†–æFW‚2“3"Ò"’çFõöÆUö'—FW2‚’“°¢–ÆöE³C‚âãS%Òæ6÷•ög&öÕ÷6Æ–6R‚bƒ#²–æFW‚2S3"’çFõöÆUö'—FW2‚’“°¢–ÆöE³S"âãSeÒæ6÷•ög&öÕ÷6Æ–6R‚b†–æFW‚2“3"R2’çFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂg–ÆöB“°¢76W'EöW‡F&vWBæ–æFW‚‚’Â–æFW‚“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÂÒFö7VÖVçBæ6†ææVÇ2‚’ç&VÖ÷fRƒ“°¢ÆWB&V6÷&G2Ò6†ææVÀ¢ç6×ÆW%öVçfVÆ÷UöÆf÷2‚¢æW‡V7B‚%6×ÆW"VçfVÆ÷R&V6÷&G26†÷VÆB&RW‡÷6VB"“° ¢f÷"†–æFW‚ÂF&vWB’–â6×ÆW$VçfVÆ÷TÆfõF&vWC£¦ÆÂ‚’æ–çFõö—FW"‚’æVçVÖW&FR‚’°¢ÆWB&V6÷&BÒ&V6÷&G5¶–æFW…ÒæW‡V7B‚&V6‚F&vWB&V6÷&B6†÷VÆBFV6öFR"“°¢76W'EöW‡&V6÷&BçF&vWB‚’ÂF&vWB“°¢76W'EöW‡&V6÷&BæfÆw5÷&r‚’Â†–æFW‚2“3"’ÃÂ“°¢76W'EöW‡&V6÷&BæVæ&ÆVB‚’Â–æFW‚Ò“°¢76W'EöW‡&V6÷&BæÆfõ÷FV×õ÷7–æ5öVæ&ÆVB‚’Â–æFW‚R"ÓÒ“°¢76W'EöW‡&V6÷&Bç&VFVÆ•÷&r‚’Â6öÖRƒ²–æFW‚2“3"’“°¢76W'EöW‡&V6÷&BæÆfõöÖ÷VçE÷&r‚’Â6öÖR†–æFW‚2“3"Ò"’“°¢76W'EöW‡&V6÷&BæÆfõ÷7VVE÷&r‚’Â6öÖRƒ#²–æFW‚2S3"’“°¢76W'EöW‡&V6÷&BæÆfõ÷6†U÷&r‚’Â6öÖR†–æFW‚2“3"R2’“°¢76W'EöW†6†ææVÂç6×ÆW%öVçfVÆ÷UöÆfò‡F&vWB’Â6öÖR‡&V6÷&B’“°¢Ð¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâ6×ÆW%öVçfVÆ÷UöÆfõöVF—G5÷&W6W'fU÷Vç6VÆV7FVEöf–VÆG5öf÷%öWfW'•÷F&vWB‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢ÆWB×WB÷&–v–æÅ÷–ÆöG2ÒfV3£¦æWr‚“°¢f÷"ò–â6×ÆW$VçfVÆ÷TÆfõF&vWC£¦ÆÂ‚’°¢ÆWB×WB–ÆöBÒfV2³÷Sƒ²seÓ°¢–ÆöE³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒSEö“3"çFõöÆUö'—FW2‚’“°¢–ÆöE³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢f÷"†öfg6WBÂfÇVR’–âƒ‚âãÓcB’ç7FWö'’ƒB’ç¦—ƒö“3"âãÓ#2’°¢–ÆöE¶öfg6WBâæöfg6WB²EÒæ6÷•ög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢Ð¢–ÆöE³s"âãseÒæ6÷•ög&öÕ÷6Æ–6R‚e³„Â„#"Â„32Â„CEÒ“°¢÷&–v–æÅ÷–ÆöG2çW6‚‡–ÆöBæ6ÆöæR‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DÂg–ÆöB“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&VçfVÆ÷Rf—‡GW&R6†÷VÆB'6R"“°¢ÆWBWfVçEö–æF–6W2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æVçVÖW&FR‚¢æf–ÇFW"‡Â…òÂWfVçB—ÂWfVçBæ÷6öFR‚’ÓÒ„D¢æÖ‡Â†–æFW‚Âò—Â–æFW‚¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢f÷"†–æFW‚ÂF&vWB’–â6×ÆW$VçfVÆ÷TÆfõF&vWC£¦ÆÂ‚’æ–çFõö—FW"‚’æVçVÖW&FR‚’°¢Fö7VÖVç@¢æVF—E÷6×ÆW%öVçfVÆ÷UöÆfò€¢rÀ¢F&vWBÀ¢6×ÆW$VçfVÆ÷TÆfôVF—B°¢Væ&ÆVC¢6öÖR‡G'VR’À¢FV×õ÷7–æ3¢6öÖR‡G'VR’À¢Æfõ÷FV×õ÷7–æ3¢6öÖR†fÇ6R’À¢Æfõ÷†6U÷&WG&–vvW#¢6öÖR‡G'VR’À¢GF6µ÷&s¢6öÖRƒó#3B²–æFW‚2“3"’À¢Æfõ÷7VVE÷&s¢6öÖRƒ%ó²–æFW‚2S3"’À¢Æfõ÷6†U÷&s¢6öÖR†–æFW‚2“3"R2’À¢âå6×ÆW$VçfVÆ÷TÆfôVF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&WfW'’÷&FW&VBF&vWB6†÷VÆB&RVF—F&ÆR"“° ¢ÆWB×WBW‡V7FVBÒ÷&–v–æÅ÷–ÆöG5¶–æFW…Òæ6ÆöæR‚“°¢W‡V7FVE³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒsUö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE³"âãeÒæ6÷•ög&öÕ÷6Æ–6R‚bƒó#3B²–æFW‚2“3"’çFõöÆUö'—FW2‚’“°¢W‡V7FVE³C‚âãS%Òæ6÷•ög&öÕ÷6Æ–6R‚bƒ%ó²–æFW‚2S3"’çFõöÆUö'—FW2‚’“°¢W‡V7FVE³S"âãSeÒæ6÷•ög&öÕ÷6Æ–6R‚b†–æFW‚2“3"R2’çFõöÆUö'—FW2‚’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•¶WfVçEö–æF–6W5¶–æFW…ÕÒç–ÆöB‚’ÂW‡V7FVB“°¢Ð ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB'6R"“°¢ÆWB&V6÷&G2Ò&W'6VBæ6†ææVÇ2‚•³Òç6×ÆW%öVçfVÆ÷UöÆf÷2‚’çVçw&‚“°¢f÷"†–æFW‚Â&V6÷&B’–â&V6÷&G2æ–çFõö—FW"‚’æVçVÖW&FR‚’°¢ÆWB&V6÷&BÒ&V6÷&BæW‡V7B‚&VF—FVBF&vWB6†÷VÆB7F–ÆÂFV6öFR"“°¢76W'B‡&V6÷&BæVæ&ÆVB‚’“°¢76W'B‡&V6÷&BçFV×õ÷7–æ5öVæ&ÆVB‚’“°¢76W'B‡&V6÷&BæÆfõ÷†6U÷&WG&–vvW%öVæ&ÆVB‚’“°¢76W'B‚&V6÷&BæÆfõ÷FV×õ÷7–æ5öVæ&ÆVB‚’“°¢76W'EöW‡&V6÷&BæGF6µ÷&r‚’Â6öÖRƒó#3B²–æFW‚2“3"’“°¢76W'EöW‡&V6÷&BæÆfõ÷7VVE÷&r‚’Â6öÖRƒ%ó²–æFW‚2S3"’“°¢76W'EöW‡&V6÷&BæÆfõ÷6†U÷&r‚’Â6öÖR†–æFW‚2“3"R2’“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%öÖ…÷öÇ—†öç•öæEöÖöæõ÷6WGF–æw5öÆ÷76ÆW76Ç’‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢ÆWB×WBöÇ—†öç’ÒfV3£¦æWr‚“°¢öÇ—†öç’æW‡FVæEög&öÕ÷6Æ–6R‚c5÷S3"çFõöÆUö'—FW2‚’“°¢öÇ—†öç’æW‡FVæEög&öÕ÷6Æ–6R‚c#S÷S3"çFõöÆUö'—FW2‚’“°¢öÇ—†öç’çW6‚ƒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DBÂgöÇ—†öç’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'öÇ—†öç’f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%öÖ…÷öÇ—†öç•÷&r‚’Â6öÖRƒ2’“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%öÖöæõöVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢ÆWB×WBVF–õö6†ææVÂÒWfVçE÷7G&VÓ°¢VF–õö6†ææVÅ³EÒÒC°¢ÆWBVF–õö–çWBÒfÇöf—‡GW&R‚fVF–õö6†ææVÂÂeµÒÂeµÒ“°¢ÆWBVF–õöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fVF–õö–çWB’æW‡V7B‚&VF–ò6Æ—6†÷VÆB'6R"“°¢76W'EöW€¢VF–õöFö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%öÖ…÷öÇ—†öç•÷&r‚’À¢æöæP¢“°¢76W'EöW†VF–õöFö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%öÖöæõöVæ&ÆVB‚’ÂæöæR“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5ö6†ææVÅö7WEöæEö7WEö'•öw&÷W5öÆ÷76ÆW76Ç’‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒƒEÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cu÷SbçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c÷SbçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&7WBÖw&÷Wf—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ6†ææVÅö7WEöw&÷W2‚’Â6öÖR‚ƒrÂ’’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢ÆWB×WB–ç7G'VÖVçEö6†ææVÂÒWfVçE÷7G&VÓ°¢–ç7G'VÖVçEö6†ææVÅ³EÒÒC°¢ÆWB–ç7G'VÖVçEöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚f–ç7G'VÖVçEö6†ææVÂÂeµÒÂeµÒ’¢æW‡V7B‚&–ç7G'VÖVçBf—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢–ç7G'VÖVçEöFö7VÖVçBæ6†ææVÇ2‚•³Òæ6†ææVÅö7WEöw&÷W2‚’À¢6öÖR‚ƒrÂ’¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%÷&WfW'6UöfÆuöæE÷&W6W'fW5ö÷F†W%öfÆuö&—G2‚’°¢f÷"†¶–æBÂg…öfÆw2ÂW‡V7FVB’–â°¢ƒÂ„Su÷SbÂG'VR’À¢ƒÂ„SBÂfÇ6R’À¢ƒBÂ„SrÂfÇ6R’À¢Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ¶–æBÂƒCeÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fg…öfÆw2çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆU÷&WfW'6VB‚’ÂW‡V7FVB“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5öæEöVF—G5÷6×ÆW%÷&V6ö×WFVEöfFW5öÆ÷76ÆW76Ç’‚’°¢ÆWBF–ÂÒ³„CÂ„C%Ó°¢ÆWB–çWBÒfÇöf—‡GW&R€¢e°¢ƒCÂrÂÂƒRÂÂƒCbÂƒ3BÂƒ"ÂƒD"ÂÂÂƒD2Â#ÂÂƒc"ÂÂÀ¢ÒÀ¢eµÒÀ¢gF–ÂÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚%6×ÆW"fFRf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó°¢76W'EöW†6†ææVÂç6×ÆW%öfFUö÷WE÷&r‚’Â6öÖRƒ’“°¢76W'EöW†6†ææVÂç6×ÆW%öfFUö–å÷&r‚’Â6öÖRƒ#’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢Fö7VÖVç@¢ç6WE÷6×ÆW%÷V–6µöfFW2ƒrÂS"Â#Sb¢æW‡V7B‚'&V6övæ—¦VB&V6ö×WFVBfFW26†÷VÆB&RVF—F&ÆR"“°¢ÆWBWFFVBÒFö7VÖVçBæ6†ææVÇ2‚’ç&VÖ÷fRƒ“°¢76W'EöW‡WFFVBç6×ÆW%öfFUö÷WE÷&r‚’Â6öÖRƒS"’“°¢76W'EöW‡WFFVBç6×ÆW%öfFUö–å÷&r‚’Â6öÖRƒ#Sb’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒCb¢æW‡V7B‚&÷F†W"6×ÆW"e‚6öçG&öÂ6†÷VÆB&VÖ–â"¢ç–ÆöB‚’À¢e³ƒ3BÂƒ%Ð¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³Òç6×ÆW%öfFUö÷WE÷&r‚’Â6öÖRƒS"’“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³Òç6×ÆW%öfFUö–å÷&r‚’Â6öÖRƒ#Sb’“° ¢ÆWB&Vf÷&Uö–çfÆ–BÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B†Fö7VÖVçBç6WE÷6×ÆW%÷V–6µöfFW2ƒrÂ#RÂ’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&Uö–çfÆ–B“°¢76W'B†Fö7VÖVçBç6WE÷6×ÆW%÷V–6µöfFW2ƒ“’ÂÂ’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&Uö–çfÆ–B“°¢Ð ¢5·FW7EÐ¢fâ–ç6W'G5öÖ—76–æu÷6×ÆW%öfFUöWfVçG5ö&W6–FU÷6×ÆW%ög…ö6öçG&öÇ2‚’°¢ÆWB–çWBÒfÇöf—‡GW&R€¢e³ƒCÂrÂÂƒRÂÂƒCbÂƒ3BÂƒ"Âƒc"ÂÂÒÀ¢eµÒÀ¢eµÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚%6×ÆW"f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç6WE÷6×ÆW%÷V–6µöfFW2ƒrÂ3#ÂcC¢æW‡V7B‚&Ö—76–ærfFRWfVçG26†÷VÆB&R7&VFVB"“°¢ÆWB6†ææVÂÒFö7VÖVçBæ6†ææVÇ2‚’ç&VÖ÷fRƒ“°¢76W'EöW†6†ææVÂç6×ÆW%öfFUö÷WE÷&r‚’Â6öÖRƒ3#’“°¢76W'EöW†6†ææVÂç6×ÆW%öfFUö–å÷&r‚’Â6öÖRƒcC’“°¢ÆWB6†ææVÅöWfVçG2Ò6†ææVÀ¢æWfVçE÷&ævR‚¢æÖ‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•¶–æFW…Òæ÷6öFR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'B€¢6†ææVÅöWfVçG0¢çv–æF÷w2ƒ2¢æç’‡Çv–æF÷wÂv–æF÷rÓÒ³ƒCbÂƒD"ÂƒD5Ò¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’çVçw&‚“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³Òç6×ÆW%öfFUö÷WE÷&r‚’Â6öÖRƒ3#’“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%öÆö÷÷ö–çEöæE÷–æu÷öæuöfÆw5öÆ÷76ÆW76Ç’‚’°¢f÷"†¶–æBÂfÆw2Â–æu÷öærÂW‡V7FVEöÆö÷÷ö–çG2ÂW‡V7FVE÷–æu÷öær’–â°¢ƒÂƒó…÷S3"ÂG'VRÂG'VRÂG'VR’À¢ƒÂƒóÂfÇ6RÂfÇ6RÂfÇ6R’À¢ƒBÂƒó‚ÂG'VRÂfÇ6RÂfÇ6R’À¢Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ¶–æBÂƒBÂSƒ£¦g&öÒ‡–æu÷öær’Âƒ„eÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚ffÆw2çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%÷W6W5öÆö÷÷ö–çG2‚’À¢W‡V7FVEöÆö÷÷ö–çG0¢“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%÷–æu÷öæuöÆö÷öVæ&ÆVB‚’À¢W‡V7FVE÷–æu÷öæp¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5÷6×ÆW%÷&ö÷Eöæ÷FUöæEö–væ÷&W5öæöå÷6×ÆW%ö÷%ö–çfÆ–E÷fÇVW2‚’°¢f÷"†¶–æBÂ&ö÷Eöæ÷FRÂW‡V7FVE÷&ö÷Eö¶W’’–à¢²ƒÂs%÷S3"Â6öÖRƒs%÷Sb’’ÂƒÂ#‚ÂæöæR’ÂƒBÂs"ÂæöæR•Ð¢°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ¶–æBÂƒƒuÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚g&ö÷Eöæ÷FRçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç6×ÆW%÷&ö÷Eö¶W’‚’ÂW‡V7FVE÷&ö÷Eö¶W’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5ö6†ææVÅöf–æU÷GVæUö5÷6–væVEö6VçG5öæEö6†V6·5öFö7VÖVçFVE÷&ævR‚’°¢f÷"†¶–æBÂ6VçG2ÂW‡V7FVB’–â°¢ƒ÷S‚ÂÓö“3"Â6öÖR‚Óö“3"’’À¢ƒBÂÂ6öÖRƒ’’À¢ƒ"ÂÓ3rÂ6öÖR‚Ó3r’’À¢ƒÂÂæöæR’À¢ƒ2ÂSÂæöæR’À¢Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ¶–æBÂƒ„UÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f6VçG2çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eöf–æU÷GVæU÷&r‚’Â6öÖR†6VçG2’“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eöf–æU÷GVæUö6VçG2‚’ÂW‡V7FVB“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð¢Ð ¢5·FW7EÐ¢fâVF—G5ö6†ææVÅöf–æU÷GVæUö–å÷Æ6Uö÷%ö–ç6W'G5ö—G5öGv÷&EöWfVçB‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒƒuÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cc÷S3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒ„UÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚b‚Ó#ö“3"’çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒ„eÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c„U÷S3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWBVç&VÆFVE÷&ö÷EöWfVçBÒFö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWBVç&VÆFVEöfÆw5öWfVçBÒFö7VÖVçBæWfVçG2‚•³EÒçv—&Uö'—FW2‚’çFõ÷fV2‚“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöf–æU÷GVæUö6VçG2ƒrÂ3R¢æW‡V7B‚$f–æRGVæR6†÷VÆBWFFR"“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eöf–æU÷GVæUö6VçG2‚’Â6öÖRƒ3R’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’ÂVç&VÆFVE÷&ö÷EöWfVçB“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³EÒçv—&Uö'—FW2‚’ÂVç&VÆFVEöfÆw5öWfVçB“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢æW‡V7B‚&VF—FVBf—‡GW&R6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eöf–æU÷GVæU÷&r‚’Â6öÖRƒ3R’“° ¢ÆWB×WBÖ—76–æuöWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂBÂƒ“Ó°¢Ö—76–æuöWfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cƒ#3EóScs…÷S3"çFõöÆUö'—FW2‚’“°¢Ö—76–æuöWfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBÖ—76–æuö–çWBÒfÇöf—‡GW&R‚fÖ—76–æuöWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÖ—76–ærÒfÇFö7VÖVçC£§'6R‚fÖ—76–æuö–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Ö—76–æp¢ç6WEö6†ææVÅöf–æU÷GVæUö6VçG2ƒrÂÓ¢æW‡V7B‚&Ö—76–ærf–æRGVæR6†÷VÆB&R–ç6W'FVB"“°¢ÆWBf–æU÷GVæUö–æFW‚ÒÖ—76–æræ6†ææVÇ2‚•³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂÖ—76–æræWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒƒ„R¢æW‡V7B‚$f–æRGVæR6†÷VÆB&R–ç6W'FVB"“°¢ÆWBföÆÆ÷v–æuöGv÷&Eö–æFW‚ÒÖ—76–æræ6†ææVÇ2‚•³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂÖ—76–æræWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒƒ“¢æW‡V7B‚'Vç&VÆFVBGv÷&B6†÷VÆB&VÖ–â"“°¢76W'B†f–æU÷GVæUö–æFW‚ÂföÆÆ÷v–æuöGv÷&Eö–æFW‚“°¢76W'EöW†Ö—76–æræ6†ææVÇ2‚•³Òæ¶W–&ö&Eöf–æU÷GVæUö6VçG2‚’Â6öÖR‚Ó’“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fÖ—76–æræVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢çVçw&‚¢æ6†ææVÇ2‚•³Ð¢æ¶W–&ö&Eöf–æU÷GVæU÷&r‚’À¢6öÖR‚Ó¢“°¢Ð ¢5·FW7EÐ¢fâ6†ææVÅöf–æU÷GVæUöVF—E÷&V¦V7G5ö–çfÆ–E÷&ævW5öæEöæöåö–ç7G'VÖVçEö6†ææVÇ2‚’°¢ÆWBWfVçE÷7G&VÒÒ³ƒCÂrÂÂƒRÂÂƒc"ÂÂÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEö6†ææVÅöf–æU÷GVæUö6VçG2ƒrÂ’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†Fö7VÖVçBÂ&Vf÷&R“° ¢ÆWBÆ–W%öWfVçE÷7G&VÒÒ³ƒCÂrÂÂƒRÂ2Âƒc"ÂÂÓ°¢ÆWBÆ–W%ö–çWBÒfÇöf—‡GW&R‚fÆ–W%öWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÆ–W"ÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒÆ–W"æ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢Æ–W"ç6WEö6†ææVÅöf–æU÷GVæUö6VçG2ƒrÂ#R’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†Æ–W"Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5ö6†ææVÅö¶W•÷&Vv–öåög&öÕ÷&ÖWFW'5öæE÷&W6W'fW5öWfVçEö'—FW2‚’°¢f÷"†Æ÷rÂ†–v‚ÂW‡V7FVB’–â²ƒ3bÂƒBÂ6öÖR‚ƒ3bÂƒB’’’Âƒs"ÂcÂæöæR’ÂƒÂ#‚ÂæöæR•Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂEÓ°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2†Æ÷rÂ†–v‚’À¢“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eö¶W•÷&Vv–öâ‚’ÂW‡V7FVB“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢ÆWB×WB6†÷'EöWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WB6†÷'EöWfVçE÷7G&VÒÂ„3rÂe³²sUÒ“°¢6†÷'EöWfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB6†÷'EöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚g6†÷'EöWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'6†÷'B&ÖWFW'2f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW‡6†÷'EöFö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&Eö¶W•÷&Vv–öâ‚’ÂæöæR“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öæEöVF—G5ö6†ææVÅ÷F–ÖU÷7G&WF6…÷6WGF–æw5öÆ÷76ÆW76Ç’‚’°¢ÆWB×WB&ÖWFW'2ÒfV2³„S²c…Ó°¢&ÖWFW'5³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cƒ#3EóScs…÷S3"çFõöÆUö'—FW2‚’“°¢&ÖWFW'5³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Ó#ö“3"’çFõöÆUö'—FW2‚’“°¢&ÖWFW'5³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚c#Sö“3"çFõöÆUö'—FW2‚’“°¢&ÖWFW'5³"âãeÒæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂg&ÖWFW'2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6WGF–æw2ÒFö7VÖVçBæ6†ææVÇ2‚•³Ð¢ç7G&WF6…÷6WGF–æw2‚¢æW‡V7B‚&gVÆÂF–ÖR7G&WF6†–ærf–VÆG26†÷VÆBFV6öFR"“°¢76W'EöW‡6WGF–æw2çF–ÖU÷&r‚’Âƒ#3EóScs‚“°¢76W'EöW‡6WGF–æw2ç—F6…÷&r‚’ÂÓ#“°¢76W'EöW‡6WGF–æw2æ×VÇF—Æ–W%÷&r‚’Â#S“°¢76W'EöW‡6WGF–æw2æÖöFU÷&r‚’Â“°¢76W'EöW‡6WGF–æw2æÖöFR‚’Â6†ææVÅ7G&WF6„ÖöFS£¤VÆ7F—VS4vVæW&–2“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢ÆWB×WBW‡V7FVE÷&ÖWFW'2Ò&ÖWFW'2æ6ÆöæR‚“°¢W‡V7FVE÷&ÖWFW'5³âãEÒæ6÷•ög&öÕ÷6Æ–6R‚c“%÷S3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&ÖWFW'5³Bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚c3ö“3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&ÖWFW'5³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Ó#Sö“3"’çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷&ÖWFW'5³"âãeÒæ6÷•ög&öÕ÷6Æ–6R‚cö“3"çFõöÆUö'—FW2‚’“°¢Fö7VÖVç@¢æVF—Eö6†ææVÅ÷7G&WF6…÷6WGF–æw2€¢rÀ¢6†ææVÅ7G&WF6…6WGF–æw4VF—B°¢F–ÖU÷&s¢6öÖRƒ“"’À¢—F6…÷&s¢6öÖRƒ3’À¢×VÇF—Æ–W%÷&s¢6öÖR‚Ó#S’À¢ÖöFU÷&s¢6öÖRƒ’À¢ÒÀ¢¢æW‡V7B‚&W†—7F–ær7G&WF6‚6WGF–æw26†÷VÆB&RVF—F&ÆR"“°¢ÆWBVF—FVEöWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„3r¢æW‡V7B‚%&ÖWFW'2WfVçB6†÷VÆB&VÖ–â&W6VçB"“°¢76W'EöW†VF—FVEöWfVçBç–ÆöB‚’ÂW‡V7FVE÷&ÖWFW'2“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òç7G&WF6…÷6WGF–æw2‚’çVçw&‚’æÖöFR‚’À¢6†ææVÅ7G&WF6„ÖöFS£¥&W6×ÆP¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB&V÷Vâ"“°¢76W'EöW‡&V÷VæVBæ6†ææVÇ2‚’ÂFö7VÖVçBæ6†ææVÇ2‚’“°¢Ð ¢5·FW7EÐ¢fâF–ÖU÷7G&WF6…öVF—F÷%÷&V¦V7G5öÖ—76–æu÷6†÷'EöÖ&–wV÷W5öæE÷Væ¶æ÷vå÷6WGF–æw2‚’°¢ÆWBVF—BÒ6†ææVÅ7G&WF6…6WGF–æw4VF—B°¢ÖöFU÷&s¢6öÖRƒ’À¢âä6†ææVÅ7G&WF6…6WGF–æw4VF—C£¦FVfVÇB‚¢Ó° ¢ÆWBÖ—76–æuö'—FW2ÒfÇöf—‡GW&R‚e³ƒCÂrÂÂƒRÂÂƒc"ÂÂÒÂeµÒÂeµÒ“°¢ÆWB×WBÖ—76–ærÒfÇFö7VÖVçC£§'6R‚fÖ—76–æuö'—FW2’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒÖ—76–æræ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢Ö—76–æræVF—Eö6†ææVÅ÷7G&WF6…÷6WGF–æw2ƒrÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†Ö—76–ærÂ&Vf÷&R“° ¢ÆWB×WB6†÷'E÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WB6†÷'E÷7G&VÒÂ„3rÂe³²UÒ“°¢6†÷'E÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WB6†÷'BÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚g6†÷'E÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'6†÷'Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒ6†÷'Bæ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢6†÷'BæVF—Eö6†ææVÅ÷7G&WF6…÷6WGF–æw2ƒrÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW‡6†÷'BÂ&Vf÷&R“° ¢ÆWB×WBGWÆ–6FU÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBGWÆ–6FU÷7G&VÒÂ„3rÂe³²eÒ“°¢VæEöFFöWfVçB‚f×WBGWÆ–6FU÷7G&VÒÂ„3rÂe³²eÒ“°¢GWÆ–6FU÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WBGWÆ–6FRÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fGWÆ–6FU÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&GWÆ–6FRf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒGWÆ–6FRæ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢GWÆ–6FRæVF—Eö6†ææVÅ÷7G&WF6…÷6WGF–æw2ƒrÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW†GWÆ–6FRÂ&Vf÷&R“° ¢ÆWB×WBgVÆÅ÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBgVÆÅ÷7G&VÒÂ„3rÂe³²eÒ“°¢gVÆÅ÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WBVæ¶æ÷våöÖöFRÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fgVÆÅ÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&gVÆÂf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒVæ¶æ÷våöÖöFRæ6ÆöæR‚“°¢76W'B†ÖF6†W2€¢Væ¶æ÷våöÖöFRæVF—Eö6†ææVÅ÷7G&WF6…÷6WGF–æw2€¢rÀ¢6†ææVÅ7G&WF6…6WGF–æw4VF—B°¢ÖöFU÷&s¢6öÖRƒ’À¢âä6†ææVÅ7G&WF6…6WGF–æw4VF—C£¦FVfVÇB‚¢Ð¢’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'EöW‡Væ¶æ÷våöÖöFRÂ&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öVæ&ÆUöÖ–å÷—F6…ög&öÕö6†ææVÅ÷&ÖWFW'5ö'—FU÷GvVÇfR‚’°¢f÷"‡7F÷&VBÂW‡V7FVB’–â²ƒÂfÇ6R’ÂƒÂG'VR’Âƒ„RÂG'VR•Ò°¢ÆWB×WB&ÖWFW'2Ò6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB“°¢&ÖWFW'5³ÒÒ7F÷&VC°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂg&ÖWFW'2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöÖ–å÷—F6…öVæ&ÆVB‚’À¢W‡V7FV@¢“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÖ7FW%÷—F6…ööfg6WEö6VçG2…6öÖRƒ3sR’’À¢–bW‡V7FVB²3sRÒVÇ6R²Ð¢“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÖ7FW%÷—F6…ööfg6WEö6VçG2„æöæR’Â“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÖ7FW%÷—F6…ööfg6WEö6VçG2…6öÖRƒ#’’À¢ ¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢ÆWB×WB6†÷'EöWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WB6†÷'EöWfVçE÷7G&VÒÂ„3rÂe³²Ò“°¢6†÷'EöWfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB6†÷'EöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚g6†÷'EöWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'6†÷'B&ÖWFW'2f—‡GW&R6†÷VÆB'6R"“°¢76W'B‚6†÷'EöFö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöÖ–å÷—F6…öVæ&ÆVB‚’“°¢76W'EöW€¢6†÷'EöFö7VÖVçBæ6†ææVÇ2‚•³ÒæÖ7FW%÷—F6…ööfg6WEö6VçG2…6öÖRƒ3sR’’À¢ ¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öFE÷Fõö¶W•ög&öÕö6†ææVÅ÷&ÖWFW'5ö'—FU÷6—‡G•öf÷W"‚’°¢f÷"‡7F÷&VBÂW‡V7FVB’–â²ƒÂfÇ6R’ÂƒÂG'VR’Âƒ„RÂG'VR•Ò°¢ÆWB×WB&ÖWFW'2Ò6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB“°¢&ÖWFW'5³c5ÒÒ7F÷&VC°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂg&ÖWFW'2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöFE÷Fõö¶W•öVæ&ÆVB‚’À¢W‡V7FV@¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢ÆWB×WB6†÷'EöWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB‚f×WB6†÷'EöWfVçE÷7G&VÒÂ„3rÂe³²c5Ò“°¢6†÷'EöWfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB6†÷'EöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚g6†÷'EöWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'6†÷'B&ÖWFW'2f—‡GW&R6†÷VÆB'6R"“°¢76W'B‚6†÷'EöFö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöFE÷Fõö¶W•öVæ&ÆVB‚’“°¢Ð ¢5·FW7EÐ¢fâVF—G5öVæ&ÆUöÖ–å÷—F6…÷v—F†÷WE÷&Ww&—F–æuö÷F†W%ö6†ææVÅ÷&ÖWFW'2‚’°¢ÆWB×WB&ÖWFW'2Ò6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB“°¢&ÖWFW'5³ÒÒ„S°¢&ÖWFW'2æW‡FVæEög&öÕ÷6Æ–6R‚e³„ÂƒSUÒ“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒRÂ„dÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂg&ÖWFW'2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&ÖWFW%öWfVçEö–æFW‚ÒFö7VÖVçBæ6†ææVÇ2‚•³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„3r¢æW‡V7B‚&6†ææVÂ6†÷VÆB†fR&ÖWFW'2FF"“°¢ÆWBVç&VÆFVEöWfVçEö&Vf÷&RÒFö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWB×WBW‡V7FVE÷&ÖWFW'2Ò&ÖWFW'2æ6ÆöæR‚“°¢W‡V7FVE÷&ÖWFW'5³ÒÒ° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂG'VR¢æW‡V7B‚&Ö–â—F6‚6†÷VÆB&RVæ&ÆVB"“°¢76W'B†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöÖ–å÷—F6…öVæ&ÆVB‚’“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·&ÖWFW%öWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷&ÖWFW'0¢“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’ÂVç&VÆFVEöWfVçEö&Vf÷&R“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂfÇ6R¢æW‡V7B‚&Ö–â—F6‚6†÷VÆB&RF—6&ÆVB"“°¢76W'B‚Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöÖ–å÷—F6…öVæ&ÆVB‚’“°¢ÆWB×WBW‡V7FVE÷&ÖWFW'2Ò&ÖWFW'3°¢W‡V7FVE÷&ÖWFW'5³ÒÒ°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·&ÖWFW%öWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷&ÖWFW'0¢“°¢Ð ¢5·FW7EÐ¢fâVF—G5öFE÷Fõö¶W•÷v—F†÷WE÷&Ww&—F–æuö÷F†W%ö6†ææVÅ÷&ÖWFW'2‚’°¢ÆWB×WB&ÖWFW'2Ò6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB“°¢&ÖWFW'5³c5ÒÒ„S°¢&ÖWFW'2æW‡FVæEög&öÕ÷6Æ–6R‚e³„ÂƒSUÒ“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒRÂ„dÓ°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂg&ÖWFW'2“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&ÖWFW%öWfVçEö–æFW‚ÒFö7VÖVçBæ6†ææVÇ2‚•³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„3r¢æW‡V7B‚&6†ææVÂ6†÷VÆB†fR&ÖWFW'2FF"“°¢ÆWBVç&VÆFVEöWfVçEö&Vf÷&RÒFö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWB×WBW‡V7FVE÷&ÖWFW'2Ò&ÖWFW'2æ6ÆöæR‚“°¢W‡V7FVE÷&ÖWFW'5³c5ÒÒ° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂfÇ6R¢æW‡V7B‚$FBFò¶W’6†÷VÆB&RF—6&ÆVB"“°¢76W'B‚Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöFE÷Fõö¶W•öVæ&ÆVB‚’“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·&ÖWFW%öWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷&ÖWFW'0¢“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òçv—&Uö'—FW2‚’ÂVç&VÆFVEöWfVçEö&Vf÷&R“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂG'VR¢æW‡V7B‚$FBFò¶W’6†÷VÆB&RVæ&ÆVB"“°¢76W'B†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöFE÷Fõö¶W•öVæ&ÆVB‚’“°¢ÆWB×WBW‡V7FVE÷&ÖWFW'2Ò&ÖWFW'3°¢W‡V7FVE÷&ÖWFW'5³c5ÒÒ°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·&ÖWFW%öWfVçEö–æFW…Òç–ÆöB‚’À¢W‡V7FVE÷&ÖWFW'0¢“°¢Ð ¢5·FW7EÐ¢fâ–ç6W'G5öVæ&ÆUöÖ–å÷—F6…÷&ÖWFW'5öæE÷&V¦V7G5öÖ&–wV÷W5ö6†ææVÇ5öFöÖ–6ÆÇ’‚’°¢ÆWB×WBæõ÷&ÖWFW'5÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒRÂ„dÓ°¢æõ÷&ÖWFW'5÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fæõ÷&ÖWFW'5÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂfÇ6R¢æW‡V7B‚&F—6&Æ–ærâ'6VçBfÆr6†÷VÆBÆVfR—B'6VçB"“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂG'VR¢æW‡V7B‚%&ÖWFW'2WfVçB6†÷VÆB&R–ç6W'FVBgFW"F†R¶–æBWfVçB"“°¢76W'B†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöÖ–å÷—F6…öVæ&ÆVB‚’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òæ÷6öFR‚’Â„3r“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òç–ÆöB‚’æÆVâ‚’Â"“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òç–ÆöB‚•³ÒÂ“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’Âe³ƒRÂ„dÒ“° ¢ÆWB×WBÆ–W%÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ5Ó°¢Æ–W%÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBÆ–W%ö–çWBÒfÇöf—‡GW&R‚fÆ–W%÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÆ–W"ÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö–çWB’æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢76W'B†Æ–W"ç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂG'VR’æ—5öW'"‚’“°¢76W'EöW†Æ–W"æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’ÂÆ–W%ö–çWB“° ¢ÆWB×WBGWÆ–6FU÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB€¢f×WBGWÆ–6FU÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB’À¢“°¢VæEöFFöWfVçB€¢f×WBGWÆ–6FU÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒC‚Â“b’À¢“°¢GWÆ–6FU÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBGWÆ–6FUö–çWBÒfÇöf—‡GW&R‚fGWÆ–6FU÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBGWÆ–6FRÐ¢fÇFö7VÖVçC£§'6R‚fGWÆ–6FUö–çWB’æW‡V7B‚&GWÆ–6FRf—‡GW&R6†÷VÆB'6R"“°¢76W'B†GWÆ–6FRç6WEö6†ææVÅöÖ–å÷—F6…öVæ&ÆVBƒrÂG'VR’æ—5öW'"‚’“°¢76W'EöW†GWÆ–6FRæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’ÂGWÆ–6FUö–çWB“°¢Ð ¢5·FW7EÐ¢fâ–ç6W'G5öFE÷Fõö¶W•÷&ÖWFW'5öæE÷&V¦V7G5öÖ&–wV÷W5ö6†ææVÇ5öFöÖ–6ÆÇ’‚’°¢ÆWB×WBæõ÷&ÖWFW'5÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒRÂ„dÓ°¢æõ÷&ÖWFW'5÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fæõ÷&ÖWFW'5÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂfÇ6R¢æW‡V7B‚&F—6&Æ–ærâ'6VçBfÆr6†÷VÆBÆVfR—B'6VçB"“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Fö7VÖVç@¢ç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂG'VR¢æW‡V7B‚%&ÖWFW'2WfVçB6†÷VÆB&R–ç6W'FVBgFW"F†R¶–æBWfVçB"“°¢76W'B†Fö7VÖVçBæ6†ææVÇ2‚•³Òæ¶W–&ö&EöFE÷Fõö¶W•öVæ&ÆVB‚’“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òæ÷6öFR‚’Â„3r“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òç–ÆöB‚’æÆVâ‚’ÂcB“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³%Òç–ÆöB‚•³c5ÒÂ“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³5Òçv—&Uö'—FW2‚’Âe³ƒRÂ„dÒ“° ¢ÆWB×WBÆ–W%÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ5Ó°¢Æ–W%÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBÆ–W%ö–çWBÒfÇöf—‡GW&R‚fÆ–W%÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÆ–W"ÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö–çWB’æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢76W'B†Æ–W"ç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂG'VR’æ—5öW'"‚’“°¢76W'EöW†Æ–W"æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’ÂÆ–W%ö–çWB“° ¢ÆWB×WBGWÆ–6FU÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB€¢f×WBGWÆ–6FU÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒ3bÂƒB’À¢“°¢VæEöFFöWfVçB€¢f×WBGWÆ–6FU÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒC‚Â“b’À¢“°¢GWÆ–6FU÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBGWÆ–6FUö–çWBÒfÇöf—‡GW&R‚fGWÆ–6FU÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBGWÆ–6FRÐ¢fÇFö7VÖVçC£§'6R‚fGWÆ–6FUö–çWB’æW‡V7B‚&GWÆ–6FRf—‡GW&R6†÷VÆB'6R"“°¢76W'B†GWÆ–6FRç6WEö6†ææVÅöFE÷Fõö¶W•öVæ&ÆVBƒrÂG'VR’æ—5öW'"‚’“°¢76W'EöW†GWÆ–6FRæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’ÂGWÆ–6FUö–çWB“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5ö6†ææVÅöÆWfVÅöF§W7FÖVçG5ö5÷&uöf–VÆG5öÆ÷76ÆW76Ç’‚’°¢ÆWB×WB–ÆöBÒ‚Ó3#“3"’çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c%ó3CWS3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c„#%ô34CGS3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óv“3"’çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚c#SV“3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³„RÂƒTÒ“° ¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ5Ó°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂg–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂg–ÆöE²âã•Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó°¢76W'EöW†6†ææVÂæÆWfVÅöF§W7FÖVçG2‚’æÆVâ‚’Â“°¢ÆWBF§W7FÖVçBÒf6†ææVÂæÆWfVÅöF§W7FÖVçG2‚•³Ó°¢76W'EöW†F§W7FÖVçBçå÷&r‚’ÂÓ3#“°¢76W'EöW†F§W7FÖVçBçföÇVÖU÷&r‚’Â%ó3CR“°¢76W'EöW†F§W7FÖVçBçVæ¶æ÷vå÷&r‚’Â„#%ô34CB“°¢76W'EöW†F§W7FÖVçBæÖöE÷…÷&r‚’ÂÓr“°¢76W'EöW†F§W7FÖVçBæÖöE÷•÷&r‚’Â#SR“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâVF—G5ööæUö6†ææVÅöÆWfVÅöF§W7FÖVçEöWfVçEöæE÷&W6W'fW5÷Væ¶æ÷våö'—FW2‚’°¢ÆWBÖ¶U÷–ÆöBÐ¢Çã¢“3"ÂföÇVÖS¢S3"ÂVæ¶æ÷vã¢S3"ÂÖöE÷ƒ¢“3"ÂÖöE÷“¢“3"ÂF–Ã¢e·S…×Â°¢ÆWB×WB–ÆöBÒâçFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gföÇVÖRçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷vâçFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fÖöE÷‚çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fÖöE÷’çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‡F–Â“°¢–Æö@¢Ó°¢ÆWBf—'7E÷–ÆöBÒÖ¶U÷–ÆöB‚ÓÂ#Â„#%ô34CBÂ3ÂCÂe³„S²Ò“°¢ÆWB6V6öæE÷–ÆöBÒÖ¶U÷–ÆöBƒÂ#Âƒ#ó3CÂ3ÂCÂe³ƒT²Ò“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂ5Ó°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂff—'7E÷–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂg6V6öæE÷–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S‚Âe³ƒÂƒ#"Âƒ35Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“° ¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó°¢ÆWBF§W7FÖVçEöWfVçG2Ò6†ææVÀ¢æWfVçE÷&ævR‚¢æf–ÇFW"‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„CR¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†F§W7FÖVçEöWfVçG2æÆVâ‚’Â"“°¢ÆWBf—'7EöWfVçEö–æFW‚ÒF§W7FÖVçEöWfVçG5³Ó°¢ÆWB6V6öæEöWfVçEö–æFW‚ÒF§W7FÖVçEöWfVçG5³Ó°¢ÆWBf—'7E÷v—&RÒFö7VÖVçBæWfVçG2‚•¶f—'7EöWfVçEö–æFW…Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢ÆWBVç&VÆFVEöWfVçEö–æFW‚Ò6†ææVÀ¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„S‚¢æW‡V7B‚&f—‡GW&R6†÷VÆB6öçF–âæ÷F†W"6†ææVÂWfVçB"“°¢ÆWBVç&VÆFVE÷v—&RÒFö7VÖVçBæWfVçG2‚•·Vç&VÆFVEöWfVçEö–æFW…Ð¢çv—&Uö'—FW2‚¢çFõ÷fV2‚“°¢ÆWB×WBW‡V7FVE÷6V6öæE÷v—&RÒFö7VÖVçBæWfVçG2‚•·6V6öæEöWfVçEö–æFW…Òçv—&Uö'—FW2‚’çFõ÷fV2‚“°¢W‡V7FVE÷6V6öæE÷v—&U³2âãuÒæ6÷•ög&öÕ÷6Æ–6R‚b‚Óó#3Eö“3"’çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷6V6öæE÷v—&U³râãÒæ6÷•ög&öÕ÷6Æ–6R‚c“…óscU÷S3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷6V6öæE÷v—&U³Râã•Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Ó3#ö“3"’çFõöÆUö'—FW2‚’“° ¢Fö7VÖVç@¢æVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2€¢’À¢6V6öæEöWfVçEö–æFW‚À¢6†ææVÄÆWfVÄF§W7FÖVçG4VF—B°¢å÷&s¢6öÖR‚Óó#3B’À¢föÇVÖU÷&s¢6öÖRƒ“…óscR’À¢ÖöE÷…÷&s¢6öÖR‚Ó3#’À¢ÖöE÷•÷&s¢æöæRÀ¢ÒÀ¢¢æW‡V7B‚'F†R6VÆV7FVBWfVçBf–VÆG26†÷VÆB&RVF—F&ÆR"“° ¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•¶f—'7EöWfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢f—'7E÷v—&P¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·6V6öæEöWfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢W‡V7FVE÷6V6öæE÷v—&P¢“°¢76W'EöW€¢fFö7VÖVçBæWfVçG2‚•·6V6öæEöWfVçEö–æFW…Òçv—&Uö'—FW2‚•³âã5ÒÀ¢e³ƒƒ"ÂƒÒÀ¢'F†R÷&–v–æÂGvòÖ'—FRFFÆVæwF‚&Vf—‚6†÷VÆB&VÖ–â–çF7B ¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·Vç&VÆFVEöWfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢Vç&VÆFVE÷v—&P¢“° ¢W‡V7FVE÷6V6öæE÷v—&U³’âã#5Òæ6÷•ög&öÕ÷6Æ–6R‚b‚Óssuö“3"’çFõöÆUö'—FW2‚’“°¢Fö7VÖVç@¢æVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2€¢’À¢6V6öæEöWfVçEö–æFW‚À¢6†ææVÄÆWfVÄF§W7FÖVçG4VF—B°¢ÖöE÷•÷&s¢6öÖR‚Óssr’À¢âä6†ææVÄÆWfVÄF§W7FÖVçG4VF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚$ÖöB’6†÷VÆB&RVF—F&ÆR–æFWVæFVçFÇ’"“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·6V6öæEöWfVçEö–æFW…Òçv—&Uö'—FW2‚’À¢W‡V7FVE÷6V6öæE÷v—&P¢“° ¢ÆWBF§W7FÖVçG2ÒFö7VÖVçBæ6†ææVÇ2‚•³ÒæÆWfVÅöF§W7FÖVçG2‚’çFõ÷fV2‚“°¢76W'EöW†F§W7FÖVçG2æÆVâ‚’Â"“°¢76W'EöW†F§W7FÖVçG5³Òçå÷&r‚’ÂÓ“°¢76W'EöW†F§W7FÖVçG5³Òçå÷&r‚’ÂÓó#3B“°¢76W'EöW†F§W7FÖVçG5³ÒçföÇVÖU÷&r‚’Â“…óscR“°¢76W'EöW†F§W7FÖVçG5³ÒçVæ¶æ÷vå÷&r‚’Âƒ#ó3C“°¢76W'EöW†F§W7FÖVçG5³ÒæÖöE÷…÷&r‚’ÂÓ3#“°¢76W'EöW†F§W7FÖVçG5³ÒæÖöE÷•÷&r‚’ÂÓssr“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†RVF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&W'6VBæ6†ææVÇ2‚•³ÒæÆWfVÅöF§W7FÖVçG2‚’çFõ÷fV2‚’À¢F§W7FÖVçG0¢“°¢Ð ¢5·FW7EÐ¢fâ6†ææVÅöÆWfVÅöF§W7FÖVçEöVF—E÷&V¦V7G5ö–çfÆ–E÷F&vWG5÷v—F†÷WEö×WFF–öâ‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ’ÂÂƒRÂ5Ó°¢ÆWB×WB6ö×ÆWFU÷–ÆöBÒfV2³ƒ²#Ó°¢6ö×ÆWFU÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³„RÂƒTÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂf6ö×ÆWFU÷–ÆöB“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S‚Âe³ƒ35Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ5Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂe³ƒ##²•Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ5Ò“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CBÂe³„CEÒ“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„CRÂf6ö×ÆWFU÷–ÆöB“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢ÆWBf—'7EöWfVçEö–æFW‚Ò6†ææVÇ5³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„CR¢æW‡V7B‚&f—'7B6†ææVÂ6†÷VÆB†fRÆWfVÂF§W7G2WfVçB"“°¢ÆWB÷F†W%öWfVçEö–æFW‚Ò6†ææVÇ5³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„S‚¢æW‡V7B‚&f—'7B6†ææVÂ6†÷VÆB†fRæ÷F†W"WfVçB"“°¢ÆWBG'Væ6FVEöWfVçEö–æFW‚Ò6†ææVÇ5³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„CR¢æW‡V7B‚'6V6öæB6†ææVÂ6†÷VÆB†fRG'Væ6FVBÆWfVÂF§W7G2WfVçB"“°¢ÆWBÇVv–åöWfVçEö–æFW‚Ò6†ææVÇ5³%Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„CR¢æW‡V7B‚'F†—&B6†ææVÂ6†÷VÆB†fRÇVrÖ–âFF"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢ÆWBVF—BÒ6†ææVÄÆWfVÄF§W7FÖVçG4VF—B°¢föÇVÖU÷&s¢6öÖRƒ#2’À¢âä6†ææVÄÆWfVÄF§W7FÖVçG4VF—C£¦FVfVÇB‚¢Ó° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2€¢’À¢f—'7EöWfVçEö–æFW‚À¢6†ææVÄÆWfVÄF§W7FÖVçG4VF—C£¦FVfVÇB‚¢’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2ƒ’Â÷F†W%öWfVçEö–æFW‚ÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2ƒ’ÂG'Væ6FVEöWfVçEö–æFW‚ÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2ƒÂG'Væ6FVEöWfVçEö–æFW‚ÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2ƒÂÇVv–åöWfVçEö–æFW‚ÂVF—B’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBæVF—Eö6†ææVÅöÆWfVÅöF§W7FÖVçG2ƒ““’Âf—'7EöWfVçEö–æFW‚ÂVF—B’À¢W'"„fÇW'&÷#£¤6†ææVÄæ÷Df÷VæBƒ““’’¢’“°¢76W'EöW€¢Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"’À¢&Vf÷&RÀ¢&f–ÆVBVF—G2×W7BÆVfRF†R&ö¦V7BVæ6†ævVB ¢“°¢Ð ¢5·FW7EÐ¢fâ6WG5ö6†ææVÅöVæ&ÆVEö'•÷WFF–æuö÷%ö–ç6W'F–æuö—G5öWfVçEöÆ÷76ÆW76Ç’‚’°¢ÆWBF–ÂÒ³„RÂƒTÓ°¢f÷"†5öVæ&ÆVEöWfVçB–â¶fÇ6RÂG'VUÒ°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢–b†5öVæ&ÆVEöWfVçB°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒÂÒ“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBW‡V7FVEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢–b†5öVæ&ÆVEöWfVçB°¢ÆWBVæ&ÆVEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒ¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fRâVæ&ÆVBWfVçB"“°¢W‡V7FVEöWfVçG5¶Væ&ÆVEö–æFW…ÒÒfV2³ƒÂÓ°¢ÒVÇ6R°¢ÆWB¶–æEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒR¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fR6†ææVÂ¶–æBWfVçB"“°¢W‡V7FVEöWfVçG2æ–ç6W'B†¶–æEö–æFW‚²ÂfV2³ƒÂÒ“°¢Ð ¢Fö7VÖVç@¢ç6WEö6†ææVÅöVæ&ÆVBƒrÂfÇ6R¢æW‡V7B‚&6†ææVÂ6†÷VÆB&R×WFVB"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæVæ&ÆVB‚’Â6öÖR†fÇ6R’“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒçföÇVÖR‚’Â6öÖRƒƒ#3B’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöWfVçG0¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&×WFVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚•³ÒæVæ&ÆVB‚’Â6öÖR†fÇ6R’“°¢Ð¢Ð ¢5·FW7EÐ¢fâ6WG5ö6†ææVÅöÖ—†W%÷G&6µö'•÷WFF–æuö÷%ö–ç6W'F–æuö—G5öWfVçEöÆ÷76ÆW76Ç’‚’°¢ÆWBF–ÂÒ³„RÂƒTÓ°¢f÷"†5ö76–væÖVçEöWfVçB–â¶fÇ6RÂG'VUÒ°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢–b†5ö76–væÖVçEöWfVçB°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒbÂ5Ò“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBW‡V7FVEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢–b†5ö76–væÖVçEöWfVçB°¢ÆWB76–væÖVçEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒb¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fRâ76–væÖVçBWfVçB"“°¢W‡V7FVEöWfVçG5¶76–væÖVçEö–æFW…ÒÒfV2³ƒbÂ„deÓ°¢ÒVÇ6R°¢ÆWB¶–æEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒR¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fR6†ææVÂ¶–æBWfVçB"“°¢W‡V7FVEöWfVçG2æ–ç6W'B†¶–æEö–æFW‚²ÂfV2³ƒbÂ„deÒ“°¢Ð ¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ—†W%÷G&6²ƒrÂÓ¢æW‡V7B‚&6†ææVÂ6†÷VÆB&÷WFRFò7W'&VçB"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÖ—†W%÷G&6²‚’Â6öÖR‚Ó’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöWfVçG0¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚•³ÒæÖ—†W%÷G&6²‚’Â6öÖR‚Ó’“°¢Ð ¢ÆWB–çWBÒfÇöf—‡GW&R‚e³ƒCÂrÂÂƒRÂÂƒc"ÂÂÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Fö7VÖVçBç6WEö6†ææVÅöÖ—†W%÷G&6²ƒrÂÓ"’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ&F6…ö6†ææVÅöÖ—†W%÷&÷WF–æuö—5öFöÖ–2‚’°¢ÆWB–çWBÒfÇöf—‡GW&R€¢e³ƒCÂrÂÂƒRÂÂƒCÂ‚ÂÂƒRÂÂƒc"ÂÂÒÀ¢eµÒÀ¢eµÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç6WEö6†ææVÅöÖ—†W%÷G&6·2‚e³rÂ…ÒÂB¢æW‡V7B‚&&÷F‚6†ææVÇ26†÷VÆB&R&÷WFVBFövWF†W""“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW†6†ææVÇ5³ÒæÖ—†W%÷G&6²‚’Â6öÖRƒB’“°¢76W'EöW†6†ææVÇ5³ÒæÖ—†W%÷G&6²‚’Â6öÖRƒB’“° ¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒ&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B‡&V¦V7FVBç6WEö6†ææVÅöÖ—†W%÷G&6·2‚e³rÂ“•ÒÂB’æ—5öW'"‚’“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ&VG5öæE÷6WG5ö6†ææVÅ÷7v–æuöÖ—…öÆ÷76ÆW76Ç’‚’°¢ÆWBF–ÂÒ³„DRÂ„EÓ°¢f÷"†¶–æBÂ–æ—F–ÅöÖ—‚Â&WVW7FVEöÖ—‚ÂW‡V7FVEöÖ—‚Â–ç6W'FVB’–â°¢ƒÂæöæRÂ3'SbÂ6öÖRƒ3'Sb’ÂG'VR’À¢ƒ"Â6öÖRƒCSb’ÂsrÂ6öÖRƒswSb’ÂfÇ6R’À¢ƒÂæöæRÂ#‚ÂæöæRÂfÇ6R’À¢Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂ¶–æEÓ°¢–bÆWB6öÖR†–æ—F–ÅöÖ—‚’Ò–æ—F–ÅöÖ—‚°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒcÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f–æ—F–ÅöÖ—‚çFõöÆUö'—FW2‚’“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç7v–æuöÖ—…÷&r‚’Â–æ—F–ÅöÖ—‚“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òç7v–æuöÖ—‚‚’À¢–æ—F–ÅöÖ—‚çVçw&ö÷"ƒ#‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅ÷7v–æuöÖ—‚ƒrÂ&WVW7FVEöÖ—‚¢æW‡V7B‚&6†ææVÂ7v–ærÖ—‚6†÷VÆBWFFR"“° ¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó°¢76W'EöW†6†ææVÂç7v–æuöÖ—…÷&r‚’ÂW‡V7FVEöÖ—‚“°¢76W'EöW†6†ææVÂç7v–æuöÖ—‚‚’ÂW‡V7FVEöÖ—‚çVçw&ö÷"ƒ#‚’“°¢ÆWB7v–æuöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒc¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢–bÆWB6öÖR†W‡V7FVEöÖ—‚’ÒW‡V7FVEöÖ—‚°¢76W'EöW‡7v–æuöWfVçG2æÆVâ‚’Â“°¢76W'EöW‡7v–æuöWfVçG5³Òç–ÆöB‚’ÂfW‡V7FVEöÖ—‚çFõöÆUö'—FW2‚’“°¢ÒVÇ6R°¢76W'B‡7v–æuöWfVçG2æ—5öV×G’‚’“°¢Ð¢–b–ç6W'FVB°¢ÆWB÷6öFW2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ„fÇWfVçC£¦÷6öFR¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†÷6öFW5³âã5ÒÂ³ƒRÂƒcÒ“°¢Ð ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚•³Òç7v–æuöÖ—…÷&r‚’ÂW‡V7FVEöÖ—‚“°¢Ð¢Ð ¢5·FW7EÐ¢fâ&V¦V7G5÷Vç7W÷'FVEö÷%öÖ&–wV÷W5ö6†ææVÅ÷7v–æuöÖ—…öVF—G2‚’°¢ÆWB×WB÷WEööe÷&ævRÐ¢fÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚e³ƒCÂrÂÂƒRÂÂƒc"ÂÂÒÂeµÒÂeµÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B†÷WEööe÷&ævRç6WEö6†ææVÅ÷7v–æuöÖ—‚ƒrÂ#’’æ—5öW'"‚’“° ¢ÆWB×WBÆ–W"Ð¢fÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚e³ƒCÂrÂÂƒRÂ2Âƒc"ÂÂÒÂeµÒÂeµÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B†Æ–W"ç6WEö6†ææVÅ÷7v–æuöÖ—‚ƒrÂcB’æ—5öW'"‚’“° ¢ÆWB×WBVF–òÐ¢fÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚e³ƒCÂrÂÂƒRÂBÂƒc"ÂÂÒÂeµÒÂeµÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B†VF–òç6WEö6†ææVÅ÷7v–æuöÖ—‚ƒrÂcB’æ—5öW'"‚’“° ¢ÆWB×WBGWÆ–6FRÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R€¢e³ƒCÂrÂÂƒRÂÂƒcÂ3"ÂÂƒcÂcBÂÂƒc"ÂÂÒÀ¢eµÒÀ¢eµÒÀ¢’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B†GWÆ–6FRç6WEö6†ææVÅ÷7v–æuöÖ—‚ƒrÂ“b’æ—5öW'"‚’“°¢Ð ¢5·FW7EÐ¢fâ&VG5öæEöVF—G5ö6†ææVÅ÷¦—VE÷7FFUöÆ÷76ÆW76Ç’‚’°¢ÆWBF–ÂÒ³„DRÂ„EÓ°¢f÷"‡7F÷&VE÷¦—Â&WVW7FVE÷¦—Â–æ—F–Å÷¦—’–â°¢„æöæRÂfÇ6RÂfÇ6R’À¢„æöæRÂG'VRÂfÇ6R’À¢…6öÖRƒ’ÂG'VRÂfÇ6R’À¢…6öÖRƒ’ÂfÇ6RÂG'VR’À¢…6öÖRƒ"’ÂG'VRÂG'VR’À¢Ò°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢–bÆWB6öÖR‡7F÷&VE÷¦—’Ò7F÷&VE÷¦—°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒbÂ7F÷&VE÷¦—Ò“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç¦—VB‚’Â–æ—F–Å÷¦—“° ¢ÆWB×WBW‡V7FVEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢–bÆWB6öÖR‡7F÷&VE÷¦—’Ò7F÷&VE÷¦—°¢ÆWB¦—ö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒb¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fR¦—VB×7FFRWfVçB"“°¢–b‡7F÷&VE÷¦—Ò’Ò&WVW7FVE÷¦—°¢W‡V7FVEöWfVçG5·¦—ö–æFW…ÒÒfV2³ƒbÂSƒ£¦g&öÒ‡&WVW7FVE÷¦—•Ó°¢Ð¢ÒVÇ6R–b&WVW7FVE÷¦—°¢ÆWB¶–æEö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒR¢æW‡V7B‚&f—‡GW&R6†÷VÆB†fR6†ææVÂ¶–æBWfVçB"“°¢W‡V7FVEöWfVçG2æ–ç6W'B†¶–æEö–æFW‚²ÂfV2³ƒbÂÒ“°¢Ð ¢Fö7VÖVç@¢ç6WEö6†ææVÅ÷¦—VBƒrÂ&WVW7FVE÷¦—¢æW‡V7B‚&6†ææVÂ6ö×7B7FFR6†÷VÆBWFFR"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òç¦—VB‚’Â&WVW7FVE÷¦—“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒçföÇVÖR‚’Â6öÖRƒƒ#3B’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöWfVçG0¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚•³Òç¦—VB‚’Â&WVW7FVE÷¦—“°¢Ð¢Ð ¢5·FW7EÐ¢fâ&VG5öæE÷6WG5ö6†ææVÅ÷&v%÷v†–ÆU÷&W6W'f–æu÷F†Uöf÷W'F…ö6öÆ÷%ö'—FR‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒƒÂƒÂƒ#"Âƒ32Â„UÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÒ“°¢ÆWBF–ÂÒ³„DRÂ„EÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òæ6öÆ÷"‚’À¢6öÖR…³ƒÂƒ#"Âƒ32Â„UÒ¢“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅö6öÆ÷"ƒrÂ³„Â„$"Â„45Ò¢æW‡V7B‚&W†—7F–ær6†ææVÂ6öÆ÷"6†÷VÆBWFFR"“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³Òæ6öÆ÷"‚’À¢6öÖR…³„Â„$"Â„42Â„UÒ¢“°¢76W'B€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æç’‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒƒbbWfVçBç–ÆöB‚’ÓÒ³„Â„$"Â„42Â„UÒ¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'WFFVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&÷VæE÷G&—æ6†ææVÇ2‚•³Òæ6öÆ÷"‚’À¢6öÖR…³„Â„$"Â„42Â„UÒ¢“° ¢ÆWBÖ—76–æuö6öÆ÷%÷7G&VÒÒfV2³ƒCÂ‚ÂÂƒRÂÂƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fÖ—76–æuö6öÆ÷%÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÖ—76–æuö6öÆ÷"ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Ö—76–æuö6öÆ÷ ¢ç6WEö6†ææVÅö6öÆ÷"ƒ‚Â³ƒÂƒ#Âƒ3Ò¢æW‡V7B‚&Ö—76–ær6öÆ÷"6†÷VÆB&R–ç6W'FVB"“°¢76W'EöW€¢Ö—76–æuö6öÆ÷"æ6†ææVÇ2‚•³Òæ6öÆ÷"‚’À¢6öÖR…³ƒÂƒ#Âƒ3ÂÒ¢“°¢ÆWBWfVçEö'—FW2ÒÖ—76–æuö6öÆ÷ ¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†WfVçEö'—FW5³%ÒÂ³ƒƒÂƒÂƒ#Âƒ3ÂÒ“°¢76W'EöW†WfVçEö'—FW5³5ÒÂ³ƒC‚Âƒ3BÂƒ%Ò“°¢Ð ¢5·FW7EÐ¢fâ6÷'G5ö6†ææVÇ5ö'•ö‡VU÷F†Våö6‡&öÖF–5÷F†Vå÷Væ6öÆ÷&VB‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢f÷"†–BÂ6öÆ÷"’–â°¢ƒrÂ6öÖR…³ÂÂ#SRÂÒ’’À¢ƒ‚Â6öÖR…³#SRÂÂÂÒ’’À¢ƒ’Â6öÖR…³#SRÂÂ#SRÂÒ’’À¢ƒÂ6öÖR…³Â#SRÂÂÒ’’À¢ƒÂ6öÖR…³sÂsÂsÂÒ’’À¢ƒ"ÂæöæR’À¢Ò°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ–BÂÂƒRÂÒ“°¢–bÆWB6öÖR…·&VBÂw&VVâÂ&ÇVRÂÇ†Ò’Ò6öÆ÷"°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒƒÂ&VBÂw&VVâÂ&ÇVRÂÇ†Ò“°¢Ð¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç6÷'Eö6†ææVÇ2„6†ææVÅ6÷'D÷&FW#£¤6öÆ÷"¢æW‡V7B‚&6öÆ÷"6÷'B6†÷VÆB7V66VVB"“° ¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦–B¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³‚ÂÂrÂ’ÂÂ%Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦6öÆ÷"¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢°¢6öÖR…³#SRÂÂÂÒ’À¢6öÖR…³Â#SRÂÂÒ’À¢6öÖR…³ÂÂ#SRÂÒ’À¢6öÖR…³#SRÂÂ#SRÂÒ’À¢6öÖR…³sÂsÂsÂÒ’À¢æöæRÀ¢Ð¢“°¢Ð ¢5·FW7EÐ¢fâ6÷'G5ö6†ææVÇ5ö'•öÖ—†W%÷G&6µ÷v—F…övVæW&F÷'5öæEöÆ–W'5öf—'7B‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢f÷"†–BÂ¶–æBÂG&6²’–â°¢ƒÂÂ6öÖRƒ2’’À¢ƒÂ"Â6öÖRƒ"’’À¢ƒ"Â2ÂæöæR’À¢ƒ2ÂRÂ6öÖRƒ’’À¢ƒBÂÂ6öÖRƒ’’À¢ƒRÂÂæöæR’À¢Ò°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ–BÂÂƒRÂ¶–æEÒ“°¢–bÆWB6öÖR‡G&6²’ÒG&6²°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒbÂG&6µÒ“°¢Ð¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅö&Æö6·2ÒFö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ°¢6†ææVÀ¢æWfVçE÷&ævR‚¢æÖ‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•¶–æFW…Òçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦Ö—†W%÷G&6²¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒ2’Â6öÖRƒ"’ÂæöæRÂ6öÖRƒ’Â6öÖRƒ’ÂæöæUÐ¢“° ¢Fö7VÖVç@¢ç6÷'Eö6†ææVÇ2„6†ææVÅ6÷'D÷&FW#£¤Ö—†W%G&6²¢æW‡V7B‚$Ö—†W"×G&6²6÷'B6†÷VÆB7V66VVB"“° ¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦–B¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³Â"ÂBÂ2ÂÂUÐ¢“°¢ÆWB×WBW‡V7FVEöWfVçG2ÒfV3£¦æWr‚“°¢f÷"–æFW‚–â³Â"ÂBÂ2ÂÂUÒ°¢W‡V7FVEöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5¶–æFW…Òæ6ÆöæR‚’“°¢Ð¢W‡V7FVEöWfVçG2æW‡FVæB…·fV2³ƒc"ÂÂÕÒ“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöWfVçG0¢“°¢Ð ¢5·FW7EÐ¢fâ&VG5ö6†ææVÅöF—7Æ•öw&÷W5öæE÷6–væVEöw&÷Wö76–væÖVçG2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$G'V×2"“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ%7–çF‡2"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂrÂÂƒRÂÂƒ“ÂÂÂÂÂƒCÂ‚ÂÂƒRÂÂƒ“ÂÂÂÂÂƒCÂ’À¢ÂƒRÂÂƒ“Â„dbÂ„dbÂ„dbÂ„dbÂƒc"ÂÂÀ¢Ò“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$æ÷BF—7Æ’w&÷W"“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBw&÷W2ÒFö7VÖVçBæ6†ææVÅöw&÷W2‚“°¢76W'EöW€¢w&÷W0¢æ—FW"‚¢æÖ„6†ææVÄw&÷W7VÖÖ'“£¦–æFW‚¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³ÂÐ¢“°¢76W'EöW€¢w&÷W0¢æ—FW"‚¢æÖ„6†ææVÄw&÷W7VÖÖ'“£¦æÖR¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖR‚$G'V×2"’Â6öÖR‚%7–çF‡2"•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦w&÷WöçVÖ&W"¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒ’Â6öÖRƒ’Â6öÖR‚Ó•Ð¢“°¢Ð ¢5·FW7EÐ¢fâ76–vç5ö6†ææVÇ5÷FõöåöW†—7F–æuöw&÷WöæE÷&W6W'fW5ö÷F†W%öWfVçG2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$G'V×2"“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ%7–çF‡2"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂrÂÂƒRÂÂƒ“ÂÂÂÂÂƒ“Âƒs‚ÂƒSbÂƒ3BÂƒ"ÂƒCÂ‚ÂÂƒRÀ¢ÂƒC‚Âƒ3BÂƒ"Âƒc"ÂÂÀ¢Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„RÂƒTÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æw&÷Wö6†ææVÇ2‚e³rÂ…ÒÂ%7–çF‡2"¢æW‡V7B‚'6VÆV7FVB6†ææVÇ26†÷VÆB&Rw&÷WVB"’À¢¢“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦w&÷WöçVÖ&W"¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒ’Â6öÖRƒ•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒ“¢æÖ„fÇWfVçC£§v—&Uö'—FW2¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢²e³ƒ“Âƒs‚ÂƒSbÂƒ3BÂƒ%Õ²âåÕÐ¢“°¢76W'B€¢Fö7VÖVçBæWfVçG2‚’æ—FW"‚’æç’‡ÆWfVçGÂ°¢WfVçBæ÷6öFR‚’ÓÒƒC‚bbWfVçBçv—&Uö'—FW2‚’ÓÒ³ƒC‚Âƒ3BÂƒ%Ð¢Ò¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&VF—FVB&ö¦V7BVæ6öFW2"“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB¢æW‡V7B‚&VF—FVB&ö¦V7B'6W2"¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'&÷VæGG&—Væ6öFW2"’À¢Væ6öFV@¢“°¢Ð ¢5·FW7EÐ¢fâ7&VFW5öæWuö6†ææVÅöw&÷WöæE÷&WW6W5ö—Eööå÷&WVB‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$G'V×2"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂrÂÂƒRÂÂƒ“Âƒs‚ÂƒSbÂƒ3BÂƒ"ÂƒCÂ‚ÂÂƒRÂÂƒ“ÂÂÂÀ¢Âƒc"ÂÂÀ¢Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æw&÷Wö6†ææVÇ2‚e³uÒÂ%7G&–æw2"¢æW‡V7B‚&æWrw&÷W6†÷VÆB&R7&VFVB"’À¢¢“°¢ÆWBgFW%ö7&VFRÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&w&÷WVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÅöw&÷W2‚¢æ—FW"‚¢æÖ„6†ææVÄw&÷W7VÖÖ'“£¦æÖR¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖR‚$G'V×2"’Â6öÖR‚%7G&–æw2"•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦w&÷WöçVÖ&W"¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒ’Â6öÖRƒ•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æw&÷Wö6†ææVÇ2‚e³uÒÂ%7G&–æw2"¢æW‡V7B‚'F†R7&VFVBw&÷W6†÷VÆB&R&WW6VB"’À¢¢“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'&WW6VBw&÷W6†÷VÆBVæ6öFR"’À¢gFW%ö7&VFP¢“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fgFW%ö7&VFR¢æW‡V7B‚&w&÷WVB&ö¦V7B6†÷VÆB'6R"¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&w&÷WVB&ö¦V7B6†÷VÆB&÷VæGG&—"’À¢gFW%ö7&VFP¢“°¢Ð ¢5·FW7EÐ¢fâFG5öæE÷&VæÖW5öV×G•ö6†ææVÅöw&÷W5÷v—F†÷WEö6†æv–æuö76–væÖVçG2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$G'V×2"“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ%7–çF‡2"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂrÂÂƒRÂÂƒ“ÂÂÂÂÂƒ“Âƒs‚ÂƒSbÂƒ3BÂƒ"À¢Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„RÂƒTÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æFEö6†ææVÅöw&÷W‚$V×G’"¢æW‡V7B‚&âV×G’F—7Æ’w&÷W6†÷VÆB&RFFVB"’À¢ ¢“°¢Fö7VÖVç@¢ç&VæÖUö6†ææVÅöw&÷WƒÂ$¶W—2"¢æW‡V7B‚&F—7Æ’w&÷W6†÷VÆB&R&VæÖVB"“°¢ÆWB&VæÖVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚'&VæÖVB&ö¦V7BVæ6öFW2"“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÅöw&÷W2‚¢æ—FW"‚¢æÖ„6†ææVÄw&÷W7VÖÖ'“£¦æÖR¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖR‚$G'V×2"’Â6öÖR‚$¶W—2"’Â6öÖR‚$V×G’"•Ð¢“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òæw&÷WöçVÖ&W"‚’Â6öÖRƒ’“°¢76W'B†Fö7VÖVçBç&VæÖUö6†ææVÅöw&÷Wƒ"Â$G'V×2"’æ—5öW'"‚’“°¢76W'EöW€¢Fö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'&V¦V7FVB&VæÖR—2FöÖ–2"’À¢&VæÖV@¢“°¢76W'B†Fö7VÖVçBæWfVçG2‚’æ—FW"‚’æç’‡ÆWfVçGÂ°¢WfVçBæ÷6öFR‚’ÓÒƒ“bbWfVçBçv—&Uö'—FW2‚’ÓÒ³ƒ“Âƒs‚ÂƒSbÂƒ3BÂƒ%Ð¢Ò’“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚g&VæÖVB¢æW‡V7B‚'&VæÖVB&ö¦V7B'6W2"¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'&VæÖVB&ö¦V7B&÷VæGG&—2"’À¢&VæÖV@¢“°¢Ð ¢5·FW7EÐ¢fâFVÆWF–æuö6†ææVÅöw&÷W÷Væ76–vç5ö6†ææVÇ5öæE÷&V–æFW†W5öÆFW%öw&÷W2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV3£¦æWr‚“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$G'V×2"“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ%7–çF‡2"“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„SrÂ$VffV7G2"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂrÂÂƒRÂÂƒ“ÂÂÂÂÂƒ“ÂƒÂƒ#"Âƒ32ÂƒCBÂƒCÂ‚ÂÂƒRÀ¢Âƒ“ÂÂÂÂÂƒCÂ’ÂÂƒRÂÂƒ“Â"ÂÂÂÀ¢Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æFVÆWFUö6†ææVÅöw&÷Wƒ¢æW‡V7B‚&F—7Æ’w&÷W6†÷VÆB&RFVÆWFVB"“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÅöw&÷W2‚¢æ—FW"‚¢æÖ„6†ææVÄw&÷W7VÖÖ'“£¦æÖR¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖR‚$G'V×2"’Â6öÖR‚$VffV7G2"•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦w&÷WöçVÖ&W"¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒ’ÂæöæRÂ6öÖRƒ•Ð¢“°¢76W'B†Fö7VÖVçBæWfVçG2‚’æ—FW"‚’æç’‡ÆWfVçGÂ°¢WfVçBæ÷6öFR‚’ÓÒƒ“bbWfVçBçv—&Uö'—FW2‚’ÓÒ³ƒ“ÂƒÂƒ#"Âƒ32ÂƒCEÐ¢Ò’“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&FVÆWFVB&ö¦V7BVæ6öFW2"“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fVæ6öFVB¢æW‡V7B‚&FVÆWFVB&ö¦V7B'6W2"¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&FVÆWFVB&ö¦V7B&÷VæGG&—2"’À¢Væ6öFV@¢“°¢Ð ¢5·FW7EÐ¢fâÖ÷fW5ö6†ææVÅöWfVçEö&Æö6·5÷v—F†÷WEö6†æv–æuö6†ææVÅöFF‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÂƒC‚ÂƒÂÓ°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â$¶–6²"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂƒRÂÂƒC‚Âƒ#"ÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â%6æ&R"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ’ÂÂƒRÂÂƒC‚Âƒ32ÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â$†B"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBF–ÂÒ³„RÂƒTÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅö&Æö6·2ÒFö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ°¢6†ææVÀ¢æWfVçE÷&ævR‚¢æÖ‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•¶–æFW…Òçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWB×WBW‡V7FVEöWfVçG2Ò÷&–v–æÅö&Æö6·5³Òæ6ÆöæR‚“°¢W‡V7FVEöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5³%Òæ6ÆöæR‚’“°¢W‡V7FVEöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5³Òæ6ÆöæR‚’“°¢W‡V7FVEöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e·fV2³ƒc"ÂÂÕÒ“° ¢Fö7VÖVç@¢æÖ÷fUö6†ææVÂƒ‚Â"¢æW‡V7B‚&6†ææVÂ6†÷VÆBÖ÷fRF÷vâöæR6Æ÷B"“° ¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW€¢6†ææVÇ0¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ6†ææVÂæ–B‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³rÂ’Â…Ð¢“°¢76W'EöW€¢6†ææVÇ0¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ6†ææVÂæF—7Æ•öæÖR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖR‚$¶–6²"’Â6öÖR‚$†B"’Â6öÖR‚%6æ&R"•Ð¢“°¢76W'EöW€¢6†ææVÇ0¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ6†ææVÂçföÇVÖR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢µ6öÖRƒƒ’Â6öÖRƒƒ32’Â6öÖRƒƒ#"•Ð¢“°¢ÆWB7GVÅöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†7GVÅöWfVçG2ÂW‡V7FVEöWfVçG2“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'&V÷&FW&VB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&÷VæE÷G&— ¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ6†ææVÂæ–B‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³rÂ’Â…Ð¢“° ¢Fö7VÖVç@¢æÖ÷fUö6†ææVÂƒ’Â¢æW‡V7B‚&6†ææVÂ6†÷VÆBÖ÷fRFòF†Rf—'7B÷6—F–öâ"“°¢76W'EöW€¢Fö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ6†ææVÂæ–B‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³’ÂrÂ…Ð¢“°¢ÆWB×WBW‡V7FVE÷WöWfVçG2Ò÷&–v–æÅö&Æö6·5³%Òæ6ÆöæR‚“°¢W‡V7FVE÷WöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5³Òæ6ÆöæR‚’“°¢W‡V7FVE÷WöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5³Òæ6ÆöæR‚’“°¢W‡V7FVE÷WöWfVçG2çW6‚‡fV2³ƒc"ÂÂÒ“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVE÷WöWfVçG0¢“°¢Ð ¢5·FW7EÐ¢fâ6÷'G5ö6†ææVÅöWfVçEö&Æö6·5ö'•öæÖUöæE÷G—U÷v—F†÷WEö6†æv–æuöFF‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒrÂ„Ó°¢f÷"†–BÂ¶–æBÂæÖR’–â°¢ƒrÂBÂ%§VÇR"’À¢ƒ‚ÂÂ$¶–6²"’À¢ƒ’Â"Â%7–çF‚"’À¢ƒÂ2Â$Æ–W""’À¢ƒÂRÂ$WFöÖF–öâ"’À¢Ò°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ–BÂÂƒRÂ¶–æBÂƒC‚Â–BÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"ÂæÖR“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBF–ÂÒ³„RÂƒTÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅö&Æö6·2ÒFö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æÖ‡Æ6†ææVÇÂ°¢6†ææVÀ¢æWfVçE÷&ævR‚¢æÖ‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•¶–æFW…Òçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWB&Vf—‚ÒfV2·fV2³ƒrÂ„ÕÓ°¢ÆWBFW&Ö–æF÷"ÒfV2·fV2³ƒc"ÂÂÕÓ° ¢Fö7VÖVç@¢ç6÷'Eö6†ææVÇ2„6†ææVÅ6÷'D÷&FW#£¥G—R¢æW‡V7B‚'G—R6÷'B6†÷VÆB7V66VVB"“° ¢ÆWBG—VEö6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW€¢G—VEö6†ææVÇ0¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦–B¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³Â’Â‚ÂrÂÐ¢“°¢ÆWB×WBW‡V7FVE÷G—UöWfVçG2Ò&Vf—‚æ6ÆöæR‚“°¢f÷"–æFW‚–â³2Â"ÂÂÂEÒ°¢W‡V7FVE÷G—UöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5¶–æFW…Òæ6ÆöæR‚’“°¢Ð¢W‡V7FVE÷G—UöWfVçG2æW‡FVæB‡FW&Ö–æF÷"æ6ÆöæR‚’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVE÷G—UöWfVçG0¢“° ¢Fö7VÖVç@¢ç6÷'Eö6†ææVÇ2„6†ææVÅ6÷'D÷&FW#£¤æÖR¢æW‡V7B‚&æÖR6÷'B6†÷VÆB7V66VVB"“° ¢ÆWBæÖVEö6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW€¢æÖVEö6†ææVÇ0¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦–B¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³Â‚ÂÂ’ÂuÐ¢“°¢76W'EöW€¢æÖVEö6†ææVÇ0¢æ—FW"‚¢æÖ„6†ææVÅ7VÖÖ'“£¦F—7Æ•öæÖR¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢°¢6öÖR‚$WFöÖF–öâ"’À¢6öÖR‚$¶–6²"’À¢6öÖR‚$Æ–W""’À¢6öÖR‚%7–çF‚"’À¢6öÖR‚%§VÇR"¢Ð¢“°¢ÆWB×WBW‡V7FVEöæÖUöWfVçG2Ò&Vf—ƒ°¢f÷"–æFW‚–â³BÂÂ2Â"ÂÒ°¢W‡V7FVEöæÖUöWfVçG2æW‡FVæB†÷&–v–æÅö&Æö6·5¶–æFW…Òæ6ÆöæR‚’“°¢Ð¢W‡V7FVEöæÖUöWfVçG2æW‡FVæB‡FW&Ö–æF÷"“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2æ6ÆöæR‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöæÖUöWfVçG0¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚gF–Â’“°¢Ð ¢5·FW7EÐ¢fâ7&VFW5÷6×ÆW%ö6†ææVÅö&Vf÷&Uö6†ææVÅ÷FW&Ö–æF÷"‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3BÀ¢gWFce÷&ö¦V7E÷7G&–ær‚"÷6×ÆW2ö¶–6²çvb"’À¢“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â$¶–6²"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„RÂƒTÒ“°¢–çWE³âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“° ¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÅö–BÒFö7VÖVç@¢æ7&VFU÷6×ÆW%ö6†ææVÂ‚"÷6×ÆW2÷6æ&Rçvb"Â%6æ&R"¢æW‡V7B‚%6×ÆW"6†ææVÂ6†÷VÆB&R7&VFVB"“° ¢76W'EöW†6†ææVÅö–BÂ‚“°¢76W'EöW†Fö7VÖVçBæ†VFW"‚’æÆVv7•ö6†ææVÅö6÷VçB‚’Â"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW†6†ææVÇ2æÆVâ‚’Â"“°¢76W'EöW†6†ææVÇ5³ÒæF—7Æ•öæÖR‚’Â6öÖR‚$¶–6²"’“°¢76W'EöW†6†ææVÇ5³Òæ–B‚’Â6†ææVÅö–B“°¢76W'EöW†6†ææVÇ5³Òæ¶–æB‚’Â6öÖRƒ’“°¢76W'EöW†6†ææVÇ5³ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†6†ææVÇ5³ÒæF—7Æ•öæÖR‚’Â6öÖR‚%6æ&R"’“°¢76W'EöW†6†ææVÇ5³Òç6×ÆU÷F‚‚’Â6öÖR‚"÷6×ÆW2÷6æ&Rçvb"’“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚e³„RÂƒTÒ’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&7&VFVB6†ææVÂ6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚’æÆVâ‚’Â"“°¢76W'EöW€¢&÷VæE÷G&—æ6†ææVÇ2‚•³Òç6×ÆU÷F‚‚’À¢6öÖR‚"÷6×ÆW2÷6æ&Rçvb"¢“°¢Ð ¢5·FW7EÐ¢fâ7&VFW5öVF–õö6Æ—ö6†ææVÅö&Vf÷&Uö6†ææVÅ÷FW&Ö–æF÷"‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂrÂÂƒRÂÓ°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3BÀ¢gWFce÷&ö¦V7E÷7G&–ær‚"÷6×ÆW2ö¶–6²çvb"’À¢“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â$¶–6²"“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB×WB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂe³„RÂƒTÒ“°¢–çWE³âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“° ¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÅö–BÒFö7VÖVç@¢æ7&VFUöVF–õö6†ææVÂ‚"÷6×ÆW2÷F¶Rçvb"Â%F¶R"¢æW‡V7B‚$VF–ò6Æ—6†ææVÂ6†÷VÆB&R7&VFVB"“° ¢76W'EöW†6†ææVÅö–BÂ‚“°¢76W'EöW†Fö7VÖVçBæ†VFW"‚’æÆVv7•ö6†ææVÅö6÷VçB‚’Â"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW†6†ææVÇ2æÆVâ‚’Â"“°¢76W'EöW†6†ææVÇ5³Òæ–B‚’Â6†ææVÅö–B“°¢76W'EöW†6†ææVÇ5³Òæ¶–æB‚’Â6öÖRƒB’“°¢76W'EöW†6†ææVÇ5³ÒæVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†6†ææVÇ5³ÒæF—7Æ•öæÖR‚’Â6öÖR‚%F¶R"’“°¢76W'EöW†6†ææVÇ5³Òç6×ÆU÷F‚‚’Â6öÖR‚"÷6×ÆW2÷F¶Rçvb"’“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚&Fö7VÖVçB6†÷VÆBVæ6öFR"“°¢76W'B†Væ6öFVBæVæG5÷v—F‚‚e³„RÂƒTÒ’“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&7&VFVB6†ææVÂ6†÷VÆB'6R"“°¢76W'EöW‡&÷VæE÷G&—æ6†ææVÇ2‚•³Òæ¶–æB‚’Â6öÖRƒB’“°¢76W'EöW€¢&÷VæE÷G&—æ6†ææVÇ2‚•³Òç6×ÆU÷F‚‚’À¢6öÖR‚"÷6×ÆW2÷F¶Rçvb"¢“°¢Ð ¢5·FW7EÐ¢fâFV6öFW5öæEöVF—G5ö6†ææVÅöÆWfVÇ5÷v—F†÷WE÷F÷V6†–æuö÷F†W%öÆWfVÅöf–VÆG2‚’°¢ÆWBF–ÂÒ³„S²eÓ°¢ÆWB–çWBÒ6†ææVÅ÷v—F…öÆWfVÇ5öf—‡GW&Rƒ’Â5ó#Â…ósSÂgF–Â“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó° ¢76W'EöW†6†ææVÂçföÇVÖR‚’Â6öÖRƒ…ósS’“°¢76W'EöW†6†ææVÂçâ‚’Â6öÖRƒ5ó#’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢Fö7VÖVç@¢ç6WEö6†ææVÅöÆWfVÇ2ƒ’Â%óÂ•óc¢æW‡V7B‚'F†RÖöFW&âÆWfVÇ2WfVçB6†÷VÆB&RVF—F&ÆR"“° ¢ÆWB6†ææVÂÒfFö7VÖVçBæ6†ææVÇ2‚•³Ó°¢76W'EöW†6†ææVÂçföÇVÖR‚’Â6öÖRƒ%ó’“°¢76W'EöW†6†ææVÂçâ‚’Â6öÖRƒ•óc’“°¢ÆWBÆWfVÇ2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„D"¢æW‡V7B‚&ÆWfVÇ2WfVçB6†÷VÆB&VÖ–â"“°¢76W'EöW‚fÆWfVÇ2ç–ÆöB‚•²âãEÒÂc•óc“3"çFõöÆUö'—FW2‚’“°¢76W'EöW‚fÆWfVÇ2ç–ÆöB‚•³Bâã…ÒÂc%óS3"çFõöÆUö'—FW2‚’“°¢76W'EöW‚fÆWfVÇ2ç–ÆöB‚•³‚âåÒÂgF–Â“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³ÒçföÇVÖR‚’Â6öÖRƒ%ó’“°¢76W'EöW‡&W'6VBæ6†ææVÇ2‚•³Òçâ‚’Â6öÖRƒ•óc’“°¢Ð ¢5·FW7EÐ¢fâ&VfW'5öÖöFW&åö6†ææVÅöÆWfVÇ5ö÷fW%öÆVv7•ö6†ææVÅö6öçG&öÇ2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂ2ÂÂƒ"Â“’ÂƒC‚Âƒ#Âƒ2ÂrÂƒC’Âƒ3ÂÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒRÂBÂ„D%Ò“°¢ÆWB×WB–ÆöBÒeóC“3"çFõöÆUö'—FW2‚’çFõ÷fV2‚“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚cóS3"çFõöÆUö'—FW2‚’“°¢–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚e³²eÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚g7WW#£¦Væ6öFUöÆV##‚‡–ÆöBæÆVâ‚’2S3"’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚g–ÆöB“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒçföÇVÖR‚’Â6öÖRƒó’“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³Òçâ‚’Â6öÖRƒeóC’“°¢Ð ¢5·FW7EÐ¢fâ6†ææVÅöÆWfVÅöVF—E÷&V¦V7G5ö÷WEööe÷&ævU÷fÇVW5öæEöÆVv7•ööæÇ•ö6†ææVÇ2‚’°¢ÆWB–çWBÒ6†ææVÅ÷v—F…öÆWfVÇ5öf—‡GW&RƒBÂeóCÂóÂe³²eÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B†Fö7VÖVçBç6WEö6†ææVÅöÆWfVÇ2ƒBÂ%óƒÂeóC’æ—5öW'"‚’“°¢76W'B†Fö7VÖVçBç6WEö6†ææVÅöÆWfVÇ2ƒBÂóÂÓ’æ—5öW'"‚’“° ¢ÆWBÆVv7’ÒfÇöf—‡GW&R‚e³ƒCÂBÂÂƒ"ÂÂƒ2ÂcBÂƒc"ÂÂÒÂeµÒÂeµÒ“°¢ÆWB×WBÆVv7’ÒfÇFö7VÖVçC£§'6R‚fÆVv7’’æW‡V7B‚&ÆVv7’f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW†ÆVv7’æ6†ææVÇ2‚•³ÒçföÇVÖR‚’Â6öÖRƒ’“°¢76W'EöW†ÆVv7’æ6†ææVÇ2‚•³Òçâ‚’Â6öÖRƒcB’“°¢76W'B†ÆVv7’ç6WEö6†ææVÅöÆWfVÇ2ƒBÂóÂeóC’æ—5öW'"‚’“°¢Ð ¢5·FW7EÐ¢fâFöW5öæ÷E÷G&VE÷ÇVv–åö6†ææVÅöf7F÷'•öFFö5ö÷6×ÆU÷F‚‚’°¢ÆWB–çWBÒ6†ææVÅ÷v—F…÷6×ÆU÷F…öf—‡GW&Rƒ"Â""TdÅ7GVF–ôf7F÷'”FFUÄFFÇ6÷VæG2çvb"“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“° ¢76W'EöW†6†ææVÇ5³Òç6×ÆU÷F‚‚’ÂæöæR“°¢Ð ¢5·FW7EÐ¢fâÆ÷76ÆW75÷&÷VæGG&—÷&W6W'fW5÷Væ¶æ÷våöWfVçEö'—FW5öæEö6öçF–æW%öW‡FVç6–öç2‚’°¢ÆWBWfVçE÷7G&VÒÒ³„dbÂƒƒ"ÂƒÂ„Â„#%Ó°¢ÆWB†VFW%öW‡FVç6–öâÒ³„32Â„CEÓ°¢ÆWBG&–Æ–ærÒ³„SRÂ„ceÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂf†VFW%öW‡FVç6–öâÂgG&–Æ–ær“° ¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBæ†VFW"‚’ç‚’Â“b“°¢76W'EöW†Fö7VÖVçBæ†VFW"‚’æW‡FVç6–öâ‚’Âf†VFW%öW‡FVç6–öâ“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚’æÆVâ‚’Â“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•³Òç–ÆöB‚’Âe³„Â„#%Ò“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•³ÒæVæ6öF–ær‚’À¢e–ÆöDVæ6öF–æs£¤FF°¢ÆVæwF…÷&Vf—ƒ¢fV2³ƒƒ"ÂƒÐ¢Ð¢“°¢76W'EöW†Fö7VÖVçBçG&–Æ–æuö'—FW2‚’ÂgG&–Æ–ær“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâGFW&åö6öçG&öÆÆW%÷ö–çG5öFV6öFUöVF—EöæE÷&÷VæGG&—öÆ÷76ÆW76Ç’‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#Â³„RÂƒTÒÂ2Âƒ“Âƒtd3ó“°¢ÆWB6V6öæBÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒCƒÂ³„C2ÂƒD5ÒÂrÂƒ$Âã#Vc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚g6V6öæB“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBGFW&âÒfFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"•³Ó°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'2æÆVâ‚’Â"“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç÷6—F–öâÂ#“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òæ6†ææVÂÂ2“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒæfÆw2Âƒ““°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVUö&—G2Âƒtd3ó“°¢76W'B‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’æ—5öæâ‚’“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’Âã#R“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢Fö7VÖVç@¢æVF—E÷GFW&åö6öçG&öÆÆW"€¢rÀ¢À¢GFW&ä6öçG&öÆÆW$VF—B°¢÷6—F–öã¢6öÖRƒssr’À¢fÇVS¢6öÖRƒãsR’À¢ÒÀ¢¢æW‡V7B‚'F†R6VÆV7FVB6öçG&öÆÆW"ö–çB6†÷VÆB&RVF—F&ÆR"“°¢ÆWBGFW&âÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ó°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç÷6—F–öâÂssr“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„C2ÂƒD5Ò“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òæ6†ææVÂÂr“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒæfÆw2Âƒ$“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãsR“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVUö&—G2Âƒtd3ó“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢ÆWBVF—FVE÷–ÆöBÒ&W'6V@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„Db¢æW‡V7B‚&6öçG&öÆÆW"WfVçB6†÷VÆB&VÖ–â"¢ç–ÆöB‚“°¢ÆWB×WBW‡V7FVE÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢ÆWB×WBW‡V7FVE÷6V6öæBÒ6V6öæC°¢W‡V7FVE÷6V6öæE²âãEÒæ6÷•ög&öÕ÷6Æ–6R‚csswS3"çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷6V6öæE³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cãsVc3"çFõö&—G2‚’çFõöÆUö'—FW2‚’“°¢W‡V7FVE÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fW‡V7FVE÷6V6öæB“°¢76W'EöW†VF—FVE÷–ÆöBÂW‡V7FVE÷–ÆöB“°¢Ð ¢5·FW7EÐ¢fâÖÆf÷&ÖVE÷GFW&åö6öçG&öÆÆW%÷–ÆöEö—5÷&V¦V7FVB‚’°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂe³²Ò“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&6öçF–æW"f—‡GW&R6†÷VÆB'6R"“° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBçGFW&ç2‚’À¢W'"„fÇW'&÷#£¤–çfÆ–DWfVçB²ââÒ¢’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÕö–ç6W'G5öWfVçE÷v—F†÷WEö÷FV×ÆFUöæE÷&÷VæGG&—2‚’°¢ÆWB×WBG&–Æ–æuöWfVçG2ÒfV2³„dRÂƒ"Â„Â„$"ÂƒCÂ‚ÂÓ°¢VæEöFFöWfVçB‚f×WBG&–Æ–æuöWfVçG2Â„CÂeµÒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂgG&–Æ–æuöWfVçG2“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6öçG&öÆÆW%ö–æFW‚ÒFö7VÖVç@¢æ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÒƒrÂ2Âƒ“Â#CÂãsR¢æW‡V7B‚&æWr&r7G&VÒ6†÷VÆBæ÷BæVVBâW†—7F–ær&V6÷&BFV×ÆFR"“°¢76W'EöW†6öçG&öÆÆW%ö–æFW‚Â“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"“°¢76W'EöW‡GFW&ç5³Òæ–BÂr“°¢76W'EöW‡GFW&ç5³Òæ6öçG&öÆÆW'2æÆVâ‚’Â“°¢76W'EöW€¢GFW&ç5³Òæ6öçG&öÆÆW'5³ÒÀ¢GFW&ä6öçG&öÆÆW"°¢÷6—F–öã¢#CÀ¢&W6W'fVC¢³²%ÒÀ¢6†ææVÃ¢2À¢fÆw3¢ƒ“À¢fÇVUö&—G3¢ãsVc3"çFõö&—G2‚’À¢Ð¢“°¢76W'B‡GFW&ç5³Òæ6öçG&öÆÆW'2æ—5öV×G’‚’“° ¢ÆWBWfVçG2ÒFö7VÖVçBæWfVçG2‚“°¢ÆWB6öçG&öÆÆW%öWfVçEö–æFW‚ÒWfVçG0¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„Db¢æW‡V7B‚&7&VF–ærF†R7G&VÒ6†÷VÆBFB„DbWfVçB"“°¢ÆWBæW‡E÷GFW&åö–æFW‚ÒWfVçG0¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’ÓÒ³‚ÂÒ¢æW‡V7B‚'F†RföÆÆ÷v–ærGFW&âÖ&¶W"6†÷VÆB&VÖ–â"“°¢76W'B†6öçG&öÆÆW%öWfVçEö–æFW‚ÂæW‡E÷GFW&åö–æFW‚“°¢76W'EöW€¢WfVçG0¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢æW‡V7B‚'Vç&VÆFVBWfVçB6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢e³„dRÂƒ"Â„Â„$%Ð¢“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&æWr7G&VÒ6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&æWr7G&VÒ6†÷VÆB'6Rv–â"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢76W'EöW‡&W'6VBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’ÂVæ6öFVB“°¢Ð ¢5·FW7EÐ¢fâ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÕöVæG5÷FõöW†—7F–æuöWfVçEöæE÷&V¦V7G5öGWÆ–6FW2‚’°¢ÆWBW†—7F–æu÷&V6÷&BÐ¢GFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#Â³„RÂƒTÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂfW†—7F–æu÷&V6÷&B“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6öçG&öÆÆW%ö–æFW‚ÒFö7VÖVç@¢æ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÒƒrÂ2Âƒ“Â#CÂãsR¢æW‡V7B‚&æWr&r7G&VÒ6†÷VÆBVæBÆöæw6–FRâVç&VÆFVB7G&VÒ"“°¢76W'EöW†6öçG&öÆÆW%ö–æFW‚Â“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'2æÆVâ‚’Â"“°¢76W'EöW†6öçG&öÆÆW'5³ÒæVæ6öFR‚’ÂW†—7F–æu÷&V6÷&B“°¢76W'EöW†6öçG&öÆÆW'5³Òæ6†ææVÂÂ2“°¢76W'EöW†6öçG&öÆÆW'5³ÒæfÆw2Âƒ““°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãsR“° ¢ÆWB&Vf÷&Uö–çfÆ–EöVF—BÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢Fö7VÖVç@¢æ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÒƒrÂ2Âƒ“ÂCƒÂã¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ7&VFU÷GFW&åö6öçG&öÆÆW%÷7G&VÒƒrÂBÂƒ“ÂCƒÂc3#£¤”äd”ä•E’¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&Uö–çfÆ–EöVF—B“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&7&VFVB7G&V×26†÷VÆB'6Rv–â"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢Ð ¢5·FW7EÐ¢fâGFW&åö6öçG&öÆÆW%öVF—E÷&V¦V7G5öæöåöf–æ—FU÷fÇVW5÷v—F†÷WEö×WFF–öâ‚’°¢ÆWB&V6÷&BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂÂÂãVc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg&V6÷&B“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Fö7VÖVç@¢æVF—E÷GFW&åö6öçG&öÆÆW"€¢rÀ¢À¢GFW&ä6öçG&öÆÆW$VF—B°¢fÇVS¢6öÖR†c3#£¤”äd”ä•E’’À¢âåGFW&ä6öçG&öÆÆW$VF—C£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâGWÆ–6FU÷GFW&åö6öçG&öÆÆW%ö6÷–W5÷Væ¶æ÷våöf–VÆG5öæE÷&÷VæGG&—2‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#Â³„RÂƒTÒÂ2Âƒ“Âã#Vc3"çFõö&—G2‚’“°¢ÆWBFV×ÆFRÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒCƒÂ³„C2ÂƒD5ÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dRÂƒ"Â„Â„$%Ó°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gFV×ÆFR“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢6öçG&öÆÆW%öWfVçBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷våöWfVçB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBæWuö–æFW‚ÒFö7VÖVç@¢æGWÆ–6FU÷GFW&åö6öçG&öÆÆW"ƒrÂÂssrÂãsR¢æW‡V7B‚&âW†—7F–ærö–çB6†÷VÆB&÷f–FRâW†7B&V6÷&BFV×ÆFR"“°¢76W'EöW†æWuö–æFW‚Â"“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'2æÆVâ‚’Â2“°¢76W'EöW†6öçG&öÆÆW'5³Òç÷6—F–öâÂ#“°¢76W'EöW†6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãR“°¢76W'EöW†6öçG&öÆÆW'5³%Òç÷6—F–öâÂssr“°¢76W'EöW†6öçG&öÆÆW'5³%Òç&W6W'fVBÂ³„C2ÂƒD5Ò“°¢76W'EöW†6öçG&öÆÆW'5³%Òæ6†ææVÂÂr“°¢76W'EöW†6öçG&öÆÆW'5³%ÒæfÆw2Âƒ$“°¢76W'EöW†6öçG&öÆÆW'5³%ÒçfÇVR‚’ÂãsR“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&GWÆ–6FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢76W'EöW€¢&W'6V@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢æW‡V7B‚'Væ¶æ÷vâWfVçB6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“°¢Ð ¢5·FW7EÐ¢fâG&v–æu÷GFW&åö6öçG&öÆÆW%÷ö–çG5÷WFFW5öæEöVæG5öFöÖ–6ÆÇ’‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#Â³„RÂƒTÒÂ2Âƒ“Âã#Vc3"çFõö&—G2‚’“°¢ÆWB÷F†W%÷7G&VÒÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒCƒÂ³„C2ÂƒD5ÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dRÂƒ"Â„Â„$%Ó°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚f÷F†W%÷7G&VÒ“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢6öçG&öÆÆW%öWfVçBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷våöWfVçB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBFFVBÒFö7VÖVç@¢æG&u÷GFW&åö6öçG&öÆÆW%÷ö–çG2ƒrÂƒ2Âƒ“’Âe²ƒ#ÂãR’Âƒ#CÂã#R’Âƒ#CÂãsR•Ò¢æW‡V7B‚'F†R&r7G&VÒ6†÷VÆB66WB–çFVB7G&ö¶R"“°¢76W'EöW†FFVBÂ“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'2æÆVâ‚’Â2“°¢76W'EöW†6öçG&öÆÆW'5³Òç÷6—F–öâÂ#“°¢76W'EöW†6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãR“°¢76W'EöW†6öçG&öÆÆW'5³ÒæVæ6öFR‚’Â÷F†W%÷7G&VÒ“°¢76W'EöW†6öçG&öÆÆW'5³%Òç÷6—F–öâÂ#C“°¢76W'EöW†6öçG&öÆÆW'5³%Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW†6öçG&öÆÆW'5³%Òæ6†ææVÂÂ2“°¢76W'EöW†6öçG&öÆÆW'5³%ÒæfÆw2Âƒ““°¢76W'EöW†6öçG&öÆÆW'5³%ÒçfÇVR‚’ÂãsR“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&G&vâ&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢76W'EöW€¢&W'6V@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢æW‡V7B‚'Væ¶æ÷vâWfVçB6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“° ¢ÆWB&Vf÷&Uö–çfÆ–EöVF—BÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢Fö7VÖVç@¢æG&u÷GFW&åö6öçG&öÆÆW%÷ö–çG2ƒrÂƒ2Âƒ“’Âe²ƒ3cÂc3#£¤äâ•Ò¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&Uö–çfÆ–EöVF—B“°¢Ð ¢5·FW7EÐ¢fâÆ–æV&—¦U÷GFW&åö6öçG&öÆÆW%÷&ævU÷&W6W'fW5ö÷VUö'—FW5öæEö÷F†W%÷7G&V×2‚’°¢ÆWBEós"ÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒs"Â³„RÂƒTÒÂ2Âƒ“Âã&c3"çFõö&—G2‚’“°¢ÆWB÷F†W%÷7G&VÒÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³„C2ÂƒD5ÒÂrÂƒ$ÂC"ãc3"çFõö&—G2‚’“°¢ÆWBEóÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³ƒ3Âƒ3%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWBEóC‚ÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³ƒC2ÂƒCEÒÂ2Âƒ“ÂãFc3"çFõö&—G2‚’“°¢ÆWBEó#BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³ƒ#Âƒ#%ÒÂ2Âƒ“Âã–c3"çFõö&—G2‚’“°¢ÆWBEó“bÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ“bÂ³ƒ“RÂƒ“eÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWB&V6÷&G2Ò¶Eós"Â÷F†W%÷7G&VÒÂEóÂEóC‚ÂEó#BÂEó“eÓ°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒfV3£¦æWr‚“°¢f÷"&V6÷&B–â&V6÷&G2°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚g&V6÷&B“°¢Ð¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6†ævVBÒFö7VÖVç@¢æÆ–æV&—¦U÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’Â“bÂ¢æW‡V7B‚'F†R6VÆV7FVB&r7G&VÒ6†÷VÆB&RÆ–æV&—¦VB"“°¢76W'EöW†6†ævVBÂ2“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'2æÆVâ‚’Âb“°¢76W'EöW†6öçG&öÆÆW'5³Òç÷6—F–öâÂs"“°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãsR“°¢76W'EöW†6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW†6öçG&öÆÆW'5³ÒæVæ6öFR‚’Â÷F†W%÷7G&VÒ“°¢76W'EöW†6öçG&öÆÆW'5³%Òç÷6—F–öâÂ“°¢76W'EöW†6öçG&öÆÆW'5³%ÒçfÇVR‚’Âã“°¢76W'EöW†6öçG&öÆÆW'5³5Òç÷6—F–öâÂC‚“°¢76W'EöW†6öçG&öÆÆW'5³5ÒçfÇVR‚’ÂãR“°¢76W'EöW†6öçG&öÆÆW'5³5Òç&W6W'fVBÂ³ƒC2ÂƒCEÒ“°¢76W'EöW†6öçG&öÆÆW'5³EÒç÷6—F–öâÂ#B“°¢76W'EöW†6öçG&öÆÆW'5³EÒçfÇVR‚’Âã#R“°¢76W'EöW†6öçG&öÆÆW'5³EÒç&W6W'fVBÂ³ƒ#Âƒ#%Ò“°¢76W'EöW†6öçG&öÆÆW'5³UÒç÷6—F–öâÂ“b“°¢76W'EöW†6öçG&öÆÆW'5³UÒçfÇVR‚’Âã“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&Æ–æV&—¦VBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢ÆWB&Vf÷&Uöæö÷ÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'EöW€¢Fö7VÖVç@¢æÆ–æV&—¦U÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’Â#BÂC‚¢çVçw&‚’À¢ ¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&Uöæö÷“°¢Ð ¢5·FW7EÐ¢fâÆ–æV&—¦U÷GFW&åö6öçG&öÆÆW%÷&ævU÷&V¦V7G5öæöåöf–æ—FUöVæGö–çE÷fÇVW2‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³„RÂƒTÒÂ2Âƒ“Âc3#£¤äâçFõö&—G2‚’“°¢ÆWBÖ–FFÆRÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³ƒ3Âƒ3%ÒÂ2Âƒ“ÂãVc3"çFõö&—G2‚’“°¢ÆWBÆ7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³ƒ“RÂƒ“eÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fÖ–FFÆR“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚fÆ7B“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Fö7VÖVç@¢æÆ–æV&—¦U÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’ÂÂC‚¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâ7Æ–æU÷GFW&åö6öçG&öÆÆW%÷&ævU÷W6W5÷6Öö÷F‡7FWöæE÷&W6W'fW5÷&uöf–VÆG2‚’°¢ÆWBEós"ÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒs"Â³ƒ“RÂƒ“eÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWBEóÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³„RÂƒTÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWBEó#BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³ƒ#Âƒ#%ÒÂ2Âƒ“Âã–c3"çFõö&—G2‚’“°¢ÆWBEó“bÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ“bÂ³ƒ3Âƒ3%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWB&V6÷&G2Ò¶Eós"ÂEóÂEó#BÂEó“eÓ°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒfV3£¦æWr‚“°¢f÷"&V6÷&B–â&V6÷&G2°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚g&V6÷&B“°¢Ð¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢ç7Æ–æU÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’ÂÂ“b¢æW‡V7B‚'F†R6VÆV7FVB&r7G&VÒ6†÷VÆBW6R7Æ–æR–çFW'öÆF–öâ"’À¢ ¢“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'5³Òç÷6—F–öâÂs"“°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’ÂãƒC3sR“°¢76W'EöW†6öçG&öÆÆW'5³Òç&W6W'fVBÂ³ƒ“RÂƒ“eÒ“°¢76W'EöW†6öçG&öÆÆW'5³Òç÷6—F–öâÂ“°¢76W'EöW†6öçG&öÆÆW'5³ÒçfÇVR‚’Âã“°¢76W'EöW†6öçG&öÆÆW'5³%Òç÷6—F–öâÂ#B“°¢76W'EöW†6öçG&öÆÆW'5³%ÒçfÇVR‚’ÂãSc#R“°¢76W'EöW†6öçG&öÆÆW'5³%Òç&W6W'fVBÂ³ƒ#Âƒ#%Ò“°¢76W'EöW†6öçG&öÆÆW'5³5Òç÷6—F–öâÂ“b“°¢76W'EöW†6öçG&öÆÆW'5³5ÒçfÇVR‚’Âã“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'7Æ–æRÖVF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“°¢Ð ¢5·FW7EÐ¢fâFVÆWFU÷GFW&åö6öçG&öÆÆW%÷&ævU÷&VÖ÷fW5ööæÇ•öÖF6†–æu÷7G&VÕ÷&V6÷&G2‚’°¢ÆWBF&vWEóÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³„Â„ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWB÷F†W%ó#BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³„#Â„#ÒÂrÂƒ$Âã#Vc3"çFõö&—G2‚’“°¢ÆWBF&vWEó#BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³„"Â„5ÒÂ2Âƒ“Âã&c3"çFõö&—G2‚’“°¢ÆWBF&vWEóC‚ÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³„BÂ„UÒÂ2Âƒ“ÂãFc3"çFõö&—G2‚’“°¢ÆWB÷F†W%óC‚ÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³„#"Â„#5ÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWBF&vWEós"ÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒs"Â³„bÂ„uÒÂ2Âƒ“Âãfc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢ÆWB×WBf—'7E÷–ÆöBÒF&vWEóçFõ÷fV2‚“°¢f—'7E÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚f÷F†W%ó#B“°¢f—'7E÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gF&vWEó#B“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂff—'7E÷–ÆöB“°¢ÆWB×WB6V6öæE÷–ÆöBÒF&vWEóC‚çFõ÷fV2‚“°¢6V6öæE÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚f÷F†W%óC‚“°¢6V6öæE÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gF&vWEós"“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg6V6öæE÷–ÆöB“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dRÂƒ"Â„4Â„dUÓ°¢6öçG&öÆÆW%öWfVçBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷våöWfVçB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æFVÆWFU÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’ÂC‚Â#B¢æW‡V7B‚'F†R6VÆV7FVB&r7G&VÒ&ævR6†÷VÆB&RW&6VB"’À¢ ¢“°¢ÆWB6öçG&öÆÆW'2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'3°¢76W'EöW†6öçG&öÆÆW'2æÆVâ‚’ÂB“°¢76W'EöW†6öçG&öÆÆW'5³ÒæVæ6öFR‚’ÂF&vWEó“°¢76W'EöW†6öçG&öÆÆW'5³ÒæVæ6öFR‚’Â÷F†W%ó#B“°¢76W'EöW†6öçG&öÆÆW'5³%ÒæVæ6öFR‚’Â÷F†W%óC‚“°¢76W'EöW†6öçG&öÆÆW'5³5ÒæVæ6öFR‚’ÂF&vWEós"“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢æW‡V7B‚'Væ¶æ÷vâWfVçB6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&W&6VBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚’ÂFö7VÖVçBçGFW&ç2‚’çVçw&‚’“° ¢ÆWB×WBÆÅ÷7G&VÕ÷–ÆöBÒF&vWEó#BçFõ÷fV2‚“°¢ÆÅ÷7G&VÕ÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚gF&vWEóC‚“°¢ÆWB×WBÆÅ÷7G&VÕöWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WBÆÅ÷7G&VÕöWfVçBÂ„DbÂfÆÅ÷7G&VÕ÷–ÆöB“°¢ÆÅ÷7G&VÕöWfVçBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷våöWfVçB“°¢ÆWBÆÅ÷7G&VÕö–çWBÒGFW&åöf—‡GW&R‚eµÒÂfÆÅ÷7G&VÕöWfVçB“°¢ÆWB×WBÆÅ÷7G&VÕöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚fÆÅ÷7G&VÕö–çWB’æW‡V7B‚'6–ævÆR7G&VÒf—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢ÆÅ÷7G&VÕöFö7VÖVç@¢æFVÆWFU÷GFW&åö6öçG&öÆÆW%÷&ævRƒrÂƒ2Âƒ“’ÂÂS3#£¤Ô‚¢æW‡V7B‚'F†R6VÆV7FVB7G&VÒ6†÷VÆB&RW&6VB"’À¢ ¢“°¢76W'B€¢ÆÅ÷7G&VÕöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ð¢æ6öçG&öÆÆW'0¢æ—5öV×G’‚¢“°¢76W'B€¢ÆÅ÷7G&VÕöFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÆÂ‡ÆWfVçGÂWfVçBæ÷6öFR‚’Ò„Db¢“°¢76W'B€¢ÆÅ÷7G&VÕöFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æç’‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢“°¢Ð ¢5·FW7EÐ¢fâFVÆWFU÷GFW&åö6öçG&öÆÆW%÷&VÖ÷fW5ööæÇ•÷F†U÷&WVW7FVE÷&V6÷&G2‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#Â³„RÂƒTÒÂ2Âƒ“Âã#Vc3"çFõö&—G2‚’“°¢ÆWB6V6öæBÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒCƒÂ³„C2ÂƒD5ÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dRÂƒ"Â„Â„$%Ó°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚g6V6öæB“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢6öçG&öÆÆW%öWfVçBæW‡FVæEög&öÕ÷6Æ–6R‚gVæ¶æ÷våöWfVçB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æFVÆWFU÷GFW&åö6öçG&öÆÆW"ƒrÂ¢æW‡V7B‚'F†R6VÆV7FVBö–çB6†÷VÆB&RFVÆWFVB"“°¢76W'EöW†Fö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'2æÆVâ‚’Â“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„Db¢æW‡V7B‚'F†R&VÖ–æ–ærö–çB6†÷VÆB¶VWF†RWfVçB"¢ç–ÆöB‚’À¢ff—'7@¢“° ¢Fö7VÖVç@¢æFVÆWFU÷GFW&åö6öçG&öÆÆW"ƒrÂ¢æW‡V7B‚'F†Rf–æÂö–çB6†÷VÆB&RFVÆWFVB"“°¢76W'B†Fö7VÖVçBæWfVçG2‚’æ—FW"‚’æÆÂ‡ÆWfVçGÂWfVçBæ÷6öFR‚’Ò„Db’“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&FVÆWFVBf–ÆR6†÷VÆB'6R"“°¢76W'B‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'2æ—5öV×G’‚’“°¢76W'EöW€¢&W'6V@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„dR¢æW‡V7B‚'Væ¶æ÷vâWfVçB6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“°¢Ð ¢5·FW7EÐ¢fâFF–æuöæ÷FW5÷WFFW5÷F†UöÆVæwF…÷&Vf—…öæE÷&W6W'fW5ö÷F†W%öWfVçG2‚’°¢ÆWB÷&–v–æÅöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dbÂƒ"Â„Â„$%Ó°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶÷&–v–æÅöæ÷FUÒÂgVæ¶æ÷våöWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æFE÷GFW&åöæ÷FR€¢rÀ¢GFW&äæ÷FR°¢÷6—F–öã¢#À¢6†ææVÅö–C¢À¢ÆVæwFƒ¢#BÀ¢¶W“¢cBÀ¢fVÆö6—G“¢“À¢âåGFW&äæ÷FS£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'F†Ræ÷FR6†÷VÆB&R–ç6W'FVB"“° ¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"“°¢76W'EöW‡GFW&ç5³Òææ÷FW2æÆVâ‚’Â"“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³Òç÷6—F–öâÂ#“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³ÒæfÆw2ÂEDU$åôäõDUôDTdTÅEôdÄu2“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³Òæf–æU÷—F6‚Â#“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³Òç&VÆV6RÂcB“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³ÒçâÂcB“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³ÒæÖöE÷‚Â#‚“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³ÒæÖöE÷’Â#‚“°¢ÆWBæ÷FW5öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„C¢æW‡V7B‚&æ÷FRWfVçB6†÷VÆB&VÖ–â"“°¢76W'EöW€¢æ÷FW5öWfVçBæVæ6öF–ær‚’À¢e–ÆöDVæ6öF–æs£¤FF°¢ÆVæwF…÷&Vf—ƒ¢fV2³ƒ3Ð¢Ð¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â"“°¢76W'EöW‡&W'6VBçG&–Æ–æuö'—FW2‚’Âe³„#%Ò“°¢Ð ¢5·FW7EÐ¢fâf–æU÷—F6…ööfg6WG5öÖ÷FõöfÅ÷7GVF–õ÷FVåö6VçE÷7FW2‚’°¢f÷"‡&rÂ6VçG2’–â²ƒÂÓó#’Âƒ’ÂÓ’Âƒ#Â’Âƒ#Â’Âƒ#CÂó#•Ò°¢ÆWBæ÷FRÒGFW&äæ÷FR°¢f–æU÷—F6ƒ¢&rÀ¢âåGFW&äæ÷FS£¦FVfVÇB‚¢Ó°¢76W'EöW†æ÷FRæf–æU÷—F6…ööfg6WEö6VçG2‚’Â6VçG2“°¢Ð ¢f÷"&r–â³#CÂSƒ£¤Ô…Ò°¢ÆWBæ÷FRÒGFW&äæ÷FR°¢f–æU÷—F6ƒ¢&rÀ¢âåGFW&äæ÷FS£¦FVfVÇB‚¢Ó°¢76W'EöW†æ÷FRæf–æU÷—F6…ööfg6WEö6VçG2‚’Â“°¢Ð¢Ð ¢5·FW7EÐ¢fâGFW&å÷F–ÖU÷6–væGW&W5ö&U÷66÷VEöVF—F&ÆUöæE÷&÷VæGG&—öÆ÷76ÆW76Ç’‚’°¢ÆWBGFW&å÷÷6—F–öâÒ#°¢ÆWBGFW&å÷&u÷÷6—F–öâÐ¢7WW#£¥D”ÔUôÔ$´U%õ4”täEU$Uô$•BÂƒóÂGFW&å÷÷6—F–öã°¢ÆWBÆ–Æ—7E÷÷6—F–öâÒCƒ°¢ÆWB×WBWfVçE÷7G&VÒÒfV2°¢ƒÂBÂƒ"ÂBÂƒCÂÂÂƒCÂrÂÂ„CÂÂƒCÂÂÂƒCÂrÂÀ¢Ó°¢VæE÷F–ÖUöÖ&¶W"€¢f×WBWfVçE÷7G&VÒÀ¢GFW&å÷&u÷÷6—F–öâÀ¢2À¢‚À¢%GFW&âÖWFW""À¢“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„dbÂ"Â„Â„$"ÂƒCÂÂÒ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÒ“°¢VæE÷F–ÖUöÖ&¶W"€¢f×WBWfVçE÷7G&VÒÀ¢7WW#£¥D”ÔUôÔ$´U%õ4”täEU$Uô$•BÂÆ–Æ—7E÷÷6—F–öâÀ¢RÀ¢BÀ¢%Æ–Æ—7BÖWFW""À¢“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBGFW&âÒFö7VÖVç@¢çGFW&ç2‚¢çVçw&‚¢æ–çFõö—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒr¢æW‡V7B‚'GFW&âr6†÷VÆBFV6öFR"“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'2æÆVâ‚’Â“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'5³Òç&u÷÷6—F–öâ‚’ÂGFW&å÷&u÷÷6—F–öâ“°¢76W'B‡GFW&âçF–ÖUöÖ&¶W'5³Òæ—5÷6–væGW&R‚’“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'5³Òç÷6—F–öå÷F–6·2‚’ÂGFW&å÷÷6—F–öâ“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'5³ÒæçVÖW&F÷"‚’Â6öÖRƒ2’“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'5³ÒæFVæöÖ–æF÷"‚’Â6öÖRƒ‚’“°¢76W'EöW‡GFW&âçF–ÖUöÖ&¶W'5³ÒææÖR‚’Â6öÖR‚%GFW&âÖWFW""’“°¢76W'EöW†Fö7VÖVçBæÖWFFF‚’çF–ÖU÷6–væGW&R‚’Â6öÖR‚ƒBÂB’’“°¢ÆWBÆ–Æ—7EöÖ&¶W'2ÒFö7VÖVçBçF–ÖUöÖ&¶W'2‚’çVçw&‚“°¢76W'EöW‡Æ–Æ—7EöÖ&¶W'2æÆVâ‚’Â“°¢76W'EöW‡Æ–Æ—7EöÖ&¶W'5³Òãç÷6—F–öå÷F–6·2‚’ÂÆ–Æ—7E÷÷6—F–öâ“°¢76W'EöW‡Æ–Æ—7EöÖ&¶W'5³ÒãæçVÖW&F÷"‚’Â6öÖRƒR’“°¢ÆWBÆ–Æ—7EöÖ&¶W%÷7VÖÖ'’ÒÆFö7VÖVçC¢dfÇFö7VÖVçGÂ°¢Fö7VÖVç@¢çF–ÖUöÖ&¶W'2‚¢çVçw&‚¢æ–çFõö—FW"‚¢æÖ‡Â†'&ævVÖVçEö–BÂÖ&¶W"—Â°¢€¢'&ævVÖVçEö–BÀ¢Ö&¶W"ç&u÷÷6—F–öâ‚’À¢Ö&¶W"æçVÖW&F÷"‚’À¢Ö&¶W"æFVæöÖ–æF÷"‚’À¢Ö&¶W"ææÖR‚’æÖ‡7G#£§Fõö÷væVB’À¢¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ó°¢ÆWB÷&–v–æÅ÷Æ–Æ—7EöÖ&¶W%÷7VÖÖ'’ÒÆ–Æ—7EöÖ&¶W%÷7VÖÖ'’‚fFö7VÖVçB“° ¢ÆWBVF—FVEö–æFW‚ÒFö7VÖVç@¢ç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂGFW&å÷÷6—F–öâÂRÂb¢æW‡V7B‚'F†RW†—7F–ærGFW&â6–væGW&R6†÷VÆB&RVF—F&ÆR"“°¢76W'EöW†VF—FVEö–æFW‚Â“°¢ÆWBVF—FVE÷GFW&âÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ“°¢76W'EöW€¢VF—FVE÷GFW&âçF–ÖUöÖ&¶W'5³Òç&u÷÷6—F–öâ‚’À¢GFW&å÷&u÷÷6—F–öà¢“°¢76W'EöW†VF—FVE÷GFW&âçF–ÖUöÖ&¶W'5³ÒæçVÖW&F÷"‚’Â6öÖRƒR’“°¢76W'EöW†VF—FVE÷GFW&âçF–ÖUöÖ&¶W'5³ÒæFVæöÖ–æF÷"‚’Â6öÖRƒb’“° ¢ÆWB7&VFVEö–æFW‚ÒFö7VÖVç@¢ç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂ3ƒBÂrÂ‚¢æW‡V7B‚&6V6öæBGFW&â6–væGW&R6†÷VÆB&R7&VFVB"“°¢76W'EöW†7&VFVEö–æFW‚Â“°¢ÆWB6–væGW&W2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’çF–ÖUöÖ&¶W'3°¢76W'EöW‡6–væGW&W2æÆVâ‚’Â"“°¢76W'EöW‡6–væGW&W5³Òç÷6—F–öå÷F–6·2‚’Â3ƒB“°¢76W'EöW‡6–væGW&W5³ÒæçVÖW&F÷"‚’Â6öÖRƒr’“°¢76W'EöW‡6–væGW&W5³ÒæFVæöÖ–æF÷"‚’Â6öÖRƒ‚’“° ¢Fö7VÖVç@¢æFVÆWFU÷GFW&å÷F–ÖU÷6–væGW&RƒrÂ3ƒB¢æW‡V7B‚'F†R7&VFVBGFW&â6–væGW&R6†÷VÆB&R&VÖ÷f&ÆR"“°¢Fö7VÖVç@¢æFVÆWFU÷GFW&å÷F–ÖU÷6–væGW&RƒrÂGFW&å÷÷6—F–öâ¢æW‡V7B‚'F†R÷&–v–æÂGFW&â6–væGW&R6†÷VÆB&R&VÖ÷f&ÆR"“°¢76W'B†Fö7VÖVçBçGFW&ç2‚’çVçw&‚•³ÒçF–ÖUöÖ&¶W'2æ—5öV×G’‚’“°¢76W'EöW†Fö7VÖVçBæÖWFFF‚’çF–ÖU÷6–væGW&R‚’Â6öÖR‚ƒBÂB’’“°¢76W'EöW€¢Æ–Æ—7EöÖ&¶W%÷7VÖÖ'’‚fFö7VÖVçB’À¢÷&–v–æÅ÷Æ–Æ—7EöÖ&¶W%÷7VÖÖ'¢“°¢76W'B€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æç’‡ÆWfVçGÂ²WfVçBæ÷6öFR‚’ÓÒ„dbbbWfVçBç–ÆöB‚’ÓÒ³„Â„$%ÒÒ¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'B‡&W'6VBçGFW&ç2‚’çVçw&‚•³ÒçF–ÖUöÖ&¶W'2æ—5öV×G’‚’“°¢76W'EöW€¢Æ–Æ—7EöÖ&¶W%÷7VÖÖ'’‚g&W'6VB’À¢÷&–v–æÅ÷Æ–Æ—7EöÖ&¶W%÷7VÖÖ'¢“°¢76W'EöW‡&W'6VBæÖWFFF‚’çF–ÖU÷6–væGW&R‚’Â6öÖR‚ƒBÂB’’“°¢Ð ¢5·FW7EÐ¢fâ6Æ–FUöæ÷FUöVF—G5÷FövvÆUööæÇ•÷F†U÷6Æ–FUöfÆuöæE÷&÷VæGG&—‚’°¢ÆWB×WB÷&–v–æÅöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB÷&–v–æÅöfÆw2Ò„TU÷Sc°¢÷&–v–æÅöæ÷FU³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚f÷&–v–æÅöfÆw2çFõöÆUö'—FW2‚’“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e¶÷&–v–æÅöæ÷FUÒÂe³„dbÂÂ„Ò’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æVF—E÷GFW&åöæ÷FR€¢rÀ¢À¢À¢GFW&äæ÷FTVF—B°¢6Æ–FS¢6öÖR‡G'VR’À¢âåGFW&äæ÷FTVF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'6Æ–FRfÆr6†÷VÆB&R6WB"“°¢ÆWBæ÷FRÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Ó°¢76W'B†æ÷FRæ—5÷6Æ–FUöæ÷FR‚’“°¢76W'EöW†æ÷FRæfÆw2Â÷&–v–æÅöfÆw2ÂEDU$åôäõDUõ4Ä”DUôdÄr“° ¢Fö7VÖVç@¢æVF—E÷GFW&åöæ÷FR€¢rÀ¢À¢À¢GFW&äæ÷FTVF—B°¢6Æ–FS¢6öÖR†fÇ6R’À¢âåGFW&äæ÷FTVF—C£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'6Æ–FRfÆr6†÷VÆB&R6ÆV&VB"“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢ÆWBæ÷FRÒg&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Ó°¢76W'B‚æ÷FRæ—5÷6Æ–FUöæ÷FR‚’“°¢76W'EöW†æ÷FRæfÆw2Â÷&–v–æÅöfÆw2“°¢76W'EöW€¢&W'6VBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂ„Ð¢“°¢Ð ¢5·FW7EÐ¢fâ66ÆUöÆWfVÇ5÷WFFW5ööæÇ•÷6VÆV7FVEö6†ææVÅ÷fVÆö6—F–W5öæE÷&÷VæGG&—2‚’°¢ÆWB×WBf—'7BÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ#“°¢f—'7E³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚c„TU÷SbçFõöÆUö'—FW2‚’“°¢f—'7E³uÒÒ„C3°¢ÆWB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&Bƒ#BÂÂS"ÂcÂcB“°¢ÆWBVç6VÆV7FVBÒæ÷FU÷&V6÷&BƒC‚ÂÂSRÂcÂC“°¢ÆWB×WB6Æ×VBÒæ÷FU÷&V6÷&Bƒs"ÂÂcÂcÂ“°¢6Æ×VE³#ÒÒƒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e¶f—'7BÂ÷F†W%ö6†ææVÂÂVç6VÆV7FVBÂ6Æ×VEÒÀ¢e³„dbÂÂ„ÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6†ævVBÒFö7VÖVç@¢ç66ÆU÷GFW&åöæ÷FU÷6VÆV7F–öåöÆWfVÇ2ƒrÂÂe³Â%ÒÂSÂ¢æW‡V7B‚'6VÆV7FVBæ÷FRÆWfVÇ26†÷VÆB66ÆR"“°¢76W'EöW†6†ævVBÂ"“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVBdÅ6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBdÅ6†÷VÆB'6R"“°¢ÆWBæ÷FW2Ò&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW5³ÒçfVÆö6—G’ÂC2“°¢76W'EöW†æ÷FW5³ÒæfÆw2Â„TR“°¢76W'EöW†æ÷FW5³Òç&W6W'fVBÂ„C2“°¢76W'EöW†æ÷FW5³ÒçfVÆö6—G’ÂcB“°¢76W'EöW†æ÷FW5³%ÒçfVÆö6—G’ÂC“°¢76W'EöW†æ÷FW5³5ÒçfVÆö6—G’Â#r“°¢76W'EöW†æ÷FW5³5ÒçâÂƒ““°¢76W'EöW€¢&W'6VBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂ„Ð¢“°¢Ð ¢5·FW7EÐ¢fâ66ÆUöÆWfVÇ5÷fÆ–FFW5öÆ–Ö—G5öæE÷&WGW&ç5÷¦W&õöf÷%÷Væ6†ævVEöæ÷FW2‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e¶æ÷FUÒÂe³„dbÂÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW†Fö7VÖVçBç66ÆU÷GFW&åöæ÷FUöÆWfVÇ2ƒrÂÂÂ’çVçw&‚’Â“°¢76W'B†Fö7VÖVçBç66ÆU÷GFW&åöæ÷FUöÆWfVÇ2ƒrÂÂ#Â’æ—5öW'"‚’“°¢76W'B†Fö7VÖVçBç66ÆU÷GFW&åöæ÷FUöÆWfVÇ2ƒrÂÂÂ’æ—5öW'"‚’“°¢76W'B€¢Fö7VÖVç@¢ç66ÆU÷GFW&åöæ÷FUöÆWfVÇ5÷v—F…ö÷F–öç2€¢rÀ¢À¢66ÆTÆWfVÇ4÷F–öç2°¢6VçFW%÷W&6VçC¢À¢âå66ÆTÆWfVÇ4÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢ç66ÆU÷GFW&åöæ÷FUöÆWfVÇ5÷v—F…ö÷F–öç2€¢rÀ¢À¢66ÆTÆWfVÇ4÷F–öç2°¢FVç6–öå÷W&6VçC¢ÓÀ¢âå66ÆTÆWfVÇ4÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢Ð ¢5·FW7EÐ¢fâ66ÆUöÆWfVÇ5öÆ–W5ö6VçFW%÷—f÷EöæEöÆöv&—F†Ö–5÷FVç6–öâ‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ3"’À¢æ÷FU÷&V6÷&Bƒ#BÂÂS"ÂcÂcB’À¢æ÷FU÷&V6÷&BƒC‚ÂÂSRÂcÂ#r’À¢ÒÀ¢e³„dbÂÂ„ÒÀ¢“°¢ÆWB×WB'&–v‡FW"ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢'&–v‡FW ¢ç66ÆU÷GFW&åöæ÷FUöÆWfVÇ5÷v—F…ö÷F–öç2€¢rÀ¢À¢66ÆTÆWfVÇ4÷F–öç2°¢FVç6–öå÷W&6VçC¢À¢âå66ÆTÆWfVÇ4÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'÷6—F—fRFVç6–öâ6†÷VÆB66ÆRæ÷FW2"“°¢ÆWB'&–v‡FW%öæ÷FW2Ò'&–v‡FW"çGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'B†'&–v‡FW%öæ÷FW5³ÒçfVÆö6—G’â3"“°¢76W'B†'&–v‡FW%öæ÷FW5³ÒçfVÆö6—G’âcB“°¢76W'EöW†'&–v‡FW%öæ÷FW5³%ÒçfVÆö6—G’Â#r“° ¢ÆWB×WB6ögFW"ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6ögFW ¢ç66ÆU÷GFW&åöæ÷FUöÆWfVÇ5÷v—F…ö÷F–öç2€¢rÀ¢À¢66ÆTÆWfVÇ4÷F–öç2°¢FVç6–öå÷W&6VçC¢ÓÀ¢âå66ÆTÆWfVÇ4÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&æVvF—fRFVç6–öâ6†÷VÆB66ÆRæ÷FW2"“°¢ÆWB6ögFW%öæ÷FW2Ò6ögFW"çGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'B‡6ögFW%öæ÷FW5³ÒçfVÆö6—G’Â3"“°¢76W'B‡6ögFW%öæ÷FW5³ÒçfVÆö6—G’ÂcB“° ¢ÆWB×WB6VçFW&VBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R€¢e¶æ÷FU÷&V6÷&BƒÂÂC‚ÂcÂcB•ÒÀ¢e³„dbÂÒÀ¢’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6VçFW&V@¢ç66ÆU÷GFW&åöæ÷FUöÆWfVÇ5÷v—F…ö÷F–öç2€¢rÀ¢À¢66ÆTÆWfVÇ4÷F–öç2°¢6VçFW%÷W&6VçC¢SÀ¢×VÇF—Æ–W%÷W&6VçC¢#À¢âå66ÆTÆWfVÇ4÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&6VçFW&VB×VÇF—Ç’6†÷VÆB66ÆR&÷VæB—G2—f÷B"“°¢76W'EöW†6VçFW&VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³ÒçfVÆö6—G’ÂcR“°¢Ð ¢5·FW7EÐ¢fâ'Vvv–F÷%÷7–æ5öÖöFW5ö6öçG&öÅ÷&WWF—F–öç5öæEö6†÷&EöVæB‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ’À¢æ÷FU÷&V6÷&BƒÂÂ#ÂcBÂ“’À¢æ÷FU÷&V6÷&BƒÂÂ#ÂcrÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢f÷"‡7–æ2ÂW‡V7FVE÷÷6—F–öç2’–â°¢„'Vvv–õ7–æ4ÖöFS£¥F–ÖRÂfV2³Â#BÂC…Ò’À¢„'Vvv–õ7–æ4ÖöFS£¤&Æö6²ÂfV2³Â#BÂC‚Âs"Â“eÒ’À¢„'Vvv–õ7–æ4ÖöFS£¤6†÷&BÂfV2³Â#EÒ’À¢Ò°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB7&VFVBÒFö7VÖVç@¢æ'Vvv–FU÷GFW&åöæ÷FW2€¢rÀ¢À¢'Vvv–ô÷F–öç2°¢7FW÷F–6·3¢#BÀ¢&ævUöö7FfW3¢À¢vFU÷W&6VçC¢À¢&ævU÷GFW&ã¢'Vvv–õ&ævUGFW&ã£¥WÀ¢7–æ2À¢ÒÀ¢¢æW‡V7B‚'7–æ2ÖöFR6†÷VÆBvVæW&FRâ'Vvv–ò"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWBvVæW&FVE÷÷6—F–öç2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†7&VFVBÂW‡V7FVE÷÷6—F–öç2æÆVâ‚’“°¢76W'EöW†vVæW&FVE÷÷6—F–öç2ÂW‡V7FVE÷÷6—F–öç2“°¢Ð¢Ð ¢5·FW7EÐ¢fâ'Vvv–F÷%÷F–ÖU÷7–æ5ö6ö×ÆWFW5ööæU÷75öf÷%÷6†÷'Eö6†÷&G2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ’À¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcBÂ“’À¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcrÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB7&VFVBÒFö7VÖVç@¢æ'Vvv–FU÷GFW&åöæ÷FW2€¢rÀ¢À¢'Vvv–ô÷F–öç2°¢7FW÷F–6·3¢#BÀ¢&ævUöö7FfW3¢À¢vFU÷W&6VçC¢À¢&ævU÷GFW&ã¢'Vvv–õ&ævUGFW&ã£¥WÀ¢7–æ3¢'Vvv–õ7–æ4ÖöFS£¥F–ÖRÀ¢ÒÀ¢¢æW‡V7B‚%F–ÖR7–æ26†÷VÆB6ö×ÆWFR—G2öæR×72Æö÷"“° ¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWBvVæW&FVE÷÷6—F–öç2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†7&VFVBÂ2“°¢76W'EöW†vVæW&FVE÷÷6—F–öç2ÂfV2³Â#BÂC…Ò“°¢Ð ¢5·FW7EÐ¢fâ'Vvv–F÷%öÇFW&æFU÷&ævU÷GFW&å÷&WVG5÷GW&æ–æuöæ÷FW2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂCBÂcÂ’À¢æ÷FU÷&V6÷&BƒÂÂCBÂcBÂ“’À¢æ÷FU÷&V6÷&BƒÂÂCBÂcrÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢f÷"‡&ævU÷GFW&âÂW‡V7FVEö¶W—2’–â°¢„'Vvv–õ&ævUGFW&ã£¥WF÷vâÂfV2³cÂcBÂcrÂcEÒ’À¢€¢'Vvv–õ&ævUGFW&ã£¤ÇFW&æFRÀ¢fV2³cÂcBÂcrÂcrÂcBÂcÒÀ¢’À¢Ò°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB7&VFVBÒFö7VÖVç@¢æ'Vvv–FU÷GFW&åöæ÷FW2€¢rÀ¢À¢'Vvv–ô÷F–öç2°¢7FW÷F–6·3¢#BÀ¢&ævUöö7FfW3¢À¢vFU÷W&6VçC¢À¢&ævU÷GFW&âÀ¢7–æ3¢'Vvv–õ7–æ4ÖöFS£¥F–ÖRÀ¢ÒÀ¢¢æW‡V7B‚'&ævRGFW&â6†÷VÆBvVæW&FR&–fb"“° ¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWBvVæW&FVEö¶W—2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂæ÷FRæ¶W’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†7&VFVBÂW‡V7FVEö¶W—2æÆVâ‚’“°¢76W'EöW†vVæW&FVEö¶W—2ÂW‡V7FVEö¶W—2“°¢Ð¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUö'V–ÆG5÷66ÆUö'Vvv–÷5ög&öÕööæ÷FU÷&öw&W76–öâ‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂc"Â“’À¢æ÷FU÷&V6÷&BƒÂÂ“bÂC‚Âs’À¢ÒÀ¢e³„dbÂÂ„uÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB7&VFVBÒFö7VÖVç@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆBG&ç6f÷&ÒF†R&öw&W76–öâ"“° ¢76W'EöW†7&VFVBÂ‚“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢6†ææVÅöæ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂ†æ÷FRç÷6—F–öâÂæ÷FRæ¶W’’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢°¢ƒÂc’À¢ƒ#BÂcB’À¢ƒC‚Âcr’À¢ƒs"Âc’À¢ƒ“bÂc"’À¢ƒ#ÂcR’À¢ƒCBÂc’’À¢ƒc‚Âc"’À¢Ð¢“°¢76W'EöW†æ÷FW5³Òæ6†ææVÅö–BÂ“°¢76W'EöW†æ÷FW5³Òæ¶W’ÂC‚“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂ„uÐ¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â’“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æU÷&W7V7G5÷6VÆV7F–öåöæE÷&öÆÇ5ö&6µö–çfÆ–E÷&ævW2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂcrÂ“’À¢æ÷FU÷&V6÷&BƒÂÂ“bÂC‚Âs’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WB6VÆV7FVBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB7&VFVBÒ6VÆV7FV@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÒÀ¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆBG&ç6f÷&ÒF†R6VÆV7F–öâ"“°¢76W'EöW†7&VFVBÂB“°¢ÆWBæ÷FW2Ò6VÆV7FVBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†6†ææVÅöæ÷FW2æÆVâ‚’ÂR“°¢76W'EöW‚†6†ææVÅöæ÷FW5³Òç÷6—F–öâÂ6†ææVÅöæ÷FW5³Òæ¶W’’ÂƒÂc’“°¢76W'EöW‚†6†ææVÅöæ÷FW5³Òç÷6—F–öâÂ6†ææVÅöæ÷FW5³Òæ¶W’’Âƒ“bÂcr’“°¢76W'EöW€¢æ÷FW2æ—FW"‚’æf–æB‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ’çVçw&‚’æ¶W’À¢C€¢“° ¢ÆWB×WB–çfÆ–BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÂÒ–çfÆ–BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢–çfÆ–@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢Ö–æ–×VÕö¶W“¢sÀ¢Ö†–×VÕö¶W“¢sÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢76W'EöW†–çfÆ–BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUö†÷&—¦öçFÅöÖ—'&÷%ö6å÷&W6W'fUööç6WG2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂc"Â“’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢Ö—'&÷%ö†÷&—¦öçFÃ¢G'VRÀ¢&W6W'fU÷7F'E÷F–ÖW3¢G'VRÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB&W6W'fRöç6WG2v†–ÆR&WfW'6–ær—F6†W2"“° ¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂ†æ÷FRç÷6—F–öâÂæ÷FRæ¶W’’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢6†ææVÅöæ÷FW2À¢°¢ƒÂc"’À¢ƒ#BÂc’’À¢ƒC‚ÂcR’À¢ƒs"Âc"’À¢ƒ“bÂc’À¢ƒ#Âcr’À¢ƒCBÂcB’À¢ƒc‚Âc’À¢Ð¢“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöÖ—'&÷%÷&WfW'6W5÷F–ÖUö÷%÷—F6…÷v—F†÷WE÷F÷V6†–æuö÷F†W%ö6†ææVÇ2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂc"Â“’À¢æ÷FU÷&V6÷&BƒÂÂ“bÂC‚Âs’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBF–ÖUöÖ—'&÷&VBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢F–ÖUöÖ—'&÷&V@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢Ö—'&÷%ö†÷&—¦öçFÃ¢G'VRÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB&WfW'6Ræ÷FRF–ÖW2"“°¢ÆWBæ÷FW2ÒF–ÖUöÖ—'&÷&VBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂ†æ÷FRç÷6—F–öâÂæ÷FRæ¶W’’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢6†ææVÅöæ÷FW2À¢°¢ƒc‚Âc’À¢ƒCBÂcB’À¢ƒ#Âcr’À¢ƒ“bÂc’À¢ƒs"Âc"’À¢ƒC‚ÂcR’À¢ƒ#BÂc’’À¢ƒÂc"’À¢Ð¢“°¢76W'EöW†æ÷FW5³Òæ6†ææVÅö–BÂ“°¢76W'EöW†æ÷FW5³Òæ¶W’ÂC‚“° ¢ÆWB×WB—F6…öÖ—'&÷&VBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢—F6…öÖ—'&÷&V@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢Ö—'&÷%÷fW'F–6Ã¢G'VRÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB–çfW'Bæ÷FR—F6†W2"“°¢ÆWBæ÷FW2Ò—F6…öÖ—'&÷&VBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂ†æ÷FRç÷6—F–öâÂæ÷FRæ¶W’’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢6†ææVÅöæ÷FW2À¢°¢ƒÂc’’À¢ƒ#BÂcR’À¢ƒC‚Âc"’À¢ƒs"Âc’’À¢ƒ“bÂcr’À¢ƒ#ÂcB’À¢ƒCBÂc’À¢ƒc‚Âcr’À¢Ð¢“°¢76W'EöW†æ÷FW5³Òæ6†ææVÅö–BÂ“°¢76W'EöW†æ÷FW5³Òæ¶W’ÂC‚“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöw&ö÷fUöÖ÷fW5÷7F'G5ö'•÷7G&VæwF…öæE÷6Vç6—F—f—G’‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂc"Â“’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WB6ögFVæVBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6ögFVæV@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢w&ö÷fU÷6æ÷F–6·3¢6öÖRƒ3b’À¢w&ö÷fU÷7F'E÷W&6VçC¢SÀ¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢À¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆBÖ—‚7F'G2F÷v&BF†R6æw&–B"“°¢ÆWBæ÷FW2Ò6ögFVæVBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†6†ææVÅöæ÷FW5³Òç÷6—F–öâÂ“°¢76W'EöW†6†ææVÅöæ÷FW5³Òç÷6—F–öâÂ3“°¢76W'EöW†6†ææVÅöæ÷FW5³%Òç÷6—F–öâÂC"“°¢76W'EöW†6†ææVÅöæ÷FW5³ÒæÆVæwF‚Â’“° ¢ÆWB×WB6Vç6—F—fRÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6Vç6—F—fP¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢w&ö÷fU÷6æ÷F–6·3¢6öÖRƒ3b’À¢w&ö÷fU÷7F'E÷W&6VçC¢À¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢SÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆBÆVfRF—7FçB7F'G2Væ6†ævVB"“°¢ÆWBæ÷FW2Ò6Vç6—F—fRçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢6†ææVÅöæ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³Â#BÂC‚Âs"Â“bÂ#ÂCBÂc…Ð¢“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöw&ö÷fU÷VçF—¦W5öGW&F–öåöæEö6åöÆVfUöæ÷FUöVæG5öf—†VB‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂc"Â“’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBVçF—¦VBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢VçF—¦V@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢w&ö÷fU÷6æ÷F–6·3¢6öÖRƒ#B’À¢w&ö÷fUöGW&F–öå÷W&6VçC¢À¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢À¢w&ö÷fU÷VçF—¦UöÖöFS¢&–fdÖ6†–æUVçF—¦TÖöFS£¥VçF—¦TGW&F–öâÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆBVçF—¦Ræ÷FRGW&F–öç2"“°¢ÆWBæ÷FW2ÒVçF—¦VBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'B€¢æ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÆÂ‡Ææ÷FWÂæ÷FRæÆVæwF‚ÓÒ#B¢“° ¢ÆWB×WBf—†VEöVæBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢f—†VEöVæ@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢w&ö÷fU÷6æ÷F–6·3¢6öÖRƒ3b’À¢w&ö÷fU÷7F'E÷W&6VçC¢À¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢À¢w&ö÷fU÷VçF—¦UöÖöFS¢&–fdÖ6†–æUVçF—¦TÖöFS£¤ÆVfTVæBÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB&W6W'fRæ÷FRVæG2"“°¢ÆWBæ÷FW2Òf—†VEöVæBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWB6†ææVÅöæ÷FW2Òæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†6†ææVÅöæ÷FW5³Òç÷6—F–öâ²6†ææVÅöæ÷FW5³ÒæÆVæwF‚ÂC2“°¢76W'EöW†6†ææVÅöæ÷FW5³Òç÷6—F–öâÂ3b“°¢76W'EöW†6†ææVÅöæ÷FW5³ÒæÆVæwF‚Âr“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöw&ö÷fUöF—7F–æwV—6†W5÷VçF—¦VEöGW&F–öåög&öÕ÷VçF—¦VEöVæB‚’°¢ÆWBæ÷FRÒGFW&äæ÷FR°¢÷6—F–öã¢3À¢ÆVæwFƒ¢#BÀ¢âåGFW&äæ÷FS£¦FVfVÇB‚¢Ó°¢ÆWBGW&F–öâÒ&–feöÖ6†–æUöw&ö÷fUöæ÷FU÷F–Ö–ær€¢fæ÷FRÀ¢#BÀ¢&–fdÖ6†–æT÷F–öç2°¢w&ö÷fU÷7F'E÷W&6VçC¢#RÀ¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢À¢w&ö÷fUöGW&F–öå÷W&6VçC¢À¢w&ö÷fU÷VçF—¦UöÖöFS¢&–fdÖ6†–æUVçF—¦TÖöFS£¥VçF—¦TGW&F–öâÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&GW&F–öâVçF—¦F–öâ6†÷VÆB7V66VVB"“°¢ÆWBVæBÒ&–feöÖ6†–æUöw&ö÷fUöæ÷FU÷F–Ö–ær€¢fæ÷FRÀ¢#BÀ¢&–fdÖ6†–æT÷F–öç2°¢w&ö÷fU÷7F'E÷W&6VçC¢#RÀ¢w&ö÷fU÷6Vç6—F—f—G•÷W&6VçC¢À¢w&ö÷fUöGW&F–öå÷W&6VçC¢À¢w&ö÷fU÷VçF—¦UöÖöFS¢&–fdÖ6†–æUVçF—¦TÖöFS£¥VçF—¦TVæBÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&VæB×F–ÖRVçF—¦F–öâ6†÷VÆB7V66VVB"“°¢76W'EöW†GW&F–öâÂƒ#’Â#B’“°¢76W'EöW†VæBÂƒ#’Â’’“°¢Ð ¢5·FW7EÐ¢fâ&æFöÖ—¦W%ö¶VW5övVæW&FVE÷—F6†W5ö–å÷F†U÷&WVW7FVE÷66ÆR‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂƒ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ“bÂcRÂƒ’À¢æ÷FU÷&V6÷&Bƒ“"ÂÂ“bÂc‚Âƒ’À¢æ÷FU÷&V6÷&Bƒ#ƒ‚ÂÂ“bÂs"Âƒ’À¢æ÷FU÷&V6÷&BƒÂÂ“bÂcÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB÷F–öç2Ò&æFöÖ—¦W$÷F–öç2°¢6VVC¢CÀ¢fVÆö6—G•öÖ÷VçE÷W&6VçC¢À¢åöÖ÷VçE÷W&6VçC¢À¢&VÆV6UöÖ÷VçE÷W&6VçC¢À¢ÖöE÷…öÖ÷VçE÷W&6VçC¢À¢ÖöE÷•öÖ÷VçE÷W&6VçC¢À¢—F6…÷&ævU÷6VÖ—FöæW3¢RÀ¢&—öÆ#¢G'VRÀ¢&W6WEöÆWfVÇ3¢fÇ6RÀ¢Ó°¢ÆWB×WBf—'7BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB6V6öæBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWBf—'7Eö6†ævVBÒf—'7@¢ç&æFöÖ—¦U÷GFW&åöæ÷FW5÷v—F…÷66ÆRƒrÂÂ÷F–öç2Â"Âe³Â"ÂBÂRÂrÂ’ÂÒ¢æW‡V7B‚'66ÆRÖv&R&æFöÖ—¦W"6†÷VÆB7V66VVB"“°¢ÆWB6V6öæEö6†ævVBÒ6V6öæ@¢ç&æFöÖ—¦U÷GFW&åöæ÷FW5÷v—F…÷66ÆRƒrÂÂ÷F–öç2Â"Âe³Â"ÂBÂRÂrÂ’ÂÒ¢æW‡V7B‚'F†R6ÖR6VVB6†÷VÆB&öGV6RF†R6ÖR&W7VÇB"“° ¢76W'B†f—'7Eö6†ævVBâ“°¢76W'EöW†f—'7Eö6†ævVBÂ6V6öæEö6†ævVB“°¢76W'EöW€¢f—'7BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢6V6öæBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚¢“°¢ÆWBæ÷FW2Òf—'7BçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢f÷"†æ÷FRÂ÷&–v–æÅö¶W’’–âæ÷FW2æ—FW"‚’çF¶RƒB’ç¦—…³cö“3"ÂcRÂc‚Âs%Ò’°¢76W'EöW†æ÷FRæ6†ææVÅö–BÂ“°¢76W'B†æ÷FU÷—F6…ö–å÷66ÆR€¢“3#£¦g&öÒ†æ÷FRæ¶W’’À¢"À¢e³Â"ÂBÂRÂrÂ’ÂÐ¢’“°¢76W'B‚†“3#£¦g&öÒ†æ÷FRæ¶W’’Ò÷&–v–æÅö¶W’’æ'2‚’ÃÒR“°¢Ð¢76W'EöW†æ÷FW5³EÒæ¶W’Âc“°¢76W'EöW†æ÷FW5³EÒæ6†ææVÅö–BÂ“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöÆWfVÇ5÷&æFöÖ—¦UöÆÅ÷7W÷'FVEöæ÷FU÷&÷W'F–W5öFWFW&Ö–æ—7F–6ÆÇ’‚’°¢ÆWB×WBf—'7E÷&ö÷BÒæ÷FU÷&V6÷&BƒÂÂ“bÂcÂƒ“°¢f—'7E÷&ö÷E³…ÒÒc°¢f—'7E÷&ö÷E³#ÒÒ3°¢f—'7E÷&ö÷E³#%ÒÒ#°¢f—'7E÷&ö÷E³#5ÒÒ#C°¢ÆWB×WB6V6öæE÷&ö÷BÒæ÷FU÷&V6÷&Bƒ“bÂÂ“bÂcBÂc“°¢6V6öæE÷&ö÷E³…ÒÒ3°¢6V6öæE÷&ö÷E³#ÒÒƒ°¢6V6öæE÷&ö÷E³#%ÒÒ#°¢6V6öæE÷&ö÷E³#5ÒÒC°¢ÆWB×WB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&BƒÂÂ“bÂC‚Âs“°¢÷F†W%ö6†ææVÅ³…ÒÒCS°¢÷F†W%ö6†ææVÅ³#ÒÒsS°¢÷F†W%ö6†ææVÅ³#%ÒÒ#S°¢÷F†W%ö6†ææVÅ³#5ÒÒsS°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶f—'7E÷&ö÷BÂ6V6öæE÷&ö÷BÂ÷F†W%ö6†ææVÅÒÂe³„dbÂÒ“°¢ÆWB÷F–öç2Ò&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢å÷f&–F–öå÷W&6VçC¢À¢&VÆV6U÷f&–F–öå÷W&6VçC¢À¢ÖöE÷…÷f&–F–öå÷W&6VçC¢À¢ÖöE÷•÷f&–F–öå÷W&6VçC¢À¢—F6…÷f&–F–öå÷6VÖ—FöæW3¢"À¢&—öÆ%öÆWfVÇ3¢G'VRÀ¢6VVC¢#2À¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢Ó°¢ÆWB×WBf—'7BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB6V6öæBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBv—F†÷WE÷—F6…÷f&–F–öâÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢f—'7@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2ƒrÂÂ÷F–öç2¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB&æFöÖ—¦RÆWfVÂ&÷W'F–W2"“°¢6V6öæ@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2ƒrÂÂ÷F–öç2¢æW‡V7B‚'F†R6ÖR6VVB6†÷VÆB&öGV6RF†R6ÖRÆWfVÇ2"“°¢v—F†÷WE÷—F6…÷f&–F–öà¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢—F6…÷f&–F–öå÷6VÖ—FöæW3¢À¢âæ÷F–öç0¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB7W÷'BF—6&Æ–ær—F6‚f&–F–öâ"“°¢76W'EöW€¢f—'7BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢6V6öæBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚¢“° ¢ÆWB÷&–v–æÅö÷F†W"ÒfÇFö7VÖVçC£§'6R‚f–çWB¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"¢çGFW&ç2‚¢çVçw&‚¢ç&VÖ÷fRƒ¢ææ÷FW0¢ç&VÖ÷fRƒ"“°¢ÆWBæ÷FW2Òf—'7BçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWBvVæW&FVBÒæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWBVçf&–VEö¶W—2Òv—F†÷WE÷—F6…÷f&–F–öà¢çGFW&ç2‚¢çVçw&‚¢ç&VÖ÷fRƒ¢ææ÷FW0¢æ–çFõö—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂæ÷FRæ¶W’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†vVæW&FVBæÆVâ‚’Â‚“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢ç¦—‡Vçf&–VEö¶W—2¢æç’‡Â‡f&–VBÂVçf&–VB—Âf&–VBæ¶W’ÒVçf&–VB¢“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢æç’‡Ææ÷FWÂæ÷FRçfVÆö6—G’Òƒbbæ÷FRçfVÆö6—G’Òc¢“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢æç’‡Ææ÷FWÂæ÷FRçâÒ3bbæ÷FRçâÒƒ¢“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢æç’‡Ææ÷FWÂæ÷FRç&VÆV6RÒcbbæ÷FRç&VÆV6RÒ3¢“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢æç’‡Ææ÷FWÂæ÷FRæÖöE÷‚Ò#bbæ÷FRæÖöE÷‚Ò#¢“°¢76W'B€¢vVæW&FV@¢æ—FW"‚¢æç’‡Ææ÷FWÂæ÷FRæÖöE÷’Ò#Cbbæ÷FRæÖöE÷’ÒC¢“°¢76W'EöW€¢æ÷FW2æ—FW"‚’æf–æB‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ’À¢6öÖR‚f÷&–v–æÅö÷F†W"¢“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUö6å÷&W6WEöÆWfVÇ5ö&Vf÷&U÷&æFöÖ—¦–ær‚’°¢ÆWB×WB&ö÷BÒæ÷FU÷&V6÷&BƒÂÂ“bÂcÂƒ“°¢&ö÷E³…ÒÒCS°¢&ö÷E³#ÒÒ#S°¢&ö÷E³#%ÒÒ“°¢&ö÷E³#5ÒÒƒ°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e·&ö÷EÒÂe³„dbÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢&W6WEöÆWfVÇ3¢G'VRÀ¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚%&–fbÖ6†–æR6†÷VÆB&W6WBvVæW&FVBæ÷FRÆWfVÇ2"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢ÆWBvVæW&FVBÒæ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'B‚vVæW&FVBæ—5öV×G’‚’“°¢76W'B†vVæW&FVBæ—FW"‚’æÆÂ‡Ææ÷FWÂ°¢æ÷FRçfVÆö6—G’ÓÒ ¢bbæ÷FRçâÓÒc@¢bbæ÷FRç&VÆV6RÓÒ ¢bbæ÷FRæÖöE÷‚ÓÒ ¢bbæ÷FRæÖöE÷’ÓÒ ¢Ò’“°¢Ð ¢5·FW7EÐ¢fâ&–feöÖ6†–æUöw&ö÷fU÷&WV—&W5÷6æ÷v†Vå÷F–Ö–æuö—5öVæ&ÆVB‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶æ÷FU÷&V6÷&BƒÂÂ“bÂcÂ•ÒÂe³„dbÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÂÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B€¢Fö7VÖVç@¢ç&–feöÖ6†–æU÷GFW&åöæ÷FW2€¢rÀ¢À¢&–fdÖ6†–æT÷F–öç2°¢fVÆö6—G•÷f&–F–öå÷W&6VçC¢À¢w&ö÷fU÷6æ÷F–6·3¢æöæRÀ¢w&ö÷fU÷7F'E÷W&6VçC¢À¢âå&–fdÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâ6ÆuöÖ6†–æU÷G&6†W5÷W&–öF–5÷6Æ–6W5öæE÷&W6W'fW5ö÷F†W%ö6†ææVÇ2‚’°¢ÆWBæ÷FU÷&V6÷&G2Ò°¢æ÷FU÷&V6÷&BƒÂÂ#BÂcÂ’À¢æ÷FU÷&V6÷&Bƒ#BÂÂ#BÂc"Â“R’À¢æ÷FU÷&V6÷&BƒC‚ÂÂ#BÂcBÂ“’À¢æ÷FU÷&V6÷&Bƒs"ÂÂ#BÂcRÂƒR’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ#BÂcrÂƒ’À¢æ÷FU÷&V6÷&Bƒ#ÂÂ#BÂc’ÂsR’À¢æ÷FU÷&V6÷&BƒCBÂÂ#BÂsÂs’À¢æ÷FU÷&V6÷&Bƒc‚ÂÂ#BÂs"ÂcR’À¢æ÷FU÷&V6÷&Bƒs"ÂÂ#BÂC‚Âc’À¢Ó°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dbÂÂ„uÓ°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚fæ÷FU÷&V6÷&G2ÂgVæ¶æ÷våöWfVçB’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6†ævVBÒFö7VÖVç@¢æ6Æu÷GFW&åöæ÷FW2€¢rÀ¢À¢6ÆtÖ6†–æT÷F–öç2°¢W&–öE÷F–6·3¢3ƒBÀ¢G&6…öWfW'“¢BÀ¢âä6ÆtÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚$6ÆrÖ6†–æR6†÷VÆB&ö6W72F†R6†ææVÂ"“° ¢76W'EöW†6†ævVBÂ"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW€¢æ÷FW0¢æ—FW"‚¢æf–ÇFW"‡Ææ÷FWÂæ÷FRæ6†ææVÅö–BÓÒ¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³Â#BÂC‚Â“bÂ#ÂCEÐ¢“°¢76W'EöW†æ÷FW2æÆ7B‚’çVçw&‚’ç÷6—F–öâÂs"“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBf–ÆR6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Âr“°¢Ð ¢5·FW7EÐ¢fâ6ÆuöÖ6†–æU÷7G&WF6†W5÷&VÖ÷fVEöv5öæEöÆ–Ö—G5÷6VÆV7F–öå÷66÷R‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ"ÂcÂ’À¢æ÷FU÷&V6÷&Bƒ#BÂÂ"Âc"Â“’À¢æ÷FU÷&V6÷&BƒC‚ÂÂ"ÂcBÂƒ’À¢æ÷FU÷&V6÷&Bƒs"ÂÂ"ÂC‚Âs’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB6†ævVBÒFö7VÖVç@¢æ6Æu÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÂÒÀ¢6ÆtÖ6†–æT÷F–öç2°¢W&–öE÷F–6·3¢3ƒBÀ¢G&6…öWfW'“¢"À¢7G&WF6…÷Fõö6ö×Vç6FS¢G'VRÀ¢âä6ÆtÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚$6ÆrÖ6†–æR6†÷VÆB&ö6W72F†R6VÆV7F–öâ"“° ¢76W'EöW†6†ævVBÂ"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW†æ÷FW5³Òç÷6—F–öâÂ“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â3b“°¢76W'EöW†æ÷FW5³Òç÷6—F–öâÂC‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â"“°¢76W'EöW†æ÷FW5³%Òæ6†ææVÅö–BÂ“°¢76W'EöW†æ÷FW5³%Òç÷6—F–öâÂs"“°¢Ð ¢5·FW7EÐ¢fâ6ÆuöÖ6†–æU÷6ÆWw5öæ÷FU÷F–Ö–æuöæE÷fÆ–FFW5ö÷F–öç2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ#BÂcÂ’À¢æ÷FU÷&V6÷&Bƒ“bÂÂ#BÂc"Â“’À¢æ÷FU÷&V6÷&Bƒ“"ÂÂ#BÂcBÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢æ6Æu÷GFW&åöæ÷FW2€¢rÀ¢À¢6ÆtÖ6†–æT÷F–öç2°¢W&–öE÷F–6·3¢3ƒBÀ¢G&6…öWfW'“¢bÀ¢F–ÖUöF—7F÷'F–öå÷W&6VçC¢À¢âä6ÆtÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚$6ÆrÖ6†–æR6†÷VÆB6ÆWræ÷FRF–Ö–ær"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW†æ÷FW5³Òç÷6—F–öâÂ“°¢76W'EöW†æ÷FW5³Òç÷6—F–öâÂb“°¢76W'EöW†æ÷FW5³%Òç÷6—F–öâÂC‚“° ¢ÆWB×WB–çfÆ–BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢–çfÆ–@¢æ6Æu÷GFW&åöæ÷FW2€¢rÀ¢À¢6ÆtÖ6†–æT÷F–öç2°¢W&–öE÷F–6·3¢RÀ¢âä6ÆtÖ6†–æT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æ—5öW'"‚¢“°¢Ð ¢5·FW7EÐ¢fâÆ–Ö—Eöæ÷FW5÷6æ5÷Fõ÷66ÆUö–å÷&WVW7FVEöF—&V7F–öåöæE÷66÷R‚’°¢6öç7BÔ¤õ%ô”åDU%dÅ3¢e·S…ÒÒe³Â"ÂBÂRÂrÂ’ÂÓ°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂcÂcÂ’À¢æ÷FU÷&V6÷&Bƒ#BÂÂcÂc2Â“’À¢æ÷FU÷&V6÷&BƒC‚ÂÂcÂcRÂƒ’À¢æ÷FU÷&V6÷&Bƒs"ÂÂcÂcÂs’À¢ÒÀ¢e³„dbÂÂ„uÒÀ¢“° ¢ÆWB×WB6æ÷WÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6æ÷W ¢æÆ–Ö—E÷GFW&åöæ÷FU÷&ævU÷v—F…ö÷F–öç2€¢rÀ¢À¢Æ–Ö—Dæ÷FT÷F–öç2°¢Ö–æ–×VÕö¶W“¢cÀ¢Ö†–×VÕö¶W“¢cbÀ¢w&÷Fõö&÷GFöÓ¢fÇ6RÀ¢66ÆU÷&ö÷C¢6öÖRƒ’À¢66ÆUö–çFW'fÇ3¢6öÖR„Ô¤õ%ô”åDU%dÅ2’À¢6æöF—&V7F–öã¢Æ–Ö—E6æF—&V7F–öã£¥WÀ¢ÒÀ¢¢æW‡V7B‚&æ÷FW26†÷VÆB6æWv&B–çFò2Ö¦÷""“°¢ÆWBæ÷FW2Ò6æ÷WçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW†æ÷FW5³Òæ¶W’Âc"“°¢76W'EöW†æ÷FW5³Òæ¶W’ÂcB“°¢76W'EöW†æ÷FW5³%Òæ¶W’ÂcR“°¢76W'EöW†æ÷FW5³5Òæ¶W’Âc“° ¢ÆWB×WB6æöF÷vâÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6æöF÷và¢æÆ–Ö—E÷GFW&åöæ÷FU÷&ævU÷v—F…ö÷F–öç2€¢rÀ¢À¢Æ–Ö—Dæ÷FT÷F–öç2°¢Ö–æ–×VÕö¶W“¢cÀ¢Ö†–×VÕö¶W“¢cbÀ¢w&÷Fõö&÷GFöÓ¢fÇ6RÀ¢66ÆU÷&ö÷C¢6öÖRƒ’À¢66ÆUö–çFW'fÇ3¢6öÖR„Ô¤õ%ô”åDU%dÅ2’À¢6æöF—&V7F–öã¢Æ–Ö—E6æF—&V7F–öã£¤F÷vâÀ¢ÒÀ¢¢æW‡V7B‚&æ÷FW26†÷VÆB6æF÷vçv&B–çFò2Ö¦÷""“°¢ÆWBæ÷FW2Ò6æöF÷vâçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW†æ÷FW5³Òæ¶W’Âc“°¢76W'EöW†æ÷FW5³Òæ¶W’Âc"“° ¢ÆWB×WBÇFW&æF–ærÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÇFW&æF–æp¢æÆ–Ö—E÷GFW&åöæ÷FU÷6VÆV7F–öå÷&ævU÷v—F…ö÷F–öç2€¢rÀ¢À¢e³ÂÒÀ¢Æ–Ö—Dæ÷FT÷F–öç2°¢Ö–æ–×VÕö¶W“¢cÀ¢Ö†–×VÕö¶W“¢cbÀ¢w&÷Fõö&÷GFöÓ¢fÇ6RÀ¢66ÆU÷&ö÷C¢6öÖRƒ’À¢66ÆUö–çFW'fÇ3¢6öÖR„Ô¤õ%ô”åDU%dÅ2’À¢6æöF—&V7F–öã¢Æ–Ö—E6æF—&V7F–öã£¤ÇFW&æFRÀ¢ÒÀ¢¢æW‡V7B‚'6VÆV7FVBæ÷FW26†÷VÆBÇFW&æFRF†V—"6æF—&V7F–öâ"“°¢ÆWBæ÷FW2ÒÇFW&æF–ærçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW†æ÷FW5³Òæ¶W’Âc"“°¢76W'EöW†æ÷FW5³Òæ¶W’Âc"“°¢76W'EöW†æ÷FW5³%Òæ¶W’ÂcR“°¢76W'EöW†æ÷FW5³5Òæ¶W’Âc“° ¢ÆWB×WBæõö–å÷66ÆUö¶W’ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢æõö–å÷66ÆUö¶W¢æÆ–Ö—E÷GFW&åöæ÷FU÷&ævU÷v—F…ö÷F–öç2€¢rÀ¢À¢Æ–Ö—Dæ÷FT÷F–öç2°¢Ö–æ–×VÕö¶W“¢cÀ¢Ö†–×VÕö¶W“¢cÀ¢w&÷Fõö&÷GFöÓ¢fÇ6RÀ¢66ÆU÷&ö÷C¢6öÖRƒ’À¢66ÆUö–çFW'fÇ3¢6öÖR„Ô¤õ%ô”åDU%dÅ2’À¢6æöF—&V7F–öã¢Æ–Ö—E6æF—&V7F–öã£¥WÀ¢ÒÀ¢¢æ—5öW'"‚¢“° ¢ÆWB×WBw&VBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂcÂs"Â’À¢æ÷FU÷&V6÷&Bƒ#BÂÂcÂƒBÂƒ’À¢ÒÀ¢e³„dbÂÒÀ¢’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢w&V@¢æÆ–Ö—E÷GFW&åöæ÷FU÷&ævU÷v—F…ö÷F–öç2€¢rÀ¢À¢Æ–Ö—Dæ÷FT÷F–öç2°¢Ö–æ–×VÕö¶W“¢C‚À¢Ö†–×VÕö¶W“¢ƒ2À¢w&÷Fõö&÷GFöÓ¢G'VRÀ¢66ÆU÷&ö÷C¢æöæRÀ¢66ÆUö–çFW'fÇ3¢æöæRÀ¢6æöF—&V7F–öã¢Æ–Ö—E6æF—&V7F–öã£¥WÀ¢ÒÀ¢¢æW‡V7B‚&æ÷FW26†÷VÆBw&FòF†RÆ÷vW7Bö7FfR–âF†R&ævR"“°¢ÆWBw&VEöæ÷FW2Òw&VBçGFW&ç2‚’çVçw&‚’ç&VÖ÷fRƒ’ææ÷FW3°¢76W'EöW‡w&VEöæ÷FW5³Òæ¶W’ÂC‚“°¢76W'EöW‡w&VEöæ÷FW5³Òæ¶W’ÂC‚“°¢Ð ¢5·FW7EÐ¢fâ'F–7VÆFU÷W6W5ö÷&–v–æÅö÷%öÆVvFõöÆVæwF‡5öæE÷6VÆV7FVEö6öçFW‡B‚’°¢ÆWB×WBf—'7BÒæ÷FU÷&V6÷&BƒÂÂ“bÂC‚Â“°¢f—'7E³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cƒ“C“E÷SbçFõöÆUö'—FW2‚’“°¢ÆWBæW‡BÒæ÷FU÷&V6÷&BƒC‚ÂÂ#BÂSRÂ““°¢ÆWB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&Bƒ#BÂÂ3ÂcÂƒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶f—'7BÂæW‡BÂ÷F†W%ö6†ææVÅÒÂe³„dbÂÂ„uÒ“° ¢ÆWB×WBÆVvFòÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ævVBÒÆVvFð¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒSÂÂÂfÇ6R’¢æW‡V7B‚&6†ææVÂ'F–7VÆF–öâ6†÷VÆB7V66VVB"“°¢76W'EöW†6†ævVBÂ"“°¢ÆWBæ÷FW2ÒÆVvFòçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â#B“°¢76W'EöW†æ÷FW5³ÒæfÆw2Âƒ“C“B“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â"“°¢76W'EöW†æ÷FW5³%ÒæÆVæwF‚Â3“° ¢ÆWB×WB6VÆV7FVEö6öçFW‡BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢6VÆV7FVEö6öçFW‡@¢æ'F–7VÆFU÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÒÀ¢'F–7VÆFUö÷F–öç2ƒSÂÂÂfÇ6R’À¢G'VRÀ¢¢æW‡V7B‚'6VÆV7FVBÖöæÇ’'F–7VÆF–öâ6öçFW‡B6†÷VÆB7V66VVB"“°¢ÆWBæ÷FW2Ò6VÆV7FVEö6öçFW‡BçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚ÂC‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â#B“° ¢ÆWB×WBW6UöÆVæwF‡2ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢W6UöÆVæwF‡0¢æ'F–7VÆFU÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÒÀ¢'F–7VÆFUö÷F–öç2ƒSÂÂÂG'VR’À¢fÇ6RÀ¢¢æW‡V7B‚&÷&–v–æÂÖÆVæwF‚'F–7VÆF–öâ6†÷VÆB7V66VVB"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚gW6UöÆVæwF‡2æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢æW‡V7B‚&VF—FVBdÅ6†÷VÆB'6R"“°¢ÆWBæ÷FW2Ò&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚ÂC‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â#B“°¢76W'EöW€¢&W'6VBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂ„uÐ¢“°¢Ð ¢5·FW7EÐ¢fâ'F–7VÆFU÷f&–F–öåö—5÷6VVFVEöæEö&÷VæFVEö'•÷F†U÷66ÆVEöÆVæwF‚‚’°¢ÆWBf—'7BÒæ÷FU÷&V6÷&BƒÂÂ“bÂcÂ“°¢ÆWB6V6öæBÒæ÷FU÷&V6÷&Bƒ#ÂÂC‚ÂcBÂ““°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶f—'7BÂ6V6öæEÒÂe³„dbÂÂƒTÒ“° ¢ÆWB×WBf—†VBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢f—†V@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒÂÂÂG'VR’¢çVçw&‚’À¢ ¢“° ¢ÆWB×WBf&–VEöf—'7BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢f&–VEöf—'7@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒÂSÂÂG'VR’¢æW‡V7B‚'6VVFVBf&–F–öâ6†÷VÆB7V66VVB"“°¢ÆWBf—'7Eöæ÷FW2Òf&–VEöf—'7BçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†f—'7Eöæ÷FW5³ÒæÆVæwF‚Âs‚“°¢76W'B‚ƒC‚âãÓCB’æ6öçF–ç2‚ff—'7Eöæ÷FW5³ÒæÆVæwF‚’“°¢76W'B‚ƒ#BâãÓs"’æ6öçF–ç2‚ff—'7Eöæ÷FW5³ÒæÆVæwF‚’“°¢76W'EöW†f—'7Eöæ÷FW5³Òæ¶W’Âc“°¢76W'EöW†f—'7Eöæ÷FW5³ÒçfVÆö6—G’Â“° ¢ÆWB×WBf&–VEöv–âÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢f&–VEöv–à¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒÂSÂÂG'VR’¢æW‡V7B‚'F†R6ÖR6VVB6†÷VÆB&R&WW6&ÆR"“°¢ÆWB&WVFVEöæ÷FW2Òf&–VEöv–âçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW‡&WVFVEöæ÷FW2Âf—'7Eöæ÷FW2“°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚gf&–VEöf—'7BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢çVçw&‚¢æWfVçG2‚¢æÆ7B‚¢çVçw&‚¢çv—&Uö'—FW2‚’À¢e³„dbÂÂƒTÐ¢“°¢Ð ¢5·FW7EÐ¢fâ'F–7VÆFUö6†÷ö6†÷&G5÷G&–×5ööæÇ•öæ÷FW5ö÷fW&Æ–æu÷F†UöæW‡Eööç6WB‚’°¢ÆWB6†÷'Eöf—'7BÒæ÷FU÷&V6÷&BƒÂÂ#BÂcÂ“°¢ÆWBÆöæuöf—'7BÒæ÷FU÷&V6÷&BƒÂÂ“bÂcBÂ““°¢ÆWBÆöæu÷6V6öæBÒæ÷FU÷&V6÷&BƒC‚ÂÂ“bÂcrÂƒR“°¢ÆWB6†÷'E÷6V6öæBÒæ÷FU÷&V6÷&BƒC‚ÂÂ#BÂs"Âƒ“°¢ÆWBf–æÅöæ÷FRÒæ÷FU÷&V6÷&Bƒ“bÂÂ#BÂsÂsR“°¢ÆWB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&Bƒ#BÂÂC‚ÂC‚Âs“°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢6†÷'Eöf—'7BÀ¢Æöæuöf—'7BÀ¢Æöæu÷6V6öæBÀ¢6†÷'E÷6V6öæBÀ¢f–æÅöæ÷FRÀ¢÷F†W%ö6†ææVÂÀ¢ÒÀ¢e³„dbÂÂƒTÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ævVBÒFö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FW2€¢rÀ¢À¢'F–7VÆFT÷F–öç2°¢6†÷ö6†÷&G3¢G'VRÀ¢âä'F–7VÆFT÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&6†÷&B6†÷–ær6†÷VÆB7V66VVB"“° ¢76W'EöW†6†ævVBÂ"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚Â#B“°¢76W'EöW†æ÷FW5³ÒæÆVæwF‚ÂC‚“°¢76W'EöW†æ÷FW5³%ÒæÆVæwF‚ÂC‚“°¢76W'EöW†æ÷FW5³5ÒæÆVæwF‚Â#B“°¢76W'EöW†æ÷FW5³EÒæÆVæwF‚Â#B“°¢76W'EöW†æ÷FW5³UÒæÆVæwF‚ÂC‚“°¢76W'EöW†æ÷FW5³Òæ¶W’ÂcB“°¢76W'EöW†æ÷FW5³ÒçfVÆö6—G’Â““°¢76W'EöW€¢fÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢çVçw&‚¢æWfVçG2‚¢æÆ7B‚¢çVçw&‚¢çv—&Uö'—FW2‚’À¢e³„dbÂÂƒTÐ¢“°¢Ð ¢5·FW7EÐ¢fâV–6µö6†÷÷7Æ—G5ö6†ææVÅöæ÷FW5öE÷6æ÷F–6·5öæEö¶VW5÷F†U÷&VÖ–æFW"‚’°¢ÆWBÆöæuöæ÷FRÒæ÷FU÷&V6÷&Bƒ2ÂÂÂcÂ“°¢ÆWB6æöÆVæwF…öæ÷FRÒæ÷FU÷&V6÷&Bƒ#BÂÂBÂcBÂ““°¢ÆWB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&BƒC‚ÂÂ’ÂC‚Âƒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e¶Æöæuöæ÷FRÂ6æöÆVæwF…öæ÷FRÂ÷F†W%ö6†ææVÅÒÀ¢e³„dbÂÂƒTÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB7&VFVBÒFö7VÖVç@¢çV–6µö6†÷÷GFW&åöæ÷FW2ƒrÂÂB¢æW‡V7B‚'V–6²6†÷6†÷VÆB7V66VVB"“° ¢76W'EöW†7&VFVBÂ"“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW†æ÷FW2æÆVâ‚’ÂR“°¢76W'EöW‚†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚’Âƒ2ÂB’“°¢76W'EöW‚†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚’Âƒ#BÂB’“°¢76W'EöW‚†æ÷FW5³%Òç÷6—F–öâÂæ÷FW5³%ÒæÆVæwF‚’ÂƒC‚Â’’“°¢76W'EöW‚†æ÷FW5³5Òç÷6—F–öâÂæ÷FW5³5ÒæÆVæwF‚’ÂƒrÂB’“°¢76W'EöW‚†æ÷FW5³EÒç÷6—F–öâÂæ÷FW5³EÒæÆVæwF‚’ÂƒÂ"’“°¢76W'EöW†æ÷FW5³5Òæ¶W’Âæ÷FW5³Òæ¶W’“°¢76W'EöW†æ÷FW5³5ÒçfVÆö6—G’Âæ÷FW5³ÒçfVÆö6—G’“°¢76W'EöW†æ÷FW5³EÒæ¶W’Âæ÷FW5³Òæ¶W’“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2Âæ÷FW2“°¢76W'EöW€¢&W'6VBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂƒTÐ¢“°¢Ð ¢5·FW7EÐ¢fâV–6µö6†÷÷6VÆV7F–öåöæE÷fÆ–FF–öåö&U÷66÷VEöæEöFöÖ–2‚’°¢ÆWBf—'7Eöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂ‚ÂcÂ“°¢ÆWB6VÆV7FVEöæ÷FRÒæ÷FU÷&V6÷&Bƒ#BÂÂÂcBÂ““°¢ÆWB÷F†W%ö6†ææVÂÒæ÷FU÷&V6÷&BƒC‚ÂÂ"ÂC‚Âƒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶f—'7Eöæ÷FRÂ6VÆV7FVEöæ÷FRÂ÷F†W%ö6†ææVÅÒÂe³„dbÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢çV–6µö6†÷÷GFW&åöæ÷FU÷6VÆV7F–öâƒrÂÂe³ÒÂB¢æW‡V7B‚'6VÆV7FVBæ÷FR6†÷VÆB6†÷"’À¢ ¢“°¢ÆWBæ÷FW2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW‚†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚’ÂƒÂ‚’“°¢76W'EöW‚†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚’Âƒ#BÂB’“°¢76W'EöW‚†æ÷FW5³%Òç÷6—F–öâÂæ÷FW5³%ÒæÆVæwF‚’ÂƒC‚Â"’“°¢76W'EöW‚†æ÷FW5³5Òç÷6—F–öâÂæ÷FW5³5ÒæÆVæwF‚’Âƒ#‚ÂB’“°¢76W'EöW‚†æ÷FW5³EÒç÷6—F–öâÂæ÷FW5³EÒæÆVæwF‚’Âƒ3"Â"’“° ¢ÆWB÷fW'6—¦VBÒæ÷FU÷&V6÷&BƒÂÂS3#£¤Ô‚ÂcÂ“°¢ÆWB×WBwV&FVBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e¶÷fW'6—¦VEÒÂe³„dbÂÒ’¢æW‡V7B‚&Æ&vRÖæ÷FRf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒwV&FV@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&÷&–v–æÂ&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'B†wV&FVBçV–6µö6†÷÷GFW&åöæ÷FW2ƒrÂÂ’æ—5öW'"‚’“°¢76W'B†wV&FVBçV–6µö6†÷÷GFW&åöæ÷FW2ƒrÂÂ’æ—5öW'"‚’“°¢76W'EöW€¢wV&FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢&Vf÷&RÀ¢&–çfÆ–B÷"÷fW'6—¦VB6†÷26†÷VÆBÆVfRF†R&ö¦V7BVæ6†ævVB ¢“°¢Ð ¢5·FW7EÐ¢fâæ÷FU÷&÷FF–öå÷w&5ö–ç6–FU÷GFW&åöÖV7W&W5öæEöföÆÆ÷w5öÖWFW%ö6†ævW2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂ#BÂcÂ’À¢æ÷FU÷&V6÷&Bƒ#3RÂÂ#BÂcBÂ“’À¢æ÷FU÷&V6÷&BƒCsRÂÂ#BÂcrÂƒR’À¢æ÷FU÷&V6÷&BƒSÂÂ#BÂs"Âƒ’À¢æ÷FU÷&V6÷&BƒÂÂC‚ÂC‚Âs’À¢ÒÀ¢e³„dbÂÂƒTÒÀ¢“°¢ÆWB×WB&–v‡BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢&–v‡@¢ç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂÂRÂ‚¢æW‡V7B‚&–æ—F–ÂGFW&âÖWFW"6†÷VÆB&R6WB"“°¢&–v‡@¢ç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂCƒÂ2Â‚¢æW‡V7B‚&ÆFW"GFW&âÖWFW"6†÷VÆB&R6WB"“° ¢76W'EöW€¢&–v‡@¢ç&÷FFU÷GFW&åöæ÷FW2ƒrÂÂ#BÂGFW&äæ÷FU&÷FF–öã£¥&–v‡B¢æW‡V7B‚&6†ææVÂ&÷FF–öâ6†÷VÆB7V66VVB"’À¢@¢“°¢ÆWB&–v‡Eöæ÷FW2Ò&–v‡BçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW€¢&–v‡Eöæ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³#BÂ’Â#S’ÂS#BÂÐ¢“°¢76W'EöW€¢&–v‡Eöæ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRæÆVæwF‚¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³#BÂ#BÂ#BÂ#BÂC…Ð¢“° ¢76W'EöW€¢&–v‡@¢ç&÷FFU÷GFW&åöæ÷FU÷6VÆV7F–öâƒrÂÂe³ÒÂ#BÂGFW&äæ÷FU&÷FF–öã£¤ÆVgB¢æW‡V7B‚'6VÆV7FVBæ÷FR6†÷VÆB&÷FFR"’À¢¢“°¢76W'EöW‡&–v‡BçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Òç÷6—F–öâÂ“° ¢ÆWB×WBÆVgBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆVgBç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂÂRÂ‚¢æW‡V7B‚&–æ—F–ÂGFW&âÖWFW"6†÷VÆB&R6WB"“°¢ÆVgBç6WE÷GFW&å÷F–ÖU÷6–væGW&RƒrÂCƒÂ2Â‚¢æW‡V7B‚&ÆFW"GFW&âÖWFW"6†÷VÆB&R6WB"“°¢76W'EöW€¢ÆVgBç&÷FFU÷GFW&åöæ÷FW2ƒrÂÂ#BÂGFW&äæ÷FU&÷FF–öã£¤ÆVgB¢æW‡V7B‚&ÆVgB&÷FF–öâ6†÷VÆB7V66VVB"’À¢@¢“°¢76W'EöW€¢ÆVgBçGFW&ç2‚’çVçw&‚•³Ð¢ææ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³#bÂ#ÂCSÂc#ÂÐ¢“° ¢ÆWB×WB–çfÆ–BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒ–çfÆ–@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&÷&–v–æÂ&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'B€¢–çfÆ–@¢ç&÷FFU÷GFW&åöæ÷FW2ƒrÂÂÂGFW&äæ÷FU&÷FF–öã£¥&–v‡B¢æ—5öW'"‚¢“°¢76W'EöW†–çfÆ–BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ'F–7VÆFU÷fÆ–FFW5÷&ævW5öæEö6öçFW‡E÷&WV—&VÖVçG2‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e¶æ÷FUÒÂe³„dbÂÒ’¢æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒ’ÂÂÂG'VR’¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒÂÂÂG'VR’¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒSÂÂÂfÇ6R’¢æ—5öö²‚¢“°¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÒÀ¢'F–7VÆFUö÷F–öç2ƒSÂÂÂfÇ6R’À¢G'VRÀ¢¢æ—5öö²‚¢“°¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FU÷6VÆV7F–öâ€¢rÀ¢À¢e³ÒÀ¢'F–7VÆFUö÷F–öç2ƒSÂÂÂG'VR’À¢G'VRÀ¢¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒÂÂÂG'VR’¢æ—5öW'"‚¢“° ¢ÆWB¦W&õöÆVæwF…öæ÷FRÒæ÷FU÷&V6÷&BƒÂÂÂC‚Â““°¢ÆWB×WB¦W&õöÆVæwF‚ÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e·¦W&õöÆVæwF…öæ÷FUÒÂe³„dbÂÒ’¢æW‡V7B‚'¦W&òÖÆVæwF‚f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢¦W&õöÆVæwF€¢æ'F–7VÆFU÷GFW&åöæ÷FW2ƒrÂÂ'F–7VÆFUö÷F–öç2ƒSÂSÂÂG'VR’¢çVçw&‚’À¢ ¢“°¢76W'EöW‡¦W&õöÆVæwF‚çGFW&ç2‚’çVçw&‚•³Òææ÷FW5³ÒæÆVæwF‚Â“°¢Ð ¢5·FW7EÐ¢fâæ÷FUöWfVçEöÆVæwF…÷&Vf—…öw&÷w5÷Fõ÷Gvõö'—FW2‚’°¢ÆWB÷&–v–æÅöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶÷&–v–æÅöæ÷FUÒÂe³„dbÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢f÷"–æFW‚–ââã°¢Fö7VÖVç@¢æFE÷GFW&åöæ÷FR€¢rÀ¢GFW&äæ÷FR°¢÷6—F–öã¢–æFW‚¢#BÀ¢6†ææVÅö–C¢À¢ÆVæwFƒ¢#BÀ¢¶W“¢cÀ¢fVÆö6—G“¢“À¢âåGFW&äæ÷FS£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&æ÷FR6†÷VÆB&RVæFVB"“°¢Ð¢ÆWBæ÷FW5öWfVçBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„C¢æW‡V7B‚&æ÷FRWfVçB6†÷VÆB&VÖ–â"“°¢76W'EöW€¢æ÷FW5öWfVçBæVæ6öF–ær‚’À¢e–ÆöDVæ6öF–æs£¤FF°¢ÆVæwF…÷&Vf—ƒ¢fV2³ƒƒ‚Âƒ%Ð¢Ð¢“°¢76W'EöW†Fö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â“°¢Ð ¢5·FW7EÐ¢fâFF–æu÷FõöåöV×G•÷GFW&å÷W6W5÷F†U÷&ö¦V7G5öö'6W'fVEöæ÷FUöVæ6öF–ær‚’°¢ÆWBW†—7F–æuöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ó°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fW†—7F–æuöæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„BÂÂÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æFE÷GFW&åöæ÷FR€¢‚À¢GFW&äæ÷FR°¢÷6—F–öã¢#BÀ¢6†ææVÅö–C¢À¢ÆVæwFƒ¢#BÀ¢¶W“¢cRÀ¢fVÆö6—G“¢“À¢âåGFW&äæ÷FS£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'F†RV×G’GFW&â6†÷VÆBW6RF†Rö'6W'fVBCVæ6öF–ær"“° ¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚“°¢ÆWBV×G•÷GFW&âÒGFW&ç2æ—FW"‚’æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒ‚’çVçw&‚“°¢76W'EöW†V×G•÷GFW&âææ÷FW2æÆVâ‚’Â“°¢76W'EöW†V×G•÷GFW&âææ÷FW5³Òæ¶W’ÂcR“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’’çVçw&‚“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â“°¢Ð ¢5·FW7EÐ¢fâ7&VF–æuöåöV×G•÷GFW&å÷W6W5÷F†Uöö'6W'fVEöVæ6öF–æuöæE÷&W6W'fW5ö÷F†W%öWfVçG2‚’°¢ÆWBW†—7F–æuöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ó°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fW†—7F–æuöæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„BÂƒƒÂƒÂÂÒ“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ%Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂrÂÂƒ“bÂÂÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂe³„ÒÂe³„#%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅöWfVçG3¢fV3ÅóâÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7B‚“° ¢ÆWBæWuö–BÒFö7VÖVç@¢æ7&VFU÷GFW&â‚¢æW‡V7B‚'F†RW†—7F–ærGFW&âVæ6öF–ær6†÷VÆB&R&WW6&ÆR"“° ¢76W'EöW†æWuö–BÂ‚“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"“°¢ÆWB7&VFVBÒGFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒæWuö–B¢æW‡V7B‚&æWrGFW&â6†÷VÆB&R&W6VçB"“°¢76W'B†7&VFVBææ÷FW2æ—5öV×G’‚’“°¢76W'B†7&VFVBææÖRæ—5öæöæR‚’“°¢ÆWBæWuöÖ&¶W"ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’ÓÒæWuö–BçFõöÆUö'—FW2‚’¢æW‡V7B‚&æWrGFW&âÖ&¶W"6†÷VÆB&R&W6VçB"“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•¶æWuöÖ&¶W"²Òæ÷6öFR‚’Â„C“°¢76W'B†Fö7VÖVçBæWfVçG2‚•¶æWuöÖ&¶W"²Òç–ÆöB‚’æ—5öV×G’‚’“° ¢ÆWB7W'&VçEöWfVçG3¢fV3ÅóâÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂ°¢†WfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’ÓÒæWuö–BçFõöÆUö'—FW2‚’¢bb†WfVçBæ÷6öFR‚’ÓÒ„CbbWfVçBç–ÆöB‚’æ—5öV×G’‚’¢Ò¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7B‚“°¢76W'EöW†7W'&VçEöWfVçG2Â÷&–v–æÅöWfVçG2“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚'&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&7&VFVB&ö¦V7B6†÷VÆB&W'6R"“°¢76W'B€¢&W'6V@¢çGFW&ç2‚¢çVçw&‚¢æ—FW"‚¢æç’‡ÇGFW&çÂGFW&âæ–BÓÒæWuö–B¢“°¢76W'EöW‡&W'6VBçG&–Æ–æuö'—FW2‚’Âe³„#%Ò“°¢Ð ¢5·FW7EÐ¢fâGWÆ–6F–æuö÷GFW&åö6÷–W5öæ÷FW5ö6öçG&öÆÆW'5÷F–ÖUöÖ&¶W'5öæEöÖWFFFöÆ÷76ÆW76Ç’‚’°¢ÆWB×WBæ÷FRÒæ÷FU÷&V6÷&BƒC‚ÂÂ“"ÂcBÂ“2“°¢æ÷FU³UÒÒ„S°¢æ÷FU³ÒÒƒ3s°¢ÆWB×WB7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3rÂ"##bããÃ"“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢VæE÷F–ÖUöÖ&¶W"€¢f×WB7G&VÒÀ¢7WW#£¥D”ÔUôÔ$´U%õ4”täEU$Uô$•BÂ#CÀ¢2À¢‚À¢%GFW&âÖWFW""À¢“°¢ÆWBf—'7Eö6öçG&öÆÆW%÷–ÆöBÒ³ƒÂÂÂÂ„RÂƒTÂƒ2Â„dRÂƒÂÂ„3ÂƒteÓ°¢ÆWB6V6öæEö6öçG&öÆÆW%÷–ÆöBÒ³ƒ#ÂÂÂÂƒd"ÂƒDBÂƒrÂƒƒÂÂÂƒƒÂƒ4eÓ°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„DbÂff—'7Eö6öçG&öÆÆW%÷–ÆöB“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„DbÂg6V6öæEö6öçG&öÆÆW%÷–ÆöB“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„BÂƒƒÂƒrÂÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WB7G&VÒÂ„3Â%fW'6R"“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ%Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂe³„ÒÂe³„#%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅöWfVçEö'—FW2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢ÆWBæWu÷GFW&åö–BÒFö7VÖVç@¢æGWÆ–6FU÷GFW&âƒr¢æW‡V7B‚'F†R&V6övæ—¦VBGFW&â6†÷VÆBGWÆ–6FR"“° ¢76W'EöW†æWu÷GFW&åö–BÂ‚“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"“°¢ÆWB6÷W&6RÒGFW&ç2æ—FW"‚’æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒr’çVçw&‚“°¢ÆWBGWÆ–6FRÒGFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒæWu÷GFW&åö–B¢çVçw&‚“°¢76W'EöW†GWÆ–6FRææ÷FW2Â6÷W&6Rææ÷FW2“°¢76W'EöW†GWÆ–6FRæ6öçG&öÆÆW'2Â6÷W&6Ræ6öçG&öÆÆW'2“°¢76W'EöW†GWÆ–6FRæ6öçG&öÆÆW'2æÆVâ‚’Â"“°¢76W'EöW†GWÆ–6FRçF–ÖUöÖ&¶W'2æÆVâ‚’Â“°¢76W'EöW€¢GWÆ–6FRçF–ÖUöÖ&¶W'5³Òç&u÷÷6—F–öâ‚’À¢6÷W&6RçF–ÖUöÖ&¶W'5³Òç&u÷÷6—F–öâ‚¢“°¢76W'EöW†GWÆ–6FRçF–ÖUöÖ&¶W'5³ÒæçVÖW&F÷"‚’Â6öÖRƒ2’“°¢76W'EöW†GWÆ–6FRçF–ÖUöÖ&¶W'5³ÒæFVæöÖ–æF÷"‚’Â6öÖRƒ‚’“°¢76W'EöW†GWÆ–6FRçF–ÖUöÖ&¶W'5³ÒææÖR‚’Â6öÖR‚%GFW&âÖWFW""’“°¢76W'EöW†GWÆ–6FRæÆVæwF…÷F–6·2Â6öÖRƒ“#’“°¢76W'EöW†GWÆ–6FRææÖRæ5öFW&Vb‚’Â6öÖR‚%fW'6R"’“° ¢ÆWBGWÆ–6FUöÖ&¶W"ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂ°¢WfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’ÓÒæWu÷GFW&åö–BçFõöÆUö'—FW2‚¢Ò¢æW‡V7B‚&GWÆ–6FRÖ&¶W"6†÷VÆB&R&W6VçB"“°¢76W'EöW†Fö7VÖVçBæWfVçG2‚•¶GWÆ–6FUöÖ&¶W"²Òæ÷6öFR‚’Â„C“°¢ÆWBGWÆ–6FUöVæBÒFö7VÖVçBæWfVçG2‚•¶GWÆ–6FUöÖ&¶W"²âåÐ¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂ°¢†WfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’æÆVâ‚’ÓÒ"¢ÇÂÖF6†W2†WfVçBæ÷6öFR‚’ÂƒCÂƒc"Âƒc2¢Ò¢æÖö÷"†Fö7VÖVçBæWfVçG2‚’æÆVâ‚’ÂÆöfg6WGÂ°¢GWÆ–6FUöÖ&¶W"²²öfg6W@¢Ò“°¢ÆWBGWÆ–6FUöWfVçG2ÒfFö7VÖVçBæWfVçG2‚•¶GWÆ–6FUöÖ&¶W"²"âæGWÆ–6FUöVæEÓ°¢ÆWB6÷W&6U÷F–ÖUöÖ&¶W%öWfVçEö'—FW2Ò÷&–v–æÅöWfVçEö'—FW0¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçE³ÒÂƒ“BÂƒ#Âƒ#"Â„4B’¢æ6ÆöæVB‚¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWBGWÆ–6FU÷F–ÖUöÖ&¶W%öWfVçEö'—FW2ÒGWÆ–6FUöWfVçG0¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFR‚’Âƒ“BÂƒ#Âƒ#"Â„4B’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢GWÆ–6FU÷F–ÖUöÖ&¶W%öWfVçEö'—FW2À¢6÷W&6U÷F–ÖUöÖ&¶W%öWfVçEö'—FW0¢“°¢ÆWBGWÆ–6FUö6öçG&öÆÆW%öWfVçG2ÒGWÆ–6FUöWfVçG0¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„Db¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†GWÆ–6FUö6öçG&öÆÆW%öWfVçG2æÆVâ‚’Â"“°¢76W'EöW€¢GWÆ–6FUö6öçG&öÆÆW%öWfVçG5³Òç–ÆöB‚’À¢f—'7Eö6öçG&öÆÆW%÷–Æö@¢“°¢76W'EöW€¢GWÆ–6FUö6öçG&öÆÆW%öWfVçG5³Òç–ÆöB‚’À¢6V6öæEö6öçG&öÆÆW%÷–Æö@¢“°¢76W'B†GWÆ–6FUöWfVçG2æ—FW"‚’æç’‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„B’“°¢76W'B†GWÆ–6FUöWfVçG2æ—FW"‚’æç’‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„3’“°¢ÆWBWfVçEö'—FW5÷v—F†÷WEöGWÆ–6FRÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æVçVÖW&FR‚¢æf–ÇFW"‡Â†–æFW‚Âò—Â‚¦–æFW‚ãÒGWÆ–6FUöÖ&¶W"bb¦–æFW‚ÂGWÆ–6FUöVæB’¢æÖ‡Â…òÂWfVçB—ÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†WfVçEö'—FW5÷v—F†÷WEöGWÆ–6FRÂ÷&–v–æÅöWfVçEö'—FW2“° ¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’¢æW‡V7B‚&GWÆ–6FVB&ö¦V7B6†÷VÆB&W'6R"“°¢ÆWB&V÷VæVEöGWÆ–6FRÒ&V÷VæV@¢çGFW&ç2‚¢çVçw&‚¢æ–çFõö—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒæWu÷GFW&åö–B¢çVçw&‚“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRææ÷FW2Â6÷W&6Rææ÷FW2“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRæ6öçG&öÆÆW'2Â6÷W&6Ræ6öçG&öÆÆW'2“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRçF–ÖUöÖ&¶W'5³ÒæçVÖW&F÷"‚’Â6öÖRƒ2’“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRçF–ÖUöÖ&¶W'5³ÒæFVæöÖ–æF÷"‚’Â6öÖRƒ‚’“°¢76W'EöW€¢&V÷VæVEöGWÆ–6FRçF–ÖUöÖ&¶W'5³ÒææÖR‚’À¢6öÖR‚%GFW&âÖWFW""¢“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRæÆVæwF…÷F–6·2Â6öÖRƒ“#’“°¢76W'EöW‡&V÷VæVEöGWÆ–6FRææÖRæ5öFW&Vb‚’Â6öÖR‚%fW'6R"’“°¢76W'EöW‡&V÷VæVBçG&–Æ–æuö'—FW2‚’Âe³„#%Ò“°¢Ð ¢5·FW7EÐ¢fâ&VæÖ–æu÷GFW&ç5÷&W6W'fW5÷Vç&VÆFVEöWfVçG5öæEöFG5öÖ—76–æuöæÖW2‚’°¢ÆWB×WB7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3rÂ"##bããÃ"“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒCÂrÂÂ„CÂÒ“°¢ÆWB×WBöÆEöæÖRÒWFce÷&ö¦V7E÷7G&–ær‚%fW'6R"“°¢öÆEöæÖRæW‡FVæEög&öÕ÷6Æ–6R‚e³„RÂƒTÒ“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3ÂföÆEöæÖR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„CÂÒ“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ%Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂe³„ÒÂe³„#%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWBVç&VÆFVEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’Ò„3¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢Fö7VÖVç@¢ç6WE÷GFW&åöæÖRƒrÂ%fW'6R(	2ÆVB"¢æW‡V7B‚'F†RæÖVBGFW&â6†÷VÆB&R&VæÖVB"“°¢Fö7VÖVç@¢ç6WE÷GFW&åöæÖRƒ‚Â$6†÷'W2"¢æW‡V7B‚&Ö—76–ærGFW&âæÖR6†÷VÆB&RFFVB"“° ¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚%GFW&ç26†÷VÆBFV6öFR"“°¢76W'EöW€¢GFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒr¢çVçw&‚¢ææÖP¢æ5öFW&Vb‚’À¢6öÖR‚%fW'6R(	2ÆVB"¢“°¢76W'EöW€¢GFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒ‚¢çVçw&‚¢ææÖP¢æ5öFW&Vb‚’À¢6öÖR‚$6†÷'W2"¢“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’Ò„3¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢Vç&VÆFVEöWfVçG0¢“°¢76W'B€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„3¢æç’‡ÆWfVçGÂWfVçBç–ÆöB‚’æVæG5÷v—F‚‚e³„RÂƒTÒ’¢“° ¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’æW‡V7B‚'&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'&VæÖVB&ö¦V7B6†÷VÆB&W'6R"“°¢ÆWB&V÷VæVE÷GFW&ç2Ò&V÷VæVBçGFW&ç2‚’çVçw&‚“°¢76W'EöW€¢&V÷VæVE÷GFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒr¢çVçw&‚¢ææÖP¢æ5öFW&Vb‚’À¢6öÖR‚%fW'6R(	2ÆVB"¢“°¢76W'EöW€¢&V÷VæVE÷GFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒ‚¢çVçw&‚¢ææÖP¢æ5öFW&Vb‚’À¢6öÖR‚$6†÷'W2"¢“°¢76W'EöW‡&V÷VæVBçG&–Æ–æuö'—FW2‚’Âe³„#%Ò“° ¢ÆWB&Vf÷&RÒ&V÷VæVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB×WB&V¦V7FVBÒ&V÷VæVC°¢76W'B‡&V¦V7FVBç6WE÷GFW&åöæÖRƒrÂ&&EÃæÖR"’æ—5öW'"‚’“°¢76W'B‡&V¦V7FVBç6WE÷GFW&åöæÖRƒ“’Â$Ö—76–ær"’æ—5öW'"‚’“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâFVÆWF–æuöå÷Vç&VfW&Væ6VE÷GFW&å÷&VÖ÷fW5ö—G5ö6ö×ÆWFUöWfVçE÷&Vv–öâ‚’°¢ÆWB×WB7G&VÒÒfV3£¦æWr‚“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FU÷&V6÷&BƒC‚Â"Â“"ÂcBÂ“2’“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³„BÂƒƒÂƒrÂÂÒ“°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WB7G&VÒÂ„3Â$öÆBGFW&â"“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„CÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FU÷&V6÷&BƒÂÂ“bÂcÂ’“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ%Ò“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂe³„ÒÂe³„#%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWBÖ&¶W%ö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢ç÷6—F–öâ‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒƒCbbWfVçBç–ÆöB‚’ÓÒu÷SbçFõöÆUö'—FW2‚’¢çVçw&‚“°¢ÆWBæW‡E÷&Vv–öåö–æFW‚ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æVçVÖW&FR‚¢ç6¶—†Ö&¶W%ö–æFW‚²¢æf–æB‡Â…òÂWfVçB—ÂWfVçBæ÷6öFR‚’ÓÒƒC¢æÖ‡Â†–æFW‚Âò—Â–æFW‚¢çVçw&‚“°¢ÆWBW‡V7FVEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æVçVÖW&FR‚¢æf–ÇFW"‡Â†–æFW‚Âò—Â¦–æFW‚ÂÖ&¶W%ö–æFW‚ÇÂ¦–æFW‚ãÒæW‡E÷&Vv–öåö–æFW‚¢æÖ‡Â…òÂWfVçB—ÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢Fö7VÖVç@¢æFVÆWFU÷GFW&âƒr¢æW‡V7B‚'Vç&VfW&Væ6VBGFW&â6†÷VÆB&RFVÆWFVB"“° ¢ÆWBGFW&ç2ÒFö7VÖVç@¢çGFW&ç2‚¢æW‡V7B‚'&VÖ–æ–ærGFW&ç26†÷VÆBFV6öFR"“°¢76W'EöW‡GFW&ç2æÆVâ‚’Â“°¢76W'EöW‡GFW&ç5³Òæ–BÂ‚“°¢76W'EöW‡GFW&ç5³Òææ÷FW2æÆVâ‚’Â“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢W‡V7FVEöWfVçG0¢“°¢ÆWBVæ6öFVBÒFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&FVÆWFVB&ö¦V7B6†÷VÆB&W'6R"“°¢76W'EöW‡&V÷VæVBçGFW&ç2‚’çVçw&‚•³Òæ–BÂ‚“°¢76W'EöW‡&V÷VæVBçG&–Æ–æuö'—FW2‚’Âe³„#%Ò“°¢Ð ¢5·FW7EÐ¢fâFVÆWF–æuö÷&VfW&Væ6VEö÷%öÖ&–wV÷W5÷GFW&åö—5÷&V¦V7FVEöFöÖ–6ÆÇ’‚’°¢ÆWB×WB&VfW&Væ6VBÒF¦6VçE÷GFW&åö6Æ—öf—‡GW&Rƒ“cÂ“cÂC‚Â“b“°¢ÆWB&Vf÷&RÒ&VfW&Væ6VBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B‡&VfW&Væ6VBæFVÆWFU÷GFW&âƒr’æ—5öW'"‚’“°¢76W'EöW‡&VfW&Væ6VBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“° ¢ÆWB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂÂƒCÂrÂÂ„CÂÓ°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBÖ&–wV÷W2ÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒÖ&–wV÷W2æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚“°¢76W'B†Ö&–wV÷W2æFVÆWFU÷GFW&âƒr’æ—5öW'"‚’“°¢76W'B†Ö&–wV÷W2æFVÆWFU÷GFW&âƒ“’’æ—5öW'"‚’“°¢76W'EöW†Ö&–wV÷W2æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ7&VF–æuö÷GFW&å÷&VgW6W5öÖ&–wV÷W5öæ÷FUöWfVçEöVæ6öF–æw5÷v—F†÷WEö×WFF–öâ‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ó°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„SÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBæ7&VFU÷GFW&â‚’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâÖ&–wV÷W5öV×G•÷GFW&åöæ÷FUöVæ6öF–æuö—5÷&V¦V7FVE÷v—F†÷WEö×WFF–öâ‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ó°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„SÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ’ÂÂ„BÂÂÂÂÒ“°¢ÆWB–çWBÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBW'&÷"ÒFö7VÖVç@¢æFE÷GFW&åöæ÷FR€¢’À¢GFW&äæ÷FR°¢6†ææVÅö–C¢À¢ÆVæwFƒ¢#BÀ¢¶W“¢cRÀ¢fVÆö6—G“¢“À¢âåGFW&äæ÷FS£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7EöW'"‚&6öæfÆ–7F–ær&ö¦V7BVæ6öF–æw2×W7Bæ÷B&RwVW76VB"“° ¢76W'B†ÖF6†W2†W'&÷"ÂfÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâFVÆWF–æuööæ÷FU÷&W6W'fW5÷F†U÷&VÖ–æ–æu÷&V6÷&G5öæEö6åö&U÷&WfW'6VB‚’°¢ÆWBf—'7BÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB6V6öæBÒæ÷FU÷&V6÷&Bƒ“bÂÂ#BÂcBÂƒ“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚e¶f—'7BÂ6V6öæEÒÂe³„dbÂÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢æFVÆWFU÷GFW&åöæ÷FRƒrÂÂ¢æW‡V7B‚'F†Rf—'7Bæ÷FR6†÷VÆB&R&VÖ÷fVB"“°¢ÆWB&VÖ–æ–ærÒFö7VÖVçBçGFW&ç2‚’çVçw&‚“°¢76W'EöW‡&VÖ–æ–æu³Òææ÷FW2æÆVâ‚’Â“°¢76W'EöW‡&VÖ–æ–æu³Òææ÷FW5³Òç÷6—F–öâÂ“b“°¢76W'EöW‡&VÖ–æ–æu³Òææ÷FW5³Òæ¶W’ÂcB“° ¢Fö7VÖVç@¢æFE÷GFW&åöæ÷FR€¢rÀ¢GFW&äæ÷FR°¢÷6—F–öã¢À¢6†ææVÅö–C¢À¢ÆVæwFƒ¢C‚À¢¶W“¢cÀ¢fVÆö6—G“¢À¢âåGFW&äæ÷FS£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&âV×G’æ÷FRWfVçB6†÷VÆB&WF–â—G2Væ6öF–ær"“°¢ÆWB&W7F÷&VBÒFö7VÖVçBçGFW&ç2‚’çVçw&‚“°¢76W'EöW‡&W7F÷&VE³Òææ÷FW2æÆVâ‚’Â"“°¢76W'EöW‡&W7F÷&VE³Òææ÷FW5³Òç÷6—F–öâÂ“°¢Ð ¢5·FW7EÐ¢fâFVÆWF–æu÷6VÆV7FVEö÷%ö6†ææVÅöæ÷FW5÷&W6W'fW5ö÷F†W%ö6†ææVÇ5öFöÖ–6ÆÇ’‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R€¢e°¢æ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ’À¢æ÷FU÷&V6÷&Bƒ"ÂÂ#BÂC‚Âƒ’À¢æ÷FU÷&V6÷&Bƒ#BÂÂ3bÂcBÂ“’À¢æ÷FU÷&V6÷&Bƒ3bÂÂ"ÂS"Âs’À¢ÒÀ¢e³„dbÂÂƒTÒÀ¢“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æFVÆWFU÷GFW&åöæ÷FU÷6VÆV7F–öâƒrÂÂe³Ò¢æW‡V7B‚'6VÆV7FVBæ÷FR6†÷VÆB&RFVÆWFVB"’À¢¢“°¢ÆWB&VÖ–æ–ærÒFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW€¢&VÖ–æ–æp¢æ—FW"‚¢æÖ‡Ææ÷FWÂ†æ÷FRæ6†ææVÅö–BÂæ÷FRç÷6—F–öâ’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢²ƒÂ’ÂƒÂ"’ÂƒÂ3b•Ð¢“°¢76W'EöW€¢Fö7VÖVç@¢æFVÆWFU÷GFW&åöæ÷FW2ƒrÂ¢æW‡V7B‚'F&vWBÖ6†ææVÂæ÷FW26†÷VÆB&RFVÆWFVB"’À¢¢“°¢ÆWB&VÖ–æ–ærÒFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æ6ÆöæR‚“°¢76W'EöW€¢&VÖ–æ–æp¢æ—FW"‚¢æÖ‡Ææ÷FWÂ†æ÷FRæ6†ææVÅö–BÂæ÷FRç÷6—F–öâ’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢²ƒÂ"’ÂƒÂ3b•Ð¢“°¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2Â&VÖ–æ–ær“°¢76W'EöW€¢&W'6VBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂÂƒTÐ¢“° ¢ÆWB×WB–çfÆ–BÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚&f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&Vf÷&RÒ–çfÆ–@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&÷&–v–æÂ&ö¦V7B6†÷VÆBVæ6öFR"“°¢76W'B†–çfÆ–BæFVÆWFU÷GFW&åöæ÷FU÷6VÆV7F–öâƒrÂÂe³%Ò’æ—5öW'"‚’“°¢76W'EöW†–çfÆ–BæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&Vf÷&R“°¢Ð ¢5·FW7EÐ¢fâ–×÷'G5öÖ–F•÷G&6µöæ÷FW5÷66ÆVE÷Fõ÷&ö¦V7E÷öæE÷&W6W'fW5ö÷F†W%öWfVçG2‚’°¢ÆWB÷&–v–æÅöæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂC‚Âƒ“°¢ÆWBVæ¶æ÷våöWfVçBÒ³„dbÂƒ"Â„Â„$%Ó°¢ÆWB&ö¦V7Eö'—FW2ÒGFW&åöf—‡GW&R‚e¶÷&–v–æÅöæ÷FUÒÂgVæ¶æ÷våöWfVçB“°¢ÆWBÖ–F•ö'—FW2ÒÖ–F•öf—‡GW&R€¢e°¢ƒÂ„#’ÂÂÂòò6†ææVÂ’â—2†&BÆVgBà¢ƒÂ„#’ÂÂÂòò6VÆV7B%âÃ¢—F6‚Ö&VæB6Vç6—F—f—G’à¢ƒÂ„#’ÂÂÂƒÂ„#’ÂbÂ"Âòò6WB"×6VÖ—FöæR&VæB&ævRà¢ƒÂ„#’Â3‚ÂÂƒÂ„#’ÂÂ#rÂòòFW6VÆV7B%âÃà¢ƒÂ„#’ÂÂ#rÂƒÂ„S’ÂÀ¢“bÂòò³c6VçG2BF†R"×6VÖ—FöæRÖ–Gö–çBà¢ƒÂ„#’ÂsBÂ#rÂòòÖöB‚—2†–v‚f÷"F†Rf—'7Bæ÷FRà¢ƒÂ„#’ÂsÂÂòòÖöB’—2Æ÷rf÷"F†Rf—'7Bæ÷FRà¢ƒÂƒ“’Âƒ42ÂƒcBÂòò6†ææVÂ’Â¶W’cÂfVÆö6—G’BF–6²à¢ƒƒ2ÂƒcÂƒƒ’Âƒ42Âƒ#Âòòæ÷FRöfbBF–6²Cƒà¢ƒÂ„#ÂÂ#rÂòò6†ææVÂâ—2†&B&–v‡Bà¢ƒÂ„#ÂsBÂÂòòÖöB‚—2Æ÷rf÷"F†R6V6öæBæ÷FRà¢ƒÂ„#ÂsÂ#rÂòòÖöB’—2†–v‚f÷"F†R6V6öæBæ÷FRà¢ƒÂ„SÂÂ3"ÂòòÓ6VçG2BF†RFVfVÇBGvò×6VÖ—FöæR&ævRà¢ƒÂƒ“ÂƒC2ÂƒSÂòò6†ææVÂÂ¶W’crÂfVÆö6—G’ƒBF–6²Cƒà¢ƒƒÂƒsÂƒƒÂƒC2ÂƒÂòòæ÷FRöfbBF–6²s#à¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•ö'—FW2’æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6²‚fÖ–F’ÂÂrÂ¢æW‡V7B‚'F†RG&6²æ÷FW26†÷VÆB–×÷'B"’À¢ ¢“° ¢ÆWBæ÷FW2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â2“°¢76W'EöW€¢†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚Âæ÷FW5³Òæ¶W’’À¢ƒÂ“bÂc¢“°¢76W'EöW†æ÷FW5³ÒçfVÆö6—G’Â“°¢76W'EöW†æ÷FW5³ÒæÖ–F•ö6†ææVÂÂ’“°¢76W'EöW†æ÷FW5³Òç&VÆV6RÂ3"“°¢76W'EöW†æ÷FW5³ÒçâÂ“°¢76W'EöW†æ÷FW5³ÒæÖöE÷‚Â#SR“°¢76W'EöW†æ÷FW5³ÒæÖöE÷’Â“°¢76W'EöW†æ÷FW5³Òæf–æU÷—F6‚Âƒ“°¢76W'EöW€¢†æ÷FW5³%Òç÷6—F–öâÂæ÷FW5³%ÒæÆVæwF‚Âæ÷FW5³%Òæ¶W’’À¢ƒ“bÂC‚Âcr¢“°¢76W'EöW†æ÷FW5³%ÒçfVÆö6—G’Âƒ“°¢76W'EöW†æ÷FW5³%ÒæÖ–F•ö6†ææVÂÂ“°¢76W'EöW†æ÷FW5³%Òç&VÆV6RÂ“°¢76W'EöW†æ÷FW5³%ÒçâÂ#‚“°¢76W'EöW†æ÷FW5³%ÒæÖöE÷‚Â“°¢76W'EöW†æ÷FW5³%ÒæÖöE÷’Â#SR“°¢76W'EöW†æ÷FW5³%Òæf–æU÷—F6‚Â“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢gVæ¶æ÷våöWfVç@¢“° ¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’’çVçw&‚“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â2“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö–×÷'Eö6å÷G&VE÷¦W&õ÷fVÆö6—G•öæ÷FUööç5ö5öæ÷FW2‚’°¢ÆWB&ö¦V7Eö'—FW2ÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWBÖ–F•ö'—FW2ÒÖ–F•öf—‡GW&R€¢e°¢ƒÂƒ“Âƒ42ÂƒÂòò¦W&ò×fVÆö6—G’æ÷FRöâBF–6²à¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒCÂòòæ÷FRöfbBF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•ö'—FW2’æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB×WBFVfVÇEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢FVfVÇEöFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6²‚fÖ–F’ÂÂrÂ¢æW‡V7B‚&FVfVÇBÔ”D’–×÷'B6†÷VÆB7V66VVB"’À¢ ¢“° ¢ÆWB×WB÷FVEö–åöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢÷FVEö–åöFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6µ÷v—F…ö÷F–öç2€¢fÖ–F’À¢À¢rÀ¢À¢Ö–F”–×÷'D÷F–öç2°¢¦W&õ÷fVÆö6—G•öæ÷FUööç5ö5öæ÷FW3¢G'VRÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&÷F–öâÖv&RÔ”D’–×÷'B6†÷VÆB7V66VVB"’À¢¢“°¢ÆWBæ÷FRÒf÷FVEö–åöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Ó°¢76W'EöW‚†æ÷FRç÷6—F–öâÂæ÷FRæÆVæwF‚Âæ÷FRæ¶W’’ÂƒÂ“bÂc’“°¢76W'EöW†æ÷FRçfVÆö6—G’Â“°¢76W'EöW†æ÷FRç&VÆV6RÂcR“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö6öçG&öÅö6†ævW5ö–×÷'Eö–çFõöåöW†—7F–æu÷&uö6öçG&öÆÆW%÷7G&VÒ‚’°¢ÆWBf—'7BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³„RÂƒTÒÂ2Âƒ“Âã#Vc3"çFõö&—G2‚’“°¢ÆWB÷F†W%÷7G&VÒÒGFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ3"Â³„C2ÂƒD5ÒÂrÂƒ$ÂãVc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%÷–ÆöBÒf—'7BçFõ÷fV2‚“°¢6öçG&öÆÆW%÷–ÆöBæW‡FVæEög&öÕ÷6Æ–6R‚f÷F†W%÷7G&VÒ“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂf6öçG&öÆÆW%÷–ÆöB“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e°¢ƒÂ„dbÂƒSÂƒ2ÂƒrÂ„Âƒ#Âòò#%Òà¢ƒÂ„#ÂsBÂÂòò42sBÂÔ”D’6†ææVÂÂF–6²à¢ƒƒÂƒsÂ„#ÂsBÂcBÂòò42sBÂÔ”D’6†ææVÂ"ÂF–6²#Cà¢ƒƒÂƒsÂ„#ÂsBÂ#rÂòò42sBÂÔ”D’6†ææVÂÂF–6²Cƒà¢ƒÂ„#ÂsÂÂòòF–ffW&VçB42çVÖ&W"Â6ÖRF–6²à¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB–×÷'FVBÒFö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW2‚fÖ–F’ÂÂrÂƒ2Âƒ“’ÂsBÂ6öÖRƒ’¢æW‡V7B‚'F†R6VÆV7FVB426†÷VÆB–×÷'B"“°¢76W'EöW†–×÷'FVBÂ"“° ¢ÆWBGFW&âÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ó°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'2æÆVâ‚’Â2“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç÷6—F–öâÂ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’Âã“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒæVæ6öFR‚’Â÷F†W%÷7G&VÒ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³%Òç÷6—F–öâÂ"“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³%Òç&W6W'fVBÂ³„RÂƒTÒ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³%Òæ6†ææVÂÂ2“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³%ÒæfÆw2Âƒ““°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³%ÒçfÇVR‚’Âã“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R–×÷'FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R–×÷'FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&÷VæE÷G&—çGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'2À¢GFW&âæ6öçG&öÆÆW'0¢“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö6öçG&öÅö6†ævW5ö6åö7&VFUö÷&uö6öçG&öÆÆW%÷7G&VÕöFöÖ–6ÆÇ’‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e°¢ƒÂ„#ÂsBÂcBÂòò42sBBF–6²à¢ƒƒ2ÂƒcÂ„#ÂsBÂ#rÂòò42sBBF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB–×÷'FVBÒFö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW5÷FõöæWu÷7G&VÒ‚fÖ–F’ÂÂrÂƒ2Âƒtb’ÂsBÂ6öÖRƒ’¢æW‡V7B‚'F†R6VÆV7FVB426†÷VÆB7&VFR&r7G&VÒ"“°¢76W'EöW†–×÷'FVBÂ"“° ¢ÆWBGFW&âÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ó°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'2æÆVâ‚’Â"“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç÷6—F–öâÂ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç&W6W'fVBÂ³²%Ò“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òæ6†ææVÂÂ2“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒæfÆw2Âƒtb“°¢76W'B‚‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’ÒcBãò#rã’æ'2‚’Âc3#£¤U4”Äôâ“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òç÷6—F–öâÂ“b“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³Òæ6†ææVÂÂ2“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒæfÆw2Âƒtb“°¢76W'EöW‡GFW&âæ6öçG&öÆÆW'5³ÒçfÇVR‚’Âã“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚'F†R–×÷'FVB&ö¦V7B6†÷VÆBVæ6öFR"“°¢ÆWB&÷VæE÷G&—ÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚'F†R–×÷'FVB&ö¦V7B6†÷VÆB'6R"“°¢76W'EöW€¢&÷VæE÷G&—çGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'2À¢GFW&âæ6öçG&öÆÆW'0¢“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö6öçG&öÅö6†ævUöæWu÷7G&VÕö–×÷'E÷6¶—5öV×G•öFFöæE÷&V¦V7G5öGWÆ–6FW2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWBæõöÖF6†–æuö62ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e³ƒÂ„#ÂsÂ#rÂƒÂ„dbÂƒ$bÂƒÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW5÷FõöæWu÷7G&VÒ€¢fæõöÖF6†–æuö62À¢À¢rÀ¢ƒ2Âƒ“’À¢sBÀ¢æöæRÀ¢¢æW‡V7B‚&âVæÖF6†VB426†÷VÆB&RæòÖ÷"’À¢ ¢“°¢76W'B†Fö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òæ6öçG&öÆÆW'2æ—5öV×G’‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“° ¢ÆWB&V6÷&BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg&V6÷&B“°¢ÆWBW†—7F–æu÷7G&VÕö–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBW†—7F–æu÷7G&VÕöFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fW†—7F–æu÷7G&VÕö–çWB¢æW‡V7B‚'&ö¦V7Bf—‡GW&Rv—F‚âW†—7F–ær7G&VÒ6†÷VÆB'6R"“°¢ÆWBÖF6†–æuö62ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e³ƒÂ„#ÂsBÂ#rÂƒÂ„dbÂƒ$bÂƒÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢W†—7F–æu÷7G&VÕöFö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW5÷FõöæWu÷7G&VÒ‚fÖF6†–æuö62ÂÂrÂƒ2Âƒ“’ÂsBÂæöæRÂ¢æ—5öW'"‚¢“°¢76W'EöW€¢W†—7F–æu÷7G&VÕöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢W†—7F–æu÷7G&VÕö–çW@¢“°¢Ð ¢5·FW7EÐ¢fâÖ–F•öW‡÷'EöÖ5ööæU÷&uö6öçG&öÆÆW%÷7G&VÕ÷Fõ÷6VÆV7FVEö65öæEö6†ææVÂ‚’°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢ÆWB&V6÷&G2Ò°¢GFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’’À¢GFW&åö6öçG&öÆÆW%÷&V6÷&BƒC‚Â³²%ÒÂ‚ÂƒÂãsVc3"çFõö&—G2‚’’À¢GFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ“bÂ³²%ÒÂ2Âƒ“ÂãVc3"çFõö&—G2‚’’À¢GFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ“"Â³²%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’’À¢Ó°¢f÷"&V6÷&B–â&V6÷&G2°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg&V6÷&B“°¢Ð¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB'—FW2Ð¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&åö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂrÂƒ2Âƒ“’ÂsBÂ"¢æW‡V7B‚'F†R6VÆV7FVB&r7G&VÒ6†÷VÆBW‡÷'B2Ô”D’42"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBÔ”D’6†÷VÆB'6R"“°¢76W'EöW†Ö–F’çG&6·2‚’æÆVâ‚’Â"“°¢ÆWB6öçG&öÅö6†ævW2ÒÖ–F’çG&6·2‚•³Òæ6öçG&öÅö6†ævW2‚“°¢76W'EöW€¢6öçG&öÅö6†ævW0¢æ—FW"‚¢æÖ‡Æ6†ævWÂ€¢6†ævRçF–6²‚’À¢6†ævRæ6†ææVÂ‚’À¢6†ævRæ6öçG&öÆÆW"‚’À¢6†ævRçfÇVR‚¢’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢fV2²ƒÂ"ÂsBÂ’Âƒ“bÂ"ÂsBÂcB’Âƒ“"Â"ÂsBÂ#r•Ð¢“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö6öçG&öÆÆW%öW‡÷'E÷&V¦V7G5öÖ—76–æu÷7G&V×5öæEöæöåöf–æ—FU÷ö–çG2‚’°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWBV×G•öFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&åö6öçG&öÆÆW%÷7G&VÒ€¢fV×G•öFö7VÖVçBÀ¢rÀ¢ƒ2Âƒ“’À¢sBÀ¢À¢¢æ—5öW'"‚¢“° ¢ÆWB&V6÷&BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂ2Âƒ“Âc3#£¤äâçFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg&V6÷&B“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&åö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂrÂƒ2Âƒ“’ÂsBÂÂ¢æ—5öW'"‚¢“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&åö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂrÂƒ2Âƒ“’Â#‚ÂÂ¢æ—5öW'"‚¢“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&åö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂrÂƒ2Âƒ“’ÂsBÂbÂ¢æ—5öW'"‚¢“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö6öçG&öÅö6†ævUö–×÷'E÷&V¦V7G5ö–çfÆ–E÷F&vWG5÷v—F†÷WEö×WFF–ær‚’°¢ÆWB&V6÷&BÒGFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂ2Âƒ“ÂãVc3"çFõö&—G2‚’“°¢ÆWB×WB6öçG&öÆÆW%öWfVçBÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB6öçG&öÆÆW%öWfVçBÂ„DbÂg&V6÷&B“°¢ÆWB–çWBÒGFW&åöf—‡GW&R‚eµÒÂf6öçG&öÆÆW%öWfVçB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f–çWB’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e³ƒÂ„#ÂsBÂ#rÂƒÂ„dbÂƒ$bÂƒÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW2‚fÖ–F’ÂÂrÂƒ2Âƒ“’Â#‚ÂæöæR¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW2‚fÖ–F’ÂÂrÂƒ2Âƒ“’ÂsBÂ6öÖRƒb’¢æ—5öW'"‚¢“°¢76W'B€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•ö6öçG&öÅö6†ævW2‚fÖ–F’ÂÂrÂƒ’Âƒ“’ÂsBÂæöæR¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â–çWB“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö–×÷'E÷&VÆ–vç5öÆÅ÷G&6·5ög&öÕ÷F†UöV&Æ–W7Eöæ÷FR‚’°¢ÆWB&ö¦V7Eö'—FW2ÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒƒRÂƒSÂ„dbÂƒS‚ÂƒBÂƒBÂƒ"Âƒ‚Âƒ‚ÂòòBóBBF–6²s#à¢ƒƒÂƒsÂƒ“Âƒ42ÂƒcBÂòò3BBF–6²“cà¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒCÂòòæ÷FRöfbBF–6²CCà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒƒ2ÂƒcÂƒ“ÂƒC2ÂƒcBÂòòsBBF–6²Cƒà¢ƒƒÂƒsÂƒƒÂƒC2ÂƒCÂòòæ÷FRöfbBF–6²s#à¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“° ¢ÆWB×WBFVfVÇEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“°¢FVfVÇEöFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·5÷v—F…ö÷F–öç2‚fÖ–F’ÂrÂe³ÂÒÂÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚’¢æW‡V7B‚&FVfVÇBÔ”D’–×÷'B6†÷VÆB7V66VVB"“°¢ÆWBFVfVÇE÷GFW&âÒfFVfVÇEöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ó°¢76W'EöW€¢FVfVÇE÷GFW&à¢ææ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³“"Â“eÐ¢“°¢76W'EöW†FVfVÇE÷GFW&âçF–ÖUöÖ&¶W'5³Òç÷6—F–öå÷F–6·2‚’ÂCB“° ¢ÆWB×WB&VÆ–væVEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“°¢&VÆ–væVEöFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢e³ÂÒÀ¢Ö–F”–×÷'D÷F–öç2°¢&VÆ–våöWfVçG3¢G'VRÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'&VÆ–væVBÔ”D’–×÷'B6†÷VÆB7V66VVB"“°¢ÆWB&VÆ–væVE÷GFW&âÒg&VÆ–væVEöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Ó°¢76W'EöW€¢&VÆ–væVE÷GFW&à¢ææ÷FW0¢æ—FW"‚¢æÖ‡Ææ÷FWÂæ÷FRç÷6—F–öâ¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢³“bÂÐ¢“°¢76W'EöW‡&VÆ–væVE÷GFW&âçF–ÖUöÖ&¶W'5³Òç÷6—F–öå÷F–6·2‚’ÂC‚“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö–×÷'Eö6†ææVÅöf–ÇFW%öÇ6õö6öçG&öÇ5÷&VÆ–våö÷&–v–â‚’°¢ÆWB&ö¦V7Eö'—FW2ÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂòò6†ææVÂ7F'G2BF–6²à¢ƒƒ2ÂƒcÂƒ“ÂƒC2ÂƒcBÂòò6†ææVÂ"7F'G2BF–6²Cƒà¢ƒƒÂƒsÂƒƒÂƒC2ÂƒÂòò6†ææVÂ"æ÷FRöfbBF–6²s#à¢ƒƒÂƒsÂƒƒÂƒ42ÂƒÂòò6†ææVÂæ÷FRöfbBF–6²“cà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢’¢æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6µ÷v—F…ö÷F–öç2€¢fÖ–F’À¢À¢rÀ¢À¢Ö–F”–×÷'D÷F–öç2°¢&VÆ–våöWfVçG3¢G'VRÀ¢Ö–F•ö6†ææVÅöÖ6³¢SbÃÂÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&f–ÇFW&VBÔ”D’–×÷'B6†÷VÆB7V66VVB"’À¢¢“°¢ÆWBæ÷FW2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â“°¢76W'EöW€¢†æ÷FW5³Òç÷6—F–öâÂæ÷FW5³ÒæÆVæwF‚Âæ÷FW5³Òæ¶W’’À¢ƒÂC‚Âcr¢“°¢76W'EöW†æ÷FW5³ÒæÖ–F•ö6†ææVÂÂ“°¢Ð ¢5·FW7EÐ¢fâ–×÷'G5öÆÅöÖ–F•÷G&6·5÷Fõ÷F†V—%öÖVEö6†ææVÇ5öFöÖ–6ÆÇ’‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂòòG&6²¢3BBF–6²à¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒÂòòæ÷FRöfbBF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂƒ“ÂƒC2ÂƒSÂòòG&6²#¢sBBF–6²à¢ƒƒÂƒsÂƒƒÂƒC2ÂƒÂòòæ÷FRöfbBF–6²#Cà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚'Gvò×G&6²Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB&ö¦V7EöWfVçG2ÒfV2°¢ƒCÂÂÂƒRÂÂòòFW7F–æF–öâ6†ææVÂà¢ƒCÂÂÂƒRÂÂòòFW7F–æF–öâ6†ææVÂà¢ƒCÂrÂÂ„CÂÂòòW†—7F–ærV×G’GFW&ârà¢Ó°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÂ„dbÂƒ"Â„Â„$%Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·2‚fÖ–F’ÂrÂe³ÂÒ¢æW‡V7B‚&&÷F‚Ô”D’G&6·26†÷VÆB–×÷'B"’À¢ ¢“°¢ÆWBæ÷FW2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â"“°¢76W'EöW€¢†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’Âæ÷FW5³ÒæÆVæwF‚’À¢ƒÂcÂ“b¢“°¢76W'EöW€¢†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’Âæ÷FW5³ÒæÆVæwF‚’À¢ƒÂcrÂC‚¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂƒ"Â„Â„$%Ð¢“° ¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B‡&V¦V7FVBæ–×÷'EöÖ–F•÷G&6·2‚fÖ–F’ÂrÂe³Â“•Ò’æ—5öW'"‚’“°¢76W'EöW€¢&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢÷&–v–æÂÀ¢&f–ÆVBFW7F–æF–öâÖ–ær×W7BÆVfRWfW'’&ö¦V7B'—FRVæ6†ævVB ¢“°¢Ð ¢5·FW7EÐ¢fâ–×÷'G5ööæÇ•÷6VÆV7FVEöÖ–F•÷G&6·5öæE÷&VÆ–vç5÷v—F†–å÷F†U÷6VÆV7F–öâ‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂòòVç6VÆV7FVBG&6²7F'G2BF–6²¦W&òà¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒƒÂƒsÂƒ“Âƒ4RÂƒcBÂòò6VÆV7FVBG&6²"7F'G2BF–6²#Cà¢ƒƒ2ÂƒcÂƒƒÂƒ4RÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒƒ2ÂƒcÂƒ“ÂƒC2ÂƒcBÂòò6VÆV7FVBG&6²27F'G2BF–6²Cƒà¢ƒƒ2ÂƒcÂƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚'F‡&VR×G&6²Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB&ö¦V7EöWfVçG2ÒfV2°¢ƒCÂÂÂƒRÂÂòòFW7F–æF–öâ6†ææVÂà¢ƒCÂÂÂƒRÂÂòòFW7F–æF–öâ6†ææVÂà¢ƒCÂrÂÂ„CÂÂòòW†—7F–ærV×G’GFW&ârà¢Ó°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÂ„dbÂƒ"Â„Â„$%Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'EöW€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢e³Â%ÒÀ¢e³ÂÒÀ¢Ö–F”–×÷'D÷F–öç2°¢&VÆ–våöWfVçG3¢G'VRÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚'6VÆV7FVBÔ”D’G&6·26†÷VÆB–×÷'B"’À¢ ¢“°¢ÆWBæ÷FW2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â"Â'F†RVç6VÆV7FVBG&6²×W7B&RöÖ—GFVB"“°¢76W'EöW€¢†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’Âæ÷FW5³Òç÷6—F–öâ’À¢ƒÂc"Â¢“°¢76W'EöW€¢†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’Âæ÷FW5³Òç÷6—F–öâ’À¢ƒÂcrÂC‚’À¢'&VÆ–væÖVçB×W7BW6RF†RV&Æ–W7B6VÆV7FVBæ÷FRÂæ÷BâV&Æ–W"Vç6VÆV7FVBG&6² ¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂƒ"Â„Â„$%Ð¢“° ¢f÷"–çfÆ–E÷6VÆV7F–öâ–â²eµÕ²âåÒÂe³ÂÕ²âåÒÂe³5Õ²âåÕÒ°¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢&V¦V7FV@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢–çfÆ–E÷6VÆV7F–öâÀ¢e³²%Õ²âæ–çfÆ–E÷6VÆV7F–öâæÆVâ‚•ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð¢Ð ¢5·FW7EÐ¢fâ6VÆV7FVEöÖ–F•÷G&6·5ö6åö&Uö–×÷'FVEö–çFõööæWu÷GFW&åöFöÖ–6ÆÇ’‚’°¢ÆWBf—'7E÷G&6²Ò°¢ƒÂƒ“Âƒ42ÂƒcBÂƒƒ2ÂƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢Ó°¢ÆWB6V6öæE÷G&6²Ò°¢ƒÂƒ“ÂƒC2ÂƒSÂƒƒÂƒsÂƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢Ó°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&UöæÖVE÷G&6·2€¢e²‚%–æò"Âff—'7E÷G&6²’Â‚$&72"Âg6V6öæE÷G&6²•ÒÀ¢CƒÀ¢’¢æW‡V7B‚'Gvò×G&6²Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB&ö¦V7EöWfVçG2ÒfV2°¢ƒCÂÂÂƒRÂÂƒCÂÂÂƒRÂÂƒCÂrÂÂ„CÂÀ¢Ó°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÂ„dbÂƒ"Â„Â„$%Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB‡GFW&åö–BÂ–×÷'FVB’ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷FõöæWu÷GFW&å÷v—F…ö÷F–öç2€¢fÖ–F’À¢e³ÒÀ¢e³ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æW‡V7B‚'F†R6VÆV7FVBG&6²6†÷VÆB7&VFRæB÷VÆFRGFW&â"“°¢76W'EöW‚‡GFW&åö–BÂ–×÷'FVB’Âƒ‚Â’“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚“°¢76W'EöW‡GFW&ç2æÆVâ‚’Â"“°¢76W'B‡GFW&ç5³Òææ÷FW2æ—5öV×G’‚’“°¢76W'EöW‡GFW&ç5³Òæ–BÂGFW&åö–B“°¢76W'EöW‡GFW&ç5³ÒææÖRæ5öFW&Vb‚’Â6öÖR‚$&72"’“°¢76W'EöW‡GFW&ç5³Òææ÷FW2æÆVâ‚’Â“°¢76W'EöW€¢‡GFW&ç5³Òææ÷FW5³Òæ6†ææVÅö–BÂGFW&ç5³Òææ÷FW5³Òæ¶W’’À¢ƒÂcr¢“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚’æÆ7B‚’çVçw&‚’çv—&Uö'—FW2‚’À¢e³„dbÂƒ"Â„Â„$%Ð¢“°¢ÆWB&W'6VBÒfÇFö7VÖVçC£§'6R‚fFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’’çVçw&‚“°¢76W'EöW‡&W'6VBçGFW&ç2‚’çVçw&‚•³Òææ÷FW2æÆVâ‚’Â“° ¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’çVçw&‚“°¢76W'B€¢&V¦V7FV@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷FõöæWu÷GFW&å÷v—F…ö÷F–öç2€¢fÖ–F’À¢e³%ÒÀ¢e³ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâÆÅöÖ–F•÷G&6·5ö6åö7&VFUöæÖVE÷GFW&ç5öFöÖ–6ÆÇ’‚’°¢ÆWBf—'7E÷G&6²Ò°¢ƒÂƒ“Âƒ42ÂƒcBÂòò3B&Vv–ç2BF–6²à¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒCÂòò3BVæG2BF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢Ó°¢ÆWB6V6öæE÷G&6²Ò°¢ƒÂƒ“ÂƒC2ÂƒSÂòòsB&Vv–ç2BF–6²öâÔ”D’6†ææVÂà¢ƒƒ2ÂƒcÂƒƒÂƒC2Âƒ3ÂòòsBVæG2BF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢Ó°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&UöæÖVE÷G&6·2€¢e²‚%–æò"Âff—'7E÷G&6²’Â‚$&72"Âg6V6öæE÷G&6²•ÒÀ¢CƒÀ¢’¢æW‡V7B‚&æÖVBÔ”D’G&6·26†÷VÆB'6R"“°¢ÆWB&ö¦V7EöWfVçG2Ò°¢ƒCÂÂÂƒRÂÂƒCÂÂÂƒRÂÂƒCÂrÂÂ„CÂÀ¢Ó°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB–×÷'FVE÷G&6·2ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·5÷FõöæWu÷GFW&ç2‚fÖ–F’Â¢æW‡V7B‚&ÆÂÔ”D’G&6·26†÷VÆB–×÷'BFöÖ–6ÆÇ’"“° ¢76W'EöW†–×÷'FVE÷G&6·2Â²ƒ‚Â’Âƒ’Â•Ò“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’æW‡V7B‚'GFW&ç26†÷VÆBFV6öFR"“°¢76W'EöW‡GFW&ç2æÆVâ‚’Â2“°¢76W'EöW‡GFW&ç5³ÒææÖRæ5öFW&Vb‚’Â6öÖR‚%–æò"’“°¢76W'EöW‡GFW&ç5³Òææ÷FW2æÆVâ‚’Â“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³Òæ¶W’Âc“°¢76W'EöW‡GFW&ç5³Òææ÷FW5³Òæ6†ææVÅö–BÂ“°¢76W'EöW‡GFW&ç5³%ÒææÖRæ5öFW&Vb‚’Â6öÖR‚$&72"’“°¢76W'EöW‡GFW&ç5³%Òææ÷FW2æÆVâ‚’Â“°¢76W'EöW‡GFW&ç5³%Òææ÷FW5³Òæ¶W’Âcr“°¢76W'EöW‡GFW&ç5³%Òææ÷FW5³Òæ6†ææVÅö–BÂ“° ¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’çVçw&‚“°¢76W'B€¢&V¦V7FV@¢æ–×÷'EöÖ–F•÷G&6·5÷FõöæWu÷GFW&ç2‚fÖ–F’ÂSc£¤Ô‚¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâÆÅ÷G&6µöÖ–F•ö–×÷'E÷&V¦V7G5ö–æ6ö×ÆWFUöÖ5öæEöf÷&ÖE÷Gvõöf–ÆW2‚’°¢ÆWBöæU÷G&6²ÒÖ–F”f–ÆS£§'6R‚fÖ–F•öf—‡GW&R€¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂƒƒÂƒsÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢’¢æW‡V7B‚'6–ævÆR×G&6²Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWBf÷&ÖE÷GvòÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e²e³ƒÂ„dbÂƒ$bÂƒÒÂe³ƒÂ„dbÂƒ$bÂƒÕÒÀ¢"À¢CƒÀ¢’¢æW‡V7B‚$f÷&ÖBÓ"Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÂÒGFW&åöf—‡GW&R‚eµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢76W'B†Fö7VÖVçBæ–×÷'EöÖ–F•÷G&6·2‚föæU÷G&6²ÂrÂeµÒ’æ—5öW'"‚’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢76W'B€¢Fö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·2‚ff÷&ÖE÷GvòÂrÂe³ÂÒ¢æ—5öW'"‚¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâ–×÷'G5öÖ–F•÷G&6·5÷FõöæÖVE÷6×ÆW%ö6ÆöæW5öFöÖ–6ÆÇ’‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂ„dbÂƒ2ÂƒRÂ"urÂ"v’rÂ"vrÂ"vârÂ"vòrÂƒÂƒ“Âƒ42À¢ƒcBÂòò3BBF–6²à¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒÂòòæ÷FRöfbBF–6²Cƒà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂ„dbÂƒ2ÂƒBÂ"t"rÂ"vrÂ"w2rÂ"w2rÂƒÂƒ“ÂƒC2À¢ƒSÂòòsBBF–6²à¢ƒƒÂƒsÂƒƒÂƒC2ÂƒÂòòæ÷FRöfbBF–6²#Cà¢ƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚&æÖVBf÷&ÖBÓÔ”D’f—‡GW&R6†÷VÆB'6R"“° ¢ÆWBWFcbÒÇfÇVS¢g7G'Â°¢fÇVP¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ó°¢ÆWB×WB&ö¦V7EöWfVçG2ÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„3rÂ"##bããÃ"“°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂÂÂòò6×ÆW"FV×ÆFRÂ6†ææVÂ”Bà¢ƒRÂÂòò¶–æBÓ6×ÆW"à¢ƒÂÂòòVæ&ÆVBà¢ƒCbÂƒ3BÂƒ"Âòò6×ÆW"e‚fÆw2&WF–æVB'’V6‚6ÆöæRà¢Ò“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„3BÂgWFcb‚"÷6×ÆW2÷FV×ÆFRçvb"’“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„4"ÂgWFcb‚%FV×ÆFR"’“°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒc"ÂÂÂòòVæBöb6†ææVÂ&6²FFà¢ƒCÂrÂÂ„CÂÂòòV×G’GFW&ârà¢Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅ÷FV×ÆFRÒFö7VÖVç@¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚'FV×ÆFR6×ÆW"6†÷VÆB&R&V6övæ—¦VB"“°¢ÆWB÷&–v–æÅ÷FV×ÆFUöWfVçG2ÒFö7VÖVçBæWfVçG2‚•¶÷&–v–æÅ÷FV×ÆFRæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢ÆWB†–×÷'FVBÂ6†ææVÅö–G2’ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ2‚fÖ–F’ÂrÂ¢æW‡V7B‚&&÷F‚G&6·26†÷VÆB–×÷'B–çFò6ÆöæVB6×ÆW'2"“° ¢76W'EöW†–×÷'FVBÂ"“°¢76W'EöW†6†ææVÅö–G2æÆVâ‚’Â"“°¢76W'EöæR†6†ææVÅö–G5³ÒÂ6†ææVÅö–G5³Ò“°¢76W'EöW†Fö7VÖVçBæ†VFW"‚’æÆVv7•ö6†ææVÅö6÷VçB‚’Â2“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢ÆWBFV×ÆFRÒ6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚&÷&–v–æÂFV×ÆFR6†÷VÆB&VÖ–â"“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·FV×ÆFRæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æÖ„fÇWfVçC£§v—&Uö'—FW2¢æÖƒÅ·S…Óã£§Fõ÷fV2¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢÷&–v–æÅ÷FV×ÆFUöWfVçG2À¢&6Æöæ–ær×W7Bæ÷BÖöF–g’F†RFV×ÆFR6†ææVÂ'—FW2 ¢“°¢f÷"†6†ææVÅö–BÂW‡V7FVEöæÖR’–â6†ææVÅö–G2æ—FW"‚’ç¦—…²%–æò"Â$&72%Ò’°¢ÆWB6†ææVÂÒ6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¦6†ææVÅö–B¢æW‡V7B‚&æWr6×ÆW"6†ææVÂ6†÷VÆB&R&V6övæ—¦VB"“°¢76W'EöW†6†ææVÂæ¶–æB‚’Â6öÖRƒ’“°¢76W'EöW†6†ææVÂæF—7Æ•öæÖR‚’Â6öÖR†W‡V7FVEöæÖR’“°¢76W'EöW†6†ææVÂç6×ÆU÷F‚‚’Â6öÖR‚"÷6×ÆW2÷FV×ÆFRçvb"’“°¢ÆWB&W6W'fVEöf–VÆG2Ò6†ææVÀ¢æWfVçE÷&ævR‚¢æf–ÇFW"‡Æ–æFW‡ÂÖF6†W2†Fö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFRÂƒCÂ„4"’¢æÖ‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•¶–æFW…Òçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢&W6W'fVEöf–VÆG2À¢÷&–v–æÅ÷FV×ÆFUöWfVçG5³âæ÷&–v–æÅ÷FV×ÆFUöWfVçG2æÆVâ‚’ÒÐ¢“°¢Ð ¢ÆWBæ÷FW2ÒfFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â"“°¢76W'EöW‚†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’’Â†6†ææVÅö–G5³ÒÂc’“°¢76W'EöW‚†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’’Â†6†ææVÅö–G5³ÒÂcr’“° ¢ÆWB×WB6VÆV7FVEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB‡6VÆV7FVEö–×÷'FVBÂ6VÆV7FVEö6†ææVÅö–G2’Ò6VÆV7FVEöFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢À¢e³ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æW‡V7B‚'6VÆV7FVBÔ”D’G&6²6†÷VÆBvWBöæR6ÆöæVB6×ÆW""“°¢76W'EöW‡6VÆV7FVEö–×÷'FVBÂ“°¢76W'EöW‡6VÆV7FVEö6†ææVÅö–G2æÆVâ‚’Â“°¢76W'EöW€¢6VÆV7FVEöFö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ6VÆV7FVEö6†ææVÅö–G5³Ò¢ææE÷F†Vâ„6†ææVÅ7VÖÖ'“£¦F—7Æ•öæÖR’À¢6öÖR‚$&72"¢“°¢76W'EöW€¢6VÆV7FVEöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Òæ¶W’À¢crÀ¢'Vç6VÆV7FVBÔ”D’G&6·2×W7Bæ÷B&öGV6Ræ÷FW2÷"6†ææVÇ2 ¢“° ¢ÆWB×WBæWu÷GFW&åöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB†æWu÷GFW&åö–BÂæWu÷GFW&åö–×÷'FVBÂæWu÷GFW&åö6†ææVÅö–G2’ÒæWu÷GFW&åöFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ5ö–åöæWu÷GFW&â‚fÖ–F’ÂÂe³Ò¢æW‡V7B‚'6VÆV7FVBÔ”D’G&6²6†÷VÆB7&VFRGFW&âæBöæR6ÆöæVB6×ÆW""“°¢76W'EöW‚†æWu÷GFW&åö–BÂæWu÷GFW&åö–×÷'FVB’Âƒ‚Â’“°¢76W'EöW†æWu÷GFW&åö6†ææVÅö–G2æÆVâ‚’Â“°¢76W'EöW†æWu÷GFW&åöFö7VÖVçBçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Òæ¶W’Âcr“°¢76W'EöW€¢æWu÷GFW&åöFö7VÖVç@¢æ6†ææVÇ2‚¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒæWu÷GFW&åö6†ææVÅö–G5³Ò¢ææE÷F†Vâ„6†ææVÅ7VÖÖ'“£¦F—7Æ•öæÖR’À¢6öÖR‚$&72"¢“° ¢ÆWBFV×ÆFUöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&ö¦V7Eö–×÷'BÐ¢fÇFö7VÖVçC£¦æWu÷&ö¦V7Eög&öÕöÖ–F•÷6VÆV7FVE÷G&6·2‚gFV×ÆFUöFö7VÖVçBÂfÖ–F’ÂÂe³Ò¢æW‡V7B‚'6VÆV7FVBÔ”D’G&6²6†÷VÆB7&VFR6W&FR&ö¦V7B"“°¢76W'EöW‡&ö¦V7Eö–×÷'Bæ–×÷'FVEöæ÷FW2Â“°¢76W'EöW‡&ö¦V7Eö–×÷'BæFW7F–æF–öç2æÆVâ‚’Â“°¢76W'EöW‡&ö¦V7Eö–×÷'BæFW7F–æF–öç5³ÒãÂ‚“°¢76W'EöW€¢&ö¦V7Eö–×÷'Bç&ö¦V7BçGFW&ç2‚’çVçw&‚•³Òææ÷FW5³Òæ6†ææVÅö–BÀ¢&ö¦V7Eö–×÷'BæFW7F–æF–öç5³Òã¢“°¢76W'EöW‡FV×ÆFUöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“° ¢ÆWB×WB&V¦V7FVBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢&V¦V7FV@¢æ–×÷'EöÖ–F•÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ2‚fÖ–F’Â“’Â¢æ—5öW'"‚¢“°¢76W'EöW€¢&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’À¢÷&–v–æÂÀ¢&f–ÆVB–×÷'BgFW"6†ææVÂ6Æöæ–ær×W7BÆVfRF†R&ö¦V7BVæ6†ævVB ¢“° ¢ÆWBf÷&ÖE÷GvòÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e²e³ƒÂ„dbÂƒ$bÂƒÒÂe³ƒÂ„dbÂƒ$bÂƒÕÒÀ¢"À¢CƒÀ¢’¢æW‡V7B‚$f÷&ÖBÓ"Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB&V¦V7FVEöf÷&ÖE÷GvòÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢&V¦V7FVEöf÷&ÖE÷Gvð¢æ–×÷'EöÖ–F•÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ2‚ff÷&ÖE÷GvòÂrÂ¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVEöf÷&ÖE÷GvòæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“° ¢ÆWB×WBæF—fU÷FV×ÆFUöWfVçG2Ò&ö¦V7EöWfVçG2æ6ÆöæR‚“°¢ÆWB¶–æEöWfVçBÒæF—fU÷FV×ÆFUöWfVçG0¢çv–æF÷w2ƒ"¢ç÷6—F–öâ‡Çv–æF÷wÂv–æF÷rÓÒ³ƒRÂÒ¢æW‡V7B‚&f—‡GW&R6×ÆW"¶–æBWfVçB6†÷VÆBW†—7B"“°¢æF—fU÷FV×ÆFUöWfVçG5¶¶–æEöWfVçB²ÒÒ#°¢ÆWBæöå÷6×ÆW"ÒfÇöf—‡GW&R‚fæF—fU÷FV×ÆFUöWfVçG2ÂeµÒÂeµÒ“°¢ÆWB×WB&V¦V7FVEöæöå÷6×ÆW"Ð¢fÇFö7VÖVçC£§'6R‚fæöå÷6×ÆW"’æW‡V7B‚&æöâÕ6×ÆW"f—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢&V¦V7FVEöæöå÷6×ÆW ¢æ–×÷'EöÖ–F•÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ2‚fÖ–F’ÂrÂ¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVEöæöå÷6×ÆW"æVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Âæöå÷6×ÆW"“°¢Ð ¢5·FW7EÐ¢fâÖ–F•÷G&6µö7&VF–öåö6ÆöæW5öæF—fU÷FV×ÆFU÷7FFUöf÷%÷6VÆV7FVE÷G&6·2‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂ„dbÂƒ2ÂƒRÂ"urÂ"v’rÂ"vrÂ"vârÂ"vòrÂƒÂƒ“Âƒ42ÂƒcBÀ¢ƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂ„dbÂƒ2ÂƒBÂ"t"rÂ"vrÂ"w2rÂ"w2rÂƒÂƒ“ÂƒC2ÂƒSÂƒcÀ¢ƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚&æÖVBf÷&ÖBÓÔ”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWBWFcbÒÇfÇVS¢g7G'Â°¢fÇVP¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚¢Ó°¢ÆWB×WB&ö¦V7EöWfVçG2ÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„3rÂ"##bããÃ"“°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂ"ÂƒÂÒ“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„CBÂe³„Â„"Â„5Ò“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„CRÂe³„#Â„3"Â„C2Â„SBÂ„cUÒ“°¢VæEöFFöWfVçB‚f×WB&ö¦V7EöWfVçG2Â„4"ÂgWFcb‚$æF—fRFV×ÆFR"’“°¢&ö¦V7EöWfVçG2æW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÂƒCÂrÂÂ„CÂÒ“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g&ö¦V7EöWfVçG2ÂeµÒÂeµÒ“°¢ÆWBFV×ÆFUöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚$æF—fRFV×ÆFR&ö¦V7B6†÷VÆB'6R"“°¢ÆWB6÷W&6Uö6†ææVÂÒFV×ÆFUöFö7VÖVç@¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚$æF—fRFV×ÆFR6†ææVÂ6†÷VÆB&R&V6övæ—¦VB"“°¢76W'EöW‡6÷W&6Uö6†ææVÂæ¶–æB‚’Â6öÖRƒ"’“°¢ÆWB6÷W&6U÷7FFRÒFV×ÆFUöFö7VÖVçBæWfVçG2‚•·6÷W&6Uö6†ææVÂæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFRÂ„CBÂ„CR’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWB×WBF—&V7EöGWÆ–6FRÒFV×ÆFUöFö7VÖVçBæ6ÆöæR‚“°¢ÆWBF—&V7EöGWÆ–6FUö–BÒF—&V7EöGWÆ–6FP¢æGWÆ–6FUö6†ææVÂƒÂ$æF—fR6÷’"¢æW‡V7B‚&æF—fR–ç7G'VÖVçB6†ææVÂ6†÷VÆBGWÆ–6FRF—&V7FÇ’"“°¢ÆWBF—&V7EöGWÆ–6FUö6†ææVÂÒF—&V7EöGWÆ–6FP¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒF—&V7EöGWÆ–6FUö–B¢æW‡V7B‚&F—&V7FÇ’GWÆ–6FVBæF—fR6†ææVÂ6†÷VÆBW†—7B"“°¢76W'EöW†F—&V7EöGWÆ–6FUö6†ææVÂæ¶–æB‚’Â6öÖRƒ"’“°¢76W'EöW†F—&V7EöGWÆ–6FUö6†ææVÂæF—7Æ•öæÖR‚’Â6öÖR‚$æF—fR6÷’"’“°¢76W'EöW€¢F—&V7EöGWÆ–6FRæWfVçG2‚•¶F—&V7EöGWÆ–6FUö6†ææVÂæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFRÂ„CBÂ„CR’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢6÷W&6U÷7FFRÀ¢&F—&V7B6†ææVÂGWÆ–6F–öâ×W7B&W6W'fRÇVrÖ–âw&W"æB7FFR'—FW2 ¢“° ¢ÆWB×WBFö7VÖVçBÒFV×ÆFUöFö7VÖVçBæ6ÆöæR‚“°¢ÆWB†–×÷'FVBÂ6†ææVÅö–G2’ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVE÷G&6·5÷Fõ÷FV×ÆFUö6†ææVÇ5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢À¢e³ÂÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æW‡V7B‚&&÷F‚6VÆV7FVBG&6·26†÷VÆB6ÆöæRF†RæF—fR6†ææVÂ"“°¢76W'EöW†–×÷'FVBÂ"“°¢76W'EöW†6†ææVÅö–G2æÆVâ‚’Â"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢f÷"†6†ææVÅö–BÂW‡V7FVEöæÖR’–â6†ææVÅö–G2æ—FW"‚’ç¦—…²%–æò"Â$&72%Ò’°¢ÆWB6†ææVÂÒ6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¦6†ææVÅö–B¢æW‡V7B‚&æWræF—fR6†ææVÂ6†÷VÆB&R&V6övæ—¦VB"“°¢76W'EöW†6†ææVÂæ¶–æB‚’Â6öÖRƒ"’“°¢76W'EöW†6†ææVÂæF—7Æ•öæÖR‚’Â6öÖR†W‡V7FVEöæÖR’“°¢ÆWB7FFRÒFö7VÖVçBæWfVçG2‚•¶6†ææVÂæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFRÂ„CBÂ„CR’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW€¢7FFRÂ6÷W&6U÷7FFRÀ¢&÷VRÇVrÖ–â7FFR×W7B&R&W6W'fVB ¢“°¢Ð¢ÆWBæ÷FW2ÒFö7VÖVç@¢çGFW&ç2‚¢çVçw&‚¢æ–çFõö—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒr¢æW‡V7B‚&–×÷'BF&vWBGFW&â6†÷VÆB&VÖ–â&W6VçB"¢ææ÷FW3°¢76W'EöW†æ÷FW2æÆVâ‚’Â"“°¢76W'EöW‚†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’’Â†6†ææVÅö–G5³ÒÂc’“°¢76W'EöW‚†æ÷FW5³Òæ6†ææVÅö–BÂæ÷FW5³Òæ¶W’’Â†6†ææVÅö–G5³ÒÂcr’“° ¢ÆWBæWu÷&ö¦V7BÐ¢fÇFö7VÖVçC£¦æWu÷&ö¦V7Eög&öÕöÖ–F•÷6VÆV7FVE÷G&6·2‚gFV×ÆFUöFö7VÖVçBÂfÖ–F’ÂÂe³Ò¢æW‡V7B‚$æF—fRFV×ÆFR6†÷VÆB7W÷'B6W&FR×&ö¦V7B–×÷'B"“°¢ÆWB&ö¦V7Eö6†ææVÅö–BÒæWu÷&ö¦V7BæFW7F–æF–öç5³Òã°¢ÆWB&ö¦V7Eö6†ææVÂÒæWu÷&ö¦V7@¢ç&ö¦V7@¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ&ö¦V7Eö6†ææVÅö–B¢æW‡V7B‚&æWr×&ö¦V7BæF—fR6†ææVÂ6†÷VÆBW†—7B"“°¢76W'EöW‡&ö¦V7Eö6†ææVÂæ¶–æB‚’Â6öÖRƒ"’“°¢76W'EöW†æWu÷&ö¦V7Bæ–×÷'FVEöæ÷FW2Â“°¢76W'EöW€¢æWu÷&ö¦V7Bç&ö¦V7BæWfVçG2‚•·&ö¦V7Eö6†ææVÂæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFRÂ„CBÂ„CR’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢6÷W&6U÷7FFP¢“°¢76W'EöW‡FV×ÆFUöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“° ¢ÆWBf÷&ÖE÷GvòÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂƒ“ÂƒC2ÂƒSÂƒcÂƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢"À¢CƒÀ¢’¢æW‡V7B‚$f÷&ÖBÓ"Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBf÷&ÖE÷GvõöFö7VÖVçBÒFV×ÆFUöFö7VÖVçBæ6ÆöæR‚“°¢ÆWB†f÷&ÖE÷Gvõö–×÷'FVBÂFW7F–æF–öç2’Òf÷&ÖE÷GvõöFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVEöf÷&ÖC%÷G&6·5÷Fõ÷FV×ÆFU÷GFW&ç5÷v—F…ö÷F–öç2€¢ff÷&ÖE÷GvòÀ¢À¢e³ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æW‡V7B‚'6VÆV7FVBf÷&ÖBÓ"6WVVæ6R6†÷VÆB6ÆöæRF†RæF—fRFV×ÆFR"“°¢76W'EöW†f÷&ÖE÷Gvõö–×÷'FVBÂ“°¢76W'EöW†FW7F–æF–öç2æÆVâ‚’Â“°¢ÆWBf÷&ÖE÷Gvõö6†ææVÂÒf÷&ÖE÷GvõöFö7VÖVç@¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒFW7F–æF–öç5³Òã¢æW‡V7B‚$f÷&ÖBÓ"æF—fR6†ææVÂ6†÷VÆBW†—7B"“°¢76W'EöW†f÷&ÖE÷Gvõö6†ææVÂæ¶–æB‚’Â6öÖRƒ"’“°¢76W'EöW€¢f÷&ÖE÷GvõöFö7VÖVçBæWfVçG2‚•¶f÷&ÖE÷Gvõö6†ææVÂæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂÖF6†W2†WfVçBæ÷6öFRÂ„CBÂ„CR’¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢6÷W&6U÷7FFP¢“°¢Ð ¢5·FW7EÐ¢fâGWÆ–6FUöWFöÖF–öåö6†ææVÅ÷&W6W'fW5ö7W'fUö'—FW5öæEö76–vç5öæWuö–B‚’°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒRÂUÓ°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„4"ÂgWFce÷&ö¦V7E÷7G&–ær‚$WFöÖF–öâÖ–â"’“°¢ÆWB×WB7W'fRÒfV2³„S²dÅôUDôÔD”ôåõô”åE5ôôde4UEÓ°¢7W'fU´dÅôUDôÔD”ôåô4õTåEôôde4UBâädÅôUDôÔD”ôåõô”åE5ôôde4UEÐ¢æ6÷•ög&öÕ÷6Æ–6R‚c÷S3"çFõöÆUö'—FW2‚’“°¢7W'fRæW‡FVæEög&öÕ÷6Æ–6R‚c"ãUöccBçFõöÆUö'—FW2‚’“°¢7W'fRæW‡FVæEög&öÕ÷6Æ–6R‚cãsUöccBçFõöÆUö'—FW2‚’“°¢7W'fRæW‡FVæEög&öÕ÷6Æ–6R‚b‚Óã#Uöc3"’çFõöÆUö'—FW2‚’“°¢7W'fRæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒ“Âƒƒ"Âƒs2ÂƒcEÒ“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„TÂf7W'fR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚$WFöÖF–öâ6†ææVÂf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6÷W&6RÒFö7VÖVç@¢æ6†ææVÇ2‚¢æ–çFõö—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚'6÷W&6RWFöÖF–öâ6†ææVÂ6†÷VÆBW†—7B"“°¢ÆWB6÷W&6Uö7W'fRÒFö7VÖVçBæWfVçG2‚•·6÷W&6RæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFRÓÒ„T¢æW‡V7B‚'6÷W&6Rö–çB&Æö"6†÷VÆBW†—7B"¢çv—&Uö'—FW2‚¢çFõ÷fV2‚“° ¢ÆWBGWÆ–6FUö–BÒFö7VÖVç@¢æGWÆ–6FUö6†ææVÂƒÂ$WFöÖF–öâ6÷’"¢æW‡V7B‚$WFöÖF–öâ6†ææVÇ26†÷VÆBGWÆ–6FRF‡&÷Vv‚F†R6†&VB6öÖÖæB"“°¢76W'EöW†GWÆ–6FUö–BÂ"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢ÆWBGWÆ–6FRÒ6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒGWÆ–6FUö–B¢æW‡V7B‚&GWÆ–6FVBWFöÖF–öâ6†ææVÂ6†÷VÆBW†—7B"“°¢76W'EöW†GWÆ–6FRæ¶–æB‚’Â6öÖRƒR’“°¢76W'EöW†GWÆ–6FRæF—7Æ•öæÖR‚’Â6öÖR‚$WFöÖF–öâ6÷’"’“°¢ÆWBGWÆ–6FUö7W'fRÒFö7VÖVçBæWfVçG2‚•¶GWÆ–6FRæWfVçE÷&ævR‚•Ð¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFRÓÒ„T¢æW‡V7B‚&GWÆ–6FVBö–çB&Æö"6†÷VÆBW†—7B"¢çv—&Uö'—FW2‚“°¢76W'EöW†GWÆ–6FUö7W'fRÂ6÷W&6Uö7W'fR“°¢ÆWB6÷W&6UögFW"Ò6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚'6÷W&6RWFöÖF–öâ6†ææVÂ6†÷VÆB&VÖ–â"¢æWfVçE÷&ævR‚“°¢76W'EöW€¢Fö7VÖVçBæWfVçG2‚•·6÷W&6UögFW%Ð¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFRÓÒ„T¢æW‡V7B‚'6÷W&6Rö–çB&Æö"6†÷VÆB&VÖ–â"¢çv—&Uö'—FW2‚’À¢6÷W&6Uö7W'fRÀ¢&GWÆ–6F–ærF†R6†ææVÂ×W7BÆVfRF†R6÷W&6R7W'fRVæ6†ævVB ¢“° ¢ÆWB&÷VæE÷G&—VBÒfÇFö7VÖVçC£§'6R€¢fFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&GWÆ–6FVBWFöÖF–öâ6†ææVÂ6†÷VÆBVæ6öFR"’À¢¢æW‡V7B‚&GWÆ–6FVBWFöÖF–öâ&ö¦V7B6†÷VÆB'6R"“°¢ÆWBWFöÖF–öâÒ&÷VæE÷G&—V@¢æWFöÖF–öåö6†ææVÇ2‚¢æW‡V7B‚&&÷F‚WFöÖF–öâ6†ææVÇ26†÷VÆBFV6öFR"“°¢76W'EöW†WFöÖF–öâæÆVâ‚’Â"“°¢76W'EöW†WFöÖF–öå³Òçö–çG2‚’ÂWFöÖF–öå³Òçö–çG2‚’“°¢Ð ¢5·FW7EÐ¢fâÖ–F•÷G&6µ÷6×ÆW%ö7&VF–öåö6å÷&÷WFU÷Fõöf—'7Eög&VUöÖ—†W%ö–ç6W'G2‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂòòf—'7BG&6³¢3Bà¢ƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂƒ“ÂƒC2ÂƒSÂòò6V6öæBG&6³¢sBà¢ƒcÂƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢À¢CƒÀ¢’¢æW‡V7B‚'Gvò×G&6²Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB7G&VÒÒfV2°¢ƒCÂÂÂòò6×ÆW"FV×ÆFRÂ”Bà¢ƒRÂÂòò¶–æBÓ6×ÆW"à¢ƒbÂÂòòFV×ÆFRÇ&VG’&÷WFW2Fò–ç6W'Bà¢ƒÂÀ¢Ó°¢ÆWB6×ÆU÷F‚Ò"÷6×ÆW2÷FV×ÆFRçvb ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3BÂg6×ÆU÷F‚“°¢ÆWBFV×ÆFUöæÖRÒ%FV×ÆFR ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„4"ÂgFV×ÆFUöæÖR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e°¢ƒCÂÂÂòòW†—7F–ær6†ææVÂÂ”Bà¢ƒRÂ"ÂòòæF—fR–ç7G'VÖVçB6†ææVÂà¢ƒbÂ2Âòò–ç6W'B2—2Ç6òÇ&VG’76–væVBà¢ƒc"ÂÂÂòòVæBöb6†ææVÂ&6²FFà¢ƒCÂrÂÂ„CÂÂòòV×G’GFW&ârà¢Ò“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“° ¢ÆWB†–×÷'FVBÂ6†ææVÅö–G2’ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6·5÷Fõ÷6×ÆW%ö6†ææVÇ5÷v—F…ö÷F–öç2€¢fÖ–F’À¢rÀ¢À¢Ö–F”–×÷'D÷F–öç2°¢6WEöÖ—†W%÷G&6·5öf÷%öæWuö6†ææVÇ3¢G'VRÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚&æWr6×ÆW'26†÷VÆB&÷WFRFòF†Rf—'7Bg&VR–ç6W'G2"“° ¢76W'EöW†–×÷'FVBÂ"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢ÆWB76–væÖVçG2Ò6†ææVÅö–G0¢æ—FW"‚¢æÖ‡Æ6†ææVÅö–GÂ°¢6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¦6†ææVÅö–B¢æW‡V7B‚&7&VFVB6×ÆW"6†÷VÆBW†—7B"¢æÖ—†W%÷G&6²‚¢æW‡V7B‚&7&VFVB6×ÆW"6†÷VÆB†fRÖ—†W"76–væÖVçB"¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW†76–væÖVçG2Â³"ÂEÒ“°¢76W'EöW€¢6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚'FV×ÆFR6†÷VÆB&VÖ–â"¢æÖ—†W%÷G&6²‚’À¢6öÖRƒ¢“°¢76W'EöW€¢6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¢æW‡V7B‚&W†—7F–ær6†ææVÂ6†÷VÆB&VÖ–â"¢æÖ—†W%÷G&6²‚’À¢6öÖRƒ2¢“°¢Ð ¢5·FW7EÐ¢fâ–×÷'G5öf÷&ÖE÷GvõöÖ–F•÷G&6·5ö–çFõ÷6W&FU÷6×ÆW%÷GFW&ç2‚’°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•÷G&6·5öf—‡GW&R€¢e°¢e°¢ƒÂ„dbÂƒ2ÂƒRÂ"urÂ"v’rÂ"vrÂ"vârÂ"vòrÂƒÂƒ“Âƒ42ÂƒcBÀ¢ƒƒ2ÂƒcÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢e°¢ƒÂ„dbÂƒ2ÂƒBÂ"t"rÂ"vrÂ"w2rÂ"w2rÂƒÂƒ“ÂƒC2ÂƒSÂƒƒÀ¢ƒsÂƒƒÂƒC2ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢ÒÀ¢"À¢CƒÀ¢’¢æW‡V7B‚$f÷&ÖBÓ"Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WB7G&VÒÒfV3£¦æWr‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3rÂ"##bããÃ"“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒRÂÂƒÂÒ“°¢ÆWB6×ÆU÷F‚Ò"÷6×ÆW2÷FV×ÆFRçvb ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„3BÂg6×ÆU÷F‚“°¢ÆWBFV×ÆFUöæÖRÒ%FV×ÆFR ¢æVæ6öFU÷WFcb‚¢æfÆEöÖ‡Sc£§FõöÆUö'—FW2¢æ6†–â…³ÂÒ¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢VæEöFFöWfVçB‚f×WB7G&VÒÂ„4"ÂgFV×ÆFUöæÖR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÂƒCÂrÂÂ„CÂÒ“°¢ÆWB÷&–v–æÂÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB†–×÷'FVBÂFW7F–æF–öç2’ÒFö7VÖVç@¢æ–×÷'EöÖ–F•öf÷&ÖC%÷G&6·5÷Fõ÷6×ÆW%÷GFW&ç2‚fÖ–F’Â¢æW‡V7B‚&V6‚–æFWVæFVçB6WVVæ6R6†÷VÆB–×÷'B–çFò—G2÷vâGFW&â"“° ¢76W'EöW†–×÷'FVBÂ"“°¢76W'EöW†FW7F–æF–öç2æÆVâ‚’Â"“°¢76W'EöæR†FW7F–æF–öç5³ÒãÂFW7F–æF–öç5³Òã“°¢76W'EöæR†FW7F–æF–öç5³ÒãÂFW7F–æF–öç5³Òã“°¢ÆWBGFW&ç2ÒFö7VÖVçBçGFW&ç2‚’çVçw&‚“°¢ÆWBf—'7E÷GFW&âÒGFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒFW7F–æF–öç5³Òã¢æW‡V7B‚&f—'7B6WVVæ6RGFW&â6†÷VÆBW†—7B"“°¢ÆWB6V6öæE÷GFW&âÒGFW&ç0¢æ—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒFW7F–æF–öç5³Òã¢æW‡V7B‚'6V6öæB6WVVæ6RGFW&â6†÷VÆBW†—7B"“°¢76W'EöW†f—'7E÷GFW&âææÖRæ5öFW&Vb‚’Â6öÖR‚%–æò"’“°¢76W'EöW‡6V6öæE÷GFW&âææÖRæ5öFW&Vb‚’Â6öÖR‚$&72"’“°¢76W'EöW†f—'7E÷GFW&âææ÷FW2æÆVâ‚’Â“°¢76W'EöW€¢€¢f—'7E÷GFW&âææ÷FW5³Òæ6†ææVÅö–BÀ¢f—'7E÷GFW&âææ÷FW5³Òæ¶W¢’À¢†FW7F–æF–öç5³ÒãÂc¢“°¢76W'EöW‡6V6öæE÷GFW&âææ÷FW2æÆVâ‚’Â“°¢76W'EöW€¢€¢6V6öæE÷GFW&âææ÷FW5³Òæ6†ææVÅö–BÀ¢6V6öæE÷GFW&âææ÷FW5³Òæ¶W¢’À¢†FW7F–æF–öç5³ÒãÂcr¢“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢f÷"‚…òÂ6†ææVÅö–B’ÂW‡V7FVEöæÖR’–âFW7F–æF–öç2æ—FW"‚’ç¦—…²%–æò"Â$&72%Ò’°¢ÆWB6†ææVÂÒ6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¦6†ææVÅö–B¢æW‡V7B‚'6WVVæ6R6×ÆW"6†÷VÆBW†—7B"“°¢76W'EöW†6†ææVÂæF—7Æ•öæÖR‚’Â6öÖR†W‡V7FVEöæÖR’“°¢76W'EöW†6†ææVÂç6×ÆU÷F‚‚’Â6öÖR‚"÷6×ÆW2÷FV×ÆFRçvb"’“°¢Ð ¢ÆWB×WB6VÆV7FVEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7B6†÷VÆB'6Rf÷"6VÆV7FVB–×÷'B"“°¢ÆWB‡6VÆV7FVEö–×÷'FVBÂ6VÆV7FVEöFW7F–æF–öç2’Ò6VÆV7FVEöFö7VÖVç@¢æ–×÷'EöÖ–F•÷6VÆV7FVEöf÷&ÖC%÷G&6·5÷Fõ÷6×ÆW%÷GFW&ç5÷v—F…ö÷F–öç2€¢fÖ–F’À¢À¢e³ÒÀ¢Ö–F”–×÷'D÷F–öç3£¦FVfVÇB‚’À¢¢æW‡V7B‚'F†R6VÆV7FVB–æFWVæFVçB6WVVæ6R6†÷VÆB–×÷'B"“°¢76W'EöW‡6VÆV7FVEö–×÷'FVBÂ“°¢76W'EöW‡6VÆV7FVEöFW7F–æF–öç2æÆVâ‚’Â“°¢ÆWB6VÆV7FVE÷GFW&âÒ6VÆV7FVEöFö7VÖVç@¢çGFW&ç2‚¢çVçw&‚¢æ–çFõö—FW"‚¢æf–æB‡ÇGFW&çÂGFW&âæ–BÓÒ6VÆV7FVEöFW7F–æF–öç5³Òã¢æW‡V7B‚&öæÇ’F†R6VÆV7FVB6WVVæ6RGFW&â6†÷VÆB&RFFVB"“°¢76W'EöW‡6VÆV7FVE÷GFW&âææÖRæ5öFW&Vb‚’Â6öÖR‚$&72"’“°¢76W'EöW‡6VÆV7FVE÷GFW&âææ÷FW2æÆVâ‚’Â“°¢76W'EöW‡6VÆV7FVE÷GFW&âææ÷FW5³Òæ¶W’Âcr“°¢76W'EöW‡6VÆV7FVEöFö7VÖVçBæ6†ææVÇ2‚’æÆVâ‚’Â"“° ¢ÆWBFV×ÆFUöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7B6†÷VÆB'6Rf÷"æWr×&ö¦V7B–×÷'B"“°¢ÆWB&ö¦V7Eö–×÷'BÒfÇFö7VÖVçC£¦æWu÷&ö¦V7Eög&öÕöÖ–F•÷6VÆV7FVE÷G&6·2€¢gFV×ÆFUöFö7VÖVçBÀ¢fÖ–F’À¢À¢e³ÂÒÀ¢¢æW‡V7B‚$f÷&ÖBÓ"6WVVæ6W26†÷VÆB–×÷'B–çFò6W&FR&ö¦V7B"“°¢76W'EöW‡&ö¦V7Eö–×÷'Bæ–×÷'FVEöæ÷FW2Â"“°¢76W'EöW‡&ö¦V7Eö–×÷'BæFW7F–æF–öç2æÆVâ‚’Â"“°¢76W'EöW€¢&ö¦V7Eö–×÷'Bç&ö¦V7BçGFW&ç2‚’çVçw&‚’æÆVâ‚’À¢FV×ÆFUöFö7VÖVçBçGFW&ç2‚’çVçw&‚’æÆVâ‚’² ¢“°¢76W'EöW‡FV×ÆFUöFö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“° ¢ÆWB×WB&÷WFVEöFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚'&ö¦V7B6†÷VÆB'6Rf÷"&÷WFVB–×÷'B"“°¢ÆWB…òÂ&÷WFVEöFW7F–æF–öç2’Ò&÷WFVEöFö7VÖVç@¢æ–×÷'EöÖ–F•öf÷&ÖC%÷G&6·5÷Fõ÷6×ÆW%÷GFW&ç5÷v—F…ö÷F–öç2€¢fÖ–F’À¢À¢Ö–F”–×÷'D÷F–öç2°¢6WEöÖ—†W%÷G&6·5öf÷%öæWuö6†ææVÇ3¢G'VRÀ¢âäÖ–F”–×÷'D÷F–öç3£¦FVfVÇB‚¢ÒÀ¢¢æW‡V7B‚$f÷&ÖBÓ"6×ÆW'26†÷VÆB&÷WFRFòg&VR–ç6W'G2"“°¢ÆWB&÷WFVEö6†ææVÇ2Ò&÷WFVEöFö7VÖVçBæ6†ææVÇ2‚“°¢ÆWB&÷WFVE÷G&6·2Ò&÷WFVEöFW7F–æF–öç0¢æ—FW"‚¢æÖ‡Â…òÂ6†ææVÅö–B—Â°¢&÷WFVEö6†ææVÇ0¢æ—FW"‚¢æf–æB‡Æ6†ææVÇÂ6†ææVÂæ–B‚’ÓÒ¦6†ææVÅö–B¢æW‡V7B‚'&÷WFVBf÷&ÖBÓ"6×ÆW"6†÷VÆBW†—7B"¢æÖ—†W%÷G&6²‚¢æW‡V7B‚'&÷WFVBf÷&ÖBÓ"6×ÆW"6†÷VÆB†fRâ76–væÖVçB"¢Ò¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢76W'EöW‡&÷WFVE÷G&6·2Â³Â%Ò“° ¢ÆWB×WBæöå÷6×ÆW%÷7G&VÒÒ7G&VÓ°¢ÆWB¶–æEöWfVçBÒæöå÷6×ÆW%÷7G&VÐ¢çv–æF÷w2ƒ"¢ç÷6—F–öâ‡Çv–æF÷wÂv–æF÷rÓÒ³ƒRÂÒ¢æW‡V7B‚&f—‡GW&R6×ÆW"¶–æBWfVçB6†÷VÆBW†—7B"“°¢æöå÷6×ÆW%÷7G&VÕ¶¶–æEöWfVçB²ÒÒ#°¢ÆWBæöå÷6×ÆW"ÒfÇöf—‡GW&R‚fæöå÷6×ÆW%÷7G&VÒÂeµÒÂeµÒ“°¢ÆWB×WB&V¦V7FVBÐ¢fÇFö7VÖVçC£§'6R‚fæöå÷6×ÆW"’æW‡V7B‚&æöâÕ6×ÆW"f—‡GW&R6†÷VÆB'6R"“°¢76W'B€¢&V¦V7FV@¢æ–×÷'EöÖ–F•öf÷&ÖC%÷G&6·5÷Fõ÷6×ÆW%÷GFW&ç2‚fÖ–F’Â¢æ—5öW'"‚¢“°¢76W'EöW‡&V¦V7FVBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Âæöå÷6×ÆW"“°¢Ð ¢5·FW7EÐ¢fâW‡÷'G5÷GFW&åöæ÷FW5÷v—F…÷&ö¦V7E÷öæE÷7F÷&VEöÖ–F•ö6†ææVÇ2‚’°¢ÆWB×WBæ÷FRÒæ÷FU÷&V6÷&Bƒ“bÂ2ÂC‚ÂcÂ“b“°¢æ÷FU³…ÒÒ#ƒ°¢æ÷FU³•ÒÒ“°¢æ÷FU³#ÒÒ°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚gGFW&åöf—‡GW&R‚e¶æ÷FUÒÂeµÒ’¢æW‡V7B‚'&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB6÷W&6U÷FV×òÒFö7VÖVçBæÖWFFF‚’çFV×õö'Ò‚’çVçw&ö÷"ƒCã“° ¢ÆWB'—FW2ÒÖ–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷GFW&â€¢fFö7VÖVçBÀ¢rÀ¢Ö–F”6†ææVÄÖ–æs£¥&W6W'fTæ÷FT6†ææVÇ2À¢¢æW‡V7B‚'GFW&â6†÷VÆBW‡÷'B"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBf–ÆR6†÷VÆB'6R"“°¢ÆWBG&6²ÒÖ–F¢çG&6·2‚¢æ—FW"‚¢æf–æB‡ÇG&6·ÂG&6²ææ÷FW2‚’æ—5öV×G’‚’¢æW‡V7B‚&W‡÷'B6†÷VÆB6öçF–âF†RGFW&âæ÷FRG&6²"“°¢ÆWBW‡÷'FVEöæ÷FRÒG&6²ææ÷FW2‚’ç&VÖ÷fRƒ“° ¢76W'EöW†Ö–F’æF—f—6–öâ‚’Â“b“°¢76W'B€¢†Ö–F’çG&6·2‚•³Ð¢çFV×õö'Ò‚¢æW‡V7B‚'FV×òWfVçB6†÷VÆBW‡÷'B"¢Ò6÷W&6U÷FV×ò¢æ'2‚¢Âã¢“°¢76W'EöW†W‡÷'FVEöæ÷FRæ6†ææVÂ‚’Â’“°¢76W'EöW†W‡÷'FVEöæ÷FRæ¶W’‚’Âc“°¢76W'EöW†W‡÷'FVEöæ÷FRçfVÆö6—G’‚’Â“b“°¢76W'EöW†W‡÷'FVEöæ÷FRç&VÆV6U÷fVÆö6—G’‚’Â6öÖRƒ#r’“°¢76W'EöW†W‡÷'FVEöæ÷FRç7F'E÷F–6²‚’Â“b“°¢76W'EöW†W‡÷'FVEöæ÷FRæVæE÷F–6²‚’Â6öÖRƒCB’“°¢76W'B‡G&6²æWfVçG2‚’æ—FW"‚’æç’‡ÆWfVçGÂ°¢WfVçBæ'6öÇWFU÷F–6²‚’ÓÒ“`¢bbÖF6†W2†WfVçBæ¶–æB‚’ÂÖ–F”WfVçD¶–æC£¤6†ææVÅfö–6R²7FGW2ÂFFÐ¢–b§7FGW2ÓÒ„#’bbFFÓÒe³ÂÒ¢Ò’“°¢Ð ¢5·FW7EÐ¢fâW‡÷'G5÷6öæu÷GFW&åö6Æ—5÷v—F…÷&WVG5öæEö6Æ—ö&÷VæF'•÷G'Væ6F–öâ‚’°¢ÆWB×WBæ÷FRÒæ÷FU÷&V6÷&BƒÂ2ÂC‚ÂcÂ“°¢æ÷FU³…ÒÒ3#°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂ#EÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÒ“°¢ÆWB×WB6Æ—Ò³Sƒ²ƒÓ°¢6Æ—²âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cwSbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&'&ævVÖVçBf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB'—FW2Ð¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öær‚fFö7VÖVçBÂÂÖ–F”6†ææVÄÖ–æs£¥&W6W'fTæ÷FT6†ææVÇ2¢æW‡V7B‚&'&ævVÖVçB6†÷VÆBW‡÷'B"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBf–ÆR6†÷VÆB'6R"“°¢ÆWBæ÷FW2ÒÖ–F¢çG&6·2‚¢æ—FW"‚¢æfÆEöÖ‡ÇG&6·ÂG&6²ææ÷FW2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢76W'EöW†æ÷FW2æÆVâ‚’Â2“°¢76W'EöW†æ÷FW5³Òç7F'E÷F–6²‚’Â“°¢76W'EöW†æ÷FW5³ÒæVæE÷F–6²‚’Â6öÖRƒC‚’“°¢76W'EöW†æ÷FW5³Òç7F'E÷F–6²‚’ÂC‚“°¢76W'EöW†æ÷FW5³ÒæVæE÷F–6²‚’Â6öÖRƒ“b’“°¢76W'EöW†æ÷FW5³%Òç7F'E÷F–6²‚’Â“b“°¢76W'EöW†æ÷FW5³%ÒæVæE÷F–6²‚’Â6öÖRƒ#’“°¢76W'B†æ÷FW2æ—FW"‚’æÆÂ‡Ææ÷FWÂæ÷FRç&VÆV6U÷fVÆö6—G’‚’ÓÒ6öÖRƒ3"’’“°¢Ð ¢5·FW7EÐ¢fâW‡÷'G5÷6VÆV7FVE÷&uö6öçG&öÆÆW%÷7G&VÕö7&÷75÷&WVFVE÷6öæuö6Æ—2‚’°¢ÆWB×WBæ÷FRÒæ÷FU÷&V6÷&BƒÂ2ÂC‚ÂcÂ“°¢æ÷FU³…ÒÒ3#°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂ#EÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢f÷"&V6÷&B–â°¢GFW&åö6öçG&öÆÆW%÷&V6÷&BƒÂ³²%ÒÂ2Âƒ“Âãc3"çFõö&—G2‚’’À¢GFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ#BÂ³²%ÒÂ2Âƒ“ÂãVc3"çFõö&—G2‚’’À¢GFW&åö6öçG&öÆÆW%÷&V6÷&Bƒ"Â³²%ÒÂ’ÂƒÂãc3"çFõö&—G2‚’’À¢Ò°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„DbÂg&V6÷&B“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÒ“° ¢ÆWB×WB6Æ—2ÒfV3£¦æWr‚“°¢f÷"‡÷6—F–öâÂÆVæwF‚’–â²ƒS3"ÂS3"’Âƒ3ÂC•Ò°¢ÆWB×WB6Æ—Ò³Sƒ²ƒÓ°¢6Æ—²âãEÒæ6÷•ög&öÕ÷6Æ–6R‚g÷6—F–öâçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cwSbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚fÆVæwF‚çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢6Æ—2æW‡FVæEög&öÕ÷6Æ–6R‚f6Æ—“°¢Ð¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„3rÂ"##RããÃ"“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—2“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&'&ævVÖVçBf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB'—FW2ÒÖ–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öæuö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂÂƒ2Âƒ“’ÂsBÂ"¢æW‡V7B‚'F†R6VÆV7FVB&r7G&VÒ6†÷VÆBW‡÷'B7&÷72F†R'&ævVÖVçB"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBÔ”D’6†÷VÆB'6R"“°¢76W'EöW†Ö–F’æF—f—6–öâ‚’Â“b“°¢76W'EöW†Ö–F’çG&6·2‚’æÆVâ‚’Â2“°¢ÆWB6öçG&öÆÆW%÷G&6²ÒÖ–F’çG&6·2‚’æÆ7B‚’çVçw&‚“°¢76W'EöW†6öçG&öÆÆW%÷G&6²æVæE÷F–6²‚’Â3C“°¢76W'EöW€¢6öçG&öÆÆW%÷G&6°¢æ6öçG&öÅö6†ævW2‚¢æ—FW"‚¢æÖ‡Æ6†ævWÂ€¢6†ævRçF–6²‚’À¢6†ævRæ6†ææVÂ‚’À¢6†ævRæ6öçG&öÆÆW"‚’À¢6†ævRçfÇVR‚¢’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢fV2°¢ƒÂ"ÂsBÂ’À¢ƒ#BÂ"ÂsBÂcB’À¢ƒC‚Â"ÂsBÂ’À¢ƒs"Â"ÂsBÂcB’À¢ƒ“bÂ"ÂsBÂ’À¢ƒ3Â"ÂsBÂ’À¢ƒ3#BÂ"ÂsBÂcB’À¢Ð¢“°¢Ð ¢5·FW7EÐ¢fâ6öæuö6öçG&öÆÆW%öW‡÷'E÷&V¦V7G5öÖ—76–æu÷7G&V×5öæEö–çfÆ–EöÖ–æw2‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂ#EÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FU÷&V6÷&BƒÂ2ÂC‚ÂcÂ’“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÒ“°¢ÆWB×WB6Æ—Ò³Sƒ²ƒÓ°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cwSbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&'&ævVÖVçBf—‡GW&R6†÷VÆB'6R"“° ¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öæuö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂÂƒ2Âƒ“’ÂsBÂÂ¢æ—5öW'"‚¢“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öæuö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂÂƒ2Âƒ“’Â#‚ÂÂ¢æ—5öW'"‚¢“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öæuö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂÂƒ2Âƒ“’ÂsBÂbÂ¢æ—5öW'"‚¢“°¢76W'B€¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öæuö6öçG&öÆÆW%÷7G&VÒ‚fFö7VÖVçBÂ’Âƒ2Âƒ“’ÂsBÂÂ¢æ—5öW'"‚¢“°¢Ð ¢5·FW7EÐ¢fâ–æfW'&VE÷GFW&å÷&WVEöÆVæwF…÷&÷VæG5÷W÷Fõ÷F†U÷&ö¦V7EöÖV7W&R‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂ2ÂóSÂcÂ“°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂ#EÓ°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc2ÂÂÒ“°¢ÆWB×WB6Æ—Ò³Sƒ²ƒÓ°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cwSbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚cóS3gS3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚&'&ævVÖVçBf—‡GW&R6†÷VÆB'6R"“° ¢ÆWB'—FW2Ð¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öær‚fFö7VÖVçBÂÂÖ–F”6†ææVÄÖ–æs£¥&W6W'fTæ÷FT6†ææVÇ2¢æW‡V7B‚&'&ævVÖVçB6†÷VÆBW‡÷'B"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBf–ÆR6†÷VÆB'6R"“°¢ÆWBæ÷FW2ÒÖ–F¢çG&6·2‚¢æ—FW"‚¢æfÆEöÖ‡ÇG&6·ÂG&6²ææ÷FW2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢76W'EöW†æ÷FW2æÆVâ‚’Â“°¢76W'EöW†æ÷FW5³Òç7F'E÷F–6²‚’Â“°¢76W'EöW†æ÷FW5³ÒæVæE÷F–6²‚’Â6öÖRƒóS’“°¢Ð ¢5·FW7EÐ¢fâW‡÷'G5÷FV×õöWFöÖF–öå÷W%÷F–6µöæE÷&W7F÷&W5÷F†U÷&ö¦V7E÷FV×ò‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂCÂÂƒRÂUÓ°¢VæE÷&ö¦V7Eö–æfõ÷7G&–ær‚f×WBWfVçE÷7G&VÒÂ„4"Â%DTÕò"“° ¢ÆWBö–çG3¢²†ccBÂccBÂc3"Â·Sƒ²EÒ“²5ÒÒ°¢ƒãÂãsRÂãc3"Â³ÂÂÂÒ’À¢ƒBãÂãsRÂÓãC#ƒSvc3"Â³ÂÂÂ„deÒ’À¢ƒ‚ãÂãÂãc3"Â³ÂÂÂ%Ò’À¢Ó°¢ÆWB×WBWFöÖF–öâÒfV2³„s²uÓ°¢WFöÖF–öâæW‡FVæEög&öÕ÷6Æ–6R‚b‡ö–çG2æÆVâ‚’2S3"’çFõöÆUö'—FW2‚’“°¢ÆWB×WB&Wf–÷W5÷÷6—F–öâÒã°¢f÷"‡÷6—F–öâÂfÇVRÂFVç6–öâÂF–Â’–âö–çG2°¢WFöÖF–öâæW‡FVæEög&öÕ÷6Æ–6R‚b‡÷6—F–öâÒ&Wf–÷W5÷÷6—F–öâ’çFõöÆUö'—FW2‚’“°¢WFöÖF–öâæW‡FVæEög&öÕ÷6Æ–6R‚gfÇVRçFõöÆUö'—FW2‚’“°¢WFöÖF–öâæW‡FVæEög&öÕ÷6Æ–6R‚gFVç6–öâçFõöÆUö'—FW2‚’“°¢WFöÖF–öâæW‡FVæEög&öÕ÷6Æ–6R‚gF–Â“°¢&Wf–÷W5÷÷6—F–öâÒ÷6—F–öã°¢Ð¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„TÂfWFöÖF–öâ“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“° ¢ÆWB×WB6Æ—Ò³Sƒ²ƒÓ°¢6Æ—³BâãeÒæ6÷•ög&öÕ÷6Æ–6R‚cóSbçFõöÆUö'—FW2‚’“°¢6Æ—³bâã…Òæ6÷•ög&öÕ÷6Æ–6R‚cCSbçFõöÆUö'—FW2‚’“°¢6Æ—³‚âã%Òæ6÷•ög&öÕ÷6Æ–6R‚csc‡S3"çFõöÆUö'—FW2‚’“°¢6Æ—³"âãEÒæ6÷•ög&öÕ÷6Æ–6R‚cC“—SbçFõöÆUö'—FW2‚’“°¢6Æ—³cBâãs%Òæ6÷•ög&öÕ÷6Æ–6R‚cãccBçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB‚f×WBWfVçE÷7G&VÒÂ„S’Âf6Æ—“° ¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚'FV×òWFöÖF–öâ&ö¦V7Bf—‡GW&R6†÷VÆB'6R"“°¢ÆWB'—FW2Ð¢Ö–F”f–ÆS£¦Væ6öFU÷&ö¦V7E÷6öær‚fFö7VÖVçBÂÂÖ–F”6†ææVÄÖ–æs£¥&W6W'fTæ÷FT6†ææVÇ2¢æW‡V7B‚'6öærFV×òWFöÖF–öâ6†÷VÆBW‡÷'B"“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚f'—FW2’æW‡V7B‚&W‡÷'FVBÔ”D’6†÷VÆB'6R"“°¢ÆWB6öæGV7F÷"ÒfÖ–F’çG&6·2‚•³Ó°¢ÆWBFV×÷2Ò6öæGV7F÷"çFV×õöWfVçG2‚“° ¢76W'B€¢FV×÷0¢æ—FW"‚¢æç’‡ÇFV×÷ÂFV×òçF–6²‚’ÓÒbb‡FV×òæ'Ò‚’ÒSã’æ'2‚’Âã¢“°¢76W'B€¢FV×÷0¢æ—FW"‚¢æç’‡ÇFV×÷ÂFV×òçF–6²‚’ÓÒ3ƒ2bb‡FV×òæ'Ò‚’ÒSã’æ'2‚’Âã¢“°¢76W'B€¢FV×÷0¢æ—FW"‚¢æç’‡ÇFV×÷ÂFV×òçF–6²‚’ÓÒ3ƒBbb‡FV×òæ'Ò‚’ÒC’ãscb’æ'2‚’Âã¢“°¢76W'B€¢FV×÷0¢æ—FW"‚¢æç’‡ÇFV×÷ÂFV×òçF–6²‚’ÓÒscrbb‡FV×òæ'Ò‚’Òcã’æ'2‚’Âã¢“°¢76W'B€¢FV×÷0¢æ—FW"‚¢æç’‡ÇFV×÷ÂFV×òçF–6²‚’ÓÒsc’bb‡FV×òæ'Ò‚’ÒCã’æ'2‚’Âã¢“°¢76W'EöW†6öæGV7F÷"æVæE÷F–6²‚’Âsc’“°¢Ð ¢5·FW7EÐ¢fâÖ–F•ö–×÷'E÷&V¦V7G5öÖ&–wV÷W5öV×G•÷GFW&åöVæ6öF–æu÷v—F†÷WEö×WFF–öâ‚’°¢ÆWBæ÷FRÒæ÷FU÷&V6÷&BƒÂÂC‚ÂcÂ“°¢ÆWB×WB7G&VÒÒfV2³ƒCÂÂÂƒCÂrÂÂ„CÂƒ…Ó°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ‚ÂÂ„SÂƒ…Ò“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚fæ÷FR“°¢7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ’ÂÂ„BÂÂÂÂÒ“°¢ÆWB&ö¦V7Eö'—FW2ÒfÇöf—‡GW&R‚g7G&VÒÂeµÒÂeµÒ“°¢ÆWBÖ–F•ö'—FW2ÒÖ–F•öf—‡GW&R€¢e°¢ƒÂƒ“Âƒ42ÂƒcBÂƒƒÂƒsÂƒƒÂƒ42ÂƒÂƒÂ„dbÂƒ$bÂƒÀ¢ÒÀ¢CƒÀ¢“°¢ÆWBÖ–F’ÒÖ–F”f–ÆS£§'6R‚fÖ–F•ö'—FW2’æW‡V7B‚$Ô”D’f—‡GW&R6†÷VÆB'6R"“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚g&ö¦V7Eö'—FW2’æW‡V7B‚'&ö¦V7B6†÷VÆB'6R"“° ¢ÆWBW'&÷"ÒFö7VÖVç@¢æ–×÷'EöÖ–F•÷G&6²‚fÖ–F’ÂÂ’Â¢æW‡V7EöW'"‚&6öæfÆ–7F–ær&ö¦V7BVæ6öF–æw2×W7Bæ÷B&RwVW76VB"“° ¢76W'B†ÖF6†W2†W'&÷"ÂfÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’’“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â&ö¦V7Eö'—FW2“°¢Ð ¢5·FW7EÐ¢fâWFöÖF–öå÷ö–çEö–ç6W'EöFVÆWFU÷&W6W'fW5ö†VFW'5÷G&–ÆW'5öæEö÷&–v–æÅö'—FW2‚’°¢ÆWB6÷W&6U÷ö–çG2Ò°¢ƒãÂã#RÂÓã#RÂ³Â"Â2ÂEÒ’À¢ƒ2ãÂãsRÂãRÂ³RÂbÂrÂ…Ò’À¢Ó° ¢f÷"G&–ÆW%öÆVæwF‚–â³"Â3eÒ°¢ÆWBG&–ÆW"ÒƒâçG&–ÆW%öÆVæwF‚¢æÖ‡Æ–æFW‡Â†–æFW‚¢2R#Sb’2S‚¢æ6öÆÆV7C££ÅfV3Åóãâ‚“°¢ÆWB÷&–v–æÂÒWFöÖF–öåö6†ææVÅöf—‡GW&R‚g6÷W&6U÷ö–çG2ÂgG&–ÆW"“°¢ÆWB×WBFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚&WFöÖF–öâf—‡GW&R6†÷VÆB'6R"“°¢ÆWB÷&–v–æÅ÷–ÆöBÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–æB‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÓÒ„T¢æW‡V7B‚&f—‡GW&R6†÷VÆB6öçF–ââWFöÖF–öâWfVçB"¢ç–ÆöB‚¢çFõ÷fV2‚“° ¢Fö7VÖVç@¢æ–ç6W'EöWFöÖF–öå÷ö–çBƒ’ÂÂ"ãÂãRÂã¢æW‡V7B‚&–ç6W'F–ær&WGvVVâ÷&FW&VBö–çG26†÷VÆB7V66VVB"“°¢ÆWB–ç6W'FVBÒFö7VÖVçBæWFöÖF–öåö6†ææVÇ2‚’çVçw&‚’ç&VÖ÷fRƒ“°¢76W'EöW†–ç6W'FVBçö–çG2‚’æÆVâ‚’Â2“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³Òç÷6—F–öåö&VG2‚’Âã“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³Òç÷6—F–öåö&VG2‚’Â"ã“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³%Òç÷6—F–öåö&VG2‚’Â2ã“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³ÒçG&–Æ–æuö'—FW2‚’Â³Â"Â2ÂEÒ“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³ÒçG&–Æ–æuö'—FW2‚’Â³²EÒ“°¢76W'EöW†–ç6W'FVBçö–çG2‚•³%ÒçG&–Æ–æuö'—FW2‚’Â³RÂbÂrÂ…Ò“° ¢ÆWB–ç6W'FVE÷–ÆöBÐ¢Fö7VÖVçBæWfVçG2‚•¶–ç6W'FVBæFFöWfVçEö–æFW‚‚’çVçw&‚•Òç–ÆöB‚“°¢76W'EöW‚f–ç6W'FVE÷–ÆöE²âãuÒÂf÷&–v–æÅ÷–ÆöE²âãuÒ“°¢76W'EöW€¢f–ç6W'FVE÷–ÆöE¶–ç6W'FVE÷–ÆöBæÆVâ‚’ÒG&–ÆW%öÆVæwF‚âåÒÀ¢G&–ÆW ¢“° ¢Fö7VÖVç@¢æFVÆWFUöWFöÖF–öå÷ö–çBƒ’Â¢æW‡V7B‚&FVÆWF–ærF†R–ç6W'FVBö–çB6†÷VÆB7V66VVB"“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð¢Ð ¢5·FW7EÐ¢fâWFöÖF–öå÷ö–çEöFVÆWFU÷&WF–ç5ö'6öÇWFU÷÷6—F–öç5öEö7W'fUöVFvW2‚’°¢ÆWB6÷W&6U÷ö–çG2Ò°¢ƒãÂãÂãÂ³ÂÂÂÒ’À¢ƒ2ãÂãRÂãÂ³"Â"Â"Â%Ò’À¢ƒBãÂã’ÂãÂ³2Â2Â2Â5Ò’À¢Ó°¢ÆWBG&–ÆW"ÒfV2³„C3²%Ó° ¢f÷"†FVÆWFVEö–æFW‚ÂW‡V7FVE÷÷6—F–öç2ÂW‡V7FVE÷F–Ç2’–â°¢ƒÂ³2ãÂBãÒÂµ³"Â"Â"Â%ÒÂ³2Â2Â2Â5ÕÒ’À¢ƒ"Â³ãÂ2ãÒÂµ³ÂÂÂÒÂ³"Â"Â"Â%ÕÒ’À¢Ò°¢ÆWB×WBFö7VÖVçBÐ¢fÇFö7VÖVçC£§'6R‚fWFöÖF–öåö6†ææVÅöf—‡GW&R‚g6÷W&6U÷ö–çG2ÂgG&–ÆW"’¢æW‡V7B‚&WFöÖF–öâf—‡GW&R6†÷VÆB'6R"“°¢Fö7VÖVç@¢æFVÆWFUöWFöÖF–öå÷ö–çBƒ’ÂFVÆWFVEö–æFW‚¢æW‡V7B‚&FVÆWF–ærâVFvRö–çB6†÷VÆB7V66VVB"“°¢ÆWB6†ææVÂÒFö7VÖVçBæWFöÖF–öåö6†ææVÇ2‚’çVçw&‚’ç&VÖ÷fRƒ“°¢76W'EöW†6†ææVÂçö–çG2‚’æÆVâ‚’Â"“°¢f÷"–æFW‚–ââã"°¢76W'EöW€¢6†ææVÂçö–çG2‚•¶–æFW…Òç÷6—F–öåö&VG2‚’À¢W‡V7FVE÷÷6—F–öç5¶–æFW…Ð¢“°¢76W'EöW€¢6†ææVÂçö–çG2‚•¶–æFW…ÒçG&–Æ–æuö'—FW2‚’À¢W‡V7FVE÷F–Ç5¶–æFW…Ð¢“°¢Ð ¢ÆWB–ÆöBÒFö7VÖVçBæWfVçG2‚•¶6†ææVÂæFFöWfVçEö–æFW‚‚’çVçw&‚•Òç–ÆöB‚“°¢76W'EöW‚g–ÆöE·–ÆöBæÆVâ‚’ÒG&–ÆW"æÆVâ‚’âåÒÂG&–ÆW"“°¢Ð¢Ð ¢5·FW7EÐ¢fâFV6öFW5öÆ–W%ö6†–ÆEö6†ææVÅö–G5öæEöfÆw5öÆ÷76ÆW76Ç’‚’°¢ÆWB÷&–v–æÂÒÆ–W%ö6†ææVÅöf—‡GW&Rƒ2Âe³Â%Ò“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚&Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB6†ææVÇ2ÒFö7VÖVçBæ6†ææVÇ2‚“°¢76W'EöW†6†ææVÇ2æÆVâ‚’Â2“°¢76W'EöW†6†ææVÇ5³Òæ¶–æB‚’Â6öÖRƒ2’“°¢76W'EöW†6†ææVÇ5³Òæ6†ææVÅ÷G—R‚’Â6öÖR‡7WW#£¤6†ææVÅG—S£¤Æ–W"’“°¢76W'EöW†6†ææVÇ5³ÒæÆ–W%ö6†–ÆEö–G2‚’Â6öÖR…³Â%Òæ5÷6Æ–6R‚’’“°¢76W'EöW†6†ææVÇ5³ÒæÆ–W%öfÆw2‚’Â6öÖRƒ2’“°¢76W'EöW†6†ææVÇ5³ÒæÆ–W%÷&æFöÕöVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†6†ææVÇ5³ÒæÆ–W%ö7&÷76fFUöVæ&ÆVB‚’Â6öÖR‡G'VR’“°¢76W'EöW†6†ææVÇ5³Òæ6†ææVÅ÷G—R‚’Â6öÖR‡7WW#£¤6†ææVÅG—S£¤æF—fR’“°¢76W'EöW€¢6†ææVÇ5³%Òæ6†ææVÅ÷G—R‚’À¢6öÖR‡7WW#£¤6†ææVÅG—S£¤–ç7G'VÖVçB¢“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâÆ–W%öæ÷FU÷&÷WFW%öfç5ö÷WEöæE÷W6W5÷&WVF&ÆU÷&æFöÕö6†–ÆEö6†ö–6W2‚’°¢ÆWBÆÅö6†–ÆG&VâÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö6†ææVÅöf—‡GW&RƒÂe³Â%Ò’¢æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†ÆÅö6†–ÆG&Vâæ6†ææVÇ2‚’“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ#2Âc’ÂfV2³Â%Ò“°¢76W'B‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒ“’Â#2Âc’æ—5öV×G’‚’“° ¢ÆWB&æFöÕöÆ–W"ÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö6†ææVÅöf—‡GW&RƒÂe³Â%Ò’¢æW‡V7B‚'&æFöÒÆ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr‡&æFöÕöÆ–W"æ6†ææVÇ2‚’“°¢ÆWBf—'7Eö6†ö–6RÒ&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ#2Âc“°¢76W'EöW†f—'7Eö6†ö–6RæÆVâ‚’Â“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ#2Âc’Âf—'7Eö6†ö–6R“°¢ÆWB6†ö–6W2ÒƒâãcB¢æfÆEöÖ‡Ç6VVGÂ&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ6VVBÂc’¢æ6öÆÆV7C££Ç7FC£¦6öÆÆV7F–öç3£¤%G&VU6WCÅóãâ‚“°¢76W'EöW†6†ö–6W2Â³Â%Òæ–çFõö—FW"‚’æ6öÆÆV7B‚’“°¢Ð ¢5·FW7EÐ¢fâÆ–W%ö7&÷76fFU÷&÷WFW5÷6×ÆW%ö6†–ÆG&VåöE÷F†U÷6fVEöfFU÷÷6—F–öâ‚’°¢f÷"†fFRÂW‡V7FVB’–â°¢ƒÂ³ãÂãÂãÒ’À¢ƒcBÂ³ãRÂãRÂãÒ’À¢ƒ#‚Â³ãÂãÂãÒ’À¢ƒ“"Â³ãÂãRÂãUÒ’À¢ƒ#SbÂ³ãÂãÂãÒ’À¢Ò°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö7&÷76fFUöf—‡GW&R†fFR’¢æW‡V7B‚$Æ–W"7&÷76fFRf—‡GW&R6†÷VÆB'6R"“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÆ–W%öfFU÷&r‚’Â6öÖR†fFR2Sb’“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Fö7VÖVçBæ6†ææVÇ2‚’“°¢ÆWBF&vWG2Ò&÷WFW"çF&vWG5öf÷%öæ÷FU÷v—F…öv–âƒÂÂc“°¢ÆWB×WB7GVÂÒ³ã²5Ó°¢f÷"†6†ææVÅö–BÂv–â’–âF&vWG2°¢7GVÅ·W6—¦S£¦g&öÒ†6†ææVÅö–BÒ•ÒÒv–ã°¢Ð¢76W'EöW†7GVÂÂW‡V7FVB“°¢Ð¢Ð ¢5·FW7EÐ¢fâÆ–W%ö7&÷76fFUö6ö×÷6W5öv–ç5÷F‡&÷Vv…÷6×ÆW%ööæÇ•öæW7FVEöÆ–W'2‚’°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fæW7FVEöÆ–W%ö7&÷76fFUöf—‡GW&R†fÇ6R’¢æW‡V7B‚&æW7FVBÆ–W"7&÷76fFRf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Fö7VÖVçBæ6†ææVÇ2‚’“°¢ÆWB7GVÂÒ&÷WFW"çF&vWG5öf÷%öæ÷FU÷v—F…öv–âƒÂÂc“°¢76W'EöW†7GVÂÂfV2²ƒ2Âã#R’ÂƒBÂã#R’Âƒ"ÂãR•Ò“°¢Ð ¢5·FW7EÐ¢fâÆ–W%ö7&÷76fFUö¶VW5öæW7FVEöÖ—†VEö6†–ÆG&VåöE÷Væ—G•öv–â‚’°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚fæW7FVEöÆ–W%ö7&÷76fFUöf—‡GW&R‡G'VR’¢æW‡V7B‚&æW7FVBÖ—†VBÆ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Fö7VÖVçBæ6†ææVÇ2‚’“°¢76W'EöW€¢&÷WFW"çF&vWG5öf÷%öæ÷FU÷v—F…öv–âƒÂÂc’À¢fV2²ƒ2Âã’ÂƒBÂã’Âƒ"Âã•Ð¢“°¢Ð ¢5·FW7EÐ¢fâVF—F–æuöÆ–W%öfFU÷&W6W'fW5ö÷F†W%÷&ÖWFW%ö'—FW2‚’°¢ÆWB÷&–v–æÂÒÆ–W%ö7&÷76fFUöf—‡GW&RƒcB“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&ÖWFW'5ö–æFW‚ÒFö7VÖVçBæ6†ææVÇ2‚•³Ð¢æWfVçE÷&ævR‚¢æf–æB‡Æ–æFW‡ÂFö7VÖVçBæWfVçG2‚•²¦–æFW…Òæ÷6öFR‚’ÓÒ„3r¢æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB†fR&ÖWFW'2"“°¢ÆWB&Vf÷&RÒFö7VÖVçBæWfVçG2‚•·&ÖWFW'5ö–æFW…Òç–ÆöB‚’çFõ÷fV2‚“° ¢Fö7VÖVç@¢ç6WEöÆ–W%öfFRƒÂ“"¢æW‡V7B‚$Æ–W"fFR6†÷VÆB&RVF—F&ÆR"“°¢ÆWBgFW"ÒFö7VÖVçBæWfVçG2‚•·&ÖWFW'5ö–æFW…Òç–ÆöB‚“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÆ–W%öfFU÷&r‚’Â6öÖRƒ“"’“°¢76W'EöW‚fgFW%²âãƒ…ÒÂf&Vf÷&U²âãƒ…Ò“°¢76W'EöW‚fgFW%³ƒ‚âã“%ÒÂc“'S3"çFõöÆUö'—FW2‚’“°¢76W'EöW‚fgFW%³“"âåÒÂf&Vf÷&U³“"âåÒ“°¢76W'B†Fö7VÖVçBç6WEöÆ–W%öfFRƒÂ#Sr’æ—5öW'"‚’“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVBÆ–W"6†÷VÆBVæ6öFR"“°¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBÆ–W"6†÷VÆB&V÷Vâ"“°¢76W'EöW‡&V÷VæVBæ6†ææVÇ2‚•³ÒæÆ–W%öfFU÷&r‚’Â6öÖRƒ“"’“°¢Ð ¢5·FW7EÐ¢fâÆ–W%ö7&÷76fFUö¶VW5÷Vç7W÷'FVEö6†–ÆG&VåöE÷Væ—G•öv–â‚’°¢ÆWBÖ—†VEö6†–ÆG&VâÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö6†ææVÅöf—‡GW&Rƒ"Âe³Â%Ò’¢æW‡V7B‚&Ö—†VBÆ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Ö—†VEö6†–ÆG&Vâæ6†ææVÇ2‚’“°¢76W'EöW€¢&÷WFW"çF&vWG5öf÷%öæ÷FU÷v—F…öv–âƒÂÂc’À¢fV2²ƒÂã’Âƒ"Âã•Ð¢“° ¢ÆWB–çfÆ–EöfFRÒfÇFö7VÖVçC£§'6R‚fÆ–W%ö7&÷76fFUöf—‡GW&Rƒ#Sr’¢æW‡V7B‚&–çfÆ–BÖfFRÆ–W"f—‡GW&R6†÷VÆB'6R"“°¢76W'EöW†–çfÆ–EöfFRæ6†ææVÇ2‚•³ÒæÆ–W%öfFU÷&r‚’ÂæöæR“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†–çfÆ–EöfFRæ6†ææVÇ2‚’“°¢76W'EöW€¢&÷WFW"çF&vWG5öf÷%öæ÷FU÷v—F…öv–âƒÂÂc’À¢fV2²ƒÂã’Âƒ"Âã’Âƒ2Âã•Ð¢“°¢Ð ¢5·FW7EÐ¢fâÆ–W%öæ÷FU÷&÷WFW%÷6¶—5öÖ—76–æuöF—6&ÆVEöæEöæW7FVEöÆ–W%ö6†–ÆG&Vâ‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒRÂ2Âƒ“Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢f÷"6†–ÆB–â³SbÂ"Â2Â“•Ò°¢WfVçE÷7G&VÒçW6‚ƒƒTR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚f6†–ÆBçFõöÆUö'—FW2‚’“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂÂÂƒÂÂƒRÂ%Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ"ÂÂƒRÂ5Ò“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ2ÂÂƒRÂBÂƒc"ÂÂÒ“°¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚$Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Fö7VÖVçBæ6†ææVÇ2‚’“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂÂc’ÂfV2³5Ò“°¢Ð ¢5·FW7EÐ¢fâæ÷FU÷&÷WFW%öÆ–W5÷6÷W&6UöæEö6†–ÆEö¶W•÷&Vv–öç5ö&Vf÷&U÷&æFöÕ÷6VÆV7F–öâ‚’°¢ÆWB×WBWfVçE÷7G&VÒÒfV2³ƒCÂÂÂƒRÂ2Âƒ“Ó°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cS3"çFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒTR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚cSbçFõöÆUö'—FW2‚’“°¢WfVçE÷7G&VÒçW6‚ƒƒTR“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚c'SbçFõöÆUö'—FW2‚’“°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒC‚ÂƒB’À¢“° ¢f÷"†6†ææVÅö–BÂÆ÷rÂ†–v‚’–â²ƒ÷SbÂC‚Âs"’Âƒ%÷SbÂcÂƒB•Ò°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ6†ææVÅö–B2S‚ÂÂƒRÂÒ“°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2†Æ÷rÂ†–v‚’À¢“°¢Ð¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒCÂ2ÂÂƒRÂÒ“°¢VæEöFFöWfVçB€¢f×WBWfVçE÷7G&VÒÀ¢„3rÀ¢f6†ææVÅö¶W•÷&Vv–öå÷&ÖWFW'2ƒcÂc’À¢“°¢WfVçE÷7G&VÒæW‡FVæEög&öÕ÷6Æ–6R‚e³ƒc"ÂÂÒ“° ¢ÆWBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚ffÇöf—‡GW&R‚fWfVçE÷7G&VÒÂeµÒÂeµÒ’¢æW‡V7B‚$Æ–W"¶W’×&Vv–öâf—‡GW&R6†÷VÆB'6R"“°¢ÆWB&÷WFW"Ò6†ææVÄæ÷FU&÷WFW#£¦æWr†Fö7VÖVçBæ6†ææVÇ2‚’“° ¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂÂCr’ÂfV3££ÇScã£¦æWr‚’“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂÂC‚’ÂfV2³Ò“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ"ÂS’’ÂfV2³Ò“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂ2Âs2’ÂfV2³%Ò“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂBÂƒB’ÂfV2³%Ò“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒÂRÂƒR’ÂfV3££ÇScã£¦æWr‚’“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒ2ÂbÂS’’ÂfV3££ÇScã£¦æWr‚’“°¢76W'EöW‡&÷WFW"çF&vWG5öf÷%öæ÷FRƒ2ÂbÂc’ÂfV2³5Ò“°¢Ð ¢5·FW7EÐ¢fâVF—F–æuöÆ–W%ö6†–ÆG&Vå÷&W6W'fW5ö÷F†W%ö6†ææVÅöWfVçG2‚’°¢ÆWB÷&–v–æÂÒÆ–W%ö6†ææVÅöf—‡GW&Rƒ„Só2Âe³Â%Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚&Æ–W"f—‡GW&R6†÷VÆB'6R"“°¢ÆWBæöåö6†–ÆEöWfVçG2ÒFö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÒƒTR¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚“° ¢Fö7VÖVç@¢ç6WEöÆ–W%ö6†–ÆEö–G2ƒÂe³"ÂÂ%Ò¢æW‡V7B‚$Æ–W"6†–ÆB”G26†÷VÆB&RVF—F&ÆR"“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÆ–W%ö6†–ÆEö–G2‚’À¢6öÖR…³"ÂÂ%Òæ5÷6Æ–6R‚’¢“°¢76W'EöW†Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÆ–W%öfÆw2‚’Â6öÖRƒ„Só2’“°¢76W'EöW€¢Fö7VÖVç@¢æWfVçG2‚¢æ—FW"‚¢æf–ÇFW"‡ÆWfVçGÂWfVçBæ÷6öFR‚’ÒƒTR¢æÖ‡ÆWfVçGÂWfVçBçv—&Uö'—FW2‚’çFõ÷fV2‚’¢æ6öÆÆV7C££ÅfV3Åóãâ‚’À¢æöåö6†–ÆEöWfVçG0¢“° ¢ÆWBVæ6öFVBÒFö7VÖVç@¢æVæ6öFUöÆ÷76ÆW72‚¢æW‡V7B‚&VF—FVBÆ–W"6†÷VÆBVæ6öFR"“°¢ÆWB&V÷VæVBÒfÇFö7VÖVçC£§'6R‚fVæ6öFVB’æW‡V7B‚&VF—FVBÆ–W"6†÷VÆB'6Rv–â"“°¢76W'EöW€¢&V÷VæVBæ6†ææVÇ2‚•³ÒæÆ–W%ö6†–ÆEö–G2‚’À¢6öÖR…³"ÂÂ%Òæ5÷6Æ–6R‚’¢“°¢Ð ¢5·FW7EÐ¢fâÆ–W%ö6†–ÆG&Våö6åö&UöFFVE÷FõöæE÷&VÖ÷fVEög&öÕöåöV×G•öÆ—7B‚’°¢ÆWB÷&–v–æÂÒÆ–W%ö6†ææVÅöf—‡GW&RƒrÂeµÒ“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚&V×G’Æ–W"f—‡GW&R6†÷VÆB'6R"“° ¢Fö7VÖVç@¢ç6WEöÆ–W%ö6†–ÆEö–G2ƒÂe³Â%Ò¢æW‡V7B‚&V×G’Æ–W"6†–ÆBÆ—7B6†÷VÆB66WB&VfW&Væ6W2"“°¢76W'EöW€¢Fö7VÖVçBæ6†ææVÇ2‚•³ÒæÆ–W%ö6†–ÆEö–G2‚’À¢6öÖR…³Â%Òæ5÷6Æ–6R‚’¢“°¢Fö7VÖVç@¢ç6WEöÆ–W%ö6†–ÆEö–G2ƒÂeµÒ¢æW‡V7B‚$Æ–W"6†–ÆB&VfW&Væ6W26†÷VÆB&R&VÖ÷f&ÆR"“°¢76W'EöW†Fö7VÖVçBæVæ6öFUöÆ÷76ÆW72‚’çVçw&‚’Â÷&–v–æÂ“°¢Ð ¢5·FW7EÐ¢fâÆ–W%ö6†–ÆEöVF—E÷&V¦V7G5öæöåöÆ–W%öæEöÖ—76–æuö6†ææVÇ2‚’°¢ÆWB÷&–v–æÂÒÆ–W%ö6†ææVÅöf—‡GW&RƒÂe³Ò“°¢ÆWB×WBFö7VÖVçBÒfÇFö7VÖVçC£§'6R‚f÷&–v–æÂ’æW‡V7B‚&Æ–W"f—‡GW&R6†÷VÆB'6R"“° ¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÆ–W%ö6†–ÆEö–G2ƒÂe³%Ò’À¢W'"„fÇW'&÷#£¥Vç7W÷'FVDVF—B…ò’¢’“°¢76W'B†ÖF6†W2€¢Fö7VÖVçBç6WEöÆ–W%ö6†–ÆEö–G2ƒ“’Âe³%Ò’À¢W'"„fÇW'&÷#£¤6†ææVÄæ÷Df÷VæBƒ“’’¢’“°¢Ð ¢5·FW7EÐ¢fâVæ¶æ÷våö6†ææVÅ÷G—U÷&÷VæE÷G&—5ö—G5÷&u÷fÇVR‚’°¢ÆWB6†ææVÅ÷G—RÒ7WW#£¤6†ææVÅG—S£¦g&öÕ÷&rƒ“°¢76W'EöW†6†ææVÅ÷G—RÂ7WW#£¤6†ææVÅG—S£¥Væ¶æ÷vâƒ’“°¢76W'EöW†6†ææVÅ÷G—Rç&r‚’Â“°¢Ð ¢5·FW7EÐ¢fâ–çfÆ–EöfÇöÖv–5ö—5÷&W÷'FVB‚’°¢ÆWBW'&÷"ÒfÇFö7VÖVçC£§'6R†"&æ÷BÖâÖfÇ"’æW‡V7EöW'"‚&&BÖv–2×W7Bf–Â"“°¢76W'B†ÖF6†W2†W'&÷"ÂfÇW'&÷#£¤&DÖv–2²ââÒ’“°¢Ð§Ð
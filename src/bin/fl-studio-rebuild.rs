use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align2, Color32, FontId, Id, PointerButton, Sense, Stroke, Vec2};
use flp_rebuild::audio::{
    AudioAccess, AudioDeviceCatalog, AudioEngine, AudioSettings, enumerate_devices,
};
use flp_rebuild::media::{
    AudioWaveform, SamplePathResolver, decode_audio_preview, decode_audio_waveform,
};
use flp_rebuild::midi::{MidiChannelMapping, MidiFile};
use flp_rebuild::plugins::{PluginCandidate, PluginFormat, scan_installed_plugins};
use flp_rebuild::project_package::ProjectPackageWorkspace;
use flp_rebuild::sample_render::{
    AudioClipRenderOptions, PlaylistRenderOptions, PlaylistRenderSummary,
    SamplerPatternRenderOptions, SamplerPatternRenderSummary, render_audio_clips_to_wav,
    render_playlist_with_vst3_to_wav_cancellable, stream_playlist_with_vst3_to_device,
    stream_sampler_pattern_to_device,
};
use flp_rebuild::vst3::{Vst3HostRuntime, Vst3PatternRenderOptions, Vst3PatternStreamHandle};
use flp_rebuild::{
    ArpeggioDirection, ArpeggioOptions, AutomationChannel, AutomationPoint, AutomationPointEdit,
    ChannelSummary, FlpDocument, FstPreset, FstPresetKind, Pattern, PatternNote, PatternNoteEdit,
    PlaylistClip, PlaylistClipEdit, PlaylistTrack, ProjectInfoEdit, ProjectSettingsEdit,
    RandomizerOptions, TimeMarker, TimeMarkerEdit, VstPluginStateMetadata,
};

const PANEL: Color32 = Color32::from_rgb(31, 32, 34);
const PANEL_DARK: Color32 = Color32::from_rgb(24, 25, 27);
const PANEL_LIGHT: Color32 = Color32::from_rgb(43, 44, 47);
const GRID: Color32 = Color32::from_rgb(56, 58, 61);
const TEXT: Color32 = Color32::from_rgb(220, 221, 224);
const MUTED: Color32 = Color32::from_rgb(139, 142, 147);
const GREEN: Color32 = Color32::from_rgb(113, 172, 77);
const BLUE: Color32 = Color32::from_rgb(73, 128, 174);
const PURPLE: Color32 = Color32::from_rgb(150, 93, 181);
const ORANGE: Color32 = Color32::from_rgb(195, 129, 61);
const HISTORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const AUDIO_WAVEFORM_BUCKETS: usize = 4096;
const MAX_WAVEFORM_WORKERS: usize = 1;
const DEFAULT_AUTOSAVE_MINUTES: u8 = 5;
const DEFAULT_BACKUP_RETENTION: usize = 20;
const RECENT_PROJECT_LIMIT: usize = 50;
const BROWSER_SEARCH_ROOT_LIMIT: usize = 30;
const MAX_BROWSER_RECURSIVE_SCAN_ENTRIES: usize = 20_000;
const AUTOSAVE_INTERVALS_MINUTES: [u8; 5] = [0, 1, 5, 10, 15];
const BACKUP_RETENTION_OPTIONS: [usize; 4] = [5, 10, 20, 50];
const PITCH_CLASSES: [&str; 12] = [
    "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
];
const MIDI_CHANNEL_COLORS: [Color32; 16] = [
    Color32::from_rgb(113, 172, 77),
    Color32::from_rgb(73, 128, 174),
    Color32::from_rgb(195, 129, 61),
    Color32::from_rgb(150, 93, 181),
    Color32::from_rgb(63, 157, 143),
    Color32::from_rgb(192, 91, 105),
    Color32::from_rgb(183, 159, 77),
    Color32::from_rgb(92, 146, 193),
    Color32::from_rgb(170, 112, 79),
    Color32::from_rgb(112, 158, 101),
    Color32::from_rgb(143, 112, 173),
    Color32::from_rgb(76, 154, 161),
    Color32::from_rgb(195, 110, 146),
    Color32::from_rgb(141, 143, 82),
    Color32::from_rgb(100, 129, 181),
    Color32::from_rgb(172, 128, 105),
];

fn project_hash(document: &FlpDocument) -> Option<u64> {
    let bytes = document.encode_lossless().ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}
const FL_GENRES: &[&str] = &[
    "(none)",
    "Acid House",
    "Afro House",
    "Afrobeats",
    "Amapiano",
    "Ambient",
    "Bass House",
    "Boom Bap",
    "Breakbeat",
    "Cinematic",
    "Classic House",
    "Classical",
    "Country",
    "D&B",
    "Dancehall",
    "Deep House",
    "Disco",
    "Downtempo",
    "Drill",
    "Dubstep",
    "EDM",
    "Electro",
    "Electronica",
    "Film Score",
    "Funk",
    "Future Bass",
    "Future Rave",
    "Hardstyle",
    "Hip hop",
    "House",
    "Hybrid Trap",
    "Hyperpop",
    "Hypertrance",
    "Indie",
    "Jazz",
    "Jersey Club",
    "Latin",
    "Latin House",
    "Lo-Fi",
    "Lounge",
    "Melodic Techno",
    "Phonk",
    "Pop",
    "Progressive House",
    "Psytrance",
    "R&B",
    "Rage Trap",
    "Reggaeton",
    "Rock",
    "Slap House",
    "Soul",
    "Synthwave",
    "Tech House",
    "Techno",
    "Trance",
    "Trap",
    "Tropical House",
];

fn candidate_matches_vst_metadata(
    candidate: &PluginCandidate,
    metadata: &VstPluginStateMetadata,
) -> bool {
    if candidate.format != PluginFormat::Vst3 {
        return false;
    }

    let candidate_name = candidate.name.to_lowercase();
    let source_bundle = metadata.path().and_then(|path| {
        Path::new(path)
            .file_stem()
            .or_else(|| Path::new(path).file_name())
            .map(|value| value.to_string_lossy().to_lowercase())
    });
    source_bundle.as_deref() == Some(candidate_name.as_str())
        || metadata
            .name()
            .is_some_and(|name| name.eq_ignore_ascii_case(&candidate.name))
}

fn detect_chord_name(notes: &[PatternNote], channel_id: u16, position: u32) -> String {
    let pitch_classes = notes
        .iter()
        .filter(|note| note.channel_id == channel_id && note.position == position)
        .map(|note| (note.key % 12) as u8)
        .collect::<BTreeSet<_>>();
    if pitch_classes.len() < 2 {
        return "No chord".to_owned();
    }

    const QUALITIES: &[(&str, &[u8])] = &[
        ("major", &[0, 4, 7]),
        ("minor", &[0, 3, 7]),
        ("diminished", &[0, 3, 6]),
        ("augmented", &[0, 4, 8]),
        ("sus2", &[0, 2, 7]),
        ("sus4", &[0, 5, 7]),
        ("7", &[0, 4, 7, 10]),
        ("maj7", &[0, 4, 7, 11]),
        ("m7", &[0, 3, 7, 10]),
        ("m7♭5", &[0, 3, 6, 10]),
        ("dim7", &[0, 3, 6, 9]),
        ("5", &[0, 7]),
    ];
    for root in &pitch_classes {
        let mut intervals = pitch_classes
            .iter()
            .map(|pitch| (*pitch + 12 - *root) % 12)
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        for (quality, expected) in QUALITIES {
            if intervals == *expected {
                return format!("{} {quality}", PITCH_CLASSES[usize::from(*root)]);
            }
        }
    }
    format!("{} notes", pitch_classes.len())
}

fn matching_vst3_candidate<'a>(
    candidates: &'a [PluginCandidate],
    metadata: &VstPluginStateMetadata,
) -> Option<&'a PluginCandidate> {
    candidates
        .iter()
        .find(|candidate| candidate_matches_vst_metadata(candidate, metadata))
}

fn main() -> eframe::Result {
    let initial_project = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("FL Studio Rebuild")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 640.0])
            .with_maximized(true),
        ..Default::default()
    };
    eframe::run_native(
        "FL Studio Rebuild",
        options,
        Box::new(move |creation| Ok(Box::new(DawUi::new(creation, initial_project)))),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MainView {
    Playlist,
    ChannelRack,
    PianoRoll,
    Mixer,
    Plugins,
    Audio,
    Automation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollSnap {
    None,
    QuarterBeat,
    HalfBeat,
    Beat,
    TwoBeats,
    Bar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollEventTarget {
    Velocity,
    Pan,
    Release,
    Pitch,
    ModX,
    ModY,
}

impl PianoRollEventTarget {
    const ALL: [Self; 6] = [
        Self::Velocity,
        Self::Pan,
        Self::Release,
        Self::Pitch,
        Self::ModX,
        Self::ModY,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Velocity => "Velocity",
            Self::Pan => "Pan",
            Self::Release => "Release",
            Self::Pitch => "Fine pitch",
            Self::ModX => "Mod X",
            Self::ModY => "Mod Y",
        }
    }

    fn maximum(self) -> u16 {
        match self {
            Self::Velocity | Self::Pan | Self::Release => 128,
            Self::Pitch => 240,
            Self::ModX | Self::ModY => 255,
        }
    }

    fn value(self, note: &PatternNote) -> u16 {
        match self {
            Self::Velocity => u16::from(note.velocity),
            Self::Pan => u16::from(note.pan),
            Self::Release => u16::from(note.release),
            Self::Pitch => u16::from(note.fine_pitch),
            Self::ModX => u16::from(note.mod_x),
            Self::ModY => u16::from(note.mod_y),
        }
    }

    fn edit(self, value: u16) -> PatternNoteEdit {
        let value = value.min(self.maximum()) as u8;
        match self {
            Self::Velocity => PatternNoteEdit {
                velocity: Some(value),
                ..PatternNoteEdit::default()
            },
            Self::Pan => PatternNoteEdit {
                pan: Some(value),
                ..PatternNoteEdit::default()
            },
            Self::Release => PatternNoteEdit {
                release: Some(value),
                ..PatternNoteEdit::default()
            },
            Self::Pitch => PatternNoteEdit {
                fine_pitch: Some(value),
                ..PatternNoteEdit::default()
            },
            Self::ModX => PatternNoteEdit {
                mod_x: Some(value),
                ..PatternNoteEdit::default()
            },
            Self::ModY => PatternNoteEdit {
                mod_y: Some(value),
                ..PatternNoteEdit::default()
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollEditScope {
    Automatic,
    Channel,
    Selection,
}

impl PianoRollEditScope {
    fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Auto",
            Self::Channel => "Channel",
            Self::Selection => "Selected notes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollSelectionCommand {
    All,
    Invert,
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepGraphMode {
    Note,
    Velocity,
    Pan,
    Release,
    FinePitch,
    ModX,
    ModY,
    Shift,
}

impl StepGraphMode {
    const ALL: [Self; 8] = [
        Self::Note,
        Self::Velocity,
        Self::Pan,
        Self::Release,
        Self::FinePitch,
        Self::ModX,
        Self::ModY,
        Self::Shift,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Velocity => "Velocity",
            Self::Pan => "Pan",
            Self::Release => "Release",
            Self::FinePitch => "Fine pitch",
            Self::ModX => "Mod X",
            Self::ModY => "Mod Y",
            Self::Shift => "Shift (%)",
        }
    }

    fn maximum(self) -> f32 {
        match self {
            Self::Note => 132.0,
            Self::Velocity | Self::Release | Self::ModX | Self::ModY => 127.0,
            Self::Pan => 128.0,
            Self::FinePitch => 255.0,
            Self::Shift => 100.0,
        }
    }

    fn center(self) -> Option<f32> {
        match self {
            Self::Pan => Some(64.0),
            Self::FinePitch => Some(128.0),
            _ => None,
        }
    }

    fn value(self, note: &PatternNote, step_start: u64, step_ticks: u64) -> f32 {
        match self {
            Self::Note => f32::from(note.key),
            Self::Velocity => f32::from(note.velocity),
            Self::Pan => f32::from(note.pan),
            Self::Release => f32::from(note.release),
            Self::FinePitch => f32::from(note.fine_pitch),
            Self::ModX => f32::from(note.mod_x),
            Self::ModY => f32::from(note.mod_y),
            Self::Shift => {
                let max_shift = (step_ticks.saturating_mul(99) / 100).max(1);
                let offset = u64::from(note.position).saturating_sub(step_start);
                (offset as f32 / max_shift as f32 * 100.0).clamp(0.0, 100.0)
            }
        }
    }

    fn edit(self, value: f32, step_start: u64, step_ticks: u64) -> PatternNoteEdit {
        let value = value.round().clamp(0.0, self.maximum());
        match self {
            Self::Note => PatternNoteEdit {
                key: Some(value as u16),
                ..PatternNoteEdit::default()
            },
            Self::Velocity => PatternNoteEdit {
                velocity: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::Pan => PatternNoteEdit {
                pan: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::Release => PatternNoteEdit {
                release: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::FinePitch => PatternNoteEdit {
                fine_pitch: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::ModX => PatternNoteEdit {
                mod_x: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::ModY => PatternNoteEdit {
                mod_y: Some(value as u8),
                ..PatternNoteEdit::default()
            },
            Self::Shift => {
                let max_shift = step_ticks.saturating_mul(99) / 100;
                let offset = (f64::from(value) * max_shift as f64 / 100.0).round() as u64;
                PatternNoteEdit {
                    position: Some(
                        step_start.saturating_add(offset).min(u64::from(u32::MAX)) as u32
                    ),
                    ..PatternNoteEdit::default()
                }
            }
        }
    }

    fn apply_to_note(self, note: &mut PatternNote, value: f32, step_start: u64, step_ticks: u64) {
        let edit = self.edit(value, step_start, step_ticks);
        if let Some(value) = edit.position {
            note.position = value;
        }
        if let Some(value) = edit.key {
            note.key = value;
        }
        if let Some(value) = edit.velocity {
            note.velocity = value;
        }
        if let Some(value) = edit.pan {
            note.pan = value;
        }
        if let Some(value) = edit.release {
            note.release = value;
        }
        if let Some(value) = edit.fine_pitch {
            note.fine_pitch = value;
        }
        if let Some(value) = edit.mod_x {
            note.mod_x = value;
        }
        if let Some(value) = edit.mod_y {
            note.mod_y = value;
        }
    }
}

impl PianoRollSnap {
    const ALL: [Self; 6] = [
        Self::None,
        Self::QuarterBeat,
        Self::HalfBeat,
        Self::Beat,
        Self::TwoBeats,
        Self::Bar,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::QuarterBeat => "1/4 beat",
            Self::HalfBeat => "1/2 beat",
            Self::Beat => "1 beat",
            Self::TwoBeats => "2 beats",
            Self::Bar => "1 bar",
        }
    }

    fn ticks(self, ppq: u16, time_signature: Option<(u8, u8)>) -> u32 {
        let ppq = u32::from(ppq).max(1);
        match self {
            Self::None => 1,
            Self::QuarterBeat => (ppq / 4).max(1),
            Self::HalfBeat => (ppq / 2).max(1),
            Self::Beat => ppq,
            Self::TwoBeats => ppq.saturating_mul(2),
            Self::Bar => {
                let (numerator, denominator) = time_signature.unwrap_or((4, 4));
                let numerator = u32::from(numerator.max(1));
                let denominator = u32::from(denominator.max(1));
                ppq.saturating_mul(numerator).saturating_mul(4) / denominator
            }
        }
        .max(1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollScale {
    None,
    Major,
    NaturalMinor,
    HarmonicMinor,
    MajorPentatonic,
    MinorPentatonic,
}

impl PianoRollScale {
    const ALL: [Self; 6] = [
        Self::None,
        Self::Major,
        Self::NaturalMinor,
        Self::HarmonicMinor,
        Self::MajorPentatonic,
        Self::MinorPentatonic,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Major => "Major",
            Self::NaturalMinor => "Natural minor",
            Self::HarmonicMinor => "Harmonic minor",
            Self::MajorPentatonic => "Major pentatonic",
            Self::MinorPentatonic => "Minor pentatonic",
        }
    }

    fn contains(self, key: u16, root: u8) -> bool {
        let intervals: &[u8] = match self {
            Self::None => return true,
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Self::MajorPentatonic => &[0, 2, 4, 7, 9],
            Self::MinorPentatonic => &[0, 3, 5, 7, 10],
        };
        intervals.contains(&(((key % 12 + 12 - u16::from(root)) % 12) as u8))
    }

    fn intervals(self) -> &'static [u8] {
        match self {
            Self::None => &[],
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Self::MajorPentatonic => &[0, 2, 4, 7, 9],
            Self::MinorPentatonic => &[0, 3, 5, 7, 10],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollChordStamp {
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
    ScaleTriad,
    ScaleSeventh,
}

impl PianoRollChordStamp {
    const ALL: [Self; 12] = [
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
        Self::ScaleTriad,
        Self::ScaleSeventh,
    ];

    fn label(self) -> &'static str {
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
            Self::ScaleTriad => "Scale triad",
            Self::ScaleSeventh => "Scale seventh",
        }
    }

    fn pitches(self, root_key: u16, scale: PianoRollScale, scale_root: u8) -> Vec<u16> {
        let intervals: &[u8] = match self {
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
            Self::ScaleTriad | Self::ScaleSeventh => &[],
        };
        if !intervals.is_empty() {
            return intervals
                .iter()
                .filter_map(|interval| root_key.checked_add(u16::from(*interval)))
                .filter(|key| *key <= 127)
                .collect();
        }

        let scale_intervals = scale.intervals();
        if scale_intervals.is_empty() {
            return [0, 4, 7]
                .into_iter()
                .filter_map(|interval| root_key.checked_add(interval))
                .filter(|key| *key <= 127)
                .collect();
        }

        let snapped_root = (0..=127u16)
            .filter(|key| scale.contains(*key, scale_root))
            .min_by_key(|key| key.abs_diff(root_key))
            .unwrap_or(root_key);
        let scale_degree = scale_intervals
            .iter()
            .position(|interval| {
                (u16::from(scale_root) + u16::from(*interval)) % 12 == snapped_root % 12
            })
            .unwrap_or(0);
        let degrees: &[usize] = if self == Self::ScaleSeventh {
            &[0, 2, 4, 6]
        } else {
            &[0, 2, 4]
        };
        let octave_base = (snapped_root / 12) * 12;
        degrees
            .iter()
            .filter_map(|degree| {
                let scale_index = scale_degree + degree;
                let octave_offset = (scale_index / scale_intervals.len()) * 12;
                octave_base
                    .checked_add(octave_offset as u16)?
                    .checked_add(u16::from(
                        scale_intervals[scale_index % scale_intervals.len()],
                    ))
            })
            .filter(|key| *key <= 127)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoteDragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NoteDragTarget {
    channel_id: u16,
    channel_note_index: usize,
    start_position: u32,
    start_length: u32,
    start_key: u16,
}

#[derive(Clone, Debug, PartialEq)]
struct ActiveNoteDrag {
    pattern_id: u16,
    channel_id: u16,
    channel_note_index: usize,
    start_pointer: egui::Pos2,
    targets: Vec<NoteDragTarget>,
    kind: NoteDragKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollSelectionDrag {
    pattern_id: u16,
    channel_id: u16,
    start: egui::Pos2,
    current: egui::Pos2,
    additive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollZoomDrag {
    pattern_id: u16,
    start: egui::Pos2,
    current: egui::Pos2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct StepGraphRamp {
    pattern_id: u16,
    channel_id: u16,
    mode: StepGraphMode,
    start_step: usize,
    start_value: f32,
}

enum AutomationEditAction {
    Insert {
        slot: usize,
        position_beats: f64,
        value: f64,
        tension: f32,
    },
    Edit {
        point_index: usize,
        edit: AutomationPointEdit,
    },
    Delete {
        point_index: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollGrid {
    rect: egui::Rect,
    keyboard_width: f32,
    tick_scale: f32,
    key_height: f32,
    key_low: u16,
    key_high: u16,
    snap_ticks: u32,
    ppq: u16,
}

struct PendingMidiImport {
    path: PathBuf,
    midi: MidiFile,
    selected_track: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserTab {
    Files,
    CurrentProject,
    Plugins,
    Favorites,
    Recent,
}

impl BrowserTab {
    const ALL: [Self; 5] = [
        Self::Files,
        Self::CurrentProject,
        Self::Plugins,
        Self::Favorites,
        Self::Recent,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::CurrentProject => "Project",
            Self::Plugins => "Plugins",
            Self::Favorites => "Favorites",
            Self::Recent => "Recent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserFilter {
    All,
    Audio,
    Projects,
    Presets,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserFileKind {
    Audio,
    Project,
    Preset,
    Midi,
}

impl BrowserFilter {
    const ALL: [Self; 4] = [Self::All, Self::Audio, Self::Projects, Self::Presets];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All files",
            Self::Audio => "Samples",
            Self::Projects => "Projects",
            Self::Presets => "Presets",
        }
    }
}

#[derive(Clone, Debug)]
struct BrowserEntry {
    path: PathBuf,
    name: String,
    is_directory: bool,
}

#[derive(Clone)]
struct FstPresetDetails {
    path: PathBuf,
    file_size: usize,
    format: u16,
    kind: FstPresetKind,
    version: String,
    event_count: usize,
    channel_count: usize,
    plugin_state_count: usize,
    mixer_insert_count: usize,
    automation_channel_count: usize,
    trailing_byte_count: usize,
}

struct BrowserTagEditor {
    path: PathBuf,
    tags: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BrowserTagLogic {
    #[default]
    Any,
    All,
}

impl BrowserTagLogic {
    fn label(self) -> &'static str {
        match self {
            Self::Any => "Any",
            Self::All => "All",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BrowserTabColor {
    #[default]
    Default,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl BrowserTabColor {
    const ALL: [Self; 7] = [
        Self::Default,
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Purple,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Yellow => "Yellow",
            Self::Green => "Green",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Purple => "purple",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|color| color.key() == value)
    }

    fn text_color(self) -> Option<egui::Color32> {
        match self {
            Self::Default => None,
            Self::Red => Some(egui::Color32::from_rgb(244, 103, 111)),
            Self::Orange => Some(egui::Color32::from_rgb(242, 164, 91)),
            Self::Yellow => Some(egui::Color32::from_rgb(229, 199, 87)),
            Self::Green => Some(egui::Color32::from_rgb(113, 194, 122)),
            Self::Blue => Some(egui::Color32::from_rgb(105, 170, 224)),
            Self::Purple => Some(egui::Color32::from_rgb(185, 144, 225)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BrowserTabIcon {
    #[default]
    None,
    Folder,
    Star,
    Music,
    Piano,
    Drum,
    Wave,
}

impl BrowserTabIcon {
    const ALL: [Self; 7] = [
        Self::None,
        Self::Folder,
        Self::Star,
        Self::Music,
        Self::Piano,
        Self::Drum,
        Self::Wave,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Folder => "Folder",
            Self::Star => "Star",
            Self::Music => "Music",
            Self::Piano => "Piano",
            Self::Drum => "Drum",
            Self::Wave => "Wave",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Folder => "folder",
            Self::Star => "star",
            Self::Music => "music",
            Self::Piano => "piano",
            Self::Drum => "drum",
            Self::Wave => "wave",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|icon| icon.key() == value)
    }

    fn glyph(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Folder => Some("▣"),
            Self::Star => Some("★"),
            Self::Music => Some("♫"),
            Self::Piano => Some("▦"),
            Self::Drum => Some("◉"),
            Self::Wave => Some("∿"),
        }
    }
}

#[derive(Clone)]
struct SavedBrowserSearch {
    name: String,
    query: String,
    filter: BrowserFilter,
    all_roots: bool,
    recursive: bool,
    path: PathBuf,
    tag_logic: BrowserTagLogic,
    selected_tags: BTreeSet<String>,
    hidden: bool,
    color: BrowserTabColor,
    icon: BrowserTabIcon,
}

struct BrowserSearchSaveDialog {
    name: String,
    search: SavedBrowserSearch,
}

struct BrowserTabCustomizeDialog {
    index: usize,
    name: String,
    color: BrowserTabColor,
    icon: BrowserTabIcon,
}

struct BrowserIndex {
    roots: Vec<PathBuf>,
    all_roots: bool,
    entries: Vec<BrowserEntry>,
    truncated: bool,
    unavailable_roots: Vec<PathBuf>,
}

struct PendingBrowserIndex {
    roots: Vec<PathBuf>,
    all_roots: bool,
    receiver: Receiver<Result<BrowserIndex, String>>,
    worker: thread::JoinHandle<()>,
}

struct PendingBrowserPreview {
    path: PathBuf,
    receiver: Receiver<Result<Vec<f32>, String>>,
    worker: thread::JoinHandle<()>,
}

struct PendingBackupWrite {
    source_path: PathBuf,
    purpose: BackupPurpose,
    receiver: Receiver<Result<PathBuf, String>>,
    worker: thread::JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum BackupPurpose {
    Autosave,
    Manual,
}

#[derive(Clone)]
struct RecoveryPrompt {
    source_path: PathBuf,
    backup_path: PathBuf,
    is_revert: bool,
}

#[derive(Clone)]
enum PendingProjectChange {
    Open(PathBuf),
    Exit,
}

#[derive(Clone, Copy)]
enum UnsavedChangeChoice {
    Save,
    Discard,
    Cancel,
}

impl MainView {
    const ALL: [Self; 7] = [
        Self::Playlist,
        Self::ChannelRack,
        Self::PianoRoll,
        Self::Mixer,
        Self::Plugins,
        Self::Audio,
        Self::Automation,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Playlist => "Playlist",
            Self::ChannelRack => "Channel Rack",
            Self::PianoRoll => "Piano roll",
            Self::Mixer => "Mixer",
            Self::Plugins => "Plug-ins",
            Self::Audio => "Audio",
            Self::Automation => "Automation",
        }
    }
}

struct DawUi {
    document: Option<FlpDocument>,
    current_path: Option<PathBuf>,
    package_workspace: Option<ProjectPackageWorkspace>,
    view: MainView,
    status: String,
    dirty: bool,
    undo_history: Vec<Vec<u8>>,
    redo_history: Vec<Vec<u8>>,
    pending_history_snapshot: Option<Vec<u8>>,
    saved_project_hash: Option<u64>,
    autosave_minutes: u8,
    autosave_before_risky: bool,
    backup_retention: usize,
    autosave_deadline: Option<Instant>,
    pending_backup_write: Option<PendingBackupWrite>,
    last_autosave_path: Option<PathBuf>,
    recovery_prompt: Option<RecoveryPrompt>,
    pending_project_change: Option<PendingProjectChange>,
    close_approved: bool,
    history_reset_during_frame: bool,
    history_navigation_during_frame: bool,
    playing: bool,
    project_playback_loaded: bool,
    playlist_playback_loaded: bool,
    tempo_bpm: f64,
    selected_pattern: Option<u16>,
    selected_note_channel: Option<u16>,
    selected_note: Option<(u16, u16, usize)>,
    selected_piano_notes: BTreeSet<(u16, u16, usize)>,
    piano_roll_select_mode: bool,
    piano_roll_selection_drag: Option<PianoRollSelectionDrag>,
    piano_roll_zoom_mode: bool,
    piano_roll_zoom_drag: Option<PianoRollZoomDrag>,
    piano_roll_playback_mode: bool,
    piano_roll_stamp_mode: bool,
    piano_roll_stamp_only_one: bool,
    piano_roll_chord_stamp: PianoRollChordStamp,
    last_piano_roll_audition: Option<(u16, u16, usize)>,
    piano_roll_zoom: f32,
    piano_roll_pending_scroll_offset: Option<Vec2>,
    piano_roll_grid_viewport: Option<egui::Rect>,
    piano_roll_grid_scroll_offset: Vec2,
    step_sequencer_bar: u32,
    selected_graph_channel: Option<u16>,
    step_graph_mode: StepGraphMode,
    step_graph_editor_open: bool,
    step_graph_ramp: Option<StepGraphRamp>,
    active_note_drag: Option<ActiveNoteDrag>,
    piano_roll_snap: PianoRollSnap,
    piano_roll_edit_scope: PianoRollEditScope,
    piano_roll_event_editor_open: bool,
    piano_roll_event_target: PianoRollEventTarget,
    piano_roll_scale: PianoRollScale,
    piano_roll_scale_root: u8,
    piano_roll_ghost_channels: bool,
    piano_roll_color_by_midi_channel: bool,
    piano_roll_paint_mode: bool,
    last_painted_note: Option<(u16, u16, u16, u32)>,
    quantize_strength_percent: u8,
    quantize_swing_percent: u8,
    chop_divisions: u8,
    strum_spread_ticks: u32,
    strum_descending: bool,
    flam_stroke_ticks: u32,
    flam_velocity: u8,
    flam_before: bool,
    randomizer_seed: u64,
    randomizer_velocity_amount: i16,
    randomizer_pan_amount: i16,
    randomizer_pitch_range: u8,
    randomizer_bipolar: bool,
    randomizer_reset_levels: bool,
    humanize_timing_range_ticks: u32,
    humanize_velocity_variation_percent: u8,
    note_limit_minimum_key: u8,
    note_limit_maximum_key: u8,
    arpeggiator_step_ticks: u32,
    arpeggiator_range_octaves: u8,
    arpeggiator_gate_percent: u8,
    arpeggiator_direction: ArpeggioDirection,
    piano_roll_slice_position_ticks: u32,
    pending_midi_import: Option<PendingMidiImport>,
    midi_channel_mapping: MidiChannelMapping,
    selected_arrangement: Option<u16>,
    selected_clip: Option<usize>,
    selected_time_marker: Option<usize>,
    selected_mixer_insert: Option<usize>,
    new_time_marker_position: u32,
    new_time_marker_is_signature: bool,
    new_time_marker_numerator: u8,
    new_time_marker_denominator: u8,
    new_time_marker_name: String,
    selected_plugin_state_channel: Option<u16>,
    selected_automation_channel: Option<u16>,
    selected_automation_point: Option<usize>,
    automation_add_mode: bool,
    automation_snap: bool,
    automation_visible_beats: f64,
    last_plugin_action: Option<String>,
    timeline_zoom: f32,
    plugin_candidates: Vec<PluginCandidate>,
    vst3_host: Option<Vst3HostRuntime>,
    channel_vst3_instances: BTreeMap<u16, u64>,
    audio_catalog: AudioDeviceCatalog,
    audio_settings: AudioSettings,
    audio_engine: Option<AudioEngine>,
    pending_audio_render: Option<PendingAudioRender>,
    audio_render_workers: Vec<PendingAudioRender>,
    pending_song_render: Option<PendingSongRender>,
    song_render_workers: Vec<PendingSongRender>,
    pending_vst3_stream: Option<Vst3PatternStreamHandle>,
    vst3_workers: Vec<Vst3PatternStreamHandle>,
    pending_sampler_stream: Option<PendingSamplerStream>,
    sampler_workers: Vec<PendingSamplerStream>,
    audio_waveform_paths: BTreeMap<u16, PathBuf>,
    audio_waveforms: BTreeMap<PathBuf, Arc<AudioWaveform>>,
    waveform_loads: BTreeMap<PathBuf, PendingWaveformLoad>,
    waveform_errors: BTreeMap<PathBuf, String>,
    browser_tab: BrowserTab,
    browser_filter: BrowserFilter,
    browser_path: PathBuf,
    browser_entries: Vec<BrowserEntry>,
    browser_index: Option<BrowserIndex>,
    pending_browser_index: Option<PendingBrowserIndex>,
    browser_index_after_pending: Option<(Vec<PathBuf>, bool)>,
    browser_search_roots: Vec<PathBuf>,
    browser_search_all_active: bool,
    browser_search: String,
    browser_selected: Option<PathBuf>,
    browser_favorites: BTreeSet<PathBuf>,
    browser_tags: BTreeMap<PathBuf, BTreeSet<String>>,
    browser_tag_editor: Option<BrowserTagEditor>,
    browser_tag_logic: BrowserTagLogic,
    browser_selected_tags: BTreeSet<String>,
    browser_saved_searches: Vec<SavedBrowserSearch>,
    browser_active_saved_search: Option<String>,
    browser_search_save_dialog: Option<BrowserSearchSaveDialog>,
    browser_tab_customize_dialog: Option<BrowserTabCustomizeDialog>,
    browser_fst_details: Option<FstPresetDetails>,
    browser_recent_projects: Vec<PathBuf>,
    browser_error: Option<String>,
    browser_full_sample: bool,
    browser_preview_volume: f32,
    browser_preview_pending: Option<PendingBrowserPreview>,
    browser_preview_queued: Option<(PathBuf, bool)>,
    browser_preview_cancelled: bool,
    browser_preview_path: Option<PathBuf>,
    browser_preview_error: Option<String>,
    audio_test_tone: bool,
    audio_monitor_input: bool,
    project_info_open: bool,
    project_info_title: String,
    project_info_author: String,
    project_info_comments: String,
    project_info_genre: String,
    project_info_web_link: String,
    project_settings_open: bool,
    project_settings_play_truncated: bool,
    project_settings_fast_declick: bool,
}

struct PendingAudioRender {
    receiver: Receiver<Result<PlaylistRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

struct PendingSongRender {
    receiver: Receiver<Result<PlaylistRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
    output_path: PathBuf,
}

struct PendingSamplerStream {
    receiver: Receiver<Result<SamplerPatternRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

struct PendingWaveformLoad {
    receiver: Receiver<Result<AudioWaveform, String>>,
    worker: thread::JoinHandle<()>,
}

impl DawUi {
    fn new(creation: &eframe::CreationContext<'_>, initial_project: Option<PathBuf>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = PANEL_DARK;
        visuals.faint_bg_color = PANEL_LIGHT;
        visuals.override_text_color = Some(TEXT);
        creation.egui_ctx.set_visuals(visuals);
        let plugin_candidates = scan_installed_plugins().candidates;
        let audio_catalog = enumerate_devices();
        let mut audio_settings = AudioSettings::default();
        if let Some(rate) = audio_catalog.default_sample_rate {
            audio_settings.sample_rate = rate;
        }
        let browser_path = initial_project
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(default_browser_directory);
        let mut app = Self {
            document: None,
            current_path: None,
            package_workspace: None,
            view: MainView::Playlist,
            status: "Open an FL Studio project to begin".to_owned(),
            dirty: false,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
            pending_history_snapshot: None,
            saved_project_hash: None,
            autosave_minutes: DEFAULT_AUTOSAVE_MINUTES,
            autosave_before_risky: false,
            backup_retention: DEFAULT_BACKUP_RETENTION,
            autosave_deadline: None,
            pending_backup_write: None,
            last_autosave_path: None,
            recovery_prompt: None,
            pending_project_change: None,
            close_approved: false,
            history_reset_during_frame: false,
            history_navigation_during_frame: false,
            playing: false,
            project_playback_loaded: false,
            playlist_playback_loaded: false,
            tempo_bpm: 140.0,
            selected_pattern: None,
            selected_note_channel: None,
            selected_note: None,
            selected_piano_notes: BTreeSet::new(),
            piano_roll_select_mode: false,
            piano_roll_selection_drag: None,
            piano_roll_zoom_mode: false,
            piano_roll_zoom_drag: None,
            piano_roll_playback_mode: false,
            piano_roll_stamp_mode: false,
            piano_roll_stamp_only_one: true,
            piano_roll_chord_stamp: PianoRollChordStamp::Major,
            last_piano_roll_audition: None,
            piano_roll_zoom: 0.10,
            piano_roll_pending_scroll_offset: None,
            piano_roll_grid_viewport: None,
            piano_roll_grid_scroll_offset: Vec2::ZERO,
            step_sequencer_bar: 0,
            selected_graph_channel: None,
            step_graph_mode: StepGraphMode::Velocity,
            step_graph_editor_open: false,
            step_graph_ramp: None,
            active_note_drag: None,
            piano_roll_snap: PianoRollSnap::QuarterBeat,
            piano_roll_edit_scope: PianoRollEditScope::Automatic,
            piano_roll_event_editor_open: false,
            piano_roll_event_target: PianoRollEventTarget::Velocity,
            piano_roll_scale: PianoRollScale::Major,
            piano_roll_scale_root: 0,
            piano_roll_ghost_channels: true,
            piano_roll_color_by_midi_channel: false,
            piano_roll_paint_mode: false,
            last_painted_note: None,
            quantize_strength_percent: 100,
            quantize_swing_percent: 0,
            chop_divisions: 4,
            strum_spread_ticks: 120,
            strum_descending: false,
            flam_stroke_ticks: 12,
            flam_velocity: 80,
            flam_before: true,
            randomizer_seed: 1,
            randomizer_velocity_amount: 0,
            randomizer_pan_amount: 0,
            randomizer_pitch_range: 0,
            randomizer_bipolar: true,
            randomizer_reset_levels: false,
            humanize_timing_range_ticks: 12,
            humanize_velocity_variation_percent: 10,
            note_limit_minimum_key: 36,
            note_limit_maximum_key: 83,
            arpeggiator_step_ticks: 24,
            arpeggiator_range_octaves: 1,
            arpeggiator_gate_percent: 80,
            arpeggiator_direction: ArpeggioDirection::Up,
            piano_roll_slice_position_ticks: 0,
            pending_midi_import: None,
            midi_channel_mapping: MidiChannelMapping::PreserveNoteChannels,
            selected_arrangement: None,
            selected_clip: None,
            selected_time_marker: None,
            selected_mixer_insert: None,
            new_time_marker_position: 0,
            new_time_marker_is_signature: false,
            new_time_marker_numerator: 4,
            new_time_marker_denominator: 4,
            new_time_marker_name: String::new(),
            selected_plugin_state_channel: None,
            selected_automation_channel: None,
            selected_automation_point: None,
            automation_add_mode: false,
            automation_snap: true,
            automation_visible_beats: 16.0,
            last_plugin_action: None,
            timeline_zoom: 0.10,
            plugin_candidates,
            vst3_host: None,
            channel_vst3_instances: BTreeMap::new(),
            audio_catalog,
            audio_settings,
            audio_engine: None,
            pending_audio_render: None,
            audio_render_workers: Vec::new(),
            pending_song_render: None,
            song_render_workers: Vec::new(),
            pending_vst3_stream: None,
            vst3_workers: Vec::new(),
            pending_sampler_stream: None,
            sampler_workers: Vec::new(),
            audio_waveform_paths: BTreeMap::new(),
            audio_waveforms: BTreeMap::new(),
            waveform_loads: BTreeMap::new(),
            waveform_errors: BTreeMap::new(),
            browser_tab: BrowserTab::Files,
            browser_filter: BrowserFilter::All,
            browser_path,
            browser_entries: Vec::new(),
            browser_index: None,
            pending_browser_index: None,
            browser_index_after_pending: None,
            browser_search_roots: load_browser_search_roots(),
            browser_search_all_active: false,
            browser_search: String::new(),
            browser_selected: None,
            browser_favorites: load_browser_favorites(),
            browser_tags: load_browser_tags(),
            browser_tag_editor: None,
            browser_tag_logic: BrowserTagLogic::default(),
            browser_selected_tags: BTreeSet::new(),
            browser_saved_searches: load_browser_saved_searches(),
            browser_active_saved_search: None,
            browser_search_save_dialog: None,
            browser_tab_customize_dialog: None,
            browser_fst_details: None,
            browser_recent_projects: load_recent_projects(),
            browser_error: None,
            browser_full_sample: false,
            browser_preview_volume: 1.0,
            browser_preview_pending: None,
            browser_preview_queued: None,
            browser_preview_cancelled: false,
            browser_preview_path: None,
            browser_preview_error: None,
            audio_test_tone: false,
            audio_monitor_input: false,
            project_info_open: false,
            project_info_title: String::new(),
            project_info_author: String::new(),
            project_info_comments: String::new(),
            project_info_genre: String::new(),
            project_info_web_link: String::new(),
            project_settings_open: false,
            project_settings_play_truncated: false,
            project_settings_fast_declick: false,
        };
        if let Some((autosave_minutes, autosave_before_risky, backup_retention)) =
            load_autosave_settings()
        {
            app.autosave_minutes = autosave_minutes;
            app.autosave_before_risky = autosave_before_risky;
            app.backup_retention = backup_retention;
        }
        app.refresh_browser_directory();
        if let Some(path) = initial_project.as_deref() {
            app.open_project(path);
        } else if let Some(prompt) = latest_recovery_prompt() {
            app.last_autosave_path = Some(prompt.backup_path.clone());
            app.recovery_prompt = Some(prompt);
        }
        app
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open FL Studio project")
            .add_filter("FL Studio project", &["flp", "zip"])
            .pick_file()
        {
            self.open_project(&path);
        }
    }

    fn open_project(&mut self, path: &Path) {
        if self.dirty {
            self.recovery_prompt = None;
            self.pending_project_change = Some(PendingProjectChange::Open(path.to_path_buf()));
        } else {
            self.load_project_from(path, path, true);
        }
    }

    fn load_project_from(
        &mut self,
        project_file: &Path,
        target_path: &Path,
        check_recovery: bool,
    ) -> bool {
        self.recovery_prompt = None;
        self.pending_project_change = None;
        let package_result = if has_extension(project_file, "zip") {
            ProjectPackageWorkspace::open(project_file)
                .map(|(workspace, bytes)| (Some(workspace), bytes))
        } else {
            let workspace = if has_extension(target_path, "zip") {
                if self
                    .current_path
                    .as_deref()
                    .is_some_and(|current| project_paths_equal(current, target_path))
                {
                    self.package_workspace.clone().map(Ok).unwrap_or_else(|| {
                        ProjectPackageWorkspace::open(target_path).map(|(workspace, _)| workspace)
                    })
                } else {
                    ProjectPackageWorkspace::open(target_path).map(|(workspace, _)| workspace)
                }
                .map(Some)
            } else {
                Ok(None)
            };
            workspace.and_then(|workspace| {
                fs::read(project_file)
                    .map(|bytes| (workspace, bytes))
                    .map_err(|error| format!("could not read {}: {error}", project_file.display()))
            })
        };
        match package_result.and_then(|(workspace, bytes)| {
            FlpDocument::parse(&bytes)
                .map(|document| (workspace, document))
                .map_err(|error| error.to_string())
        }) {
            Ok((package_workspace, document)) => {
                self.stop_project_playback();
                self.clear_history();
                self.history_reset_during_frame = true;
                self.autosave_deadline = None;
                self.saved_project_hash = project_hash(&document);
                self.tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
                self.project_info_title =
                    document.metadata().title().unwrap_or_default().to_owned();
                self.project_info_author =
                    document.metadata().author().unwrap_or_default().to_owned();
                self.project_info_comments = document
                    .metadata()
                    .comments()
                    .unwrap_or_default()
                    .to_owned();
                self.project_info_genre =
                    document.metadata().genre().unwrap_or_default().to_owned();
                self.project_info_web_link = document
                    .metadata()
                    .web_link()
                    .unwrap_or_default()
                    .to_owned();
                if let Some(settings) = document.project_settings() {
                    self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
                    self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
                } else {
                    self.project_settings_play_truncated = false;
                    self.project_settings_fast_declick = false;
                }
                self.selected_pattern = document
                    .patterns()
                    .ok()
                    .and_then(|patterns| patterns.first().map(|pattern| pattern.id));
                self.selected_note_channel =
                    document.channels().first().map(|channel| channel.id());
                self.selected_graph_channel =
                    document.channels().first().map(|channel| channel.id());
                self.selected_clip = None;
                self.selected_time_marker = None;
                self.selected_note = None;
                self.selected_piano_notes.clear();
                self.piano_roll_selection_drag = None;
                self.active_note_drag = None;
                self.pending_midi_import = None;
                self.selected_plugin_state_channel = document
                    .channel_plugin_states()
                    .first()
                    .map(|state| state.channel_id());
                self.selected_automation_channel = document
                    .automation_channels()
                    .ok()
                    .and_then(|channels| channels.first().map(AutomationChannel::channel_id));
                self.selected_automation_point = None;
                self.automation_add_mode = false;
                self.last_plugin_action = None;
                self.vst3_host = None;
                self.channel_vst3_instances.clear();
                self.selected_arrangement = document.arrangements().ok().and_then(|arrangements| {
                    arrangements.first().map(|arrangement| arrangement.id)
                });
                self.current_path = Some(target_path.to_path_buf());
                self.package_workspace = package_workspace;
                self.document = Some(document);
                self.refresh_audio_waveform_paths();
                let recent_path =
                    fs::canonicalize(target_path).unwrap_or_else(|_| target_path.to_path_buf());
                self.browser_recent_projects
                    .retain(|recent| recent != &recent_path);
                self.browser_recent_projects.insert(0, recent_path);
                self.browser_recent_projects.truncate(RECENT_PROJECT_LIMIT);
                let recent_project_save_error =
                    save_recent_projects(&self.browser_recent_projects).err();
                self.dirty = false;
                self.last_autosave_path = latest_project_autosave(target_path);
                if check_recovery {
                    self.recovery_prompt = recovery_prompt_for_project(target_path);
                }
                let plugin_summary = self
                    .load_project_vst3_channels()
                    .map_or_else(String::new, |summary| format!(" · {summary}"));
                if check_recovery {
                    self.status = format!("Opened {}{plugin_summary}", target_path.display());
                } else {
                    self.status = format!(
                        "Recovered {} from autosave{plugin_summary}",
                        target_path.display()
                    );
                }
                if let Some(error) = recent_project_save_error {
                    self.status.push_str(&format!(
                        " · recent project list could not be saved: {error}"
                    ));
                }
                true
            }
            Err(error) => {
                self.status = format!("Could not open project: {error}");
                false
            }
        }
    }

    fn refresh_audio_waveform_paths(&mut self) {
        let Some(project_path) = self.sample_project_path() else {
            self.audio_waveform_paths.clear();
            return;
        };
        let channels = self
            .document
            .as_ref()
            .map(FlpDocument::channels)
            .unwrap_or_default();
        let resolver = SamplePathResolver::new(project_path);
        self.audio_waveform_paths = channels
            .iter()
            .filter(|channel| channel.kind() == Some(4))
            .filter_map(|channel| {
                let sample_path = channel.sample_path()?;
                let resolved = resolver.resolve(sample_path).ok()?;
                Some((channel.id(), resolved))
            })
            .collect();
    }

    fn sample_project_path(&self) -> Option<PathBuf> {
        self.package_workspace
            .as_ref()
            .map(|workspace| workspace.sample_project_path().to_path_buf())
            .or_else(|| self.current_path.clone())
    }

    fn request_audio_waveform(&mut self, path: &Path) {
        if self.audio_waveforms.contains_key(path)
            || self.waveform_loads.contains_key(path)
            || self.waveform_errors.contains_key(path)
            || self.waveform_loads.len() >= MAX_WAVEFORM_WORKERS
        {
            return;
        }
        let path = path.to_path_buf();
        let worker_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = match thread::Builder::new()
            .name("flp-audio-waveform".to_owned())
            .spawn(move || {
                let result = decode_audio_waveform(&worker_path, AUDIO_WAVEFORM_BUCKETS);
                let _ = sender.send(result);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.waveform_errors.insert(path, error.to_string());
                return;
            }
        };
        self.waveform_loads
            .insert(path, PendingWaveformLoad { receiver, worker });
    }

    fn poll_audio_waveforms(&mut self, ctx: &egui::Context) {
        let completed = self
            .waveform_loads
            .iter()
            .filter_map(|(path, load)| match load.receiver.try_recv() {
                Ok(result) => Some((path.clone(), result)),
                Err(TryRecvError::Disconnected) => Some((
                    path.clone(),
                    Err("waveform preview worker stopped unexpectedly".to_owned()),
                )),
                Err(TryRecvError::Empty) => None,
            })
            .collect::<Vec<_>>();
        for (path, result) in completed {
            if let Some(load) = self.waveform_loads.remove(&path) {
                let _ = load.worker.join();
            }
            match result {
                Ok(waveform) => {
                    self.waveform_errors.remove(&path);
                    self.audio_waveforms.insert(path, Arc::new(waveform));
                }
                Err(error) => {
                    self.waveform_errors.insert(path, error);
                }
            }
        }
        if !self.waveform_loads.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }

    fn load_project_vst3_channels(&mut self) -> Option<String> {
        let states = self
            .document
            .as_ref()
            .map(FlpDocument::channel_plugin_states)
            .unwrap_or_default();
        let mut eligible_channels = 0usize;
        let mut unresolved = Vec::new();
        let mut plans = Vec::new();
        for state in states {
            let Some(metadata) = state.vst_metadata() else {
                continue;
            };
            if metadata.fourcc().is_some() {
                continue;
            }
            let Some(class_uid) = metadata.class_uid() else {
                continue;
            };
            eligible_channels += 1;
            if let Some(candidate) = matching_vst3_candidate(&self.plugin_candidates, metadata) {
                plans.push((state, candidate.clone(), class_uid));
            } else {
                unresolved.push(format!(
                    "channel {} has no installed match",
                    state.channel_id()
                ));
            }
        }
        if eligible_channels == 0 {
            return None;
        }
        if plans.is_empty() {
            return Some(format!(
                "no installed VST3 matched {} project channel(s)",
                eligible_channels
            ));
        }

        let mut host = match Vst3HostRuntime::new(f64::from(self.audio_settings.sample_rate), 512) {
            Ok(host) => host,
            Err(error) => {
                return Some(format!("VST3 host initialization failed: {error}"));
            }
        };
        let mut channel_instances = BTreeMap::new();
        let mut restored_count = 0usize;
        for (state, candidate, class_uid) in plans {
            let channel_id = state.channel_id();
            match host.load(&candidate.path, Some(&class_uid)) {
                Ok(info) => {
                    channel_instances.insert(channel_id, info.id);
                    match host.restore_flp_channel_state(info.id, &state) {
                        Ok(()) => restored_count += 1,
                        Err(error) => unresolved.push(format!(
                            "channel {channel_id} state could not be restored: {error}"
                        )),
                    }
                }
                Err(error) => unresolved.push(format!(
                    "channel {channel_id} plug-in could not load: {error}"
                )),
            }
        }
        let loaded_count = channel_instances.len();
        self.channel_vst3_instances = channel_instances;
        if !host.loaded_plugins().is_empty() {
            self.vst3_host = Some(host);
        }

        if unresolved.is_empty() {
            Some(format!(
                "auto-loaded {loaded_count} project VST3 channel(s); restored {restored_count} state(s)"
            ))
        } else {
            Some(format!(
                "auto-loaded {loaded_count}/{eligible_channels} project VST3 channel(s); restored {restored_count} state(s); {} issue(s) ({})",
                unresolved.len(),
                unresolved
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        }
    }

    fn save(&mut self) {
        if let Some(path) = self.current_path.clone() {
            self.write_project(&path);
        } else {
            self.save_as();
        }
    }

    fn save_as(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Save FL Studio project")
            .add_filter("FL Studio project", &["flp", "zip"])
            .save_file()
        {
            self.write_project(&path);
        }
    }

    fn save_new_version(&mut self) {
        let Some(path) = self.current_path.clone() else {
            self.save_as();
            return;
        };
        self.write_project(&next_project_version_path(&path));
    }

    fn backup_now(&mut self) {
        if self.pending_backup_write.is_some() {
            self.status = "A backup is already being written".to_owned();
            return;
        }
        self.start_backup_write(BackupPurpose::Manual);
    }

    fn revert_to_last_autosave(&mut self) {
        let (Some(source_path), Some(backup_path)) =
            (self.current_path.clone(), self.last_autosave_path.clone())
        else {
            self.status = "No autosave is available for this project".to_owned();
            return;
        };
        if !backup_path.is_file() {
            self.last_autosave_path = None;
            self.status = "The last autosave is no longer available".to_owned();
            return;
        }
        self.recovery_prompt = Some(RecoveryPrompt {
            source_path,
            backup_path,
            is_revert: true,
        });
    }

    fn persist_autosave_settings(&mut self) {
        self.autosave_deadline = None;
        if let Err(error) = save_autosave_settings(
            self.autosave_minutes,
            self.autosave_before_risky,
            self.backup_retention,
        ) {
            self.status = format!("Could not save autosave settings: {error}");
        }
    }

    fn backup_before_risky_operation(&mut self, operation: &str) {
        if !self.autosave_before_risky {
            return;
        }
        let (Some(source_path), Some(document)) =
            (self.current_path.clone(), self.document.as_ref())
        else {
            return;
        };
        let result = document
            .encode_lossless()
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                write_project_backup(
                    &source_path,
                    &bytes,
                    BackupPurpose::Autosave,
                    self.backup_retention,
                )
            });
        match result {
            Ok(path) => {
                self.last_autosave_path = Some(path.clone());
                self.status = format!("Autosaved before {operation}");
            }
            Err(error) => self.status = format!("Could not autosave before {operation}: {error}"),
        }
    }

    fn start_backup_write(&mut self, purpose: BackupPurpose) {
        let (Some(source_path), Some(document)) =
            (self.current_path.clone(), self.document.as_ref())
        else {
            self.status = "Save the project before creating a backup".to_owned();
            return;
        };
        let bytes = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not encode autosave: {error}");
                return;
            }
        };
        let retention = self.backup_retention;
        let worker_source = source_path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = match thread::Builder::new()
            .name("flp-autosave".to_owned())
            .spawn(move || {
                let result = write_project_backup(&worker_source, &bytes, purpose, retention);
                let _ = sender.send(result);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.status = format!("Could not start backup writer: {error}");
                self.schedule_next_autosave();
                return;
            }
        };
        self.pending_backup_write = Some(PendingBackupWrite {
            source_path,
            purpose,
            receiver,
            worker,
        });
        if matches!(purpose, BackupPurpose::Autosave) {
            self.schedule_next_autosave();
        }
    }

    fn poll_backup_write(&mut self) {
        let result = self.pending_backup_write.as_ref().and_then(|pending| {
            match pending.receiver.try_recv() {
                Ok(result) => Some((pending.source_path.clone(), pending.purpose, result)),
                Err(TryRecvError::Disconnected) => Some((
                    pending.source_path.clone(),
                    pending.purpose,
                    Err("backup writer stopped unexpectedly".to_owned()),
                )),
                Err(TryRecvError::Empty) => None,
            }
        });
        let Some((source_path, purpose, result)) = result else {
            return;
        };
        if let Some(pending) = self.pending_backup_write.take() {
            let _ = pending.worker.join();
        }
        match (purpose, result) {
            (BackupPurpose::Autosave, Ok(path)) => {
                let is_current_project = self
                    .current_path
                    .as_deref()
                    .is_some_and(|current_path| project_paths_equal(current_path, &source_path));
                if is_current_project {
                    self.last_autosave_path = Some(path.clone());
                    self.status = format!("Autosaved to {}", path.display());
                }
            }
            (BackupPurpose::Manual, Ok(path)) => {
                self.status = format!("Backup created at {}", path.display());
            }
            (_, Err(error)) => self.status = format!("Could not write backup: {error}"),
        }
    }

    fn schedule_next_autosave(&mut self) {
        self.autosave_deadline = (self.autosave_minutes > 0)
            .then(|| Instant::now() + Duration::from_secs(u64::from(self.autosave_minutes) * 60));
    }

    fn advance_autosave(&mut self, context: &egui::Context) {
        self.poll_backup_write();
        if self.pending_backup_write.is_some() {
            context.request_repaint_after(Duration::from_millis(500));
            return;
        }
        if self.autosave_minutes == 0 || !self.dirty || self.current_path.is_none() {
            self.autosave_deadline = None;
            return;
        }
        if self.autosave_deadline.is_none() {
            self.schedule_next_autosave();
        }
        if self.playing && !self.autosave_before_risky {
            context.request_repaint_after(Duration::from_millis(500));
            return;
        }
        let Some(deadline) = self.autosave_deadline else {
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            self.start_backup_write(BackupPurpose::Autosave);
        } else {
            context.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    fn recovery_prompt_dialog(&mut self, context: &egui::Context) {
        let Some(prompt) = self.recovery_prompt.clone() else {
            return;
        };
        let mut open = true;
        let mut recover = false;
        let mut dismiss = false;
        egui::Window::new(if prompt.is_revert {
            "Revert to autosave?"
        } else {
            "Recover project?"
        })
        .id(Id::new("project-recovery-dialog"))
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(context, |ui| {
            if prompt.is_revert {
                ui.label("Replace the current project with its latest autosave?");
            } else {
                ui.label("A newer autosave is available for this project.");
                ui.label(prompt.source_path.display().to_string());
                ui.label("Recover the autosaved version?");
            }
            ui.horizontal(|ui| {
                if ui
                    .button(if prompt.is_revert {
                        "Revert"
                    } else {
                        "Recover"
                    })
                    .clicked()
                {
                    recover = true;
                }
                if ui
                    .button(if prompt.is_revert {
                        "Cancel"
                    } else {
                        "Discard"
                    })
                    .clicked()
                {
                    dismiss = true;
                }
            });
        });
        if recover {
            self.recover_from_backup(&prompt);
        } else if dismiss {
            self.recovery_prompt = None;
            if !prompt.is_revert {
                let _ = fs::remove_file(&prompt.backup_path);
                if self.last_autosave_path.as_ref() == Some(&prompt.backup_path) {
                    self.last_autosave_path = None;
                }
            }
        } else if !open {
            self.recovery_prompt = None;
        }
    }

    fn guard_window_close(&mut self, context: &egui::Context) {
        if !context.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.close_approved {
            self.close_approved = false;
            return;
        }
        if self.pending_project_change.is_some() || self.dirty {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.dirty {
            self.recovery_prompt = None;
            self.pending_project_change = Some(PendingProjectChange::Exit);
        }
    }

    fn unsaved_changes_dialog(&mut self, context: &egui::Context) {
        let Some(change) = self.pending_project_change.clone() else {
            return;
        };
        let (title, message) = match &change {
            PendingProjectChange::Open(path) => (
                "Save changes before opening another project?",
                format!(
                    "Save changes to the current project before opening {}?",
                    path.display()
                ),
            ),
            PendingProjectChange::Exit => (
                "Save changes before closing?",
                "Save changes to the current project before closing FL Studio Rebuild?".to_owned(),
            ),
        };
        let mut open = true;
        let mut action = None;
        egui::Window::new(title)
            .id(Id::new("unsaved-project-changes-dialog"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(context, |ui| {
                ui.label(message);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        action = Some(UnsavedChangeChoice::Save);
                    }
                    if ui.button("Don't Save").clicked() {
                        action = Some(UnsavedChangeChoice::Discard);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(UnsavedChangeChoice::Cancel);
                    }
                });
            });
        match action {
            Some(UnsavedChangeChoice::Save) => self.save_then_continue_project_change(context),
            Some(UnsavedChangeChoice::Discard) => {
                self.discard_then_continue_project_change(context)
            }
            Some(UnsavedChangeChoice::Cancel) => self.cancel_project_change(),
            None if !open => self.cancel_project_change(),
            None => {}
        }
    }

    fn cancel_project_change(&mut self) {
        self.pending_project_change = None;
    }

    fn save_then_continue_project_change(&mut self, context: &egui::Context) {
        if self.dirty {
            self.save();
            if self.dirty {
                return;
            }
        }
        self.perform_pending_project_change(context);
    }

    fn discard_then_continue_project_change(&mut self, context: &egui::Context) {
        self.perform_pending_project_change(context);
    }

    fn perform_pending_project_change(&mut self, context: &egui::Context) {
        let Some(change) = self.pending_project_change.take() else {
            return;
        };
        match change {
            PendingProjectChange::Open(path) => {
                self.load_project_from(&path, &path, true);
            }
            PendingProjectChange::Exit => {
                self.close_approved = true;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn recover_from_backup(&mut self, prompt: &RecoveryPrompt) {
        let original_hash = fs::read(&prompt.source_path)
            .ok()
            .and_then(|bytes| FlpDocument::parse(&bytes).ok())
            .and_then(|document| project_hash(&document));
        if self.load_project_from(&prompt.backup_path, &prompt.source_path, false) {
            self.saved_project_hash = original_hash;
            self.dirty = true;
            self.last_autosave_path = Some(prompt.backup_path.clone());
            self.status = format!("Recovered autosave for {}", prompt.source_path.display());
        }
    }

    fn open_project_info(&mut self) {
        if let Some(metadata) = self.document.as_ref().map(FlpDocument::metadata) {
            self.project_info_title = metadata.title().unwrap_or_default().to_owned();
            self.project_info_author = metadata.author().unwrap_or_default().to_owned();
            self.project_info_comments = metadata.comments().unwrap_or_default().to_owned();
            self.project_info_genre = metadata.genre().unwrap_or_default().to_owned();
            self.project_info_web_link = metadata.web_link().unwrap_or_default().to_owned();
            self.project_info_open = true;
        }
    }

    fn apply_project_info(&mut self) {
        let edit = ProjectInfoEdit {
            title: Some(self.project_info_title.clone()),
            author: Some(self.project_info_author.clone()),
            comments: Some(self.project_info_comments.clone()),
            genre: Some(self.project_info_genre.clone()),
            web_link: Some(self.project_info_web_link.clone()),
        };
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before editing Project Info".to_owned();
            return;
        };
        match document.set_project_info(edit) {
            Ok(()) => {
                self.dirty = true;
                self.project_info_open = false;
                self.status = "Project Info updated".to_owned();
            }
            Err(error) => self.status = format!("Could not update Project Info: {error}"),
        }
    }

    fn project_info_dialog(&mut self, context: &egui::Context) {
        if !self.project_info_open {
            return;
        }
        let mut apply = false;
        let mut cancel = false;
        let mut open = self.project_info_open;
        egui::Window::new("Project Info")
            .id(Id::new("project-info-dialog"))
            .open(&mut open)
            .resizable(true)
            .default_width(500.0)
            .show(context, |ui| {
                ui.label("Title");
                ui.text_edit_singleline(&mut self.project_info_title);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label("Author");
                        ui.text_edit_singleline(&mut self.project_info_author);
                    });
                    ui.vertical(|ui| {
                        ui.label("Genre");
                        egui::ComboBox::from_id_salt("project-info-genre")
                            .selected_text(if self.project_info_genre.is_empty() {
                                "(none)"
                            } else {
                                &self.project_info_genre
                            })
                            .show_ui(ui, |ui| {
                                for genre in FL_GENRES {
                                    let value = if *genre == "(none)" {
                                        String::new()
                                    } else {
                                        (*genre).to_owned()
                                    };
                                    ui.selectable_value(
                                        &mut self.project_info_genre,
                                        value,
                                        *genre,
                                    );
                                }
                            });
                    });
                });
                ui.label("Comments");
                ui.add(
                    egui::TextEdit::multiline(&mut self.project_info_comments)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                ui.label("Web link");
                ui.text_edit_singleline(&mut self.project_info_web_link);
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        self.project_info_open = open && !cancel;
        if apply {
            self.apply_project_info();
        }
    }

    fn open_project_settings(&mut self) {
        let settings = self
            .document
            .as_ref()
            .and_then(FlpDocument::project_settings);
        if let Some(settings) = settings {
            self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
            self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
            self.project_settings_open = true;
        } else {
            self.status = "This project's settings block is not recognized yet".to_owned();
        }
    }

    fn apply_project_settings(&mut self) {
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before editing Project settings".to_owned();
            return;
        };
        match document.set_project_settings(ProjectSettingsEdit {
            play_truncated_notes_in_clips: Some(self.project_settings_play_truncated),
            fast_declick_for_cut_groups: Some(self.project_settings_fast_declick),
        }) {
            Ok(()) => {
                self.dirty = true;
                self.project_settings_open = false;
                self.status = "Project settings updated".to_owned();
            }
            Err(error) => self.status = format!("Could not update Project settings: {error}"),
        }
    }

    fn create_pattern(&mut self) {
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before creating a pattern".to_owned();
            return;
        };
        match document.create_pattern() {
            Ok(pattern_id) => {
                self.selected_pattern = Some(pattern_id);
                self.dirty = true;
                self.status = format!("Created empty pattern {pattern_id}");
            }
            Err(error) => self.status = format!("Could not create a pattern: {error}"),
        }
    }

    fn project_settings_dialog(&mut self, context: &egui::Context) {
        if !self.project_settings_open {
            return;
        }
        let mut apply = false;
        let mut cancel = false;
        let mut open = self.project_settings_open;
        egui::Window::new("Project settings")
            .id(Id::new("project-settings-dialog"))
            .open(&mut open)
            .resizable(false)
            .default_width(360.0)
            .show(context, |ui| {
                ui.checkbox(
                    &mut self.project_settings_play_truncated,
                    "Play truncated notes in clips",
                );
                ui.checkbox(
                    &mut self.project_settings_fast_declick,
                    "Fast declick for cut groups",
                );
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        self.project_settings_open = open && !cancel;
        if apply {
            self.apply_project_settings();
        }
    }

    fn clear_history(&mut self) {
        self.undo_history.clear();
        self.redo_history.clear();
        self.pending_history_snapshot = None;
    }

    fn history_bytes(&self) -> usize {
        self.undo_history
            .iter()
            .chain(&self.redo_history)
            .map(Vec::len)
            .fold(0usize, usize::saturating_add)
    }

    fn trim_history_to_limit(&mut self) {
        while self.history_bytes() > HISTORY_LIMIT_BYTES {
            if !self.undo_history.is_empty() {
                self.undo_history.remove(0);
            } else if !self.redo_history.is_empty() {
                self.redo_history.remove(0);
            } else {
                break;
            }
        }
    }

    fn remember_undo_snapshot(&mut self, snapshot: Vec<u8>) {
        if snapshot.len() > HISTORY_LIMIT_BYTES {
            self.status = "Undo snapshot exceeds the 64 MiB history limit".to_owned();
            self.redo_history.clear();
            return;
        }
        self.redo_history.clear();
        self.undo_history.push(snapshot);
        self.trim_history_to_limit();
    }

    fn finish_history_frame(
        &mut self,
        frame_snapshot: Option<Vec<u8>>,
        pointer_down: bool,
        mut history_navigation: bool,
    ) {
        history_navigation |= self.history_navigation_during_frame;
        self.history_navigation_during_frame = false;
        if history_navigation {
            self.pending_history_snapshot = None;
            self.history_reset_during_frame = false;
            return;
        }
        if self.history_reset_during_frame {
            self.clear_history();
            self.history_reset_during_frame = false;
            return;
        }
        if pointer_down {
            if self.pending_history_snapshot.is_none() {
                self.pending_history_snapshot = frame_snapshot;
            }
            return;
        }
        let Some(snapshot) = self.pending_history_snapshot.take().or(frame_snapshot) else {
            return;
        };
        let Some(document) = &self.document else {
            return;
        };
        match document.encode_lossless() {
            Ok(current) if current != snapshot => self.remember_undo_snapshot(snapshot),
            Ok(_) => {}
            Err(error) => {
                self.status = format!("Could not capture undo state: {error}");
            }
        }
    }

    fn undo_document(&mut self) {
        self.history_navigation_during_frame = true;
        let Some(snapshot) = self.undo_history.pop() else {
            self.status = "Nothing to undo".to_owned();
            return;
        };
        let Some(current) = self
            .document
            .as_ref()
            .and_then(|document| document.encode_lossless().ok())
        else {
            self.undo_history.push(snapshot);
            self.status = "Could not capture the current project for redo".to_owned();
            return;
        };
        let restored = match FlpDocument::parse(&snapshot) {
            Ok(document) => document,
            Err(error) => {
                self.undo_history.push(snapshot);
                self.status = format!("Could not restore undo state: {error}");
                return;
            }
        };
        self.redo_history.push(current);
        self.trim_history_to_limit();
        self.document = Some(restored);
        self.refresh_after_history_navigation("Undo");
    }

    fn redo_document(&mut self) {
        self.history_navigation_during_frame = true;
        let Some(snapshot) = self.redo_history.pop() else {
            self.status = "Nothing to redo".to_owned();
            return;
        };
        let Some(current) = self
            .document
            .as_ref()
            .and_then(|document| document.encode_lossless().ok())
        else {
            self.redo_history.push(snapshot);
            self.status = "Could not capture the current project for undo".to_owned();
            return;
        };
        let restored = match FlpDocument::parse(&snapshot) {
            Ok(document) => document,
            Err(error) => {
                self.redo_history.push(snapshot);
                self.status = format!("Could not restore redo state: {error}");
                return;
            }
        };
        self.undo_history.push(current);
        self.trim_history_to_limit();
        self.document = Some(restored);
        self.refresh_after_history_navigation("Redo");
    }

    fn refresh_after_history_navigation(&mut self, action: &str) {
        self.stop_project_playback();
        if let Some(document) = &self.document {
            self.tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
            self.project_info_title = document.metadata().title().unwrap_or_default().to_owned();
            self.project_info_author = document.metadata().author().unwrap_or_default().to_owned();
            self.project_info_comments = document
                .metadata()
                .comments()
                .unwrap_or_default()
                .to_owned();
            self.project_info_genre = document.metadata().genre().unwrap_or_default().to_owned();
            self.project_info_web_link = document
                .metadata()
                .web_link()
                .unwrap_or_default()
                .to_owned();
            if let Some(settings) = document.project_settings() {
                self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
                self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
            }
            let patterns = document.patterns().unwrap_or_default();
            if self
                .selected_pattern
                .is_none_or(|id| !patterns.iter().any(|pattern| pattern.id == id))
            {
                self.selected_pattern = patterns.first().map(|pattern| pattern.id);
            }
            let channels = document.channels();
            if self
                .selected_note_channel
                .is_none_or(|id| !channels.iter().any(|channel| channel.id() == id))
            {
                self.selected_note_channel = channels.first().map(|channel| channel.id());
            }
            let arrangements = document.arrangements().unwrap_or_default();
            if self
                .selected_arrangement
                .is_none_or(|id| !arrangements.iter().any(|arrangement| arrangement.id == id))
            {
                self.selected_arrangement = arrangements.first().map(|arrangement| arrangement.id);
            }
            self.selected_automation_channel = document
                .automation_channels()
                .ok()
                .and_then(|items| items.first().map(AutomationChannel::channel_id));
        }
        self.selected_note = None;
        self.selected_clip = None;
        self.selected_time_marker = None;
        self.selected_automation_point = None;
        self.active_note_drag = None;
        self.selected_piano_notes.clear();
        self.piano_roll_selection_drag = None;
        self.dirty = self
            .document
            .as_ref()
            .and_then(project_hash)
            .zip(self.saved_project_hash)
            .is_none_or(|(current, saved)| current != saved);
        self.status = format!("{action} project edit");
    }

    fn write_project(&mut self, path: &Path) {
        let Some(document) = &self.document else {
            self.status = "Open a project before saving".to_owned();
            return;
        };
        let encoded = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not encode project: {error}");
                return;
            }
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        encoded.hash(&mut hasher);
        let saved_hash = hasher.finish();
        let backups = self.backup_before_manual_save(path);
        let mut package_workspace_after_save = None;
        let new_zip_has_only_project =
            has_extension(path, "zip") && self.package_workspace.is_none();
        let save_result = if has_extension(path, "zip") {
            let workspace = if let Some(workspace) = self.package_workspace.as_ref() {
                Ok(workspace.clone())
            } else {
                let project_name = path
                    .file_stem()
                    .map(|stem| format!("{}.flp", stem.to_string_lossy()))
                    .unwrap_or_else(|| "Project.flp".to_owned());
                let sample_project_path = self
                    .current_path
                    .clone()
                    .unwrap_or_else(|| path.to_path_buf());
                ProjectPackageWorkspace::single_project(
                    &project_name,
                    &encoded,
                    sample_project_path,
                )
            };
            workspace.and_then(|workspace| {
                workspace
                    .write_to(path, &encoded)
                    .map(|()| package_workspace_after_save = Some(workspace))
            })
        } else {
            fs::write(path, encoded).map_err(|error| error.to_string())
        };
        match save_result {
            Ok(()) => {
                self.current_path = Some(path.to_path_buf());
                if let Some(workspace) = package_workspace_after_save {
                    self.package_workspace = Some(workspace);
                }
                self.refresh_audio_waveform_paths();
                self.saved_project_hash = Some(saved_hash);
                self.dirty = false;
                self.autosave_deadline = None;
                self.last_autosave_path = latest_project_autosave(path);
                self.status = match backups {
                    Ok(paths) if !paths.is_empty() => {
                        format!("Saved {} · backup {}", path.display(), paths[0].display())
                    }
                    Err(error) => format!(
                        "Saved {} · previous version backup failed: {error}",
                        path.display()
                    ),
                    _ => format!("Saved {}", path.display()),
                };
                if new_zip_has_only_project {
                    self.status.push_str(
                        " · this ZIP contains the FLP only; samples from a plain FLP are not bundled yet",
                    );
                }
            }
            Err(error) => {
                self.status = match backups {
                    Err(backup_error) => format!(
                        "Could not save project: {error}; previous version backup failed: {backup_error}"
                    ),
                    _ => format!("Could not save project: {error}"),
                }
            }
        }
    }

    fn backup_before_manual_save(&self, target_path: &Path) -> Result<Vec<PathBuf>, String> {
        let mut paths = Vec::new();
        if let Some(current_path) = self.current_path.as_deref() {
            paths.push(current_path.to_path_buf());
        }
        if !paths
            .iter()
            .any(|existing| project_paths_equal(existing, target_path))
        {
            paths.push(target_path.to_path_buf());
        }
        let mut backups = Vec::new();
        for path in paths.into_iter().filter(|path| path.is_file()) {
            let bytes = fs::read(&path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            backups.push(write_project_backup(
                &path,
                &bytes,
                BackupPurpose::Manual,
                self.backup_retention,
            )?);
        }
        Ok(backups)
    }

    fn update_tempo(&mut self, bpm: f64) {
        if !bpm.is_finite() || bpm <= 0.0 {
            return;
        }
        let milli_bpm = (bpm * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32;
        let changed = self
            .document
            .as_mut()
            .is_some_and(|document| document.set_tempo_milli_bpm(milli_bpm).is_ok());
        if changed {
            self.stop_project_playback();
            self.dirty = true;
            self.status = format!("Tempo set to {:.3} BPM", f64::from(milli_bpm) / 1000.0);
        }
    }

    fn start_project_playback(&mut self) {
        if self.pending_song_render.is_some() {
            self.status = "Stop the Playlist render before starting playback".to_owned();
            return;
        }
        if self.pending_audio_render.is_some() {
            self.status = "Project audio is already being prepared".to_owned();
            return;
        }
        if self.document.is_none() {
            self.status = "Open an FL Studio project before playing".to_owned();
            return;
        }
        let Some(project_path) = self.sample_project_path() else {
            self.status = "Save the project to a file before playing its audio clips".to_owned();
            return;
        };
        if self.audio_engine.is_none() {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => self.audio_engine = Some(engine),
                Err(error) => {
                    self.status = format!("Could not start audio output: {error}");
                    return;
                }
            }
        }
        let Some(engine) = self.audio_engine.as_ref() else {
            self.status = "Audio output is not available".to_owned();
            return;
        };
        if !engine.output_active() {
            self.status = "Enable an output device in Audio settings before playing".to_owned();
            return;
        }
        if self.audio_test_tone {
            engine.set_test_tone(false);
            self.audio_test_tone = false;
        }
        if self.audio_monitor_input {
            let _ = engine.set_input_monitor(false);
            self.audio_monitor_input = false;
        }
        let sample_rate = engine.sample_rate();
        let project_bytes = match self
            .document
            .as_ref()
            .expect("project was checked above")
            .encode_lossless()
        {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not prepare project audio: {error}");
                return;
            }
        };
        let options = PlaylistRenderOptions {
            arrangement_id: self.selected_arrangement.unwrap_or(0),
            sample_rate,
            ..PlaylistRenderOptions::default()
        };
        let vst3_processor = self
            .vst3_host
            .as_ref()
            .map(|host| {
                host.prepare_playlist_stream(
                    self.document.as_ref().expect("project was checked above"),
                    options.arrangement_id,
                    &self.channel_vst3_instances,
                    sample_rate,
                    2.0,
                )
            })
            .transpose();
        let vst3_processor = match vst3_processor {
            Ok(processor) => processor,
            Err(error) => {
                self.status = format!("Could not prepare Playlist VST3 instruments: {error}");
                return;
            }
        };
        let writer = match engine.begin_streaming_playback() {
            Ok(writer) => writer,
            Err(error) => {
                self.status = format!("Could not start Playlist audio output: {error}");
                return;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("project-audio-render".to_owned())
            .spawn(move || {
                let result = FlpDocument::parse(&project_bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|document| {
                        stream_playlist_with_vst3_to_device(
                            &document,
                            &project_path,
                            options,
                            &writer,
                            &worker_cancelled,
                            vst3_processor,
                        )
                    });
                writer.finish();
                let _ = sender.send(result);
            });
        match worker {
            Ok(worker) => {
                self.playing = true;
                self.project_playback_loaded = true;
                self.playlist_playback_loaded = true;
                self.pending_audio_render = Some(PendingAudioRender {
                    receiver,
                    cancelled,
                    worker,
                });
                self.status =
                    "Preparing Playlist audio, Samplers, and VST3 instruments…".to_owned();
            }
            Err(error) => {
                self.stop_project_playback();
                self.status = format!("Could not start audio streaming: {error}");
            }
        }
    }

    fn poll_project_audio_render(&mut self) {
        let completed = self.pending_audio_render.as_ref().and_then(|pending| {
            match pending.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("audio render worker stopped unexpectedly".to_owned()))
                }
                Err(TryRecvError::Empty) => None,
            }
        });
        if let Some(result) = completed {
            let worker_panicked = self
                .pending_audio_render
                .take()
                .is_some_and(|pending| pending.worker.join().is_err());
            if worker_panicked {
                self.stop_project_playback();
                self.status = "Playlist audio worker panicked".to_owned();
                return;
            }
            match result {
                Ok(summary) => {
                    self.status = format!(
                        "Playlist finished: {} audio clips, {} Sampler pattern clips, {} Sampler notes, {} VST3 channels, {} VST3 notes, {} source files at {} Hz ({} VST3 channels unloaded, {} scaled audio and {} scaled pattern clips skipped); automation and Mixer effects are not rendered",
                        summary.audio_clips_rendered,
                        summary.sampler_pattern_clips_rendered,
                        summary.sampler_notes_rendered,
                        summary.vst3_plugin_channels_rendered,
                        summary.vst3_notes_rendered,
                        summary.source_files,
                        summary.sample_rate,
                        summary.vst3_plugin_channels_unloaded,
                        summary.audio_clips_skipped_unsupported_scale,
                        summary.pattern_clips_skipped_unsupported_scale,
                    );
                }
                Err(error) => {
                    self.stop_project_playback();
                    self.status = format!("Could not render project audio for playback: {error}");
                }
            }
        }
    }

    fn poll_vst3_stream(&mut self) {
        let completed =
            self.pending_vst3_stream
                .as_ref()
                .and_then(|stream| match stream.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => {
                        Some(Err("VST3 render worker stopped unexpectedly".to_owned()))
                    }
                    Err(TryRecvError::Empty) => None,
                });
        if let Some(result) = completed {
            let worker_panicked = self
                .pending_vst3_stream
                .take()
                .is_some_and(|stream| stream.join().is_err());
            if worker_panicked {
                self.stop_project_playback();
                self.status = "VST3 pattern stream worker panicked".to_owned();
                return;
            }
            match result {
                Ok(summary) => {
                    self.status = format!(
                        "VST3 pattern stream rendered {} frames at {} Hz",
                        summary.frames, summary.sample_rate
                    );
                }
                Err(error) => {
                    self.stop_project_playback();
                    self.status = format!("VST3 pattern stream failed: {error}");
                }
            }
        }
    }

    fn poll_sampler_stream(&mut self) {
        let completed = self.pending_sampler_stream.as_ref().and_then(|stream| {
            match stream.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("Sampler render worker stopped unexpectedly".to_owned()))
                }
                Err(TryRecvError::Empty) => None,
            }
        });
        if let Some(result) = completed {
            let worker_panicked = self
                .pending_sampler_stream
                .take()
                .is_some_and(|stream| stream.worker.join().is_err());
            if worker_panicked {
                self.stop_project_playback();
                self.status = "Sampler pattern stream worker panicked".to_owned();
                return;
            }
            match result {
                Ok(summary) => {
                    self.status = format!(
                        "Sampler pattern finished: {} notes, {} voices stolen, {:.2}s at {} Hz{}",
                        summary.notes_rendered,
                        summary.voices_stolen,
                        summary.frames as f64 / f64::from(summary.sample_rate),
                        summary.sample_rate,
                        if summary.notes_skipped_unresolved_sample == 0 {
                            String::new()
                        } else {
                            format!(
                                "; {} notes skipped because samples could not be resolved",
                                summary.notes_skipped_unresolved_sample
                            )
                        }
                    );
                }
                Err(error) => {
                    self.stop_project_playback();
                    self.status = format!("Sampler pattern stream failed: {error}");
                }
            }
        }
    }

    fn toggle_project_playback(&mut self) {
        if self.pending_song_render.is_some() {
            self.status = "Stop the Playlist render before starting playback".to_owned();
            return;
        }
        if self.playing {
            if let Some(engine) = &self.audio_engine {
                engine.pause_project_playback();
            }
            self.playing = false;
            self.status = "Project playback paused".to_owned();
        } else if self.project_playback_loaded {
            let result = self
                .audio_engine
                .as_ref()
                .ok_or_else(|| "Audio output is not available".to_owned())
                .and_then(AudioEngine::resume_project_playback);
            match result {
                Ok(()) => {
                    self.playing = true;
                    self.status = "Project playback resumed".to_owned();
                }
                Err(error) => self.status = format!("Could not resume project playback: {error}"),
            }
        } else if self.pending_audio_render.is_some() {
            self.status = "Preparing Playlist audio…".to_owned();
        } else {
            self.start_project_playback();
        }
    }

    fn stop_project_playback(&mut self) {
        if let Some(pending) = self.pending_audio_render.take() {
            pending.cancelled.store(true, Ordering::Release);
            self.audio_render_workers.push(pending);
        }
        if let Some(render) = self.pending_song_render.take() {
            render.cancelled.store(true, Ordering::Release);
            self.song_render_workers.push(render);
        }
        if let Some(stream) = self.pending_vst3_stream.take() {
            self.vst3_workers.push(stream);
        }
        if let Some(stream) = self.pending_sampler_stream.take() {
            stream.cancelled.store(true, Ordering::Release);
            self.sampler_workers.push(stream);
        }
        if let Some(engine) = &self.audio_engine {
            engine.stop_project_playback();
        }
        self.playing = false;
        self.project_playback_loaded = false;
        self.playlist_playback_loaded = false;
    }

    fn reap_vst3_workers(&mut self) {
        let mut index = 0;
        while index < self.vst3_workers.len() {
            if self.vst3_workers[index].is_finished() {
                let worker = self.vst3_workers.swap_remove(index);
                if worker.join().is_err() {
                    self.status = "A cancelled VST3 stream worker panicked".to_owned();
                }
            } else {
                index += 1;
            }
        }
    }

    fn reap_sampler_workers(&mut self) {
        let mut index = 0;
        while index < self.sampler_workers.len() {
            if self.sampler_workers[index].worker.is_finished() {
                let stream = self.sampler_workers.swap_remove(index);
                if stream.worker.join().is_err() {
                    self.status = "A cancelled Sampler stream worker panicked".to_owned();
                }
            } else {
                index += 1;
            }
        }
    }

    fn reap_audio_render_workers(&mut self) {
        let mut index = 0;
        while index < self.audio_render_workers.len() {
            if self.audio_render_workers[index].worker.is_finished() {
                let worker = self.audio_render_workers.swap_remove(index);
                if worker.worker.join().is_err() {
                    self.status = "A cancelled Playlist audio worker panicked".to_owned();
                }
            } else {
                index += 1;
            }
        }
    }

    fn reap_song_render_workers(&mut self) {
        let mut index = 0;
        while index < self.song_render_workers.len() {
            if self.song_render_workers[index].worker.is_finished() {
                let render = self.song_render_workers.swap_remove(index);
                if render.worker.join().is_err() {
                    self.status = "A cancelled Playlist render worker panicked".to_owned();
                }
            } else {
                index += 1;
            }
        }
    }

    fn top_menu(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.strong("FL")
                .on_hover_text("Independent FL Studio project editor");
            ui.separator();
            ui.menu_button("File", |ui| {
                if ui.button("Open…").clicked() {
                    self.open_dialog();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.document.is_some(),
                        egui::Button::new("Save (Ctrl/Cmd+S)"),
                    )
                    .clicked()
                {
                    self.save();
                    ui.close();
                }
                if ui
                    .add_enabled(self.document.is_some(), egui::Button::new("Save as…"))
                    .clicked()
                {
                    self.save_as();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.document.is_some(),
                        egui::Button::new("Save new version (Ctrl/Cmd+N)"),
                    )
                    .clicked()
                {
                    self.save_new_version();
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        self.document.is_some()
                            && self.current_path.is_some()
                            && self.pending_backup_write.is_none(),
                        egui::Button::new("Backup now"),
                    )
                    .clicked()
                {
                    self.backup_now();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.document.is_some()
                            && self
                                .last_autosave_path
                                .as_ref()
                                .is_some_and(|path| path.is_file())
                            && self.recovery_prompt.is_none(),
                        egui::Button::new("Revert to last autosave…"),
                    )
                    .clicked()
                {
                    self.revert_to_last_autosave();
                    ui.close();
                }
                ui.separator();
                let autosave_label = if self.autosave_minutes == 0 {
                    "Never".to_owned()
                } else if self.autosave_before_risky {
                    format!("{} min + risky operations", self.autosave_minutes)
                } else {
                    format!("Every {} minutes", self.autosave_minutes)
                };
                ui.menu_button(format!("Autosave: {autosave_label}"), |ui| {
                    for (minutes, before_risky, label) in [
                        (0, false, "Never"),
                        (15, false, "Rarely · every 15 minutes"),
                        (10, false, "Occasionally · every 10 minutes"),
                        (5, false, "Regularly · every 5 minutes"),
                        (
                            5,
                            true,
                            "Frequently · 5 minutes, including playback; before risky operations",
                        ),
                        (
                            1,
                            true,
                            "Very frequently · every minute and before risky operations",
                        ),
                    ] {
                        let selected = self.autosave_minutes == minutes
                            && self.autosave_before_risky == before_risky;
                        if ui.selectable_label(selected, label).clicked() && !selected {
                            self.autosave_minutes = minutes;
                            self.autosave_before_risky = before_risky;
                            self.persist_autosave_settings();
                            ui.close();
                        }
                    }
                });
                ui.menu_button(format!("Keep {} backups", self.backup_retention), |ui| {
                    for count in BACKUP_RETENTION_OPTIONS {
                        if ui
                            .selectable_value(
                                &mut self.backup_retention,
                                count,
                                format!("Keep {count} backups"),
                            )
                            .changed()
                        {
                            self.persist_autosave_settings();
                            ui.close();
                        }
                    }
                });
                ui.separator();
                let recent_projects = self.browser_recent_projects.clone();
                ui.menu_button("Recent projects", |ui| {
                    if recent_projects.is_empty() {
                        ui.label("No recent projects");
                    }
                    let mut open_path = None;
                    for (index, path) in recent_projects.iter().take(10).enumerate() {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        if ui
                            .button(format!("{}. {name}", index + 1))
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            open_path = Some(path.clone());
                        }
                    }
                    if recent_projects.len() > 10 {
                        ui.menu_button("More…", |ui| {
                            for (index, path) in recent_projects
                                .iter()
                                .enumerate()
                                .skip(10)
                                .take(RECENT_PROJECT_LIMIT - 10)
                            {
                                let name = path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| path.display().to_string());
                                if ui
                                    .button(format!("{}. {name}", index + 1))
                                    .on_hover_text(path.display().to_string())
                                    .clicked()
                                {
                                    open_path = Some(path.clone());
                                    ui.close();
                                }
                            }
                        });
                    }
                    if let Some(path) = open_path {
                        ui.close();
                        self.open_project(&path);
                    }
                });
                ui.separator();
                if ui.button("Exit").clicked() {
                    if self.dirty {
                        self.recovery_prompt = None;
                        self.pending_project_change = Some(PendingProjectChange::Exit);
                    } else {
                        self.close_approved = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.close();
                }
            });
            if ui
                .add_enabled(!self.undo_history.is_empty(), egui::Button::new("Undo"))
                .on_hover_text("Undo the last project edit (Ctrl/Cmd+Z)")
                .clicked()
            {
                self.undo_document();
            }
            if ui
                .add_enabled(!self.redo_history.is_empty(), egui::Button::new("Redo"))
                .on_hover_text("Redo the last undone project edit (Ctrl/Cmd+Shift+Z)")
                .clicked()
            {
                self.redo_document();
            }
            if ui
                .add_enabled(self.document.is_some(), egui::Button::new("Project Info…"))
                .clicked()
            {
                self.open_project_info();
            }
            if ui
                .add_enabled(self.document.is_some(), egui::Button::new("New pattern"))
                .clicked()
            {
                self.create_pattern();
            }
            if ui
                .add_enabled(
                    self.document.is_some(),
                    egui::Button::new("Project settings…"),
                )
                .clicked()
            {
                self.open_project_settings();
            }
            if ui
                .add_enabled(
                    self.document.is_some()
                        && self.current_path.is_some()
                        && self.pending_song_render.is_none(),
                    egui::Button::new("Render Playlist mix…"),
                )
                .clicked()
            {
                self.render_playlist_dialog();
            }
            if ui
                .add_enabled(
                    self.document.is_some() && self.current_path.is_some(),
                    egui::Button::new("Render audio clips…"),
                )
                .clicked()
            {
                self.render_audio_clips_dialog();
            }
            if ui
                .add_enabled(
                    self.document.is_some(),
                    egui::Button::new("Export song MIDI…"),
                )
                .clicked()
            {
                self.export_song_midi_dialog();
            }
            for item in [
                "Edit", "Add", "Patterns", "View", "Options", "Tools", "Help",
            ] {
                ui.label(item);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let title = self
                    .current_path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Untitled".to_owned());
                ui.label(format!("{}{}", title, if self.dirty { " *" } else { "" }));
            });
        });
    }

    fn current_playhead_tick(&self) -> Option<f64> {
        if !self.playlist_playback_loaded || !self.tempo_bpm.is_finite() || self.tempo_bpm <= 0.0 {
            return None;
        }
        let document = self.document.as_ref()?;
        let engine = self.audio_engine.as_ref()?;
        let sample_rate = engine.sample_rate();
        if sample_rate == 0 {
            return None;
        }
        let frames = engine.project_playback_position_frames();
        let ticks = frames as f64 * self.tempo_bpm * f64::from(document.header().ppq().max(1))
            / (60.0 * f64::from(sample_rate));
        ticks.is_finite().then_some(ticks)
    }

    fn song_position_label(&self) -> String {
        let Some(tick) = self.current_playhead_tick() else {
            return "1:01:000".to_owned();
        };
        let ppq = self
            .document
            .as_ref()
            .map(|document| u64::from(document.header().ppq().max(1)))
            .unwrap_or(96);
        let (numerator, denominator) = self
            .document
            .as_ref()
            .and_then(|document| document.metadata().time_signature())
            .unwrap_or((4, 4));
        let beat_ticks = ppq as f64 * 4.0 / f64::from(denominator.max(1));
        let bar_ticks = beat_ticks * f64::from(numerator.max(1));
        let bar = (tick / bar_ticks).floor() as u64 + 1;
        let within_bar = tick % bar_ticks;
        let beat = (within_bar / beat_ticks).floor() as u64 + 1;
        let tick_in_beat = (within_bar % beat_ticks).floor() as u64;
        format!("{bar}:{beat:02}:{tick_in_beat:03}")
    }

    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.add_space(4.0);
            if ui.button("●").on_hover_text("Record").clicked() {
                self.status = "Recording is not implemented yet".to_owned();
            }
            if ui.button("■").on_hover_text("Stop").clicked() {
                let was_preparing =
                    self.pending_audio_render.is_some() || self.pending_song_render.is_some();
                self.stop_project_playback();
                self.status = if was_preparing {
                    "Audio preparation or render cancelled".to_owned()
                } else {
                    "Project playback stopped".to_owned()
                };
            }
            let play_label = if self.pending_audio_render.is_some() {
                "…"
            } else if self.playing {
                "❚❚"
            } else {
                "▶"
            };
            let play_hint = if self.pending_audio_render.is_some() {
                "Preparing audio"
            } else if self.playing {
                "Pause"
            } else {
                "Play"
            };
            if ui.button(play_label).on_hover_text(play_hint).clicked() {
                self.toggle_project_playback();
            }
            ui.separator();
            ui.label("PAT");
            ui.label("SONG");
            ui.separator();
            ui.label("Tempo");
            let mut bpm = self.tempo_bpm;
            if ui
                .add(egui::DragValue::new(&mut bpm).speed(0.1).suffix(" BPM"))
                .changed()
            {
                self.tempo_bpm = bpm;
                self.update_tempo(bpm);
            }
            ui.separator();
            ui.monospace(self.song_position_label());
            ui.separator();
            for label in [
                "Playlist",
                "Channel Rack",
                "Piano roll",
                "Mixer",
                "Audio",
                "Automation",
            ] {
                if ui.small_button(label).clicked() {
                    self.view = match label {
                        "Channel Rack" => MainView::ChannelRack,
                        "Piano roll" => MainView::PianoRoll,
                        "Mixer" => MainView::Mixer,
                        "Audio" => MainView::Audio,
                        "Automation" => MainView::Automation,
                        _ => MainView::Playlist,
                    };
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label("00:00:00");
                ui.label("CPU --%  voices --");
            });
        });
    }

    fn refresh_browser_directory(&mut self) {
        self.browser_index = None;
        self.browser_search_all_active = false;
        self.browser_index_after_pending = None;
        match fs::read_dir(&self.browser_path) {
            Ok(directory) => {
                let mut entries = directory
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let is_directory = path.is_dir();
                        (is_directory || browser_file_kind(&path).is_some()).then_some(
                            BrowserEntry {
                                path,
                                name,
                                is_directory,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                entries.sort_by(|left, right| {
                    right
                        .is_directory
                        .cmp(&left.is_directory)
                        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                });
                self.browser_entries = entries;
                self.browser_error = None;
            }
            Err(error) => {
                self.browser_entries.clear();
                self.browser_error = Some(format!(
                    "Could not read {}: {error}",
                    self.browser_path.display()
                ));
            }
        }
    }

    fn start_browser_index(&mut self) {
        self.browser_search_all_active = false;
        self.start_browser_index_for(vec![self.browser_path.clone()], false);
    }

    fn start_browser_roots_index(&mut self) {
        if self.browser_search_roots.is_empty() {
            self.browser_search_all_active = false;
            self.browser_error =
                Some("Add Browser search folders before searching all folders".to_owned());
            return;
        }
        self.browser_search_all_active = true;
        self.start_browser_index_for(self.browser_search_roots.clone(), true);
    }

    fn start_browser_index_for(&mut self, roots: Vec<PathBuf>, all_roots: bool) {
        if self
            .browser_index
            .as_ref()
            .is_some_and(|index| index.roots == roots && index.all_roots == all_roots)
        {
            self.browser_index_after_pending = None;
            return;
        }
        if let Some(pending) = self.pending_browser_index.as_ref() {
            self.browser_index_after_pending =
                if pending.roots == roots && pending.all_roots == all_roots {
                    None
                } else {
                    Some((roots, all_roots))
                };
            return;
        }
        let worker_roots = roots.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = sender.send(index_browser_folders(worker_roots, all_roots));
        });
        self.browser_error = None;
        self.pending_browser_index = Some(PendingBrowserIndex {
            roots,
            all_roots,
            receiver,
            worker,
        });
    }

    fn collect_browser_index(&mut self) {
        let Some(pending) = self.pending_browser_index.as_ref() else {
            return;
        };
        match pending.receiver.try_recv() {
            Ok(result) => {
                let pending = self
                    .pending_browser_index
                    .take()
                    .expect("the completed Browser index is still pending");
                let active = self.browser_index_request_is_active(&pending);
                if pending.worker.join().is_err() {
                    if active {
                        self.browser_error =
                            Some("Recursive Browser indexing stopped unexpectedly".to_owned());
                    }
                    self.start_browser_index_after_pending();
                    return;
                }
                match result {
                    Ok(index) if active => {
                        self.browser_index = Some(index);
                    }
                    Err(error) if active => {
                        self.browser_error = Some(error);
                    }
                    Ok(_) | Err(_) => {}
                }
                self.start_browser_index_after_pending();
            }
            Err(TryRecvError::Disconnected) => {
                let pending = self
                    .pending_browser_index
                    .take()
                    .expect("the disconnected Browser index is still pending");
                let active = self.browser_index_request_is_active(&pending);
                let _ = pending.worker.join();
                if active {
                    self.browser_error = Some(format!(
                        "Could not finish recursive indexing of {}",
                        if pending.all_roots {
                            "Browser search folders".to_owned()
                        } else {
                            pending
                                .roots
                                .first()
                                .map(|root| root.display().to_string())
                                .unwrap_or_else(|| "the selected folder".to_owned())
                        }
                    ));
                }
                self.start_browser_index_after_pending();
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn start_browser_index_after_pending(&mut self) {
        if let Some((roots, all_roots)) = self.browser_index_after_pending.take() {
            self.start_browser_index_for(roots, all_roots);
        }
    }

    fn browser_index_request_is_active(&self, pending: &PendingBrowserIndex) -> bool {
        if pending.all_roots {
            self.browser_search_all_active && pending.roots == self.browser_search_roots
        } else {
            !self.browser_search_all_active
                && pending.roots.len() == 1
                && pending.roots.first() == Some(&self.browser_path)
        }
    }

    fn add_browser_search_root(&mut self, path: PathBuf) -> Option<PathBuf> {
        let root = fs::canonicalize(&path).unwrap_or(path);
        if !root.is_dir() {
            self.status = format!("Browser folder is not available: {}", root.display());
            return None;
        }
        if self.browser_search_roots.contains(&root) {
            self.status = "That folder is already in Browser search folders".to_owned();
            return Some(root);
        }
        if self.browser_search_roots.len() >= BROWSER_SEARCH_ROOT_LIMIT {
            self.status =
                format!("Browser supports up to {BROWSER_SEARCH_ROOT_LIMIT} extra search folders");
            return None;
        }
        self.browser_search_roots.push(root.clone());
        if let Err(error) = save_browser_search_roots(&self.browser_search_roots) {
            self.browser_search_roots.pop();
            self.status = format!("Could not save Browser search folders: {error}");
            return None;
        }
        if self
            .browser_index
            .as_ref()
            .is_some_and(|index| index.all_roots)
        {
            self.browser_index = None;
        }
        self.browser_index_after_pending = None;
        self.browser_search_all_active = false;
        self.status = format!("Added {} to Browser search folders", root.display());
        Some(root)
    }

    fn remove_browser_search_root(&mut self, path: &Path) {
        let Some(index) = self
            .browser_search_roots
            .iter()
            .position(|root| root == path)
        else {
            return;
        };
        let root = self.browser_search_roots.remove(index);
        if let Err(error) = save_browser_search_roots(&self.browser_search_roots) {
            self.browser_search_roots.insert(index, root);
            self.status = format!("Could not save Browser search folders: {error}");
        } else {
            if self
                .browser_index
                .as_ref()
                .is_some_and(|index| index.all_roots)
            {
                self.browser_index = None;
            }
            self.browser_index_after_pending = None;
            self.browser_search_all_active = false;
            self.status = format!("Removed {} from Browser search folders", root.display());
        }
    }

    fn set_browser_directory(&mut self, path: PathBuf) {
        if path.is_dir() {
            if path != self.browser_path {
                self.browser_index = None;
                self.browser_search_all_active = false;
            }
            self.browser_path = path;
            self.browser_selected = None;
            self.browser_search.clear();
            self.refresh_browser_directory();
        } else {
            self.browser_error = Some(format!("Folder is not available: {}", path.display()));
        }
    }

    fn activate_browser_path(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.set_browser_directory(path);
            return;
        }
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("flp" | "zip") => self.open_project(&path),
            Some("fst") => self.inspect_browser_preset(&path),
            Some("mid" | "midi") => match fs::read(&path) {
                Ok(bytes) => match MidiFile::parse(&bytes) {
                    Ok(midi) if !midi.tracks().is_empty() => {
                        self.pending_midi_import = Some(PendingMidiImport {
                            path: path.clone(),
                            midi,
                            selected_track: 0,
                        });
                        self.view = MainView::PianoRoll;
                        self.status = format!("Loaded MIDI file {}", path.display());
                    }
                    Ok(_) => self.status = "The MIDI file contains no tracks".to_owned(),
                    Err(error) => self.status = format!("Could not parse MIDI file: {error}"),
                },
                Err(error) => self.status = format!("Could not read {}: {error}", path.display()),
            },
            Some("wav" | "wave" | "mp3" | "m4a" | "ogg" | "flac" | "aif" | "aiff" | "wv") => {
                self.request_browser_preview(path, self.browser_full_sample);
            }
            _ => self.status = format!("Selected {}", path.display()),
        }
    }

    fn inspect_browser_preset(&mut self, path: &Path) {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not read preset {}: {error}", path.display());
                return;
            }
        };
        let preset = match FstPreset::parse(&bytes) {
            Ok(preset) => preset,
            Err(error) => {
                self.status = format!("Could not inspect preset {}: {error}", path.display());
                return;
            }
        };
        let document = preset.document();
        self.browser_fst_details = Some(FstPresetDetails {
            path: path.to_path_buf(),
            file_size: bytes.len(),
            format: document.header().format(),
            kind: preset.kind(),
            version: document.project_version().unwrap_or("unknown").to_owned(),
            event_count: document.events().len(),
            channel_count: document.channels().len(),
            plugin_state_count: document.channel_plugin_states().len(),
            mixer_insert_count: document.mixer_inserts().len(),
            automation_channel_count: document
                .automation_channels()
                .map_or(0, |channels| channels.len()),
            trailing_byte_count: document.trailing_bytes().len(),
        });
        self.status = format!("Inspected preset {}; state was not applied", path.display());
    }

    fn selected_sample_channel(&self) -> Option<(u16, String)> {
        let channel_id = self.selected_graph_channel?;
        let channel = self
            .document
            .as_ref()?
            .channels()
            .into_iter()
            .find(|channel| channel.id() == channel_id)?;
        if !matches!(channel.kind(), Some(0 | 4)) || channel.sample_path().is_none() {
            return None;
        }
        Some((
            channel_id,
            channel
                .display_name()
                .unwrap_or("Sample channel")
                .to_owned(),
        ))
    }

    fn load_browser_sample_into_channel(&mut self, channel_id: u16, path: &Path) {
        let path_string = path.to_string_lossy().into_owned();
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| "no project is open".to_owned())
            .and_then(|document| {
                document
                    .set_channel_sample_path(channel_id, &path_string)
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(()) => {
                self.stop_project_playback();
                self.dirty = true;
                let channel_name = self
                    .document
                    .as_ref()
                    .and_then(|document| {
                        document
                            .channels()
                            .into_iter()
                            .find(|channel| channel.id() == channel_id)
                    })
                    .and_then(|channel| channel.display_name().map(str::to_owned))
                    .unwrap_or_else(|| format!("channel {channel_id}"));
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.status = format!("Loaded sample {file_name} into {channel_name}");
                self.refresh_audio_waveform_paths();
            }
            Err(error) => self.status = format!("Could not load sample into channel: {error}"),
        }
    }

    fn browser_fst_details_dialog(&mut self, context: &egui::Context) {
        let Some(details) = self.browser_fst_details.clone() else {
            return;
        };
        let mut open = true;
        let mut close = false;
        egui::Window::new("FST preset details")
            .id(Id::new("browser-fst-details-dialog"))
            .open(&mut open)
            .resizable(false)
            .default_width(420.0)
            .show(context, |ui| {
                ui.monospace(details.path.display().to_string());
                ui.separator();
                ui.label(format!("State kind: {}", details.kind));
                ui.label(format!("Header format: {}", details.format));
                ui.label(format!("FL Studio version: {}", details.version));
                ui.label(format!("Size: {} bytes", details.file_size));
                ui.separator();
                ui.label(format!("Events: {}", details.event_count));
                ui.label(format!("Channel records: {}", details.channel_count));
                ui.label(format!("Plug-in states: {}", details.plugin_state_count));
                ui.label(format!("Mixer inserts: {}", details.mixer_insert_count));
                ui.label(format!(
                    "Automation channels: {}",
                    details.automation_channel_count
                ));
                ui.label(format!("Trailing bytes: {}", details.trailing_byte_count));
                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "This view reads preset metadata. Applying the state to a channel is not implemented yet.",
                    )
                    .color(MUTED),
                );
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        if !open || close {
            self.browser_fst_details = None;
        }
    }

    fn toggle_browser_favorite(&mut self, path: PathBuf) {
        if !self.browser_favorites.remove(&path) {
            self.browser_favorites.insert(path);
        }
        if let Err(error) = save_browser_favorites(&self.browser_favorites) {
            self.status = format!("Could not save Browser favorites: {error}");
        }
    }

    fn open_browser_tag_editor(&mut self, path: PathBuf) {
        let tags = self
            .browser_tags
            .get(&path)
            .map(|tags| tags.iter().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        self.browser_tag_editor = Some(BrowserTagEditor { path, tags });
    }

    fn save_browser_tag_editor(&mut self, editor: BrowserTagEditor) {
        let tags = editor
            .tags
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let previous = self.browser_tags.clone();
        if tags.is_empty() {
            self.browser_tags.remove(&editor.path);
        } else {
            self.browser_tags.insert(editor.path.clone(), tags);
        }
        if let Err(error) = save_browser_tags(&self.browser_tags) {
            self.browser_tags = previous;
            self.status = format!("Could not save Browser tags: {error}");
        } else {
            self.status = format!("Saved tags for {}", editor.path.display());
        }
    }

    fn browser_tag_editor_dialog(&mut self, context: &egui::Context) {
        let Some(mut editor) = self.browser_tag_editor.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Browser tags")
            .id(Id::new("browser-tag-editor"))
            .open(&mut open)
            .resizable(false)
            .show(context, |ui| {
                ui.label(editor.path.display().to_string());
                ui.add(
                    egui::TextEdit::singleline(&mut editor.tags)
                        .hint_text("Kick, 808, C#")
                        .desired_width(360.0),
                );
                ui.small("Separate tags with commas");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if save {
            self.save_browser_tag_editor(editor);
        } else if open && !cancel {
            self.browser_tag_editor = Some(editor);
        }
    }

    fn browser_tag_search_controls(&mut self, ui: &mut egui::Ui) {
        let available_tags = self
            .browser_tags
            .values()
            .flat_map(|tags| tags.iter().cloned())
            .collect::<BTreeSet<_>>();
        let selected_tags = self.browser_selected_tags.clone();
        let has_available_tags = !available_tags.is_empty();
        let available_tags = available_tags
            .union(&selected_tags)
            .cloned()
            .collect::<Vec<_>>();
        let selected_text = if selected_tags.is_empty() {
            "Tags".to_owned()
        } else {
            format!("Tags ({})", selected_tags.len())
        };
        let mut tag_changes = Vec::new();
        let mut clear_tags = false;
        egui::ComboBox::from_id_salt("browser-tag-search")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.small("Match");
                    ui.selectable_value(&mut self.browser_tag_logic, BrowserTagLogic::Any, "Any");
                    ui.selectable_value(&mut self.browser_tag_logic, BrowserTagLogic::All, "All");
                });
                ui.separator();
                if ui
                    .add_enabled(
                        !selected_tags.is_empty(),
                        egui::Button::new("Clear selected tags"),
                    )
                    .clicked()
                {
                    clear_tags = true;
                }
                if !has_available_tags {
                    ui.small("No file tags available");
                }
                for tag in &available_tags {
                    let mut selected = selected_tags.contains(tag);
                    if ui.checkbox(&mut selected, tag).changed() {
                        tag_changes.push((tag.clone(), selected));
                    }
                }
            });
        if clear_tags {
            self.browser_selected_tags.clear();
        }
        for (tag, selected) in tag_changes {
            if selected {
                self.browser_selected_tags.insert(tag);
            } else {
                self.browser_selected_tags.remove(&tag);
            }
        }
    }

    fn open_browser_search_save_dialog(&mut self) {
        let all_roots = self.browser_search_all_active;
        let recursive = all_roots
            || self.browser_index.as_ref().is_some_and(|index| {
                !index.all_roots
                    && index.roots.len() == 1
                    && index.roots.first() == Some(&self.browser_path)
            })
            || self.pending_browser_index.as_ref().is_some_and(|pending| {
                !pending.all_roots
                    && pending.roots.len() == 1
                    && pending.roots.first() == Some(&self.browser_path)
            });
        let (color, icon) = self
            .browser_active_saved_search
            .as_deref()
            .and_then(|active_name| {
                self.browser_saved_searches
                    .iter()
                    .find(|search| search.name == active_name)
                    .map(|search| (search.color, search.icon))
            })
            .unwrap_or_default();
        let default_name = self.browser_active_saved_search.clone().unwrap_or_else(|| {
            if !self.browser_search.trim().is_empty() {
                self.browser_search.trim().to_owned()
            } else if all_roots {
                "All Browser folders".to_owned()
            } else {
                self.browser_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Browser search".to_owned())
            }
        });
        self.browser_search_save_dialog = Some(BrowserSearchSaveDialog {
            name: default_name,
            search: SavedBrowserSearch {
                name: String::new(),
                query: self.browser_search.clone(),
                filter: self.browser_filter,
                all_roots,
                recursive,
                path: self.browser_path.clone(),
                tag_logic: self.browser_tag_logic,
                selected_tags: self.browser_selected_tags.clone(),
                hidden: false,
                color,
                icon,
            },
        });
    }

    fn save_browser_search(&mut self, mut search: SavedBrowserSearch) -> bool {
        search.name = search.name.trim().to_owned();
        if search.name.is_empty() {
            self.status = "Enter a name for this Browser search".to_owned();
            return false;
        }
        let previous = self.browser_saved_searches.clone();
        if let Some(index) = self
            .browser_saved_searches
            .iter()
            .position(|saved| saved.name.eq_ignore_ascii_case(&search.name))
        {
            self.browser_saved_searches[index] = search.clone();
        } else {
            self.browser_saved_searches.push(search.clone());
        }
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches = previous;
            self.status = format!("Could not save Browser searches: {error}");
            false
        } else {
            self.browser_tab = BrowserTab::Files;
            self.browser_active_saved_search = Some(search.name.clone());
            self.status = format!("Saved Browser search {}", search.name);
            true
        }
    }

    fn remove_browser_search(&mut self, index: usize) {
        if index >= self.browser_saved_searches.len() {
            return;
        }
        let search = self.browser_saved_searches.remove(index);
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches.insert(index, search);
            self.status = format!("Could not save Browser searches: {error}");
        } else {
            if self.browser_active_saved_search.as_deref() == Some(search.name.as_str()) {
                self.browser_active_saved_search = None;
            }
            self.status = "Removed saved Browser search".to_owned();
        }
    }

    fn set_browser_search_hidden(&mut self, index: usize, hidden: bool) {
        let Some(search) = self.browser_saved_searches.get_mut(index) else {
            return;
        };
        if search.hidden == hidden {
            return;
        }
        let name = search.name.clone();
        let previous = self.browser_saved_searches.clone();
        self.browser_saved_searches[index].hidden = hidden;
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches = previous;
            self.status = format!("Could not save Browser tabs: {error}");
        } else {
            if hidden && self.browser_active_saved_search.as_deref() == Some(name.as_str()) {
                self.browser_active_saved_search = None;
            }
            self.status = if hidden {
                format!("Hidden Browser tab {name}")
            } else {
                format!("Showed Browser tab {name}")
            };
        }
    }

    fn open_browser_tab_customize_dialog(&mut self, index: usize) {
        let Some(search) = self.browser_saved_searches.get(index) else {
            return;
        };
        self.browser_tab_customize_dialog = Some(BrowserTabCustomizeDialog {
            index,
            name: search.name.clone(),
            color: search.color,
            icon: search.icon,
        });
    }

    fn customize_browser_search(
        &mut self,
        index: usize,
        name: String,
        color: BrowserTabColor,
        icon: BrowserTabIcon,
    ) -> bool {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.status = "Enter a name for this Browser tab".to_owned();
            return false;
        }
        if index >= self.browser_saved_searches.len() {
            self.status = "Browser tab no longer exists".to_owned();
            return false;
        }
        if self
            .browser_saved_searches
            .iter()
            .enumerate()
            .any(|(other_index, search)| {
                other_index != index && search.name.eq_ignore_ascii_case(&name)
            })
        {
            self.status = "Browser tab names must be unique".to_owned();
            return false;
        }

        let previous = self.browser_saved_searches.clone();
        let old_name = self.browser_saved_searches[index].name.clone();
        self.browser_saved_searches[index].name = name.clone();
        self.browser_saved_searches[index].color = color;
        self.browser_saved_searches[index].icon = icon;
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches = previous;
            self.status = format!("Could not save Browser tabs: {error}");
            false
        } else {
            if self.browser_active_saved_search.as_deref() == Some(old_name.as_str()) {
                self.browser_active_saved_search = Some(name.clone());
            }
            self.status = format!("Updated Browser tab {name}");
            true
        }
    }

    fn move_browser_search(&mut self, index: usize, direction: isize) {
        if index >= self.browser_saved_searches.len()
            || self.browser_saved_searches[index].hidden
            || direction == 0
        {
            return;
        }
        let target = if direction < 0 {
            self.browser_saved_searches[..index]
                .iter()
                .rposition(|search| !search.hidden)
        } else {
            self.browser_saved_searches
                .iter()
                .enumerate()
                .skip(index + 1)
                .find_map(|(index, search)| (!search.hidden).then_some(index))
        };
        let Some(target) = target else {
            return;
        };

        let previous = self.browser_saved_searches.clone();
        self.browser_saved_searches.swap(index, target);
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches = previous;
            self.status = format!("Could not save Browser tabs: {error}");
        } else {
            self.status = if direction < 0 {
                "Moved Browser tab left".to_owned()
            } else {
                "Moved Browser tab right".to_owned()
            };
        }
    }

    fn clone_browser_search(&mut self, index: usize) {
        let Some(mut clone) = self.browser_saved_searches.get(index).cloned() else {
            return;
        };
        let base_name = format!("{} copy", clone.name);
        let mut name = base_name.clone();
        let mut suffix = 2usize;
        while self
            .browser_saved_searches
            .iter()
            .any(|search| search.name.eq_ignore_ascii_case(&name))
        {
            name = format!("{base_name} {suffix}");
            suffix += 1;
        }
        clone.name = name.clone();

        let insert_index = index + 1;
        self.browser_saved_searches
            .insert(insert_index, clone.clone());
        if let Err(error) = save_browser_saved_searches(&self.browser_saved_searches) {
            self.browser_saved_searches.remove(insert_index);
            self.status = format!("Could not save Browser tabs: {error}");
        } else {
            self.apply_browser_search(clone);
            if self.browser_active_saved_search.as_deref() == Some(name.as_str()) {
                self.status = format!("Cloned Browser tab as {name}");
            }
        }
    }

    fn apply_browser_search(&mut self, search: SavedBrowserSearch) {
        if !search.all_roots && !search.path.is_dir() {
            self.status = format!(
                "Saved Browser folder is not available: {}",
                search.path.display()
            );
            return;
        }
        self.browser_tab = BrowserTab::Files;
        self.browser_active_saved_search = Some(search.name.clone());
        self.browser_filter = search.filter;
        self.browser_tag_logic = search.tag_logic;
        self.browser_selected_tags = search.selected_tags;
        if search.all_roots {
            self.browser_search = search.query;
            self.start_browser_roots_index();
        } else {
            self.set_browser_directory(search.path);
            self.browser_search = search.query;
            if search.recursive {
                self.start_browser_index();
            }
        }
    }

    fn browser_search_save_dialog(&mut self, context: &egui::Context) {
        let Some(mut dialog) = self.browser_search_save_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Save Browser search")
            .id(Id::new("browser-search-save-dialog"))
            .open(&mut open)
            .resizable(false)
            .show(context, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut dialog.name);
                let tag_summary = if dialog.search.selected_tags.is_empty() {
                    "no tag filter".to_owned()
                } else {
                    format!(
                        "{} tags: {}",
                        dialog.search.tag_logic.label(),
                        dialog
                            .search
                            .selected_tags
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                ui.small(format!(
                    "Filter: {} · {} · {tag_summary}",
                    dialog.search.filter.label(),
                    if dialog.search.all_roots {
                        "all saved folders"
                    } else if dialog.search.recursive {
                        "recursive current folder"
                    } else {
                        "current folder"
                    }
                ));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        let mut keep_dialog_open = open && !cancel;
        if save {
            dialog.search.name = dialog.name.clone();
            keep_dialog_open = !self.save_browser_search(dialog.search.clone());
        }
        if keep_dialog_open {
            self.browser_search_save_dialog = Some(dialog);
        }
    }

    fn browser_tab_customize_dialog(&mut self, context: &egui::Context) {
        let Some(mut dialog) = self.browser_tab_customize_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Customize Browser tab")
            .id(Id::new("browser-tab-customize-dialog"))
            .open(&mut open)
            .resizable(false)
            .show(context, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut dialog.name);
                ui.horizontal(|ui| {
                    ui.label("Color");
                    egui::ComboBox::from_id_salt(("browser-tab-color", dialog.index))
                        .selected_text(dialog.color.label())
                        .show_ui(ui, |ui| {
                            for color in BrowserTabColor::ALL {
                                ui.selectable_value(&mut dialog.color, color, color.label());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Icon");
                    egui::ComboBox::from_id_salt(("browser-tab-icon", dialog.index))
                        .selected_text(dialog.icon.label())
                        .show_ui(ui, |ui| {
                            for icon in BrowserTabIcon::ALL {
                                ui.selectable_value(&mut dialog.icon, icon, icon.label());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        let mut keep_dialog_open = open && !cancel;
        if save {
            keep_dialog_open = !self.customize_browser_search(
                dialog.index,
                dialog.name.clone(),
                dialog.color,
                dialog.icon,
            );
        }
        if keep_dialog_open {
            self.browser_tab_customize_dialog = Some(dialog);
        }
    }

    fn request_browser_preview(&mut self, path: PathBuf, full_sample: bool) {
        self.browser_preview_error = None;
        self.browser_preview_path = None;
        if let Some(engine) = self.audio_engine.as_ref() {
            engine.stop_browser_preview();
        }
        if self.browser_preview_pending.is_some() {
            self.browser_preview_queued = Some((path, full_sample));
            self.browser_preview_cancelled = true;
            return;
        }
        self.browser_preview_cancelled = false;
        self.begin_browser_preview_load(path, full_sample);
    }

    fn begin_browser_preview_load(&mut self, path: PathBuf, full_sample: bool) {
        if self.audio_engine.is_none() {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => self.audio_engine = Some(engine),
                Err(error) => {
                    self.browser_preview_error = Some(error.clone());
                    self.status = format!("Could not start sample preview: {error}");
                    return;
                }
            }
        }
        let Some(engine) = self.audio_engine.as_ref() else {
            self.status = "Audio output is not available".to_owned();
            return;
        };
        if !engine.output_active() {
            let error = "Enable an output device before previewing a sample".to_owned();
            self.browser_preview_error = Some(error.clone());
            self.status = error;
            return;
        }
        let output_sample_rate = engine.sample_rate();
        let maximum_seconds = (!full_sample).then_some(5.0);
        let worker_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = match thread::Builder::new()
            .name("browser-sample-preview".to_owned())
            .spawn(move || {
                let result =
                    decode_audio_preview(&worker_path, output_sample_rate, maximum_seconds);
                let _ = sender.send(result);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                self.browser_preview_error = Some(error.to_string());
                self.status = format!("Could not start sample decoder: {error}");
                return;
            }
        };
        self.browser_preview_pending = Some(PendingBrowserPreview {
            path,
            receiver,
            worker,
        });
        self.status = if full_sample {
            "Loading full sample preview".to_owned()
        } else {
            "Loading 5-second sample preview".to_owned()
        };
    }

    fn stop_browser_preview(&mut self) {
        self.browser_preview_queued = None;
        self.browser_preview_cancelled = self.browser_preview_pending.is_some();
        self.browser_preview_path = None;
        if let Some(engine) = self.audio_engine.as_ref() {
            engine.stop_browser_preview();
        }
        self.status = "Stopped Browser sample preview".to_owned();
    }

    fn poll_browser_preview(&mut self, ctx: &egui::Context) {
        let completed = self.browser_preview_pending.as_ref().and_then(|pending| {
            match pending.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("sample preview worker stopped unexpectedly".to_owned()))
                }
                Err(TryRecvError::Empty) => None,
            }
        });
        if let Some(result) = completed
            && let Some(pending) = self.browser_preview_pending.take()
        {
            let _ = pending.worker.join();
            if let Some((path, full_sample)) = self.browser_preview_queued.take() {
                self.browser_preview_cancelled = false;
                self.begin_browser_preview_load(path, full_sample);
                return;
            }
            if self.browser_preview_cancelled {
                self.browser_preview_cancelled = false;
                return;
            }
            match result {
                Ok(samples) => {
                    let start_result = if let Some(engine) = self.audio_engine.as_ref() {
                        engine.set_browser_preview_gain(self.browser_preview_volume);
                        engine.set_browser_preview(samples)
                    } else {
                        Err("Audio output is not available".to_owned())
                    };
                    match start_result {
                        Ok(()) => {
                            self.status = format!("Previewing {}", pending.path.display());
                            self.browser_preview_path = Some(pending.path);
                        }
                        Err(error) => {
                            self.browser_preview_error = Some(error.clone());
                            self.status = format!("Could not play sample preview: {error}");
                        }
                    }
                }
                Err(error) => {
                    self.browser_preview_error = Some(error.clone());
                    self.status = format!("Could not decode sample preview: {error}");
                }
            }
        }
        if self.browser_preview_pending.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(40));
        }
        if self.browser_preview_path.is_some()
            && self
                .audio_engine
                .as_ref()
                .is_none_or(|engine| !engine.browser_preview_active())
        {
            self.browser_preview_path = None;
        }
    }

    fn browser(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Browser");
            if self.browser_tab == BrowserTab::Files {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("↻")
                        .on_hover_text("Refresh this folder")
                        .clicked()
                    {
                        self.refresh_browser_directory();
                    }
                });
            }
        });
        ui.separator();
        let saved_searches = self.browser_saved_searches.clone();
        let mut apply_saved_search = None;
        let mut remove_saved_search = None;
        let mut hide_saved_search = None;
        let mut show_saved_search = None;
        let mut rename_saved_search = None;
        let mut move_saved_search = None;
        let mut clone_saved_search = None;
        ui.horizontal_wrapped(|ui| {
            for tab in BrowserTab::ALL {
                let selected =
                    self.browser_tab == tab && self.browser_active_saved_search.is_none();
                if ui.selectable_label(selected, tab.label()).clicked() {
                    self.browser_tab = tab;
                    self.browser_active_saved_search = None;
                }
            }
            for (index, search) in saved_searches
                .iter()
                .enumerate()
                .filter(|(_, search)| !search.hidden)
            {
                let can_move_left = saved_searches
                    .iter()
                    .take(index)
                    .any(|search| !search.hidden);
                let can_move_right = saved_searches
                    .iter()
                    .skip(index + 1)
                    .any(|search| !search.hidden);
                ui.horizontal(|ui| {
                    let selected = self.browser_tab == BrowserTab::Files
                        && self.browser_active_saved_search.as_deref()
                            == Some(search.name.as_str());
                    let label = search.icon.glyph().map_or_else(
                        || search.name.clone(),
                        |glyph| format!("{glyph} {}", search.name),
                    );
                    let label = egui::RichText::new(label);
                    let label = search
                        .color
                        .text_color()
                        .map_or(label.clone(), |color| label.color(color));
                    let response = ui
                        .selectable_label(selected, label)
                        .on_hover_text("Apply this saved Browser search");
                    if response.clicked() {
                        apply_saved_search = Some(search.clone());
                    }
                    response.context_menu(|ui| {
                        if ui.button("Rename, color and icon…").clicked() {
                            rename_saved_search = Some(index);
                            ui.close();
                        }
                        if ui
                            .add_enabled(can_move_left, egui::Button::new("Move left"))
                            .clicked()
                        {
                            move_saved_search = Some((index, -1));
                            ui.close();
                        }
                        if ui
                            .add_enabled(can_move_right, egui::Button::new("Move right"))
                            .clicked()
                        {
                            move_saved_search = Some((index, 1));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Clone this tab").clicked() {
                            clone_saved_search = Some(index);
                            ui.close();
                        }
                        if ui.button("Hide tab").clicked() {
                            hide_saved_search = Some(index);
                            ui.close();
                        }
                        if ui.button("Delete tab").clicked() {
                            remove_saved_search = Some(index);
                            ui.close();
                        }
                    });
                    if ui
                        .small_button("×")
                        .on_hover_text("Delete this saved Browser tab")
                        .clicked()
                    {
                        remove_saved_search = Some(index);
                    }
                });
            }
            if saved_searches.iter().any(|search| search.hidden) {
                ui.menu_button("Other tabs", |ui| {
                    ui.small("Show hidden");
                    ui.separator();
                    for (index, search) in saved_searches
                        .iter()
                        .enumerate()
                        .filter(|(_, search)| search.hidden)
                    {
                        if ui.button(&search.name).clicked() {
                            show_saved_search = Some(index);
                            ui.close();
                        }
                    }
                });
            }
        });
        if let Some(index) = remove_saved_search {
            self.remove_browser_search(index);
        } else if let Some(index) = hide_saved_search {
            self.set_browser_search_hidden(index, true);
        } else if let Some(index) = show_saved_search {
            self.set_browser_search_hidden(index, false);
        } else if let Some((index, direction)) = move_saved_search {
            self.move_browser_search(index, direction);
        } else if let Some(index) = rename_saved_search {
            self.open_browser_tab_customize_dialog(index);
        } else if let Some(index) = clone_saved_search {
            self.clone_browser_search(index);
        }
        if let Some(search) = apply_saved_search {
            self.apply_browser_search(search);
        }
        ui.separator();

        match self.browser_tab {
            BrowserTab::Files => self.browser_files(ui),
            BrowserTab::CurrentProject => self.browser_current_project(ui),
            BrowserTab::Plugins => self.browser_plugins(ui),
            BrowserTab::Favorites => self.browser_favorites_view(ui),
            BrowserTab::Recent => self.browser_recent_view(ui),
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.separator();
            ui.small("Local files · project data stays lossless");
            self.browser_preview_player(ui);
        });
    }

    fn browser_preview_player(&mut self, ui: &mut egui::Ui) {
        let selected_audio = self
            .browser_selected
            .as_ref()
            .filter(|path| browser_file_kind(path) == Some(BrowserFileKind::Audio))
            .cloned();
        let path = selected_audio
            .or_else(|| {
                self.browser_preview_pending
                    .as_ref()
                    .map(|pending| pending.path.clone())
            })
            .or_else(|| self.browser_preview_path.clone());
        let Some(path) = path else {
            return;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let display_name = if name.chars().count() > 22 {
            format!("{}…", name.chars().take(21).collect::<String>())
        } else {
            name
        };
        let mut play_to_end = false;
        let mut stop_preview = false;
        let mut volume_changed = false;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.strong("Preview");
                ui.label(egui::RichText::new(display_name).small())
                    .on_hover_text(path.display().to_string());
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.browser_full_sample, "Full sample")
                    .on_hover_text(
                        "Play the full file on click; otherwise previews stop after five seconds",
                    );
                play_to_end = ui
                    .small_button("▶")
                    .on_hover_text("Play selected sample to the end")
                    .clicked();
                stop_preview = ui.small_button("■").on_hover_text("Stop preview").clicked();
            });
            ui.horizontal(|ui| {
                ui.small("Volume");
                volume_changed = ui
                    .add_sized(
                        Vec2::new(110.0, 18.0),
                        egui::Slider::new(&mut self.browser_preview_volume, 0.0..=1.0)
                            .show_value(false),
                    )
                    .on_hover_text("Preview volume")
                    .changed();
            });
            if let Some(error) = &self.browser_preview_error {
                ui.label(egui::RichText::new(error).color(ORANGE).small());
            } else if self.browser_preview_pending.is_some() && !self.browser_preview_cancelled {
                ui.small("Loading sample preview…");
            } else if self.browser_preview_path.as_ref() == Some(&path) {
                ui.small("Playing");
            } else {
                ui.small("Ready");
            }
        });
        if volume_changed && let Some(engine) = self.audio_engine.as_ref() {
            engine.set_browser_preview_gain(self.browser_preview_volume);
        }
        if stop_preview {
            self.stop_browser_preview();
        }
        if play_to_end {
            self.request_browser_preview(path, true);
        }
    }

    fn browser_files(&mut self, ui: &mut egui::Ui) {
        self.collect_browser_index();
        let home = default_browser_directory();
        let project_folder = self
            .current_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let packs_folder = browser_factory_packs_directory();
        let mut requested_folder = None;
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("Home").clicked() {
                requested_folder = Some(home.clone());
            }
            if let Some(path) = project_folder.as_ref()
                && ui.small_button("Project").clicked()
            {
                requested_folder = Some(path.clone());
            }
            if let Some(path) = packs_folder.as_ref()
                && ui.small_button("Packs").clicked()
            {
                requested_folder = Some(path.clone());
            }
        });

        let search_roots = self.browser_search_roots.clone();
        let current_folder = self.browser_path.clone();
        let mut add_search_root = false;
        let mut selected_search_root = None;
        let mut removed_search_root = None;
        ui.collapsing("Search folders", |ui| {
            if ui.small_button("+ Add folder").clicked() {
                add_search_root = true;
            }
            if search_roots.is_empty() {
                ui.small("No extra folders added");
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("browser-search-roots")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        for root in &search_roots {
                            let name = root
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| root.display().to_string());
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(
                                        current_folder == *root || current_folder.starts_with(root),
                                        format!("▸ {name}"),
                                    )
                                    .on_hover_text(root.display().to_string())
                                    .clicked()
                                {
                                    selected_search_root = Some(root.clone());
                                }
                                if ui
                                    .small_button("×")
                                    .on_hover_text("Remove this Browser search folder")
                                    .clicked()
                                {
                                    removed_search_root = Some(root.clone());
                                }
                            });
                        }
                    });
            }
        });
        if let Some(root) = selected_search_root {
            requested_folder = Some(root);
        }
        if let Some(root) = removed_search_root {
            self.remove_browser_search_root(&root);
        }
        if add_search_root
            && let Some(path) = rfd::FileDialog::new()
                .set_title("Add Browser search folder")
                .set_directory(&self.browser_path)
                .pick_folder()
            && let Some(root) = self.add_browser_search_root(path)
        {
            requested_folder = Some(root);
        }

        let mut refresh = false;
        ui.horizontal(|ui| {
            let parent = self.browser_path.parent().map(Path::to_path_buf);
            if ui
                .add_enabled(parent.is_some(), egui::Button::new("↑"))
                .on_hover_text("Parent folder")
                .clicked()
            {
                requested_folder = parent;
            }
            let path_label = self
                .browser_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.browser_path.display().to_string());
            ui.label(egui::RichText::new(path_label).small())
                .on_hover_text(self.browser_path.display().to_string());
            refresh = ui
                .small_button("↻")
                .on_hover_text("Read this folder again")
                .clicked();
        });
        if let Some(path) = requested_folder {
            self.set_browser_directory(path);
        } else if refresh {
            self.refresh_browser_directory();
        }

        if ui.small_button("Save search").clicked() {
            self.open_browser_search_save_dialog();
        }

        let current_indexed = self.browser_index.as_ref().is_some_and(|index| {
            !self.browser_search_all_active
                && !index.all_roots
                && index.roots.len() == 1
                && index.roots.first() == Some(&self.browser_path)
        });
        let all_roots_indexed = self.browser_index.as_ref().is_some_and(|index| {
            self.browser_search_all_active
                && index.all_roots
                && index.roots == self.browser_search_roots
        });
        let scan_running = self.pending_browser_index.is_some();
        let search_all_requested =
            ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F));
        ui.horizontal_wrapped(|ui| {
            let search_field = ui.add(
                egui::TextEdit::singleline(&mut self.browser_search)
                    .hint_text(if self.browser_search_all_active || search_all_requested {
                        "Filter all Browser folders"
                    } else {
                        "Filter this folder"
                    })
                    .desired_width(f32::INFINITY),
            );
            if search_all_requested {
                search_field.request_focus();
            }
            if ui
                .add_enabled(
                    !scan_running && !current_indexed,
                    egui::Button::new("⌕ Recursive"),
                )
                .on_hover_text("Search supported files in this folder and its subfolders")
                .clicked()
            {
                self.start_browser_index();
            }
            if ui
                .add_enabled(
                    !scan_running && !all_roots_indexed && !self.browser_search_roots.is_empty(),
                    egui::Button::new("⌕ All folders"),
                )
                .on_hover_text(
                    "Search supported files in every saved Browser folder and its subfolders",
                )
                .clicked()
            {
                self.start_browser_roots_index();
            }
        });
        if search_all_requested {
            self.start_browser_roots_index();
        }
        if let Some(pending) = self.pending_browser_index.as_ref() {
            if self.browser_index_request_is_active(pending) && pending.all_roots {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small("Indexing all Browser folders and their subfolders…");
                });
            } else if self.browser_index_request_is_active(pending) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small("Indexing this folder and its subfolders…");
                });
            } else {
                ui.small("Another recursive search is still indexing…");
            }
            ui.ctx().request_repaint_after(Duration::from_millis(80));
        } else if let Some(index) = self.browser_index.as_ref().filter(|index| {
            (!self.browser_search_all_active
                && !index.all_roots
                && index.roots.len() == 1
                && index.roots.first() == Some(&self.browser_path))
                || (self.browser_search_all_active
                    && index.all_roots
                    && index.roots == self.browser_search_roots)
        }) {
            let indexed_count = index.entries.len();
            let truncated = index.truncated;
            if index.all_roots {
                let mut summary = if truncated {
                    format!("All folders · scan limit reached ({indexed_count} files)")
                } else {
                    format!(
                        "All folders · {indexed_count} supported files in {} roots",
                        index.roots.len()
                    )
                };
                if !index.unavailable_roots.is_empty() {
                    summary.push_str(&format!(" · {} unavailable", index.unavailable_roots.len()));
                }
                ui.small(summary);
            } else {
                ui.small(if truncated {
                    format!("Recursive index · scan limit reached ({indexed_count} files)")
                } else {
                    format!("Recursive index · {indexed_count} supported files")
                });
            }
        }
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("browser-file-filter")
                .selected_text(self.browser_filter.label())
                .show_ui(ui, |ui| {
                    for filter in BrowserFilter::ALL {
                        ui.selectable_value(&mut self.browser_filter, filter, filter.label());
                    }
                });
            self.browser_tag_search_controls(ui);
        });
        if let Some(error) = &self.browser_error {
            ui.label(egui::RichText::new(error).color(ORANGE).small());
        }

        let query = self.browser_search.to_lowercase();
        let index_is_active = self.browser_index.as_ref().is_some_and(|index| {
            (self.browser_search_all_active
                && index.all_roots
                && index.roots == self.browser_search_roots)
                || (!self.browser_search_all_active
                    && !index.all_roots
                    && index.roots.len() == 1
                    && index.roots.first() == Some(&self.browser_path))
        });
        let source_entries = self
            .browser_index
            .as_ref()
            .filter(|_| index_is_active)
            .map(|index| &index.entries)
            .unwrap_or(&self.browser_entries);
        let recursive_results = index_is_active;
        let entries = source_entries
            .iter()
            .filter(|entry| {
                (!recursive_results && entry.is_directory)
                    || browser_filter_matches(&entry.path, self.browser_filter)
            })
            .filter(|entry| {
                entry.is_directory
                    || browser_tag_selection_matches(
                        self.browser_tag_logic,
                        &self.browser_selected_tags,
                        self.browser_tags.get(&entry.path),
                    )
            })
            .filter(|entry| {
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || self.browser_tags.get(&entry.path).is_some_and(|tags| {
                        tags.iter().any(|tag| tag.to_lowercase().contains(&query))
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut advance_search_result =
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F3));
        ui.horizontal(|ui| {
            ui.small(if recursive_results {
                format!("{} recursive matches", entries.len())
            } else {
                format!("{} items", entries.len())
            });
            if ui
                .add_enabled(!entries.is_empty(), egui::Button::new("Next"))
                .on_hover_text("Select the next result (F3)")
                .clicked()
            {
                advance_search_result = true;
            }
        });
        let next_match_path = if advance_search_result && !entries.is_empty() {
            let next_index = self
                .browser_selected
                .as_ref()
                .and_then(|selected| entries.iter().position(|entry| &entry.path == selected))
                .map(|index| (index + 1) % entries.len())
                .unwrap_or(0);
            let path = entries[next_index].path.clone();
            self.browser_selected = Some(path.clone());
            Some(path)
        } else {
            None
        };
        let mut activate = None;
        let mut preview = None;
        let mut favorite = None;
        let mut edit_tags = None;
        let mut inspect_preset = None;
        let mut load_sample = None;
        let sample_target = self.selected_sample_channel();
        egui::ScrollArea::vertical()
            .id_salt("browser-files-list")
            .show(ui, |ui| {
                for entry in entries {
                    let is_favorite = self.browser_favorites.contains(&entry.path);
                    let selected = self.browser_selected.as_ref() == Some(&entry.path);
                    let tags = self
                        .browser_tags
                        .get(&entry.path)
                        .map(|tags| tags.iter().cloned().collect::<Vec<_>>().join(", "));
                    let row = ui.horizontal(|ui| {
                        let icon = if entry.is_directory {
                            "▸"
                        } else {
                            browser_file_icon(&entry.path)
                        };
                        let suffix = tags
                            .as_ref()
                            .map(|tags| format!("  #{tags}"))
                            .unwrap_or_default();
                        let response =
                            ui.selectable_label(selected, format!("{icon} {}{suffix}", entry.name));
                        let clicked = response.clicked();
                        let double_clicked = response.double_clicked();
                        let hover_text = tags.map_or_else(
                            || entry.path.display().to_string(),
                            |tags| format!("{}\nTags: {tags}", entry.path.display()),
                        );
                        response.on_hover_text(hover_text).context_menu(|ui| {
                            if !entry.is_directory
                                && browser_file_kind(&entry.path) == Some(BrowserFileKind::Audio)
                                && let Some((channel_id, channel_name)) = &sample_target
                                && ui
                                    .button(format!("Send to {channel_name} ({channel_id})"))
                                    .clicked()
                            {
                                load_sample = Some((*channel_id, entry.path.clone()));
                                ui.close();
                            }
                            if !entry.is_directory
                                && entry
                                    .path
                                    .extension()
                                    .and_then(|extension| extension.to_str())
                                    .is_some_and(|extension| extension.eq_ignore_ascii_case("fst"))
                                && ui.button("Inspect preset…").clicked()
                            {
                                inspect_preset = Some(entry.path.clone());
                                ui.close();
                            }
                            if !entry.is_directory && ui.button("Edit tags…").clicked() {
                                edit_tags = Some(entry.path.clone());
                                ui.close();
                            }
                        });
                        let favorite_clicked = !entry.is_directory
                            && ui
                                .small_button(if is_favorite { "★" } else { "☆" })
                                .clicked();
                        let edit_tags_clicked = !entry.is_directory
                            && ui.small_button("#").on_hover_text("Edit tags").clicked();
                        (clicked, double_clicked, favorite_clicked, edit_tags_clicked)
                    });
                    let row_rect = row.response.rect;
                    let (clicked, double_clicked, favorite_clicked, edit_tags_clicked) = row.inner;
                    if next_match_path.as_ref() == Some(&entry.path) {
                        ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                    }
                    if double_clicked {
                        self.browser_selected = Some(entry.path.clone());
                        activate = Some(entry.path.clone());
                    } else if clicked {
                        self.browser_selected = Some(entry.path.clone());
                        if !entry.is_directory
                            && browser_file_kind(&entry.path) == Some(BrowserFileKind::Audio)
                        {
                            preview = Some(entry.path.clone());
                        }
                    }
                    if favorite_clicked {
                        favorite = Some(entry.path.clone());
                    }
                    if edit_tags_clicked {
                        edit_tags = Some(entry.path);
                    }
                }
            });
        if let Some(path) = favorite {
            self.toggle_browser_favorite(path);
        }
        if let Some(path) = preview {
            self.request_browser_preview(path, self.browser_full_sample);
        }
        if let Some(path) = activate {
            self.activate_browser_path(path);
        }
        if let Some(path) = edit_tags {
            self.open_browser_tag_editor(path);
        }
        if let Some(path) = inspect_preset {
            self.inspect_browser_preset(&path);
        }
        if let Some((channel_id, path)) = load_sample {
            self.load_browser_sample_into_channel(channel_id, &path);
        }
    }

    fn browser_current_project(&self, ui: &mut egui::Ui) {
        if let Some(document) = &self.document {
            egui::CollapsingHeader::new("Channels")
                .default_open(true)
                .show(ui, |ui| {
                    for channel in document.channels() {
                        ui.label(channel.display_name().unwrap_or("(unnamed channel)"));
                    }
                });
            egui::CollapsingHeader::new("Patterns")
                .default_open(true)
                .show(ui, |ui| {
                    if let Ok(patterns) = document.patterns() {
                        for pattern in patterns {
                            ui.label(pattern.name.as_deref().unwrap_or("Pattern"));
                        }
                    }
                });
            ui.collapsing("Plug-in state", |ui| {
                for state in document.channel_plugin_states() {
                    ui.label(format!(
                        "{} · {} bytes",
                        state.plugin_identifier().unwrap_or("Plug-in"),
                        state.data_payload().len()
                    ));
                }
            });
        } else {
            ui.label(egui::RichText::new("No project open").color(MUTED));
        }
    }

    fn browser_plugins(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.browser_search)
                .hint_text("Find installed plug-ins")
                .desired_width(f32::INFINITY),
        );
        let query = self.browser_search.to_lowercase();
        let candidates = self
            .plugin_candidates
            .iter()
            .filter(|candidate| {
                query.is_empty()
                    || candidate.name.to_lowercase().contains(&query)
                    || candidate
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect::<Vec<_>>();
        ui.small(format!("{} installed plug-ins", candidates.len()));
        let mut favorite = None;
        egui::ScrollArea::vertical()
            .id_salt("browser-plugin-list")
            .show(ui, |ui| {
                for candidate in candidates {
                    let is_favorite = self.browser_favorites.contains(&candidate.path);
                    ui.horizontal(|ui| {
                        ui.label(format!("{} · {}", candidate.name, candidate.format))
                            .on_hover_text(candidate.path.display().to_string());
                        if ui
                            .small_button(if is_favorite { "★" } else { "☆" })
                            .clicked()
                        {
                            favorite = Some(candidate.path.clone());
                        }
                    });
                }
            });
        if let Some(path) = favorite {
            self.toggle_browser_favorite(path);
        }
    }

    fn browser_favorites_view(&mut self, ui: &mut egui::Ui) {
        self.browser_tag_search_controls(ui);
        ui.add(
            egui::TextEdit::singleline(&mut self.browser_search)
                .hint_text("Find favorites")
                .desired_width(f32::INFINITY),
        );
        let query = self.browser_search.to_lowercase();
        let favorites = self
            .browser_favorites
            .iter()
            .filter(|path| {
                browser_tag_selection_matches(
                    self.browser_tag_logic,
                    &self.browser_selected_tags,
                    self.browser_tags.get(*path),
                ) && (query.is_empty()
                    || path.to_string_lossy().to_lowercase().contains(&query)
                    || self.browser_tags.get(*path).is_some_and(|tags| {
                        tags.iter().any(|tag| tag.to_lowercase().contains(&query))
                    }))
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut activate = None;
        let mut preview = None;
        let mut remove = None;
        egui::ScrollArea::vertical()
            .id_salt("browser-favorites-list")
            .show(ui, |ui| {
                for path in favorites {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let (clicked, double_clicked, remove_clicked) = ui
                        .horizontal(|ui| {
                            let response = ui.selectable_label(
                                self.browser_selected.as_ref() == Some(&path),
                                format!("★ {name}"),
                            );
                            let clicked = response.clicked();
                            let double_clicked = response.double_clicked();
                            response.on_hover_text(path.display().to_string());
                            (clicked, double_clicked, ui.small_button("×").clicked())
                        })
                        .inner;
                    if clicked {
                        self.browser_selected = Some(path.clone());
                    }
                    if double_clicked {
                        activate = Some(path.clone());
                    } else if clicked && browser_file_kind(&path) == Some(BrowserFileKind::Audio) {
                        preview = Some(path.clone());
                    }
                    if remove_clicked {
                        remove = Some(path);
                    }
                }
            });
        if let Some(path) = remove {
            self.toggle_browser_favorite(path);
        }
        if let Some(path) = preview {
            self.request_browser_preview(path, self.browser_full_sample);
        }
        if let Some(path) = activate {
            self.activate_browser_path(path);
        }
    }

    fn browser_recent_view(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.browser_search)
                .hint_text("Find recent projects")
                .desired_width(f32::INFINITY),
        );
        let query = self.browser_search.to_lowercase();
        let recent = self
            .browser_recent_projects
            .iter()
            .filter(|path| {
                query.is_empty() || path.to_string_lossy().to_lowercase().contains(&query)
            })
            .cloned()
            .collect::<Vec<_>>();
        if recent.is_empty() {
            ui.label(egui::RichText::new("No recent projects").color(MUTED));
        }
        let mut activate = None;
        egui::ScrollArea::vertical()
            .id_salt("browser-recent-list")
            .show(ui, |ui| {
                for path in recent {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let response = ui
                        .selectable_label(false, format!("◷ {name}"))
                        .on_hover_text(path.display().to_string());
                    if response.double_clicked() {
                        activate = Some(path);
                    }
                }
            });
        if let Some(path) = activate {
            self.open_project(&path);
        }
    }

    fn view_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for view in MainView::ALL {
                if ui
                    .selectable_label(self.view == view, view.label())
                    .clicked()
                {
                    self.view = view;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::Slider::new(&mut self.timeline_zoom, 0.04..=0.24).text("Zoom"));
            });
        });
        ui.separator();
    }

    fn playlist(&mut self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            empty_view(ui, "Open an FL Studio project to see its Playlist");
            return;
        };
        let arrangements = document.arrangements().unwrap_or_default();
        let tracks = document.playlist_tracks();
        let channel_kinds: BTreeMap<_, _> = document
            .channels()
            .into_iter()
            .map(|channel| (channel.id(), channel.kind()))
            .collect();
        let ppq = document.header().ppq().max(1);
        let Some(arrangement) = arrangements
            .iter()
            .find(|arrangement| Some(arrangement.id) == self.selected_arrangement)
            .or_else(|| arrangements.first())
            .cloned()
        else {
            empty_view(ui, "This project has no decoded Playlist arrangement");
            return;
        };

        let waveform_paths = arrangement
            .clips
            .iter()
            .filter_map(|clip| match clip.target() {
                flp_rebuild::PlaylistClipTarget::Channel { id } => {
                    self.audio_waveform_paths.get(&id).cloned()
                }
                flp_rebuild::PlaylistClipTarget::Pattern { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        for path in &waveform_paths {
            self.request_audio_waveform(path);
        }
        self.poll_audio_waveforms(ui.ctx());

        ui.horizontal(|ui| {
            ui.label("Arrangement");
            ui.strong(arrangement.name.as_deref().unwrap_or("Arrangement"));
            ui.separator();
            ui.label(format!("{} clips", arrangement.clips.len()));
            ui.separator();
            ui.label(format!("{} PPQ", ppq));
        });
        self.time_marker_editor(ui, arrangement.id, &arrangement.time_markers);

        let max_tick = arrangement
            .clips
            .iter()
            .map(|clip| clip.position_ticks.saturating_add(clip.length_ticks))
            .chain(
                arrangement
                    .time_markers
                    .iter()
                    .map(TimeMarker::position_ticks),
            )
            .max()
            .unwrap_or(ppq as u32 * 64)
            .max(ppq as u32 * 16);
        let grid_width = ((max_tick as f32 * self.timeline_zoom) + 240.0).clamp(1800.0, 50000.0);
        let last_track = arrangement
            .clips
            .iter()
            .filter_map(|clip| clip.track_index)
            .max()
            .unwrap_or(15)
            .max(15);
        let label_width = 178.0;
        let row_height = 27.0;
        let tick_scale = self.timeline_zoom;
        let playhead_tick = self.current_playhead_tick();
        let bars_per_measure = 4u32;
        let measure_ticks = ppq as u32 * bars_per_measure;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (label_rect, _) =
                        ui.allocate_exact_size(Vec2::new(label_width, 24.0), Sense::hover());
                    ui.painter().rect_filled(label_rect, 0, PANEL_DARK);
                    ui.painter().text(
                        label_rect.center(),
                        Align2::CENTER_CENTER,
                        "TRACK",
                        FontId::proportional(10.0),
                        MUTED,
                    );
                    let (ruler_rect, _) =
                        ui.allocate_exact_size(Vec2::new(grid_width, 24.0), Sense::hover());
                    let painter = ui.painter_at(ruler_rect);
                    painter.rect_filled(ruler_rect, 0, PANEL_DARK);
                    let measure_width = measure_ticks as f32 * tick_scale;
                    if measure_width > 0.0 {
                        let measures = (grid_width / measure_width).ceil() as u32;
                        for measure in 0..=measures {
                            let x = ruler_rect.left() + measure as f32 * measure_width;
                            painter.line_segment(
                                [
                                    egui::pos2(x, ruler_rect.top()),
                                    egui::pos2(x, ruler_rect.bottom()),
                                ],
                                Stroke::new(1.0, GRID),
                            );
                            painter.text(
                                egui::pos2(x + 4.0, ruler_rect.center().y),
                                Align2::LEFT_CENTER,
                                (measure + 1).to_string(),
                                FontId::proportional(10.0),
                                MUTED,
                            );
                        }
                    }
                    for (marker_index, marker) in arrangement.time_markers.iter().enumerate() {
                        let x = ruler_rect.left() + marker.position_ticks() as f32 * tick_scale;
                        let color = if self.selected_time_marker == Some(marker_index) {
                            ORANGE
                        } else {
                            MUTED
                        };
                        painter.line_segment(
                            [
                                egui::pos2(x, ruler_rect.top()),
                                egui::pos2(x, ruler_rect.bottom()),
                            ],
                            Stroke::new(1.5, color),
                        );
                        let label = marker.name().map(str::to_owned).unwrap_or_else(|| {
                            if marker.is_signature() {
                                format!(
                                    "{}/{}",
                                    marker.numerator().unwrap_or(4),
                                    marker.denominator().unwrap_or(4)
                                )
                            } else {
                                format!("M{}", marker_index + 1)
                            }
                        });
                        painter.text(
                            egui::pos2(x + 3.0, ruler_rect.top() + 2.0),
                            Align2::LEFT_TOP,
                            label,
                            FontId::proportional(9.0),
                            color,
                        );
                    }
                    if let Some(tick) = playhead_tick {
                        let x = ruler_rect.left() + tick as f32 * tick_scale;
                        if (ruler_rect.left()..=ruler_rect.right()).contains(&x) {
                            painter.line_segment(
                                [
                                    egui::pos2(x, ruler_rect.top()),
                                    egui::pos2(x, ruler_rect.bottom()),
                                ],
                                Stroke::new(2.0, GREEN),
                            );
                        }
                    }
                });

                for row in 0..=last_track {
                    let track_id = u32::from(row) + 1;
                    let track_name = tracks
                        .iter()
                        .find(|track| track.id == track_id)
                        .and_then(|track| track.name.as_deref())
                        .unwrap_or("Track");
                    ui.horizontal(|ui| {
                        let (label_rect, _) = ui.allocate_exact_size(
                            Vec2::new(label_width, row_height),
                            Sense::hover(),
                        );
                        ui.painter().rect_filled(label_rect, 0, PANEL_LIGHT);
                        ui.painter().rect_stroke(
                            label_rect,
                            0,
                            Stroke::new(1.0, PANEL_DARK),
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().text(
                            egui::pos2(label_rect.left() + 7.0, label_rect.center().y),
                            Align2::LEFT_CENTER,
                            format!("{:02}  {}", row + 1, track_name),
                            FontId::proportional(11.0),
                            TEXT,
                        );

                        let (grid_rect, _) = ui
                            .allocate_exact_size(Vec2::new(grid_width, row_height), Sense::hover());
                        let painter = ui.painter_at(grid_rect);
                        painter.rect_filled(grid_rect, 0, PANEL_DARK);
                        painter.line_segment(
                            [grid_rect.left_bottom(), grid_rect.right_bottom()],
                            Stroke::new(1.0, GRID),
                        );
                        let measure_width = measure_ticks as f32 * tick_scale;
                        if measure_width > 0.0 {
                            let measures = (grid_width / measure_width).ceil() as u32;
                            for measure in 0..=measures {
                                let x = grid_rect.left() + measure as f32 * measure_width;
                                painter.line_segment(
                                    [
                                        egui::pos2(x, grid_rect.top()),
                                        egui::pos2(x, grid_rect.bottom()),
                                    ],
                                    Stroke::new(1.0, GRID),
                                );
                            }
                        }

                        for (clip_index, clip) in arrangement.clips.iter().enumerate() {
                            if clip.track_index != Some(row) {
                                continue;
                            }
                            let left = grid_rect.left() + clip.position_ticks as f32 * tick_scale;
                            let width = (clip.length_ticks as f32 * tick_scale).max(4.0);
                            let clip_rect = egui::Rect::from_min_size(
                                egui::pos2(left, grid_rect.top() + 3.0),
                                Vec2::new(width, row_height - 6.0),
                            );
                            if !clip_rect.intersects(grid_rect) {
                                continue;
                            }
                            let color = match clip.target() {
                                flp_rebuild::PlaylistClipTarget::Pattern { .. } => PURPLE,
                                flp_rebuild::PlaylistClipTarget::Channel { id } => {
                                    if channel_kinds.get(&id).copied().flatten() == Some(5) {
                                        ORANGE
                                    } else {
                                        BLUE
                                    }
                                }
                            };
                            let selected = self.selected_clip == Some(clip_index)
                                && self.selected_arrangement == Some(arrangement.id);
                            painter.rect_filled(
                                clip_rect,
                                egui::CornerRadius::same(3),
                                if selected {
                                    color.gamma_multiply(1.25)
                                } else {
                                    color
                                },
                            );
                            painter.rect_stroke(
                                clip_rect,
                                egui::CornerRadius::same(3),
                                Stroke::new(1.0, color.gamma_multiply(0.65)),
                                egui::StrokeKind::Inside,
                            );
                            if let flp_rebuild::PlaylistClipTarget::Channel { id } = clip.target()
                                && let Some(path) = self.audio_waveform_paths.get(&id)
                            {
                                if let Some(waveform) = self.audio_waveforms.get(path).cloned() {
                                    draw_audio_clip_waveform(&painter, clip_rect, clip, &waveform);
                                } else if self.waveform_loads.contains_key(path) {
                                    painter.text(
                                        clip_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "…",
                                        FontId::proportional(10.0),
                                        Color32::from_white_alpha(150),
                                    );
                                }
                            }
                            if width > 50.0 {
                                let name = clip_name(clip, &tracks);
                                painter.text(
                                    egui::pos2(clip_rect.left() + 5.0, clip_rect.center().y),
                                    Align2::LEFT_CENTER,
                                    name,
                                    FontId::proportional(10.0),
                                    Color32::WHITE,
                                );
                            }
                            let response = ui.interact(
                                clip_rect,
                                Id::new(("playlist-clip", arrangement.id, clip_index)),
                                Sense::click(),
                            );
                            let response = if let flp_rebuild::PlaylistClipTarget::Channel { id } =
                                clip.target()
                                && let Some(path) = self.audio_waveform_paths.get(&id)
                                && let Some(error) = self.waveform_errors.get(path)
                            {
                                response.on_hover_text(format!("Waveform unavailable: {error}"))
                            } else {
                                response
                            };
                            if response.clicked() {
                                self.selected_arrangement = Some(arrangement.id);
                                self.selected_clip = Some(clip_index);
                                self.status = format!("Selected Playlist clip {}", clip_index + 1);
                            }
                        }
                        for (marker_index, marker) in arrangement.time_markers.iter().enumerate() {
                            let x = grid_rect.left() + marker.position_ticks() as f32 * tick_scale;
                            if !(grid_rect.left()..=grid_rect.right()).contains(&x) {
                                continue;
                            }
                            let color = if self.selected_time_marker == Some(marker_index) {
                                ORANGE
                            } else {
                                MUTED.gamma_multiply(0.7)
                            };
                            painter.line_segment(
                                [
                                    egui::pos2(x, grid_rect.top()),
                                    egui::pos2(x, grid_rect.bottom()),
                                ],
                                Stroke::new(1.0, color),
                            );
                        }
                        if let Some(tick) = playhead_tick {
                            let x = grid_rect.left() + tick as f32 * tick_scale;
                            if (grid_rect.left()..=grid_rect.right()).contains(&x) {
                                painter.line_segment(
                                    [
                                        egui::pos2(x, grid_rect.top()),
                                        egui::pos2(x, grid_rect.bottom()),
                                    ],
                                    Stroke::new(2.0, GREEN),
                                );
                            }
                        }
                    });
                }
            });

        self.selected_clip_editor(ui, arrangement.id, &arrangement.clips);
    }

    fn selected_clip_editor(
        &mut self,
        ui: &mut egui::Ui,
        arrangement_id: u16,
        clips: &[PlaylistClip],
    ) {
        let Some(index) = self.selected_clip else {
            return;
        };
        let Some(clip) = clips.get(index) else {
            self.selected_clip = None;
            return;
        };
        let mut position = clip.position_ticks;
        let mut length = clip.length_ticks;
        let mut item_index = clip.item_index;
        let mut raw_track_index = clip.raw_track_index;
        let mut group = clip.group;
        let mut item_flags = clip.item_flags;
        let mut start_offset = clip.start_offset;
        let mut end_offset = clip.end_offset;
        let mut scale = clip.scale.unwrap_or(1.0);
        let mut position_changed = false;
        let mut length_changed = false;
        let mut item_index_changed = false;
        let mut raw_track_index_changed = false;
        let mut group_changed = false;
        let mut item_flags_changed = false;
        let mut start_offset_changed = false;
        let mut end_offset_changed = false;
        let mut scale_changed = false;
        let mut duplicate_requested = false;
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("Clip {}", index + 1));
            ui.label("Start");
            position_changed = ui
                .add(egui::DragValue::new(&mut position).speed(1.0))
                .changed();
            ui.label("Length");
            length_changed = ui
                .add(egui::DragValue::new(&mut length).speed(1.0))
                .changed();
            duplicate_requested = ui.button("Duplicate after").clicked();
            if ui.button("Deselect").clicked() {
                self.selected_clip = None;
            }
        });
        ui.collapsing("Clip data fields", |ui| {
            ui.horizontal_wrapped(|ui| {
                item_index_changed = ui
                    .add(
                        egui::DragValue::new(&mut item_index)
                            .prefix("Item index raw ")
                            .speed(0.1),
                    )
                    .changed();
                raw_track_index_changed = ui
                    .add(
                        egui::DragValue::new(&mut raw_track_index)
                            .prefix("Track index raw ")
                            .speed(0.1),
                    )
                    .changed();
                group_changed = ui
                    .add(
                        egui::DragValue::new(&mut group)
                            .prefix("Group raw ")
                            .speed(0.1),
                    )
                    .changed();
                item_flags_changed = ui
                    .add(
                        egui::DragValue::new(&mut item_flags)
                            .prefix("Flags raw ")
                            .speed(0.1),
                    )
                    .changed();
            });
            ui.horizontal_wrapped(|ui| {
                start_offset_changed = ui
                    .add(
                        egui::DragValue::new(&mut start_offset)
                            .prefix("Start offset ")
                            .speed(0.001),
                    )
                    .changed();
                end_offset_changed = ui
                    .add(
                        egui::DragValue::new(&mut end_offset)
                            .prefix("End offset ")
                            .speed(0.001),
                    )
                    .changed();
                if clip.scale.is_some() {
                    scale_changed = ui
                        .add(
                            egui::DragValue::new(&mut scale)
                                .prefix("Scale ")
                                .speed(0.001),
                        )
                        .changed();
                } else {
                    ui.label("Scale unavailable in this record layout");
                }
            });
        });
        let mut clip_edit_succeeded = true;
        if position_changed
            || length_changed
            || item_index_changed
            || raw_track_index_changed
            || group_changed
            || item_flags_changed
            || start_offset_changed
            || end_offset_changed
            || scale_changed
        {
            let edit = PlaylistClipEdit {
                position_ticks: position_changed.then_some(position),
                length_ticks: length_changed.then_some(length),
                item_index: item_index_changed.then_some(item_index),
                raw_track_index: raw_track_index_changed.then_some(raw_track_index),
                group: group_changed.then_some(group),
                item_flags: item_flags_changed.then_some(item_flags),
                start_offset: start_offset_changed.then_some(start_offset),
                end_offset: end_offset_changed.then_some(end_offset),
                scale: scale_changed.then_some(scale),
            };
            if let Some(document) = self.document.as_mut() {
                match document.edit_playlist_clip(arrangement_id, index, edit) {
                    Ok(()) => {
                        self.stop_project_playback();
                        self.dirty = true;
                        self.status = "Playlist clip updated".to_owned();
                    }
                    Err(error) => {
                        clip_edit_succeeded = false;
                        self.status = format!("Could not update Playlist clip: {error}");
                    }
                }
            }
        }
        if duplicate_requested && clip_edit_succeeded {
            let duplicate_position = position.saturating_add(length);
            if let Some(document) = self.document.as_mut() {
                match document.duplicate_playlist_clip(
                    arrangement_id,
                    index,
                    Some(duplicate_position),
                    None,
                ) {
                    Ok(duplicate_index) => {
                        self.stop_project_playback();
                        self.selected_clip = Some(duplicate_index);
                        self.dirty = true;
                        self.status = format!("Duplicated Playlist clip {}", index + 1);
                    }
                    Err(error) => {
                        self.status = format!("Could not duplicate Playlist clip: {error}");
                    }
                }
            }
        }
    }

    fn time_marker_editor(
        &mut self,
        ui: &mut egui::Ui,
        arrangement_id: u16,
        markers: &[TimeMarker],
    ) {
        if self
            .selected_time_marker
            .is_some_and(|index| index >= markers.len())
        {
            self.selected_time_marker = None;
        }

        let selected_label = self
            .selected_time_marker
            .and_then(|index| markers.get(index).map(|marker| (index, marker)))
            .map(|(index, marker)| {
                marker.name().map(str::to_owned).unwrap_or_else(|| {
                    format!("Marker {} at {}", index + 1, marker.position_ticks())
                })
            })
            .unwrap_or_else(|| "Select marker".to_owned());

        let mut delete_selected = false;
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Time markers");
            egui::ComboBox::from_id_salt("playlist-time-marker-select")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.selected_time_marker, None, "Select marker");
                    for (index, marker) in markers.iter().enumerate() {
                        let label = marker.name().map(str::to_owned).unwrap_or_else(|| {
                            format!("Marker {} at {}", index + 1, marker.position_ticks())
                        });
                        ui.selectable_value(&mut self.selected_time_marker, Some(index), label);
                    }
                });
            delete_selected = ui
                .add_enabled(
                    self.selected_time_marker.is_some(),
                    egui::Button::new("Delete marker"),
                )
                .clicked();
        });

        let mut marker_edit = None;
        if let Some((index, marker)) = self
            .selected_time_marker
            .and_then(|index| markers.get(index).map(|marker| (index, marker)))
        {
            let mut position = marker.position_ticks();
            let mut name = marker.name().unwrap_or_default().to_owned();
            let mut numerator = marker.numerator().unwrap_or(4);
            let mut denominator = marker.denominator().unwrap_or(4);
            ui.horizontal(|ui| {
                ui.label("Position");
                let position_changed = ui
                    .add(egui::DragValue::new(&mut position).speed(1.0))
                    .changed();
                ui.label(if marker.is_signature() {
                    "Time signature"
                } else {
                    "Time marker"
                });
                let mut numerator_changed = false;
                let mut denominator_changed = false;
                if marker.is_signature() {
                    numerator_changed = ui
                        .add(egui::DragValue::new(&mut numerator).speed(1.0))
                        .changed();
                    ui.label("/");
                    denominator_changed = ui
                        .add(egui::DragValue::new(&mut denominator).speed(1.0))
                        .changed();
                }
                ui.label("Name");
                let name_changed = ui.text_edit_singleline(&mut name).changed();
                if position_changed || numerator_changed || denominator_changed || name_changed {
                    let meter_changed = numerator_changed || denominator_changed;
                    marker_edit = Some((
                        index,
                        TimeMarkerEdit {
                            position_ticks: position_changed.then_some(position),
                            numerator: (marker.is_signature() && meter_changed)
                                .then_some(numerator),
                            denominator: (marker.is_signature() && meter_changed)
                                .then_some(denominator),
                            name: name_changed.then_some(name),
                            ..TimeMarkerEdit::default()
                        },
                    ));
                }
            });
        }

        let mut create_marker = false;
        ui.horizontal(|ui| {
            ui.label("New");
            ui.label("Position");
            ui.add(egui::DragValue::new(&mut self.new_time_marker_position).speed(1.0));
            ui.checkbox(&mut self.new_time_marker_is_signature, "Time signature");
            if self.new_time_marker_is_signature {
                ui.add(egui::DragValue::new(&mut self.new_time_marker_numerator).speed(1.0));
                ui.label("/");
                ui.add(egui::DragValue::new(&mut self.new_time_marker_denominator).speed(1.0));
            }
            ui.text_edit_singleline(&mut self.new_time_marker_name);
            create_marker = ui.button("Add marker").clicked();
        });

        if delete_selected {
            if let Some(marker_index) = self.selected_time_marker {
                let deleted = self.document.as_mut().is_some_and(|document| {
                    document
                        .delete_time_marker(arrangement_id, marker_index)
                        .is_ok()
                });
                if deleted {
                    self.stop_project_playback();
                    self.selected_time_marker = None;
                    self.dirty = true;
                    self.status = format!("Deleted time marker {}", marker_index + 1);
                } else {
                    self.status = "Could not delete the selected time marker".to_owned();
                }
            }
        } else if create_marker {
            let edit = TimeMarkerEdit {
                position_ticks: Some(self.new_time_marker_position),
                is_signature: Some(self.new_time_marker_is_signature),
                numerator: self
                    .new_time_marker_is_signature
                    .then_some(self.new_time_marker_numerator.max(1)),
                denominator: self
                    .new_time_marker_is_signature
                    .then_some(self.new_time_marker_denominator.max(1)),
                name: (!self.new_time_marker_name.is_empty())
                    .then(|| self.new_time_marker_name.clone()),
            };
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    document
                        .create_time_marker(arrangement_id, edit)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(marker_index) => {
                    self.stop_project_playback();
                    self.selected_time_marker = Some(marker_index);
                    self.new_time_marker_name.clear();
                    self.dirty = true;
                    self.status = format!("Created time marker {}", marker_index + 1);
                }
                Err(error) => self.status = format!("Could not create time marker: {error}"),
            }
        } else if let Some((marker_index, edit)) = marker_edit {
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    document
                        .edit_time_marker(arrangement_id, marker_index, edit)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(()) => {
                    self.stop_project_playback();
                    self.dirty = true;
                    self.status = format!("Updated time marker {}", marker_index + 1);
                }
                Err(error) => self.status = format!("Could not update time marker: {error}"),
            }
        }
    }

    fn channel_rack(&mut self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            empty_view(ui, "Open a project to see its Channel Rack");
            return;
        };
        let channels = document.channels();
        let plugin_states = document.channel_plugin_states();
        let patterns = document.patterns().unwrap_or_default();
        if self
            .selected_graph_channel
            .is_none_or(|id| !channels.iter().any(|channel| channel.id() == id))
        {
            self.selected_graph_channel = channels.first().map(|channel| channel.id());
        }
        if self
            .selected_pattern
            .is_none_or(|id| !patterns.iter().any(|pattern| pattern.id == id))
        {
            self.selected_pattern = patterns.first().map(|pattern| pattern.id);
        }
        let ppq = u64::from(document.header().ppq().max(1));
        let (numerator, denominator) = document.metadata().time_signature().unwrap_or((4, 4));
        let measure_ticks = ppq
            .saturating_mul(u64::from(numerator.max(1)))
            .saturating_mul(4)
            .checked_div(u64::from(denominator.max(1)))
            .unwrap_or(1)
            .max(1);
        let step_ticks = (ppq / 4).max(1);
        let steps_per_bar = measure_ticks
            .saturating_add(step_ticks - 1)
            .checked_div(step_ticks)
            .unwrap_or(1)
            .clamp(1, 64) as usize;
        let mut open_editor = None;
        let mut level_edits = Vec::new();
        let mut layer_edits = Vec::new();
        let mut layer_flag_edits = Vec::new();
        let mut step_toggles = Vec::new();
        ui.horizontal(|ui| {
            ui.strong("Channel Rack");
            ui.separator();
            ui.label("Pattern");
            egui::ComboBox::from_id_salt("channel-rack-pattern-picker")
                .selected_text(format!("Pattern {}", self.selected_pattern.unwrap_or(0)))
                .show_ui(ui, |ui| {
                    for pattern in &patterns {
                        ui.selectable_value(
                            &mut self.selected_pattern,
                            Some(pattern.id),
                            pattern
                                .name
                                .as_deref()
                                .map_or_else(|| format!("Pattern {}", pattern.id), str::to_owned),
                        );
                    }
                });
            if ui.small_button("◀").clicked() {
                self.step_sequencer_bar = self.step_sequencer_bar.saturating_sub(1);
            }
            if ui.small_button("▶").clicked() {
                self.step_sequencer_bar = self.step_sequencer_bar.saturating_add(1);
            }
            ui.label(format!("Bar {}", self.step_sequencer_bar + 1));
            if ui
                .selectable_label(self.step_graph_editor_open, "Graph editor (Ctrl/Cmd+K)")
                .clicked()
            {
                self.step_graph_editor_open = !self.step_graph_editor_open;
            }
        });
        let selected_pattern = self
            .selected_pattern
            .and_then(|pattern_id| patterns.iter().find(|pattern| pattern.id == pattern_id))
            .cloned();
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for channel in &channels {
                let mut volume = channel.volume().unwrap_or(10_000);
                let mut pan = channel.pan().unwrap_or(6_400);
                let levels_editable = channel.levels_editable();
                let mut volume_changed = false;
                let mut pan_changed = false;
                let mut layer_child_apply = None;
                let mut layer_flags_apply = None;
                let layer_flags = channel.layer_flags();
                let mut layer_random = channel.layer_random_enabled().unwrap_or(false);
                let mut layer_crossfade = channel.layer_crossfade_enabled().unwrap_or(false);
                egui::Frame::new()
                    .fill(PANEL_DARK)
                    .inner_margin(4.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(if channel.enabled() == Some(false) {
                                "○"
                            } else {
                                "●"
                            });
                            let plugin_state = plugin_states
                                .iter()
                                .find(|state| state.channel_id() == channel.id());
                            let selected = self.selected_graph_channel == Some(channel.id());
                            let channel_button = ui.add_sized(
                                [190.0, 24.0],
                                egui::Button::new(
                                    channel.display_name().unwrap_or("(unnamed channel)"),
                                )
                                .fill(if selected { BLUE } else { PANEL_LIGHT }),
                            );
                            if channel_button.clicked() {
                                self.selected_graph_channel = Some(channel.id());
                                self.selected_note_channel = Some(channel.id());
                            }
                            ui.label(
                                plugin_state
                                    .and_then(|state| state.vst_metadata())
                                    .and_then(VstPluginStateMetadata::name)
                                    .or(channel.plugin_identifier())
                                    .unwrap_or("Audio"),
                            );
                            ui.add_space(8.0);
                            volume_changed = ui
                                .add_enabled_ui(levels_editable, |ui| {
                                    ui.add_sized(
                                        [96.0, 22.0],
                                        egui::Slider::new(&mut volume, 0..=12_800)
                                            .show_value(false),
                                    )
                                })
                                .inner
                                .changed();
                            ui.label(format!("VOL {volume}"));
                            pan_changed = ui
                                .add_enabled_ui(levels_editable, |ui| {
                                    ui.add_sized(
                                        [96.0, 22.0],
                                        egui::Slider::new(&mut pan, 0..=12_800).show_value(false),
                                    )
                                })
                                .inner
                                .changed();
                            ui.label(format!("PAN {pan}"));
                            ui.separator();
                            let state_bytes =
                                plugin_state.map_or(0, |state| state.data_payload().len());
                            if state_bytes > 0 {
                                ui.small(format!("state {state_bytes} B"));
                            }
                            let loaded_instance =
                                self.channel_vst3_instances.get(&channel.id()).copied();
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if let Some(instance_id) = loaded_instance {
                                        if ui.button("Plug-in editor").clicked() {
                                            open_editor = Some(instance_id);
                                        }
                                    } else {
                                        ui.add_enabled(false, egui::Button::new("Plug-in editor"));
                                    }
                                },
                            );
                        });
                        if let Some(mut selected_children) =
                            channel.layer_child_ids().map(<[u16]>::to_vec)
                        {
                            let mut children_changed = false;
                            let selected_text =
                                layer_child_selection_label(&selected_children, &channels);
                            ui.horizontal(|ui| {
                                ui.label("Children");
                                egui::ComboBox::from_id_salt(("layer-child-select", channel.id()))
                                    .selected_text(selected_text)
                                    .show_ui(ui, |ui| {
                                        for child in &channels {
                                            let child_id = child.id();
                                            let mut selected =
                                                selected_children.contains(&child_id);
                                            let label = format!(
                                                "{} (ID {child_id})",
                                                child.display_name().unwrap_or("(unnamed channel)")
                                            );
                                            if ui.checkbox(&mut selected, label).changed() {
                                                update_layer_child_selection(
                                                    &mut selected_children,
                                                    child_id,
                                                    selected,
                                                );
                                                children_changed = true;
                                            }
                                        }
                                        let missing_ids = selected_children
                                            .iter()
                                            .copied()
                                            .filter(|child_id| {
                                                !channels
                                                    .iter()
                                                    .any(|child| child.id() == *child_id)
                                            })
                                            .collect::<Vec<_>>();
                                        for child_id in missing_ids {
                                            let mut selected = true;
                                            if ui
                                                .checkbox(
                                                    &mut selected,
                                                    format!("Missing channel (ID {child_id})"),
                                                )
                                                .changed()
                                            {
                                                update_layer_child_selection(
                                                    &mut selected_children,
                                                    child_id,
                                                    selected,
                                                );
                                                children_changed = true;
                                            }
                                        }
                                    });
                                if ui.small_button("Clear").clicked() {
                                    selected_children.clear();
                                    children_changed = true;
                                }
                            });
                            if children_changed {
                                layer_child_apply = Some(selected_children);
                            }
                        }
                        if channel.layer_child_ids().is_some() {
                            ui.horizontal(|ui| {
                                ui.label("Layer flags");
                                let random_changed = ui
                                    .add_enabled(
                                        layer_flags.is_some(),
                                        egui::Checkbox::new(&mut layer_random, "Random"),
                                    )
                                    .changed();
                                let crossfade_changed = ui
                                    .add_enabled(
                                        layer_flags.is_some(),
                                        egui::Checkbox::new(&mut layer_crossfade, "Crossfade"),
                                    )
                                    .changed();
                                if random_changed || crossfade_changed {
                                    layer_flags_apply = Some((
                                        random_changed.then_some(layer_random),
                                        crossfade_changed.then_some(layer_crossfade),
                                    ));
                                }
                                if layer_flags.is_none() {
                                    ui.small("No recognized flags event");
                                }
                            });
                        }
                    });
                if volume_changed || pan_changed {
                    level_edits.push((channel.id(), volume, pan));
                }
                if let Some(children) = layer_child_apply {
                    layer_edits.push((channel.id(), children));
                }
                if let Some((random, crossfade)) = layer_flags_apply {
                    layer_flag_edits.push((channel.id(), random, crossfade));
                }
                if let Some(pattern) = &selected_pattern {
                    ui.horizontal_wrapped(|ui| {
                        ui.small("Steps");
                        let mut channel_note_index = 0usize;
                        for step in 0..steps_per_bar {
                            let tick = u64::from(self.step_sequencer_bar)
                                .saturating_mul(measure_ticks)
                                .saturating_add((step as u64).saturating_mul(step_ticks));
                            let position = tick.min(u64::from(u32::MAX)) as u32;
                            let step_end = tick.saturating_add(step_ticks);
                            let mut active_note = None;
                            for note in pattern
                                .notes
                                .iter()
                                .filter(|note| note.channel_id == channel.id())
                            {
                                let note_tick = u64::from(note.position);
                                if note_tick >= tick && note_tick < step_end {
                                    active_note = Some(channel_note_index);
                                    break;
                                }
                                channel_note_index += 1;
                            }
                            let active = active_note.is_some();
                            if ui
                                .selectable_label(active, format!("{:02}", step + 1))
                                .clicked()
                            {
                                step_toggles.push((channel.id(), active_note, position));
                            }
                            channel_note_index = 0;
                        }
                    });
                }
                ui.add_space(2.0);
            }
            ui.separator();
            ui.label(
                egui::RichText::new(
                    "Click a step to add or remove a C5 note at sixteenth-note resolution. Use the graph editor for note and event values.",
                )
                    .color(MUTED),
            );
            if self.step_graph_editor_open {
                if let Some(pattern) = selected_pattern.as_ref() {
                    self.step_graph_editor(
                        ui,
                        pattern,
                        &channels,
                        steps_per_bar,
                        measure_ticks,
                        step_ticks,
                    );
                } else {
                    ui.small("Select or create a pattern to edit its Graph Editor values.");
                }
            }
        });
        if let Some(pattern_id) = self.selected_pattern
            && !step_toggles.is_empty()
        {
            let results = if let Some(document) = self.document.as_mut() {
                step_toggles
                    .into_iter()
                    .map(|(channel_id, note_index, position)| {
                        let removing = note_index.is_some();
                        let result = if let Some(note_index) = note_index {
                            document.delete_pattern_note(pattern_id, channel_id, note_index)
                        } else {
                            document.add_pattern_note(
                                pattern_id,
                                PatternNote {
                                    position,
                                    channel_id,
                                    length: step_ticks.min(u64::from(u32::MAX)) as u32,
                                    key: 60,
                                    velocity: 100,
                                    ..PatternNote::default()
                                },
                            )
                        };
                        (
                            channel_id,
                            removing,
                            result.map_err(|error| error.to_string()),
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            for (channel_id, removing, result) in results {
                match result {
                    Ok(()) => {
                        self.stop_project_playback();
                        self.dirty = true;
                        self.status = if removing {
                            format!("Removed step from pattern {pattern_id}, channel {channel_id}")
                        } else {
                            format!("Added step to pattern {pattern_id}, channel {channel_id}")
                        };
                    }
                    Err(error) => {
                        self.status = format!("Could not edit step: {error}");
                    }
                }
            }
        }
        if !level_edits.is_empty()
            && let Some(document) = self.document.as_mut()
        {
            for (channel_id, volume, pan) in level_edits {
                match document.set_channel_levels(channel_id, volume, pan) {
                    Ok(()) => {
                        self.dirty = true;
                        self.status = format!("Channel {channel_id} volume and pan updated");
                    }
                    Err(error) => self.status = format!("Could not update channel levels: {error}"),
                }
            }
        }
        if !layer_edits.is_empty()
            && let Some(document) = self.document.as_mut()
        {
            for (channel_id, child_ids) in layer_edits {
                match document.set_layer_child_ids(channel_id, &child_ids) {
                    Ok(()) => {
                        self.dirty = true;
                        self.status = format!("Layer channel {channel_id} children updated");
                    }
                    Err(error) => {
                        self.status = format!("Could not update Layer children: {error}");
                    }
                }
            }
        }
        if !layer_flag_edits.is_empty()
            && let Some(document) = self.document.as_mut()
        {
            for (channel_id, random, crossfade) in layer_flag_edits {
                match document.set_layer_flags(channel_id, random, crossfade) {
                    Ok(()) => {
                        self.dirty = true;
                        self.status = format!("Layer channel {channel_id} flags updated");
                    }
                    Err(error) => {
                        self.status = format!("Could not update Layer flags: {error}");
                    }
                }
            }
        }
        if let Some(instance_id) = open_editor
            && let Some(host) = &mut self.vst3_host
        {
            match host.open_editor(instance_id) {
                Ok(()) => self.status = "VST3 editor opened from Channel Rack".to_owned(),
                Err(error) => self.status = format!("Could not open VST3 editor: {error}"),
            }
        }
    }

    fn step_graph_editor(
        &mut self,
        ui: &mut egui::Ui,
        pattern: &Pattern,
        channels: &[ChannelSummary],
        steps_per_bar: usize,
        measure_ticks: u64,
        step_ticks: u64,
    ) {
        if channels.is_empty() {
            ui.small("No channels are available for the Graph Editor.");
            return;
        }
        if self
            .selected_graph_channel
            .is_none_or(|id| !channels.iter().any(|channel| channel.id() == id))
        {
            self.selected_graph_channel = channels.first().map(|channel| channel.id());
        }

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.strong("Graph Editor");
            ui.separator();
            let selected_channel_name = self
                .selected_graph_channel
                .and_then(|id| channels.iter().find(|channel| channel.id() == id))
                .and_then(ChannelSummary::display_name)
                .unwrap_or("Channel");
            egui::ComboBox::from_id_salt(("step-graph-channel", pattern.id))
                .selected_text(selected_channel_name)
                .show_ui(ui, |ui| {
                    for channel in channels {
                        ui.selectable_value(
                            &mut self.selected_graph_channel,
                            Some(channel.id()),
                            format!(
                                "{} · {}",
                                channel.display_name().unwrap_or("Channel"),
                                channel.id()
                            ),
                        );
                    }
                });
            ui.separator();
            for mode in StepGraphMode::ALL {
                ui.selectable_value(&mut self.step_graph_mode, mode, mode.label());
            }
        });
        let Some(channel_id) = self.selected_graph_channel else {
            return;
        };
        let channel_name = channels
            .iter()
            .find(|channel| channel.id() == channel_id)
            .and_then(ChannelSummary::display_name)
            .unwrap_or("Channel");
        ui.small(format!(
            "Pattern {} · {} · bar {} · click or drag to set {}",
            pattern.id,
            channel_name,
            self.step_sequencer_bar + 1,
            self.step_graph_mode.label().to_lowercase()
        ));

        let graph_size = Vec2::new(ui.available_width().max(280.0), 142.0);
        let (graph_rect, graph_response) =
            ui.allocate_exact_size(graph_size, Sense::click_and_drag());
        let graph_response = graph_response.on_hover_text(
            "Left-click or drag to edit. Right-drag ramps values across steps. Clicking an empty step adds a C5 note.",
        );
        let painter = ui.painter_at(graph_rect);
        painter.rect_filled(graph_rect, egui::CornerRadius::same(3), PANEL_DARK);
        painter.rect_stroke(
            graph_rect,
            egui::CornerRadius::same(3),
            Stroke::new(1.0, GRID),
            egui::StrokeKind::Inside,
        );
        let plot_rect = egui::Rect::from_min_max(
            egui::pos2(graph_rect.left() + 3.0, graph_rect.top() + 18.0),
            egui::pos2(graph_rect.right() - 3.0, graph_rect.bottom() - 17.0),
        );
        let step_count = steps_per_bar.max(1);
        let step_width = plot_rect.width() / step_count as f32;
        let mode = self.step_graph_mode;
        let maximum = mode.maximum();
        let notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
            .collect::<Vec<_>>();

        for step in 0..step_count {
            let x0 = plot_rect.left() + step as f32 * step_width;
            let x1 = plot_rect.left() + (step + 1) as f32 * step_width;
            let cell = egui::Rect::from_min_max(
                egui::pos2(x0, plot_rect.top()),
                egui::pos2(x1, plot_rect.bottom()),
            );
            painter.rect_filled(
                cell,
                0,
                if step % 2 == 0 {
                    PANEL_DARK
                } else {
                    PANEL.gamma_multiply(0.7)
                },
            );
            let tick = u64::from(self.step_sequencer_bar)
                .saturating_mul(measure_ticks)
                .saturating_add((step as u64).saturating_mul(step_ticks));
            let step_end = tick.saturating_add(step_ticks);
            let active_note = notes
                .iter()
                .find(|(_, note)| {
                    let note_tick = u64::from(note.position);
                    note_tick >= tick && note_tick < step_end
                })
                .map(|(note_index, note)| (*note_index, *note));

            painter.line_segment(
                [
                    egui::pos2(x0, plot_rect.top()),
                    egui::pos2(x0, plot_rect.bottom()),
                ],
                Stroke::new(if step % 4 == 0 { 1.0 } else { 0.5 }, GRID),
            );
            if let Some((_, note)) = active_note {
                let value = mode.value(note, tick, step_ticks);
                let value_y = plot_rect.bottom() - value / maximum * plot_rect.height();
                let baseline_y = mode.center().map_or(plot_rect.bottom(), |center| {
                    plot_rect.bottom() - center / maximum * plot_rect.height()
                });
                let mut top = value_y.min(baseline_y);
                let mut bottom = value_y.max(baseline_y);
                if bottom - top < 2.0 {
                    top = (top - 1.0).max(plot_rect.top());
                    bottom = (bottom + 1.0).min(plot_rect.bottom());
                }
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0 + 2.0, top),
                        egui::pos2((x1 - 2.0).max(x0 + 3.0), bottom),
                    ),
                    1,
                    ORANGE,
                );
            }
            if step % 4 == 0 || step_count <= 16 {
                painter.text(
                    egui::pos2((x0 + x1) * 0.5, plot_rect.bottom() + 3.0),
                    Align2::CENTER_TOP,
                    format!("{:02}", step + 1),
                    FontId::proportional(8.0),
                    MUTED,
                );
            }
        }
        painter.line_segment(
            [
                egui::pos2(plot_rect.right(), plot_rect.top()),
                egui::pos2(plot_rect.right(), plot_rect.bottom()),
            ],
            Stroke::new(1.0, GRID),
        );
        if let Some(center) = mode.center() {
            let y = plot_rect.bottom() - center / maximum * plot_rect.height();
            painter.line_segment(
                [
                    egui::pos2(plot_rect.left(), y),
                    egui::pos2(plot_rect.right(), y),
                ],
                Stroke::new(1.0, MUTED.gamma_multiply(0.8)),
            );
        }

        let pointer = graph_response.interact_pointer_pos();
        let pointer_value = |pointer: egui::Pos2| {
            ((plot_rect.bottom() - pointer.y) / plot_rect.height() * maximum).clamp(0.0, maximum)
        };
        let step_at = |pointer: egui::Pos2| {
            (((pointer.x - plot_rect.left()) / step_width).floor() as usize).min(step_count - 1)
        };
        let secondary_pressed =
            ui.input(|input| input.pointer.button_pressed(PointerButton::Secondary));
        let secondary_down = ui.input(|input| input.pointer.button_down(PointerButton::Secondary));
        let secondary_released =
            ui.input(|input| input.pointer.button_released(PointerButton::Secondary));
        if secondary_pressed
            && let Some(pointer) = pointer
            && plot_rect.contains(pointer)
        {
            self.step_graph_ramp = None;
            let start_step = step_at(pointer);
            let tick = u64::from(self.step_sequencer_bar)
                .saturating_mul(measure_ticks)
                .saturating_add((start_step as u64).saturating_mul(step_ticks));
            let step_end = tick.saturating_add(step_ticks);
            if let Some((_, note)) = notes.iter().find(|(_, note)| {
                let note_tick = u64::from(note.position);
                note_tick >= tick && note_tick < step_end
            }) {
                self.step_graph_ramp = Some(StepGraphRamp {
                    pattern_id: pattern.id,
                    channel_id,
                    mode,
                    start_step,
                    start_value: mode.value(note, tick, step_ticks),
                });
            }
        }
        if self.step_graph_ramp.is_some_and(|ramp| {
            ramp.pattern_id != pattern.id || ramp.channel_id != channel_id || ramp.mode != mode
        }) {
            self.step_graph_ramp = None;
        }

        let mut graph_actions = Vec::new();
        if (graph_response.clicked_by(PointerButton::Primary)
            || graph_response.dragged_by(PointerButton::Primary))
            && let Some(pointer) = graph_response.interact_pointer_pos()
            && plot_rect.contains(pointer)
        {
            let step = step_at(pointer);
            let tick = u64::from(self.step_sequencer_bar)
                .saturating_mul(measure_ticks)
                .saturating_add((step as u64).saturating_mul(step_ticks));
            let position = tick.min(u64::from(u32::MAX)) as u32;
            let step_end = tick.saturating_add(step_ticks);
            let note_index = notes
                .iter()
                .find(|(_, note)| {
                    let note_tick = u64::from(note.position);
                    note_tick >= tick && note_tick < step_end
                })
                .map(|(note_index, _)| *note_index);
            graph_actions.push((step, position, note_index, pointer_value(pointer)));
        }

        if graph_response.dragged_by(PointerButton::Secondary)
            && secondary_down
            && let (Some(ramp), Some(pointer)) = (self.step_graph_ramp, pointer)
        {
            let end_step = step_at(pointer);
            let end_value = pointer_value(pointer);
            let denominator = end_step as f32 - ramp.start_step as f32;
            for step in ramp.start_step.min(end_step)..=ramp.start_step.max(end_step) {
                let progress = if denominator == 0.0 {
                    0.0
                } else {
                    (step as f32 - ramp.start_step as f32) / denominator
                };
                let value = ramp.start_value + (end_value - ramp.start_value) * progress;
                let tick = u64::from(self.step_sequencer_bar)
                    .saturating_mul(measure_ticks)
                    .saturating_add((step as u64).saturating_mul(step_ticks));
                let step_end = tick.saturating_add(step_ticks);
                let note_index = notes
                    .iter()
                    .find(|(_, note)| {
                        let note_tick = u64::from(note.position);
                        note_tick >= tick && note_tick < step_end
                    })
                    .map(|(note_index, _)| *note_index);
                if let Some(note_index) = note_index {
                    graph_actions.push((
                        step,
                        tick.min(u64::from(u32::MAX)) as u32,
                        Some(note_index),
                        value.clamp(0.0, maximum),
                    ));
                }
            }
        }
        if secondary_released {
            self.step_graph_ramp = None;
        }

        if !graph_actions.is_empty() {
            let pattern_id = pattern.id;
            let mut updated = self.document.clone();
            let result = if let Some(document) = updated.as_mut() {
                graph_actions.iter().try_for_each(
                    |(_, position, note_index, value)| -> Result<(), _> {
                        let edit = mode.edit(*value, u64::from(*position), step_ticks);
                        if let Some(note_index) = note_index {
                            document.edit_pattern_note(pattern_id, channel_id, *note_index, edit)
                        } else {
                            let mut note = PatternNote {
                                position: *position,
                                channel_id,
                                length: step_ticks.min(u64::from(u32::MAX)) as u32,
                                key: 60,
                                velocity: 100,
                                ..PatternNote::default()
                            };
                            mode.apply_to_note(&mut note, *value, u64::from(*position), step_ticks);
                            document.add_pattern_note(pattern_id, note)
                        }
                    },
                )
            } else {
                return;
            };
            match result {
                Ok(()) => {
                    self.document = updated;
                    self.stop_project_playback();
                    self.dirty = true;
                    let first_step = graph_actions
                        .first()
                        .map(|action| action.0 + 1)
                        .unwrap_or(1);
                    let last_step = graph_actions
                        .last()
                        .map(|action| action.0 + 1)
                        .unwrap_or(first_step);
                    self.status = if graph_actions.len() > 1 {
                        format!(
                            "Ramped {} across bar {} steps {}–{} in pattern {}",
                            mode.label().to_lowercase(),
                            self.step_sequencer_bar + 1,
                            first_step.min(last_step),
                            first_step.max(last_step),
                            pattern_id
                        )
                    } else {
                        format!(
                            "Updated {} on bar {} step {} in pattern {}",
                            mode.label().to_lowercase(),
                            self.step_sequencer_bar + 1,
                            first_step,
                            pattern_id
                        )
                    };
                }
                Err(error) => {
                    self.status = format!("Could not update Graph Editor value: {error}");
                }
            }
        }
    }

    fn automation_editor(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.document.as_ref() else {
            empty_view(ui, "Open an FL Studio project to edit automation curves");
            return;
        };
        let automation_channels = match document.automation_channels() {
            Ok(channels) if !channels.is_empty() => channels,
            Ok(_) => {
                empty_view(ui, "This project has no decoded automation channels");
                return;
            }
            Err(error) => {
                ui.colored_label(ORANGE, format!("Could not decode automation: {error}"));
                return;
            }
        };

        let first_channel_id = automation_channels[0].channel_id();
        let mut channel_id = self
            .selected_automation_channel
            .filter(|id| {
                automation_channels
                    .iter()
                    .any(|channel| channel.channel_id() == *id)
            })
            .unwrap_or(first_channel_id);
        let old_channel_id = self.selected_automation_channel;
        let mut action = None;
        ui.horizontal(|ui| {
            ui.strong("Automation");
            ui.separator();
            egui::ComboBox::from_id_salt("automation-channel-select")
                .selected_text(
                    automation_channels
                        .iter()
                        .find(|channel| channel.channel_id() == channel_id)
                        .and_then(AutomationChannel::display_name)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Channel {channel_id}")),
                )
                .show_ui(ui, |ui| {
                    for channel in &automation_channels {
                        let label = channel
                            .display_name()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Channel {}", channel.channel_id()));
                        ui.selectable_value(&mut channel_id, channel.channel_id(), label);
                    }
                });

            let has_blob = automation_channels
                .iter()
                .find(|channel| channel.channel_id() == channel_id)
                .is_some_and(|channel| channel.data_event_index().is_some());
            let add = ui.add_enabled_ui(has_blob, |ui| {
                ui.selectable_label(self.automation_add_mode, "Add point")
            });
            if add.inner.clicked() {
                self.automation_add_mode = !self.automation_add_mode;
                self.selected_automation_point = None;
            }
            let can_delete = self.selected_automation_point.is_some();
            if ui
                .add_enabled(can_delete, egui::Button::new("Delete point"))
                .clicked()
                && let Some(point_index) = self.selected_automation_point
            {
                action = Some(AutomationEditAction::Delete { point_index });
            }
            ui.checkbox(&mut self.automation_snap, "Snap 1/4 beat");
            ui.add(egui::Slider::new(&mut self.automation_visible_beats, 4.0..=64.0).text("Span"));
        });
        if old_channel_id != Some(channel_id) {
            self.selected_automation_point = None;
            self.automation_add_mode = false;
            action = None;
        }
        self.selected_automation_channel = Some(channel_id);

        let Some(channel) = automation_channels
            .iter()
            .find(|channel| channel.channel_id() == channel_id)
        else {
            empty_view(ui, "Select an automation channel");
            return;
        };
        let points = channel.points();
        let points_are_editable = channel.data_event_index().is_some()
            && points.iter().all(|point| {
                point.position_beats().is_finite()
                    && point.position_beats() >= 0.0
                    && point.value().is_finite()
                    && point.tension().is_finite()
            })
            && points
                .windows(2)
                .all(|pair| pair[0].position_beats() <= pair[1].position_beats());
        let selected_index = self
            .selected_automation_point
            .filter(|index| *index < points.len());
        self.selected_automation_point = selected_index;
        ui.horizontal(|ui| {
            ui.label(format!("{} points", points.len()));
            if channel.data_event_index().is_none() {
                ui.colored_label(
                    MUTED,
                    "This channel has no existing 0xEA curve blob to edit",
                );
            } else if !points_are_editable {
                ui.colored_label(
                    ORANGE,
                    "Curve has invalid or unordered values; editing is disabled",
                );
            }
            if self.automation_add_mode {
                ui.colored_label(ORANGE, "Click inside the graph to insert a point");
            } else {
                ui.colored_label(MUTED, "Drag a point to edit its position and value");
            }
            ui.colored_label(MUTED, "Straight-line preview; tension is not rendered yet");
        });

        if points_are_editable && let Some(point_index) = selected_index {
            let point = &points[point_index];
            let mut position = point.position_beats();
            let mut value = point.value();
            let mut tension = point.tension();
            let position_changed = ui
                .add(
                    egui::DragValue::new(&mut position)
                        .speed(0.01)
                        .prefix("Position ")
                        .suffix(" beats"),
                )
                .changed();
            let value_changed = ui
                .add(egui::Slider::new(&mut value, 0.0..=1.0).text("Value"))
                .changed();
            let tension_changed = ui
                .add(egui::Slider::new(&mut tension, -1.0..=1.0).text("Tension"))
                .changed();
            if position_changed || value_changed || tension_changed {
                action = Some(AutomationEditAction::Edit {
                    point_index,
                    edit: AutomationPointEdit {
                        position_beats: position_changed.then_some(position),
                        value: value_changed.then_some(value),
                        tension: tension_changed.then_some(tension),
                    },
                });
            }
        }

        let chart_height = ui.available_height().clamp(230.0, 430.0);
        let (canvas_response, painter) = ui.allocate_painter(
            Vec2::new(ui.available_width().max(1.0), chart_height),
            Sense::click(),
        );
        let canvas_rect = canvas_response.rect;
        painter.rect_filled(canvas_rect, egui::CornerRadius::same(2), PANEL_DARK);
        painter.rect_stroke(
            canvas_rect,
            egui::CornerRadius::same(2),
            Stroke::new(1.0, GRID),
            egui::StrokeKind::Inside,
        );
        let plot_rect = egui::Rect::from_min_max(
            egui::pos2(canvas_rect.left() + 48.0, canvas_rect.top() + 12.0),
            egui::pos2(canvas_rect.right() - 12.0, canvas_rect.bottom() - 25.0),
        );
        let visible_beats = self.automation_visible_beats.max(4.0);
        for division in 0..=4 {
            let value = division as f32 / 4.0;
            let y = egui::lerp(plot_rect.bottom()..=plot_rect.top(), value);
            painter.line_segment(
                [
                    egui::pos2(plot_rect.left(), y),
                    egui::pos2(plot_rect.right(), y),
                ],
                Stroke::new(1.0, GRID),
            );
            painter.text(
                egui::pos2(canvas_rect.left() + 5.0, y),
                Align2::LEFT_CENTER,
                format!("{value:.2}"),
                FontId::proportional(10.0),
                MUTED,
            );
        }
        let beat_step = if visible_beats <= 32.0 { 1.0 } else { 4.0 };
        let beat_divisions = (visible_beats / beat_step).ceil() as usize;
        for division in 0..=beat_divisions {
            let beat = division as f64 * beat_step;
            let x = egui::lerp(
                plot_rect.left()..=plot_rect.right(),
                (beat / visible_beats) as f32,
            );
            painter.line_segment(
                [
                    egui::pos2(x, plot_rect.top()),
                    egui::pos2(x, plot_rect.bottom()),
                ],
                Stroke::new(1.0, GRID),
            );
            painter.text(
                egui::pos2(x + 3.0, canvas_rect.bottom() - 13.0),
                Align2::LEFT_CENTER,
                format!("{beat:.0}"),
                FontId::proportional(10.0),
                MUTED,
            );
        }
        let point_position = |point: &AutomationPoint| {
            automation_point_screen_position(point, plot_rect, visible_beats)
        };
        for pair in points.windows(2) {
            painter.line_segment(
                [point_position(&pair[0]), point_position(&pair[1])],
                Stroke::new(1.5, ORANGE),
            );
        }
        let mut interacted_with_point = false;
        for (point_index, point) in points.iter().enumerate() {
            let center = point_position(point);
            let hit_rect = egui::Rect::from_center_size(center, Vec2::splat(14.0));
            let response = ui.interact(
                hit_rect,
                Id::new(("automation-point", channel_id, point_index)),
                if points_are_editable {
                    Sense::click_and_drag()
                } else {
                    Sense::hover()
                },
            );
            let selected = selected_index == Some(point_index);
            painter.circle_filled(
                center,
                if selected { 5.5 } else { 4.5 },
                if selected { Color32::WHITE } else { PURPLE },
            );
            painter.circle_stroke(
                center,
                if selected { 6.0 } else { 5.0 },
                Stroke::new(1.0, if selected { ORANGE } else { TEXT }),
            );
            if response.clicked() {
                interacted_with_point = true;
                self.selected_automation_point = Some(point_index);
            }
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                interacted_with_point = true;
                self.selected_automation_point = Some(point_index);
                let mut position =
                    f64::from(((pointer.x - plot_rect.left()) / plot_rect.width()).clamp(0.0, 1.0))
                        * visible_beats;
                if self.automation_snap {
                    position = (position * 4.0).round() / 4.0;
                }
                let previous_position = if point_index == 0 {
                    0.0
                } else {
                    points[point_index - 1].position_beats()
                };
                let next_position = points
                    .get(point_index + 1)
                    .map(AutomationPoint::position_beats)
                    .unwrap_or_else(|| visible_beats.max(point.position_beats()));
                let value = f64::from(
                    ((plot_rect.bottom() - pointer.y) / plot_rect.height()).clamp(0.0, 1.0),
                );
                if previous_position.is_finite()
                    && next_position.is_finite()
                    && previous_position <= next_position
                {
                    position = position.clamp(previous_position, next_position);
                }
                if previous_position.is_finite()
                    && next_position.is_finite()
                    && previous_position <= next_position
                    && (position != point.position_beats() || value != point.value())
                {
                    action = Some(AutomationEditAction::Edit {
                        point_index,
                        edit: AutomationPointEdit {
                            position_beats: Some(position),
                            value: Some(value),
                            ..AutomationPointEdit::default()
                        },
                    });
                }
            }
        }
        if canvas_response.clicked() && !interacted_with_point {
            if self.automation_add_mode
                && points_are_editable
                && let Some(pointer) = canvas_response.interact_pointer_pos()
                && plot_rect.contains(pointer)
            {
                let mut position =
                    f64::from(((pointer.x - plot_rect.left()) / plot_rect.width()).clamp(0.0, 1.0))
                        * visible_beats;
                if self.automation_snap {
                    position = (position * 4.0).round() / 4.0;
                }
                let value = f64::from(
                    ((plot_rect.bottom() - pointer.y) / plot_rect.height()).clamp(0.0, 1.0),
                );
                let slot = points.partition_point(|point| point.position_beats() <= position);
                action = Some(AutomationEditAction::Insert {
                    slot,
                    position_beats: position,
                    value,
                    tension: 0.0,
                });
            } else {
                self.selected_automation_point = None;
            }
        }

        if let Some(action) = action
            && let Some(document) = self.document.as_mut()
        {
            let result = match action {
                AutomationEditAction::Insert {
                    slot,
                    position_beats,
                    value,
                    tension,
                } => {
                    self.selected_automation_point = Some(slot);
                    document.insert_automation_point(
                        channel_id,
                        slot,
                        position_beats,
                        value,
                        tension,
                    )
                }
                AutomationEditAction::Edit { point_index, edit } => {
                    self.selected_automation_point = Some(point_index);
                    document.edit_automation_point(channel_id, point_index, edit)
                }
                AutomationEditAction::Delete { point_index } => {
                    self.selected_automation_point = None;
                    self.automation_add_mode = false;
                    document.delete_automation_point(channel_id, point_index)
                }
            };
            match result {
                Ok(()) => {
                    self.dirty = true;
                    self.status = "Automation curve updated".to_owned();
                }
                Err(error) => self.status = format!("Could not edit automation: {error}"),
            }
        }
    }

    fn apply_piano_roll_selection_command(&mut self, command: PianoRollSelectionCommand) {
        let target = self.selected_pattern.zip(self.selected_note_channel);
        let note_ids = target
            .and_then(|(pattern_id, channel_id)| {
                self.document.as_ref().and_then(|document| {
                    document.patterns().ok().and_then(|patterns| {
                        patterns
                            .iter()
                            .find(|pattern| pattern.id == pattern_id)
                            .map(|pattern| {
                                pattern
                                    .notes
                                    .iter()
                                    .filter(|note| note.channel_id == channel_id)
                                    .enumerate()
                                    .map(|(note_index, _)| (pattern_id, channel_id, note_index))
                                    .collect::<Vec<_>>()
                            })
                    })
                })
            })
            .unwrap_or_default();

        let previous = self
            .selected_piano_notes
            .iter()
            .filter(|(pattern_id, channel_id, _)| {
                target.is_some_and(|target| (*pattern_id, *channel_id) == target)
            })
            .copied()
            .collect::<BTreeSet<_>>();
        self.selected_piano_notes.clear();
        match command {
            PianoRollSelectionCommand::All => {
                self.selected_piano_notes.extend(note_ids);
            }
            PianoRollSelectionCommand::Invert => {
                self.selected_piano_notes.extend(
                    note_ids
                        .into_iter()
                        .filter(|note_id| !previous.contains(note_id)),
                );
            }
            PianoRollSelectionCommand::Clear => {}
        }
        self.selected_note = self.selected_piano_notes.iter().next_back().copied();
        self.piano_roll_selection_drag = None;
        self.active_note_drag = None;
        self.status = match command {
            PianoRollSelectionCommand::All => {
                format!("Selected {} notes", self.selected_piano_notes.len())
            }
            PianoRollSelectionCommand::Invert => {
                format!(
                    "Inverted selection: {} notes selected",
                    self.selected_piano_notes.len()
                )
            }
            PianoRollSelectionCommand::Clear => "Note selection cleared".to_owned(),
        };
    }

    fn piano_roll(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.document.as_ref() else {
            empty_view(ui, "Open a project to see its Piano roll");
            return;
        };
        let patterns = document.patterns().unwrap_or_default();
        let channels = document.channels();
        let ppq = document.header().ppq().max(1);
        let time_signature = document.metadata().time_signature();
        if patterns.is_empty() {
            empty_view(ui, "This project has no decoded patterns");
            return;
        }
        if self
            .selected_pattern
            .is_none_or(|id| !patterns.iter().any(|pattern| pattern.id == id))
        {
            self.selected_pattern = patterns.first().map(|pattern| pattern.id);
        }
        if self
            .active_note_drag
            .as_ref()
            .is_some_and(|drag| Some(drag.pattern_id) != self.selected_pattern)
        {
            self.active_note_drag = None;
        }
        if self
            .selected_note_channel
            .is_none_or(|id| !channels.iter().any(|channel| channel.id() == id))
        {
            self.selected_note_channel = channels.first().map(|channel| channel.id());
        }
        if self
            .piano_roll_selection_drag
            .is_some_and(|drag| Some(drag.pattern_id) != self.selected_pattern)
        {
            self.piano_roll_selection_drag = None;
        }
        let selected_channel_label = self
            .selected_note_channel
            .and_then(|id| channels.iter().find(|channel| channel.id() == id))
            .map(|channel| {
                channel
                    .display_name()
                    .or(channel.plugin_identifier())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Channel {}", channel.id()))
            })
            .unwrap_or_else(|| "No channel".to_owned());
        let sampler_channel_ids: BTreeSet<_> = channels
            .iter()
            .filter(|channel| channel.kind() == Some(0) && channel.enabled() != Some(false))
            .map(|channel| channel.id())
            .collect();
        let has_sampler_notes = self
            .selected_pattern
            .and_then(|pattern_id| patterns.iter().find(|pattern| pattern.id == pattern_id))
            .is_some_and(|pattern| {
                pattern
                    .notes
                    .iter()
                    .any(|note| sampler_channel_ids.contains(&note.channel_id))
            });
        let mut add_note_requested = false;
        let mut open_midi_requested = false;
        let mut export_midi_requested = false;
        let mut render_requested = false;
        let mut preview_requested = false;
        let mut sampler_preview_requested = false;
        let mut quantize_requested = false;
        let mut legato_requested = false;
        let mut chop_requested = false;
        let mut glue_requested = false;
        let mut flip_requested = false;
        let mut strum_requested = false;
        let mut flam_requested = false;
        let mut randomize_requested = false;
        let mut humanize_requested = false;
        let mut limit_requested = false;
        let mut arpeggiate_requested = false;
        let mut slice_requested = false;
        let mut delete_selection_requested = false;
        let mut duplicate_notes_requested = false;
        let mut quantize_selected_requested = false;
        if ui.memory(|memory| memory.focused().is_none()) {
            let select_all_notes =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A));
            let invert_note_selection =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::I));
            let clear_note_selection =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::D));
            duplicate_notes_requested =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::B));
            let select_draw =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::P));
            let select_paint =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::B));
            let select_select =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::E));
            let select_zoom =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Z));
            let select_playback =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Y));
            let cycle_event_target =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::F));
            if select_all_notes {
                self.apply_piano_roll_selection_command(PianoRollSelectionCommand::All);
            } else if invert_note_selection {
                self.apply_piano_roll_selection_command(PianoRollSelectionCommand::Invert);
            } else if clear_note_selection {
                self.apply_piano_roll_selection_command(PianoRollSelectionCommand::Clear);
            }
            if cycle_event_target {
                let current = PianoRollEventTarget::ALL
                    .iter()
                    .position(|target| *target == self.piano_roll_event_target)
                    .unwrap_or(0);
                self.piano_roll_event_target =
                    PianoRollEventTarget::ALL[(current + 1) % PianoRollEventTarget::ALL.len()];
            }
            if select_draw {
                self.piano_roll_paint_mode = false;
                self.piano_roll_select_mode = false;
                self.piano_roll_zoom_mode = false;
                self.piano_roll_playback_mode = false;
                self.piano_roll_stamp_mode = false;
            } else if select_paint {
                self.piano_roll_paint_mode = true;
                self.piano_roll_select_mode = false;
                self.piano_roll_zoom_mode = false;
                self.piano_roll_playback_mode = false;
                self.piano_roll_stamp_mode = false;
            } else if select_select {
                self.piano_roll_paint_mode = false;
                self.piano_roll_select_mode = true;
                self.piano_roll_zoom_mode = false;
                self.piano_roll_playback_mode = false;
                self.piano_roll_stamp_mode = false;
            } else if select_zoom {
                self.piano_roll_paint_mode = false;
                self.piano_roll_select_mode = false;
                self.piano_roll_zoom_mode = true;
                self.piano_roll_playback_mode = false;
                self.piano_roll_stamp_mode = false;
            } else if select_playback {
                self.piano_roll_paint_mode = false;
                self.piano_roll_select_mode = false;
                self.piano_roll_zoom_mode = false;
                self.piano_roll_playback_mode = true;
                self.piano_roll_stamp_mode = false;
            }
        }
        let selection_pattern_before_toolbar = self.selected_pattern;
        let selection_channel_before_toolbar = self.selected_note_channel;
        let selected_quantize_indices = match (self.selected_pattern, self.selected_note_channel) {
            (Some(pattern_id), Some(channel_id)) => self
                .selected_piano_notes
                .iter()
                .filter(|(selected_pattern, selected_channel, _)| {
                    *selected_pattern == pattern_id && *selected_channel == channel_id
                })
                .map(|(_, _, note_index)| *note_index)
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        ui.horizontal_wrapped(|ui| {
            ui.strong("Piano roll");
            ui.separator();
            egui::ComboBox::from_id_salt("pattern-picker")
                .selected_text(format!("Pattern {}", self.selected_pattern.unwrap_or(0)))
                .show_ui(ui, |ui| {
                    for pattern in &patterns {
                        ui.selectable_value(
                            &mut self.selected_pattern,
                            Some(pattern.id),
                            pattern
                                .name
                                .as_deref()
                                .map_or_else(|| format!("Pattern {}", pattern.id), str::to_owned),
                        );
                    }
                });
            egui::ComboBox::from_id_salt("piano-roll-channel-picker")
                .selected_text(selected_channel_label)
                .show_ui(ui, |ui| {
                    for channel in &channels {
                        let label = channel
                            .display_name()
                            .or(channel.plugin_identifier())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Channel {}", channel.id()));
                        ui.selectable_value(
                            &mut self.selected_note_channel,
                            Some(channel.id()),
                            label,
                        );
                    }
                });
            egui::ComboBox::from_id_salt("piano-roll-snap")
                .selected_text(format!("Snap: {}", self.piano_roll_snap.label()))
                .show_ui(ui, |ui| {
                    for snap in PianoRollSnap::ALL {
                        ui.selectable_value(&mut self.piano_roll_snap, snap, snap.label());
                    }
                });
            egui::ComboBox::from_id_salt("piano-roll-edit-scope")
                .selected_text(format!("Edit: {}", self.piano_roll_edit_scope.label()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.piano_roll_edit_scope,
                        PianoRollEditScope::Automatic,
                        "Auto (selection if any)",
                    );
                    ui.selectable_value(
                        &mut self.piano_roll_edit_scope,
                        PianoRollEditScope::Channel,
                        "Channel",
                    );
                    ui.selectable_value(
                        &mut self.piano_roll_edit_scope,
                        PianoRollEditScope::Selection,
                        "Selected notes",
                    );
                });
            egui::ComboBox::from_id_salt("piano-roll-scale-root")
                .selected_text(format!(
                    "Key: {}",
                    PITCH_CLASSES[self.piano_roll_scale_root as usize]
                ))
                .show_ui(ui, |ui| {
                    for (root, label) in PITCH_CLASSES.iter().enumerate() {
                        ui.selectable_value(&mut self.piano_roll_scale_root, root as u8, *label);
                    }
                });
            egui::ComboBox::from_id_salt("piano-roll-scale")
                .selected_text(format!("Scale: {}", self.piano_roll_scale.label()))
                .show_ui(ui, |ui| {
                    for scale in PianoRollScale::ALL {
                        ui.selectable_value(&mut self.piano_roll_scale, scale, scale.label());
                    }
                });
            ui.checkbox(&mut self.piano_roll_ghost_channels, "Ghost channels");
            ui.checkbox(
                &mut self.piano_roll_color_by_midi_channel,
                "Color by MIDI channel",
            );
            if ui
                .selectable_label(
                    !self.piano_roll_paint_mode
                        && !self.piano_roll_select_mode
                        && !self.piano_roll_zoom_mode
                        && !self.piano_roll_playback_mode
                        && !self.piano_roll_stamp_mode,
                    "Draw (P)",
                )
                .clicked()
            {
                self.piano_roll_paint_mode = false;
                self.piano_roll_select_mode = false;
                self.piano_roll_zoom_mode = false;
                self.piano_roll_playback_mode = false;
                self.piano_roll_stamp_mode = false;
            }
            if ui
                .selectable_label(self.piano_roll_paint_mode, "Paint (B)")
                .clicked()
            {
                self.piano_roll_paint_mode = !self.piano_roll_paint_mode;
                if self.piano_roll_paint_mode {
                    self.piano_roll_select_mode = false;
                    self.piano_roll_zoom_mode = false;
                    self.piano_roll_playback_mode = false;
                    self.piano_roll_stamp_mode = false;
                }
            }
            if ui
                .selectable_label(self.piano_roll_stamp_mode, "Stamp")
                .clicked()
            {
                self.piano_roll_stamp_mode = !self.piano_roll_stamp_mode;
                if self.piano_roll_stamp_mode {
                    self.piano_roll_paint_mode = false;
                    self.piano_roll_select_mode = false;
                    self.piano_roll_zoom_mode = false;
                    self.piano_roll_playback_mode = false;
                }
            }
            egui::ComboBox::from_id_salt("piano-roll-chord-stamp")
                .selected_text(format!("Chord: {}", self.piano_roll_chord_stamp.label()))
                .show_ui(ui, |ui| {
                    for stamp in PianoRollChordStamp::ALL {
                        ui.selectable_value(&mut self.piano_roll_chord_stamp, stamp, stamp.label());
                    }
                });
            if self.piano_roll_stamp_mode {
                ui.checkbox(&mut self.piano_roll_stamp_only_one, "Only one");
            }
            if ui
                .selectable_label(self.piano_roll_select_mode, "Select (E)")
                .clicked()
            {
                self.piano_roll_select_mode = !self.piano_roll_select_mode;
                if self.piano_roll_select_mode {
                    self.piano_roll_paint_mode = false;
                    self.piano_roll_zoom_mode = false;
                    self.piano_roll_playback_mode = false;
                    self.piano_roll_stamp_mode = false;
                }
            }
            ui.menu_button("Selection", |ui| {
                if ui.button("Select all notes (Ctrl/Cmd+A)").clicked() {
                    self.apply_piano_roll_selection_command(PianoRollSelectionCommand::All);
                }
                if ui.button("Invert selection (Shift+I)").clicked() {
                    self.apply_piano_roll_selection_command(PianoRollSelectionCommand::Invert);
                }
                if ui.button("Deselect notes (Ctrl/Cmd+D)").clicked() {
                    self.apply_piano_roll_selection_command(PianoRollSelectionCommand::Clear);
                }
                if ui.button("Duplicate to right (Ctrl/Cmd+B)").clicked() {
                    duplicate_notes_requested = true;
                    ui.close();
                }
            });
            if ui
                .selectable_label(self.piano_roll_zoom_mode, "Zoom (Z)")
                .clicked()
            {
                self.piano_roll_zoom_mode = !self.piano_roll_zoom_mode;
                if self.piano_roll_zoom_mode {
                    self.piano_roll_paint_mode = false;
                    self.piano_roll_select_mode = false;
                    self.piano_roll_playback_mode = false;
                    self.piano_roll_stamp_mode = false;
                }
            }
            if ui
                .selectable_label(self.piano_roll_playback_mode, "Playback (Y)")
                .clicked()
            {
                self.piano_roll_playback_mode = !self.piano_roll_playback_mode;
                if self.piano_roll_playback_mode {
                    self.piano_roll_paint_mode = false;
                    self.piano_roll_select_mode = false;
                    self.piano_roll_zoom_mode = false;
                    self.piano_roll_stamp_mode = false;
                }
            }
            if ui
                .selectable_label(self.piano_roll_event_editor_open, "Events")
                .on_hover_text("Show note properties below the grid (Shift+F cycles target)")
                .clicked()
            {
                self.piano_roll_event_editor_open = !self.piano_roll_event_editor_open;
            }
            if self.piano_roll_event_editor_open {
                egui::ComboBox::from_id_salt("piano-roll-event-target")
                    .selected_text(self.piano_roll_event_target.label())
                    .show_ui(ui, |ui| {
                        for target in PianoRollEventTarget::ALL {
                            ui.selectable_value(
                                &mut self.piano_roll_event_target,
                                target,
                                target.label(),
                            );
                        }
                    });
            }
            let zoom_center = self
                .piano_roll_grid_viewport
                .map_or(500.0, |viewport| viewport.width() * 0.5);
            if ui
                .button("−")
                .on_hover_text("Zoom out (Page Down)")
                .clicked()
            {
                self.request_piano_roll_zoom(self.piano_roll_zoom / 1.2, zoom_center, zoom_center);
            }
            if ui.button("+").on_hover_text("Zoom in (Page Up)").clicked() {
                self.request_piano_roll_zoom(self.piano_roll_zoom * 1.2, zoom_center, zoom_center);
            }
            let mut zoom_slider_value = self.piano_roll_zoom;
            ui.add(
                egui::Slider::new(&mut zoom_slider_value, 0.06..=0.24)
                    .text("Zoom")
                    .show_value(false),
            );
            if (zoom_slider_value - self.piano_roll_zoom).abs() > f32::EPSILON {
                self.request_piano_roll_zoom(zoom_slider_value, zoom_center, zoom_center);
            }
            let edit_selection_only = self.piano_roll_edit_scope == PianoRollEditScope::Selection
                || (self.piano_roll_edit_scope == PianoRollEditScope::Automatic
                    && !selected_quantize_indices.is_empty());
            let edit_target = if edit_selection_only {
                "selected"
            } else {
                "channel"
            };
            let edit_scope_available = self.selected_pattern.is_some()
                && self.selected_note_channel.is_some()
                && (self.piano_roll_edit_scope != PianoRollEditScope::Selection
                    || !selected_quantize_indices.is_empty());
            quantize_requested = ui
                .add_enabled(
                    self.selected_pattern.is_some()
                        && self.selected_note_channel.is_some()
                        && self.piano_roll_snap != PianoRollSnap::None,
                    egui::Button::new("Quantize channel"),
                )
                .clicked();
            quantize_selected_requested = ui
                .add_enabled(
                    self.selected_pattern.is_some()
                        && self.selected_note_channel.is_some()
                        && self.piano_roll_snap != PianoRollSnap::None
                        && !selected_quantize_indices.is_empty(),
                    egui::Button::new(format!(
                        "Quantize selected ({})",
                        selected_quantize_indices.len()
                    )),
                )
                .clicked();
            legato_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Legato {edit_target}")),
                )
                .clicked();
            chop_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Chop {edit_target}")),
                )
                .clicked();
            glue_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Glue {edit_target}")),
                )
                .clicked();
            flip_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Flip {edit_target}")),
                )
                .clicked();
            strum_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Strum {edit_target}")),
                )
                .clicked();
            flam_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Flam {edit_target}")),
                )
                .clicked();
            randomize_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Randomize {edit_target}")),
                )
                .clicked();
            humanize_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Humanize {edit_target}")),
                )
                .clicked();
            limit_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Limit {edit_target}")),
                )
                .clicked();
            arpeggiate_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Arpeggiate {edit_target}")),
                )
                .clicked();
            slice_requested = ui
                .add_enabled(
                    edit_scope_available,
                    egui::Button::new(format!("Slice {edit_target}")),
                )
                .clicked();
            egui::ComboBox::from_id_salt("midi-channel-mapping")
                .selected_text(match self.midi_channel_mapping {
                    MidiChannelMapping::PreserveNoteChannels => "MIDI channels: Stored",
                    MidiChannelMapping::AssignProjectChannels => "MIDI channels: Per channel",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.midi_channel_mapping,
                        MidiChannelMapping::PreserveNoteChannels,
                        "Stored note channels",
                    );
                    ui.selectable_value(
                        &mut self.midi_channel_mapping,
                        MidiChannelMapping::AssignProjectChannels,
                        "One per FL channel",
                    );
                });
            export_midi_requested = ui
                .add_enabled(
                    self.selected_pattern.is_some(),
                    egui::Button::new("Export pattern MIDI…"),
                )
                .clicked();
            add_note_requested = ui
                .add_enabled(
                    self.selected_note_channel.is_some(),
                    egui::Button::new("Add note"),
                )
                .clicked();
            let has_loaded_instrument = self
                .selected_note_channel
                .is_some_and(|channel_id| self.channel_vst3_instances.contains_key(&channel_id));
            render_requested = ui
                .add_enabled(
                    has_loaded_instrument && self.selected_pattern.is_some(),
                    egui::Button::new("Render WAV…"),
                )
                .clicked();
            preview_requested = ui
                .add_enabled(
                    has_loaded_instrument && self.selected_pattern.is_some(),
                    egui::Button::new("Preview VST3"),
                )
                .clicked();
            sampler_preview_requested = ui
                .add_enabled(has_sampler_notes, egui::Button::new("Preview Samplers"))
                .clicked();
            open_midi_requested = ui.button("Open MIDI…").clicked();
            let selected_note_count = self
                .selected_pattern
                .map(|pattern_id| {
                    self.selected_piano_notes
                        .iter()
                        .filter(|(selected_pattern, _, _)| *selected_pattern == pattern_id)
                        .count()
                })
                .unwrap_or(0);
            delete_selection_requested = ui
                .add_enabled(
                    selected_note_count > 0,
                    egui::Button::new(format!("Delete selection ({selected_note_count})")),
                )
                .clicked();
        });

        if selection_pattern_before_toolbar != self.selected_pattern
            || selection_channel_before_toolbar != self.selected_note_channel
        {
            self.selected_piano_notes.clear();
            self.selected_note = None;
            self.piano_roll_selection_drag = None;
        }
        if selection_pattern_before_toolbar != self.selected_pattern
            || selection_channel_before_toolbar != self.selected_note_channel
        {
            quantize_selected_requested = false;
            legato_requested = false;
            chop_requested = false;
            glue_requested = false;
            flip_requested = false;
            strum_requested = false;
            flam_requested = false;
            randomize_requested = false;
            humanize_requested = false;
            limit_requested = false;
            arpeggiate_requested = false;
            slice_requested = false;
        }

        let edit_selection_only = self.piano_roll_edit_scope == PianoRollEditScope::Selection
            || (self.piano_roll_edit_scope == PianoRollEditScope::Automatic
                && !selected_quantize_indices.is_empty());
        let edit_scope_description = if edit_selection_only {
            "selected notes"
        } else {
            "channel"
        };

        if duplicate_notes_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
            && let Some(pattern) = patterns.iter().find(|pattern| pattern.id == pattern_id)
        {
            let channel_notes = pattern
                .notes
                .iter()
                .filter(|note| note.channel_id == channel_id)
                .cloned()
                .collect::<Vec<_>>();
            let selected_indices = self
                .selected_piano_notes
                .iter()
                .filter(|(selected_pattern, selected_channel, _)| {
                    *selected_pattern == pattern_id && *selected_channel == channel_id
                })
                .map(|(_, _, note_index)| *note_index)
                .collect::<BTreeSet<_>>();
            let source_notes = if selected_indices.is_empty() {
                channel_notes.clone()
            } else {
                channel_notes
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| selected_indices.contains(index))
                    .map(|(_, note)| note.clone())
                    .collect()
            };
            if source_notes.is_empty() {
                self.status = format!("No notes to duplicate in pattern {pattern_id}");
            } else {
                let first_position = source_notes
                    .iter()
                    .map(|note| note.position)
                    .min()
                    .unwrap_or(0);
                let last_end = source_notes
                    .iter()
                    .map(|note| note.position.saturating_add(note.length))
                    .max()
                    .unwrap_or(first_position);
                let offset = last_end.saturating_sub(first_position).max(1);
                let duplicates = source_notes
                    .iter()
                    .cloned()
                    .map(|mut note| {
                        note.position = note.position.saturating_add(offset);
                        note
                    })
                    .collect::<Vec<_>>();
                let first_note_index = channel_notes.len();
                let duplicated_count = duplicates.len();
                if let Some(document) = &mut self.document {
                    match document.add_pattern_notes(pattern_id, &duplicates) {
                        Ok(()) => {
                            self.selected_piano_notes.clear();
                            self.selected_piano_notes.extend(
                                (first_note_index..first_note_index + duplicated_count)
                                    .map(|note_index| (pattern_id, channel_id, note_index)),
                            );
                            self.selected_note =
                                self.selected_piano_notes.iter().next_back().copied();
                            self.dirty = true;
                            self.status = format!(
                                "Duplicated {duplicated_count} notes to the right in pattern {pattern_id}"
                            );
                        }
                        Err(error) => {
                            self.status = format!("Could not duplicate notes: {error}");
                        }
                    }
                }
            }
        }

        if delete_selection_requested
            && let Some(pattern_id) = self.selected_pattern
            && let Some(document) = self.document.as_ref()
        {
            let mut updated = document.clone();
            let mut selected_by_channel = BTreeMap::<u16, Vec<usize>>::new();
            for (selected_pattern, channel_id, note_index) in &self.selected_piano_notes {
                if *selected_pattern == pattern_id {
                    selected_by_channel
                        .entry(*channel_id)
                        .or_default()
                        .push(*note_index);
                }
            }
            let result =
                selected_by_channel
                    .into_iter()
                    .try_for_each(|(channel_id, mut note_indices)| {
                        note_indices.sort_unstable_by(|left, right| right.cmp(left));
                        note_indices.into_iter().try_for_each(|note_index| {
                            updated.delete_pattern_note(pattern_id, channel_id, note_index)
                        })
                    });
            match result {
                Ok(()) => {
                    self.document = Some(updated);
                    self.selected_piano_notes.clear();
                    self.selected_note = None;
                    self.active_note_drag = None;
                    self.stop_project_playback();
                    self.dirty = true;
                    self.status = format!("Deleted selected notes from pattern {pattern_id}");
                }
                Err(error) => {
                    self.status = format!("Could not delete selected notes: {error}");
                }
            }
        }

        ui.collapsing("Edit tool settings", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::Slider::new(&mut self.quantize_strength_percent, 0..=100)
                        .text("Strength (%)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.quantize_swing_percent, 0..=100).text("Swing (%)"),
                );
                ui.add(egui::Slider::new(&mut self.chop_divisions, 2..=16).text("Chop divisions"));
                ui.add(
                    egui::Slider::new(&mut self.strum_spread_ticks, 0..=1536)
                        .text("Strum spread (ticks)"),
                );
                egui::ComboBox::from_id_salt("piano-roll-strum-direction")
                    .selected_text(if self.strum_descending {
                        "Strum: Down"
                    } else {
                        "Strum: Up"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.strum_descending, false, "Low to high");
                        ui.selectable_value(&mut self.strum_descending, true, "High to low");
                    });
                ui.add(
                    egui::Slider::new(&mut self.flam_stroke_ticks, 1..=384)
                        .text("Flam time (ticks)"),
                );
                ui.add(egui::Slider::new(&mut self.flam_velocity, 0..=127).text("Flam velocity"));
                ui.checkbox(&mut self.flam_before, "Flam before notes");
                ui.add(
                    egui::Slider::new(&mut self.randomizer_velocity_amount, -100..=100)
                        .text("Velocity randomize (%)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.randomizer_pan_amount, -100..=100)
                        .text("Pan randomize (%)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.randomizer_pitch_range, 0..=24)
                        .text("Pitch range (semitones)"),
                );
                ui.add(egui::DragValue::new(&mut self.randomizer_seed).prefix("Seed "));
                ui.checkbox(&mut self.randomizer_bipolar, "Bipolar");
                ui.checkbox(&mut self.randomizer_reset_levels, "Reset levels first");
                ui.add(
                    egui::Slider::new(&mut self.humanize_timing_range_ticks, 0..=96)
                        .text("Humanize timing (± ticks)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.humanize_velocity_variation_percent, 0..=100)
                        .text("Humanize velocity (%)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.note_limit_minimum_key, 0..=127)
                        .text("Limit lowest key"),
                );
                ui.add(
                    egui::Slider::new(&mut self.note_limit_maximum_key, 0..=127)
                        .text("Limit highest key"),
                );
                ui.add(
                    egui::Slider::new(&mut self.arpeggiator_step_ticks, 1..=384)
                        .text("Arpeggiator step (ticks)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.arpeggiator_range_octaves, 1..=4)
                        .text("Arpeggiator octaves"),
                );
                ui.add(
                    egui::Slider::new(&mut self.arpeggiator_gate_percent, 1..=100)
                        .text("Arpeggiator gate (%)"),
                );
                egui::ComboBox::from_id_salt("piano-roll-arpeggiator-direction")
                    .selected_text(match self.arpeggiator_direction {
                        ArpeggioDirection::Up => "Arpeggiator: Up",
                        ArpeggioDirection::Down => "Arpeggiator: Down",
                        ArpeggioDirection::UpDown => "Arpeggiator: Up/Down",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.arpeggiator_direction,
                            ArpeggioDirection::Up,
                            "Up",
                        );
                        ui.selectable_value(
                            &mut self.arpeggiator_direction,
                            ArpeggioDirection::Down,
                            "Down",
                        );
                        ui.selectable_value(
                            &mut self.arpeggiator_direction,
                            ArpeggioDirection::UpDown,
                            "Up / Down",
                        );
                    });
                ui.add(
                    egui::DragValue::new(&mut self.piano_roll_slice_position_ticks)
                        .prefix("Slice at tick ")
                        .speed(1.0),
                );
            });
            ui.small("Quantize uses the selected snap grid and moves note starts only.");
            ui.small(
                "Edit: Auto uses the current selection when present; otherwise it affects the channel.",
            );
        });

        if render_requested {
            self.render_selected_pattern_channel();
        }
        if preview_requested {
            self.play_selected_pattern_channel();
        }
        if sampler_preview_requested {
            self.play_selected_sampler_pattern();
        }
        if export_midi_requested {
            self.export_selected_pattern_midi_dialog();
        }
        if (quantize_requested || quantize_selected_requested)
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let grid_ticks = self.piano_roll_snap.ticks(ppq, time_signature);
            let strength = f64::from(self.quantize_strength_percent) / 100.0;
            let swing = f64::from(self.quantize_swing_percent) / 100.0;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if quantize_selected_requested {
                        document.quantize_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            grid_ticks,
                            strength,
                            swing,
                        )
                    } else {
                        document.quantize_pattern_notes(
                            pattern_id, channel_id, grid_ticks, strength, swing,
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = if quantize_selected_requested {
                        format!("Quantized {changed} selected note starts in pattern {pattern_id}")
                    } else {
                        format!(
                            "Quantized {changed} note starts in pattern {pattern_id}, channel {channel_id}"
                        )
                    };
                }
                Err(error) => self.status = format!("Could not quantize notes: {error}"),
            }
        }
        if legato_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.legato_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                        )
                    } else {
                        document.legato_pattern_notes(pattern_id, channel_id)
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Extended {changed} note lengths in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not apply Legato: {error}"),
            }
        }
        if chop_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let divisions = self.chop_divisions;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.chop_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            divisions,
                        )
                    } else {
                        document.chop_pattern_notes(pattern_id, channel_id, divisions)
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(created) => {
                    if created > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Created {created} chopped notes in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not chop notes: {error}"),
            }
        }
        if glue_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.glue_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                        )
                    } else {
                        document.glue_pattern_notes(pattern_id, channel_id)
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(removed) => {
                    if removed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                        self.selected_piano_notes.clear();
                        self.selected_note = None;
                        self.active_note_drag = None;
                    }
                    self.status = format!(
                        "Joined notes and removed {removed} records in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not glue notes: {error}"),
            }
        }
        if flip_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.flip_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                        )
                    } else {
                        document.flip_pattern_notes(pattern_id, channel_id)
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Flipped {changed} note positions in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not flip notes: {error}"),
            }
        }
        if strum_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let spread_ticks = self.strum_spread_ticks;
            let descending = self.strum_descending;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.strum_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            spread_ticks,
                            descending,
                        )
                    } else {
                        document.strum_pattern_notes(
                            pattern_id,
                            channel_id,
                            spread_ticks,
                            descending,
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Strummed {changed} notes in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not strum notes: {error}"),
            }
        }
        if flam_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let stroke_ticks = self.flam_stroke_ticks;
            let velocity = self.flam_velocity;
            let before = self.flam_before;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.flam_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            stroke_ticks,
                            velocity,
                            before,
                        )
                    } else {
                        document.flam_pattern_notes(
                            pattern_id,
                            channel_id,
                            stroke_ticks,
                            velocity,
                            before,
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(created) => {
                    if created > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Added {created} flam strokes to pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not add flam strokes: {error}"),
            }
        }
        if randomize_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let seed = self.randomizer_seed;
            let velocity_amount = self.randomizer_velocity_amount;
            let pan_amount = self.randomizer_pan_amount;
            let pitch_range = self.randomizer_pitch_range;
            let bipolar = self.randomizer_bipolar;
            let reset_levels = self.randomizer_reset_levels;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.randomize_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            RandomizerOptions {
                                seed,
                                velocity_amount_percent: velocity_amount,
                                pan_amount_percent: pan_amount,
                                pitch_range_semitones: pitch_range,
                                bipolar,
                                reset_levels,
                            },
                        )
                    } else {
                        document.randomize_pattern_notes(
                            pattern_id,
                            channel_id,
                            RandomizerOptions {
                                seed,
                                velocity_amount_percent: velocity_amount,
                                pan_amount_percent: pan_amount,
                                pitch_range_semitones: pitch_range,
                                bipolar,
                                reset_levels,
                            },
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    self.randomizer_seed = seed.saturating_add(1);
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Randomized {changed} notes in pattern {pattern_id}, {edit_scope_description} using seed {seed}"
                    );
                }
                Err(error) => self.status = format!("Could not randomize notes: {error}"),
            }
        }
        if humanize_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let seed = self.randomizer_seed;
            let timing_range_ticks = self.humanize_timing_range_ticks;
            let velocity_variation_percent = self.humanize_velocity_variation_percent;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.humanize_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            seed,
                            timing_range_ticks,
                            velocity_variation_percent,
                        )
                    } else {
                        document.humanize_pattern_notes(
                            pattern_id,
                            channel_id,
                            seed,
                            timing_range_ticks,
                            velocity_variation_percent,
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    self.randomizer_seed = seed.saturating_add(1);
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Humanized {changed} notes in pattern {pattern_id}, {edit_scope_description} using seed {seed}"
                    );
                }
                Err(error) => self.status = format!("Could not humanize notes: {error}"),
            }
        }
        if limit_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let minimum_key = u16::from(self.note_limit_minimum_key);
            let maximum_key = u16::from(self.note_limit_maximum_key);
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.limit_pattern_note_selection_range(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            minimum_key,
                            maximum_key,
                        )
                    } else {
                        document.limit_pattern_note_range(
                            pattern_id,
                            channel_id,
                            minimum_key,
                            maximum_key,
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(changed) => {
                    if changed > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Limited {changed} note pitches in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not limit note pitches: {error}"),
            }
        }
        if arpeggiate_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let step_ticks = self.arpeggiator_step_ticks;
            let range_octaves = self.arpeggiator_range_octaves;
            let gate_percent = self.arpeggiator_gate_percent;
            let direction = self.arpeggiator_direction;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.arpeggiate_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            ArpeggioOptions {
                                step_ticks,
                                range_octaves,
                                gate_percent,
                                direction,
                            },
                        )
                    } else {
                        document.arpeggiate_pattern_notes(
                            pattern_id,
                            channel_id,
                            ArpeggioOptions {
                                step_ticks,
                                range_octaves,
                                gate_percent,
                                direction,
                            },
                        )
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(created) => {
                    if created > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                        self.selected_piano_notes.clear();
                        self.selected_note = None;
                        self.active_note_drag = None;
                    }
                    self.status = format!(
                        "Generated {created} arpeggiated notes in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not arpeggiate notes: {error}"),
            }
        }
        if slice_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let position_ticks = self.piano_roll_slice_position_ticks;
            let result = self
                .document
                .as_mut()
                .ok_or_else(|| "no project is open".to_owned())
                .and_then(|document| {
                    let result = if edit_selection_only {
                        document.slice_pattern_note_selection(
                            pattern_id,
                            channel_id,
                            &selected_quantize_indices,
                            position_ticks,
                        )
                    } else {
                        document.slice_pattern_notes(pattern_id, channel_id, position_ticks)
                    };
                    result.map_err(|error| error.to_string())
                });
            match result {
                Ok(created) => {
                    if created > 0 {
                        self.stop_project_playback();
                        self.dirty = true;
                    }
                    self.status = format!(
                        "Sliced {created} notes at tick {position_ticks} in pattern {pattern_id}, {edit_scope_description}"
                    );
                }
                Err(error) => self.status = format!("Could not slice notes: {error}"),
            }
        }

        if open_midi_requested
            && let Some(path) = rfd::FileDialog::new()
                .set_title("Open MIDI file")
                .add_filter("MIDI files", &["mid", "midi"])
                .pick_file()
        {
            match fs::read(&path) {
                Ok(bytes) => match MidiFile::parse(&bytes) {
                    Ok(midi) if !midi.tracks().is_empty() => {
                        self.pending_midi_import = Some(PendingMidiImport {
                            path: path.clone(),
                            midi,
                            selected_track: 0,
                        });
                        self.status = format!("Loaded MIDI file {}", path.display());
                    }
                    Ok(_) => self.status = "The MIDI file contains no tracks".to_owned(),
                    Err(error) => self.status = format!("Could not parse MIDI file: {error}"),
                },
                Err(error) => self.status = format!("Could not read {}: {error}", path.display()),
            }
        }

        let mut import_midi_request = None;
        let mut clear_midi_import = false;
        if let Some(pending) = &mut self.pending_midi_import {
            ui.horizontal(|ui| {
                ui.small(pending.path.file_name().map_or_else(
                    || "MIDI".to_owned(),
                    |name| name.to_string_lossy().into_owned(),
                ));
                let selected_track_label = pending
                    .midi
                    .tracks()
                    .get(pending.selected_track)
                    .map(|track| {
                        track
                            .name()
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| format!("Track {}", pending.selected_track + 1))
                    })
                    .unwrap_or_else(|| "No track".to_owned());
                egui::ComboBox::from_id_salt("midi-import-track-picker")
                    .selected_text(selected_track_label)
                    .show_ui(ui, |ui| {
                        for (index, track) in pending.midi.tracks().iter().enumerate() {
                            let label = track
                                .name()
                                .filter(|name| !name.is_empty())
                                .unwrap_or_else(|| format!("Track {}", index + 1));
                            ui.selectable_value(&mut pending.selected_track, index, label);
                        }
                    });
                if ui
                    .add_enabled(
                        self.selected_pattern.is_some() && self.selected_note_channel.is_some(),
                        egui::Button::new("Import track"),
                    )
                    .clicked()
                    && let (Some(pattern_id), Some(channel_id)) =
                        (self.selected_pattern, self.selected_note_channel)
                {
                    import_midi_request = Some((
                        pending.midi.clone(),
                        pending.selected_track,
                        pattern_id,
                        channel_id,
                    ));
                }
                clear_midi_import = ui.button("Clear").clicked();
            });
        }
        if clear_midi_import {
            self.pending_midi_import = None;
        }
        if let Some((midi, track_index, pattern_id, channel_id)) = import_midi_request {
            let first_note_index = patterns
                .iter()
                .find(|pattern| pattern.id == pattern_id)
                .map(|pattern| {
                    pattern
                        .notes
                        .iter()
                        .filter(|note| note.channel_id == channel_id)
                        .count()
                })
                .unwrap_or(0);
            if let Some(document) = &mut self.document {
                match document.import_midi_track(&midi, track_index, pattern_id, channel_id) {
                    Ok(imported) => {
                        if imported > 0 {
                            self.selected_note = Some((pattern_id, channel_id, first_note_index));
                            self.selected_piano_notes.clear();
                            self.selected_piano_notes.insert((
                                pattern_id,
                                channel_id,
                                first_note_index,
                            ));
                            self.dirty = true;
                        }
                        self.status =
                            format!("Imported {imported} MIDI notes into pattern {pattern_id}");
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
        }

        if add_note_requested
            && let (Some(pattern_id), Some(channel_id)) =
                (self.selected_pattern, self.selected_note_channel)
        {
            let note_index = patterns
                .iter()
                .find(|pattern| pattern.id == pattern_id)
                .map(|pattern| {
                    pattern
                        .notes
                        .iter()
                        .filter(|note| note.channel_id == channel_id)
                        .count()
                })
                .unwrap_or(0);
            let position = patterns
                .iter()
                .find(|pattern| pattern.id == pattern_id)
                .into_iter()
                .flat_map(|pattern| pattern.notes.iter())
                .filter(|note| note.channel_id == channel_id)
                .map(|note| note.position.saturating_add(note.length))
                .max()
                .unwrap_or(0);
            let note = PatternNote {
                position,
                channel_id,
                length: u32::from(ppq),
                key: 60,
                velocity: 100,
                ..PatternNote::default()
            };
            if let Some(document) = &mut self.document {
                match document.add_pattern_note(pattern_id, note) {
                    Ok(()) => {
                        self.selected_note = Some((pattern_id, channel_id, note_index));
                        self.selected_piano_notes.clear();
                        self.selected_piano_notes
                            .insert((pattern_id, channel_id, note_index));
                        self.dirty = true;
                        self.status = format!("Added note to pattern {pattern_id}");
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
        }
        ui.separator();
        let updated_patterns = self
            .document
            .as_ref()
            .and_then(|document| document.patterns().ok())
            .unwrap_or(patterns);
        let Some(pattern) = updated_patterns
            .iter()
            .find(|pattern| Some(pattern.id) == self.selected_pattern)
            .cloned()
        else {
            return;
        };
        let snap_ticks = self.piano_roll_snap.ticks(ppq, time_signature);
        self.draw_notes(ui, &pattern, ppq, snap_ticks);
        let editor_pattern = self
            .document
            .as_ref()
            .and_then(|document| document.patterns().ok())
            .and_then(|patterns| patterns.into_iter().find(|item| item.id == pattern.id))
            .unwrap_or(pattern);
        self.selected_note_editor(ui, &editor_pattern);
    }

    fn render_selected_pattern_channel(&mut self) {
        let (Some(pattern_id), Some(channel_id)) =
            (self.selected_pattern, self.selected_note_channel)
        else {
            return;
        };
        let Some(instance_id) = self.channel_vst3_instances.get(&channel_id).copied() else {
            self.status = "Load a VST3 instrument for this channel before rendering".to_owned();
            return;
        };
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before rendering".to_owned();
            return;
        };
        let notes = match document.patterns() {
            Ok(patterns) => match patterns
                .into_iter()
                .find(|pattern| pattern.id == pattern_id)
            {
                Some(pattern) => pattern.notes,
                None => {
                    self.status = format!("Pattern {pattern_id} was not found");
                    return;
                }
            },
            Err(error) => {
                self.status = format!("Could not decode pattern: {error}");
                return;
            }
        };
        let ppq = document.header().ppq();
        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(self.tempo_bpm);
        let Some(path) = rfd::FileDialog::new()
            .set_title("Render selected pattern channel")
            .set_file_name(format!("Pattern_{pattern_id}_Channel_{channel_id}.wav"))
            .add_filter("WAV audio", &["wav"])
            .save_file()
        else {
            return;
        };

        let result = self
            .vst3_host
            .as_ref()
            .ok_or_else(|| "VST3 host is not initialized".to_owned())
            .and_then(|host| {
                host.render_pattern_channel_to_wav(
                    instance_id,
                    &notes,
                    Vst3PatternRenderOptions {
                        channel_id,
                        ppq,
                        tempo_bpm,
                        tail_seconds: 2.0,
                    },
                    &path,
                )
            });
        match result {
            Ok(summary) => {
                self.status = format!(
                    "Rendered pattern {pattern_id}, channel {channel_id} to {} ({:.2}s)",
                    path.display(),
                    summary.frames as f64 / f64::from(summary.sample_rate),
                );
            }
            Err(error) => self.status = format!("Could not render pattern channel: {error}"),
        }
    }

    fn play_piano_roll_note(&mut self, pattern_id: u16, channel_id: u16, note_index: usize) {
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before auditioning a Piano roll note".to_owned();
            return;
        };
        let patterns = match document.patterns() {
            Ok(patterns) => patterns,
            Err(error) => {
                self.status = format!("Could not decode Piano roll notes: {error}");
                return;
            }
        };
        let Some(pattern) = patterns.iter().find(|pattern| pattern.id == pattern_id) else {
            self.status = format!("Pattern {pattern_id} was not found");
            return;
        };
        let Some(mut note) = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .nth(note_index)
            .cloned()
        else {
            self.status = format!("Note {note_index} was not found in channel {channel_id}");
            return;
        };
        note.position = 0;
        note.length = note.length.max(1);
        let ppq = document.header().ppq();
        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(self.tempo_bpm);
        let is_sampler = document
            .channels()
            .into_iter()
            .find(|channel| channel.id() == channel_id)
            .is_some_and(|channel| channel.kind() == Some(0));
        let instance_id = self.channel_vst3_instances.get(&channel_id).copied();
        let sampler_project = if instance_id.is_none() && is_sampler {
            let Some(project_path) = self.sample_project_path() else {
                self.status = "Save the project before auditioning its Sampler notes".to_owned();
                return;
            };
            let mut preview_document = document.clone();
            if let Err(error) = preview_document.edit_pattern_note(
                pattern_id,
                channel_id,
                note_index,
                PatternNoteEdit {
                    position: Some(0),
                    length: Some(note.length),
                    ..PatternNoteEdit::default()
                },
            ) {
                self.status = format!("Could not prepare the Sampler note: {error}");
                return;
            }
            let mut channel_indices = BTreeMap::<u16, usize>::new();
            let mut removals = BTreeMap::<u16, Vec<usize>>::new();
            for pattern_note in &pattern.notes {
                let channel_note_index =
                    channel_indices.entry(pattern_note.channel_id).or_default();
                if pattern_note.channel_id != channel_id || *channel_note_index != note_index {
                    removals
                        .entry(pattern_note.channel_id)
                        .or_default()
                        .push(*channel_note_index);
                }
                *channel_note_index += 1;
            }
            for (remove_channel, mut indices) in removals {
                indices.sort_unstable_by(|left, right| right.cmp(left));
                for remove_index in indices {
                    if let Err(error) = preview_document.delete_pattern_note(
                        pattern_id,
                        remove_channel,
                        remove_index,
                    ) {
                        self.status = format!("Could not isolate the Sampler note: {error}");
                        return;
                    }
                }
            }
            let project_bytes = match preview_document.encode_lossless() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.status = format!("Could not encode the Sampler note preview: {error}");
                    return;
                }
            };
            Some((project_path, project_bytes))
        } else {
            None
        };
        if instance_id.is_none() && !is_sampler {
            self.status = "Load a VST3 instrument to audition this channel's notes".to_owned();
            return;
        }
        if self.audio_engine.is_none() {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => self.audio_engine = Some(engine),
                Err(error) => {
                    self.status = format!("Could not start audio output: {error}");
                    return;
                }
            }
        }
        let Some(engine) = self.audio_engine.as_ref() else {
            self.status = "Audio output is not available".to_owned();
            return;
        };
        if !engine.output_active() {
            self.status = "Enable an output device in Audio settings before auditioning".to_owned();
            return;
        }
        let device_rate = engine.sample_rate();
        self.stop_project_playback();
        if let Some(engine) = &self.audio_engine {
            engine.set_test_tone(false);
            if self.audio_monitor_input {
                let _ = engine.set_input_monitor(false);
            }
        }
        self.audio_test_tone = false;
        self.audio_monitor_input = false;

        if let Some(instance_id) = instance_id {
            let prepared = self
                .vst3_host
                .as_ref()
                .ok_or_else(|| "VST3 host is not initialized".to_owned())
                .and_then(|host| {
                    host.prepare_pattern_channel_stream(
                        instance_id,
                        std::slice::from_ref(&note),
                        Vst3PatternRenderOptions {
                            channel_id,
                            ppq,
                            tempo_bpm,
                            tail_seconds: 1.0,
                        },
                    )
                });
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.status = format!("Could not prepare note preview: {error}");
                    return;
                }
            };
            let writer = match self
                .audio_engine
                .as_ref()
                .ok_or_else(|| "Audio output is not available".to_owned())
                .and_then(AudioEngine::begin_streaming_playback)
            {
                Ok(writer) => writer,
                Err(error) => {
                    self.status = format!("Could not start note preview: {error}");
                    return;
                }
            };
            match prepared.start(writer, device_rate) {
                Ok(stream) => {
                    self.pending_vst3_stream = Some(stream);
                    self.playing = true;
                    self.project_playback_loaded = true;
                    self.status = format!("Auditioning note {} on channel {channel_id}", note.key);
                }
                Err(error) => {
                    self.stop_project_playback();
                    self.status = format!("Could not start note preview: {error}");
                }
            }
            return;
        }

        let Some((project_path, project_bytes)) = sampler_project else {
            self.status = "Could not prepare the Sampler note preview".to_owned();
            return;
        };
        let writer = match self
            .audio_engine
            .as_ref()
            .ok_or_else(|| "Audio output is not available".to_owned())
            .and_then(AudioEngine::begin_streaming_playback)
        {
            Ok(writer) => writer,
            Err(error) => {
                self.status = format!("Could not start Sampler note preview: {error}");
                return;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("sampler-note-preview".to_owned())
            .spawn(move || {
                let result = FlpDocument::parse(&project_bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|document| {
                        stream_sampler_pattern_to_device(
                            &document,
                            &project_path,
                            SamplerPatternRenderOptions {
                                pattern_id,
                                sample_rate: device_rate,
                                ..SamplerPatternRenderOptions::default()
                            },
                            &writer,
                            &worker_cancelled,
                        )
                    });
                writer.finish();
                let _ = sender.send(result);
            });
        match worker {
            Ok(worker) => {
                self.pending_sampler_stream = Some(PendingSamplerStream {
                    receiver,
                    cancelled,
                    worker,
                });
                self.playing = true;
                self.project_playback_loaded = true;
                self.status = format!("Auditioning note {} on channel {channel_id}", note.key);
            }
            Err(error) => {
                self.stop_project_playback();
                self.status = format!("Could not start Sampler note preview: {error}");
            }
        }
    }

    fn play_selected_pattern_channel(&mut self) {
        let (Some(pattern_id), Some(channel_id)) =
            (self.selected_pattern, self.selected_note_channel)
        else {
            self.status = "Select a pattern and channel before previewing".to_owned();
            return;
        };
        let Some(instance_id) = self.channel_vst3_instances.get(&channel_id).copied() else {
            self.status = "Load a VST3 instrument for this channel before previewing".to_owned();
            return;
        };
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before previewing a pattern".to_owned();
            return;
        };
        let notes = match document.patterns() {
            Ok(patterns) => match patterns
                .into_iter()
                .find(|pattern| pattern.id == pattern_id)
            {
                Some(pattern) => pattern.notes,
                None => {
                    self.status = format!("Pattern {pattern_id} was not found");
                    return;
                }
            },
            Err(error) => {
                self.status = format!("Could not decode pattern: {error}");
                return;
            }
        };
        let ppq = document.header().ppq();
        let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(self.tempo_bpm);

        if self.audio_engine.is_none() {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => self.audio_engine = Some(engine),
                Err(error) => {
                    self.status = format!("Could not start audio output: {error}");
                    return;
                }
            }
        }
        let Some(engine) = self.audio_engine.as_ref() else {
            self.status = "Audio output is not available".to_owned();
            return;
        };
        if !engine.output_active() {
            self.status = "Enable an output device in Audio settings before previewing".to_owned();
            return;
        }
        let device_rate = engine.sample_rate();

        self.stop_project_playback();
        if let Some(engine) = &self.audio_engine {
            engine.set_test_tone(false);
            if self.audio_monitor_input {
                let _ = engine.set_input_monitor(false);
            }
        }
        self.audio_test_tone = false;
        self.audio_monitor_input = false;

        let prepared = self
            .vst3_host
            .as_ref()
            .ok_or_else(|| "VST3 host is not initialized".to_owned())
            .and_then(|host| {
                host.prepare_pattern_channel_stream(
                    instance_id,
                    &notes,
                    Vst3PatternRenderOptions {
                        channel_id,
                        ppq,
                        tempo_bpm,
                        tail_seconds: 2.0,
                    },
                )
            });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status = format!("Could not prepare pattern playback: {error}");
                return;
            }
        };
        let writer = self
            .audio_engine
            .as_ref()
            .ok_or_else(|| "Audio output is not available".to_owned())
            .and_then(AudioEngine::begin_streaming_playback);
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                self.status = format!("Could not start VST3 output: {error}");
                return;
            }
        };
        let stream = match prepared.start(writer, device_rate) {
            Ok(stream) => stream,
            Err(error) => {
                self.stop_project_playback();
                self.status = format!("Could not start VST3 render worker: {error}");
                return;
            }
        };
        let plugin_name = self
            .vst3_host
            .as_ref()
            .and_then(|host| {
                host.loaded_plugins()
                    .into_iter()
                    .find(|plugin| plugin.id == instance_id)
            })
            .map(|plugin| plugin.name)
            .unwrap_or_else(|| "VST3 instrument".to_owned());
        let notes_to_stream = notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .count();
        self.pending_vst3_stream = Some(stream);
        self.playing = true;
        self.project_playback_loaded = true;
        self.status = format!(
            "Streaming pattern {pattern_id} through {plugin_name} ({notes_to_stream} notes at {device_rate} Hz); Mixer effects are not included"
        );
    }

    fn play_selected_sampler_pattern(&mut self) {
        let Some(pattern_id) = self.selected_pattern else {
            self.status = "Select a pattern before previewing Sampler channels".to_owned();
            return;
        };
        let Some(project_path) = self.sample_project_path() else {
            self.status = "Save the project before previewing Sampler channels".to_owned();
            return;
        };
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before previewing Sampler channels".to_owned();
            return;
        };
        let project_bytes = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not prepare Sampler preview: {error}");
                return;
            }
        };
        if self.audio_engine.is_none() {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => self.audio_engine = Some(engine),
                Err(error) => {
                    self.status = format!("Could not start audio output: {error}");
                    return;
                }
            }
        }
        let Some(engine) = self.audio_engine.as_ref() else {
            self.status = "Audio output is not available".to_owned();
            return;
        };
        if !engine.output_active() {
            self.status = "Enable an output device in Audio settings before previewing".to_owned();
            return;
        }
        let device_rate = engine.sample_rate();

        self.stop_project_playback();
        if let Some(engine) = &self.audio_engine {
            engine.set_test_tone(false);
            if self.audio_monitor_input {
                let _ = engine.set_input_monitor(false);
            }
        }
        self.audio_test_tone = false;
        self.audio_monitor_input = false;

        let writer = match self
            .audio_engine
            .as_ref()
            .ok_or_else(|| "Audio output is not available".to_owned())
            .and_then(AudioEngine::begin_streaming_playback)
        {
            Ok(writer) => writer,
            Err(error) => {
                self.status = format!("Could not start Sampler output: {error}");
                return;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("sampler-pattern-stream".to_owned())
            .spawn(move || {
                let result = FlpDocument::parse(&project_bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|document| {
                        stream_sampler_pattern_to_device(
                            &document,
                            &project_path,
                            SamplerPatternRenderOptions {
                                pattern_id,
                                sample_rate: device_rate,
                                ..SamplerPatternRenderOptions::default()
                            },
                            &writer,
                            &worker_cancelled,
                        )
                    });
                writer.finish();
                let _ = sender.send(result);
            });
        match worker {
            Ok(worker) => {
                self.pending_sampler_stream = Some(PendingSamplerStream {
                    receiver,
                    cancelled,
                    worker,
                });
                self.playing = true;
                self.project_playback_loaded = true;
                self.status = format!(
                    "Preparing Sampler voices for pattern {pattern_id} at {device_rate} Hz…"
                );
            }
            Err(error) => {
                self.stop_project_playback();
                self.status = format!("Could not start Sampler render worker: {error}");
            }
        }
    }

    fn render_audio_clips_dialog(&mut self) {
        let (Some(project_path), Some(document)) =
            (self.current_path.as_deref(), self.document.as_ref())
        else {
            self.status = "Open a project before rendering audio clips".to_owned();
            return;
        };
        let project_stem = project_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "FL_Studio_Project".to_owned());
        let Some(output_path) = rfd::FileDialog::new()
            .set_title("Render Playlist audio clips")
            .set_file_name(format!("{project_stem}_audio.wav"))
            .add_filter("WAV audio", &["wav"])
            .save_file()
        else {
            return;
        };
        let options = AudioClipRenderOptions {
            arrangement_id: self.selected_arrangement.unwrap_or_default(),
            ..AudioClipRenderOptions::default()
        };
        let sample_project_path = self
            .sample_project_path()
            .unwrap_or_else(|| project_path.to_path_buf());
        match render_audio_clips_to_wav(document, &sample_project_path, options, &output_path) {
            Ok(summary) => {
                self.status = format!(
                    "Rendered {} audio clips to {} ({} skipped for non-default scale)",
                    summary.clips_rendered,
                    output_path.display(),
                    summary.clips_skipped_unsupported_scale
                );
            }
            Err(error) => self.status = format!("Could not render Playlist audio clips: {error}"),
        }
    }

    fn render_playlist_dialog(&mut self) {
        let (Some(project_path), Some(document)) =
            (self.current_path.as_deref(), self.document.as_ref())
        else {
            self.status = "Open a project before rendering the Playlist".to_owned();
            return;
        };
        if self.pending_song_render.is_some() {
            self.status = "A Playlist render is already running".to_owned();
            return;
        }
        if self.playing
            || self.project_playback_loaded
            || self.pending_audio_render.is_some()
            || self.pending_vst3_stream.is_some()
            || self.pending_sampler_stream.is_some()
        {
            self.status =
                "Stop Song playback or pattern preview before rendering the Playlist".to_owned();
            return;
        }
        let project_stem = project_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "FL_Studio_Project".to_owned());
        let Some(output_path) = rfd::FileDialog::new()
            .set_title("Render Playlist mix")
            .set_file_name(format!("{project_stem}.wav"))
            .add_filter("32-bit float WAV audio", &["wav"])
            .save_file()
        else {
            return;
        };

        let options = PlaylistRenderOptions {
            arrangement_id: self.selected_arrangement.unwrap_or_default(),
            sample_rate: self.audio_settings.sample_rate,
            ..PlaylistRenderOptions::default()
        };
        let vst3_processor = self
            .vst3_host
            .as_ref()
            .map(|host| {
                host.prepare_playlist_stream(
                    document,
                    options.arrangement_id,
                    &self.channel_vst3_instances,
                    options.sample_rate,
                    2.0,
                )
            })
            .transpose();
        let vst3_processor = match vst3_processor {
            Ok(processor) => processor,
            Err(error) => {
                self.status = format!("Could not prepare Playlist VST3 instruments: {error}");
                return;
            }
        };

        let project_path = self
            .sample_project_path()
            .unwrap_or_else(|| project_path.to_path_buf());
        let document = document.clone();
        let output_path_for_worker = output_path.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("playlist-wav-render".to_owned())
            .spawn(move || {
                let result = render_playlist_with_vst3_to_wav_cancellable(
                    &document,
                    &project_path,
                    options,
                    &output_path_for_worker,
                    vst3_processor,
                    &worker_cancelled,
                );
                let _ = sender.send(result);
            });
        match worker {
            Ok(worker) => {
                self.pending_song_render = Some(PendingSongRender {
                    receiver,
                    cancelled,
                    worker,
                    output_path: output_path.clone(),
                });
                self.status = format!("Rendering Playlist mix to {}…", output_path.display());
            }
            Err(error) => {
                self.status = format!("Could not start Playlist render worker: {error}");
            }
        }
    }

    fn poll_song_render(&mut self) {
        let completed = self.pending_song_render.as_ref().and_then(|pending| {
            match pending.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("Playlist render worker stopped unexpectedly".to_owned()))
                }
                Err(TryRecvError::Empty) => None,
            }
        });
        let Some(result) = completed else {
            return;
        };
        let Some(pending) = self.pending_song_render.take() else {
            return;
        };
        if pending.worker.join().is_err() {
            self.status = "Playlist render worker panicked".to_owned();
            return;
        }
        match result {
            Ok(summary) => {
                self.status = format!(
                    "Rendered {} audio clips, {} Sampler clips ({} notes), and {} VST3 channels ({} notes) to {} at {} Hz; {} plug-in channels unloaded, {} scaled audio and {} scaled pattern clips skipped. Mixer effects and automation were not rendered",
                    summary.audio_clips_rendered,
                    summary.sampler_pattern_clips_rendered,
                    summary.sampler_notes_rendered,
                    summary.vst3_plugin_channels_rendered,
                    summary.vst3_notes_rendered,
                    pending.output_path.display(),
                    summary.sample_rate,
                    summary.vst3_plugin_channels_unloaded,
                    summary.audio_clips_skipped_unsupported_scale,
                    summary.pattern_clips_skipped_unsupported_scale,
                );
            }
            Err(error) => self.status = format!("Could not render Playlist mix: {error}"),
        }
    }

    fn export_selected_pattern_midi_dialog(&mut self) {
        let Some(pattern_id) = self.selected_pattern else {
            self.status = "Select a pattern before exporting MIDI".to_owned();
            return;
        };
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before exporting MIDI".to_owned();
            return;
        };
        let bytes =
            match MidiFile::encode_project_pattern(document, pattern_id, self.midi_channel_mapping)
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.status = format!("Could not export pattern MIDI: {error}");
                    return;
                }
            };
        let project_stem = self
            .current_path
            .as_deref()
            .and_then(Path::file_stem)
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "FL_Studio_Project".to_owned());
        let Some(output_path) = rfd::FileDialog::new()
            .set_title("Export pattern as MIDI")
            .set_file_name(format!("{project_stem}_pattern_{pattern_id}.mid"))
            .add_filter("MIDI files", &["mid", "midi"])
            .save_file()
        else {
            return;
        };
        match fs::write(&output_path, bytes) {
            Ok(()) => {
                self.status = format!("Exported pattern {pattern_id} to {}", output_path.display())
            }
            Err(error) => {
                self.status = format!("Could not write {}: {error}", output_path.display())
            }
        }
    }

    fn export_song_midi_dialog(&mut self) {
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before exporting MIDI".to_owned();
            return;
        };
        let arrangements = match document.arrangements() {
            Ok(arrangements) => arrangements,
            Err(error) => {
                self.status = format!("Could not read Playlist arrangements: {error}");
                return;
            }
        };
        let arrangement_id = self
            .selected_arrangement
            .filter(|id| arrangements.iter().any(|arrangement| arrangement.id == *id))
            .or_else(|| arrangements.first().map(|arrangement| arrangement.id));
        let Some(arrangement_id) = arrangement_id else {
            self.status = "This project has no Playlist arrangement to export".to_owned();
            return;
        };
        let bytes = match MidiFile::encode_project_song(
            document,
            arrangement_id,
            self.midi_channel_mapping,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not export song MIDI: {error}");
                return;
            }
        };
        let project_stem = self
            .current_path
            .as_deref()
            .and_then(Path::file_stem)
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "FL_Studio_Project".to_owned());
        let Some(output_path) = rfd::FileDialog::new()
            .set_title("Export Playlist arrangement as MIDI")
            .set_file_name(format!("{project_stem}.mid"))
            .add_filter("MIDI files", &["mid", "midi"])
            .save_file()
        else {
            return;
        };
        match fs::write(&output_path, bytes) {
            Ok(()) => {
                self.status = format!(
                    "Exported arrangement {arrangement_id} to {}",
                    output_path.display()
                )
            }
            Err(error) => {
                self.status = format!("Could not write {}: {error}", output_path.display())
            }
        }
    }

    fn request_piano_roll_zoom(&mut self, zoom: f32, source_view_x: f32, target_view_x: f32) {
        let old_zoom = self.piano_roll_zoom.clamp(0.06, 0.24);
        let new_zoom = zoom.clamp(0.06, 0.24);
        if (new_zoom - old_zoom).abs() <= f32::EPSILON {
            return;
        }
        let old_scale = (old_zoom * 0.9).clamp(0.05, 0.22);
        let new_scale = (new_zoom * 0.9).clamp(0.05, 0.22);
        let scale_ratio = new_scale / old_scale;
        let source_content_x = self.piano_roll_grid_scroll_offset.x + source_view_x;
        let anchored_content_x = 68.0 + (source_content_x - 68.0) * scale_ratio;
        let offset_x = (anchored_content_x - target_view_x).max(0.0);
        self.piano_roll_zoom = new_zoom;
        self.piano_roll_pending_scroll_offset =
            Some(Vec2::new(offset_x, self.piano_roll_grid_scroll_offset.y));
    }

    fn draw_notes(&mut self, ui: &mut egui::Ui, pattern: &Pattern, ppq: u16, snap_ticks: u32) {
        let key_low = 36u16;
        let key_high = 83u16;
        let key_height = 13.0;
        let keyboard_width = 68.0;
        if let (Some(viewport), Some(pointer)) = (
            self.piano_roll_grid_viewport,
            ui.input(|input| input.pointer.hover_pos()),
        ) && viewport.contains(pointer)
        {
            let zoom_in = ui.input(|input| input.key_pressed(egui::Key::PageUp));
            let zoom_out = ui.input(|input| input.key_pressed(egui::Key::PageDown));
            if zoom_in || zoom_out {
                let anchor = pointer.x - viewport.left();
                self.request_piano_roll_zoom(
                    if zoom_in {
                        self.piano_roll_zoom * 1.2
                    } else {
                        self.piano_roll_zoom / 1.2
                    },
                    anchor,
                    anchor,
                );
            }
        }
        let tick_scale = (self.piano_roll_zoom * 0.9).clamp(0.05, 0.22);
        let max_tick = pattern
            .notes
            .iter()
            .map(|note| note.position.saturating_add(note.length))
            .max()
            .unwrap_or(ppq as u32 * 16)
            .max(ppq as u32 * 16);
        let grid_width = (max_tick as f32 * tick_scale + 160.0).clamp(1400.0, 30000.0);
        let grid_height = f32::from(key_high - key_low + 1) * key_height;
        let mut notes_to_add = Vec::new();
        let pending_scroll_offset = self.piano_roll_pending_scroll_offset.take();
        let mut scroll_area = egui::ScrollArea::both()
            .id_salt("piano-roll-grid")
            .auto_shrink([false, false]);
        if self.piano_roll_event_editor_open {
            scroll_area = scroll_area.max_height((ui.available_height() - 142.0).max(180.0));
        }
        if let Some(offset) = pending_scroll_offset {
            scroll_area = scroll_area.scroll_offset(offset);
        }
        let scroll_output = scroll_area.show(ui, |ui| {
            let size = Vec2::new(keyboard_width + grid_width, grid_height);
            let (rect, grid_response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            let grid_geometry = PianoRollGrid {
                rect,
                keyboard_width,
                tick_scale,
                key_height,
                key_low,
                key_high,
                snap_ticks,
                ppq,
            };
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0, PANEL_DARK);
            if self.piano_roll_scale != PianoRollScale::None {
                for key in key_low..=key_high {
                    if !self
                        .piano_roll_scale
                        .contains(key, self.piano_roll_scale_root)
                    {
                        continue;
                    }
                    let row = key_high - key;
                    let y = rect.top() + f32::from(row) * key_height;
                    let scale_row = egui::Rect::from_min_size(
                        egui::pos2(rect.left() + keyboard_width, y),
                        Vec2::new(grid_width, key_height),
                    );
                    let is_root = key % 12 == u16::from(self.piano_roll_scale_root);
                    painter.rect_filled(
                        scale_row,
                        0,
                        if is_root {
                            Color32::from_rgb(41, 63, 47)
                        } else {
                            Color32::from_rgb(33, 47, 37)
                        },
                    );
                }
            }
            let measure_ticks = ppq as u32 * 4;
            let measure_width = measure_ticks as f32 * tick_scale;
            if measure_width > 0.0 {
                let measures = (grid_width / measure_width).ceil() as u32;
                for measure in 0..=measures {
                    let x = rect.left() + keyboard_width + measure as f32 * measure_width;
                    painter.line_segment(
                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                        Stroke::new(1.0, GRID),
                    );
                }
            }
            for key in key_low..=key_high {
                let row = key_high - key;
                let y = rect.top() + f32::from(row) * key_height;
                let keyboard_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.left(), y),
                    Vec2::new(keyboard_width, key_height),
                );
                let is_black = matches!(key % 12, 1 | 3 | 6 | 8 | 10);
                painter.rect_filled(
                    keyboard_rect,
                    0,
                    if is_black {
                        PANEL_LIGHT
                    } else {
                        Color32::from_rgb(188, 190, 193)
                    },
                );
                painter.text(
                    egui::pos2(keyboard_rect.left() + 5.0, keyboard_rect.center().y),
                    Align2::LEFT_CENTER,
                    note_name(key),
                    FontId::proportional(9.0),
                    if is_black {
                        TEXT
                    } else {
                        Color32::from_rgb(38, 39, 41)
                    },
                );
                painter.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    Stroke::new(1.0, GRID),
                );
            }

            let mut per_channel = BTreeMap::<u16, usize>::new();
            let mut note_rects = Vec::with_capacity(pattern.notes.len());
            let mut note_selection_rects = Vec::with_capacity(pattern.notes.len());
            for note in &pattern.notes {
                let channel_index = per_channel.entry(note.channel_id).or_default();
                let left = rect.left() + keyboard_width + note.position as f32 * tick_scale;
                let y =
                    rect.top() + f32::from(key_high.saturating_sub(note.key)) * key_height + 1.0;
                let note_rect = egui::Rect::from_min_size(
                    egui::pos2(left, y),
                    Vec2::new((note.length as f32 * tick_scale).max(4.0), key_height - 2.0),
                );
                note_rects.push(note_rect);
                note_selection_rects.push((note.channel_id, *channel_index, note_rect));
                let ghost = self.selected_note_channel != Some(note.channel_id);
                if ghost && !self.piano_roll_ghost_channels {
                    *channel_index += 1;
                    continue;
                }
                let selected = self.selected_piano_notes.contains(&(
                    pattern.id,
                    note.channel_id,
                    *channel_index,
                ));
                let channel_color = if self.piano_roll_color_by_midi_channel {
                    MIDI_CHANNEL_COLORS[usize::from(note.midi_channel & 0x0f)]
                } else {
                    GREEN
                };
                painter.rect_filled(
                    note_rect,
                    egui::CornerRadius::same(2),
                    if selected {
                        channel_color.gamma_multiply(1.3)
                    } else if ghost {
                        Color32::from_rgb(99, 112, 136)
                    } else {
                        channel_color
                    },
                );
                let resize_handle = egui::Rect::from_min_max(
                    egui::pos2(
                        (note_rect.right() - 5.0).max(note_rect.left()),
                        note_rect.top(),
                    ),
                    note_rect.right_bottom(),
                );
                painter.rect_filled(
                    resize_handle,
                    egui::CornerRadius::same(1),
                    Color32::from_white_alpha(if selected { 100 } else { 45 }),
                );
                let response = ui.interact(
                    note_rect,
                    Id::new(("piano-note", pattern.id, note.channel_id, *channel_index)),
                    if self.piano_roll_zoom_mode {
                        Sense::hover()
                    } else {
                        Sense::click_and_drag()
                    },
                );
                let note_id = (pattern.id, note.channel_id, *channel_index);
                let modifiers = ui.input(|input| input.modifiers);
                let audition_gesture = response.clicked()
                    || (ui.input(|input| input.pointer.primary_down()) && response.hovered());
                if self.piano_roll_playback_mode
                    && audition_gesture
                    && self.last_piano_roll_audition != Some(note_id)
                {
                    self.last_piano_roll_audition = Some(note_id);
                    self.play_piano_roll_note(pattern.id, note.channel_id, *channel_index);
                }
                if response.clicked()
                    && !self.piano_roll_zoom_mode
                    && !self.piano_roll_playback_mode
                {
                    let additive = modifiers.shift || modifiers.command;
                    if !additive
                        || self
                            .selected_piano_notes
                            .iter()
                            .any(|(_, channel_id, _)| *channel_id != note.channel_id)
                    {
                        self.selected_piano_notes.clear();
                    }
                    if additive && self.selected_piano_notes.remove(&note_id) {
                        self.selected_note = self
                            .selected_piano_notes
                            .iter()
                            .rev()
                            .find(|(selected_pattern, _, _)| *selected_pattern == pattern.id)
                            .copied();
                    } else {
                        self.selected_piano_notes.insert(note_id);
                        self.selected_note = Some(note_id);
                    }
                    self.selected_note_channel = Some(note.channel_id);
                    let selected_count = self
                        .selected_piano_notes
                        .iter()
                        .filter(|(selected_pattern, _, _)| *selected_pattern == pattern.id)
                        .count();
                    self.status = format!(
                        "Selected {} note{}",
                        selected_count,
                        if selected_count == 1 { "" } else { "s" }
                    );
                }
                if response.is_pointer_button_down_on()
                    && !self.piano_roll_zoom_mode
                    && !self.piano_roll_playback_mode
                    && self.active_note_drag.is_none()
                    && !modifiers.command
                    && !modifiers.shift
                {
                    let resize = response
                        .interact_pointer_pos()
                        .is_some_and(|pointer| pointer.x >= note_rect.right() - 6.0);
                    if !self.selected_piano_notes.contains(&note_id)
                        || self
                            .selected_piano_notes
                            .iter()
                            .any(|(_, channel_id, _)| *channel_id != note.channel_id)
                    {
                        self.selected_piano_notes
                            .retain(|(selected_pattern, _, _)| *selected_pattern != pattern.id);
                        self.selected_piano_notes.insert(note_id);
                    }
                    let mut selected_note_indices = BTreeMap::<u16, usize>::new();
                    let mut targets = Vec::new();
                    for selected_note in &pattern.notes {
                        let selected_channel_index = selected_note_indices
                            .entry(selected_note.channel_id)
                            .or_default();
                        if self.selected_piano_notes.contains(&(
                            pattern.id,
                            selected_note.channel_id,
                            *selected_channel_index,
                        )) {
                            targets.push(NoteDragTarget {
                                channel_id: selected_note.channel_id,
                                channel_note_index: *selected_channel_index,
                                start_position: selected_note.position,
                                start_length: selected_note.length,
                                start_key: selected_note.key,
                            });
                        }
                        *selected_channel_index += 1;
                    }
                    let start_pointer = response
                        .interact_pointer_pos()
                        .unwrap_or(note_rect.center());
                    self.active_note_drag = Some(ActiveNoteDrag {
                        pattern_id: pattern.id,
                        channel_id: note.channel_id,
                        channel_note_index: *channel_index,
                        start_pointer,
                        targets,
                        kind: if resize {
                            NoteDragKind::Resize
                        } else {
                            NoteDragKind::Move
                        },
                    });
                    self.selected_note = Some((pattern.id, note.channel_id, *channel_index));
                    self.selected_note_channel = Some(note.channel_id);
                }
                if response.dragged()
                    && !self.piano_roll_zoom_mode
                    && !self.piano_roll_playback_mode
                    && let Some(drag) = self.active_note_drag.as_ref().filter(|drag| {
                        drag.pattern_id == pattern.id
                            && drag.channel_id == note.channel_id
                            && drag.channel_note_index == *channel_index
                    })
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    let tick_delta =
                        ((pointer.x - drag.start_pointer.x) / tick_scale).round() as i64;
                    let semitones =
                        ((drag.start_pointer.y - pointer.y) / key_height).round() as i32;
                    let mut updated = self.document.clone();
                    let result = if let Some(document) = updated.as_mut() {
                        drag.targets.iter().try_for_each(|target| {
                            let edit = match drag.kind {
                                NoteDragKind::Move => PatternNoteEdit {
                                    position: Some(snap_note_tick(
                                        i64::from(target.start_position).saturating_add(tick_delta),
                                        snap_ticks,
                                        0,
                                    )),
                                    key: Some(
                                        i32::from(target.start_key)
                                            .saturating_add(semitones)
                                            .clamp(i32::from(key_low), i32::from(key_high))
                                            as u16,
                                    ),
                                    ..PatternNoteEdit::default()
                                },
                                NoteDragKind::Resize => PatternNoteEdit {
                                    length: Some(snap_note_tick(
                                        i64::from(target.start_length).saturating_add(tick_delta),
                                        snap_ticks,
                                        1,
                                    )),
                                    ..PatternNoteEdit::default()
                                },
                            };
                            document.edit_pattern_note(
                                drag.pattern_id,
                                target.channel_id,
                                target.channel_note_index,
                                edit,
                            )
                        })
                    } else {
                        return;
                    };
                    if result.is_ok() {
                        self.document = updated;
                        self.dirty = true;
                        self.status = if drag.targets.len() > 1 {
                            let action = match drag.kind {
                                NoteDragKind::Move => "Moved",
                                NoteDragKind::Resize => "Resized",
                            };
                            format!("{action} {} selected notes", drag.targets.len())
                        } else {
                            match drag.kind {
                                NoteDragKind::Move => "Piano roll note moved".to_owned(),
                                NoteDragKind::Resize => "Piano roll note length changed".to_owned(),
                            }
                        };
                    } else if let Err(error) = result {
                        self.status = format!("Could not edit selected notes: {error}");
                    }
                }
                if response.drag_stopped()
                    && self.active_note_drag.as_ref().is_some_and(|drag| {
                        drag.pattern_id == pattern.id
                            && drag.channel_id == note.channel_id
                            && drag.channel_note_index == *channel_index
                    })
                {
                    self.active_note_drag = None;
                }
                *channel_index += 1;
            }
            let modifiers = ui.input(|input| input.modifiers);
            if self.piano_roll_zoom_mode
                && grid_response.drag_started_by(PointerButton::Primary)
                && let Some(origin) = ui
                    .input(|input| input.pointer.press_origin())
                    .or_else(|| grid_response.interact_pointer_pos())
            {
                let grid_body = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + keyboard_width, rect.top()),
                    rect.right_bottom(),
                );
                if grid_body.contains(origin) {
                    self.piano_roll_zoom_drag = Some(PianoRollZoomDrag {
                        pattern_id: pattern.id,
                        start: origin,
                        current: origin,
                    });
                }
            }
            if let Some(mut zoom_drag) = self
                .piano_roll_zoom_drag
                .filter(|drag| drag.pattern_id == pattern.id)
            {
                let grid_body = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + keyboard_width, rect.top()),
                    rect.right_bottom(),
                );
                if let Some(pointer) = grid_response
                    .interact_pointer_pos()
                    .or_else(|| ui.input(|input| input.pointer.interact_pos()))
                {
                    zoom_drag.current = egui::pos2(
                        pointer.x.clamp(grid_body.left(), grid_body.right()),
                        pointer.y.clamp(grid_body.top(), grid_body.bottom()),
                    );
                }
                let zoom_rect = egui::Rect::from_two_pos(zoom_drag.start, zoom_drag.current);
                painter.rect_filled(
                    zoom_rect,
                    0,
                    Color32::from_rgba_unmultiplied(73, 128, 174, 36),
                );
                painter.rect_stroke(
                    zoom_rect,
                    egui::CornerRadius::ZERO,
                    Stroke::new(1.0, BLUE),
                    egui::StrokeKind::Inside,
                );
                if grid_response.drag_stopped_by(PointerButton::Primary) {
                    if zoom_rect.width() >= 12.0
                        && let Some(viewport) = self.piano_roll_grid_viewport
                    {
                        let visible_grid_width = (viewport.width() - keyboard_width).max(1.0);
                        let zoom_factor = visible_grid_width / zoom_rect.width();
                        self.request_piano_roll_zoom(
                            self.piano_roll_zoom * zoom_factor,
                            zoom_rect.left() - viewport.left(),
                            keyboard_width,
                        );
                        self.status = "Zoomed to Piano roll selection".to_owned();
                    }
                    self.piano_roll_zoom_drag = None;
                } else {
                    self.piano_roll_zoom_drag = Some(zoom_drag);
                }
            }
            if self.piano_roll_zoom_mode
                && grid_response.clicked_by(PointerButton::Primary)
                && let Some(pointer) = grid_response.interact_pointer_pos()
                && pointer.x >= rect.left() + keyboard_width
                && !note_rects
                    .iter()
                    .any(|note_rect| note_rect.contains(pointer))
                && let Some(viewport) = self.piano_roll_grid_viewport
            {
                let anchor = pointer.x - viewport.left();
                self.request_piano_roll_zoom(self.piano_roll_zoom / 1.2, anchor, anchor);
                self.status = "Zoomed out around cursor".to_owned();
            }
            if grid_response.drag_started_by(PointerButton::Primary)
                && !self.piano_roll_zoom_mode
                && (self.piano_roll_select_mode || modifiers.command)
                && let Some(origin) = ui
                    .input(|input| input.pointer.press_origin())
                    .or_else(|| grid_response.interact_pointer_pos())
                && let Some(channel_id) = self.selected_note_channel
            {
                let grid_body = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + keyboard_width, rect.top()),
                    rect.right_bottom(),
                );
                if grid_body.contains(origin) {
                    let additive = modifiers.shift;
                    if !additive {
                        self.selected_piano_notes
                            .retain(|(selected_pattern, _, _)| *selected_pattern != pattern.id);
                    }
                    self.piano_roll_selection_drag = Some(PianoRollSelectionDrag {
                        pattern_id: pattern.id,
                        channel_id,
                        start: origin,
                        current: origin,
                        additive,
                    });
                }
            }
            if let Some(mut selection) = self
                .piano_roll_selection_drag
                .filter(|selection| selection.pattern_id == pattern.id)
            {
                let grid_body = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + keyboard_width, rect.top()),
                    rect.right_bottom(),
                );
                if let Some(pointer) = grid_response
                    .interact_pointer_pos()
                    .or_else(|| ui.input(|input| input.pointer.interact_pos()))
                {
                    selection.current = egui::pos2(
                        pointer.x.clamp(grid_body.left(), grid_body.right()),
                        pointer.y.clamp(grid_body.top(), grid_body.bottom()),
                    );
                }
                let selection_rect = egui::Rect::from_two_pos(selection.start, selection.current);
                painter.rect_filled(
                    selection_rect,
                    0,
                    Color32::from_rgba_unmultiplied(73, 128, 174, 36),
                );
                painter.rect_stroke(
                    selection_rect,
                    egui::CornerRadius::ZERO,
                    Stroke::new(1.0, BLUE),
                    egui::StrokeKind::Inside,
                );
                if grid_response.drag_stopped_by(PointerButton::Primary) {
                    let enclosed = note_selection_rects
                        .iter()
                        .filter(|(channel_id, _, note_rect)| {
                            *channel_id == selection.channel_id
                                && note_rect.intersects(selection_rect)
                        })
                        .map(|(channel_id, channel_note_index, _)| {
                            (pattern.id, *channel_id, *channel_note_index)
                        })
                        .collect::<Vec<_>>();
                    for note_id in enclosed {
                        if selection.additive && self.selected_piano_notes.remove(&note_id) {
                            continue;
                        }
                        self.selected_piano_notes.insert(note_id);
                    }
                    self.selected_note = self
                        .selected_piano_notes
                        .iter()
                        .rev()
                        .find(|(selected_pattern, _, _)| *selected_pattern == pattern.id)
                        .copied();
                    self.selected_note_channel = Some(selection.channel_id);
                    let selected_count = self
                        .selected_piano_notes
                        .iter()
                        .filter(|(selected_pattern, _, _)| *selected_pattern == pattern.id)
                        .count();
                    self.status = format!("Selected {selected_count} notes");
                    self.piano_roll_selection_drag = None;
                } else {
                    self.piano_roll_selection_drag = Some(selection);
                }
            }
            if self.piano_roll_stamp_mode
                && !self.piano_roll_select_mode
                && !self.piano_roll_zoom_mode
                && grid_response.clicked_by(PointerButton::Primary)
                && let (Some(pointer), Some(channel_id)) = (
                    grid_response.interact_pointer_pos(),
                    self.selected_note_channel,
                )
                && !note_rects
                    .iter()
                    .any(|note_rect| note_rect.contains(pointer))
                && let Some(mut root_note) =
                    note_from_grid_position(pointer, grid_geometry, channel_id)
            {
                root_note.length = snap_ticks.max(1);
                let chord_pitches = self.piano_roll_chord_stamp.pitches(
                    root_note.key,
                    self.piano_roll_scale,
                    self.piano_roll_scale_root,
                );
                notes_to_add.extend(chord_pitches.into_iter().map(|key| {
                    let mut note = root_note.clone();
                    note.key = key;
                    note
                }));
            }
            if self.piano_roll_paint_mode
                && !(self.piano_roll_select_mode || self.piano_roll_zoom_mode || modifiers.command)
                && ui.input(|input| input.pointer.primary_down())
                && let (Some(pointer), Some(channel_id)) = (
                    grid_response.interact_pointer_pos(),
                    self.selected_note_channel,
                )
                && !note_rects
                    .iter()
                    .any(|note_rect| note_rect.contains(pointer))
                && let Some(mut note) = note_from_grid_position(pointer, grid_geometry, channel_id)
            {
                note.length = snap_ticks.max(1);
                let note_key = (pattern.id, channel_id, note.key, note.position);
                let already_exists = pattern.notes.iter().any(|existing| {
                    existing.channel_id == channel_id
                        && existing.key == note.key
                        && existing.position == note.position
                });
                if !already_exists && self.last_painted_note != Some(note_key) {
                    self.last_painted_note = Some(note_key);
                    notes_to_add.push(note);
                }
            }
            if !self.piano_roll_paint_mode
                && !self.piano_roll_stamp_mode
                && !self.piano_roll_select_mode
                && !self.piano_roll_zoom_mode
                && !modifiers.command
                && grid_response.double_clicked()
                && let (Some(pointer), Some(channel_id)) = (
                    grid_response.interact_pointer_pos(),
                    self.selected_note_channel,
                )
                && !note_rects
                    .iter()
                    .any(|note_rect| note_rect.contains(pointer))
                && let Some(note) = note_from_grid_position(pointer, grid_geometry, channel_id)
            {
                notes_to_add.push(note);
            }
        });
        self.piano_roll_grid_viewport = Some(scroll_output.inner_rect);
        self.piano_roll_grid_scroll_offset = scroll_output.state.offset;
        if self.piano_roll_event_editor_open {
            self.draw_piano_roll_event_editor(
                ui,
                pattern,
                ppq,
                tick_scale,
                keyboard_width,
                scroll_output.state.offset.x,
            );
        }
        if !ui.input(|input| input.pointer.primary_down()) {
            self.last_painted_note = None;
            self.last_piano_roll_audition = None;
        }
        if !ui.input(|input| input.pointer.primary_down()) {
            self.active_note_drag = None;
            self.piano_roll_selection_drag = None;
            self.piano_roll_zoom_drag = None;
        }
        if !notes_to_add.is_empty() {
            let stamped_chord = self.piano_roll_stamp_mode;
            let only_one = self.piano_roll_stamp_only_one;
            let channel_id = notes_to_add[0].channel_id;
            let first_note_index = pattern
                .notes
                .iter()
                .filter(|existing| existing.channel_id == channel_id)
                .count();
            let added_count = notes_to_add.len();
            if let Some(document) = &mut self.document {
                match document.add_pattern_notes(pattern.id, &notes_to_add) {
                    Ok(()) => {
                        self.selected_piano_notes.clear();
                        self.selected_piano_notes.extend(
                            (first_note_index..first_note_index + added_count)
                                .map(|note_index| (pattern.id, channel_id, note_index)),
                        );
                        self.selected_note = self.selected_piano_notes.iter().next_back().copied();
                        self.selected_note_channel = Some(channel_id);
                        self.dirty = true;
                        if stamped_chord && only_one {
                            self.piano_roll_stamp_mode = false;
                        }
                        self.status = if stamped_chord {
                            format!("Stamped {} notes in pattern {}", added_count, pattern.id)
                        } else if self.piano_roll_paint_mode {
                            format!("Painted note in pattern {}", pattern.id)
                        } else {
                            format!("Added note to pattern {}", pattern.id)
                        };
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
        }
    }

    fn draw_piano_roll_event_editor(
        &mut self,
        ui: &mut egui::Ui,
        pattern: &Pattern,
        ppq: u16,
        tick_scale: f32,
        keyboard_width: f32,
        horizontal_scroll: f32,
    ) {
        ui.horizontal(|ui| {
            ui.strong(format!("{} events", self.piano_roll_event_target.label()));
            ui.label("Drag stems to change note properties · Shift+F cycles targets");
        });
        let size = Vec2::new(ui.available_width().max(1.0), 102.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let plot_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + keyboard_width, rect.top() + 8.0),
            egui::pos2(rect.right(), rect.bottom() - 8.0),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, PANEL_DARK);
        painter.rect_filled(
            egui::Rect::from_min_max(rect.min, egui::pos2(plot_rect.left(), rect.bottom())),
            0,
            PANEL_LIGHT,
        );
        painter.rect_stroke(
            rect,
            egui::CornerRadius::ZERO,
            Stroke::new(1.0, GRID),
            egui::StrokeKind::Inside,
        );
        painter.text(
            egui::pos2(rect.left() + 6.0, rect.top() + 14.0),
            Align2::LEFT_TOP,
            self.piano_roll_event_target.label(),
            FontId::proportional(10.0),
            TEXT,
        );
        let maximum = self.piano_roll_event_target.maximum();
        let baseline = plot_rect.bottom() - 1.0;
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), baseline),
                egui::pos2(plot_rect.right(), baseline),
            ],
            Stroke::new(1.0, GRID),
        );
        let measure_ticks = u32::from(ppq).saturating_mul(4).max(1);
        let measure_width = measure_ticks as f32 * tick_scale;
        if measure_width > 0.0 {
            let measure_count = (rect.width() / measure_width).ceil() as u32 + 2;
            for measure in 0..measure_count {
                let x = plot_rect.left() + measure as f32 * measure_width - horizontal_scroll;
                if x >= plot_rect.left() && x <= plot_rect.right() {
                    painter.line_segment(
                        [egui::pos2(x, plot_rect.top()), egui::pos2(x, baseline)],
                        Stroke::new(1.0, GRID),
                    );
                }
            }
        }

        let Some(channel_id) = self.selected_note_channel else {
            return;
        };
        let mut same_onset_count = BTreeMap::<u32, u32>::new();
        let target = self.piano_roll_event_target;
        for (note_index, note) in pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .enumerate()
        {
            let occurrence = same_onset_count.entry(note.position).or_default();
            let stagger = (*occurrence as f32 * 4.0).min(28.0);
            *occurrence = occurrence.saturating_add(1);
            let x =
                plot_rect.left() + note.position as f32 * tick_scale - horizontal_scroll + stagger;
            if x < plot_rect.left() - 8.0 || x > plot_rect.right() + 8.0 {
                continue;
            }
            let value = target.value(note).min(maximum);
            let normalized = value as f32 / f32::from(maximum.max(1));
            let point = egui::pos2(
                x,
                egui::lerp(plot_rect.bottom()..=plot_rect.top(), normalized),
            );
            let note_id = (pattern.id, channel_id, note_index);
            let selected = self.selected_piano_notes.contains(&note_id);
            let color = if selected { BLUE } else { GREEN };
            painter.line_segment(
                [egui::pos2(x, baseline), point],
                Stroke::new(if selected { 2.0 } else { 1.5 }, color),
            );
            painter.circle_filled(point, if selected { 4.5 } else { 3.5 }, color);
            let hit_rect = egui::Rect::from_min_max(
                egui::pos2(x - 6.0, point.y - 7.0),
                egui::pos2(x + 6.0, baseline + 2.0),
            );
            let response = ui.interact(
                hit_rect,
                Id::new(("piano-note-event", pattern.id, channel_id, note_index)),
                Sense::click_and_drag(),
            );
            if response.clicked() {
                self.selected_piano_notes.clear();
                self.selected_piano_notes.insert(note_id);
                self.selected_note = Some(note_id);
                self.selected_note_channel = Some(channel_id);
            }
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let normalized = ((baseline - pointer.y) / plot_rect.height()).clamp(0.0, 1.0);
                let value = (normalized * f32::from(maximum)).round() as u16;
                if value != target.value(note).min(maximum)
                    && let Some(document) = &mut self.document
                {
                    match document.edit_pattern_note(
                        pattern.id,
                        channel_id,
                        note_index,
                        target.edit(value),
                    ) {
                        Ok(()) => {
                            self.dirty = true;
                            self.status = format!("Updated note {}", target.label());
                        }
                        Err(error) => {
                            self.status = format!("Could not update note event: {error}");
                        }
                    }
                }
            }
        }
    }

    fn selected_note_editor(&mut self, ui: &mut egui::Ui, pattern: &Pattern) {
        let Some((pattern_id, channel_id, channel_note_index)) = self.selected_note else {
            return;
        };
        if pattern_id != pattern.id {
            self.selected_note = None;
            return;
        }
        let mut current_index = 0usize;
        let Some(note) = pattern.notes.iter().find(|note| {
            if note.channel_id == channel_id {
                let is_selected = current_index == channel_note_index;
                current_index += 1;
                is_selected
            } else {
                false
            }
        }) else {
            self.selected_note = None;
            return;
        };
        ui.small(format!(
            "Chord at this onset: {}",
            detect_chord_name(&pattern.notes, channel_id, note.position)
        ));
        let mut position = note.position;
        let mut length = note.length;
        let mut key = note.key;
        let mut velocity = note.velocity;
        let mut flags = note.flags;
        let mut group = note.group;
        let mut fine_pitch = note.fine_pitch;
        let mut release = note.release;
        let mut midi_channel = note.midi_channel;
        let mut pan = note.pan;
        let mut mod_x = note.mod_x;
        let mut mod_y = note.mod_y;
        let mut delete_requested = false;
        let mut position_changed = false;
        let mut length_changed = false;
        let mut key_changed = false;
        let mut velocity_changed = false;
        let mut flags_changed = false;
        let mut group_changed = false;
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Note");
            position_changed = ui
                .add(
                    egui::DragValue::new(&mut position)
                        .prefix("Start ")
                        .speed(1.0),
                )
                .changed();
            length_changed = ui
                .add(
                    egui::DragValue::new(&mut length)
                        .prefix("Length ")
                        .speed(1.0),
                )
                .changed();
            key_changed = ui
                .add(egui::DragValue::new(&mut key).prefix("Key ").speed(0.1))
                .changed();
            velocity_changed = ui
                .add(
                    egui::DragValue::new(&mut velocity)
                        .prefix("Velocity ")
                        .speed(0.1),
                )
                .changed();
            delete_requested = ui.button("Delete note").clicked();
            if ui.button("Deselect").clicked() {
                self.selected_note = None;
                self.selected_piano_notes.clear();
            }
        });
        let mut fine_pitch_changed = false;
        let mut release_changed = false;
        let mut midi_channel_changed = false;
        let mut pan_changed = false;
        let mut mod_x_changed = false;
        let mut mod_y_changed = false;
        ui.collapsing("Advanced note properties", |ui| {
            ui.horizontal_wrapped(|ui| {
                flags_changed = ui
                    .add(
                        egui::DragValue::new(&mut flags)
                            .prefix("Flags raw ")
                            .speed(0.1),
                    )
                    .changed();
                group_changed = ui
                    .add(
                        egui::DragValue::new(&mut group)
                            .prefix("Group raw ")
                            .speed(0.1),
                    )
                    .changed();
                fine_pitch_changed = ui
                    .add(
                        egui::DragValue::new(&mut fine_pitch)
                            .prefix("Fine pitch ")
                            .speed(0.1),
                    )
                    .changed();
                release_changed = ui
                    .add(
                        egui::DragValue::new(&mut release)
                            .prefix("Release ")
                            .speed(0.1),
                    )
                    .changed();
                midi_channel_changed = ui
                    .add(
                        egui::DragValue::new(&mut midi_channel)
                            .prefix("MIDI channel raw ")
                            .speed(0.1),
                    )
                    .changed();
                pan_changed = ui
                    .add(egui::DragValue::new(&mut pan).prefix("Pan ").speed(0.1))
                    .changed();
                mod_x_changed = ui
                    .add(egui::DragValue::new(&mut mod_x).prefix("Mod X ").speed(0.1))
                    .changed();
                mod_y_changed = ui
                    .add(egui::DragValue::new(&mut mod_y).prefix("Mod Y ").speed(0.1))
                    .changed();
            });
        });
        if !delete_requested
            && (position_changed
                || length_changed
                || key_changed
                || velocity_changed
                || flags_changed
                || group_changed
                || fine_pitch_changed
                || release_changed
                || midi_channel_changed
                || pan_changed
                || mod_x_changed
                || mod_y_changed)
        {
            let edit = PatternNoteEdit {
                position: position_changed.then_some(position),
                length: length_changed.then_some(length),
                key: key_changed.then_some(key),
                velocity: velocity_changed.then_some(velocity),
                flags: flags_changed.then_some(flags),
                group: group_changed.then_some(group),
                fine_pitch: fine_pitch_changed.then_some(fine_pitch),
                release: release_changed.then_some(release),
                midi_channel: midi_channel_changed.then_some(midi_channel),
                pan: pan_changed.then_some(pan),
                mod_x: mod_x_changed.then_some(mod_x),
                mod_y: mod_y_changed.then_some(mod_y),
            };
            if let Some(document) = &mut self.document
                && document
                    .edit_pattern_note(pattern_id, channel_id, channel_note_index, edit)
                    .is_ok()
            {
                self.dirty = true;
                self.status = "Piano roll note updated".to_owned();
            }
        }
        if delete_requested && let Some(document) = &mut self.document {
            match document.delete_pattern_note(pattern_id, channel_id, channel_note_index) {
                Ok(()) => {
                    self.selected_note = None;
                    self.selected_piano_notes.clear();
                    self.dirty = true;
                    self.status = format!("Deleted note from pattern {pattern_id}");
                }
                Err(error) => self.status = error.to_string(),
            }
        }
    }

    fn mixer(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.document.as_ref() else {
            empty_view(ui, "Open a project to see the Mixer");
            return;
        };
        let inserts = document.mixer_inserts();
        let parameters = document.mixer_parameter_records().unwrap_or_default();
        let parameter_count = parameters.len();
        ui.horizontal(|ui| {
            ui.strong("Mixer");
            ui.separator();
            ui.label(format!("{} insert records", inserts.len()));
            ui.separator();
            ui.label(format!("{parameter_count} parameter records"));
        });
        ui.separator();
        if inserts.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("No recognized Mixer insert records");
                    ui.label(
                        egui::RichText::new(
                            "This project may use an older or not yet decoded Mixer layout",
                        )
                        .color(MUTED),
                    );
                });
            });
            return;
        }

        let active_insert = self
            .selected_mixer_insert
            .filter(|ordinal| inserts.iter().any(|insert| insert.ordinal() == *ordinal))
            .or_else(|| inserts.first().map(|insert| insert.ordinal()));
        self.selected_mixer_insert = active_insert;

        let mut rename_edits = Vec::new();
        let mut parameter_edits = Vec::new();
        let available_height = ui.available_height();
        let inspector_width = 238.0_f32.min((ui.available_width() * 0.3).max(190.0));
        let bank_width = (ui.available_width() - inspector_width - 10.0).max(180.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(bank_width, available_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Insert tracks");
                        ui.label(egui::RichText::new("Select a strip to inspect it").color(MUTED));
                    });
                    ui.separator();
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                for insert in &inserts {
                                    let selected = active_insert == Some(insert.ordinal());
                                    let fill = if selected { PANEL_LIGHT } else { PANEL_DARK };
                                    let stroke = if selected {
                                        Stroke::new(1.5, GREEN)
                                    } else {
                                        Stroke::new(1.0, GRID)
                                    };
                                    egui::Frame::new()
                                        .fill(fill)
                                        .stroke(stroke)
                                        .inner_margin(7.0)
                                        .show(ui, |ui| {
                                            ui.set_min_width(104.0);
                                            ui.set_max_width(104.0);
                                            ui.set_min_height((available_height - 42.0).max(180.0));
                                            ui.vertical(|ui| {
                                                ui.small(format!(
                                                    "INSERT {:02}",
                                                    insert.ordinal() + 1
                                                ));
                                                let display_name = insert
                                                    .name()
                                                    .filter(|name| !name.is_empty())
                                                    .map(str::to_owned)
                                                    .unwrap_or_else(|| {
                                                        format!("Insert {}", insert.ordinal() + 1)
                                                    });
                                                if ui
                                                    .add_sized(
                                                        [104.0, 26.0],
                                                        egui::Button::selectable(
                                                            selected,
                                                            display_name,
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    self.selected_mixer_insert =
                                                        Some(insert.ordinal());
                                                }
                                                ui.separator();
                                                ui.small("INPUT");
                                                ui.monospace(insert.input_raw().to_string());
                                                ui.add_space(8.0);
                                                ui.small("OUTPUT");
                                                ui.monospace(insert.output_raw().to_string());
                                                ui.separator();
                                                ui.centered_and_justified(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "Mixer controls\nnot decoded",
                                                        )
                                                        .color(MUTED),
                                                    );
                                                });
                                            });
                                        });
                                }
                            });
                        });
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                Vec2::new(inspector_width, available_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.strong("Track inspector");
                    ui.separator();
                    if let Some(insert) = inserts
                        .iter()
                        .find(|insert| Some(insert.ordinal()) == self.selected_mixer_insert)
                    {
                        ui.label(format!("Insert {}", insert.ordinal() + 1));
                        let mut name = insert.name().unwrap_or_default().to_owned();
                        if ui
                            .add(egui::TextEdit::singleline(&mut name).hint_text("Insert name"))
                            .changed()
                        {
                            rename_edits.push((insert.ordinal(), name));
                        }
                        ui.add_space(6.0);
                        ui.label("Routing fields");
                        ui.small(format!("Input raw: {}", insert.input_raw()));
                        ui.small(format!("Output raw: {}", insert.output_raw()));
                        ui.small(format!("Color raw: 0x{:08X}", insert.color_raw()));
                        ui.small(format!("Icon raw: {:?}", insert.icon_raw()));
                        ui.small(format!("Events: {:?}", insert.event_range()));
                        ui.separator();
                        ui.label("Effect slots");
                        ui.label(
                            egui::RichText::new("Slot contents are preserved but not decoded yet.")
                                .color(MUTED),
                        );
                        for slot in 1..=10 {
                            let width = ui.available_width();
                            ui.add_enabled(
                                false,
                                egui::Button::new(format!("{slot:02}   State opaque"))
                                    .min_size(Vec2::new(width, 22.0)),
                            );
                        }
                        ui.collapsing(
                            format!("Project 0xE1 records ({})", parameters.len()),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(
                                        "Values are raw signed integers. Target and slot bits are candidate fields.",
                                    )
                                    .color(MUTED),
                                );
                                egui::ScrollArea::vertical()
                                    .id_salt("mixer-parameter-records")
                                    .max_height(280.0)
                                    .show(ui, |ui| {
                                        for record in &parameters {
                                            ui.horizontal(|ui| {
                                                ui.vertical(|ui| {
                                                    ui.small(format!(
                                                        "Event {} / record {}",
                                                        record.event_index(),
                                                        record.record_index()
                                                    ));
                                                    ui.small(format!(
                                                        "ID {} · {:?}",
                                                        record.parameter_id(),
                                                        record.kind()
                                                    ));
                                                    ui.small(format!(
                                                        "Target bits {} · slot bits {} · scope {}",
                                                        record.target_index(),
                                                        record.slot_index(),
                                                        record.target_scope_raw()
                                                    ));
                                                });
                                                let mut value = record.value();
                                                if ui
                                                    .add(
                                                        egui::DragValue::new(&mut value)
                                                            .speed(1.0),
                                                    )
                                                    .changed()
                                                {
                                                    parameter_edits.push((
                                                        record.event_index(),
                                                        record.record_index(),
                                                        value,
                                                    ));
                                                }
                                            });
                                            ui.separator();
                                        }
                                    });
                            },
                        );
                    } else {
                        ui.label(egui::RichText::new("Select an insert track").color(MUTED));
                    }
                },
            );
        });

        if !rename_edits.is_empty()
            && let Some(document) = self.document.as_mut()
        {
            for (insert_ordinal, name) in rename_edits {
                match document.set_mixer_insert_name(insert_ordinal, &name) {
                    Ok(()) => {
                        self.dirty = true;
                        self.status = format!("Renamed Mixer insert {}", insert_ordinal + 1);
                    }
                    Err(error) => {
                        self.status = format!("Could not rename Mixer insert: {error}");
                    }
                }
            }
        }
        if !parameter_edits.is_empty()
            && let Some(document) = self.document.as_mut()
        {
            for (event_index, record_index, value) in parameter_edits {
                match document.set_mixer_parameter_record_value(event_index, record_index, value) {
                    Ok(()) => {
                        self.dirty = true;
                        self.status = format!(
                            "Edited Mixer parameter event {event_index}, record {record_index}"
                        );
                    }
                    Err(error) => {
                        self.status = format!("Could not edit Mixer parameter: {error}");
                    }
                }
            }
        }
    }

    fn audio_settings_view(&mut self, ui: &mut egui::Ui) {
        let mut start_requested = false;
        let mut stop_requested = false;

        ui.horizontal(|ui| {
            ui.strong("Audio settings");
            ui.separator();
            if ui.button("Refresh devices").clicked() {
                self.audio_catalog = enumerate_devices();
            }
        });
        ui.label(
            egui::RichText::new(
                "Select separate input and output devices. The input meter checks capture; monitor input to hear it through the selected output.",
            )
            .color(MUTED),
        );
        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.audio_settings.enable_input, "Enable input");
            ui.checkbox(&mut self.audio_settings.enable_output, "Enable output");
        });

        let input_name = self
            .audio_settings
            .input_device_id
            .as_ref()
            .and_then(|id| {
                self.audio_catalog
                    .inputs
                    .iter()
                    .find(|device| &device.id == id)
            })
            .map(|device| device.name.clone())
            .unwrap_or_else(|| {
                self.audio_catalog
                    .default_input_id
                    .as_ref()
                    .and_then(|id| {
                        self.audio_catalog
                            .inputs
                            .iter()
                            .find(|device| &device.id == id)
                    })
                    .map(|device| format!("System default · {}", device.name))
                    .unwrap_or_else(|| "System default input".to_owned())
            });
        ui.horizontal(|ui| {
            ui.label("Input device");
            egui::ComboBox::from_id_salt("audio_input_device")
                .selected_text(input_name)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.audio_settings.input_device_id,
                        None,
                        "System default input",
                    );
                    for device in &self.audio_catalog.inputs {
                        ui.selectable_value(
                            &mut self.audio_settings.input_device_id,
                            Some(device.id.clone()),
                            &device.name,
                        );
                    }
                });
        });

        let output_name = self
            .audio_settings
            .output_device_id
            .as_ref()
            .and_then(|id| {
                self.audio_catalog
                    .outputs
                    .iter()
                    .find(|device| &device.id == id)
            })
            .map(|device| device.name.clone())
            .unwrap_or_else(|| {
                self.audio_catalog
                    .default_output_id
                    .as_ref()
                    .and_then(|id| {
                        self.audio_catalog
                            .outputs
                            .iter()
                            .find(|device| &device.id == id)
                    })
                    .map(|device| format!("System default · {}", device.name))
                    .unwrap_or_else(|| "System default output".to_owned())
            });
        ui.horizontal(|ui| {
            ui.label("Output device");
            egui::ComboBox::from_id_salt("audio_output_device")
                .selected_text(output_name)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.audio_settings.output_device_id,
                        None,
                        "System default output",
                    );
                    for device in &self.audio_catalog.outputs {
                        ui.selectable_value(
                            &mut self.audio_settings.output_device_id,
                            Some(device.id.clone()),
                            &device.name,
                        );
                    }
                });
        });

        ui.horizontal(|ui| {
            ui.label("Windows audio API");
            egui::ComboBox::from_id_salt("audio_access_mode")
                .selected_text(if self.audio_settings.access == AudioAccess::Exclusive {
                    "WASAPI · Exclusive"
                } else if cfg!(target_os = "windows") {
                    "WASAPI · Shared"
                } else {
                    "System audio · Shared"
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.audio_settings.access,
                        AudioAccess::Shared,
                        if cfg!(target_os = "windows") {
                            "WASAPI · Shared"
                        } else {
                            "System audio · Shared"
                        },
                    );
                    ui.add_enabled_ui(cfg!(target_os = "windows"), |ui| {
                        ui.selectable_value(
                            &mut self.audio_settings.access,
                            AudioAccess::Exclusive,
                            "WASAPI · Exclusive",
                        );
                    });
                });
            if !cfg!(target_os = "windows") {
                ui.label(
                    egui::RichText::new("Exclusive device access is Windows-only").color(MUTED),
                );
            }
        });

        ui.horizontal(|ui| {
            ui.label("Sample rate");
            egui::ComboBox::from_id_salt("audio_sample_rate")
                .selected_text(format!("{} Hz", self.audio_settings.sample_rate))
                .show_ui(ui, |ui| {
                    for rate in [44_100, 48_000, 88_200, 96_000, 176_400, 192_000] {
                        ui.selectable_value(
                            &mut self.audio_settings.sample_rate,
                            rate,
                            format!("{rate} Hz"),
                        );
                    }
                });
            ui.label("Buffer");
            egui::ComboBox::from_id_salt("audio_buffer_frames")
                .selected_text(format!("{} frames", self.audio_settings.buffer_frames))
                .show_ui(ui, |ui| {
                    for frames in [64, 128, 256, 512, 1_024, 2_048] {
                        ui.selectable_value(
                            &mut self.audio_settings.buffer_frames,
                            frames,
                            format!("{frames} frames"),
                        );
                    }
                });
        });
        ui.add_space(8.0);

        if self.audio_engine.is_some() {
            if ui.button("Stop audio engine").clicked() {
                stop_requested = true;
            }
        } else if ui.button("Start audio engine").clicked() {
            start_requested = true;
        }

        if let Some(engine) = &self.audio_engine {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        engine.output_active(),
                        egui::Button::new(if self.audio_test_tone {
                            "Stop output check"
                        } else {
                            "Play 440 Hz output check"
                        }),
                    )
                    .clicked()
                {
                    self.audio_test_tone = !self.audio_test_tone;
                    if self.audio_test_tone {
                        self.playing = false;
                        self.audio_monitor_input = false;
                        let _ = engine.set_input_monitor(false);
                    }
                    engine.set_test_tone(self.audio_test_tone);
                }
                if ui
                    .add_enabled(
                        engine.input_active() && engine.output_active(),
                        egui::Button::new(if self.audio_monitor_input {
                            "Stop input monitor"
                        } else {
                            "Monitor input"
                        }),
                    )
                    .clicked()
                {
                    self.audio_monitor_input = !self.audio_monitor_input;
                    if self.audio_monitor_input {
                        self.playing = false;
                        self.audio_test_tone = false;
                        engine.set_test_tone(false);
                    }
                    if let Err(error) = engine.set_input_monitor(self.audio_monitor_input) {
                        self.status = error;
                    }
                }
            });

            if engine.input_active() {
                let peak = engine.input_peak();
                ui.horizontal(|ui| {
                    ui.label("Input level");
                    ui.add(egui::ProgressBar::new(peak).desired_width(260.0));
                    ui.monospace(format!("{:.1} dBFS", engine.input_level_db()));
                });
            } else {
                ui.label(egui::RichText::new("No input stream is active").color(MUTED));
            }
            ui.small("The output check is a quiet 440 Hz tone. Stop it before leaving this page.");
            if let Some(error) = engine.take_error() {
                self.status = format!("Audio device error: {error}");
            }
        }

        if let Some(error) = &self.audio_catalog.error {
            ui.label(egui::RichText::new(format!("Device list: {error}")).color(ORANGE));
        }

        if self.audio_engine.is_some() {
            ui.label(
                egui::RichText::new(
                    "Changes to device, API, rate, and buffer settings apply after restarting the audio engine.",
                )
                .color(MUTED),
            );
        }

        if stop_requested {
            self.stop_project_playback();
            self.audio_engine = None;
            self.audio_test_tone = false;
            self.audio_monitor_input = false;
            self.status = "Audio engine stopped".to_owned();
        } else if start_requested {
            match AudioEngine::start(&self.audio_settings) {
                Ok(engine) => {
                    let mode = self.audio_settings.access.label();
                    self.audio_engine = Some(engine);
                    self.audio_test_tone = false;
                    self.audio_monitor_input = false;
                    self.status = if cfg!(target_os = "windows") {
                        format!("WASAPI {mode} audio engine started")
                    } else {
                        format!(
                            "Shared audio engine started at {} Hz",
                            self.audio_settings.sample_rate
                        )
                    };
                }
                Err(error) => self.status = format!("Could not start audio engine: {error}"),
            }
        }
    }

    fn plugins(&mut self, ui: &mut egui::Ui) {
        let mut refresh = false;
        let mut load_path = None;
        let mut restore_request = None;
        let mut open_id = None;
        let mut close_id = None;
        let mut parameter_edits = Vec::new();

        ui.horizontal(|ui| {
            ui.strong("Installed VST3 plug-ins");
            ui.separator();
            if ui.button("Refresh scan").clicked() {
                refresh = true;
            }
        });
        ui.label(
            egui::RichText::new(
                "Matching installed VST3 channels load with the project. Open a plug-in's native editor when you need it.",
            )
            .color(MUTED),
        );
        ui.separator();

        if refresh {
            self.plugin_candidates = scan_installed_plugins().candidates;
        }

        let loaded = self
            .vst3_host
            .as_ref()
            .map(Vst3HostRuntime::loaded_plugins)
            .unwrap_or_default();
        let plugin_states = self
            .document
            .as_ref()
            .map(FlpDocument::channel_plugin_states)
            .unwrap_or_default();
        if self.selected_plugin_state_channel.is_none_or(|selected| {
            !plugin_states
                .iter()
                .any(|state| state.channel_id() == selected)
        }) {
            self.selected_plugin_state_channel =
                plugin_states.first().map(|state| state.channel_id());
        }
        if !plugin_states.is_empty() {
            let selected_label = self
                .selected_plugin_state_channel
                .and_then(|channel_id| {
                    plugin_states
                        .iter()
                        .find(|state| state.channel_id() == channel_id)
                })
                .map(|state| {
                    format!(
                        "Channel {} · {} · {} bytes",
                        state.channel_id(),
                        state.display_name().unwrap_or("unnamed plug-in"),
                        state.data_payload().len()
                    )
                })
                .unwrap_or_else(|| "Select a channel state".to_owned());
            let mut selected_channel = self.selected_plugin_state_channel;
            ui.horizontal(|ui| {
                ui.label("Project state channel");
                egui::ComboBox::from_id_salt("project-plugin-state-channel")
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        for state in &plugin_states {
                            let label = format!(
                                "{} · {} · {} bytes",
                                state.channel_id(),
                                state.display_name().unwrap_or("unnamed plug-in"),
                                state.data_payload().len()
                            );
                            ui.selectable_value(
                                &mut selected_channel,
                                Some(state.channel_id()),
                                label,
                            );
                        }
                    });
            });
            self.selected_plugin_state_channel = selected_channel;
        } else {
            ui.label(
                egui::RichText::new(
                    "Open a project with a channel plug-in state to try restoration",
                )
                .color(MUTED),
            );
        }
        let selected_state = self.selected_plugin_state_channel.and_then(|channel_id| {
            plugin_states
                .iter()
                .find(|state| state.channel_id() == channel_id)
        });
        if let Some(state) = selected_state {
            if let Some(metadata) = state.vst_metadata() {
                let plugin_name = metadata
                    .name()
                    .or(state.display_name())
                    .unwrap_or("unknown VST");
                ui.label(format!(
                    "Project VST: {} · {} · class {}",
                    plugin_name,
                    metadata.vendor().unwrap_or("unknown vendor"),
                    metadata.class_uid().as_deref().unwrap_or("unknown")
                ));
                let mapped_instance = self
                    .channel_vst3_instances
                    .get(&state.channel_id())
                    .copied();
                if mapped_instance.is_some() {
                    ui.label(
                        egui::RichText::new("Loaded and mapped to this channel's playback")
                            .color(GREEN),
                    );
                }
                if let Some(candidate) = matching_vst3_candidate(&self.plugin_candidates, metadata)
                {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Installed match: {} · {}",
                            candidate.name,
                            candidate.path.display()
                        ));
                        if let Some(instance_id) = mapped_instance
                            && let Some(info) = loaded.iter().find(|info| info.id == instance_id)
                            && info.has_editor
                            && ui.button("Open editor").clicked()
                        {
                            open_id = Some(info.id);
                        }
                    });
                } else {
                    ui.label(
                        egui::RichText::new("No installed VST3 bundle matches this project state")
                            .color(ORANGE),
                    );
                }
            } else {
                ui.label(
                    egui::RichText::new(
                        "This channel has opaque FL plug-in data without a recognized VST identity.",
                    )
                    .color(MUTED),
                );
            }
        }
        ui.label(
            egui::RichText::new(
                "Use the installed list below to load VST3 instances or try restoring another channel state.",
            )
            .color(MUTED),
        );
        if let Some(message) = &self.last_plugin_action {
            let color = if message.to_lowercase().contains("could not") {
                ORANGE
            } else {
                MUTED
            };
            ui.label(egui::RichText::new(message).color(color));
        }
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            for candidate in &self.plugin_candidates {
                if candidate.format != PluginFormat::Vst3 {
                    continue;
                }
                let project_match = selected_state
                    .and_then(|state| state.vst_metadata())
                    .is_some_and(|metadata| candidate_matches_vst_metadata(candidate, metadata));
                let candidate_loaded = loaded
                    .iter()
                    .filter(|info| info.path == candidate.path)
                    .collect::<Vec<_>>();
                let mapped_instance = self
                    .selected_plugin_state_channel
                    .and_then(|channel_id| self.channel_vst3_instances.get(&channel_id).copied())
                    .filter(|instance_id| {
                        candidate_loaded.iter().any(|info| info.id == *instance_id)
                    });
                egui::Frame::new()
                    .fill(PANEL_DARK)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.strong(&candidate.name);
                                ui.small(candidate.path.display().to_string());
                                if project_match {
                                    ui.small(egui::RichText::new("Project match").color(GREEN));
                                }
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if let Some(instance_id) = mapped_instance {
                                        if ui.button("Open mapped editor").clicked() {
                                            open_id = Some(instance_id);
                                        }
                                    } else if let Some(info) = candidate_loaded.first() {
                                        if info.has_editor {
                                            if ui.button("Open editor").clicked() {
                                                open_id = Some(info.id);
                                            }
                                        } else {
                                            ui.small("Loaded without an editor");
                                        }
                                    } else if ui.button("Load and open editor").clicked() {
                                        load_path = Some(candidate.path.clone());
                                    }
                                    if let Some(channel_id) = self.selected_plugin_state_channel
                                        && !self.channel_vst3_instances.contains_key(&channel_id)
                                        && project_match
                                        && ui.button("Load + try FLP state").clicked()
                                    {
                                        let class_uid = selected_state
                                            .filter(|_| project_match)
                                            .and_then(|state| state.vst_metadata())
                                            .and_then(VstPluginStateMetadata::class_uid);
                                        restore_request =
                                            Some((candidate.path.clone(), channel_id, class_uid));
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(3.0);
            }
        });

        if let Some((path, channel_id, class_uid)) = restore_request {
            self.load_installed_vst3(&path, Some(channel_id), class_uid);
        } else if let Some(path) = load_path {
            self.load_installed_vst3(&path, None, None);
        }

        if !loaded.is_empty() {
            ui.separator();
            ui.heading("Loaded plug-in instances");
            for info in loaded {
                ui.horizontal(|ui| {
                    ui.strong(format!("{} · {}", info.name, info.vendor));
                    ui.small(format!("VST3 · {}", info.version));
                    if info.has_editor && ui.button("Open editor").clicked() {
                        open_id = Some(info.id);
                    }
                    if ui.button("Close editor").clicked() {
                        close_id = Some(info.id);
                    }
                });
                ui.small(format!("{} · class {}", info.path.display(), info.uid));

                egui::CollapsingHeader::new("Parameters").show(ui, |ui| {
                    let parameters = self
                        .vst3_host
                        .as_ref()
                        .and_then(|host| host.parameter_snapshot(info.id).ok())
                        .unwrap_or_default();
                    for parameter in parameters {
                        let mut value = parameter.value.clamp(0.0, 1.0) as f32;
                        let label = if parameter.unit.is_empty() {
                            parameter.name.clone()
                        } else {
                            format!("{} ({})", parameter.name, parameter.unit)
                        };
                        if ui
                            .add_enabled(
                                !parameter.is_read_only,
                                egui::Slider::new(&mut value, 0.0..=1.0).text(label),
                            )
                            .changed()
                        {
                            parameter_edits.push((info.id, parameter.id, f64::from(value)));
                        }
                    }
                });
                ui.separator();
            }
        }

        if let Some(id) = open_id
            && let Some(host) = &mut self.vst3_host
        {
            match host.open_editor(id) {
                Ok(()) => {
                    self.status = "VST3 editor opened".to_owned();
                    self.last_plugin_action = Some(self.status.clone());
                }
                Err(error) => {
                    self.status = format!("Could not open VST3 editor: {error}");
                    self.last_plugin_action = Some(self.status.clone());
                }
            }
        }
        if let Some(id) = close_id
            && let Some(host) = &mut self.vst3_host
        {
            match host.close_editor(id) {
                Ok(()) => {
                    self.status = "VST3 editor closed".to_owned();
                    self.last_plugin_action = Some(self.status.clone());
                }
                Err(error) => {
                    self.status = format!("Could not close VST3 editor: {error}");
                    self.last_plugin_action = Some(self.status.clone());
                }
            }
        }
        if let Some(host) = &self.vst3_host {
            for (id, parameter_id, value) in parameter_edits {
                if let Err(error) = host.set_parameter(id, parameter_id, value) {
                    self.status = format!("Could not update VST3 parameter: {error}");
                }
            }
        }
    }

    fn load_installed_vst3(
        &mut self,
        path: &Path,
        restore_channel_id: Option<u16>,
        class_uid: Option<String>,
    ) {
        let project_plugin_state = restore_channel_id.and_then(|channel_id| {
            self.document
                .as_ref()?
                .channel_plugin_states()
                .into_iter()
                .find(|state| state.channel_id() == channel_id)
        });
        if restore_channel_id.is_some() && project_plugin_state.is_none() {
            self.status = "Selected channel has no FLP plug-in state".to_owned();
            self.last_plugin_action = Some(self.status.clone());
            return;
        }
        self.backup_before_risky_operation("loading a VST3 plug-in");
        if self.vst3_host.is_none() {
            match Vst3HostRuntime::new(f64::from(self.audio_settings.sample_rate), 512) {
                Ok(host) => self.vst3_host = Some(host),
                Err(error) => {
                    self.status = format!("Could not initialize the VST3 host: {error}");
                    self.last_plugin_action = Some(self.status.clone());
                    return;
                }
            }
        }

        let Some(host) = &mut self.vst3_host else {
            return;
        };
        let mut loaded_channel_instance = None;
        match host.load(path, class_uid.as_deref()) {
            Ok(info) => {
                if let Some(channel_id) = restore_channel_id {
                    loaded_channel_instance = Some((channel_id, info.id));
                }
                let state_message = if let (Some(channel_id), Some(project_state)) =
                    (restore_channel_id, project_plugin_state.as_ref())
                {
                    match host.restore_flp_channel_state(info.id, project_state) {
                        Ok(()) => {
                            format!("; restored FLP VST state from channel {channel_id}")
                        }
                        Err(error) => format!(
                            "; could not restore FLP VST state from channel {channel_id}: {error}"
                        ),
                    }
                } else {
                    String::new()
                };
                match host.open_editor(info.id) {
                    Ok(()) => self.status = format!("Loaded {}{state_message}", info.name),
                    Err(error) => {
                        self.status = format!(
                            "Loaded {}{state_message}, but its editor did not open: {error}",
                            info.name
                        );
                    }
                }
            }
            Err(error) => self.status = format!("Could not load VST3: {error}"),
        }
        if let Some((channel_id, instance_id)) = loaded_channel_instance {
            self.channel_vst3_instances.insert(channel_id, instance_id);
        }
        self.last_plugin_action = Some(self.status.clone());
    }
}

impl eframe::App for DawUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.guard_window_close(ui.ctx());
        let recent_project_index = ui.input_mut(|input| {
            if !input.modifiers.alt
                || input.modifiers.ctrl
                || input.modifiers.command
                || input.modifiers.shift
            {
                return None;
            }
            [
                (egui::Key::Num1, 0),
                (egui::Key::Num2, 1),
                (egui::Key::Num3, 2),
                (egui::Key::Num4, 3),
                (egui::Key::Num5, 4),
                (egui::Key::Num6, 5),
                (egui::Key::Num7, 6),
                (egui::Key::Num8, 7),
                (egui::Key::Num9, 8),
                (egui::Key::Num0, 9),
            ]
            .into_iter()
            .find_map(|(key, index)| {
                input
                    .consume_key(egui::Modifiers::ALT, key)
                    .then_some(index)
            })
        });
        if let Some(index) = recent_project_index
            && let Some(path) = self.browser_recent_projects.get(index).cloned()
        {
            self.open_project(&path);
        }
        let save_as_requested = ui.input_mut(|input| {
            input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::S,
            )
        });
        let save_version_requested =
            ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::N));
        let save_requested = !save_as_requested
            && !save_version_requested
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        if save_as_requested {
            self.save_as();
        } else if save_version_requested {
            self.save_new_version();
        } else if save_requested {
            self.save();
        }
        self.advance_autosave(ui.ctx());
        let redo_requested = ui.input_mut(|input| {
            input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            ) || input.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)
        });
        let undo_requested = !redo_requested
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z));
        if undo_requested {
            self.undo_document();
        } else if redo_requested {
            self.redo_document();
        }
        let history_navigation = undo_requested || redo_requested;
        let pointer_down = ui.input(|input| {
            input.pointer.primary_down() || input.pointer.button_down(PointerButton::Secondary)
        });
        let edit_input_received = ui.input(|input| {
            input.pointer.primary_pressed()
                || input.pointer.primary_down()
                || input.pointer.primary_released()
                || input.pointer.button_pressed(PointerButton::Secondary)
                || input.pointer.button_down(PointerButton::Secondary)
                || input.pointer.button_released(PointerButton::Secondary)
                || input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key { pressed: true, .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                    )
                })
        });
        let mut frame_snapshot = if !history_navigation
            && self.pending_history_snapshot.is_none()
            && edit_input_received
        {
            self.document
                .as_ref()
                .and_then(|document| document.encode_lossless().ok())
        } else {
            None
        };
        self.poll_project_audio_render();
        self.poll_song_render();
        self.poll_vst3_stream();
        self.poll_sampler_stream();
        self.poll_browser_preview(ui.ctx());
        self.reap_audio_render_workers();
        self.reap_song_render_workers();
        self.reap_vst3_workers();
        self.reap_sampler_workers();
        for (key, view) in [
            (egui::Key::F5, MainView::Playlist),
            (egui::Key::F6, MainView::ChannelRack),
            (egui::Key::F7, MainView::PianoRoll),
            (egui::Key::F9, MainView::Mixer),
        ] {
            if ui.input(|input| input.key_pressed(key)) {
                self.view = view;
            }
        }
        if self.view == MainView::ChannelRack
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::K))
        {
            self.step_graph_editor_open = !self.step_graph_editor_open;
        }
        if let Some(host) = &mut self.vst3_host
            && let Err(error) = host.service_editors()
        {
            self.status = format!("VST3 editor update failed: {error}");
        }
        if let Some(error) = self.audio_engine.as_ref().and_then(AudioEngine::take_error) {
            self.status = format!("Audio device error: {error}");
        }
        if self.playing
            && self
                .audio_engine
                .as_ref()
                .is_some_and(|engine| !engine.project_playback_active())
        {
            self.playing = false;
            self.project_playback_loaded = false;
            self.playlist_playback_loaded = false;
            self.status = "Project playback reached the end".to_owned();
        }
        if self.audio_engine.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }
        let width = ui.available_width();
        let full_height = ui.available_height();
        ui.vertical(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(width, 29.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| self.top_menu(ui),
            );
            ui.allocate_ui_with_layout(
                Vec2::new(width, 43.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| self.transport_bar(ui),
            );
            ui.separator();
            let content_height = (full_height - 29.0 - 43.0 - 23.0 - 8.0).max(100.0);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(218.0, content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.browser(ui),
                );
                ui.separator();
                let content_width = ui.available_width().max(100.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(content_width, content_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        self.view_tabs(ui);
                        match self.view {
                            MainView::Playlist => self.playlist(ui),
                            MainView::ChannelRack => self.channel_rack(ui),
                            MainView::PianoRoll => self.piano_roll(ui),
                            MainView::Mixer => self.mixer(ui),
                            MainView::Plugins => self.plugins(ui),
                            MainView::Audio => self.audio_settings_view(ui),
                            MainView::Automation => self.automation_editor(ui),
                        }
                    },
                );
            });
            ui.separator();
            ui.allocate_ui_with_layout(
                Vec2::new(width, 23.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.label(if self.dirty { "●" } else { "" });
                        ui.label(&self.status);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label("FLP compatibility foundation");
                        });
                    });
                },
            );
        });
        self.project_info_dialog(ui.ctx());
        self.project_settings_dialog(ui.ctx());
        self.browser_tag_editor_dialog(ui.ctx());
        self.browser_search_save_dialog(ui.ctx());
        self.browser_tab_customize_dialog(ui.ctx());
        self.browser_fst_details_dialog(ui.ctx());
        self.recovery_prompt_dialog(ui.ctx());
        self.unsaved_changes_dialog(ui.ctx());
        self.finish_history_frame(frame_snapshot.take(), pointer_down, history_navigation);
    }
}

impl Drop for DawUi {
    fn drop(&mut self) {
        self.stop_project_playback();
        for worker in self.audio_render_workers.drain(..) {
            let _ = worker.worker.join();
        }
        for render in self.song_render_workers.drain(..) {
            let _ = render.worker.join();
        }
        if let Some(stream) = self.pending_vst3_stream.take() {
            self.vst3_workers.push(stream);
        }
        for worker in self.vst3_workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(backup) = self.pending_backup_write.take() {
            let _ = backup.worker.join();
        }
    }
}

fn empty_view(ui: &mut egui::Ui, message: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(egui::RichText::new(message).color(MUTED));
    });
}

fn layer_child_selection_label(child_ids: &[u16], channels: &[ChannelSummary]) -> String {
    if child_ids.is_empty() {
        return "No child channels".to_owned();
    }
    child_ids
        .iter()
        .map(|child_id| {
            channels
                .iter()
                .find(|channel| channel.id() == *child_id)
                .map(|channel| {
                    format!(
                        "{} (ID {child_id})",
                        channel.display_name().unwrap_or("(unnamed channel)")
                    )
                })
                .unwrap_or_else(|| format!("Missing channel (ID {child_id})"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn update_layer_child_selection(child_ids: &mut Vec<u16>, child_id: u16, selected: bool) {
    if selected {
        if !child_ids.contains(&child_id) {
            child_ids.push(child_id);
        }
    } else {
        child_ids.retain(|id| *id != child_id);
    }
}

fn automation_point_screen_position(
    point: &AutomationPoint,
    plot_rect: egui::Rect,
    visible_beats: f64,
) -> egui::Pos2 {
    let beat_fraction = if point.position_beats().is_finite() {
        (point.position_beats() / visible_beats).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let value_fraction = if point.value().is_finite() {
        point.value().clamp(0.0, 1.0) as f32
    } else {
        0.5
    };
    egui::pos2(
        egui::lerp(plot_rect.left()..=plot_rect.right(), beat_fraction),
        egui::lerp(plot_rect.bottom()..=plot_rect.top(), value_fraction),
    )
}

fn index_browser_folders(roots: Vec<PathBuf>, all_roots: bool) -> Result<BrowserIndex, String> {
    let mut entries = Vec::new();
    let mut seen_paths = BTreeSet::new();
    let mut scan_entries = 0usize;
    let mut truncated = false;
    let mut unavailable_roots = Vec::new();

    'roots: for root in &roots {
        let mut directories = VecDeque::from([root.clone()]);
        'tree: while let Some(directory) = directories.pop_front() {
            let children = match fs::read_dir(&directory) {
                Ok(children) => children,
                Err(error) if directory == *root => {
                    if all_roots {
                        unavailable_roots.push(root.clone());
                        break 'tree;
                    }
                    return Err(format!("Could not search {}: {error}", root.display()));
                }
                Err(_) => continue,
            };
            for child in children {
                let Ok(child) = child else {
                    continue;
                };
                let Ok(file_type) = child.file_type() else {
                    continue;
                };
                let path = child.path();
                if scan_entries >= MAX_BROWSER_RECURSIVE_SCAN_ENTRIES {
                    truncated = true;
                    break 'roots;
                }
                scan_entries += 1;
                if file_type.is_dir() {
                    directories.push_back(path);
                } else if file_type.is_file()
                    && browser_file_kind(&path).is_some()
                    && seen_paths.insert(path.clone())
                {
                    let relative_path = path.strip_prefix(root).unwrap_or(&path);
                    let name = if all_roots {
                        let root_name = root
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| root.display().to_string());
                        format!("{root_name}/{}", relative_path.display())
                    } else {
                        relative_path.to_string_lossy().into_owned()
                    };
                    entries.push(BrowserEntry {
                        name,
                        path,
                        is_directory: false,
                    });
                }
            }
        }
    }

    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(BrowserIndex {
        roots,
        all_roots,
        entries,
        truncated,
        unavailable_roots,
    })
}

fn default_browser_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn browser_favorites_file() -> Option<PathBuf> {
    let root = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Roaming"))
        })
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".config"))
            })
    }?;
    Some(root.join("fl-studio-rebuild").join("browser-favorites.txt"))
}

fn browser_search_roots_file() -> Option<PathBuf> {
    browser_favorites_file()?
        .parent()
        .map(|directory| directory.join("browser-search-roots.txt"))
}

fn browser_tags_file() -> Option<PathBuf> {
    browser_favorites_file()?
        .parent()
        .map(|directory| directory.join("browser-tags.txt"))
}

fn browser_saved_searches_file() -> Option<PathBuf> {
    browser_favorites_file()?
        .parent()
        .map(|directory| directory.join("browser-saved-searches.txt"))
}

fn recent_projects_file() -> Option<PathBuf> {
    browser_favorites_file()?
        .parent()
        .map(|directory| directory.join("recent-projects.txt"))
}

fn load_recent_projects() -> Vec<PathBuf> {
    let Some(path) = recent_projects_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut recent = Vec::new();
    for entry in contents.lines().filter(|entry| !entry.is_empty()) {
        let path = PathBuf::from(entry);
        let path = fs::canonicalize(&path).unwrap_or(path);
        if !recent.contains(&path) {
            recent.push(path);
        }
        if recent.len() >= RECENT_PROJECT_LIMIT {
            break;
        }
    }
    recent
}

fn save_recent_projects(projects: &[PathBuf]) -> Result<(), String> {
    let path = recent_projects_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the recent projects path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let contents = projects
        .iter()
        .take(RECENT_PROJECT_LIMIT)
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn autosave_settings_file() -> Option<PathBuf> {
    browser_favorites_file()?
        .parent()
        .map(|directory| directory.join("autosave-settings.txt"))
}

fn load_autosave_settings() -> Option<(u8, bool, usize)> {
    let contents = fs::read_to_string(autosave_settings_file()?).ok()?;
    let mut lines = contents.lines();
    let autosave_minutes = lines.next()?.parse::<u8>().ok()?;
    let backup_retention = lines.next()?.parse::<usize>().ok()?;
    let autosave_before_risky = lines
        .next()
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false);
    if !AUTOSAVE_INTERVALS_MINUTES.contains(&autosave_minutes)
        || !BACKUP_RETENTION_OPTIONS.contains(&backup_retention)
    {
        return None;
    }
    Some((autosave_minutes, autosave_before_risky, backup_retention))
}

fn save_autosave_settings(
    autosave_minutes: u8,
    autosave_before_risky: bool,
    backup_retention: usize,
) -> Result<(), String> {
    let path = autosave_settings_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the autosave settings path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    fs::write(
        &path,
        format!("{autosave_minutes}\n{backup_retention}\n{autosave_before_risky}\n"),
    )
    .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn user_data_directory() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Roaming"))
        })
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local").join("share"))
            })
    }
    .map(|root| root.join("fl-studio-rebuild"))
}

fn project_backup_directory(project_path: &Path) -> Option<PathBuf> {
    Some(
        user_data_directory()?
            .join("Backups")
            .join(stable_project_id(project_path)),
    )
}

fn stable_project_id(project_path: &Path) -> String {
    let normalized = fs::canonicalize(project_path).unwrap_or_else(|_| {
        let absolute = normalized_absolute_path(project_path);
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => fs::canonicalize(parent)
                .unwrap_or_else(|_| normalized_absolute_path(parent))
                .join(name),
            _ => absolute,
        }
    });
    let mut value = normalized.to_string_lossy().into_owned();
    if cfg!(target_os = "windows") {
        value.make_ascii_lowercase();
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn write_project_backup(
    project_path: &Path,
    bytes: &[u8],
    purpose: BackupPurpose,
    retention: usize,
) -> Result<PathBuf, String> {
    let directory = project_backup_directory(project_path)
        .ok_or_else(|| "the user data folder is not available".to_owned())?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let recorded_path =
        fs::canonicalize(project_path).unwrap_or_else(|_| normalized_absolute_path(project_path));
    fs::write(
        directory.join("project.path"),
        recorded_path.to_string_lossy().as_bytes(),
    )
    .map_err(|error| format!("could not record project path: {error}"))?;

    let kind = match purpose {
        BackupPurpose::Autosave => "autosave",
        BackupPurpose::Manual => "backup",
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let extension = if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        "zip"
    } else {
        "flp"
    };
    let path = directory.join(format!(
        "{kind}-{timestamp}-{}.{extension}",
        std::process::id()
    ));
    write_file_atomically(&path, bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    prune_global_project_backups(&directory, retention);
    Ok(path)
}

fn write_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_file_name(format!(".{name}.tmp-{}-{timestamp}", std::process::id()));
    fs::write(&temporary, bytes)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temporary);
            Err(error)
        }
    }
}

fn prune_global_project_backups(project_directory: &Path, retention: usize) {
    let Some(root) = project_directory.parent() else {
        return;
    };
    let mut backups = fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_dir(entry.path()).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("flp") || extension.eq_ignore_ascii_case("zip")
            }) && path.file_name().is_some_and(|name| {
                name.to_string_lossy().starts_with("autosave-")
                    || name.to_string_lossy().starts_with("backup-")
            })
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    let remove_count = backups.len().saturating_sub(retention);
    for path in backups.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

fn project_autosave_files(project_path: &Path) -> Vec<PathBuf> {
    let Some(directory) = project_backup_directory(project_path) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("flp"))
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("autosave-"))
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH),
        )
    });
    paths
}

fn latest_project_autosave(project_path: &Path) -> Option<PathBuf> {
    project_autosave_files(project_path)
        .into_iter()
        .find(|path| {
            fs::read(path)
                .ok()
                .is_some_and(|bytes| FlpDocument::parse(&bytes).is_ok())
        })
}

fn recovery_prompt_for_project(project_path: &Path) -> Option<RecoveryPrompt> {
    let directory = project_backup_directory(project_path)?;
    let recorded_path = fs::read_to_string(directory.join("project.path")).ok()?;
    if !project_paths_equal(Path::new(recorded_path.trim()), project_path) {
        return None;
    }
    let backup_path = latest_project_autosave(project_path)?;
    let backup_modified = fs::metadata(&backup_path).ok()?.modified().ok()?;
    let source_modified = fs::metadata(project_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    if source_modified.is_some_and(|modified| backup_modified < modified) {
        return None;
    }
    if fs::read(project_path).ok().is_some_and(|source_bytes| {
        fs::read(&backup_path).is_ok_and(|backup| backup == source_bytes)
    }) {
        return None;
    }
    Some(RecoveryPrompt {
        source_path: project_path.to_path_buf(),
        backup_path,
        is_revert: false,
    })
}

fn latest_recovery_prompt() -> Option<RecoveryPrompt> {
    let root = user_data_directory()?.join("Backups");
    let directories = fs::read_dir(root).ok()?;
    directories
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let directory = entry.path();
            let source_path = fs::read_to_string(directory.join("project.path")).ok()?;
            recovery_prompt_for_project(Path::new(source_path.trim()))
        })
        .max_by_key(|prompt| {
            fs::metadata(&prompt.backup_path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH)
        })
}

fn project_paths_equal(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| absolute_path(left));
    let right = fs::canonicalize(right).unwrap_or_else(|_| absolute_path(right));
    if cfg!(target_os = "windows") {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(extension))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn normalized_absolute_path(path: &Path) -> PathBuf {
    let absolute = absolute_path(path);
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn next_project_version_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "project".to_owned());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| "flp".to_owned());
    let (base, mut version) = stem
        .rsplit_once('_')
        .and_then(|(base, suffix)| {
            suffix
                .parse::<u32>()
                .ok()
                .filter(|version| *version >= 2)
                .map(|version| (base.to_owned(), version + 1))
        })
        .unwrap_or((stem, 2));
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    loop {
        let candidate = directory.join(format!("{base}_{version}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
        version = version.saturating_add(1);
    }
}

fn load_browser_favorites() -> BTreeSet<PathBuf> {
    let Some(path) = browser_favorites_file() else {
        return BTreeSet::new();
    };
    fs::read_to_string(path)
        .map(|contents| {
            contents
                .lines()
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

fn load_browser_search_roots() -> Vec<PathBuf> {
    let Some(path) = browser_search_roots_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in contents.lines().filter(|entry| !entry.is_empty()) {
        let path = PathBuf::from(entry);
        let path = fs::canonicalize(&path).unwrap_or(path);
        if !roots.contains(&path) {
            roots.push(path);
        }
        if roots.len() >= BROWSER_SEARCH_ROOT_LIMIT {
            break;
        }
    }
    roots
}

fn save_browser_search_roots(roots: &[PathBuf]) -> Result<(), String> {
    let path = browser_search_roots_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the Browser search folders path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let contents = roots
        .iter()
        .take(BROWSER_SEARCH_ROOT_LIMIT)
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn save_browser_favorites(favorites: &BTreeSet<PathBuf>) -> Result<(), String> {
    let path = browser_favorites_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the user configuration path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let contents = favorites
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn load_browser_tags() -> BTreeMap<PathBuf, BTreeSet<String>> {
    let Some(path) = browser_tags_file() else {
        return BTreeMap::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut tags_by_path = BTreeMap::new();
    for line in contents.lines() {
        let mut fields = line.split('\t');
        let Some(path) = fields.next().filter(|path| !path.is_empty()) else {
            continue;
        };
        let tags = fields
            .flat_map(|field| field.split(','))
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if !tags.is_empty() {
            tags_by_path.insert(PathBuf::from(path), tags);
        }
    }
    tags_by_path
}

fn save_browser_tags(tags_by_path: &BTreeMap<PathBuf, BTreeSet<String>>) -> Result<(), String> {
    let path = browser_tags_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the Browser tags path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let contents = tags_by_path
        .iter()
        .filter(|(_, tags)| !tags.is_empty())
        .map(|(path, tags)| {
            let mut line = path.to_string_lossy().into_owned();
            for tag in tags {
                line.push('\t');
                line.push_str(&tag.replace(['\t', '\n', '\r'], " "));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn browser_search_filter_key(filter: BrowserFilter) -> &'static str {
    match filter {
        BrowserFilter::All => "all",
        BrowserFilter::Audio => "audio",
        BrowserFilter::Projects => "projects",
        BrowserFilter::Presets => "presets",
    }
}

fn parse_browser_search_filter(value: &str) -> Option<BrowserFilter> {
    match value {
        "all" => Some(BrowserFilter::All),
        "audio" => Some(BrowserFilter::Audio),
        "projects" => Some(BrowserFilter::Projects),
        "presets" => Some(BrowserFilter::Presets),
        _ => None,
    }
}

fn browser_tag_logic_key(logic: BrowserTagLogic) -> &'static str {
    match logic {
        BrowserTagLogic::Any => "any",
        BrowserTagLogic::All => "all",
    }
}

fn parse_browser_tag_logic(value: &str) -> Option<BrowserTagLogic> {
    match value {
        "any" => Some(BrowserTagLogic::Any),
        "all" => Some(BrowserTagLogic::All),
        _ => None,
    }
}

fn encode_browser_search_field(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            '\t' => encoded.push_str("%09"),
            '\n' => encoded.push_str("%0A"),
            '\r' => encoded.push_str("%0D"),
            _ => encoded.push(character),
        }
    }
    encoded
}

fn decode_browser_search_field(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push(((high << 4) | low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn load_browser_saved_searches() -> Vec<SavedBrowserSearch> {
    let Some(path) = browser_saved_searches_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut searches = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split('\t');
        let version = fields.next();
        let (Some(name), Some(query), Some(filter), Some(all_roots), Some(recursive), Some(path)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let (tag_logic, hidden, color, icon, selected_tags) = match version {
            Some("v1") if fields.next().is_none() => (
                BrowserTagLogic::Any,
                false,
                BrowserTabColor::default(),
                BrowserTabIcon::default(),
                BTreeSet::new(),
            ),
            Some("v2") => {
                let Some(tag_logic) = fields.next().and_then(parse_browser_tag_logic) else {
                    continue;
                };
                let selected_tags = fields
                    .map(decode_browser_search_field)
                    .map(|tag| tag.trim().to_owned())
                    .filter(|tag| !tag.is_empty())
                    .collect::<BTreeSet<_>>();
                (
                    tag_logic,
                    false,
                    BrowserTabColor::default(),
                    BrowserTabIcon::default(),
                    selected_tags,
                )
            }
            Some(version @ ("v3" | "v4")) => {
                let Some(tag_logic) = fields.next().and_then(parse_browser_tag_logic) else {
                    continue;
                };
                let Some(Ok(hidden)) = fields.next().map(str::parse::<bool>) else {
                    continue;
                };
                let (color, icon) = if version == "v4" {
                    let Some(color) = fields.next().and_then(BrowserTabColor::parse) else {
                        continue;
                    };
                    let Some(icon) = fields.next().and_then(BrowserTabIcon::parse) else {
                        continue;
                    };
                    (color, icon)
                } else {
                    (BrowserTabColor::default(), BrowserTabIcon::default())
                };
                let selected_tags = fields
                    .map(decode_browser_search_field)
                    .map(|tag| tag.trim().to_owned())
                    .filter(|tag| !tag.is_empty())
                    .collect::<BTreeSet<_>>();
                (tag_logic, hidden, color, icon, selected_tags)
            }
            _ => continue,
        };
        let name = decode_browser_search_field(name);
        let query = decode_browser_search_field(query);
        let Some(filter) = parse_browser_search_filter(filter) else {
            continue;
        };
        let (Ok(all_roots), Ok(recursive)) = (all_roots.parse::<bool>(), recursive.parse::<bool>())
        else {
            continue;
        };
        let path = decode_browser_search_field(path);
        if name.trim().is_empty() || (!all_roots && path.is_empty()) {
            continue;
        }
        let search = SavedBrowserSearch {
            name: name.trim().to_owned(),
            query,
            filter,
            all_roots,
            recursive,
            path: if all_roots {
                PathBuf::new()
            } else {
                PathBuf::from(path)
            },
            tag_logic,
            selected_tags,
            hidden,
            color,
            icon,
        };
        if let Some(existing) = searches
            .iter_mut()
            .find(|existing: &&mut SavedBrowserSearch| {
                existing.name.eq_ignore_ascii_case(&search.name)
            })
        {
            *existing = search;
        } else {
            searches.push(search);
        }
    }
    searches
}

fn save_browser_saved_searches(searches: &[SavedBrowserSearch]) -> Result<(), String> {
    let path = browser_saved_searches_file()
        .ok_or_else(|| "the user configuration folder is not available".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the Browser saved searches path has no parent folder".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let contents = searches
        .iter()
        .map(|search| {
            let path = if search.all_roots {
                String::new()
            } else {
                search.path.to_string_lossy().into_owned()
            };
            let mut fields = vec![
                "v4".to_owned(),
                encode_browser_search_field(&search.name),
                encode_browser_search_field(&search.query),
                browser_search_filter_key(search.filter).to_owned(),
                search.all_roots.to_string(),
                search.recursive.to_string(),
                encode_browser_search_field(&path),
                browser_tag_logic_key(search.tag_logic).to_owned(),
                search.hidden.to_string(),
                search.color.key().to_owned(),
                search.icon.key().to_owned(),
            ];
            fields.extend(
                search
                    .selected_tags
                    .iter()
                    .map(|tag| encode_browser_search_field(tag)),
            );
            fields.join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn browser_factory_packs_directory() -> Option<PathBuf> {
    let resolver = SamplePathResolver::new(Path::new("browser-root.flp"));
    for root in resolver.factory_roots() {
        for candidate in [
            root.join("Data").join("Patches").join("Packs"),
            root.join("Patches").join("Packs"),
            root.join("Packs"),
        ] {
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        if root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Packs"))
            && root.is_dir()
        {
            return Some(root.clone());
        }
    }
    None
}

fn browser_file_kind(path: &Path) -> Option<BrowserFileKind> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "wav" | "wave" | "mp3" | "m4a" | "ogg" | "flac" | "aif" | "aiff" | "wv" => {
            Some(BrowserFileKind::Audio)
        }
        "flp" | "zip" => Some(BrowserFileKind::Project),
        "fst" | "fxp" | "fxb" | "vstpreset" => Some(BrowserFileKind::Preset),
        "mid" | "midi" => Some(BrowserFileKind::Midi),
        _ => None,
    }
}

fn browser_filter_matches(path: &Path, filter: BrowserFilter) -> bool {
    matches!(
        (filter, browser_file_kind(path)),
        (BrowserFilter::All, Some(_))
            | (BrowserFilter::Audio, Some(BrowserFileKind::Audio))
            | (BrowserFilter::Projects, Some(BrowserFileKind::Project))
            | (BrowserFilter::Presets, Some(BrowserFileKind::Preset))
    )
}

fn browser_tag_selection_matches(
    logic: BrowserTagLogic,
    selected_tags: &BTreeSet<String>,
    file_tags: Option<&BTreeSet<String>>,
) -> bool {
    if selected_tags.is_empty() {
        return true;
    }
    let Some(file_tags) = file_tags else {
        return false;
    };
    match logic {
        BrowserTagLogic::Any => selected_tags.iter().any(|selected| {
            let selected = selected.to_lowercase();
            file_tags.iter().any(|tag| tag.to_lowercase() == selected)
        }),
        BrowserTagLogic::All => selected_tags.iter().all(|selected| {
            let selected = selected.to_lowercase();
            file_tags.iter().any(|tag| tag.to_lowercase() == selected)
        }),
    }
}

fn browser_file_icon(path: &Path) -> &'static str {
    match browser_file_kind(path) {
        Some(BrowserFileKind::Audio) => "♫",
        Some(BrowserFileKind::Project) => "FLP",
        Some(BrowserFileKind::Preset) => "FST",
        Some(BrowserFileKind::Midi) => "MIDI",
        None => "·",
    }
}

fn clip_name(clip: &PlaylistClip, tracks: &[PlaylistTrack]) -> String {
    match clip.target() {
        flp_rebuild::PlaylistClipTarget::Pattern { id } => format!("Pattern {}", id),
        flp_rebuild::PlaylistClipTarget::Channel { id } => tracks
            .iter()
            .find(|track| Some(track.id) == clip.playlist_track_id())
            .and_then(|track| track.name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Channel {}", id)),
    }
}

fn note_name(key: u16) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[(key % 12) as usize], key / 12)
}

fn draw_audio_clip_waveform(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    clip: &PlaylistClip,
    waveform: &AudioWaveform,
) {
    if waveform.peaks.is_empty() || waveform.frame_count == 0 || waveform.sample_rate == 0 {
        return;
    }
    let full_frame_count = waveform.frame_count;
    let to_frame = |milliseconds: f32| -> Option<u64> {
        let frame = f64::from(milliseconds) * f64::from(waveform.sample_rate) / 1000.0;
        (frame.is_finite() && frame >= 0.0 && frame <= u64::MAX as f64)
            .then_some(frame.round() as u64)
    };
    let source_window = if clip.start_offset == -1.0 && clip.end_offset == -1.0 {
        Some((0, full_frame_count))
    } else {
        to_frame(clip.start_offset)
            .zip(to_frame(clip.end_offset))
            .map(|(start, end)| (start.min(full_frame_count), end.min(full_frame_count)))
            .filter(|(start, end)| start < end)
    };
    let Some((source_start, source_end)) = source_window else {
        return;
    };
    let columns = clip_rect.width().ceil().clamp(1.0, 1024.0) as usize;
    let peak_count = waveform.peaks.len() as u128;
    let frame_count = u128::from(full_frame_count);
    let amplitude = (clip_rect.height() * 0.42).max(1.0);
    let center_y = clip_rect.center().y;
    let color = Color32::from_rgba_unmultiplied(238, 243, 248, 132);
    for column in 0..columns {
        let frame_start = u128::from(source_start)
            + u128::from(source_end - source_start) * column as u128 / columns as u128;
        let frame_end = u128::from(source_start)
            + u128::from(source_end - source_start) * (column + 1) as u128 / columns as u128;
        let first_peak = (frame_start * peak_count / frame_count) as usize;
        let last_peak = ((frame_end * peak_count / frame_count) as usize)
            .max(first_peak + 1)
            .min(waveform.peaks.len());
        let mut minimum = f32::INFINITY;
        let mut maximum = f32::NEG_INFINITY;
        for peak in &waveform.peaks[first_peak.min(waveform.peaks.len() - 1)..last_peak] {
            minimum = minimum.min(peak.minimum);
            maximum = maximum.max(peak.maximum);
        }
        if !minimum.is_finite() || !maximum.is_finite() {
            continue;
        }
        let x = clip_rect.left() + (column as f32 + 0.5) * clip_rect.width() / columns as f32;
        painter.line_segment(
            [
                egui::pos2(x, center_y - maximum * amplitude),
                egui::pos2(x, center_y - minimum * amplitude),
            ],
            Stroke::new(1.0, color),
        );
    }
}

fn snap_note_tick(value: i64, quantum: u32, minimum: u32) -> u32 {
    let quantum = u64::from(quantum.max(1));
    let value = value.max(0) as u64;
    let snapped = value
        .saturating_add(quantum / 2)
        .checked_div(quantum)
        .unwrap_or(0)
        .saturating_mul(quantum)
        .clamp(u64::from(minimum), u64::from(u32::MAX));
    snapped as u32
}

fn note_from_grid_position(
    pointer: egui::Pos2,
    grid: PianoRollGrid,
    channel_id: u16,
) -> Option<PatternNote> {
    let grid_left = grid.rect.left() + grid.keyboard_width;
    if !grid.rect.contains(pointer)
        || pointer.x < grid_left
        || grid.tick_scale <= 0.0
        || grid.key_height <= 0.0
    {
        return None;
    }
    let ticks_from_start = ((pointer.x - grid_left) / grid.tick_scale).round() as i64;
    let row = ((pointer.y - grid.rect.top()) / grid.key_height).floor() as u16;
    let key = grid
        .key_high
        .saturating_sub(row)
        .clamp(grid.key_low, grid.key_high);
    Some(PatternNote {
        position: snap_note_tick(ticks_from_start, grid.snap_ticks, 0),
        channel_id,
        length: u32::from(grid.ppq).max(1),
        key,
        velocity: 100,
        ..PatternNote::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        PianoRollGrid, PianoRollSnap, note_from_grid_position, snap_note_tick,
        update_layer_child_selection,
    };

    fn test_grid() -> PianoRollGrid {
        PianoRollGrid {
            rect: eframe::egui::Rect::from_min_size(
                eframe::egui::pos2(100.0, 50.0),
                eframe::egui::vec2(600.0, 624.0),
            ),
            keyboard_width: 68.0,
            tick_scale: 0.1,
            key_height: 13.0,
            key_low: 36,
            key_high: 83,
            snap_ticks: 24,
            ppq: 96,
        }
    }

    #[test]
    fn piano_roll_snap_sizes_follow_project_ppq_and_meter() {
        assert_eq!(PianoRollSnap::None.ticks(96, None), 1);
        assert_eq!(PianoRollSnap::QuarterBeat.ticks(96, None), 24);
        assert_eq!(PianoRollSnap::HalfBeat.ticks(96, None), 48);
        assert_eq!(PianoRollSnap::Beat.ticks(96, None), 96);
        assert_eq!(PianoRollSnap::TwoBeats.ticks(96, None), 192);
        assert_eq!(PianoRollSnap::Bar.ticks(96, Some((3, 4))), 288);
    }

    #[test]
    fn layer_child_editor_adds_and_removes_channel_selections() {
        let mut child_ids = vec![4, 2];
        update_layer_child_selection(&mut child_ids, 3, true);
        assert_eq!(child_ids, [4, 2, 3]);
        update_layer_child_selection(&mut child_ids, 2, false);
        assert_eq!(child_ids, [4, 3]);
        update_layer_child_selection(&mut child_ids, 3, true);
        assert_eq!(child_ids, [4, 3]);
    }

    #[test]
    fn note_tick_snapping_rounds_and_respects_length_minimum() {
        assert_eq!(snap_note_tick(-12, 24, 0), 0);
        assert_eq!(snap_note_tick(35, 24, 0), 24);
        assert_eq!(snap_note_tick(36, 24, 0), 48);
        assert_eq!(snap_note_tick(0, 24, 1), 1);
        assert_eq!(snap_note_tick(i64::from(u32::MAX), 1, 0), u32::MAX);
    }

    #[test]
    fn double_click_grid_mapping_applies_pitch_channel_and_snap() {
        let note = note_from_grid_position(eframe::egui::pos2(203.0, 121.5), test_grid(), 5)
            .expect("grid position should produce a note");
        assert_eq!(note.position, 360);
        assert_eq!(note.key, 78);
        assert_eq!(note.channel_id, 5);
        assert_eq!(note.length, 96);
        assert_eq!(note.velocity, 100);
    }

    #[test]
    fn piano_keyboard_and_outside_grid_do_not_create_notes() {
        for pointer in [
            eframe::egui::pos2(120.0, 100.0),
            eframe::egui::pos2(701.0, 100.0),
        ] {
            assert!(note_from_grid_position(pointer, test_grid(), 5).is_none());
        }
    }
}

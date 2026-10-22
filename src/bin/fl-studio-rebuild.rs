use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap_probe::{
    ClapInspectionResponse, ClapInspectionState, ClapInstanceInspectionResponse,
    ClapInstanceInspectionState, ClapNotePreviewRequest, ClapNotePreviewResponse,
    ClapNotePreviewState,
};
use eframe::egui::{self, Align2, Color32, FontId, Id, PointerButton, Sense, Stroke, Vec2};
use flp_rebuild::audio::{
    AudioAccess, AudioDeviceCatalog, AudioEngine, AudioSettings, MIXER_TRACK_METER_COUNT,
    MixerTrackPeakMeters, enumerate_devices,
};
use flp_rebuild::media::{
    AudioWaveform, DecodedAudio, SamplePathResolver, WaveformPeak, decode_audio_file,
    decode_audio_preview, decode_audio_preview_from_position, decode_audio_waveform,
    find_empty_project_template, summarize_audio_waveform,
    write_audio_range_to_wav_with_processing,
};
use flp_rebuild::midi::{MidiChannelMapping, MidiFile, MidiImportOptions};
use flp_rebuild::plugins::{PluginCandidate, PluginFormat, scan_installed_plugins};
use flp_rebuild::project_package::{ProjectPackageWorkspace, copy_unique_sample_file};
use flp_rebuild::sample_render::{
    AudioClipRenderOptions, AudioClipRenderSummary, AudioFileRenderOptions, AudioRecordingSummary,
    AudioRenderEncodingOptions, AudioRenderOutput, LayerLiveInputConfig, LayerLiveInputEvent,
    MP3_BITRATES_KBPS, MixerStemRenderOptions, PlaylistPerformanceCommand,
    PlaylistPerformanceLaunch, PlaylistPerformanceStream, PlaylistRenderOptions,
    PlaylistRenderRange, PlaylistRenderSummary, PlaylistStreamMonitor, PlaylistTempoMap,
    ResamplingQuality, SamplerLiveInputRouting, SamplerLiveNoteEvent, SamplerPatternLoopPlayback,
    SamplerPatternRenderOptions, SamplerPatternRenderSummary, Vst3LiveInputEvent,
    Vst3PatternLoopPlayback, WavChannelMode, WavDitherMode, WavSampleFormat,
    nearest_playlist_audio_clip_zero_crossing_tick, playlist_performance_clip_indices_with_vst3,
    playlist_performance_static_document_with_vst3, playlist_performance_vst3_channel_ids,
    playlist_performance_vst3_clip_indices, render_audio_clips_to_file,
    render_playlist_mixer_stems_with_vst3_cancellable,
    render_playlist_with_vst3_to_file_from_frame_cancellable,
    render_playlist_with_vst3_to_flac_cancellable, render_playlist_with_vst3_to_mp3_cancellable,
    render_playlist_with_vst3_to_ogg_cancellable, render_playlist_with_vst3_to_wav_cancellable,
    render_sampler_pattern_to_file, stream_mixed_layer_midi_input_to_device,
    stream_pattern_loop_with_vst3_from_tick_to_device_with_live_master_controls,
    stream_playlist_performance_with_vst3_to_device,
    stream_playlist_with_vst3_to_device_with_monitor, stream_sampler_midi_input_to_device,
    stream_sampler_pattern_loop_from_tick_to_device_with_live_master_controls,
    stream_sampler_pattern_to_device, stream_vst3_midi_input_to_device,
    write_input_recording_to_wav,
};
use flp_rebuild::vst3::{
    MAX_REPORTED_TAIL_SECONDS, Vst3AutomationPoint, Vst3HostRuntime, Vst3ParameterAutomation,
    Vst3PatternChannelRenderRequest, Vst3PatternRenderOptions, Vst3PatternStreamHandle,
    Vst3PlaylistLiveInputSender, Vst3PlaylistLiveInputSession, Vst3PlaylistStreamProcessor,
};
use flp_rebuild::{
    ArpeggioDirection, ArpeggioOptions, ArpeggioRangePattern, ArpeggioSyncMode, Arrangement,
    ArticulateOptions, AutomationChannel, AutomationPoint, AutomationPointEdit,
    ChannelGroupSummary, ChannelNoteRouter, ChannelSortOrder, ChannelStretchMode,
    ChannelStretchSettingsEdit, ChannelSummary, ChannelType, ClawMachineOptions, FlamOptions,
    FlpDocument, FlpError, FstPreset, FstPresetKind, HumanizerOptions, LimitNoteOptions,
    LimitSnapDirection, MixerInsertEdit, MixerInsertSummary, MixerParameterKind,
    MixerParameterRecord, MixerSendSummary, PATTERN_NOTE_DEFAULT_FLAGS,
    PATTERN_NOTE_DEFAULT_RELEASE, PATTERN_NOTE_SLIDE_FLAG, Pattern, PatternController,
    PatternControllerEdit, PatternNote, PatternNoteEdit, PatternNoteRotation, PayloadEncoding,
    PlaylistClip, PlaylistClipEdit, PlaylistClipSelectionClipboard, PlaylistClipTarget,
    PlaylistTrack, PlaylistTrackEdit, PlaylistTrackMotion, PlaylistTrackPress, PlaylistTrackSync,
    ProjectInfoEdit, ProjectSettingsEdit, QuantizeNoteMode, QuantizeNoteOptions,
    RandomNoteChordMap, RandomNoteGeneratorOptions, RandomizerOptions, RiffMachineOptions,
    RiffMachineQuantizeMode, SamplerEnvelopeLfo, SamplerEnvelopeLfoEdit, SamplerEnvelopeLfoTarget,
    ScaleLevelsOptions, StrumOptions, TimeMarker, TimeMarkerEdit, VstPluginStateMetadata,
};
use md5::{Digest, Md5};
use vst3_host::midi::{MidiChannel as Vst3MidiChannel, MidiEvent as Vst3MidiEvent};

#[path = "fl-studio-rebuild/clap_probe.rs"]
mod clap_probe;
mod midi_input;
mod midi_output;
#[path = "fl-studio-rebuild/midi_thru.rs"]
mod midi_thru;

use midi_input::{
    ConnectedMidiInput, MidiInputChannelState, MidiInputDevice, MidiInputEvent,
    MidiPatternRecorder, MidiPatternRecorderSettings, MidiThruInputQueue, MidiTransportCommand,
    ReceivedMidiMessage, connect_input_device, describe_event, enumerate_input_devices,
};
use midi_output::{
    ConnectedMidiOutput, MIDI_TEST_NOTE_OFF, MIDI_TEST_NOTE_ON, MidiClockScheduler,
    MidiOutputDevice, MidiPatternLoopScheduler, MidiPlaylistScheduler, ScheduledMidiFrameMessage,
    ScheduledMidiMessage, ScheduledMidiTickMessage, connect_output_device_with_routes,
    enumerate_output_devices, midi_patch_setup_messages, pattern_loop_frame_messages,
    pattern_preview_messages, playlist_playback_messages,
};
use midi_thru::{MidiThruSettings, MidiThruWorker};

const APP_BACKGROUND: Color32 = Color32::from_rgb(27, 27, 27);
const PANEL: Color32 = Color32::from_rgb(44, 44, 44);
const PANEL_DARK: Color32 = Color32::from_rgb(34, 34, 34);
const PANEL_LIGHT: Color32 = Color32::from_rgb(57, 57, 57);
const GRID: Color32 = Color32::from_rgb(50, 50, 50);
const BORDER: Color32 = Color32::from_rgb(68, 68, 68);
const TEXT: Color32 = Color32::from_rgb(242, 242, 242);
const MUTED: Color32 = Color32::from_rgb(175, 175, 175);
const GREEN: Color32 = Color32::from_rgb(14, 175, 98);
const BLUE: Color32 = Color32::from_rgb(59, 99, 251);
const BLUE_TEXT: Color32 = Color32::from_rgb(86, 129, 255);
const BLUE_SELECTION: Color32 = Color32::from_rgb(14, 23, 63);
const RED_SELECTION: Color32 = Color32::from_rgb(62, 22, 20);
const PLAYLIST_LABEL_WIDTH: f32 = 178.0;
const PURPLE: Color32 = Color32::from_rgb(150, 93, 181);
const ORANGE: Color32 = Color32::from_rgb(195, 129, 61);
const RED: Color32 = Color32::from_rgb(252, 67, 46);

fn tempo_relative_note_division_ticks(ppq: u16, denominator: u16) -> Option<u32> {
    if ppq == 0 || !matches!(denominator, 1 | 2 | 4 | 8 | 16 | 32 | 64 | 128) {
        return None;
    }
    let quarter_note_ticks = u64::from(ppq) * 4;
    let denominator = u64::from(denominator);
    let ticks = (quarter_note_ticks + denominator / 2) / denominator;
    u32::try_from(ticks).ok().filter(|ticks| *ticks > 0)
}

fn state_preset_file_stem(name: &str, fallback: &str) -> String {
    let safe_name = name
        .chars()
        .map(|character| {
            if character.is_control() || "/\\:*?\"<>|".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let stem = safe_name.trim().trim_matches('.');
    let stem = if stem.to_ascii_lowercase().ends_with(".fst") {
        &stem[..stem.len() - 4]
    } else {
        stem
    };
    if stem.is_empty() {
        fallback.to_owned()
    } else {
        stem.to_owned()
    }
}

const MENU_BAR_HEIGHT: f32 = 32.0;
const TRANSPORT_BAR_HEIGHT: f32 = 46.0;
const STATUS_BAR_HEIGHT: f32 = 27.0;
const PIANO_ROLL_RULER_HEIGHT: f32 = 20.0;
const MIN_WINDOW_INNER_WIDTH: f32 = 960.0;
const MIN_WINDOW_INNER_HEIGHT: f32 = 640.0;
const DEFAULT_BROWSER_COLUMN_WIDTH: f32 = 280.0;
const MIN_BROWSER_COLUMN_WIDTH: f32 = 180.0;
const MAX_BROWSER_COLUMN_WIDTH: f32 = 500.0;
const HISTORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const MAX_TIME_MARKER_TICK: u32 = 0x07FF_FFFF;
const AUDIO_WAVEFORM_BUCKETS: usize = 4096;
const MAX_WAVEFORM_WORKERS: usize = 1;
const DEFAULT_AUTOSAVE_MINUTES: u8 = 5;
const DEFAULT_BACKUP_RETENTION: usize = 20;
const RECENT_PROJECT_LIMIT: usize = 50;
const UI_SCALE_STEPS: [f32; 7] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4];
const BROWSER_SEARCH_ROOT_LIMIT: usize = 30;
const MIDI_RECORDING_OFFSET_MIN_MS: i16 = -200;
const MIDI_RECORDING_OFFSET_MAX_MS: i16 = 200;
const AUDIO_RECORDING_OFFSET_MIN_MS: i16 = -200;
const AUDIO_RECORDING_OFFSET_MAX_MS: i16 = 200;
const MIDI_FIXED_RECORDING_VELOCITY: u8 = 100;
const MAX_CAPTURED_PATTERN_CONTROLLER_POINTS: usize = 100_000;
const MAX_BROWSER_RECURSIVE_SCAN_ENTRIES: usize = 20_000;
const AUTOSAVE_INTERVALS_MINUTES: [u8; 5] = [0, 1, 5, 10, 15];
const BACKUP_RETENTION_OPTIONS: [usize; 4] = [5, 10, 20, 50];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PlaylistSongOutputFormat {
    #[default]
    Wav,
    Flac,
    Ogg,
    Mp3,
}

impl PlaylistSongOutputFormat {
    const fn label(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Flac => "FLAC",
            Self::Ogg => "OGG",
            Self::Mp3 => "MP3",
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Mp3 => "mp3",
        }
    }
}

fn add_audio_render_file_filter(
    file_dialog: rfd::FileDialog,
    output_format: PlaylistSongOutputFormat,
) -> rfd::FileDialog {
    match output_format {
        PlaylistSongOutputFormat::Wav => file_dialog.add_filter("WAV audio", &["wav"]),
        PlaylistSongOutputFormat::Flac => file_dialog.add_filter("FLAC audio", &["flac"]),
        PlaylistSongOutputFormat::Ogg => file_dialog.add_filter("Ogg Vorbis audio", &["ogg"]),
        PlaylistSongOutputFormat::Mp3 => file_dialog.add_filter("MP3 audio", &["mp3"]),
    }
}

const PITCH_CLASSES: [&str; 12] = [
    "C", "Câ™¯", "D", "Dâ™¯", "E", "F", "Fâ™¯", "G", "Gâ™¯", "A", "Aâ™¯", "B",
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

fn install_ui_fonts(context: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        egui::FontData::from_static(include_bytes!("../../assets/fonts/InterVariable.ttf")).into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    context.set_fonts(fonts);
}

fn app_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = APP_BACKGROUND;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = PANEL_DARK;
    visuals.text_edit_bg_color = Some(PANEL_DARK);
    visuals.faint_bg_color = PANEL_LIGHT;
    visuals.code_bg_color = PANEL_DARK;
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.hyperlink_color = BLUE_TEXT;
    visuals.selection.bg_fill = BLUE_SELECTION;
    visuals.selection.stroke = Stroke::new(1.0, BLUE);
    visuals.window_corner_radius = egui::CornerRadius::same(4);
    visuals.menu_corner_radius = egui::CornerRadius::same(3);
    visuals.window_shadow = egui::Shadow::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.disabled_alpha = 0.94;
    visuals.button_frame = true;
    visuals.striped = true;

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = APP_BACKGROUND;
    widgets.noninteractive.weak_bg_fill = PANEL;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.noninteractive.corner_radius = egui::CornerRadius::same(3);

    widgets.inactive.bg_fill = PANEL;
    widgets.inactive.weak_bg_fill = PANEL;
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.inactive.corner_radius = egui::CornerRadius::same(4);

    widgets.hovered.bg_fill = PANEL_LIGHT;
    widgets.hovered.weak_bg_fill = PANEL_LIGHT;
    widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.hovered.corner_radius = egui::CornerRadius::same(4);

    widgets.active.bg_fill = BLUE_SELECTION;
    widgets.active.weak_bg_fill = BLUE_SELECTION;
    widgets.active.bg_stroke = Stroke::new(1.0, BLUE);
    widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.active.corner_radius = egui::CornerRadius::same(4);

    widgets.open.bg_fill = BLUE_SELECTION;
    widgets.open.weak_bg_fill = PANEL_LIGHT;
    widgets.open.bg_stroke = Stroke::new(1.0, BLUE);
    widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.open.corner_radius = egui::CornerRadius::same(4);

    visuals
}

fn install_ui_visuals(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);
    let visuals = app_visuals();

    // Some native backends initialize the root app Ui before applying the
    // user's theme. Keep both context variants on this palette for menus and
    // popups; the root Ui also reapplies it each frame below.
    context.set_visuals_of(egui::Theme::Dark, visuals.clone());
    context.set_visuals_of(egui::Theme::Light, visuals.clone());
    context.set_visuals(visuals);
    context.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(7.0, 5.0);
        style.spacing.button_padding = Vec2::new(9.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(14);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.slider_rail_height = 4.0;
    });
}

fn unique_candidate_mixer_parameter(
    parameters: &[MixerParameterRecord],
    insert_ordinal: usize,
    kind: MixerParameterKind,
) -> Option<&MixerParameterRecord> {
    let mut matches = parameters.iter().filter(|record| {
        record.kind() == kind
            && record.candidate_insert_ordinal() == Some(insert_ordinal)
            && record.target_scope_raw() == 0
    });
    let record = matches.next()?;
    matches.next().is_none().then_some(record)
}

fn candidate_mixer_controls(
    parameters: &[MixerParameterRecord],
    insert_ordinal: usize,
) -> Option<(&MixerParameterRecord, &MixerParameterRecord)> {
    Some((
        unique_candidate_mixer_parameter(parameters, insert_ordinal, MixerParameterKind::Volume)?,
        unique_candidate_mixer_parameter(parameters, insert_ordinal, MixerParameterKind::Pan)?,
    ))
}

fn candidate_mixer_master_fader_gain(document: &FlpDocument) -> f32 {
    let parameters = document.mixer_parameter_records().unwrap_or_default();
    unique_candidate_mixer_parameter(&parameters, 0, MixerParameterKind::Volume)
        .filter(|record| (0..=16_000).contains(&record.value()))
        .map_or(1.0, |record| record.value() as f32 / 10_000.0)
}

fn candidate_mixer_master_pan_raw(document: &FlpDocument) -> Option<i32> {
    let parameters = document.mixer_parameter_records().ok()?;
    unique_candidate_mixer_parameter(&parameters, 0, MixerParameterKind::Pan)
        .map(MixerParameterRecord::value)
        .filter(|pan_raw| (-6_400..=6_400).contains(pan_raw))
}

fn candidate_mixer_master_stereo_separation_raw(document: &FlpDocument) -> Option<i32> {
    let parameters = document.mixer_parameter_records().ok()?;
    candidate_mixer_stereo_separation(&parameters, 0).map(MixerParameterRecord::value)
}

fn candidate_mixer_master_enabled(document: &FlpDocument) -> Option<bool> {
    document
        .mixer_inserts()
        .into_iter()
        .find(|insert| insert.ordinal() == 0)
        .and_then(|master| master.enabled())
}

fn candidate_mixer_insert_enabled(
    inserts: &[MixerInsertSummary],
    insert_ordinal: usize,
) -> Option<bool> {
    inserts
        .iter()
        .find(|insert| insert.ordinal() == insert_ordinal)
        .and_then(MixerInsertSummary::enabled)
}

fn candidate_live_mixer_send_key(
    sends: &[MixerSendSummary],
    selected_send: &MixerSendSummary,
) -> Option<(usize, usize)> {
    selected_send.enabled()?;
    let destination_track = usize::from(selected_send.route_parameter_id().checked_sub(64)?);
    if destination_track >= MIXER_TRACK_METER_COUNT {
        return None;
    }
    let matching_routes = sends
        .iter()
        .filter(|send| {
            send.enabled().is_some()
                && send.insert_ordinal() == selected_send.insert_ordinal()
                && send.route_parameter_id() == selected_send.route_parameter_id()
        })
        .count();
    (matching_routes == 1).then_some((selected_send.insert_ordinal(), destination_track))
}

fn candidate_mixer_master_signal_flags(document: &FlpDocument) -> Option<(bool, bool)> {
    candidate_mixer_insert_signal_flags(&document.mixer_inserts(), 0)
}

fn candidate_mixer_insert_signal_flags(
    inserts: &[MixerInsertSummary],
    insert_ordinal: usize,
) -> Option<(bool, bool)> {
    let insert = inserts
        .iter()
        .find(|insert| insert.ordinal() == insert_ordinal)?;
    Some((insert.polarity_reversed()?, insert.swap_left_right()?))
}

fn candidate_mixer_stereo_separation(
    parameters: &[MixerParameterRecord],
    insert_ordinal: usize,
) -> Option<&MixerParameterRecord> {
    let record = unique_candidate_mixer_parameter(
        parameters,
        insert_ordinal,
        MixerParameterKind::StereoSeparation,
    )?;
    (-64..=64).contains(&record.value()).then_some(record)
}

fn project_hash(document: &FlpDocument) -> Option<u64> {
    let bytes = document.encode_lossless().ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

fn project_channel_swing_mix_raw(document: &FlpDocument, channel_id: u16) -> u16 {
    document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .map_or(128, |channel| channel.swing_mix())
}

fn project_channel_pitch_offset_cents(document: &FlpDocument, channel_id: u16) -> i16 {
    let project_master_pitch_cents = document.metadata().master_pitch_cents();
    document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .map_or(0, |channel| {
            channel
                .master_pitch_render_offset_cents(project_master_pitch_cents)
                .saturating_add(channel.keyboard_fine_tune_cents().unwrap_or_default() as i16)
        })
}

fn project_channel_master_pitch_note_offset_semitones(
    document: &FlpDocument,
    channel_id: u16,
) -> i16 {
    let project_master_pitch_cents = document.metadata().master_pitch_cents();
    document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .map_or(0, |channel| {
            channel.master_pitch_note_offset_semitones(project_master_pitch_cents)
        })
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
    let class_uid = metadata.class_uid();
    candidate_matches_vst_identity(
        candidate,
        class_uid.as_deref(),
        metadata.path(),
        metadata.name(),
    )
}

fn candidate_matches_vst_identity(
    candidate: &PluginCandidate,
    class_uid: Option<&str>,
    plugin_path: Option<&str>,
    plugin_name: Option<&str>,
) -> bool {
    if candidate.format != PluginFormat::Vst3 {
        return false;
    }

    if let Some(class_uid) = class_uid.filter(|_| !candidate.class_ids.is_empty()) {
        return candidate
            .class_ids
            .iter()
            .any(|candidate_uid| candidate_uid.eq_ignore_ascii_case(class_uid));
    }

    let candidate_name = candidate.name.to_lowercase();
    let source_bundle = plugin_path.and_then(|path| {
        Path::new(path)
            .file_stem()
            .or_else(|| Path::new(path).file_name())
            .map(|value| value.to_string_lossy().to_lowercase())
    });
    source_bundle.as_deref() == Some(candidate_name.as_str())
        || plugin_name.is_some_and(|name| name.eq_ignore_ascii_case(&candidate.name))
}

fn detect_chord_name(notes: &[PatternNote], channel_id: u16, position: u32) -> String {
    let onset_notes = notes
        .iter()
        .filter(|note| note.channel_id == channel_id && note.position == position)
        .collect::<Vec<_>>();
    let pitch_classes = notes
        .iter()
        .filter(|note| note.channel_id == channel_id && note.position == position)
        .map(|note| (note.key % 12) as u8)
        .collect::<BTreeSet<_>>();
    if pitch_classes.len() < 2 {
        return "No chord".to_owned();
    }
    let bass_pitch_class = onset_notes
        .iter()
        .min_by_key(|note| note.key)
        .map(|note| (note.key % 12) as u8)
        .expect("a chord pitch class has at least one note");

    const QUALITIES: &[(&str, &[u8])] = &[
        ("maj13", &[0, 2, 4, 5, 7, 9, 11]),
        ("13", &[0, 2, 4, 5, 7, 9, 10]),
        ("m13", &[0, 2, 3, 5, 7, 9, 10]),
        ("maj11", &[0, 2, 4, 5, 7, 11]),
        ("11", &[0, 2, 4, 5, 7, 10]),
        ("m11", &[0, 2, 3, 5, 7, 10]),
        ("maj9", &[0, 2, 4, 7, 11]),
        ("9", &[0, 2, 4, 7, 10]),
        ("m9", &[0, 2, 3, 7, 10]),
        ("m(maj9)", &[0, 2, 3, 7, 11]),
        ("maj7â™¯11", &[0, 4, 6, 7, 11]),
        ("7â™­9", &[0, 1, 4, 7, 10]),
        ("7â™¯9", &[0, 3, 4, 7, 10]),
        ("6/9", &[0, 2, 4, 7, 9]),
        ("m(maj7)", &[0, 3, 7, 11]),
        ("m7â™­5", &[0, 3, 6, 10]),
        ("dim7", &[0, 3, 6, 9]),
        ("maj7", &[0, 4, 7, 11]),
        ("maj7â™¯5", &[0, 4, 8, 11]),
        ("m7", &[0, 3, 7, 10]),
        ("7", &[0, 4, 7, 10]),
        ("7â™­5", &[0, 4, 6, 10]),
        ("7â™¯5", &[0, 4, 8, 10]),
        ("m7â™¯5", &[0, 3, 8, 10]),
        ("7sus4", &[0, 5, 7, 10]),
        ("7sus2", &[0, 2, 7, 10]),
        ("m6", &[0, 3, 7, 9]),
        ("6", &[0, 4, 7, 9]),
        ("madd9", &[0, 2, 3, 7]),
        ("add9", &[0, 2, 4, 7]),
        ("diminished", &[0, 3, 6]),
        ("augmented", &[0, 4, 8]),
        ("sus2", &[0, 2, 7]),
        ("sus4", &[0, 5, 7]),
        ("major", &[0, 4, 7]),
        ("minor", &[0, 3, 7]),
        ("5", &[0, 7]),
    ];
    let roots = std::iter::once(bass_pitch_class)
        .chain(
            pitch_classes
                .iter()
                .copied()
                .filter(|root| *root != bass_pitch_class),
        )
        .collect::<Vec<_>>();
    for root in &roots {
        let mut intervals = pitch_classes
            .iter()
            .map(|pitch| (*pitch + 12 - *root) % 12)
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        for (quality, expected) in QUALITIES {
            if intervals == *expected {
                let inversion = if bass_pitch_class == *root {
                    String::new()
                } else {
                    format!("/{}", PITCH_CLASSES[usize::from(bass_pitch_class)])
                };
                return format!("{} {quality}{inversion}", PITCH_CLASSES[usize::from(*root)]);
            }
        }
    }
    for root in roots {
        let mut intervals = pitch_classes
            .iter()
            .map(|pitch| (*pitch + 12 - root) % 12)
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        if intervals.len() < 3 || !intervals.contains(&0) {
            continue;
        }
        for (quality, expected) in QUALITIES {
            if expected.len() < 4
                || !expected.contains(&7)
                || !(expected.contains(&3) || expected.contains(&4))
                || !expected.contains(&0)
                || intervals.len() + 1 != expected.len()
                || intervals
                    .iter()
                    .any(|interval| !expected.contains(interval))
            {
                continue;
            }
            let missing = expected
                .iter()
                .filter(|interval| !intervals.contains(interval))
                .copied()
                .collect::<Vec<_>>();
            if missing.as_slice() != [7] {
                continue;
            }
            let inversion = if bass_pitch_class == root {
                String::new()
            } else {
                format!("/{}", PITCH_CLASSES[usize::from(bass_pitch_class)])
            };
            return format!(
                "{} {quality}(no5){inversion}",
                PITCH_CLASSES[usize::from(root)]
            );
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

fn clap_descriptor_supports_note_preview(features: &[String]) -> bool {
    features.iter().any(|feature| {
        matches!(
            feature.to_ascii_lowercase().as_str(),
            "instrument" | "synthesizer" | "sampler" | "drum" | "drum-machine"
        )
    })
}

fn main() -> eframe::Result {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args
        .get(1)
        .is_some_and(|argument| argument == std::ffi::OsStr::new(clap_probe::PROBE_FLAG))
    {
        let Some(path) = args.get(2).map(PathBuf::from) else {
            eprintln!("CLAP descriptor probe needs a bundle or library path");
            std::process::exit(2);
        };
        std::process::exit(clap_probe::run_child(&path));
    }
    if args
        .get(1)
        .is_some_and(|argument| argument == std::ffi::OsStr::new(clap_probe::INSTANCE_PROBE_FLAG))
    {
        let Some(path) = args.get(2).map(PathBuf::from) else {
            eprintln!("CLAP instance probe needs a bundle or library path and plug-in ID");
            std::process::exit(2);
        };
        let Some(plugin_id) = args.get(3).and_then(|value| value.to_str()) else {
            eprintln!("CLAP instance probe needs a UTF-8 plug-in ID");
            std::process::exit(2);
        };
        std::process::exit(clap_probe::run_instance_child(&path, plugin_id));
    }
    if args
        .get(1)
        .is_some_and(|argument| argument == std::ffi::OsStr::new(clap_probe::NOTE_PREVIEW_FLAG))
    {
        let Some(path) = args.get(2).map(PathBuf::from) else {
            eprintln!(
                "CLAP note preview needs a bundle or library path, plug-in ID, and sample rate"
            );
            std::process::exit(2);
        };
        let Some(plugin_id) = args.get(3).and_then(|value| value.to_str()) else {
            eprintln!("CLAP note preview needs a UTF-8 plug-in ID");
            std::process::exit(2);
        };
        let Some(sample_rate) = args.get(4).and_then(|value| value.to_str()) else {
            eprintln!("CLAP note preview needs a sample rate");
            std::process::exit(2);
        };
        let Ok(sample_rate) = sample_rate.parse::<u32>() else {
            eprintln!("CLAP note preview sample rate must be an integer");
            std::process::exit(2);
        };
        std::process::exit(clap_probe::run_note_preview_child(
            &path,
            plugin_id,
            sample_rate,
        ));
    }
    let initial_project = args.get(1).map(PathBuf::from);
    let ui_scale = load_ui_scale();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("FL Studio Rebuild")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([
                MIN_WINDOW_INNER_WIDTH / ui_scale,
                MIN_WINDOW_INNER_HEIGHT / ui_scale,
            ])
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DetachedWindowKind {
    Playlist,
    ChannelRack,
    PianoRoll,
    Mixer,
    Plugins,
    Audio,
    Automation,
}

impl DetachedWindowKind {
    const ALL: [Self; 7] = [
        Self::Playlist,
        Self::ChannelRack,
        Self::PianoRoll,
        Self::Mixer,
        Self::Plugins,
        Self::Audio,
        Self::Automation,
    ];

    const fn settings_key(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::ChannelRack => "channel_rack",
            Self::PianoRoll => "piano_roll",
            Self::Mixer => "mixer",
            Self::Plugins => "plugins",
            Self::Audio => "audio",
            Self::Automation => "automation",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Playlist => 0,
            Self::ChannelRack => 1,
            Self::PianoRoll => 2,
            Self::Mixer => 3,
            Self::Plugins => 4,
            Self::Audio => 5,
            Self::Automation => 6,
        }
    }

    const fn minimum_size(self) -> Vec2 {
        match self {
            Self::Playlist => Vec2::new(800.0, 560.0),
            Self::ChannelRack => Vec2::new(640.0, 480.0),
            Self::PianoRoll | Self::Mixer => Vec2::new(760.0, 480.0),
            Self::Plugins => Vec2::new(720.0, 480.0),
            Self::Audio => Vec2::new(720.0, 480.0),
            Self::Automation => Vec2::new(760.0, 480.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DetachedWindowPlacement {
    position: Option<egui::Pos2>,
    inner_size: Vec2,
}

impl DetachedWindowPlacement {
    fn is_valid(self, kind: DetachedWindowKind) -> bool {
        let minimum = kind.minimum_size();
        self.inner_size.x.is_finite()
            && self.inner_size.y.is_finite()
            && self.inner_size.x >= minimum.x
            && self.inner_size.y >= minimum.y
            && self.inner_size.x <= 16_384.0
            && self.inner_size.y <= 16_384.0
            && self.position.is_none_or(|position| {
                position.x.is_finite()
                    && position.y.is_finite()
                    && position.x.abs() <= 100_000.0
                    && position.y.abs() <= 100_000.0
            })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DetachedWindowPlacements {
    playlist: Option<DetachedWindowPlacement>,
    channel_rack: Option<DetachedWindowPlacement>,
    piano_roll: Option<DetachedWindowPlacement>,
    mixer: Option<DetachedWindowPlacement>,
    plugins: Option<DetachedWindowPlacement>,
    audio: Option<DetachedWindowPlacement>,
    automation: Option<DetachedWindowPlacement>,
}

impl DetachedWindowPlacements {
    fn get(self, kind: DetachedWindowKind) -> Option<DetachedWindowPlacement> {
        match kind {
            DetachedWindowKind::Playlist => self.playlist,
            DetachedWindowKind::ChannelRack => self.channel_rack,
            DetachedWindowKind::PianoRoll => self.piano_roll,
            DetachedWindowKind::Mixer => self.mixer,
            DetachedWindowKind::Plugins => self.plugins,
            DetachedWindowKind::Audio => self.audio,
            DetachedWindowKind::Automation => self.automation,
        }
    }

    fn set(&mut self, kind: DetachedWindowKind, placement: DetachedWindowPlacement) {
        match kind {
            DetachedWindowKind::Playlist => self.playlist = Some(placement),
            DetachedWindowKind::ChannelRack => self.channel_rack = Some(placement),
            DetachedWindowKind::PianoRoll => self.piano_roll = Some(placement),
            DetachedWindowKind::Mixer => self.mixer = Some(placement),
            DetachedWindowKind::Plugins => self.plugins = Some(placement),
            DetachedWindowKind::Audio => self.audio = Some(placement),
            DetachedWindowKind::Automation => self.automation = Some(placement),
        }
    }
}

const WINDOW_LAYOUT_PRESET_LIMIT: usize = 32;
const WINDOW_LAYOUT_PRESET_NAME_LIMIT: usize = 40;
const WINDOW_LAYOUT_PRESETS_HEADER: &str = "fl-studio-rust-window-layouts-v1";

#[derive(Clone, Debug, PartialEq)]
struct WindowLayoutPreset {
    name: String,
    main_view: MainView,
    browser_visible: bool,
    detached: [bool; 7],
    placements: DetachedWindowPlacements,
}

impl WindowLayoutPreset {
    fn is_valid(&self) -> bool {
        let name = self.name.trim();
        !name.is_empty()
            && name.chars().count() <= WINDOW_LAYOUT_PRESET_NAME_LIMIT
            && !name.chars().any(char::is_control)
            && DetachedWindowKind::ALL.into_iter().all(|kind| {
                self.placements
                    .get(kind)
                    .is_none_or(|placement| placement.is_valid(kind))
            })
    }
}

fn detached_window_viewport_builder(
    kind: DetachedWindowKind,
    title: &'static str,
    default_size: Vec2,
    placement: Option<DetachedWindowPlacement>,
) -> egui::ViewportBuilder {
    let mut builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(default_size)
        .with_min_inner_size(kind.minimum_size())
        .with_resizable(true);
    if let Some(placement) = placement {
        builder = builder.with_inner_size(placement.inner_size);
        if let Some(position) = placement.position {
            builder = builder.with_position(position);
        }
    }
    builder
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ChannelDisplayFilter {
    #[default]
    All,
    Unsorted,
    Group(i32),
}

impl ChannelDisplayFilter {
    fn label(self, groups: &[ChannelGroupSummary]) -> String {
        match self {
            Self::All => "All".to_owned(),
            Self::Unsorted => "Unsorted".to_owned(),
            Self::Group(index) => groups
                .iter()
                .find(|group| group.index() == index)
                .and_then(ChannelGroupSummary::name)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Group {}", index.saturating_add(1))),
        }
    }

    fn shows(self, group_number: Option<i32>, known_groups: &BTreeSet<i32>) -> bool {
        match self {
            Self::All => true,
            Self::Unsorted => {
                group_number.is_none_or(|index| index < 0 || !known_groups.contains(&index))
            }
            Self::Group(index) => group_number == Some(index),
        }
    }

    fn step_group(self, group_indices: &[i32], direction: i32) -> Self {
        if group_indices.is_empty() {
            return self;
        }
        let current = match self {
            Self::Group(index) => group_indices.iter().position(|group| *group == index),
            Self::All | Self::Unsorted => None,
        };
        let next = match (current, direction.signum()) {
            (Some(index), 1) => (index + 1) % group_indices.len(),
            (Some(index), -1) => (index + group_indices.len() - 1) % group_indices.len(),
            (None, 1) => 0,
            (None, -1) => group_indices.len() - 1,
            (_, 0) => return self,
            _ => unreachable!("direction was normalized above"),
        };
        Self::Group(group_indices[next])
    }
}

#[derive(Clone)]
enum ChannelGroupDialogAction {
    AssignSelected(Vec<u16>),
    CreateEmpty,
    Rename(i32),
}

#[derive(Clone)]
struct ChannelGroupDialog {
    action: ChannelGroupDialogAction,
    name: String,
}

#[derive(Clone)]
struct PatternRenameDialog {
    pattern_id: u16,
    name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistWaveformMode {
    Combined,
    Stereo,
    Spectral,
}

impl PlaylistWaveformMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Combined => "Combined",
            Self::Stereo => "Stereo",
            Self::Spectral => "Spectral",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollSnap {
    None,
    QuarterBeat,
    TripletSixteenth,
    HalfBeat,
    TripletEighth,
    Beat,
    TripletQuarter,
    TwoBeats,
    TripletHalf,
    Bar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformanceGlobalSnap {
    None,
    Line,
    Cell,
    StepSixth,
    StepQuarter,
    StepThird,
    StepHalf,
    StepTwoThirds,
    StepThreeQuarters,
    Step,
    BeatSixth,
    BeatQuarter,
    BeatThird,
    BeatHalf,
    BeatTwoThirds,
    BeatThreeQuarters,
    Beat,
    Bar,
}

impl PerformanceGlobalSnap {
    const ALL: [Self; 18] = [
        Self::Line,
        Self::Cell,
        Self::None,
        Self::StepSixth,
        Self::StepQuarter,
        Self::StepThird,
        Self::StepHalf,
        Self::StepTwoThirds,
        Self::StepThreeQuarters,
        Self::Step,
        Self::BeatSixth,
        Self::BeatQuarter,
        Self::BeatThird,
        Self::BeatHalf,
        Self::BeatTwoThirds,
        Self::BeatThreeQuarters,
        Self::Beat,
        Self::Bar,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Line => "Line",
            Self::Cell => "Cell",
            Self::StepSixth => "1/6 step",
            Self::StepQuarter => "1/4 step",
            Self::StepThird => "1/3 step",
            Self::StepHalf => "1/2 step",
            Self::StepTwoThirds => "2/3 step",
            Self::StepThreeQuarters => "3/4 step",
            Self::Step => "1 step",
            Self::BeatSixth => "1/6 beat",
            Self::BeatQuarter => "1/4 beat",
            Self::BeatThird => "1/3 beat",
            Self::BeatHalf => "1/2 beat",
            Self::BeatTwoThirds => "2/3 beat",
            Self::BeatThreeQuarters => "3/4 beat",
            Self::Beat => "1 beat",
            Self::Bar => "Bar",
        }
    }

    fn interval_ticks(self, ppq: u16, signature: (u8, u8)) -> Option<u64> {
        let ppq = ppq.max(1);
        let ppq_ticks = u64::from(ppq);
        let beat_ticks = playlist_bar_ticks(ppq, (1, signature.1.max(1)));
        let (base_ticks, numerator, denominator) = match self {
            Self::None | Self::Line | Self::Cell => return None,
            Self::StepSixth => (ppq_ticks, 1, 24),
            Self::StepQuarter => (ppq_ticks, 1, 16),
            Self::StepThird => (ppq_ticks, 1, 12),
            Self::StepHalf => (ppq_ticks, 1, 8),
            Self::StepTwoThirds => (ppq_ticks, 1, 6),
            Self::StepThreeQuarters => (ppq_ticks, 3, 16),
            Self::Step => (ppq_ticks, 1, 4),
            Self::BeatSixth => (beat_ticks, 1, 6),
            Self::BeatQuarter => (beat_ticks, 1, 4),
            Self::BeatThird => (beat_ticks, 1, 3),
            Self::BeatHalf => (beat_ticks, 1, 2),
            Self::BeatTwoThirds => (beat_ticks, 2, 3),
            Self::BeatThreeQuarters => (beat_ticks, 3, 4),
            Self::Beat => (beat_ticks, 1, 1),
            Self::Bar => return Some(playlist_bar_ticks(ppq, signature).max(1)),
        };
        Some(
            base_ticks
                .saturating_mul(numerator)
                .div_ceil(denominator)
                .max(1),
        )
    }

    const fn uses_timing_grid(self) -> bool {
        !matches!(self, Self::None | Self::Line | Self::Cell)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollEventTarget {
    Velocity,
    Pan,
    Release,
    Pitch,
    ModX,
    ModY,
    RawControllers,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PianoRollControllerTool {
    #[default]
    Edit,
    Draw,
    Paint,
    Interpolate,
    Erase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollControllerRangeOperation {
    Linear,
    Spline,
    Erase,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollControllerStroke {
    pattern_id: u16,
    stream: (u8, u8),
    tick: u32,
    value: f32,
    start_value: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollControllerSelectionDrag {
    pattern_id: u16,
    stream: (u8, u8),
    start_tick: u32,
    current_tick: u32,
    operation: PianoRollControllerRangeOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollMouseWheelAction {
    AdjustProperty,
    NudgePosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PianoRollMouseWheelEdit {
    action: PianoRollMouseWheelAction,
    ppq: u16,
    step_delta: i16,
}

impl PianoRollEventTarget {
    const ALL: [Self; 7] = [
        Self::Velocity,
        Self::Pan,
        Self::Release,
        Self::Pitch,
        Self::ModX,
        Self::ModY,
        Self::RawControllers,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Velocity => "Velocity",
            Self::Pan => "Pan",
            Self::Release => "Release",
            Self::Pitch => "Fine pitch",
            Self::ModX => "Mod X",
            Self::ModY => "Mod Y",
            Self::RawControllers => "Raw controllers",
        }
    }

    fn maximum(self) -> u16 {
        match self {
            Self::Velocity | Self::Pan | Self::Release => 128,
            Self::Pitch => 240,
            Self::ModX | Self::ModY => 255,
            Self::RawControllers => unreachable!("raw controllers use their own value scale"),
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
            Self::RawControllers => unreachable!("raw controllers are not note properties"),
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
            Self::RawControllers => unreachable!("raw controllers use PatternControllerEdit"),
        }
    }
}

fn piano_roll_mouse_wheel_step_delta(scroll_delta_y: f32, fine: bool, remainder: &mut f32) -> i16 {
    const POINTS_PER_NOTCH: f32 = 50.0;
    if !scroll_delta_y.is_finite() {
        return 0;
    }
    *remainder += scroll_delta_y;
    let notches = (*remainder / POINTS_PER_NOTCH).trunc() as i16;
    *remainder -= f32::from(notches) * POINTS_PER_NOTCH;
    notches
        .saturating_neg()
        .saturating_mul(if fine { 1 } else { 4 })
}

#[derive(Clone, Copy)]
struct TouchGesture {
    center: egui::Pos2,
    translation: Vec2,
    zoom_factor: f32,
    rotation_delta: f32,
}

#[derive(Clone, Copy)]
struct TouchTapTracker {
    start_time: f64,
    start_center: egui::Pos2,
    last_center: egui::Pos2,
    travel: f32,
    zoom_change: f32,
    rotation_change: f32,
    eligible: bool,
}

#[derive(Clone, Copy)]
struct TouchZoomConfig {
    origin: Vec2,
    current: Vec2,
    min: Vec2,
    max: Vec2,
    horizontal: bool,
    vertical: bool,
}

fn track_two_finger_tap(
    tracker: &mut Option<TouchTapTracker>,
    viewport: egui::Rect,
    body_inset: Vec2,
    touch: Option<egui::MultiTouchInfo>,
    any_touches: bool,
    now: f64,
) -> Option<egui::Pos2> {
    let body = egui::Rect::from_min_max(viewport.min + body_inset, viewport.max);
    if let Some(touch) = touch {
        if tracker.is_none()
            && touch.num_touches == 2
            && body.contains(touch.start_pos)
            && body.contains(touch.center_pos)
        {
            *tracker = Some(TouchTapTracker {
                start_time: touch.start_time,
                start_center: touch.center_pos,
                last_center: touch.center_pos,
                travel: 0.0,
                zoom_change: 0.0,
                rotation_change: 0.0,
                eligible: true,
            });
        }

        if let Some(candidate) = tracker.as_mut() {
            if candidate.start_time != touch.start_time || touch.num_touches != 2 {
                candidate.eligible = false;
            } else {
                candidate.travel += candidate.last_center.distance(touch.center_pos);
                candidate.travel = candidate
                    .travel
                    .max(touch.center_pos.distance(candidate.start_center));
                candidate.zoom_change += if touch.zoom_delta.is_finite() && touch.zoom_delta > 0.0 {
                    touch.zoom_delta.ln().abs()
                } else {
                    f32::INFINITY
                };
                candidate.rotation_change += touch.rotation_delta.abs();
                candidate.last_center = touch.center_pos;
                candidate.eligible &= body.contains(touch.center_pos);
            }
        }
    } else if !any_touches
        && let Some(candidate) = tracker.take()
        && candidate.eligible
        && (0.0..=0.5).contains(&(now - candidate.start_time))
        && candidate.travel <= 12.0
        && candidate.zoom_change <= 0.04
        && candidate.rotation_change <= 0.04
    {
        return Some(candidate.start_center);
    }
    None
}

fn zoom_to_fit(available_span: f32, content_span_at_unit_zoom: f32, min: f32, max: f32) -> f32 {
    if !available_span.is_finite()
        || !content_span_at_unit_zoom.is_finite()
        || content_span_at_unit_zoom <= 0.0
    {
        return min;
    }
    (available_span.max(1.0) / content_span_at_unit_zoom).clamp(min, max)
}

fn touch_pan_zoom_scroll_offset(
    viewport: egui::Rect,
    current_offset: Vec2,
    gesture: TouchGesture,
    zoom_config: TouchZoomConfig,
) -> Option<(Vec2, Vec2)> {
    if !viewport.contains(gesture.center)
        || !zoom_config.current.x.is_finite()
        || zoom_config.current.x <= 0.0
        || !zoom_config.current.y.is_finite()
        || zoom_config.current.y <= 0.0
        || !zoom_config.origin.x.is_finite()
        || !zoom_config.origin.y.is_finite()
    {
        return None;
    }

    let zoom_factor = if gesture.zoom_factor.is_finite() && gesture.zoom_factor > 0.0 {
        gesture.zoom_factor.clamp(0.5, 2.0)
    } else {
        1.0
    };
    let rotation_delta = if gesture.rotation_delta.is_finite() {
        gesture.rotation_delta
    } else {
        0.0
    };
    let rotation_zoom_factor = (rotation_delta * 0.4).exp().clamp(0.5, 2.0);
    let new_zoom = Vec2::new(
        if zoom_config.horizontal {
            (zoom_config.current.x * zoom_factor).clamp(zoom_config.min.x, zoom_config.max.x)
        } else {
            zoom_config.current.x
        },
        if zoom_config.vertical {
            (zoom_config.current.y * rotation_zoom_factor)
                .clamp(zoom_config.min.y, zoom_config.max.y)
        } else {
            zoom_config.current.y
        },
    );
    let viewport_position = gesture.center - viewport.left_top();
    let translation_x = if gesture.translation.x.is_finite() {
        gesture.translation.x
    } else {
        0.0
    };
    let translation_y = if gesture.translation.y.is_finite() {
        gesture.translation.y
    } else {
        0.0
    };
    let previous_viewport_position = viewport_position - Vec2::new(translation_x, translation_y);
    let content_position = zoom_config.origin
        + Vec2::new(
            (current_offset.x + previous_viewport_position.x - zoom_config.origin.x)
                * (new_zoom.x / zoom_config.current.x),
            (current_offset.y + previous_viewport_position.y - zoom_config.origin.y)
                * (new_zoom.y / zoom_config.current.y),
        );
    let new_offset = Vec2::new(
        (content_position.x - viewport_position.x).max(0.0),
        (content_position.y - viewport_position.y).max(0.0),
    );
    if (new_zoom - zoom_config.current).length_sq() <= f32::EPSILON
        && (new_offset - current_offset).length_sq() <= 0.01
    {
        None
    } else {
        Some((new_zoom, new_offset))
    }
}

fn piano_roll_event_target_selector(ui: &mut egui::Ui, target: &mut PianoRollEventTarget) {
    egui::ComboBox::from_id_salt("piano-roll-event-target")
        .selected_text(format!("Target: {}", target.label()))
        .show_ui(ui, |ui| {
            for option in PianoRollEventTarget::ALL {
                ui.selectable_value(target, option, option.label());
            }
        });
}

#[derive(Clone, Copy, Debug)]
struct PianoRollEventEditorGeometry {
    ppq: u16,
    tick_scale: f32,
    keyboard_width: f32,
    horizontal_scroll: f32,
    snap_ticks: u32,
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
enum ArticulatePreset {
    Legato,
    Portato,
    Staccato,
    SmallGap,
    ChopChords,
}

impl ArticulatePreset {
    const ALL: [Self; 5] = [
        Self::Legato,
        Self::Portato,
        Self::Staccato,
        Self::SmallGap,
        Self::ChopChords,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Legato => "Legato",
            Self::Portato => "Portato",
            Self::Staccato => "Staccato",
            Self::SmallGap => "Ensure small gap between notes",
            Self::ChopChords => "Just chop chords",
        }
    }

    fn settings(self) -> (u8, bool, bool) {
        match self {
            Self::Legato => (100, false, false),
            Self::Portato => (90, false, false),
            Self::Staccato => (50, true, false),
            Self::SmallGap => (98, false, false),
            Self::ChopChords => (100, true, true),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollSelectionCommand {
    All,
    Invert,
    Clear,
    Muted,
    RandomOne,
    RandomMore,
    Odd,
    Overlapping,
    Stacked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimeSelectionShift {
    Previous,
    Next,
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
    Rep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepToggleOperation {
    Add,
    Delete,
    Mute,
    Unmute,
}

fn step_toggle_targets(
    notes: &[&PatternNote],
    step_start: u64,
    step_ticks: u64,
    mute_removed_steps: bool,
) -> (Vec<usize>, StepToggleOperation, usize, usize) {
    let step_end = step_start.saturating_add(step_ticks.max(1));
    let mut all_indices = Vec::new();
    let mut active_indices = Vec::new();
    let mut muted_indices = Vec::new();
    for (note_index, note) in notes.iter().enumerate() {
        let note_tick = u64::from(note.position);
        if note_tick < step_start || note_tick >= step_end {
            continue;
        }
        all_indices.push(note_index);
        if note.is_muted() {
            muted_indices.push(note_index);
        } else {
            active_indices.push(note_index);
        }
    }
    let active_count = active_indices.len();
    let muted_count = muted_indices.len();
    if active_count > 0 {
        if mute_removed_steps {
            (
                active_indices,
                StepToggleOperation::Mute,
                active_count,
                muted_count,
            )
        } else {
            (
                all_indices,
                StepToggleOperation::Delete,
                active_count,
                muted_count,
            )
        }
    } else if muted_count > 0 {
        (
            muted_indices,
            StepToggleOperation::Unmute,
            active_count,
            muted_count,
        )
    } else {
        (
            Vec::new(),
            StepToggleOperation::Add,
            active_count,
            muted_count,
        )
    }
}

fn step_toggle_hover_text(
    operation: StepToggleOperation,
    active_count: usize,
    muted_count: usize,
) -> String {
    match operation {
        StepToggleOperation::Add => "Click to add a C5 note to this step".to_owned(),
        StepToggleOperation::Delete => format!(
            "Click to remove {} note(s) from this step",
            active_count + muted_count
        ),
        StepToggleOperation::Mute => format!(
            "Click to mute {active_count} note(s) and keep their note data; click again to restore them"
        ),
        StepToggleOperation::Unmute => {
            format!("Click to restore {muted_count} muted note(s) in this step")
        }
    }
}

impl StepGraphMode {
    const ALL: [Self; 9] = [
        Self::Note,
        Self::Velocity,
        Self::Pan,
        Self::Release,
        Self::FinePitch,
        Self::ModX,
        Self::ModY,
        Self::Shift,
        Self::Rep,
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
            Self::Rep => "Rep",
        }
    }

    fn maximum(self) -> f32 {
        match self {
            Self::Note => 132.0,
            Self::Velocity | Self::Release => 127.0,
            Self::Pan => 128.0,
            Self::FinePitch | Self::ModX | Self::ModY => 255.0,
            Self::Shift => 100.0,
            Self::Rep => 8.0,
        }
    }

    fn center(self) -> Option<f32> {
        match self {
            Self::Pan => Some(64.0),
            Self::FinePitch | Self::ModX | Self::ModY => Some(128.0),
            _ => None,
        }
    }

    fn reset_value(self) -> f32 {
        match self {
            Self::Note => 60.0,
            Self::Velocity => 100.0,
            Self::Pan => 64.0,
            Self::Release | Self::Shift => 0.0,
            Self::Rep => 1.0,
            Self::FinePitch | Self::ModX | Self::ModY => 128.0,
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
            Self::Rep => 1.0,
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
            Self::Rep => PatternNoteEdit::default(),
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

fn step_graph_repeat_note_matches(note: &PatternNote, root: &PatternNote) -> bool {
    note.channel_id == root.channel_id
        && note.flags == root.flags
        && note.length == root.length
        && note.key == root.key
        && note.group == root.group
        && note.fine_pitch == root.fine_pitch
        && note.reserved == root.reserved
        && note.release == root.release
        && note.midi_channel == root.midi_channel
        && note.pan == root.pan
        && note.velocity == root.velocity
        && note.mod_x == root.mod_x
        && note.mod_y == root.mod_y
}

fn step_graph_repeat_count(
    notes: &[(usize, &PatternNote)],
    root_note_index: usize,
    step_start: u64,
    step_ticks: u64,
) -> u8 {
    let Some(root_note) = notes
        .iter()
        .find(|(note_index, _)| *note_index == root_note_index)
        .map(|(_, note)| *note)
    else {
        return 1;
    };
    let step_end = step_start.saturating_add(step_ticks.max(1));
    notes
        .iter()
        .filter(|(_, note)| {
            let position = u64::from(note.position);
            position >= step_start
                && position < step_end
                && step_graph_repeat_note_matches(note, root_note)
        })
        .count()
        .clamp(1, 8) as u8
}

fn step_graph_repeated_notes(
    root_note: &PatternNote,
    step_start: u64,
    step_ticks: u64,
    repeat_count: u8,
) -> Vec<PatternNote> {
    let step_end = step_start
        .saturating_add(step_ticks.max(1))
        .min(u64::from(u32::MAX) + 1);
    let root_position = u64::from(root_note.position).clamp(
        step_start.min(u64::from(u32::MAX)),
        step_end.saturating_sub(1),
    );
    let available_ticks = step_end.saturating_sub(root_position).max(1);
    let repeat_count = usize::from(repeat_count.clamp(1, 8))
        .min(available_ticks.min(usize::MAX as u64) as usize)
        .max(1);
    (0..repeat_count)
        .map(|index| {
            let offset = available_ticks.saturating_mul(index as u64) / repeat_count as u64;
            let mut note = root_note.clone();
            note.position = root_position
                .saturating_add(offset)
                .min(u64::from(u32::MAX)) as u32;
            note
        })
        .collect()
}

fn set_step_graph_note_repeats(
    document: &mut FlpDocument,
    pattern_id: u16,
    channel_id: u16,
    step_start: u32,
    step_ticks: u64,
    value: f32,
) -> Result<(), FlpError> {
    let step_start = u64::from(step_start);
    let step_end = step_start
        .saturating_add(step_ticks.max(1))
        .min(u64::from(u32::MAX) + 1);
    let pattern = document
        .patterns()?
        .into_iter()
        .find(|pattern| pattern.id == pattern_id)
        .ok_or(FlpError::UnsupportedEdit(
            "the target pattern could not be read for the Rep lane",
        ))?;
    let source = pattern
        .notes
        .iter()
        .find(|note| {
            note.channel_id == channel_id
                && u64::from(note.position) >= step_start
                && u64::from(note.position) < step_end
        })
        .cloned()
        .unwrap_or_else(|| PatternNote {
            position: step_start.min(u64::from(u32::MAX)) as u32,
            channel_id,
            length: step_ticks.min(u64::from(u32::MAX)) as u32,
            key: 60,
            velocity: 100,
            ..PatternNote::default()
        });
    let repeat_indices = pattern
        .notes
        .iter()
        .filter(|note| note.channel_id == channel_id)
        .enumerate()
        .filter_map(|(note_index, note)| {
            (u64::from(note.position) >= step_start
                && u64::from(note.position) < step_end
                && step_graph_repeat_note_matches(note, &source))
            .then_some(note_index)
        })
        .collect::<Vec<_>>();
    if !repeat_indices.is_empty() {
        document.delete_pattern_note_selection(pattern_id, channel_id, &repeat_indices)?;
    }
    let repeated_notes = step_graph_repeated_notes(
        &source,
        step_start,
        step_ticks,
        value.round().clamp(1.0, 8.0) as u8,
    );
    document.add_pattern_notes(pattern_id, &repeated_notes)
}

impl PianoRollSnap {
    const ALL: [Self; 10] = [
        Self::None,
        Self::QuarterBeat,
        Self::TripletSixteenth,
        Self::HalfBeat,
        Self::TripletEighth,
        Self::Beat,
        Self::TripletQuarter,
        Self::TwoBeats,
        Self::TripletHalf,
        Self::Bar,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::QuarterBeat => "1/4 beat",
            Self::TripletSixteenth => "1/16 triplet",
            Self::HalfBeat => "1/2 beat",
            Self::TripletEighth => "1/8 triplet",
            Self::Beat => "1 beat",
            Self::TripletQuarter => "1/4 triplet",
            Self::TwoBeats => "2 beats",
            Self::TripletHalf => "1/2 triplet",
            Self::Bar => "1 bar",
        }
    }

    fn ticks(self, ppq: u16, time_signature: Option<(u8, u8)>) -> u32 {
        let ppq = u32::from(ppq).max(1);
        match self {
            Self::None => 1,
            Self::QuarterBeat => (ppq / 4).max(1),
            Self::TripletSixteenth => ppq / 6,
            Self::HalfBeat => (ppq / 2).max(1),
            Self::TripletEighth => ppq / 3,
            Self::Beat => ppq,
            Self::TripletQuarter => ppq.saturating_mul(2) / 3,
            Self::TwoBeats => ppq.saturating_mul(2),
            Self::TripletHalf => ppq.saturating_mul(4) / 3,
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
    MajorLocrian,
    HarmonicMajor,
    DoubleHarmonicMajor,
    IonianSharpFive,
    IonianSharpTwo,
    IonianSharpTwoSharpSix,
    IonianAugmentedSharpTwo,
    Dorian,
    DorianFlatTwo,
    UkrainianDorianFlatTwo,
    DorianSharpFour,
    DorianFlatFive,
    Phrygian,
    PhrygianFlatFour,
    Ultraphrygian,
    UltraphrygianDoubleFlatThree,
    PhrygianDominant,
    Lydian,
    LydianSharpTwo,
    LydianSharpTwoSharpSix,
    LydianSharpThreeSharpSix,
    LydianSharpSix,
    LydianFlatThree,
    LydianAugmentedSharpTwo,
    LydianAugmentedSharpThree,
    LydianAugmentedSharpSix,
    LydianAugmentedDominant,
    LydianAugmented,
    LydianDominant,
    LydianDominantFlatSix,
    Mixolydian,
    MixolydianAugmented,
    MixolydianAugmentedSharpTwo,
    MixolydianFlatTwo,
    MixolydianFlatSix,
    NaturalMinor,
    Locrian,
    LocrianDominant,
    LocrianNaturalTwo,
    LocrianNaturalSix,
    LocrianDoubleFlatSeven,
    LocrianDoubleFlatThreeDoubleFlatSeven,
    UltraLocrianDoubleFlatThree,
    HarmonicMinor,
    HarmonicMinorFlatFive,
    MelodicMinor,
    MelodicMinorSharpFive,
    NeapolitanMinor,
    NeapolitanMinorSharpFour,
    RomanianMinor,
    NeapolitanMajor,
    HungarianMinor,
    HungarianMajor,
    HungarianMajorModeTwo,
    ChromaticHypophrygianInverse,
    Persian,
    Oriental,
    Blues,
    MajorPentatonic,
    MinorPentatonic,
    WholeTone,
    WholeHalfDiminished,
    HalfWholeDiminished,
    HalfDiminishedFlatFour,
    Altered,
    AlteredDominantNaturalSix,
    AlteredDominantDoubleFlatThree,
    SuperLocrianDoubleFlatSeven,
}

impl PianoRollScale {
    const ALL: [Self; 70] = [
        Self::None,
        Self::Major,
        Self::MajorLocrian,
        Self::HarmonicMajor,
        Self::DoubleHarmonicMajor,
        Self::IonianSharpFive,
        Self::IonianSharpTwo,
        Self::IonianSharpTwoSharpSix,
        Self::IonianAugmentedSharpTwo,
        Self::Dorian,
        Self::DorianFlatTwo,
        Self::UkrainianDorianFlatTwo,
        Self::DorianSharpFour,
        Self::DorianFlatFive,
        Self::Phrygian,
        Self::PhrygianFlatFour,
        Self::Ultraphrygian,
        Self::UltraphrygianDoubleFlatThree,
        Self::PhrygianDominant,
        Self::Lydian,
        Self::LydianSharpTwo,
        Self::LydianSharpTwoSharpSix,
        Self::LydianSharpThreeSharpSix,
        Self::LydianSharpSix,
        Self::LydianFlatThree,
        Self::LydianAugmentedSharpTwo,
        Self::LydianAugmentedSharpThree,
        Self::LydianAugmentedSharpSix,
        Self::LydianAugmentedDominant,
        Self::LydianAugmented,
        Self::LydianDominant,
        Self::LydianDominantFlatSix,
        Self::Mixolydian,
        Self::MixolydianAugmented,
        Self::MixolydianAugmentedSharpTwo,
        Self::MixolydianFlatTwo,
        Self::MixolydianFlatSix,
        Self::NaturalMinor,
        Self::Locrian,
        Self::LocrianDominant,
        Self::LocrianNaturalTwo,
        Self::LocrianNaturalSix,
        Self::LocrianDoubleFlatSeven,
        Self::LocrianDoubleFlatThreeDoubleFlatSeven,
        Self::UltraLocrianDoubleFlatThree,
        Self::HarmonicMinor,
        Self::HarmonicMinorFlatFive,
        Self::MelodicMinor,
        Self::MelodicMinorSharpFive,
        Self::NeapolitanMinor,
        Self::NeapolitanMinorSharpFour,
        Self::RomanianMinor,
        Self::NeapolitanMajor,
        Self::HungarianMinor,
        Self::HungarianMajor,
        Self::HungarianMajorModeTwo,
        Self::ChromaticHypophrygianInverse,
        Self::Persian,
        Self::Oriental,
        Self::Blues,
        Self::MajorPentatonic,
        Self::MinorPentatonic,
        Self::WholeTone,
        Self::WholeHalfDiminished,
        Self::HalfWholeDiminished,
        Self::HalfDiminishedFlatFour,
        Self::Altered,
        Self::AlteredDominantNaturalSix,
        Self::AlteredDominantDoubleFlatThree,
        Self::SuperLocrianDoubleFlatSeven,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Major => "Major",
            Self::MajorLocrian => "Major Locrian",
            Self::HarmonicMajor => "Harmonic major",
            Self::DoubleHarmonicMajor => "Double harmonic major",
            Self::IonianSharpFive => "Ionian #5",
            Self::IonianSharpTwo => "Ionian #2",
            Self::IonianSharpTwoSharpSix => "Ionian #2 #6",
            Self::IonianAugmentedSharpTwo => "Ionian augmented #2",
            Self::Dorian => "Dorian",
            Self::DorianFlatTwo => "Dorian b2",
            Self::UkrainianDorianFlatTwo => "Ukrainian Dorian b2",
            Self::DorianSharpFour => "Dorian #4",
            Self::DorianFlatFive => "Dorian b5",
            Self::Phrygian => "Phrygian",
            Self::PhrygianFlatFour => "Phrygian b4",
            Self::Ultraphrygian => "Ultraphrygian",
            Self::UltraphrygianDoubleFlatThree => "Ultraphrygian bb3",
            Self::PhrygianDominant => "Phrygian dominant",
            Self::Lydian => "Lydian",
            Self::LydianSharpTwo => "Lydian #2",
            Self::LydianSharpTwoSharpSix => "Lydian #2 #6",
            Self::LydianSharpThreeSharpSix => "Lydian #3 #6",
            Self::LydianSharpSix => "Lydian #6",
            Self::LydianFlatThree => "Lydian b3",
            Self::LydianAugmentedSharpTwo => "Lydian augmented #2",
            Self::LydianAugmentedSharpThree => "Lydian augmented #3",
            Self::LydianAugmentedSharpSix => "Lydian augmented #6",
            Self::LydianAugmentedDominant => "Lydian augmented dominant",
            Self::LydianAugmented => "Lydian augmented",
            Self::LydianDominant => "Lydian dominant",
            Self::LydianDominantFlatSix => "Lydian dominant b6",
            Self::Mixolydian => "Mixolydian",
            Self::MixolydianAugmented => "Mixolydian augmented",
            Self::MixolydianAugmentedSharpTwo => "Mixolydian augmented #2",
            Self::MixolydianFlatTwo => "Mixolydian b2",
            Self::MixolydianFlatSix => "Mixolydian b6",
            Self::NaturalMinor => "Natural minor",
            Self::Locrian => "Locrian",
            Self::LocrianDominant => "Locrian dominant",
            Self::LocrianNaturalTwo => "Locrian natural 2",
            Self::LocrianNaturalSix => "Locrian natural 6",
            Self::LocrianDoubleFlatSeven => "Locrian bb7",
            Self::LocrianDoubleFlatThreeDoubleFlatSeven => "Locrian bb3 bb7",
            Self::UltraLocrianDoubleFlatThree => "Ultra Locrian bb3",
            Self::HarmonicMinor => "Harmonic minor",
            Self::HarmonicMinorFlatFive => "Harmonic minor b5",
            Self::MelodicMinor => "Melodic minor",
            Self::MelodicMinorSharpFive => "Melodic minor #5",
            Self::NeapolitanMinor => "Neapolitan minor",
            Self::NeapolitanMinorSharpFour => "Neapolitan minor #4",
            Self::RomanianMinor => "Romanian minor",
            Self::NeapolitanMajor => "Neapolitan major",
            Self::HungarianMinor => "Hungarian minor",
            Self::HungarianMajor => "Hungarian major",
            Self::HungarianMajorModeTwo => "Hungarian major mode II",
            Self::ChromaticHypophrygianInverse => "Chromatic Hypophrygian Inverse",
            Self::Persian => "Persian",
            Self::Oriental => "Oriental",
            Self::Blues => "Blues",
            Self::MajorPentatonic => "Major pentatonic",
            Self::MinorPentatonic => "Minor pentatonic",
            Self::WholeTone => "Whole tone",
            Self::WholeHalfDiminished => "Whole-half diminished",
            Self::HalfWholeDiminished => "Half-whole diminished",
            Self::HalfDiminishedFlatFour => "Half-diminished b4",
            Self::Altered => "Altered",
            Self::AlteredDominantNaturalSix => "Altered dominant natural 6",
            Self::AlteredDominantDoubleFlatThree => "Altered dominant bb3",
            Self::SuperLocrianDoubleFlatSeven => "Super-Locrian bb7",
        }
    }

    fn contains(self, key: u16, root: u8) -> bool {
        self == Self::None
            || self
                .intervals()
                .contains(&(((key % 12 + 12 - u16::from(root)) % 12) as u8))
    }

    fn intervals(self) -> &'static [u8] {
        match self {
            Self::None => &[],
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::MajorLocrian => &[0, 2, 4, 5, 6, 8, 10],
            Self::HarmonicMajor => &[0, 2, 4, 5, 7, 8, 11],
            Self::DoubleHarmonicMajor => &[0, 1, 4, 5, 7, 8, 11],
            Self::IonianSharpFive => &[0, 2, 4, 5, 8, 9, 11],
            Self::IonianSharpTwo => &[0, 3, 4, 5, 7, 9, 11],
            Self::IonianSharpTwoSharpSix => &[0, 3, 4, 5, 7, 10, 11],
            Self::IonianAugmentedSharpTwo => &[0, 3, 4, 5, 8, 9, 11],
            Self::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Self::DorianFlatTwo => &[0, 1, 3, 5, 7, 9, 10],
            Self::UkrainianDorianFlatTwo => &[0, 1, 3, 6, 7, 9, 10],
            Self::DorianSharpFour => &[0, 2, 3, 6, 7, 9, 10],
            Self::DorianFlatFive => &[0, 2, 3, 5, 6, 9, 10],
            Self::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            Self::PhrygianFlatFour => &[0, 1, 3, 4, 7, 8, 10],
            Self::Ultraphrygian => &[0, 1, 3, 4, 7, 8, 9],
            Self::UltraphrygianDoubleFlatThree => &[0, 1, 2, 4, 7, 8, 9],
            Self::PhrygianDominant => &[0, 1, 4, 5, 7, 8, 10],
            Self::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            Self::LydianSharpTwo => &[0, 3, 4, 6, 7, 9, 11],
            Self::LydianSharpTwoSharpSix => &[0, 3, 4, 6, 7, 10, 11],
            Self::LydianSharpThreeSharpSix => &[0, 2, 5, 6, 7, 10, 11],
            Self::LydianSharpSix => &[0, 2, 4, 6, 7, 10, 11],
            Self::LydianFlatThree => &[0, 2, 3, 6, 7, 9, 11],
            Self::LydianAugmentedSharpTwo => &[0, 3, 4, 6, 8, 9, 11],
            Self::LydianAugmentedSharpThree => &[0, 2, 5, 6, 8, 9, 11],
            Self::LydianAugmentedSharpSix => &[0, 2, 4, 6, 8, 10, 11],
            Self::LydianAugmentedDominant => &[0, 2, 4, 6, 8, 9, 10],
            Self::LydianAugmented => &[0, 2, 4, 6, 8, 9, 11],
            Self::LydianDominant => &[0, 2, 4, 6, 7, 9, 10],
            Self::LydianDominantFlatSix => &[0, 2, 4, 6, 7, 8, 10],
            Self::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Self::MixolydianAugmented => &[0, 2, 4, 5, 8, 9, 10],
            Self::MixolydianAugmentedSharpTwo => &[0, 3, 4, 5, 8, 9, 10],
            Self::MixolydianFlatTwo => &[0, 1, 4, 5, 7, 9, 10],
            Self::MixolydianFlatSix => &[0, 2, 4, 5, 7, 8, 10],
            Self::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            Self::Locrian => &[0, 1, 3, 5, 6, 8, 10],
            Self::LocrianDominant => &[0, 1, 4, 5, 6, 8, 10],
            Self::LocrianNaturalTwo => &[0, 2, 3, 5, 6, 8, 10],
            Self::LocrianNaturalSix => &[0, 1, 3, 5, 6, 9, 10],
            Self::LocrianDoubleFlatSeven => &[0, 1, 3, 5, 6, 8, 9],
            Self::LocrianDoubleFlatThreeDoubleFlatSeven => &[0, 1, 2, 5, 6, 8, 9],
            Self::UltraLocrianDoubleFlatThree => &[0, 1, 2, 4, 6, 8, 9],
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Self::HarmonicMinorFlatFive => &[0, 2, 3, 5, 6, 8, 11],
            Self::MelodicMinor => &[0, 2, 3, 5, 7, 9, 11],
            Self::MelodicMinorSharpFive => &[0, 2, 3, 5, 8, 9, 11],
            Self::NeapolitanMinor => &[0, 1, 3, 5, 7, 8, 11],
            Self::NeapolitanMinorSharpFour => &[0, 1, 3, 6, 7, 8, 11],
            Self::RomanianMinor => &[0, 2, 3, 6, 7, 8, 10],
            Self::NeapolitanMajor => &[0, 1, 3, 5, 7, 9, 11],
            Self::HungarianMinor => &[0, 2, 3, 6, 7, 8, 11],
            Self::HungarianMajor => &[0, 3, 4, 6, 7, 9, 10],
            Self::HungarianMajorModeTwo => &[0, 1, 3, 4, 6, 7, 9],
            Self::ChromaticHypophrygianInverse => &[0, 1, 2, 5, 6, 7, 9],
            Self::Persian => &[0, 1, 4, 5, 6, 8, 11],
            Self::Oriental => &[0, 1, 4, 5, 6, 9, 10],
            Self::Blues => &[0, 3, 5, 6, 7, 10],
            Self::MajorPentatonic => &[0, 2, 4, 7, 9],
            Self::MinorPentatonic => &[0, 3, 5, 7, 10],
            Self::WholeTone => &[0, 2, 4, 6, 8, 10],
            Self::WholeHalfDiminished => &[0, 2, 3, 5, 6, 8, 9, 11],
            Self::HalfWholeDiminished => &[0, 1, 3, 4, 6, 7, 9, 10],
            Self::HalfDiminishedFlatFour => &[0, 2, 3, 4, 6, 8, 10],
            Self::Altered => &[0, 1, 3, 4, 6, 8, 10],
            Self::AlteredDominantNaturalSix => &[0, 1, 3, 4, 6, 9, 10],
            Self::AlteredDominantDoubleFlatThree => &[0, 1, 2, 4, 6, 8, 10],
            Self::SuperLocrianDoubleFlatSeven => &[0, 1, 3, 4, 6, 8, 9],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DetectedPianoRollScale {
    root: u8,
    scale: PianoRollScale,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PianoRollGhostClipKey {
    arrangement_id: u16,
    clip_index: usize,
    clip_id: Option<u32>,
    pattern_id: u16,
    position_ticks: u32,
    length_ticks: u32,
    track_id: Option<u32>,
}

struct PianoRollNoteView<'a> {
    project_time_signature: Option<(u8, u8)>,
    scale_highlight: Option<DetectedPianoRollScale>,
    transport_playhead_tick: Option<f64>,
    cross_pattern_ghost_notes: &'a [PianoRollGhostNote],
}

fn detect_piano_roll_scale(pitches: &[u16]) -> Option<DetectedPianoRollScale> {
    if pitches.len() < 3 {
        return None;
    }

    let mut pitch_counts = [0usize; 12];
    for pitch in pitches {
        pitch_counts[usize::from(*pitch % 12)] += 1;
    }
    let distinct_pitch_classes = pitch_counts.iter().filter(|count| **count > 0).count();
    if distinct_pitch_classes < 3 {
        return None;
    }

    let first_pitch_class = usize::from(pitches[0] % 12);
    let last_pitch_class = usize::from(*pitches.last()? % 12);
    let scales = [
        PianoRollScale::Major,
        PianoRollScale::MajorLocrian,
        PianoRollScale::HarmonicMajor,
        PianoRollScale::DoubleHarmonicMajor,
        PianoRollScale::IonianSharpFive,
        PianoRollScale::IonianSharpTwo,
        PianoRollScale::IonianSharpTwoSharpSix,
        PianoRollScale::IonianAugmentedSharpTwo,
        PianoRollScale::Dorian,
        PianoRollScale::DorianFlatTwo,
        PianoRollScale::UkrainianDorianFlatTwo,
        PianoRollScale::DorianSharpFour,
        PianoRollScale::DorianFlatFive,
        PianoRollScale::Phrygian,
        PianoRollScale::PhrygianFlatFour,
        PianoRollScale::Ultraphrygian,
        PianoRollScale::UltraphrygianDoubleFlatThree,
        PianoRollScale::PhrygianDominant,
        PianoRollScale::Lydian,
        PianoRollScale::LydianSharpTwo,
        PianoRollScale::LydianSharpTwoSharpSix,
        PianoRollScale::LydianSharpThreeSharpSix,
        PianoRollScale::LydianSharpSix,
        PianoRollScale::LydianFlatThree,
        PianoRollScale::LydianAugmentedSharpTwo,
        PianoRollScale::LydianAugmentedSharpThree,
        PianoRollScale::LydianAugmentedSharpSix,
        PianoRollScale::LydianAugmentedDominant,
        PianoRollScale::LydianAugmented,
        PianoRollScale::LydianDominant,
        PianoRollScale::LydianDominantFlatSix,
        PianoRollScale::Mixolydian,
        PianoRollScale::MixolydianAugmented,
        PianoRollScale::MixolydianAugmentedSharpTwo,
        PianoRollScale::MixolydianFlatTwo,
        PianoRollScale::MixolydianFlatSix,
        PianoRollScale::NaturalMinor,
        PianoRollScale::Locrian,
        PianoRollScale::LocrianDominant,
        PianoRollScale::LocrianNaturalTwo,
        PianoRollScale::LocrianNaturalSix,
        PianoRollScale::LocrianDoubleFlatSeven,
        PianoRollScale::LocrianDoubleFlatThreeDoubleFlatSeven,
        PianoRollScale::UltraLocrianDoubleFlatThree,
        PianoRollScale::HarmonicMinor,
        PianoRollScale::HarmonicMinorFlatFive,
        PianoRollScale::MelodicMinor,
        PianoRollScale::MelodicMinorSharpFive,
        PianoRollScale::NeapolitanMinor,
        PianoRollScale::NeapolitanMinorSharpFour,
        PianoRollScale::RomanianMinor,
        PianoRollScale::NeapolitanMajor,
        PianoRollScale::HungarianMinor,
        PianoRollScale::HungarianMajor,
        PianoRollScale::HungarianMajorModeTwo,
        PianoRollScale::ChromaticHypophrygianInverse,
        PianoRollScale::Persian,
        PianoRollScale::Oriental,
        PianoRollScale::Blues,
        PianoRollScale::MajorPentatonic,
        PianoRollScale::MinorPentatonic,
        PianoRollScale::WholeTone,
        PianoRollScale::WholeHalfDiminished,
        PianoRollScale::HalfWholeDiminished,
        PianoRollScale::HalfDiminishedFlatFour,
        PianoRollScale::Altered,
        PianoRollScale::AlteredDominantNaturalSix,
        PianoRollScale::AlteredDominantDoubleFlatThree,
        PianoRollScale::SuperLocrianDoubleFlatSeven,
    ];
    let mut best: Option<((usize, usize, usize, i8), DetectedPianoRollScale)> = None;

    for scale in scales {
        if matches!(
            scale,
            PianoRollScale::MajorPentatonic | PianoRollScale::MinorPentatonic
        ) && distinct_pitch_classes < 5
        {
            continue;
        }
        if matches!(scale, PianoRollScale::Blues | PianoRollScale::WholeTone)
            && distinct_pitch_classes < 6
        {
            continue;
        }
        if matches!(
            scale,
            PianoRollScale::WholeHalfDiminished | PianoRollScale::HalfWholeDiminished
        ) && distinct_pitch_classes < 8
        {
            continue;
        }
        for root in 0..12u8 {
            let matched_notes = pitches
                .iter()
                .filter(|pitch| scale.contains(**pitch, root))
                .count();
            let matched_pitch_classes = pitch_counts
                .iter()
                .enumerate()
                .filter(|(pitch_class, count)| {
                    **count > 0 && scale.contains(*pitch_class as u16, root)
                })
                .count();
            let scale_completeness = matched_pitch_classes * 100 / scale.intervals().len();
            let tonic_evidence = pitch_counts[usize::from(root)] * 4
                + (if first_pitch_class == usize::from(root) {
                    12
                } else {
                    0
                })
                + (if last_pitch_class == usize::from(root) {
                    6
                } else {
                    0
                });
            let scale_preference = match scale {
                PianoRollScale::Major => 3,
                PianoRollScale::MajorLocrian => -35,
                PianoRollScale::HarmonicMajor => 0,
                PianoRollScale::DoubleHarmonicMajor => -29,
                PianoRollScale::IonianSharpFive => -20,
                PianoRollScale::IonianSharpTwo => -34,
                PianoRollScale::IonianSharpTwoSharpSix => -35,
                PianoRollScale::IonianAugmentedSharpTwo => -30,
                PianoRollScale::DorianFlatTwo => -14,
                PianoRollScale::DorianSharpFour => -19,
                PianoRollScale::DorianFlatFive => -23,
                PianoRollScale::NaturalMinor => 2,
                PianoRollScale::HarmonicMinor => 1,
                PianoRollScale::HarmonicMinorFlatFive => -34,
                PianoRollScale::MelodicMinor => 0,
                PianoRollScale::MelodicMinorSharpFive => -35,
                PianoRollScale::NeapolitanMinor => -27,
                PianoRollScale::NeapolitanMinorSharpFour => -37,
                PianoRollScale::RomanianMinor => -34,
                PianoRollScale::NeapolitanMajor => -28,
                PianoRollScale::HungarianMinor => -30,
                PianoRollScale::HungarianMajor => -32,
                PianoRollScale::HungarianMajorModeTwo => -38,
                PianoRollScale::ChromaticHypophrygianInverse => -38,
                PianoRollScale::Persian => -31,
                PianoRollScale::Oriental => -35,
                PianoRollScale::MajorPentatonic => 0,
                PianoRollScale::MinorPentatonic => -1,
                PianoRollScale::Dorian => -2,
                PianoRollScale::UkrainianDorianFlatTwo => -33,
                PianoRollScale::Phrygian => -3,
                PianoRollScale::PhrygianFlatFour => -24,
                PianoRollScale::Ultraphrygian => -34,
                PianoRollScale::UltraphrygianDoubleFlatThree => -38,
                PianoRollScale::PhrygianDominant => -11,
                PianoRollScale::Lydian => -4,
                PianoRollScale::LydianSharpTwo => -21,
                PianoRollScale::LydianSharpTwoSharpSix => -32,
                PianoRollScale::LydianSharpThreeSharpSix => -37,
                PianoRollScale::LydianSharpSix => -34,
                PianoRollScale::LydianFlatThree => -25,
                PianoRollScale::LydianAugmentedSharpTwo => -26,
                PianoRollScale::LydianAugmentedSharpThree => -37,
                PianoRollScale::LydianAugmentedSharpSix => -36,
                PianoRollScale::LydianAugmentedDominant => -37,
                PianoRollScale::LydianAugmented => -15,
                PianoRollScale::LydianDominant => -12,
                PianoRollScale::LydianDominantFlatSix => -36,
                PianoRollScale::Mixolydian => -5,
                PianoRollScale::MixolydianAugmented => -33,
                PianoRollScale::MixolydianAugmentedSharpTwo => -38,
                PianoRollScale::MixolydianFlatTwo => -24,
                PianoRollScale::MixolydianFlatSix => -16,
                PianoRollScale::Locrian => -6,
                PianoRollScale::LocrianDominant => -34,
                PianoRollScale::LocrianNaturalTwo => -17,
                PianoRollScale::LocrianNaturalSix => -18,
                PianoRollScale::LocrianDoubleFlatSeven => -27,
                PianoRollScale::LocrianDoubleFlatThreeDoubleFlatSeven => -39,
                PianoRollScale::UltraLocrianDoubleFlatThree => -40,
                PianoRollScale::Blues => -7,
                PianoRollScale::WholeTone => -8,
                PianoRollScale::WholeHalfDiminished => -9,
                PianoRollScale::HalfWholeDiminished => -10,
                PianoRollScale::HalfDiminishedFlatFour => -38,
                PianoRollScale::Altered => -13,
                PianoRollScale::AlteredDominantNaturalSix => -36,
                PianoRollScale::AlteredDominantDoubleFlatThree => -40,
                PianoRollScale::SuperLocrianDoubleFlatSeven => -22,
                PianoRollScale::None => unreachable!("None is not a detection candidate"),
            };
            let candidate = DetectedPianoRollScale { root, scale };
            let score = (
                matched_notes,
                scale_completeness,
                tonic_evidence,
                scale_preference,
            );
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, candidate));
            }
        }
    }

    best.map(|(_, detected)| detected)
}

fn snap_piano_roll_key_to_scale(
    key: u16,
    scale: PianoRollScale,
    root: u8,
    minimum_key: u16,
    maximum_key: u16,
) -> u16 {
    let low = minimum_key.min(maximum_key);
    let high = minimum_key.max(maximum_key);
    let clamped_key = key.clamp(low, high);
    (low..=high)
        .filter(|candidate| scale.contains(*candidate, root))
        .min_by_key(|candidate| (candidate.abs_diff(clamped_key), *candidate))
        .unwrap_or(clamped_key)
}

fn snap_recorded_midi_notes_to_scale(notes: &mut [PatternNote], scale: DetectedPianoRollScale) {
    for note in notes {
        note.key = snap_piano_roll_key_to_scale(note.key, scale.scale, scale.root, 0, 127);
    }
}

fn detect_piano_roll_channel_scale(
    pattern: &Pattern,
    pattern_id: u16,
    channel_id: u16,
    selected_notes: &BTreeSet<(u16, u16, usize)>,
) -> Option<DetectedPianoRollScale> {
    let mut channel_notes = Vec::new();
    let mut channel_index = 0usize;
    for (event_index, note) in pattern.notes.iter().enumerate() {
        if note.channel_id != channel_id {
            continue;
        }
        channel_notes.push((channel_index, event_index, note.position, note.key));
        channel_index += 1;
    }

    let selection_exists = channel_notes.iter().any(|(note_index, _, _, _)| {
        selected_notes.contains(&(pattern_id, channel_id, *note_index))
    });
    let mut pitches = channel_notes
        .into_iter()
        .filter(|(note_index, _, _, _)| {
            !selection_exists || selected_notes.contains(&(pattern_id, channel_id, *note_index))
        })
        .map(|(_, event_index, position, key)| (position, event_index, key))
        .collect::<Vec<_>>();
    pitches.sort_by_key(|(position, event_index, _)| (*position, *event_index));
    detect_piano_roll_scale(
        &pitches
            .into_iter()
            .map(|(_, _, key)| key)
            .collect::<Vec<_>>(),
    )
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
    ScaleTriad,
    ScaleSeventh,
    ScaleNinth,
    ScaleEleventh,
    ScaleThirteenth,
    TopDownTriad,
    TopDownSeventh,
    BottomUpTriad,
    BottomUpSeventh,
    SlideUp,
    QuarterPulse,
    EighthPulse,
    SixteenthPulse,
    Backbeat,
    OffbeatEighths,
    ShuffleEighths,
}

impl PianoRollChordStamp {
    const ALL: [Self; 43] = [
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
        Self::ScaleTriad,
        Self::ScaleSeventh,
        Self::ScaleNinth,
        Self::ScaleEleventh,
        Self::ScaleThirteenth,
        Self::TopDownTriad,
        Self::TopDownSeventh,
        Self::BottomUpTriad,
        Self::BottomUpSeventh,
        Self::SlideUp,
        Self::QuarterPulse,
        Self::EighthPulse,
        Self::SixteenthPulse,
        Self::Backbeat,
        Self::OffbeatEighths,
        Self::ShuffleEighths,
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
            Self::ScaleTriad => "Scale triad",
            Self::ScaleSeventh => "Scale seventh",
            Self::ScaleNinth => "Scale ninth",
            Self::ScaleEleventh => "Scale eleventh",
            Self::ScaleThirteenth => "Scale thirteenth",
            Self::TopDownTriad => "Top-down triad",
            Self::TopDownSeventh => "Top-down seventh",
            Self::BottomUpTriad => "Bottom-up triad",
            Self::BottomUpSeventh => "Bottom-up seventh",
            Self::SlideUp => "Slide up",
            Self::QuarterPulse => "Quarter pulse",
            Self::EighthPulse => "Eighth pulse",
            Self::SixteenthPulse => "Sixteenth pulse",
            Self::Backbeat => "Backbeat",
            Self::OffbeatEighths => "Offbeat eighths",
            Self::ShuffleEighths => "Shuffle eighths",
        }
    }

    fn pitches(self, root_key: u16, scale: PianoRollScale, scale_root: u8) -> Vec<u16> {
        if self.is_slide_up() || self.is_percussion() {
            return vec![root_key];
        }
        if let Some(note_count) = self.top_down_note_count() {
            return scale_chord_pitches_from_top(root_key, scale, scale_root, note_count);
        }
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
            Self::ScaleTriad
            | Self::ScaleSeventh
            | Self::ScaleNinth
            | Self::ScaleEleventh
            | Self::ScaleThirteenth
            | Self::BottomUpTriad
            | Self::BottomUpSeventh => &[],
            Self::SlideUp => unreachable!("slide stamps return before chord interval selection"),
            Self::QuarterPulse
            | Self::EighthPulse
            | Self::SixteenthPulse
            | Self::Backbeat
            | Self::OffbeatEighths
            | Self::ShuffleEighths => {
                unreachable!("percussion stamps return before chord interval selection")
            }
            Self::TopDownTriad | Self::TopDownSeventh => unreachable!(),
        };
        if !intervals.is_empty() {
            return intervals
                .iter()
                .filter_map(|interval| root_key.checked_add(u16::from(*interval)))
                .filter(|key| *key <= 127)
                .collect();
        }

        if scale == PianoRollScale::None {
            let intervals: &[u16] = match self {
                Self::ScaleTriad => &[0, 4, 7],
                Self::ScaleSeventh => &[0, 4, 7, 11],
                Self::ScaleNinth => &[0, 4, 7, 11, 14],
                Self::ScaleEleventh => &[0, 4, 7, 11, 14, 17],
                Self::ScaleThirteenth => &[0, 4, 7, 11, 14, 17, 21],
                Self::BottomUpTriad => &[0, 4, 7],
                Self::BottomUpSeventh => &[0, 4, 7, 11],
                Self::SlideUp => unreachable!("slide stamps return before scale selection"),
                Self::QuarterPulse
                | Self::EighthPulse
                | Self::SixteenthPulse
                | Self::Backbeat
                | Self::OffbeatEighths
                | Self::ShuffleEighths => {
                    unreachable!("percussion stamps return before scale selection")
                }
                _ => unreachable!("manual chord stamps return before scale-degree selection"),
            };
            return intervals
                .iter()
                .filter_map(|interval| root_key.checked_add(*interval))
                .filter(|key| *key <= 127)
                .collect();
        }
        let scale_intervals = scale.intervals();

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
        let degrees: &[usize] = match self {
            Self::ScaleTriad => &[0, 2, 4],
            Self::ScaleSeventh => &[0, 2, 4, 6],
            Self::ScaleNinth => &[0, 2, 4, 6, 8],
            Self::ScaleEleventh => &[0, 2, 4, 6, 8, 10],
            Self::ScaleThirteenth => &[0, 2, 4, 6, 8, 10, 12],
            Self::BottomUpTriad => &[0, 2, 4],
            Self::BottomUpSeventh => &[0, 2, 4, 6],
            Self::SlideUp => unreachable!("slide stamps return before scale-degree selection"),
            Self::QuarterPulse
            | Self::EighthPulse
            | Self::SixteenthPulse
            | Self::Backbeat
            | Self::OffbeatEighths
            | Self::ShuffleEighths => {
                unreachable!("percussion stamps return before scale-degree selection")
            }
            _ => unreachable!("manual chord stamps return before scale-degree selection"),
        };
        let scale_anchor = i32::from(snapped_root) - i32::from(scale_intervals[scale_degree]);
        let scale_len = scale_intervals.len();
        degrees
            .iter()
            .filter_map(|degree| {
                let scale_index = scale_degree.checked_add(*degree)?;
                let octave_offset = i32::try_from(scale_index / scale_len).ok()?;
                let key = scale_anchor
                    + octave_offset * 12
                    + i32::from(scale_intervals[scale_index % scale_len]);
                if !(0..=127).contains(&key) {
                    return None;
                }
                u16::try_from(key).ok()
            })
            .collect()
    }

    fn top_down_note_count(self) -> Option<usize> {
        match self {
            Self::TopDownTriad => Some(3),
            Self::TopDownSeventh => Some(4),
            _ => None,
        }
    }

    fn bottom_up_note_count(self) -> Option<usize> {
        match self {
            Self::BottomUpTriad => Some(3),
            Self::BottomUpSeventh => Some(4),
            _ => None,
        }
    }

    fn is_slide_up(self) -> bool {
        self == Self::SlideUp
    }

    fn is_percussion(self) -> bool {
        matches!(
            self,
            Self::QuarterPulse
                | Self::EighthPulse
                | Self::SixteenthPulse
                | Self::Backbeat
                | Self::OffbeatEighths
                | Self::ShuffleEighths
        )
    }

    fn percussion_hits(
        self,
        ppq: u16,
        time_signature: Option<(u8, u8)>,
    ) -> Option<(u32, Vec<(u32, u8)>)> {
        if !self.is_percussion() {
            return None;
        }

        let ppq_ticks = u32::from(ppq.max(1));
        let bar_ticks = PianoRollSnap::Bar.ticks(ppq.max(1), time_signature);
        let eighth_ticks = (ppq_ticks / 2).max(1);
        let sixteenth_ticks = (ppq_ticks / 4).max(1);
        let quarter_positions = (0..bar_ticks).step_by(ppq_ticks as usize);
        let mut hits = Vec::new();

        match self {
            Self::QuarterPulse => {
                hits.extend(quarter_positions.map(|position| (position, 104)));
            }
            Self::EighthPulse => {
                hits.extend(
                    (0..bar_ticks)
                        .step_by(eighth_ticks as usize)
                        .map(|position| {
                            (position, if position % ppq_ticks == 0 { 110 } else { 78 })
                        }),
                );
            }
            Self::SixteenthPulse => {
                hits.extend(
                    (0..bar_ticks)
                        .step_by(sixteenth_ticks as usize)
                        .map(|position| {
                            let velocity = if position % ppq_ticks == 0 {
                                112
                            } else if position % eighth_ticks == 0 {
                                82
                            } else {
                                64
                            };
                            (position, velocity)
                        }),
                );
            }
            Self::Backbeat => {
                hits.extend(
                    quarter_positions
                        .enumerate()
                        .filter(|(beat, _)| beat % 2 == 1)
                        .map(|(_, position)| (position, 112)),
                );
            }
            Self::OffbeatEighths => {
                hits.extend(quarter_positions.filter_map(|position| {
                    let offbeat = position.saturating_add(eighth_ticks);
                    (offbeat < bar_ticks).then_some((offbeat, 82))
                }));
            }
            Self::ShuffleEighths => {
                hits.extend(quarter_positions.flat_map(|position| {
                    let shuffle = position.saturating_add(ppq_ticks.saturating_mul(2) / 3);
                    [(position, 104), (shuffle, 76)]
                        .into_iter()
                        .filter(move |(hit_position, _)| *hit_position < bar_ticks)
                }));
            }
            _ => unreachable!("non-percussion stamp has no rhythm hits"),
        }

        hits.sort_unstable_by_key(|(position, _)| *position);
        hits.dedup_by_key(|(position, _)| *position);
        Some((bar_ticks, hits))
    }
}

fn piano_roll_percussion_stamp_notes(
    stamp: PianoRollChordStamp,
    root_note: &PatternNote,
    ppq: u16,
    time_signature: Option<(u8, u8)>,
    snap_ticks: u32,
) -> Option<Vec<PatternNote>> {
    let (bar_ticks, hits) = stamp.percussion_hits(ppq, time_signature)?;
    let mut notes = Vec::with_capacity(hits.len());
    for (index, (offset_ticks, velocity)) in hits.iter().copied().enumerate() {
        let Some(position) = root_note.position.checked_add(offset_ticks) else {
            continue;
        };
        let next_hit = hits
            .get(index + 1)
            .map(|(next_offset, _)| *next_offset)
            .unwrap_or(bar_ticks);
        let spacing = next_hit.saturating_sub(offset_ticks).max(1);
        let mut note = root_note.clone();
        note.position = position;
        note.length = snap_ticks.max(1).min(spacing);
        note.velocity = velocity;
        notes.push(note);
    }
    Some(notes)
}

fn apply_piano_roll_chord_inversion(pitches: &mut Vec<u16>, inversion: u8) -> bool {
    pitches.sort_unstable();
    if usize::from(inversion) >= pitches.len() && inversion != 0 {
        return false;
    }
    for _ in 0..inversion {
        let Some(raised_pitch) = pitches.first().and_then(|pitch| pitch.checked_add(12)) else {
            return false;
        };
        if raised_pitch > 127 {
            return false;
        }
        pitches.remove(0);
        pitches.push(raised_pitch);
    }
    pitches.sort_unstable();
    true
}

#[derive(Clone, Copy)]
struct PianoRollChordContext<'a> {
    pattern_notes: &'a [PatternNote],
    channel_id: u16,
    position_ticks: u32,
    window_ticks: u32,
}

fn piano_roll_bottom_up_chord_pitches(
    clicked_key: u16,
    scale: PianoRollScale,
    scale_root: u8,
    chord_context: PianoRollChordContext<'_>,
    note_count: usize,
) -> Vec<u16> {
    let stamp = match note_count {
        3 => PianoRollChordStamp::ScaleTriad,
        4 => PianoRollChordStamp::ScaleSeventh,
        _ => return Vec::new(),
    };
    let scale = if scale == PianoRollScale::None {
        PianoRollScale::Major
    } else {
        scale
    };
    let nearest_context_onset = chord_context
        .pattern_notes
        .iter()
        .filter(|note| note.channel_id == chord_context.channel_id)
        .map(|note| note.position)
        .filter(|onset| onset.abs_diff(chord_context.position_ticks) <= chord_context.window_ticks)
        .min_by_key(|onset| (onset.abs_diff(chord_context.position_ticks), *onset));
    let mut nearby_chord = nearest_context_onset
        .into_iter()
        .flat_map(|onset| {
            chord_context
                .pattern_notes
                .iter()
                .filter(move |note| {
                    note.channel_id == chord_context.channel_id && note.position == onset
                })
                .map(|note| note.key)
        })
        .collect::<Vec<_>>();
    nearby_chord.sort_unstable();

    let minimum_root = clicked_key.saturating_sub(12);
    let maximum_root = clicked_key.saturating_add(12).min(127);
    let mut best: Option<(u32, u16, u8, u16, Vec<u16>)> = None;
    for root in minimum_root..=maximum_root {
        if !scale.contains(root, scale_root) {
            continue;
        }
        let base = stamp.pitches(root, scale, scale_root);
        if base.len() != note_count {
            continue;
        }
        for inversion in 0..note_count {
            let mut chord = base.clone();
            if !apply_piano_roll_chord_inversion(&mut chord, inversion as u8) {
                continue;
            }
            for octave_shift in [-24, -12, 0, 12, 24] {
                let Some(voicing) = chord
                    .iter()
                    .map(|pitch| u16::try_from(i32::from(*pitch) + octave_shift).ok())
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let voice_leading_cost = voicing
                    .iter()
                    .zip(&nearby_chord)
                    .map(|(pitch, context_pitch)| u32::from(pitch.abs_diff(*context_pitch)))
                    .sum::<u32>()
                    .saturating_add((voicing.len().abs_diff(nearby_chord.len()) as u32) * 12);
                let score = (
                    voice_leading_cost,
                    root.abs_diff(clicked_key),
                    inversion as u8,
                    root,
                    voicing,
                );
                if best
                    .as_ref()
                    .is_none_or(|current| score.cmp(current).is_lt())
                {
                    best = Some(score);
                }
            }
        }
    }
    best.map(|(_, _, _, _, pitches)| pitches)
        .unwrap_or_else(|| stamp.pitches(clicked_key, scale, scale_root))
}

fn scale_chord_pitches_from_top(
    top_key: u16,
    scale: PianoRollScale,
    scale_root: u8,
    note_count: usize,
) -> Vec<u16> {
    let scale = if scale == PianoRollScale::None {
        PianoRollScale::Major
    } else {
        scale
    };
    let intervals = scale.intervals();
    if intervals.is_empty() || note_count == 0 {
        return Vec::new();
    }
    let Some(top_key) = (0..=127u16)
        .filter(|key| scale.contains(*key, scale_root))
        .min_by_key(|key| key.abs_diff(top_key))
    else {
        return Vec::new();
    };
    let relative_pitch_class =
        match u8::try_from((top_key % 12 + 12 - u16::from(scale_root % 12)) % 12) {
            Ok(pitch_class) => pitch_class,
            Err(_) => return Vec::new(),
        };
    let Some(top_degree) = intervals
        .iter()
        .position(|interval| *interval == relative_pitch_class)
    else {
        return Vec::new();
    };
    let scale_anchor = i32::from(top_key) - i32::from(intervals[top_degree]);
    let Ok(scale_len) = i32::try_from(intervals.len()) else {
        return Vec::new();
    };
    let pitches = (0..note_count)
        .filter_map(|voice| {
            let scale_index =
                i32::try_from(top_degree).ok()? - i32::try_from(voice).ok()?.saturating_mul(2);
            let octave_offset = scale_index.div_euclid(scale_len);
            let degree = usize::try_from(scale_index.rem_euclid(scale_len)).ok()?;
            let key = scale_anchor + octave_offset * 12 + i32::from(intervals[degree]);
            if !(0..=127).contains(&key) {
                return None;
            }
            u16::try_from(key).ok()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if pitches.len() == note_count {
        pitches
    } else {
        Vec::new()
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
    scale_snap: Option<DetectedPianoRollScale>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PianoRollScrubDrag {
    pattern_id: u16,
    channel_id: u16,
    last_tick: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistClipResizeEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PlaylistClipDragKind {
    Move,
    Resize,
    ResizeAudioTicks {
        edge: PlaylistClipResizeEdge,
        source_start_tick: u32,
        source_end_tick: u32,
        sample_length_ticks: u32,
    },
    ResizeAudioMilliseconds {
        edge: PlaylistClipResizeEdge,
        source_start_ms: f32,
        source_end_ms: f32,
        sample_length_ms: f32,
        stored_start_offset: f32,
        stored_end_offset: f32,
    },
    Slip,
    SlipTicks {
        source_start_tick: u32,
        source_end_tick: u32,
        source_length_ticks: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PlaylistClipSlipWindow {
    Milliseconds {
        start_ms: f32,
        end_ms: f32,
        sample_length_ms: f32,
    },
    Ticks {
        start_tick: u32,
        end_tick: u32,
        sample_length_ticks: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ActivePlaylistClipDrag {
    arrangement_id: u16,
    clip_index: usize,
    start_pointer: egui::Pos2,
    start_position_ticks: u32,
    start_length_ticks: u32,
    start_track_index: u16,
    start_source_start_ms: f32,
    start_source_end_ms: f32,
    source_length_ms: f32,
    tempo_bpm: f64,
    kind: PlaylistClipDragKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PlaylistClipDragTarget {
    clip_index: usize,
    start_position_ticks: u32,
    start_length_ticks: u32,
    start_track_index: u16,
    audio_resize_ticks: Option<(u32, u32, u32)>,
    audio_resize_milliseconds: Option<(f32, f32, f32, f32, f32)>,
    audio_slip_window: Option<PlaylistClipSlipWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PlaylistClipDragContext {
    pointer: egui::Pos2,
    tick_scale: f32,
    row_height: f32,
    ppq: u16,
    snap: bool,
    max_track_index: u16,
}

#[derive(Clone, Copy)]
struct PlaylistAudioResizeContext<'a> {
    channel_kinds: &'a BTreeMap<u16, Option<u8>>,
    audio_waveform_paths: &'a BTreeMap<u16, PathBuf>,
    audio_waveforms: &'a BTreeMap<PathBuf, Arc<AudioWaveform>>,
    arrangement_clips: &'a [PlaylistClip],
    tempo_channel_ids: &'a BTreeSet<u16>,
    ppq: u16,
    tempo_bpm: f64,
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
struct PianoRollSliceDrag {
    pattern_id: u16,
    start: egui::Pos2,
    current: egui::Pos2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollDrawDrag {
    pattern_id: u16,
    channel_id: u16,
    start: egui::Pos2,
    current: egui::Pos2,
    kind: PianoRollDrawDragKind,
    unsnapped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PianoRollDrawDragKind {
    Move,
    Length,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollTimeSelectionDrag {
    pattern_id: u16,
    start_tick: u32,
    current_tick: u32,
    additive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PianoRollPitchSelectionDrag {
    pattern_id: u16,
    channel_id: u16,
    start_key: u16,
    current_key: u16,
    additive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct StepGraphRamp {
    pattern_id: u16,
    channel_id: u16,
    mode: StepGraphMode,
    start_step: usize,
    start_value: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct StepGraphScaleValue {
    step: usize,
    note_index: usize,
    start_value: f32,
}

#[derive(Clone, Debug, PartialEq)]
struct StepGraphScale {
    pattern_id: u16,
    channel_id: u16,
    mode: StepGraphMode,
    bar_index: u32,
    start_step: usize,
    start_value: f32,
    last_value: f32,
    values: Vec<StepGraphScaleValue>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct StepGraphReset {
    pattern_id: u16,
    channel_id: u16,
    mode: StepGraphMode,
    bar_index: u32,
    last_step: usize,
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
    Interpolate {
        first_point_index: usize,
        last_point_index: usize,
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
    import_all_tracks: bool,
    selected_midi_tracks: Vec<bool>,
    track_channel_ids: Vec<u16>,
    create_new_pattern: bool,
    start_new_project: bool,
    project_template_path: Option<PathBuf>,
    zero_velocity_note_ons_as_notes: bool,
    realign_events: bool,
    midi_channel_mask: u16,
    set_mixer_tracks_for_new_channels: bool,
}

struct PendingMidiCcImport {
    path: PathBuf,
    midi: MidiFile,
    selected_track: usize,
    controller_number: u8,
    midi_channel: Option<u8>,
    pattern_id: u16,
    target_stream: (u8, u8),
    new_stream: bool,
}

struct PendingMidiCcExport {
    path: PathBuf,
    target: PendingMidiCcExportTarget,
    target_stream: (u8, u8),
    controller_number: u8,
    midi_channel: u8,
}

#[derive(Clone, Copy)]
enum PendingMidiCcExportTarget {
    Pattern(u16),
    Song(u16),
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

#[derive(Clone, Debug)]
struct BrowserSampleDrag(PathBuf);

#[derive(Clone)]
struct FstPresetDetails {
    path: PathBuf,
    preset: FstPreset,
    file_size: usize,
    format: u16,
    kind: FstPresetKind,
    version: String,
    event_count: usize,
    channel_count: usize,
    channel_summaries: Vec<String>,
    plugin_state_count: usize,
    plugin_state_summaries: Vec<String>,
    mixer_insert_count: usize,
    mixer_insert_summaries: Vec<String>,
    automation_channel_count: usize,
    automation_summaries: Vec<String>,
    trailing_byte_count: usize,
}

struct BrowserTagEditor {
    path: PathBuf,
    tags: String,
}

struct BrowserBulkTagEditor {
    paths: Vec<PathBuf>,
    add_tags: String,
    remove_tags: String,
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
            Self::Folder => Some("â–£"),
            Self::Star => Some("â˜…"),
            Self::Music => Some("â™«"),
            Self::Piano => Some("â–¦"),
            Self::Drum => Some("â—‰"),
            Self::Wave => Some("âˆ¿"),
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
    start_fraction: Option<f64>,
    receiver: Receiver<Result<Vec<f32>, String>>,
    worker: thread::JoinHandle<()>,
}

struct PendingSampleEditor {
    path: PathBuf,
    receiver: Receiver<Result<(DecodedAudio, AudioWaveform), String>>,
    worker: thread::JoinHandle<()>,
    recording_notice: Option<String>,
}

struct SampleEditorRecordingSession {
    receiver: Receiver<Result<AudioRecordingSummary, String>>,
    worker: thread::JoinHandle<()>,
    output_path: PathBuf,
    started_at: Instant,
    capture_active: bool,
}

struct SampleEditorState {
    path: PathBuf,
    audio: DecodedAudio,
    waveform: AudioWaveform,
    selection_start: usize,
    selection_end: usize,
    spectral_view: bool,
    selection_drag_anchor: Option<usize>,
    fade_in_seconds: f32,
    fade_out_seconds: f32,
    normalize_export: bool,
    normalization_target_dbfs: f32,
    reverse_export: bool,
}

struct PendingBackupWrite {
    source_path: PathBuf,
    purpose: BackupPurpose,
    plugin_state_warnings: Vec<String>,
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
    is_untitled: bool,
    template_path: Option<PathBuf>,
}

#[derive(Clone)]
enum PendingProjectChange {
    Open(PathBuf),
    NewProject {
        document: Box<FlpDocument>,
        template_path: PathBuf,
        imported: usize,
        destinations: Vec<(u16, u16)>,
    },
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

    const fn settings_key(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::ChannelRack => "channel_rack",
            Self::PianoRoll => "piano_roll",
            Self::Mixer => "mixer",
            Self::Plugins => "plugins",
            Self::Audio => "audio",
            Self::Automation => "automation",
        }
    }

    fn from_settings_key(key: &str) -> Option<Self> {
        match key {
            "playlist" => Some(Self::Playlist),
            "channel_rack" => Some(Self::ChannelRack),
            "piano_roll" => Some(Self::PianoRoll),
            "mixer" => Some(Self::Mixer),
            "plugins" => Some(Self::Plugins),
            "audio" => Some(Self::Audio),
            "automation" => Some(Self::Automation),
            _ => None,
        }
    }
}

struct ActiveMidiPatternPlayback {
    pattern_id: u16,
    started_at: Instant,
    messages: Vec<ScheduledMidiMessage>,
    next_message: usize,
}

struct ActiveMidiPlaylistPlayback {
    arrangement_id: u16,
    scheduler: MidiPlaylistScheduler,
}

struct ActiveMidiPatternTransport {
    pattern_id: u16,
    scheduler: MidiPatternLoopScheduler,
}

struct PerformanceLaunchContext<'a> {
    arrangement: &'a Arrangement,
    tracks: &'a [PlaylistTrack],
    ppq: u16,
    initial_time_signature: Option<(u8, u8)>,
    signature_changes: &'a [(u32, u8, u8)],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MidiHeldPerformanceClip {
    arrangement_id: u16,
    track_id: u32,
    clip_index: usize,
}

type MidiPerformanceHoldMap = BTreeMap<(u8, u8), Vec<MidiHeldPerformanceClip>>;

struct DawUi {
    document: Option<FlpDocument>,
    current_path: Option<PathBuf>,
    new_project_template_path: Option<PathBuf>,
    untitled_autosave_key: Option<PathBuf>,
    package_workspace: Option<ProjectPackageWorkspace>,
    view: MainView,
    ui_scale: f32,
    status: String,
    dirty: bool,
    undo_history: Vec<Vec<u8>>,
    redo_history: Vec<Vec<u8>>,
    pending_history_snapshot: Option<Vec<u8>>,
    saved_project_hash: Option<u64>,
    project_generation: u64,
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
    history_snapshot_available_this_frame: bool,
    playing: bool,
    pattern_playback_mode: bool,
    pattern_mode_playback_source: Option<(u16, u64)>,
    pattern_seek_tick: u64,
    performance_mode_active: bool,
    performance_midi_channel: u8,
    held_performance_clips: Vec<(u16, u32, usize)>,
    midi_held_performance_clips: MidiPerformanceHoldMap,
    midi_performance_page_navigation: bool,
    performance_global_snap: PerformanceGlobalSnap,
    midi_performance_global_snap_mode: bool,
    midi_performance_scene_mode: bool,
    midi_performance_plus_scene_mode: bool,
    performance_scene_round_robin: BTreeMap<(u16, PlaylistPerformanceSceneKey, u32), usize>,
    performance_launch_sender: Option<Sender<PlaylistPerformanceCommand>>,
    project_playback_loaded: bool,
    playlist_playback_loaded: bool,
    playlist_seek_tick: u32,
    playlist_tempo_map: Option<PlaylistTempoMap>,
    tempo_bpm: f64,
    selected_pattern: Option<u16>,
    channel_display_filter: ChannelDisplayFilter,
    channel_group_dialog: Option<ChannelGroupDialog>,
    pattern_rename_dialog: Option<PatternRenameDialog>,
    pattern_delete_dialog: Option<u16>,
    selected_note_channel: Option<u16>,
    selected_note: Option<(u16, u16, usize)>,
    selected_piano_notes: BTreeSet<(u16, u16, usize)>,
    piano_roll_time_selection: Option<(u16, u32, u32)>,
    piano_roll_time_selection_drag: Option<PianoRollTimeSelectionDrag>,
    piano_roll_pitch_selection_drag: Option<PianoRollPitchSelectionDrag>,
    piano_roll_select_mode: bool,
    piano_roll_slice_mode: bool,
    piano_roll_slice_drag: Option<PianoRollSliceDrag>,
    piano_roll_draw_drag: Option<PianoRollDrawDrag>,
    piano_roll_delete_mode: bool,
    piano_roll_mute_mode: bool,
    last_piano_roll_mute_note: Option<(u16, u16, usize)>,
    last_piano_roll_delete_note: Option<(u16, u16, usize)>,
    piano_roll_selection_drag: Option<PianoRollSelectionDrag>,
    piano_roll_zoom_mode: bool,
    piano_roll_zoom_drag: Option<PianoRollZoomDrag>,
    piano_roll_playback_mode: bool,
    piano_roll_stamp_mode: bool,
    piano_roll_stamp_only_one: bool,
    piano_roll_chord_stamp: PianoRollChordStamp,
    piano_roll_chord_inversion: u8,
    piano_roll_scrub_drag: Option<PianoRollScrubDrag>,
    piano_roll_zoom: f32,
    piano_roll_vertical_zoom: f32,
    piano_roll_touch_gesture_active: bool,
    piano_roll_touch_tap: Option<TouchTapTracker>,
    piano_roll_window_detached: bool,
    mixer_window_detached: bool,
    channel_rack_window_detached: bool,
    playlist_window_detached: bool,
    plugins_window_detached: bool,
    audio_window_detached: bool,
    automation_window_detached: bool,
    detached_window_placements: DetachedWindowPlacements,
    detached_window_current_placements: DetachedWindowPlacements,
    window_layout_presets: Vec<WindowLayoutPreset>,
    window_layout_preset_name: String,
    piano_roll_pending_scroll_offset: Option<Vec2>,
    piano_roll_grid_viewport: Option<egui::Rect>,
    piano_roll_grid_scroll_offset: Vec2,
    playlist_touch_gesture_active: bool,
    playlist_touch_tap: Option<TouchTapTracker>,
    playlist_pending_scroll_offset: Option<Vec2>,
    playlist_grid_viewport: Option<egui::Rect>,
    playlist_grid_scroll_offset: Vec2,
    step_sequencer_bar: u32,
    channel_rack_mute_removed_steps: bool,
    step_sequencer_add_note_key: u16,
    selected_graph_channel: Option<u16>,
    selected_rack_channels: BTreeSet<u16>,
    rack_selection_anchor: Option<u16>,
    step_graph_mode: StepGraphMode,
    step_graph_editor_open: bool,
    step_graph_ramp: Option<StepGraphRamp>,
    step_graph_scale: Option<StepGraphScale>,
    step_graph_reset: Option<StepGraphReset>,
    active_note_drag: Option<ActiveNoteDrag>,
    piano_roll_snap: PianoRollSnap,
    piano_roll_mouse_wheel_remainder: f32,
    piano_roll_mouse_wheel_action: Option<PianoRollMouseWheelAction>,
    piano_roll_mouse_wheel_fine: bool,
    piano_roll_nudge_notes_with_mouse_wheel: bool,
    piano_roll_edit_scope: PianoRollEditScope,
    piano_roll_event_editor_open: bool,
    piano_roll_event_target: PianoRollEventTarget,
    piano_roll_controller_stream: Option<(u8, u8)>,
    piano_roll_controller_new_channel: u8,
    piano_roll_controller_new_flags: u8,
    piano_roll_controller_tool: PianoRollControllerTool,
    piano_roll_controller_stroke: Option<PianoRollControllerStroke>,
    piano_roll_controller_selection: Option<PianoRollControllerSelectionDrag>,
    piano_roll_scale: PianoRollScale,
    piano_roll_scale_root: u8,
    piano_roll_auto_scale_highlighting: bool,
    piano_roll_snap_notes_to_scale: bool,
    midi_record_snap_to_scale: bool,
    piano_roll_ghost_channels: bool,
    piano_roll_hidden_ghost_channels: BTreeSet<u16>,
    piano_roll_hidden_ghost_tracks: BTreeSet<(u16, u32)>,
    piano_roll_hidden_ghost_clips: BTreeSet<PianoRollGhostClipKey>,
    piano_roll_editable_ghost_channels: bool,
    piano_roll_color_by_midi_channel: bool,
    piano_roll_note_color_group: u8,
    piano_roll_paint_mode: bool,
    last_painted_note: Option<(u16, u16, u16, u32)>,
    quantize_strength_percent: u8,
    quantize_swing_percent: u8,
    quantize_sensitivity_percent: u8,
    quantize_duration_strength_percent: u8,
    quantize_mode: QuantizeNoteMode,
    chop_divisions: u8,
    strum_spread_ticks: u32,
    strum_descending: bool,
    strum_velocity_amount: i16,
    strum_velocity_tension_percent: i16,
    flam_stroke_ticks: u32,
    flam_time_absolute: bool,
    flam_stroke_ms: f32,
    flam_velocity: u8,
    flam_before: bool,
    flam_group_notes: bool,
    randomizer_seed: u64,
    randomizer_velocity_amount: i16,
    randomizer_pan_amount: i16,
    randomizer_release_amount: i16,
    randomizer_mod_x_amount: i16,
    randomizer_mod_y_amount: i16,
    randomizer_pitch_range: u8,
    randomizer_bipolar: bool,
    randomizer_reset_levels: bool,
    randomizer_snap_to_scale: bool,
    random_note_generation_density_percent: u8,
    random_note_generation_stack: u8,
    random_note_generation_length_variation_percent: i16,
    random_note_generation_merge_repeated_notes: bool,
    random_note_generation_base_octave: u8,
    random_note_generation_range_octaves: u8,
    random_note_generation_chord_map: Option<RandomNoteChordMap>,
    random_note_generation_chord_root: u8,
    random_note_generation_minimum_key: u8,
    random_note_generation_maximum_key: u8,
    random_note_generation_snap_to_scale: bool,
    scale_levels_center_percent: i16,
    scale_levels_tension_percent: i16,
    scale_levels_multiplier_percent: u16,
    scale_levels_offset_percent: i16,
    articulate_multiplier_percent: u8,
    articulate_variation_percent: u8,
    articulate_seed: u64,
    articulate_use_original_lengths: bool,
    articulate_only_with_selection: bool,
    articulate_chop_chords: bool,
    humanize_timing_range_ticks: u32,
    humanize_timing_note_division: Option<u16>,
    humanize_velocity_variation_percent: u8,
    humanize_pan_variation_percent: u8,
    humanize_release_variation_percent: u8,
    humanize_mod_x_variation_percent: u8,
    humanize_mod_y_variation_percent: u8,
    note_limit_minimum_key: u8,
    note_limit_maximum_key: u8,
    note_limit_wrap_to_bottom: bool,
    note_limit_snap_to_scale: bool,
    note_limit_snap_direction: LimitSnapDirection,
    arpeggiator_step_ticks: u32,
    arpeggiator_step_note_division: Option<u16>,
    arpeggiator_range_octaves: u8,
    arpeggiator_gate_percent: u8,
    arpeggiator_range_pattern: ArpeggioRangePattern,
    riff_machine_direction: ArpeggioDirection,
    arpeggiator_sync_mode: ArpeggioSyncMode,
    riff_seed: u64,
    riff_velocity_variation_percent: u8,
    riff_pan_variation_percent: u8,
    riff_release_variation_percent: u8,
    riff_mod_x_variation_percent: u8,
    riff_mod_y_variation_percent: u8,
    riff_pitch_variation_semitones: u8,
    riff_reset_levels: bool,
    riff_bipolar_levels: bool,
    riff_length_multiplier_percent: u8,
    riff_mirror_horizontal: bool,
    riff_preserve_start_times: bool,
    riff_mirror_vertical: bool,
    riff_groove_start_percent: u8,
    riff_groove_sensitivity_percent: u8,
    riff_groove_duration_percent: u8,
    riff_groove_quantize_mode: RiffMachineQuantizeMode,
    claw_period_ticks: u32,
    claw_trash_every: u8,
    claw_time_distortion_percent: i16,
    claw_remove_short_notes: bool,
    claw_stretch_to_compensate: bool,
    piano_roll_slice_position_ticks: u32,
    pending_midi_import: Option<PendingMidiImport>,
    pending_midi_cc_import: Option<PendingMidiCcImport>,
    pending_midi_cc_export: Option<PendingMidiCcExport>,
    midi_channel_mapping: MidiChannelMapping,
    selected_arrangement: Option<u16>,
    selected_clip: Option<usize>,
    selected_playlist_clips: BTreeSet<usize>,
    playlist_selection_anchor: Option<usize>,
    selected_playlist_track: Option<u32>,
    selected_playlist_tracks: BTreeSet<u32>,
    playlist_track_selection_anchor: Option<u32>,
    playlist_track_name_editor: Option<(u32, String)>,
    playlist_clip_context_split: Option<(u16, usize, u32)>,
    playlist_clip_clipboard: Option<PlaylistClipSelectionClipboard>,
    playlist_slip_tool_active: bool,
    playlist_mute_tool_active: bool,
    soloed_playlist_track_range: Option<(u32, u32)>,
    collapsed_playlist_groups: BTreeSet<(u16, u32)>,
    hide_collapsed_playlist_groups: bool,
    playlist_zero_crossing_splits: bool,
    active_playlist_clip_drag: Option<ActivePlaylistClipDrag>,
    active_playlist_clip_drag_targets: Vec<PlaylistClipDragTarget>,
    selected_time_marker: Option<usize>,
    selected_pattern_time_signature: Option<usize>,
    selected_pattern_time_signature_pattern: Option<u16>,
    pattern_time_signature_dialog_open: bool,
    selected_mixer_insert: Option<usize>,
    mixer_master_peak_hold: f32,
    mixer_insert_peak_holds: [f32; MIXER_TRACK_METER_COUNT],
    mixer_track_peak_meters: Arc<MixerTrackPeakMeters>,
    mixer_master_peak_updated_at: Instant,
    new_time_marker_position: u32,
    new_time_marker_is_signature: bool,
    new_time_marker_numerator: u8,
    new_time_marker_denominator: u8,
    new_time_marker_name: String,
    new_pattern_time_signature_position: u32,
    new_pattern_time_signature_numerator: u8,
    new_pattern_time_signature_denominator: u8,
    selected_plugin_state_channel: Option<u16>,
    selected_automation_channel: Option<u16>,
    selected_automation_point: Option<usize>,
    automation_add_mode: bool,
    automation_interpolate_mode: bool,
    automation_interpolation_anchor: Option<usize>,
    automation_snap: bool,
    automation_visible_beats: f64,
    last_plugin_action: Option<String>,
    timeline_zoom: f32,
    playlist_track_zoom: f32,
    playlist_waveform_mode: PlaylistWaveformMode,
    plugin_candidates: Vec<PluginCandidate>,
    clap_inspection_sender: Sender<PathBuf>,
    clap_inspection_receiver: Receiver<ClapInspectionResponse>,
    clap_inspection_states: BTreeMap<PathBuf, ClapInspectionState>,
    clap_instance_inspection_sender: Sender<(PathBuf, String)>,
    clap_instance_inspection_receiver: Receiver<ClapInstanceInspectionResponse>,
    clap_instance_inspection_states: BTreeMap<(PathBuf, String), ClapInstanceInspectionState>,
    clap_note_preview_sender: Sender<ClapNotePreviewRequest>,
    clap_note_preview_receiver: Receiver<ClapNotePreviewResponse>,
    clap_note_preview_states: BTreeMap<(PathBuf, String), ClapNotePreviewState>,
    vst3_host: Option<Vst3HostRuntime>,
    channel_vst3_instances: BTreeMap<u16, u64>,
    audio_catalog: AudioDeviceCatalog,
    audio_settings: AudioSettings,
    live_resampling_quality: ResamplingQuality,
    read_sample_root_note: bool,
    audio_engine: Option<AudioEngine>,
    audio_recording: Option<AudioRecordingSession>,
    audio_recording_offset_ms: i16,
    record_audio_enabled: bool,
    audio_record_start_requested: bool,
    audio_record_stop_requested: bool,
    midi_input_devices: Vec<MidiInputDevice>,
    midi_selected_input_id: Option<String>,
    midi_connect_input_on_startup: bool,
    midi_input_error: Option<String>,
    midi_input_connection: Option<ConnectedMidiInput>,
    midi_message_sender: SyncSender<ReceivedMidiMessage>,
    midi_message_receiver: Receiver<ReceivedMidiMessage>,
    midi_dropped_messages: Arc<AtomicU64>,
    midi_reported_dropped_messages: u64,
    midi_activity: Option<(String, Instant)>,
    midi_input_channel_state: [MidiInputChannelState; 16],
    midi_input_thru_output: bool,
    midi_input_thru_snap_to_scale: bool,
    midi_input_thru_output_channel: Option<u8>,
    midi_thru_worker: MidiThruWorker,
    midi_thru_input_queue: MidiThruInputQueue,
    midi_live_sampler_note_mapping: BTreeMap<(u8, u8), VecDeque<u8>>,
    midi_live_vst3_note_mapping: BTreeMap<(u8, u8), VecDeque<Option<u8>>>,
    midi_live_vst3_route_mapping: BTreeMap<(u8, u8), VecDeque<BTreeSet<u16>>>,
    midi_live_layer_note_mapping: BTreeMap<(u8, u8), VecDeque<u8>>,
    live_input_note_route_seed: u64,
    current_live_input_note_route_seed: Option<u64>,
    midi_live_sampler_sustain_channels: BTreeSet<u8>,
    midi_output_gain_mapping: Option<MidiControlChangeMapping>,
    midi_mixer_control_mappings: Vec<MidiMixerControlMapping>,
    midi_vst3_parameter_mappings: Vec<MidiVst3ParameterMapping>,
    pattern_vst3_parameter_links: Vec<PatternVst3ParameterLink>,
    midi_vst3_parameter_learning: Option<MidiVst3ParameterTarget>,
    midi_mixer_button_cc_active: BTreeSet<(String, u8, u8)>,
    midi_mixer_control_learning: Option<MidiMixerControlLearning>,
    midi_mixer_control_undo_snapshot: Option<Vec<u8>>,
    midi_mixer_control_undo_last_update: Option<Instant>,
    midi_controller_templates: Vec<MidiControllerTemplate>,
    midi_controller_template_index: Option<usize>,
    midi_controller_template_name: String,
    midi_output_gain_learning: bool,
    midi_play_pause_mapping: Option<MidiControlChangeMapping>,
    midi_play_pause_learning: bool,
    midi_play_pause_cc_active: bool,
    midi_recording: Option<ActiveMidiRecording>,
    midi_record_snap: PianoRollSnap,
    midi_recording_offset_ms: i16,
    midi_record_fixed_velocity: bool,
    record_notes_enabled: bool,
    record_automation_enabled: bool,
    midi_record_automation_controller: u8,
    midi_record_automation_channel: u8,
    midi_record_automation_flags: u8,
    midi_record_toggle_requested: bool,
    midi_input_refresh_requested: bool,
    midi_input_toggle_requested: bool,
    midi_output_devices: Vec<MidiOutputDevice>,
    midi_selected_output_id: Option<String>,
    midi_connect_output_on_startup: bool,
    midi_output_error: Option<String>,
    midi_output_connection: Option<ConnectedMidiOutput>,
    midi_output_channel_routes: [Option<String>; 16],
    midi_output_bank_msb: [Option<u8>; 16],
    midi_output_bank_lsb: [Option<u8>; 16],
    midi_output_programs: [Option<u8>; 16],
    midi_output_send_clock: bool,
    midi_output_send_playlist_notes: bool,
    midi_output_send_pattern_notes: bool,
    midi_output_clock: Option<MidiClockScheduler>,
    midi_playlist_playback: Option<ActiveMidiPlaylistPlayback>,
    midi_pattern_transport: Option<ActiveMidiPatternTransport>,
    midi_pattern_playback: Option<ActiveMidiPatternPlayback>,
    midi_pattern_play_requested: bool,
    midi_pattern_stop_requested: bool,
    midi_output_test_note_off_at: Option<Instant>,
    midi_output_refresh_requested: bool,
    midi_output_toggle_requested: bool,
    midi_output_test_requested: bool,
    pending_audio_clip_export: Option<PendingAudioClipExport>,
    pending_audio_render: Option<PendingAudioRender>,
    audio_render_workers: Vec<PendingAudioRender>,
    pending_song_render: Option<PendingSongRender>,
    song_render_workers: Vec<PendingSongRender>,
    pending_sampler_pattern_render: Option<PendingSamplerPatternRender>,
    pending_vst3_stream: Option<Vst3PatternStreamHandle>,
    vst3_workers: Vec<Vst3PatternStreamHandle>,
    pending_sampler_stream: Option<PendingSamplerStream>,
    live_sampler_input: Option<LiveSamplerInputSession>,
    live_sampler_workers: Vec<thread::JoinHandle<()>>,
    live_layer_input: Option<LiveLayerInputSession>,
    live_vst3_input: Option<LiveVst3InputSession>,
    live_playlist_vst3_input: Option<LivePlaylistVst3InputSession>,
    live_vst3_workers: Vec<thread::JoinHandle<()>>,
    sampler_workers: Vec<PendingSamplerStream>,
    audio_waveform_paths: BTreeMap<u16, PathBuf>,
    audio_waveforms: BTreeMap<PathBuf, Arc<AudioWaveform>>,
    waveform_loads: BTreeMap<PathBuf, PendingWaveformLoad>,
    waveform_errors: BTreeMap<PathBuf, String>,
    pending_playlist_audio_drops: VecDeque<PendingPlaylistAudioDrop>,
    browser_tab: BrowserTab,
    browser_filter: BrowserFilter,
    browser_path: PathBuf,
    browser_visible: bool,
    browser_column_width: f32,
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
    browser_bulk_tag_editor: Option<BrowserBulkTagEditor>,
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
    browser_preview_looping: bool,
    browser_preview_volume: f32,
    browser_preview_pending: Option<PendingBrowserPreview>,
    browser_preview_queued: Option<(PathBuf, bool, Option<f64>)>,
    browser_preview_cancelled: bool,
    browser_preview_path: Option<PathBuf>,
    browser_preview_error: Option<String>,
    sample_editor: Option<SampleEditorState>,
    sample_editor_pending: Option<PendingSampleEditor>,
    sample_editor_recording: Option<SampleEditorRecordingSession>,
    audio_test_tone: bool,
    audio_monitor_input: bool,
    project_info_open: bool,
    project_info_title: String,
    project_info_author: String,
    project_info_comments: String,
    project_info_genre: String,
    project_info_web_link: String,
    project_settings_open: bool,
    project_settings_advanced_supported: bool,
    project_settings_play_truncated: bool,
    project_settings_fast_declick: bool,
    project_settings_master_pitch_cents: i16,
    project_settings_master_pitch_changed: bool,
    project_settings_pan_law_raw: u8,
    project_settings_time_signature_enabled: bool,
    project_settings_time_signature_numerator: u8,
    project_settings_time_signature_denominator: u8,
    playlist_render_options_open: bool,
    playlist_render_mixer_stems: bool,
    playlist_render_include_master_stem: bool,
    playlist_render_include_current_stem: bool,
    playlist_render_current_track: Option<usize>,
    playlist_song_output_format: PlaylistSongOutputFormat,
    playlist_render_format: WavSampleFormat,
    playlist_render_flac_bits_per_sample: u8,
    playlist_render_ogg_bitrate_kbps: u16,
    playlist_render_mp3_bitrate_kbps: u16,
    playlist_render_dither_mode: WavDitherMode,
    playlist_render_save_markers: bool,
    playlist_render_save_note_markers: bool,
    playlist_render_quality: ResamplingQuality,
    playlist_render_channel_mode: WavChannelMode,
    playlist_render_tail_seconds: u8,
    playlist_render_include_plugin_tails: bool,
}

struct PendingAudioRender {
    receiver: Receiver<Result<PlaylistRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
    live_midi_senders: BTreeMap<u16, Vst3PlaylistLiveInputSender>,
}

struct PendingAudioClipExport {
    receiver: Receiver<Result<AudioClipRenderSummary, String>>,
    worker: Option<thread::JoinHandle<()>>,
    output_path: PathBuf,
    selected_clip_index: Option<usize>,
    start_from_song_start: bool,
    action: SelectedAudioClipRenderAction,
    source: Option<PendingAudioClipSource>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectedAudioClipRenderAction {
    Export,
    Insert,
    Consolidate,
}

struct PendingAudioClipSource {
    arrangement_id: u16,
    source_clip_index: usize,
    source_channel_id: u16,
    source_position_ticks: u32,
    source_track_index: u16,
    project_generation: u64,
    project_path: PathBuf,
    project_fingerprint: [u8; 16],
}

struct PendingSongRender {
    receiver: Receiver<Result<PlaylistRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
    output_path: PathBuf,
    kind: PendingSongRenderKind,
    track_freeze: Option<PendingPlaylistTrackFreeze>,
    pattern_clip_consolidation: Option<PendingPlaylistPatternClipConsolidation>,
}

struct PendingPlaylistTrackFreeze {
    arrangement_id: u16,
    track_id: u32,
    track_index: u16,
    expected_audio_clip_count: usize,
    project_generation: u64,
    project_path: PathBuf,
    project_fingerprint: [u8; 16],
}

struct PendingPlaylistPatternClipConsolidation {
    arrangement_id: u16,
    source_clip_index: usize,
    pattern_id: u16,
    source_position_ticks: u32,
    source_length_ticks: u32,
    source_track_index: u16,
    project_generation: u64,
    project_path: PathBuf,
    project_fingerprint: [u8; 16],
}

#[derive(Clone, Copy)]
enum PendingSongRenderKind {
    Mix,
    MixerStems,
}

struct PendingSamplerPatternRender {
    receiver: Receiver<Result<SamplerPatternRenderSummary, String>>,
    worker: thread::JoinHandle<()>,
    output_path: PathBuf,
    pattern_id: u16,
    insert_placement: Option<PendingPatternRenderPlacement>,
}

#[derive(Clone, Copy)]
struct PendingPatternRenderPlacement {
    arrangement_id: u16,
    position_ticks: u32,
    track_index: u16,
    project_generation: u64,
    project_fingerprint: [u8; 16],
}

struct PendingSamplerStream {
    receiver: Receiver<Result<SamplerPatternRenderSummary, String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
    live_midi_senders: BTreeMap<u16, Vst3PlaylistLiveInputSender>,
}

struct LiveSamplerInputSession {
    pattern_id: u16,
    channel_id: u16,
    target_channel_ids: BTreeSet<u16>,
    sender: Sender<SamplerLiveNoteEvent>,
    receiver: Receiver<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveLayerInputTargets {
    sampler_channel_ids: BTreeSet<u16>,
    vst3_channel_ids: BTreeSet<u16>,
}

struct LiveLayerInputSession {
    pattern_id: u16,
    channel_id: u16,
    targets: LiveLayerInputTargets,
    sender: Sender<LayerLiveInputEvent>,
    receiver: Receiver<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

struct LiveVst3InputSession {
    pattern_id: u16,
    channel_id: u16,
    target_channel_ids: BTreeSet<u16>,
    sender: Sender<Vst3LiveInputEvent>,
    receiver: Receiver<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

struct LivePlaylistVst3InputSession {
    pattern_id: u16,
    channel_id: u16,
    pattern_mode: bool,
    performance_mode: bool,
    target_channel_ids: BTreeSet<u16>,
    sessions: BTreeMap<u16, Vst3PlaylistLiveInputSession>,
    router: ChannelNoteRouter,
}

fn live_playlist_vst3_note_targets(
    router: &ChannelNoteRouter,
    target_channel_ids: &BTreeSet<u16>,
    source_channel_id: u16,
    route_seed: u64,
    key: u8,
) -> BTreeSet<u16> {
    router
        .targets_for_note(source_channel_id, route_seed, u16::from(key.min(127)))
        .into_iter()
        .filter(|target_id| target_channel_ids.contains(target_id))
        .collect()
}

fn is_performance_clip_trigger_channel_event(
    event: MidiInputEvent,
    performance_channel: u8,
) -> bool {
    let performance_channel = performance_channel & 0x0F;
    match event {
        MidiInputEvent::NoteOn { channel, .. }
        | MidiInputEvent::NoteOff { channel, .. }
        | MidiInputEvent::PolyphonicPressure { channel, .. }
        | MidiInputEvent::ControlChange { channel, .. }
        | MidiInputEvent::ProgramChange { channel, .. }
        | MidiInputEvent::ChannelPressure { channel, .. }
        | MidiInputEvent::PitchBend { channel, .. } => channel & 0x0F == performance_channel,
        MidiInputEvent::TimingClock
        | MidiInputEvent::SongPosition { .. }
        | MidiInputEvent::Transport(_) => false,
    }
}

fn vst3_playlist_midi_event(event: Vst3LiveInputEvent) -> Option<Vst3MidiEvent> {
    let channel = match event {
        Vst3LiveInputEvent::NoteOn { channel, .. }
        | Vst3LiveInputEvent::NoteOff { channel, .. }
        | Vst3LiveInputEvent::ControlChange { channel, .. }
        | Vst3LiveInputEvent::ProgramChange { channel, .. }
        | Vst3LiveInputEvent::PitchBend { channel, .. }
        | Vst3LiveInputEvent::ChannelPressure { channel, .. }
        | Vst3LiveInputEvent::PolyphonicPressure { channel, .. } => {
            Vst3MidiChannel::from_index(channel & 0x0F)?
        }
    };
    Some(match event {
        Vst3LiveInputEvent::NoteOn { key, velocity, .. } => Vst3MidiEvent::NoteOn {
            channel,
            note: key.min(127),
            velocity: velocity.min(127),
        },
        Vst3LiveInputEvent::NoteOff { key, .. } => Vst3MidiEvent::NoteOff {
            channel,
            note: key.min(127),
            velocity: 0,
        },
        Vst3LiveInputEvent::ControlChange {
            controller, value, ..
        } => Vst3MidiEvent::ControlChange {
            channel,
            controller: controller.min(127),
            value: value.min(127),
        },
        Vst3LiveInputEvent::ProgramChange { program, .. } => Vst3MidiEvent::ProgramChange {
            channel,
            program: program.min(127),
        },
        Vst3LiveInputEvent::PitchBend { value, .. } => Vst3MidiEvent::PitchBend {
            channel,
            value: value.min(16_383),
        },
        Vst3LiveInputEvent::ChannelPressure { pressure, .. } => Vst3MidiEvent::ChannelAftertouch {
            channel,
            pressure: pressure.min(127),
        },
        Vst3LiveInputEvent::PolyphonicPressure { key, pressure, .. } => {
            Vst3MidiEvent::PolyAftertouch {
                channel,
                note: key.min(127),
                pressure: pressure.min(127),
            }
        }
    })
}

struct PendingWaveformLoad {
    receiver: Receiver<Result<AudioWaveform, String>>,
    worker: thread::JoinHandle<()>,
}

struct PendingPlaylistAudioDrop {
    path: PathBuf,
    arrangement_id: u16,
    position_ticks: u32,
    track_index: u16,
    project_generation: u64,
}

struct AudioRecordingSession {
    receiver: Receiver<Result<AudioRecordingSummary, String>>,
    worker: thread::JoinHandle<()>,
    output_path: PathBuf,
    arrangement_id: u16,
    start_tick: u32,
    recording_offset_ms: i16,
    tempo_map: PlaylistTempoMap,
    project_generation: u64,
    capture_active: bool,
}

struct ActiveMidiRecording {
    recorder: MidiPatternRecorder,
    fixed_velocity: Option<u8>,
    record_notes: bool,
    automation: Option<MidiAutomationRecording>,
    playlist_recording_target: Option<MidiPlaylistRecordingTarget>,
    scale_snap: Option<DetectedPianoRollScale>,
}

struct MidiAutomationRecording {
    controller: u8,
    stream: (u8, u8),
    points: Vec<(u32, f32)>,
    overflowed: bool,
}

impl MidiAutomationRecording {
    fn new(controller: u8, stream: (u8, u8)) -> Self {
        Self {
            controller: controller.min(127),
            stream,
            points: Vec::new(),
            overflowed: false,
        }
    }

    fn record(&mut self, position: u32, value: u8) {
        let point = (position, f32::from(value.min(127)) / 127.0);
        if let Some(last) = self.points.last_mut() {
            if last.0 == position {
                *last = point;
                return;
            }
            if last.1.to_bits() == point.1.to_bits() {
                return;
            }
        }
        if self.points.len() >= MAX_CAPTURED_PATTERN_CONTROLLER_POINTS {
            self.overflowed = true;
            return;
        }
        self.points.push(point);
    }

    fn normalized_points(&self) -> Vec<(u32, f32)> {
        let mut points = self.points.clone();
        points.sort_by_key(|(position, _)| *position);
        let mut normalized: Vec<(u32, f32)> = Vec::with_capacity(points.len());
        for (position, value) in points {
            if let Some(last) = normalized.last_mut()
                && last.0 == position
            {
                last.1 = value;
            } else if normalized
                .last()
                .is_none_or(|(_, last_value): &(u32, f32)| last_value.to_bits() != value.to_bits())
            {
                normalized.push((position, value));
            }
        }
        normalized
    }
}

fn record_pattern_controller_points(
    document: &mut FlpDocument,
    pattern_id: u16,
    stream: (u8, u8),
    points: &[(u32, f32)],
) -> Result<usize, String> {
    if points.is_empty() {
        return Ok(0);
    }
    let stream_exists = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pattern| pattern.id == pattern_id)
        .ok_or_else(|| "the selected Pattern no longer exists".to_owned())?
        .controllers
        .iter()
        .any(|point| (point.channel, point.flags) == stream);
    if !stream_exists {
        let (position, value) = points[0];
        document
            .create_pattern_controller_stream(pattern_id, stream.0, stream.1, position, value)
            .map_err(|error| error.to_string())?;
    }
    document
        .draw_pattern_controller_points(pattern_id, stream, points)
        .map_err(|error| error.to_string())?;
    Ok(points.len())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiControlChangeMapping {
    input_device_id: String,
    channel: u8,
    controller: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiVst3ParameterTarget {
    project_channel_id: u16,
    plugin_uid: String,
    parameter_id: u32,
    parameter_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiVst3ParameterMapping {
    input_device_id: String,
    channel: u8,
    controller: u8,
    target: MidiVst3ParameterTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PatternVst3ParameterLink {
    project_path: String,
    pattern_id: u16,
    stream_channel: u8,
    stream_flags: u8,
    target: MidiVst3ParameterTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiMixerControlMapping {
    input_device_id: String,
    channel: u8,
    controller: u8,
    insert_ordinal: usize,
    target: MidiMixerControlTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MidiMixerControlTarget {
    Fader,
    Pan,
    StereoSeparation,
    Mute,
    Solo,
    PhaseInvert,
    SwapLeftRight,
    EffectsEnabled,
}

impl MidiMixerControlTarget {
    const fn name(self) -> &'static str {
        match self {
            Self::Fader => "fader",
            Self::Pan => "pan",
            Self::StereoSeparation => "stereo separation",
            Self::Mute => "mute",
            Self::Solo => "solo",
            Self::PhaseInvert => "phase invert",
            Self::SwapLeftRight => "left/right swap",
            Self::EffectsEnabled => "effects enabled",
        }
    }

    fn from_setting(value: &str) -> Option<Self> {
        match value {
            "fader" => Some(Self::Fader),
            "pan" => Some(Self::Pan),
            "stereo-separation" => Some(Self::StereoSeparation),
            "mute" => Some(Self::Mute),
            "solo" => Some(Self::Solo),
            "phase-invert" => Some(Self::PhaseInvert),
            "swap-left-right" => Some(Self::SwapLeftRight),
            "effects-enabled" => Some(Self::EffectsEnabled),
            _ => None,
        }
    }

    const fn setting(self) -> &'static str {
        match self {
            Self::Fader => "fader",
            Self::Pan => "pan",
            Self::StereoSeparation => "stereo-separation",
            Self::Mute => "mute",
            Self::Solo => "solo",
            Self::PhaseInvert => "phase-invert",
            Self::SwapLeftRight => "swap-left-right",
            Self::EffectsEnabled => "effects-enabled",
        }
    }

    fn raw_value(self, cc: u8) -> i32 {
        match self {
            Self::Fader => midi_cc_to_mixer_fader_raw(cc),
            Self::Pan => midi_cc_to_mixer_pan_raw(cc),
            Self::StereoSeparation => midi_cc_to_mixer_stereo_separation_raw(cc),
            Self::Mute
            | Self::Solo
            | Self::PhaseInvert
            | Self::SwapLeftRight
            | Self::EffectsEnabled => i32::from(cc >= 64),
        }
    }

    fn supports_raw_value(self, value: i32) -> bool {
        match self {
            Self::Fader => (0..=16_000).contains(&value),
            Self::Pan => (-6_400..=6_400).contains(&value),
            Self::StereoSeparation => (-64..=64).contains(&value),
            Self::Mute
            | Self::Solo
            | Self::PhaseInvert
            | Self::SwapLeftRight
            | Self::EffectsEnabled => (0..=1).contains(&value),
        }
    }

    const fn is_toggle(self) -> bool {
        matches!(
            self,
            Self::Mute
                | Self::Solo
                | Self::PhaseInvert
                | Self::SwapLeftRight
                | Self::EffectsEnabled
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MidiMixerControlLearning {
    insert_ordinal: usize,
    target: MidiMixerControlTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiControllerCcBinding {
    channel: u8,
    controller: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MidiControllerTemplate {
    name: String,
    output_gain: Option<MidiControllerCcBinding>,
    play_pause: Option<MidiControllerCcBinding>,
    built_in: bool,
}

fn midi_button_press_edge(value: u8, active: &mut bool) -> bool {
    if value < 64 {
        *active = false;
        false
    } else if *active {
        false
    } else {
        *active = true;
        true
    }
}

fn midi_mixer_button_press_edge(
    active_mappings: &mut BTreeSet<(String, u8, u8)>,
    input_device_id: &str,
    channel: u8,
    controller: u8,
    value: u8,
) -> bool {
    let key = (input_device_id.to_owned(), channel, controller);
    let mut active = active_mappings.contains(&key);
    let pressed = midi_button_press_edge(value, &mut active);
    if active {
        active_mappings.insert(key);
    } else {
        active_mappings.remove(&key);
    }
    pressed
}

fn midi_cc_to_mixer_fader_raw(value: u8) -> i32 {
    i32::from(value.min(127)) * 16_000 / 127
}

fn midi_cc_to_mixer_pan_raw(value: u8) -> i32 {
    let value = i32::from(value.min(127));
    if value <= 64 {
        (value - 64) * 100
    } else {
        (value - 64) * 6_400 / 63
    }
}

fn midi_cc_to_mixer_stereo_separation_raw(value: u8) -> i32 {
    let value = i32::from(value.min(127));
    if value <= 64 {
        value - 64
    } else {
        (value - 64) * 64 / 63
    }
}

impl ActiveMidiRecording {
    fn record(&mut self, message: ReceivedMidiMessage) {
        if let Some(automation) = &mut self.automation {
            let position = self.recorder.position_for_message(message);
            if let (
                Some(position),
                MidiInputEvent::ControlChange {
                    controller, value, ..
                },
            ) = (position, message.event)
                && controller == automation.controller
            {
                automation.record(position, value);
            }
        }
        if self.record_notes {
            if let Some(fixed_velocity) = self.fixed_velocity {
                self.recorder
                    .record_with_velocity_override(message, Some(fixed_velocity));
            } else {
                self.recorder.record(message);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct MidiPlaylistClipPlacement {
    arrangement_id: u16,
    start_tick: u32,
}

#[derive(Clone, Copy)]
struct MidiPlaylistClipExtension {
    arrangement_id: u16,
    clip_index: usize,
    position_ticks: u32,
    length_ticks: u32,
    track_index: u16,
}

#[derive(Clone, Copy)]
enum MidiPlaylistRecordingTarget {
    AddClip(MidiPlaylistClipPlacement),
    ExtendClip(MidiPlaylistClipExtension),
}

enum MidiPlaylistRecordingOutcome {
    ClipAdded {
        placement: MidiPlaylistClipPlacement,
        clip_index: usize,
        track_index: u16,
    },
    ClipExtended {
        extension: MidiPlaylistClipExtension,
        new_length_ticks: Option<u32>,
    },
}

fn recolor_piano_roll_notes(
    document: &mut FlpDocument,
    pattern_id: u16,
    target_channel_id: u16,
    selected_notes: &BTreeSet<(u16, u16, usize)>,
    color_group: u8,
) -> Result<usize, String> {
    let patterns = document
        .patterns()
        .map_err(|error| format!("Could not read Pattern notes: {error}"))?;
    let Some(pattern) = patterns.iter().find(|pattern| pattern.id == pattern_id) else {
        return Err(format!("Pattern {pattern_id} does not exist"));
    };

    let mut note_indices_by_channel = BTreeMap::<u16, BTreeSet<usize>>::new();
    for (selected_pattern, channel_id, note_index) in selected_notes {
        if *selected_pattern == pattern_id {
            note_indices_by_channel
                .entry(*channel_id)
                .or_default()
                .insert(*note_index);
        }
    }
    if note_indices_by_channel.is_empty() {
        note_indices_by_channel.insert(
            target_channel_id,
            pattern
                .notes
                .iter()
                .filter(|note| note.channel_id == target_channel_id)
                .enumerate()
                .map(|(note_index, _)| note_index)
                .collect(),
        );
    }
    let note_count = note_indices_by_channel
        .values()
        .map(BTreeSet::len)
        .sum::<usize>();
    if note_count == 0 {
        return Err("There are no notes to recolor".to_owned());
    }

    let mut updated = document.clone();
    for (channel_id, note_indices) in note_indices_by_channel {
        let channel_notes = pattern
            .notes
            .iter()
            .filter(|note| note.channel_id == channel_id)
            .collect::<Vec<_>>();
        for note_index in note_indices {
            let Some(note) = channel_notes.get(note_index) else {
                return Err("The selected note no longer exists".to_owned());
            };
            let midi_channel = (note.midi_channel & 0xf0) | (color_group & 0x0f);
            updated
                .edit_pattern_note(
                    pattern_id,
                    channel_id,
                    note_index,
                    PatternNoteEdit {
                        midi_channel: Some(midi_channel),
                        ..PatternNoteEdit::default()
                    },
                )
                .map_err(|error| format!("Could not change note color: {error}"))?;
        }
    }
    *document = updated;
    Ok(note_count)
}

impl DawUi {
    fn new(creation: &eframe::CreationContext<'_>, initial_project: Option<PathBuf>) -> Self {
        install_ui_fonts(&creation.egui_ctx);
        install_ui_visuals(&creation.egui_ctx);
        let ui_scale = load_ui_scale();
        creation.egui_ctx.set_zoom_factor(ui_scale);
        creation
            .egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
        creation
            .egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(Vec2::new(
                MIN_WINDOW_INNER_WIDTH / ui_scale,
                MIN_WINDOW_INNER_HEIGHT / ui_scale,
            )));
        let plugin_candidates = scan_installed_plugins().candidates;
        let (clap_inspection_sender, clap_inspection_receiver) =
            clap_probe::start_worker(creation.egui_ctx.clone());
        let (clap_instance_inspection_sender, clap_instance_inspection_receiver) =
            clap_probe::start_instance_worker(creation.egui_ctx.clone());
        let (clap_note_preview_sender, clap_note_preview_receiver) =
            clap_probe::start_note_preview_worker(creation.egui_ctx.clone());
        let audio_catalog = enumerate_devices();
        let (midi_message_sender, midi_message_receiver) =
            mpsc::sync_channel(midi_input::MIDI_INPUT_QUEUE_CAPACITY);
        let midi_thru_worker = MidiThruWorker::new();
        let midi_thru_input_queue = midi_thru_worker.input_queue();
        let (midi_input_devices, midi_input_error) = match enumerate_input_devices() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let (midi_output_devices, midi_output_error) = match enumerate_output_devices() {
            Ok(devices) => (devices, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let MidiDeviceSelections {
            input_id: midi_selected_input_id,
            connect_input_on_startup: midi_connect_input_on_startup,
            output_id: midi_selected_output_id,
            connect_output_on_startup: midi_connect_output_on_startup,
            output_channel_routes: midi_output_channel_routes,
            bank_msb: midi_output_bank_msb,
            bank_lsb: midi_output_bank_lsb,
            programs: midi_output_programs,
            send_clock: midi_output_send_clock,
            send_playlist_notes: midi_output_send_playlist_notes,
            send_pattern_notes: midi_output_send_pattern_notes,
            fixed_velocity: midi_record_fixed_velocity,
            performance_channel: performance_midi_channel,
            record_audio: record_audio_enabled,
            record_notes: record_notes_enabled,
            record_automation: record_automation_enabled,
            record_automation_controller: midi_record_automation_controller,
            record_automation_channel: midi_record_automation_channel,
            record_automation_flags: midi_record_automation_flags,
        } = load_midi_device_selections();
        let (midi_input_thru_snap_to_scale, midi_input_thru_output_channel) =
            load_midi_thru_preferences();
        let mut audio_settings = AudioSettings::default();
        if let Some(rate) = audio_catalog.default_sample_rate {
            audio_settings.sample_rate = rate;
        }
        let midi_output_gain_mapping = load_midi_output_gain_mapping();
        let midi_mixer_control_mappings = load_midi_mixer_control_mappings();
        let midi_vst3_parameter_mappings = load_midi_vst3_parameter_mappings();
        let pattern_vst3_parameter_links = load_pattern_vst3_parameter_links();
        let midi_play_pause_mapping = load_midi_play_pause_mapping();
        let midi_controller_templates = load_midi_controller_templates();
        let live_resampling_quality = load_live_resampling_quality();
        let playlist_render_quality = load_playlist_render_quality();
        let detached_window_placements = load_detached_window_placements();
        let window_layout_presets = load_window_layout_presets();
        let read_sample_root_note = load_read_sample_root_note();
        let audio_recording_offset_ms = load_audio_recording_offset_ms();
        let midi_recording_offset_ms = load_midi_recording_offset_ms();
        let piano_roll_nudge_notes_with_mouse_wheel =
            load_piano_roll_nudge_notes_with_mouse_wheel();
        let channel_rack_mute_removed_steps = load_channel_rack_mute_removed_steps();
        let hide_collapsed_playlist_groups = load_hide_collapsed_playlist_groups();
        let playlist_zero_crossing_splits = load_playlist_zero_crossing_splits();
        let browser_path = initial_project
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(default_browser_directory);
        let mut app = Self {
            document: None,
            current_path: None,
            new_project_template_path: None,
            untitled_autosave_key: None,
            package_workspace: None,
            view: MainView::Playlist,
            ui_scale,
            status: "Ready".to_owned(),
            dirty: false,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
            pending_history_snapshot: None,
            saved_project_hash: None,
            project_generation: 0,
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
            history_snapshot_available_this_frame: false,
            playing: false,
            pattern_playback_mode: false,
            pattern_mode_playback_source: None,
            pattern_seek_tick: 0,
            performance_mode_active: false,
            performance_midi_channel,
            held_performance_clips: Vec::new(),
            midi_held_performance_clips: BTreeMap::new(),
            midi_performance_page_navigation: false,
            performance_global_snap: PerformanceGlobalSnap::BeatQuarter,
            midi_performance_global_snap_mode: false,
            midi_performance_scene_mode: false,
            midi_performance_plus_scene_mode: false,
            performance_scene_round_robin: BTreeMap::new(),
            performance_launch_sender: None,
            project_playback_loaded: false,
            playlist_playback_loaded: false,
            playlist_seek_tick: 0,
            playlist_tempo_map: None,
            tempo_bpm: 140.0,
            selected_pattern: None,
            channel_display_filter: ChannelDisplayFilter::All,
            channel_group_dialog: None,
            pattern_rename_dialog: None,
            pattern_delete_dialog: None,
            selected_note_channel: None,
            selected_note: None,
            selected_piano_notes: BTreeSet::new(),
            piano_roll_time_selection: None,
            piano_roll_time_selection_drag: None,
            piano_roll_pitch_selection_drag: None,
            piano_roll_select_mode: false,
            piano_roll_slice_mode: false,
            piano_roll_slice_drag: None,
            piano_roll_draw_drag: None,
            piano_roll_delete_mode: false,
            piano_roll_mute_mode: false,
            last_piano_roll_mute_note: None,
            last_piano_roll_delete_note: None,
            piano_roll_selection_drag: None,
            piano_roll_zoom_mode: false,
            piano_roll_zoom_drag: None,
            piano_roll_playback_mode: false,
            piano_roll_stamp_mode: false,
            piano_roll_stamp_only_one: true,
            piano_roll_chord_stamp: PianoRollChordStamp::Major,
            piano_roll_chord_inversion: 0,
            piano_roll_scrub_drag: None,
            piano_roll_zoom: 0.10,
            piano_roll_vertical_zoom: 1.0,
            piano_roll_touch_gesture_active: false,
            piano_roll_touch_tap: None,
            piano_roll_window_detached: false,
            mixer_window_detached: false,
            channel_rack_window_detached: false,
            playlist_window_detached: false,
            plugins_window_detached: false,
            audio_window_detached: false,
            automation_window_detached: false,
            detached_window_placements,
            detached_window_current_placements: detached_window_placements,
            window_layout_presets,
            window_layout_preset_name: String::new(),
            piano_roll_pending_scroll_offset: None,
            piano_roll_grid_viewport: None,
            piano_roll_grid_scroll_offset: Vec2::ZERO,
            playlist_touch_gesture_active: false,
            playlist_touch_tap: None,
            playlist_pending_scroll_offset: None,
            playlist_grid_viewport: None,
            playlist_grid_scroll_offset: Vec2::ZERO,
            step_sequencer_bar: 0,
            channel_rack_mute_removed_steps,
            step_sequencer_add_note_key: 60,
            selected_graph_channel: None,
            selected_rack_channels: BTreeSet::new(),
            rack_selection_anchor: None,
            step_graph_mode: StepGraphMode::Velocity,
            step_graph_editor_open: false,
            step_graph_ramp: None,
            step_graph_scale: None,
            step_graph_reset: None,
            active_note_drag: None,
            piano_roll_snap: PianoRollSnap::QuarterBeat,
            piano_roll_mouse_wheel_remainder: 0.0,
            piano_roll_mouse_wheel_action: None,
            piano_roll_mouse_wheel_fine: false,
            piano_roll_nudge_notes_with_mouse_wheel,
            piano_roll_edit_scope: PianoRollEditScope::Automatic,
            piano_roll_event_editor_open: false,
            piano_roll_event_target: PianoRollEventTarget::Velocity,
            piano_roll_controller_stream: None,
            piano_roll_controller_new_channel: 0,
            piano_roll_controller_new_flags: 0,
            piano_roll_controller_tool: PianoRollControllerTool::Edit,
            piano_roll_controller_stroke: None,
            piano_roll_controller_selection: None,
            piano_roll_scale: PianoRollScale::Major,
            piano_roll_scale_root: 0,
            piano_roll_auto_scale_highlighting: false,
            piano_roll_snap_notes_to_scale: false,
            midi_record_snap_to_scale: false,
            piano_roll_ghost_channels: true,
            piano_roll_hidden_ghost_channels: BTreeSet::new(),
            piano_roll_hidden_ghost_tracks: BTreeSet::new(),
            piano_roll_hidden_ghost_clips: BTreeSet::new(),
            piano_roll_editable_ghost_channels: false,
            piano_roll_color_by_midi_channel: false,
            piano_roll_note_color_group: 0,
            piano_roll_paint_mode: false,
            last_painted_note: None,
            quantize_strength_percent: 100,
            quantize_swing_percent: 0,
            quantize_sensitivity_percent: 100,
            quantize_duration_strength_percent: 100,
            quantize_mode: QuantizeNoteMode::LeaveDuration,
            chop_divisions: 4,
            strum_spread_ticks: 120,
            strum_descending: false,
            strum_velocity_amount: 0,
            strum_velocity_tension_percent: 0,
            flam_stroke_ticks: 12,
            flam_time_absolute: true,
            flam_stroke_ms: 50.0,
            flam_velocity: 80,
            flam_before: false,
            flam_group_notes: true,
            randomizer_seed: 1,
            randomizer_velocity_amount: 0,
            randomizer_pan_amount: 0,
            randomizer_release_amount: 0,
            randomizer_mod_x_amount: 0,
            randomizer_mod_y_amount: 0,
            randomizer_pitch_range: 0,
            randomizer_bipolar: true,
            randomizer_reset_levels: false,
            randomizer_snap_to_scale: false,
            random_note_generation_density_percent: 100,
            random_note_generation_stack: 1,
            random_note_generation_length_variation_percent: 0,
            random_note_generation_merge_repeated_notes: false,
            random_note_generation_base_octave: 4,
            random_note_generation_range_octaves: 3,
            random_note_generation_chord_map: None,
            random_note_generation_chord_root: 0,
            random_note_generation_minimum_key: 0,
            random_note_generation_maximum_key: 127,
            random_note_generation_snap_to_scale: false,
            scale_levels_center_percent: 0,
            scale_levels_tension_percent: 0,
            scale_levels_multiplier_percent: 100,
            scale_levels_offset_percent: 0,
            articulate_multiplier_percent: 100,
            articulate_variation_percent: 0,
            articulate_seed: 1,
            articulate_use_original_lengths: true,
            articulate_only_with_selection: false,
            articulate_chop_chords: false,
            humanize_timing_range_ticks: 12,
            humanize_timing_note_division: None,
            humanize_velocity_variation_percent: 10,
            humanize_pan_variation_percent: 0,
            humanize_release_variation_percent: 0,
            humanize_mod_x_variation_percent: 0,
            humanize_mod_y_variation_percent: 0,
            note_limit_minimum_key: 36,
            note_limit_maximum_key: 83,
            note_limit_wrap_to_bottom: false,
            note_limit_snap_to_scale: false,
            note_limit_snap_direction: LimitSnapDirection::Up,
            arpeggiator_step_ticks: 24,
            arpeggiator_step_note_division: None,
            arpeggiator_range_octaves: 1,
            arpeggiator_gate_percent: 80,
            arpeggiator_range_pattern: ArpeggioRangePattern::Up,
            riff_machine_direction: ArpeggioDirection::Up,
            arpeggiator_sync_mode: ArpeggioSyncMode::Block,
            riff_seed: 1,
            riff_velocity_variation_percent: 10,
            riff_pan_variation_percent: 0,
            riff_release_variation_percent: 0,
            riff_mod_x_variation_percent: 0,
            riff_mod_y_variation_percent: 0,
            riff_pitch_variation_semitones: 0,
            riff_reset_levels: false,
            riff_bipolar_levels: true,
            riff_length_multiplier_percent: 100,
            riff_mirror_horizontal: false,
            riff_preserve_start_times: false,
            riff_mirror_vertical: false,
            riff_groove_start_percent: 0,
            riff_groove_sensitivity_percent: 50,
            riff_groove_duration_percent: 0,
            riff_groove_quantize_mode: RiffMachineQuantizeMode::LeaveDuration,
            claw_period_ticks: 384,
            claw_trash_every: 4,
            claw_time_distortion_percent: 0,
            claw_remove_short_notes: false,
            claw_stretch_to_compensate: false,
            piano_roll_slice_position_ticks: 0,
            pending_midi_import: None,
            pending_midi_cc_import: None,
            pending_midi_cc_export: None,
            midi_channel_mapping: MidiChannelMapping::PreserveNoteChannels,
            selected_arrangement: None,
            selected_clip: None,
            selected_playlist_clips: BTreeSet::new(),
            playlist_selection_anchor: None,
            selected_playlist_track: None,
            selected_playlist_tracks: BTreeSet::new(),
            playlist_track_selection_anchor: None,
            playlist_track_name_editor: None,
            playlist_clip_context_split: None,
            playlist_clip_clipboard: None,
            playlist_slip_tool_active: false,
            playlist_mute_tool_active: false,
            soloed_playlist_track_range: None,
            collapsed_playlist_groups: BTreeSet::new(),
            hide_collapsed_playlist_groups,
            active_playlist_clip_drag: None,
            active_playlist_clip_drag_targets: Vec::new(),
            selected_time_marker: None,
            selected_pattern_time_signature: None,
            selected_pattern_time_signature_pattern: None,
            pattern_time_signature_dialog_open: false,
            selected_mixer_insert: None,
            mixer_master_peak_hold: 0.0,
            mixer_insert_peak_holds: [0.0; MIXER_TRACK_METER_COUNT],
            mixer_track_peak_meters: Arc::new(MixerTrackPeakMeters::default()),
            mixer_master_peak_updated_at: Instant::now(),
            new_time_marker_position: 0,
            new_time_marker_is_signature: false,
            new_time_marker_numerator: 4,
            new_time_marker_denominator: 4,
            new_time_marker_name: String::new(),
            new_pattern_time_signature_position: 0,
            new_pattern_time_signature_numerator: 4,
            new_pattern_time_signature_denominator: 4,
            selected_plugin_state_channel: None,
            selected_automation_channel: None,
            selected_automation_point: None,
            automation_add_mode: false,
            automation_interpolate_mode: false,
            automation_interpolation_anchor: None,
            automation_snap: true,
            automation_visible_beats: 16.0,
            last_plugin_action: None,
            timeline_zoom: 0.10,
            playlist_track_zoom: 1.0,
            playlist_waveform_mode: PlaylistWaveformMode::Combined,
            playlist_zero_crossing_splits,
            plugin_candidates,
            clap_inspection_sender,
            clap_inspection_receiver,
            clap_inspection_states: BTreeMap::new(),
            clap_instance_inspection_sender,
            clap_instance_inspection_receiver,
            clap_instance_inspection_states: BTreeMap::new(),
            clap_note_preview_sender,
            clap_note_preview_receiver,
            clap_note_preview_states: BTreeMap::new(),
            vst3_host: None,
            channel_vst3_instances: BTreeMap::new(),
            audio_catalog,
            audio_settings,
            live_resampling_quality,
            read_sample_root_note,
            audio_engine: None,
            audio_recording: None,
            audio_recording_offset_ms,
            record_audio_enabled,
            audio_record_start_requested: false,
            audio_record_stop_requested: false,
            midi_input_devices,
            midi_selected_input_id,
            midi_connect_input_on_startup,
            midi_input_error,
            midi_input_connection: None,
            midi_message_sender,
            midi_message_receiver,
            midi_dropped_messages: Arc::new(AtomicU64::new(0)),
            midi_reported_dropped_messages: 0,
            midi_activity: None,
            midi_input_channel_state: [MidiInputChannelState::default(); 16],
            midi_input_thru_output: false,
            midi_input_thru_snap_to_scale,
            midi_input_thru_output_channel,
            midi_thru_worker,
            midi_thru_input_queue,
            midi_live_sampler_note_mapping: BTreeMap::new(),
            midi_live_vst3_note_mapping: BTreeMap::new(),
            midi_live_vst3_route_mapping: BTreeMap::new(),
            midi_live_layer_note_mapping: BTreeMap::new(),
            live_input_note_route_seed: 0,
            current_live_input_note_route_seed: None,
            midi_live_sampler_sustain_channels: BTreeSet::new(),
            midi_output_gain_mapping,
            midi_mixer_control_mappings,
            midi_vst3_parameter_mappings,
            pattern_vst3_parameter_links,
            midi_vst3_parameter_learning: None,
            midi_mixer_button_cc_active: BTreeSet::new(),
            midi_mixer_control_learning: None,
            midi_mixer_control_undo_snapshot: None,
            midi_mixer_control_undo_last_update: None,
            midi_controller_templates,
            midi_controller_template_index: None,
            midi_controller_template_name: String::new(),
            midi_output_gain_learning: false,
            midi_play_pause_mapping,
            midi_play_pause_learning: false,
            midi_play_pause_cc_active: false,
            midi_recording: None,
            midi_record_snap: PianoRollSnap::None,
            midi_recording_offset_ms,
            midi_record_fixed_velocity,
            record_notes_enabled,
            record_automation_enabled,
            midi_record_automation_controller,
            midi_record_automation_channel,
            midi_record_automation_flags,
            midi_record_toggle_requested: false,
            midi_input_refresh_requested: false,
            midi_input_toggle_requested: false,
            midi_output_devices,
            midi_selected_output_id,
            midi_connect_output_on_startup,
            midi_output_error,
            midi_output_connection: None,
            midi_output_channel_routes,
            midi_output_bank_msb,
            midi_output_bank_lsb,
            midi_output_programs,
            midi_output_send_clock,
            midi_output_send_playlist_notes,
            midi_output_send_pattern_notes,
            midi_output_clock: None,
            midi_playlist_playback: None,
            midi_pattern_transport: None,
            midi_pattern_playback: None,
            midi_pattern_play_requested: false,
            midi_pattern_stop_requested: false,
            midi_output_test_note_off_at: None,
            midi_output_refresh_requested: false,
            midi_output_toggle_requested: false,
            midi_output_test_requested: false,
            pending_audio_clip_export: None,
            pending_audio_render: None,
            audio_render_workers: Vec::new(),
            pending_song_render: None,
            song_render_workers: Vec::new(),
            pending_sampler_pattern_render: None,
            pending_vst3_stream: None,
            vst3_workers: Vec::new(),
            pending_sampler_stream: None,
            live_sampler_input: None,
            live_sampler_workers: Vec::new(),
            live_layer_input: None,
            live_vst3_input: None,
            live_playlist_vst3_input: None,
            live_vst3_workers: Vec::new(),
            sampler_workers: Vec::new(),
            audio_waveform_paths: BTreeMap::new(),
            audio_waveforms: BTreeMap::new(),
            waveform_loads: BTreeMap::new(),
            waveform_errors: BTreeMap::new(),
            pending_playlist_audio_drops: VecDeque::new(),
            browser_tab: BrowserTab::Files,
            browser_filter: BrowserFilter::All,
            browser_path,
            browser_visible: load_browser_visibility(),
            browser_column_width: load_browser_column_width(),
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
            browser_bulk_tag_editor: None,
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
            browser_preview_looping: false,
            browser_preview_volume: 1.0,
            browser_preview_pending: None,
            browser_preview_queued: None,
            browser_preview_cancelled: false,
            browser_preview_path: None,
            browser_preview_error: None,
            sample_editor: None,
            sample_editor_pending: None,
            sample_editor_recording: None,
            audio_test_tone: false,
            audio_monitor_input: false,
            project_info_open: false,
            project_info_title: String::new(),
            project_info_author: String::new(),
            project_info_comments: String::new(),
            project_info_genre: String::new(),
            project_info_web_link: String::new(),
            project_settings_open: false,
            project_settings_advanced_supported: false,
            project_settings_play_truncated: false,
            project_settings_fast_declick: false,
            project_settings_master_pitch_cents: 0,
            project_settings_master_pitch_changed: false,
            project_settings_pan_law_raw: 0,
            project_settings_time_signature_enabled: false,
            project_settings_time_signature_numerator: 4,
            project_settings_time_signature_denominator: 4,
            playlist_render_options_open: false,
            playlist_render_mixer_stems: false,
            playlist_render_include_master_stem: true,
            playlist_render_include_current_stem: false,
            playlist_render_current_track: None,
            playlist_song_output_format: PlaylistSongOutputFormat::Wav,
            playlist_render_format: WavSampleFormat::Float32,
            playlist_render_flac_bits_per_sample: 24,
            playlist_render_ogg_bitrate_kbps: 192,
            playlist_render_mp3_bitrate_kbps: 192,
            playlist_render_dither_mode: WavDitherMode::Off,
            playlist_render_save_markers: false,
            playlist_render_save_note_markers: false,
            playlist_render_quality,
            playlist_render_channel_mode: WavChannelMode::Stereo,
            playlist_render_tail_seconds: 0,
            playlist_render_include_plugin_tails: false,
        };
        if let Some((autosave_minutes, autosave_before_risky, backup_retention)) =
            load_autosave_settings()
        {
            app.autosave_minutes = autosave_minutes;
            app.autosave_before_risky = autosave_before_risky;
            app.backup_retention = backup_retention;
        }
        app.refresh_browser_directory();
        if should_auto_connect_midi_input(
            app.midi_connect_input_on_startup,
            app.midi_selected_input_id.as_deref(),
            &app.midi_input_devices,
        ) {
            app.connect_selected_midi_input();
        }
        if should_auto_connect_midi_output(
            app.midi_connect_output_on_startup,
            app.midi_selected_output_id.as_deref(),
            &app.midi_output_devices,
        ) {
            app.connect_selected_midi_output();
        }
        if let Some(path) = initial_project.as_deref() {
            app.open_project(path);
        } else if let Some(prompt) = latest_recovery_prompt() {
            app.last_autosave_path = Some(prompt.backup_path.clone());
            if prompt.is_untitled {
                app.untitled_autosave_key = Some(prompt.source_path.clone());
            }
            app.recovery_prompt = Some(prompt);
        }
        app
    }

    fn capture_detached_window_placement(
        &mut self,
        kind: DetachedWindowKind,
        context: &egui::Context,
        inner_size: Vec2,
    ) {
        let viewport = context.input(|input| input.viewport().clone());
        let position = if cfg!(target_os = "macos") {
            viewport.inner_rect.map(|rect| rect.min)
        } else {
            viewport
                .outer_rect
                .or(viewport.inner_rect)
                .map(|rect| rect.min)
        }
        .or_else(|| {
            self.detached_window_placements
                .get(kind)
                .and_then(|placement| placement.position)
        });
        let placement = DetachedWindowPlacement {
            position,
            inner_size,
        };
        if placement.is_valid(kind) {
            self.detached_window_current_placements.set(kind, placement);
        }
    }

    fn persist_detached_window_placements(&mut self) {
        for kind in DetachedWindowKind::ALL {
            if let Some(placement) = self.detached_window_current_placements.get(kind) {
                self.detached_window_placements.set(kind, placement);
            }
        }
        if let Err(error) = save_detached_window_placements(self.detached_window_placements) {
            self.status = format!("Could not save detached window layout: {error}");
        }
    }

    fn dock_all_and_reset_detached_window_positions(&mut self, context: &egui::Context) {
        self.piano_roll_window_detached = false;
        self.mixer_window_detached = false;
        self.channel_rack_window_detached = false;
        self.playlist_window_detached = false;
        self.plugins_window_detached = false;
        self.audio_window_detached = false;
        self.automation_window_detached = false;

        let (main_window_position, available_size) = context.input(|input| {
            let viewport = input.viewport();
            let window_rect = viewport.outer_rect.or(viewport.inner_rect);
            (
                window_rect.map_or(egui::Pos2::ZERO, |rect| rect.min),
                viewport
                    .monitor_size
                    .or_else(|| window_rect.map(|rect| rect.size())),
            )
        });
        let mut placements = self.detached_window_placements;
        for (index, kind) in DetachedWindowKind::ALL.into_iter().enumerate() {
            if let Some(mut placement) = self
                .detached_window_current_placements
                .get(kind)
                .or_else(|| placements.get(kind))
            {
                let cascade = index as f32 * 8.0;
                placement.position = Some(egui::pos2(
                    main_window_position.x + 480.0 + cascade,
                    main_window_position.y + 48.0 + cascade,
                ));
                if let Some(available_size) = available_size {
                    let minimum_size = kind.minimum_size();
                    placement.inner_size = Vec2::new(
                        placement
                            .inner_size
                            .x
                            .min((available_size.x - 544.0).max(minimum_size.x)),
                        placement
                            .inner_size
                            .y
                            .min((available_size.y - 112.0).max(minimum_size.y)),
                    );
                }
                placements.set(kind, placement);
            }
        }
        self.detached_window_placements = placements;
        self.detached_window_current_placements = placements;
        self.status = match save_detached_window_placements(placements) {
            Ok(()) => {
                "All detached views docked; window positions reset near the main window".to_owned()
            }
            Err(error) => {
                format!("Detached views docked, but window positions could not be reset: {error}")
            }
        };
    }

    fn capture_window_layout_preset(&self, name: String) -> WindowLayoutPreset {
        let mut placements = self.detached_window_placements;
        for kind in DetachedWindowKind::ALL {
            if let Some(placement) = self.detached_window_current_placements.get(kind) {
                placements.set(kind, placement);
            }
        }
        WindowLayoutPreset {
            name,
            main_view: self.view,
            browser_visible: self.browser_visible,
            detached: [
                self.playlist_window_detached,
                self.channel_rack_window_detached,
                self.piano_roll_window_detached,
                self.mixer_window_detached,
                self.plugins_window_detached,
                self.audio_window_detached,
                self.automation_window_detached,
            ],
            placements,
        }
    }

    fn save_window_layout_preset(&mut self) {
        let name = self.window_layout_preset_name.trim().to_owned();
        let preset = self.capture_window_layout_preset(name);
        if !preset.is_valid() {
            self.status = format!(
                "Layout name must contain 1â€“{WINDOW_LAYOUT_PRESET_NAME_LIMIT} characters and no control characters"
            );
            return;
        }

        let mut presets = self.window_layout_presets.clone();
        if let Some(existing) = presets
            .iter_mut()
            .find(|existing| existing.name.eq_ignore_ascii_case(&preset.name))
        {
            *existing = preset.clone();
        } else if presets.len() >= WINDOW_LAYOUT_PRESET_LIMIT {
            self.status =
                format!("At most {WINDOW_LAYOUT_PRESET_LIMIT} window layout presets can be saved");
            return;
        } else {
            presets.push(preset.clone());
        }
        presets.sort_by_key(|item| item.name.to_lowercase());

        if let Err(error) = save_window_layout_presets(&presets) {
            self.status = format!("Could not save window layout preset: {error}");
            return;
        }
        self.window_layout_presets = presets;
        self.detached_window_placements = preset.placements;
        self.detached_window_current_placements = preset.placements;
        self.window_layout_preset_name.clear();
        self.status = match save_detached_window_placements(preset.placements) {
            Ok(()) => format!("Saved window layout preset â€˜{}â€™", preset.name),
            Err(error) => format!(
                "Saved window layout preset â€˜{}â€™, but could not save default window positions: {error}",
                preset.name
            ),
        };
    }

    fn apply_window_layout_preset(&mut self, preset: WindowLayoutPreset) {
        self.view = preset.main_view;
        self.browser_visible = preset.browser_visible;
        self.playlist_window_detached = preset.detached[DetachedWindowKind::Playlist.index()];
        self.channel_rack_window_detached =
            preset.detached[DetachedWindowKind::ChannelRack.index()];
        self.piano_roll_window_detached = preset.detached[DetachedWindowKind::PianoRoll.index()];
        self.mixer_window_detached = preset.detached[DetachedWindowKind::Mixer.index()];
        self.plugins_window_detached = preset.detached[DetachedWindowKind::Plugins.index()];
        self.audio_window_detached = preset.detached[DetachedWindowKind::Audio.index()];
        self.automation_window_detached = preset.detached[DetachedWindowKind::Automation.index()];
        self.detached_window_placements = preset.placements;
        self.detached_window_current_placements = preset.placements;

        let browser_result = save_browser_visibility(preset.browser_visible);
        let placements_result = save_detached_window_placements(preset.placements);
        self.status = match (browser_result, placements_result) {
            (Ok(()), Ok(())) => format!("Applied window layout preset â€˜{}â€™", preset.name),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => {
                format!(
                    "Applied window layout preset â€˜{}â€™, but could not save settings: {error}",
                    preset.name
                )
            }
            (Err(browser_error), Err(placement_error)) => format!(
                "Applied window layout preset â€˜{}â€™, but could not save Browser settings ({browser_error}) or window positions ({placement_error})",
                preset.name
            ),
        };
    }

    fn delete_window_layout_preset(&mut self, name: &str) {
        let mut presets = self.window_layout_presets.clone();
        let original_len = presets.len();
        presets.retain(|preset| !preset.name.eq_ignore_ascii_case(name));
        if presets.len() == original_len {
            return;
        }
        if let Err(error) = save_window_layout_presets(&presets) {
            self.status = format!("Could not delete window layout preset: {error}");
            return;
        }
        self.window_layout_presets = presets;
        self.status = format!("Deleted window layout preset â€˜{name}â€™");
    }

    fn show_detached_auxiliary_view(
        &mut self,
        kind: DetachedWindowKind,
        ui: &mut egui::Ui,
        pointer_down: &mut bool,
        history_navigation: &mut bool,
        frame_snapshot: &mut Option<Vec<u8>>,
    ) {
        let context = ui.ctx().clone();
        self.capture_detached_window_placement(kind, &context, ui.max_rect().size());
        if context.input(|input| input.viewport().close_requested()) {
            self.persist_detached_window_placements();
            match kind {
                DetachedWindowKind::Plugins => self.plugins_window_detached = false,
                DetachedWindowKind::Audio => self.audio_window_detached = false,
                DetachedWindowKind::Automation => self.automation_window_detached = false,
                _ => {}
            }
            return;
        }

        context.set_theme(egui::Theme::Dark);
        ui.set_style(context.global_style());
        *ui.visuals_mut() = app_visuals();
        ui.painter()
            .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, APP_BACKGROUND);

        let child_history_navigation = !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| {
                input.consume_key(
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    egui::Key::Z,
                ) || input.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)
            });
        let child_undo_requested = !child_history_navigation
            && !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z));
        if child_history_navigation {
            self.redo_document();
            *history_navigation = true;
        } else if child_undo_requested {
            self.undo_document();
            *history_navigation = true;
        }
        let child_save_as_requested = !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| {
                input.consume_key(
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    egui::Key::S,
                )
            });
        let child_save_version_requested = !child_save_as_requested
            && !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::N));
        let child_save_requested = !child_save_as_requested
            && !child_save_version_requested
            && !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        if child_save_as_requested {
            self.save_as();
        } else if child_save_version_requested {
            self.save_new_version();
        } else if child_save_requested {
            self.save();
        }
        let child_play_pause_requested = !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        if child_play_pause_requested {
            self.toggle_project_playback();
        }
        let child_pattern_mode_toggle_requested = !context.egui_wants_keyboard_input()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::L));
        if child_pattern_mode_toggle_requested {
            self.set_pattern_playback_mode(!self.pattern_playback_mode);
        }

        *pointer_down |= ui.input(|input| {
            input.pointer.primary_down() || input.pointer.button_down(PointerButton::Secondary)
        });
        let child_edit_input_received = ui.input(|input| {
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
                            | egui::Event::MouseWheel { .. }
                    )
                })
        });
        if !*history_navigation
            && frame_snapshot.is_none()
            && self.pending_history_snapshot.is_none()
            && child_edit_input_received
        {
            *frame_snapshot = match self.encode_history_snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.status = format!("Could not capture undo state: {error}");
                    None
                }
            };
        }
        self.history_snapshot_available_this_frame =
            frame_snapshot.is_some() || self.pending_history_snapshot.is_some();
        match kind {
            DetachedWindowKind::Plugins => self.plugins(ui),
            DetachedWindowKind::Audio => self.scrollable_audio_settings_view(ui),
            DetachedWindowKind::Automation => self.automation_editor(ui),
            _ => {}
        }
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

    fn open_channel_group_dialog(&mut self) {
        let Some(document) = &self.document else {
            self.status = "Open a project before grouping channels".to_owned();
            return;
        };
        let channels = document.channels();
        let channel_ids = channels
            .iter()
            .filter(|channel| self.selected_rack_channels.contains(&channel.id()))
            .map(ChannelSummary::id)
            .collect::<Vec<_>>();
        if channel_ids.is_empty() {
            self.status = "Select one or more channels to group".to_owned();
            return;
        }
        let selected_group = channels
            .iter()
            .filter(|channel| self.selected_rack_channels.contains(&channel.id()))
            .map(ChannelSummary::group_number)
            .reduce(|left, right| (left == right).then_some(left).flatten())
            .flatten();
        let groups = document.channel_groups();
        let name = selected_group
            .and_then(|index| {
                groups
                    .iter()
                    .find(|group| group.index() == index)
                    .and_then(ChannelGroupSummary::name)
            })
            .unwrap_or_default()
            .to_owned();
        self.channel_group_dialog = Some(ChannelGroupDialog {
            action: ChannelGroupDialogAction::AssignSelected(channel_ids),
            name,
        });
    }

    fn open_empty_channel_group_dialog(&mut self) {
        if self.document.is_none() {
            self.status = "Open a project before adding a display group".to_owned();
            return;
        }
        self.channel_group_dialog = Some(ChannelGroupDialog {
            action: ChannelGroupDialogAction::CreateEmpty,
            name: String::new(),
        });
    }

    fn open_rename_channel_group_dialog(&mut self, group_index: i32) {
        let Some(document) = &self.document else {
            self.status = "Open a project before renaming a display group".to_owned();
            return;
        };
        let Some(group) = document
            .channel_groups()
            .into_iter()
            .find(|group| group.index() == group_index)
        else {
            self.status = "The selected display group no longer exists".to_owned();
            return;
        };
        self.channel_group_dialog = Some(ChannelGroupDialog {
            action: ChannelGroupDialogAction::Rename(group_index),
            name: group
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Group {}", group_index.saturating_add(1))),
        });
    }

    fn apply_channel_group_dialog(&mut self, dialog: ChannelGroupDialog) {
        let before = self
            .document
            .as_ref()
            .and_then(|document| document.encode_lossless().ok());
        let result = self.document.as_mut().map(|document| match &dialog.action {
            ChannelGroupDialogAction::AssignSelected(channel_ids) => {
                document.group_channels(channel_ids, &dialog.name)
            }
            ChannelGroupDialogAction::CreateEmpty => document.add_channel_group(&dialog.name),
            ChannelGroupDialogAction::Rename(group_index) => document
                .rename_channel_group(*group_index, &dialog.name)
                .map(|()| *group_index),
        });
        match result {
            Some(Ok(group_index)) => {
                let changed = self
                    .document
                    .as_ref()
                    .and_then(|document| document.encode_lossless().ok())
                    .zip(before)
                    .is_some_and(|(after, before)| after != before);
                if changed {
                    self.stop_project_playback();
                    self.dirty = true;
                    self.status = match &dialog.action {
                        ChannelGroupDialogAction::AssignSelected(channel_ids) => format!(
                            "Grouped {} channel(s) in {}",
                            channel_ids.len(),
                            dialog.name
                        ),
                        ChannelGroupDialogAction::CreateEmpty => {
                            format!("Added empty display group {}", dialog.name)
                        }
                        ChannelGroupDialogAction::Rename(_) => {
                            format!("Renamed display group to {}", dialog.name)
                        }
                    };
                } else {
                    self.status = match &dialog.action {
                        ChannelGroupDialogAction::AssignSelected(_) => {
                            format!("Channels already belong to {}", dialog.name)
                        }
                        ChannelGroupDialogAction::CreateEmpty => {
                            format!("Display group {} already exists", dialog.name)
                        }
                        ChannelGroupDialogAction::Rename(_) => {
                            format!("Display group is already named {}", dialog.name)
                        }
                    };
                }
                self.channel_display_filter = ChannelDisplayFilter::Group(group_index);
            }
            Some(Err(error)) => {
                self.status = format!("Could not update display group: {error}");
                self.channel_group_dialog = Some(dialog);
            }
            None => self.status = "Open a project before updating display groups".to_owned(),
        }
    }

    fn delete_channel_group_from_ui(&mut self, group_index: i32) {
        let group_name = self
            .document
            .as_ref()
            .and_then(|document| {
                document
                    .channel_groups()
                    .into_iter()
                    .find(|group| group.index() == group_index)
            })
            .and_then(|group| group.name().map(str::to_owned))
            .unwrap_or_else(|| format!("Group {}", group_index.saturating_add(1)));
        let result = self
            .document
            .as_mut()
            .map(|document| document.delete_channel_group(group_index));
        match result {
            Some(Ok(())) => {
                self.stop_project_playback();
                self.dirty = true;
                self.status = format!(
                    "Deleted display group {}; its channels are now unsorted",
                    group_name
                );
                self.channel_display_filter = match self.channel_display_filter {
                    ChannelDisplayFilter::Group(index) if index == group_index => {
                        ChannelDisplayFilter::All
                    }
                    ChannelDisplayFilter::Group(index) if index > group_index => {
                        ChannelDisplayFilter::Group(index - 1)
                    }
                    filter => filter,
                };
            }
            Some(Err(error)) => {
                self.status = format!("Could not delete display group: {error}");
            }
            None => self.status = "Open a project before deleting a display group".to_owned(),
        }
    }

    fn channel_group_dialog(&mut self, context: &egui::Context) {
        let Some(mut dialog) = self.channel_group_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        let (title, description, submit_label) = match &dialog.action {
            ChannelGroupDialogAction::AssignSelected(_) => (
                "Group selected channels",
                "Enter a group name. An existing name moves channels into that group.",
                "Assign selected",
            ),
            ChannelGroupDialogAction::CreateEmpty => (
                "Add display group",
                "Enter a name for the empty display group.",
                "Add group",
            ),
            ChannelGroupDialogAction::Rename(_) => (
                "Rename display group",
                "Enter a new name for this display group.",
                "Rename group",
            ),
        };
        egui::Window::new(title)
            .id(Id::new("channel-group-dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(context, |ui| {
                ui.label(description);
                ui.label("Group name");
                ui.text_edit_singleline(&mut dialog.name);
                ui.horizontal(|ui| {
                    if ui.button(submit_label).clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if apply {
            self.apply_channel_group_dialog(dialog);
        } else if open && !cancel {
            self.channel_group_dialog = Some(dialog);
        }
    }

    fn open_pattern_rename_dialog(&mut self, pattern_id: u16) {
        let Some(document) = &self.document else {
            self.status = "Open a project before renaming a Pattern".to_owned();
            return;
        };
        let Some(pattern) = document.patterns().ok().and_then(|patterns| {
            patterns
                .into_iter()
                .find(|pattern| pattern.id == pattern_id)
        }) else {
            self.status = format!("Pattern {pattern_id} is no longer available");
            return;
        };
        self.pattern_rename_dialog = Some(PatternRenameDialog {
            pattern_id,
            name: pattern.name.unwrap_or_default(),
        });
    }

    fn apply_pattern_rename_dialog(&mut self, dialog: PatternRenameDialog) {
        let before = self
            .document
            .as_ref()
            .and_then(|document| document.encode_lossless().ok());
        let result = self
            .document
            .as_mut()
            .map(|document| document.set_pattern_name(dialog.pattern_id, &dialog.name));
        match result {
            Some(Ok(())) => {
                let changed = self
                    .document
                    .as_ref()
                    .and_then(|document| document.encode_lossless().ok())
                    .zip(before)
                    .is_some_and(|(after, before)| after != before);
                if changed {
                    self.stop_project_playback();
                    self.dirty = true;
                    self.status =
                        format!("Renamed Pattern {} to {}", dialog.pattern_id, dialog.name);
                } else {
                    self.status = format!(
                        "Pattern {} is already named {}",
                        dialog.pattern_id, dialog.name
                    );
                }
            }
            Some(Err(error)) => {
                self.status = format!("Could not rename Pattern: {error}");
                self.pattern_rename_dialog = Some(dialog);
            }
            None => self.status = "Open a project before renaming a Pattern".to_owned(),
        }
    }

    fn pattern_rename_dialog(&mut self, context: &egui::Context) {
        let Some(mut dialog) = self.pattern_rename_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new("Rename Pattern")
            .id(Id::new("pattern-rename-dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(context, |ui| {
                ui.label(format!("Pattern {} name", dialog.pattern_id));
                ui.text_edit_singleline(&mut dialog.name);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if apply {
            self.apply_pattern_rename_dialog(dialog);
        } else if open && !cancel {
            self.pattern_rename_dialog = Some(dialog);
        }
    }

    fn delete_pattern_from_ui(&mut self, pattern_id: u16) {
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| "no project is open".to_owned())
            .and_then(|document| {
                document
                    .delete_pattern(pattern_id)
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(()) => {
                self.stop_project_playback();
                self.dirty = true;
                self.selected_pattern = self
                    .document
                    .as_ref()
                    .and_then(|document| document.patterns().ok())
                    .and_then(|patterns| patterns.first().map(|pattern| pattern.id));
                self.selected_note = None;
                self.selected_piano_notes.clear();
                self.piano_roll_time_selection = None;
                self.selected_pattern_time_signature = None;
                self.selected_pattern_time_signature_pattern = None;
                self.status = format!("Deleted unused Pattern {pattern_id}");
            }
            Err(error) => self.status = format!("Could not delete Pattern {pattern_id}: {error}"),
        }
    }

    fn pattern_delete_dialog(&mut self, context: &egui::Context) {
        let Some(pattern_id) = self.pattern_delete_dialog.take() else {
            return;
        };
        let pattern_label = self
            .document
            .as_ref()
            .and_then(|document| document.patterns().ok())
            .and_then(|patterns| {
                patterns
                    .into_iter()
                    .find(|pattern| pattern.id == pattern_id)
            })
            .and_then(|pattern| pattern.name)
            .map_or_else(
                || format!("Pattern {pattern_id}"),
                |name| format!("{name} (Pattern {pattern_id})"),
            );
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Delete Pattern")
            .id(Id::new("pattern-delete-dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(context, |ui| {
                ui.label(format!("Delete {pattern_label}?"));
                ui.label("Patterns used by Playlist Clips cannot be deleted.");
                ui.horizontal(|ui| {
                    if ui.button("Delete Pattern").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    confirm = true;
                }
                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if confirm {
            self.delete_pattern_from_ui(pattern_id);
        } else if open && !cancel {
            self.pattern_delete_dialog = Some(pattern_id);
        }
    }

    fn open_project(&mut self, path: &Path) {
        self.finish_midi_mixer_control_undo_session();
        if self.audio_recording.is_some() {
            self.stop_audio_recording();
            self.status =
                "Audio recording is finalizing; wait for the take to reach the Playlist before switching projects".to_owned();
            return;
        }
        self.finish_active_midi_recording();
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
        if self.audio_recording.is_some() {
            self.stop_audio_recording();
            self.status =
                "Audio recording is finalizing; wait for the take to reach the Playlist before switching projects".to_owned();
            return false;
        }
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
                self.project_generation = self.project_generation.wrapping_add(1);
                self.piano_roll_hidden_ghost_channels.clear();
                self.piano_roll_hidden_ghost_tracks.clear();
                self.piano_roll_hidden_ghost_clips.clear();
                self.soloed_playlist_track_range = None;
                self.playlist_track_name_editor = None;
                self.collapsed_playlist_groups.clear();
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
                let project_settings = document.project_settings();
                self.project_settings_advanced_supported = project_settings.is_some();
                if let Some(settings) = project_settings {
                    self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
                    self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
                } else {
                    self.project_settings_play_truncated = false;
                    self.project_settings_fast_declick = false;
                }
                self.project_settings_pan_law_raw = document.metadata().pan_law_raw().unwrap_or(0);
                self.project_settings_master_pitch_cents =
                    document.metadata().master_pitch_cents().unwrap_or(0);
                self.project_settings_master_pitch_changed = false;
                if let Some((numerator, denominator)) = document.metadata().time_signature() {
                    self.project_settings_time_signature_enabled = true;
                    self.project_settings_time_signature_numerator = numerator;
                    self.project_settings_time_signature_denominator = denominator;
                } else {
                    self.project_settings_time_signature_enabled = false;
                    self.project_settings_time_signature_numerator = 4;
                    self.project_settings_time_signature_denominator = 4;
                }
                self.selected_pattern = document
                    .patterns()
                    .ok()
                    .and_then(|patterns| patterns.first().map(|pattern| pattern.id));
                self.selected_note_channel =
                    document.channels().first().map(|channel| channel.id());
                self.selected_graph_channel =
                    document.channels().first().map(|channel| channel.id());
                self.selected_rack_channels.clear();
                if let Some(channel_id) = self.selected_graph_channel {
                    self.selected_rack_channels.insert(channel_id);
                }
                self.rack_selection_anchor = self.selected_graph_channel;
                self.selected_clip = None;
                self.selected_playlist_clips.clear();
                self.playlist_selection_anchor = None;
                self.selected_playlist_track = None;
                self.selected_playlist_tracks.clear();
                self.playlist_track_selection_anchor = None;
                self.playlist_clip_context_split = None;
                self.playlist_clip_clipboard = None;
                self.active_playlist_clip_drag = None;
                self.selected_time_marker = None;
                self.selected_note = None;
                self.selected_piano_notes.clear();
                self.piano_roll_time_selection = None;
                self.piano_roll_time_selection_drag = None;
                self.piano_roll_pitch_selection_drag = None;
                self.piano_roll_selection_drag = None;
                self.active_note_drag = None;
                self.pending_midi_import = None;
                self.pending_midi_cc_import = None;
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
                self.automation_interpolate_mode = false;
                self.automation_interpolation_anchor = None;
                self.last_plugin_action = None;
                self.vst3_host = None;
                self.channel_vst3_instances.clear();
                self.selected_arrangement = document.arrangements().ok().and_then(|arrangements| {
                    arrangements.first().map(|arrangement| arrangement.id)
                });
                if self
                    .untitled_autosave_key
                    .as_deref()
                    .is_some_and(|key| !project_paths_equal(key, target_path))
                {
                    self.clear_untitled_autosave();
                }
                self.current_path = Some(target_path.to_path_buf());
                self.new_project_template_path = None;
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
                    .map_or_else(String::new, |summary| format!(" Â· {summary}"));
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
                        " Â· recent project list could not be saved: {error}"
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
            .or_else(|| self.new_project_template_path.clone())
    }

    fn selected_pattern_vst3_parameter_source(&self) -> Option<(String, u16, u8, u8)> {
        let project_path = self.current_path.as_ref()?.to_string_lossy().into_owned();
        let pattern_id = self.selected_pattern?;
        let (stream_channel, stream_flags) = self.piano_roll_controller_stream?;
        let pattern = self
            .document
            .as_ref()?
            .patterns()
            .ok()?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)?;
        pattern
            .controllers
            .iter()
            .any(|point| point.channel == stream_channel && point.flags == stream_flags)
            .then_some((project_path, pattern_id, stream_channel, stream_flags))
    }

    fn pattern_vst3_parameter_automations_for_pattern(
        &self,
        pattern_id: u16,
        controllers: &[PatternController],
        ppq: u16,
    ) -> BTreeMap<u16, Vec<Vst3ParameterAutomation>> {
        let (Some(project_path), Some(host)) =
            (self.current_path.as_deref(), self.vst3_host.as_ref())
        else {
            return BTreeMap::new();
        };
        let loaded_plugins = host.loaded_plugins();
        let mut automations = BTreeMap::new();
        for (&channel_id, &instance_id) in &self.channel_vst3_instances {
            let Some(plugin) = loaded_plugins
                .iter()
                .find(|plugin| plugin.id == instance_id)
            else {
                continue;
            };
            let lanes = pattern_vst3_parameter_automation_for_channel(
                &self.pattern_vst3_parameter_links,
                Some(project_path),
                pattern_id,
                controllers,
                channel_id,
                &plugin.uid,
                ppq,
            );
            if !lanes.is_empty() {
                automations.insert(channel_id, lanes);
            }
        }
        automations
    }

    fn make_audio_clip_unique_as_sample(
        &mut self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<u16, String> {
        let project_path = self.sample_project_path().ok_or_else(|| {
            "save the project before making an Audio Clip unique as a sample".to_owned()
        })?;
        let mut document = self.document.as_ref().cloned().ok_or_else(|| {
            "open a project before making an Audio Clip unique as a sample".to_owned()
        })?;
        let source_sample_path = {
            let arrangement = document
                .arrangements()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|arrangement| arrangement.id == arrangement_id)
                .ok_or_else(|| "the requested arrangement does not exist".to_owned())?;
            let clip = arrangement
                .clips
                .get(clip_index)
                .ok_or_else(|| "the requested Playlist clip does not exist".to_owned())?;
            let flp_rebuild::PlaylistClipTarget::Channel { id } = clip.target() else {
                return Err("Make unique as sample applies to Playlist Audio Clips".to_owned());
            };
            let channel = document
                .channels()
                .into_iter()
                .find(|channel| channel.id() == id && channel.kind() == Some(4))
                .ok_or_else(|| "the Playlist Audio Clip channel does not exist".to_owned())?;
            channel
                .sample_path()
                .ok_or_else(|| "the Audio Clip channel has no sample path".to_owned())?
                .to_owned()
        };
        let source_sample = SamplePathResolver::new(&project_path)
            .resolve(&source_sample_path)
            .map_err(|error| format!("could not find the Audio Clip sample: {error}"))?;
        let channel_id = document
            .make_playlist_audio_clip_unique(arrangement_id, clip_index)
            .map_err(|error| error.to_string())?;

        let (copied_sample, sample_reference) = if let Some(workspace) = &self.package_workspace {
            workspace.copy_unique_sample(&source_sample)?
        } else {
            let project_directory = project_path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| source_sample.parent().unwrap_or_else(|| Path::new(".")));
            let sample_directory = project_directory.join("Samples");
            let copied_sample = copy_unique_sample_file(&source_sample, &sample_directory)?;
            let Some(filename) = copied_sample
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                let _ = fs::remove_file(&copied_sample);
                return Err("the copied sample filename cannot be stored in the project".to_owned());
            };
            (
                copied_sample,
                Path::new("Samples")
                    .join(filename)
                    .to_string_lossy()
                    .into_owned(),
            )
        };

        if let Err(error) = document.set_channel_sample_path(channel_id, &sample_reference) {
            let _ = fs::remove_file(&copied_sample);
            return Err(error.to_string());
        }
        self.document = Some(document);
        self.stop_project_playback();
        self.selected_arrangement = Some(arrangement_id);
        self.selected_clip = Some(clip_index);
        self.selected_playlist_clips.clear();
        self.selected_playlist_clips.insert(clip_index);
        self.playlist_clip_context_split = None;
        self.active_playlist_clip_drag = None;
        self.dirty = true;
        self.status = format!(
            "Made Audio Clip {} unique as sample in channel {channel_id}",
            clip_index + 1
        );
        self.refresh_audio_waveform_paths();
        Ok(channel_id)
    }

    fn selected_playlist_audio_clip(&self) -> Option<(u16, usize)> {
        let arrangement_id = self.selected_arrangement.unwrap_or_default();
        let clip_index = self.selected_clip?;
        let document = self.document.as_ref()?;
        let arrangement = document
            .arrangements()
            .ok()?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)?;
        let clip = arrangement.clips.get(clip_index)?;
        let flp_rebuild::PlaylistClipTarget::Channel { id } = clip.target() else {
            return None;
        };
        document
            .channels()
            .iter()
            .any(|channel| {
                channel.id() == id && channel.kind() == Some(4) && channel.enabled() != Some(false)
            })
            .then_some((arrangement_id, clip_index))
    }

    fn audio_clip_render_source_details(
        &self,
        arrangement_id: u16,
        clip_index: usize,
    ) -> Result<PendingAudioClipSource, String> {
        let document = self
            .document
            .as_ref()
            .ok_or_else(|| "Open a project before placing a rendered Audio Clip".to_owned())?;
        if !project_has_audio_clip_template(document) {
            return Err(
                "Add a sample-backed Audio Clip to the Playlist before inserting a render"
                    .to_owned(),
            );
        }
        let arrangement = document
            .arrangements()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|arrangement| arrangement.id == arrangement_id)
            .ok_or_else(|| "The selected Playlist arrangement no longer exists".to_owned())?;
        let clip = arrangement
            .clips
            .get(clip_index)
            .ok_or_else(|| "The selected Playlist clip no longer exists".to_owned())?;
        if clip.muted {
            return Err(
                "Unmute the selected Audio Clip before inserting or consolidating its render"
                    .to_owned(),
            );
        }
        let Some(track_index) = clip.track_index else {
            return Err("The selected Audio Clip has no recognized Playlist track".to_owned());
        };
        let PlaylistClipTarget::Channel {
            id: source_channel_id,
        } = clip.target()
        else {
            return Err("Consolidate selected Audio Clip applies to Audio Clips".to_owned());
        };
        let project_path = self
            .sample_project_path()
            .ok_or_else(|| "Save the project before placing a rendered Audio Clip".to_owned())?;
        let project_bytes = document
            .encode_lossless()
            .map_err(|error| format!("Could not verify the current project: {error}"))?;
        let project_fingerprint: [u8; 16] = Md5::digest(&project_bytes).into();

        Ok(PendingAudioClipSource {
            arrangement_id,
            source_clip_index: clip_index,
            source_channel_id,
            source_position_ticks: clip.position_ticks,
            source_track_index: track_index,
            project_generation: self.project_generation,
            project_path,
            project_fingerprint,
        })
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

    fn queue_playlist_audio_drop(
        &mut self,
        path: PathBuf,
        arrangement_id: u16,
        position_ticks: u32,
        track_index: u16,
        ctx: &egui::Context,
    ) {
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before adding audio to the Playlist".to_owned();
            return;
        };
        if !project_has_audio_clip_template(document) {
            self.status =
                "Add an Audio Clip to the project first so its FLP record can be reused".to_owned();
            return;
        }

        self.pending_playlist_audio_drops
            .push_back(PendingPlaylistAudioDrop {
                path: path.clone(),
                arrangement_id,
                position_ticks,
                track_index,
                project_generation: self.project_generation,
            });
        self.request_audio_waveform(&path);
        self.status = format!("Preparing {} for the Playlistâ€¦", path.display());
        ctx.request_repaint_after(Duration::from_millis(80));
    }

    fn process_pending_playlist_audio_drops(
        &mut self,
        ctx: &egui::Context,
        history_snapshot_available: bool,
    ) {
        while let Some((pending_path, pending_project_generation)) = self
            .pending_playlist_audio_drops
            .front()
            .map(|pending| (pending.path.clone(), pending.project_generation))
        {
            if pending_project_generation != self.project_generation {
                let pending = self
                    .pending_playlist_audio_drops
                    .pop_front()
                    .expect("the pending Playlist drop was checked above");
                self.status = format!(
                    "Skipped {} because the active project changed",
                    pending.path.display()
                );
                continue;
            }
            if let Some(error) = self.waveform_errors.get(&pending_path).cloned() {
                let pending = self
                    .pending_playlist_audio_drops
                    .pop_front()
                    .expect("the pending Playlist drop was checked above");
                self.status = format!(
                    "Could not add {} to the Playlist: {error}",
                    pending.path.display()
                );
                continue;
            }
            if let Some(waveform) = self.audio_waveforms.get(&pending_path).cloned() {
                let pending = self
                    .pending_playlist_audio_drops
                    .pop_front()
                    .expect("the pending Playlist drop was checked above");
                self.insert_browser_audio_clip(pending, &waveform, ctx, history_snapshot_available);
                continue;
            }

            self.request_audio_waveform(&pending_path);
            ctx.request_repaint_after(Duration::from_millis(80));
            break;
        }
    }

    fn insert_browser_audio_clip(
        &mut self,
        pending: PendingPlaylistAudioDrop,
        waveform: &AudioWaveform,
        ctx: &egui::Context,
        history_snapshot_available: bool,
    ) {
        let Some(document) = self.document.as_ref() else {
            self.status = format!(
                "Could not add {} because no project is open",
                pending.path.display()
            );
            return;
        };
        if !project_has_audio_clip_template(document) {
            self.status = format!(
                "Could not add {}: the project has no Audio Clip record to reuse",
                pending.path.display()
            );
            return;
        }
        if !document
            .arrangements()
            .is_ok_and(|arrangements| arrangements.iter().any(|a| a.id == pending.arrangement_id))
        {
            self.status = format!(
                "Could not add {} because its Playlist arrangement no longer exists",
                pending.path.display()
            );
            return;
        }
        let tempo_map = match PlaylistTempoMap::for_arrangement(document, pending.arrangement_id) {
            Ok(tempo_map) => tempo_map,
            Err(error) => {
                self.status = format!(
                    "Could not determine the Playlist tempo map for {}: {error}",
                    pending.path.display()
                );
                return;
            }
        };
        let length_ticks = match tempo_map
            .ticks_for_audio_frames(
                u64::from(pending.position_ticks),
                waveform.frame_count,
                waveform.sample_rate,
            )
            .and_then(|ticks| {
                u32::try_from(ticks)
                    .map_err(|_| "audio source is too long for this Playlist".to_owned())
            }) {
            Ok(length_ticks) => length_ticks,
            Err(error) => {
                self.status = format!(
                    "Could not determine the Playlist length for {}: {error}",
                    pending.path.display()
                );
                return;
            }
        };
        if pending.position_ticks.checked_add(length_ticks).is_none() {
            self.status = format!(
                "{} is too long to fit at this Playlist position",
                pending.path.display()
            );
            return;
        }
        let Ok(undo_snapshot) = document.encode_lossless() else {
            self.status = "Could not capture undo state before adding the Audio Clip".to_owned();
            return;
        };

        let mut updated = document.clone();
        let sample_path = pending.path.to_string_lossy();
        let channel_name = pending
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Audio Clip".to_owned());
        let channel_id = match updated.create_audio_channel(&sample_path, &channel_name) {
            Ok(channel_id) => channel_id,
            Err(error) => {
                self.status = format!(
                    "Could not create an Audio Clip channel for {}: {error}",
                    pending.path.display()
                );
                return;
            }
        };
        let clip_index = match updated.create_playlist_audio_clip(
            pending.arrangement_id,
            channel_id,
            pending.position_ticks,
            length_ticks,
            pending.track_index,
        ) {
            Ok(clip_index) => clip_index,
            Err(error) => {
                self.status = format!(
                    "Could not place {} in the Playlist: {error}",
                    pending.path.display()
                );
                return;
            }
        };

        self.stop_project_playback();
        self.document = Some(updated);
        if !history_snapshot_available {
            self.remember_undo_snapshot(undo_snapshot);
        }
        self.selected_arrangement = Some(pending.arrangement_id);
        self.selected_clip = Some(clip_index);
        self.selected_playlist_clips.clear();
        self.selected_playlist_clips.insert(clip_index);
        self.playlist_selection_anchor = Some(clip_index);
        self.dirty = true;
        self.view = MainView::Playlist;
        self.refresh_audio_waveform_paths();
        self.status = format!(
            "Added {} to Playlist track {}",
            pending.path.display(),
            pending.track_index + 1
        );
        ctx.request_repaint();
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
        self.finish_active_midi_recording();
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

    fn save_channel_state_preset(&mut self, channel_id: u16, channel_name: &str) {
        let Some(mut document) = self.document.clone() else {
            self.status = "Open a project before saving a channel preset".to_owned();
            return;
        };
        let file_name = state_preset_file_stem(channel_name, &format!("Channel {channel_id}"));
        let Some(path) = self.choose_state_preset_path("Save channel state preset", &file_name)
        else {
            return;
        };

        let plugin_state_warnings = self.capture_loaded_vst3_state(&mut document);
        let selected_channel_warning = format!("channel {channel_id}: ");
        if let Some(warning) = plugin_state_warnings
            .iter()
            .find(|warning| warning.starts_with(&selected_channel_warning))
        {
            self.status = format!("Could not capture live channel state: {warning}");
            return;
        }
        let preset_bytes = match document.encode_channel_state_preset(channel_id) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not save channel preset: {error}");
                return;
            }
        };
        self.write_state_preset_file(path, preset_bytes, "channel");
    }

    fn save_mixer_insert_state_preset(&mut self, insert_ordinal: usize, insert_name: &str) {
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before saving a Mixer state".to_owned();
            return;
        };
        let fallback_name = if insert_ordinal == 0 {
            "Master".to_owned()
        } else {
            format!("Insert {insert_ordinal}")
        };
        let file_name = state_preset_file_stem(insert_name, &fallback_name);
        let Some(path) = self.choose_state_preset_path("Save Mixer state preset", &file_name)
        else {
            return;
        };
        let preset_bytes = match document.encode_mixer_insert_state_preset(insert_ordinal) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not save Mixer state preset: {error}");
                return;
            }
        };
        self.write_state_preset_file(path, preset_bytes, "Mixer state");
    }

    fn load_mixer_insert_state_preset(&mut self, insert_ordinal: usize) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Load Mixer state preset")
            .add_filter("FL Studio Mixer state", &["fst"])
            .pick_file()
        else {
            return;
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!(
                    "Could not read Mixer state preset {}: {error}",
                    path.display()
                );
                return;
            }
        };
        let preset = match FstPreset::parse(&bytes) {
            Ok(preset) => preset,
            Err(error) => {
                self.status = format!("Could not parse Mixer state preset: {error}");
                return;
            }
        };
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before loading a Mixer state".to_owned();
            return;
        };
        match document.apply_mixer_insert_state_preset(insert_ordinal, &preset) {
            Ok(event_count) => {
                self.dirty = true;
                self.status = format!(
                    "Loaded {event_count} Mixer effect-state events into insert {insert_ordinal} from {}",
                    path.display()
                );
            }
            Err(error) => {
                self.status = format!("Could not load Mixer state preset: {error}");
            }
        }
    }

    fn choose_state_preset_path(&self, title: &str, file_name: &str) -> Option<PathBuf> {
        let path = rfd::FileDialog::new()
            .set_title(title)
            .add_filter("FL Studio state preset", &["fst"])
            .set_file_name(format!("{file_name}.fst"))
            .save_file()?;
        Some(
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("fst"))
            {
                path
            } else {
                path.with_extension("fst")
            },
        )
    }

    fn write_state_preset_file(&mut self, path: PathBuf, preset_bytes: Vec<u8>, state_name: &str) {
        match fs::write(&path, preset_bytes) {
            Ok(()) => {
                self.status = format!("Saved {state_name} preset to {}", path.display());
            }
            Err(error) => {
                self.status = format!("Could not write preset {}: {error}", path.display());
            }
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
        self.finish_active_midi_recording();
        if self.pending_backup_write.is_some() {
            self.status = "A backup is already being written".to_owned();
            return;
        }
        self.start_backup_write(BackupPurpose::Manual);
    }

    fn revert_to_last_autosave(&mut self) {
        let Some(backup_path) = self.last_autosave_path.clone() else {
            self.status = "No autosave is available for this project".to_owned();
            return;
        };
        let is_untitled = self.current_path.is_none();
        let Some(source_path) = self
            .current_path
            .clone()
            .or_else(|| self.untitled_autosave_key.clone())
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
            is_untitled,
            template_path: is_untitled
                .then(|| self.new_project_template_path.clone())
                .flatten(),
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
        let (Some(source_path), Some(document)) = (
            self.current_path
                .clone()
                .or_else(|| self.untitled_autosave_key.clone()),
            self.document.as_ref(),
        ) else {
            return;
        };
        let is_untitled = self.current_path.is_none();
        let template_path = is_untitled
            .then(|| self.new_project_template_path.clone())
            .flatten();
        let mut document = document.clone();
        let plugin_state_warnings = self.capture_loaded_vst3_state(&mut document);
        let result = document
            .encode_lossless()
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                if is_untitled {
                    write_untitled_backup_metadata(&source_path, template_path.as_deref())?;
                }
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
                self.status = format!(
                    "Autosaved before {operation}{}",
                    plugin_state_warning_suffix(&plugin_state_warnings)
                );
            }
            Err(error) => {
                self.status = format!(
                    "Could not autosave before {operation}: {error}{}",
                    plugin_state_warning_suffix(&plugin_state_warnings)
                );
            }
        }
    }

    fn start_backup_write(&mut self, purpose: BackupPurpose) {
        let source_path = self.current_path.clone().or_else(|| match purpose {
            BackupPurpose::Autosave => self.untitled_autosave_key.clone(),
            BackupPurpose::Manual => None,
        });
        let Some(source_path) = source_path else {
            self.status = "Save the project before creating a backup".to_owned();
            return;
        };
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before creating a backup".to_owned();
            return;
        };
        let is_untitled = self.current_path.is_none();
        let template_path = is_untitled
            .then(|| self.new_project_template_path.clone())
            .flatten();
        let mut document = document.clone();
        let plugin_state_warnings = self.capture_loaded_vst3_state(&mut document);
        let bytes = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not encode autosave: {error}");
                return;
            }
        };
        let retention = self.backup_retention;
        let worker_source = source_path.clone();
        let worker_template_path = template_path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = match thread::Builder::new()
            .name("flp-autosave".to_owned())
            .spawn(move || {
                let result = if is_untitled {
                    write_untitled_backup_metadata(&worker_source, worker_template_path.as_deref())
                        .and_then(|()| {
                            write_project_backup(&worker_source, &bytes, purpose, retention)
                        })
                } else {
                    write_project_backup(&worker_source, &bytes, purpose, retention)
                };
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
            plugin_state_warnings,
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
                Ok(result) => Some((
                    pending.source_path.clone(),
                    pending.purpose,
                    pending.plugin_state_warnings.clone(),
                    result,
                )),
                Err(TryRecvError::Disconnected) => Some((
                    pending.source_path.clone(),
                    pending.purpose,
                    pending.plugin_state_warnings.clone(),
                    Err("backup writer stopped unexpectedly".to_owned()),
                )),
                Err(TryRecvError::Empty) => None,
            }
        });
        let Some((source_path, purpose, plugin_state_warnings, result)) = result else {
            return;
        };
        if let Some(pending) = self.pending_backup_write.take() {
            let _ = pending.worker.join();
        }
        match (purpose, result) {
            (BackupPurpose::Autosave, Ok(path)) => {
                let is_current_project =
                    self.current_path.as_deref().is_some_and(|current_path| {
                        project_paths_equal(current_path, &source_path)
                    }) || (self.current_path.is_none()
                        && self
                            .untitled_autosave_key
                            .as_deref()
                            .is_some_and(|key| project_paths_equal(key, &source_path)));
                if is_current_project {
                    self.last_autosave_path = Some(path.clone());
                    self.status = format!(
                        "Autosaved to {}{}",
                        path.display(),
                        plugin_state_warning_suffix(&plugin_state_warnings)
                    );
                }
            }
            (BackupPurpose::Manual, Ok(path)) => {
                self.status = format!(
                    "Backup created at {}{}",
                    path.display(),
                    plugin_state_warning_suffix(&plugin_state_warnings)
                );
            }
            (_, Err(error)) => {
                self.status = format!(
                    "Could not write backup: {error}{}",
                    plugin_state_warning_suffix(&plugin_state_warnings)
                );
            }
        }
    }

    fn schedule_next_autosave(&mut self) {
        self.autosave_deadline = (self.autosave_minutes > 0)
            .then(|| Instant::now() + Duration::from_secs(u64::from(self.autosave_minutes) * 60));
    }

    fn clear_untitled_autosave(&mut self) {
        let Some(key) = self.untitled_autosave_key.take() else {
            return;
        };
        if self
            .pending_backup_write
            .as_ref()
            .is_some_and(|pending| project_paths_equal(&pending.source_path, &key))
            && let Some(pending) = self.pending_backup_write.take()
        {
            let _ = pending.worker.join();
        }
        remove_untitled_project_backup(&key);
        self.last_autosave_path = None;
        self.autosave_deadline = None;
    }

    fn advance_autosave(&mut self, context: &egui::Context) {
        self.poll_backup_write();
        if self.pending_backup_write.is_some() {
            context.request_repaint_after(Duration::from_millis(500));
            return;
        }
        if self.autosave_minutes == 0
            || !self.dirty
            || (self.current_path.is_none() && self.untitled_autosave_key.is_none())
        {
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
            } else if prompt.is_untitled {
                ui.label("An autosaved copy of an untitled project is available.");
                ui.label("Recover the autosaved version?");
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
                if prompt.is_untitled
                    && self
                        .untitled_autosave_key
                        .as_deref()
                        .is_some_and(|key| project_paths_equal(key, &prompt.source_path))
                {
                    self.clear_untitled_autosave();
                } else {
                    let _ = fs::remove_file(&prompt.backup_path);
                    if self.last_autosave_path.as_ref() == Some(&prompt.backup_path) {
                        self.last_autosave_path = None;
                    }
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
        if self.audio_recording.is_some() {
            self.stop_audio_recording();
            self.status =
                "Audio recording is finalizing; close again after the take reaches the Playlist"
                    .to_owned();
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            return;
        }
        self.finish_active_midi_recording();
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
            PendingProjectChange::NewProject { template_path, .. } => (
                "Save changes before starting a new project?",
                format!(
                    "Save changes to the current project before starting a MIDI project from {}?",
                    template_path.display()
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
            PendingProjectChange::NewProject {
                document,
                template_path,
                imported,
                destinations,
            } => {
                self.install_midi_project(context, *document, template_path, imported, destinations)
            }
            PendingProjectChange::Exit => {
                self.clear_untitled_autosave();
                self.close_approved = true;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn request_midi_new_project(
        &mut self,
        context: &egui::Context,
        template_path: PathBuf,
        midi: MidiFile,
        track_indices: Vec<usize>,
        options: MidiImportOptions,
    ) {
        let result = (|| {
            let bytes = fs::read(&template_path).map_err(|error| {
                format!(
                    "Could not read project template {}: {error}",
                    template_path.display()
                )
            })?;
            let template = FlpDocument::parse(&bytes).map_err(|error| {
                format!(
                    "Could not parse project template {}: {error}",
                    template_path.display()
                )
            })?;
            let channels = template.channels();
            let template_channel_id = channels
                .iter()
                .find(|channel| channel.kind() == Some(0))
                .or_else(|| channels.iter().find(|channel| channel.kind() == Some(2)))
                .map(|channel| channel.id())
                .ok_or_else(|| {
                    format!(
                        "Project template {} has no Sampler or Native instrument channel to copy",
                        template_path.display()
                    )
                })?;
            FlpDocument::new_project_from_midi_selected_tracks_with_options(
                &template,
                &midi,
                template_channel_id,
                &track_indices,
                options,
            )
            .map_err(|error| error.to_string())
        })();
        match result {
            Ok(import_result) => {
                self.pending_project_change = Some(PendingProjectChange::NewProject {
                    document: Box::new(import_result.project),
                    template_path,
                    imported: import_result.imported_notes,
                    destinations: import_result.destinations,
                });
                if self.dirty {
                    self.recovery_prompt = None;
                } else {
                    self.perform_pending_project_change(context);
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn install_midi_project(
        &mut self,
        _context: &egui::Context,
        document: FlpDocument,
        template_path: PathBuf,
        imported: usize,
        destinations: Vec<(u16, u16)>,
    ) {
        let bytes = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not encode new MIDI project: {error}");
                return;
            }
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let temporary_path = std::env::temp_dir().join(format!(
            "fl-studio-midi-project-{}-{nonce}.flp",
            std::process::id()
        ));
        if let Err(error) = fs::write(&temporary_path, bytes) {
            self.status = format!("Could not stage the new MIDI project: {error}");
            return;
        }
        let temporary_recent_path =
            fs::canonicalize(&temporary_path).unwrap_or_else(|_| temporary_path.clone());
        let loaded = self.load_project_from(&temporary_path, &temporary_path, false);
        let _ = fs::remove_file(&temporary_path);
        if !loaded {
            self.browser_recent_projects
                .retain(|path| path != &temporary_recent_path);
            return;
        }

        self.browser_recent_projects
            .retain(|path| path != &temporary_recent_path);
        let _ = save_recent_projects(&self.browser_recent_projects);
        self.current_path = None;
        self.package_workspace = None;
        self.new_project_template_path = Some(template_path);
        self.untitled_autosave_key = new_untitled_project_autosave_key();
        self.saved_project_hash = None;
        self.dirty = true;
        self.autosave_deadline = None;
        self.last_autosave_path = None;
        self.selected_pattern = destinations.first().map(|(pattern_id, _)| *pattern_id);
        self.selected_note_channel = destinations.first().map(|(_, channel_id)| *channel_id);
        if let Some(channel_id) = self.selected_note_channel {
            self.selected_graph_channel = Some(channel_id);
            self.selected_rack_channels.clear();
            self.selected_rack_channels.insert(channel_id);
            self.rack_selection_anchor = Some(channel_id);
        }
        self.selected_note = destinations
            .first()
            .filter(|_| imported > 0)
            .map(|(pattern_id, channel_id)| (*pattern_id, *channel_id, 0));
        self.selected_piano_notes.clear();
        if let Some(note) = self.selected_note {
            self.selected_piano_notes.insert(note);
        }
        self.view = MainView::PianoRoll;
        self.refresh_audio_waveform_paths();
        self.status = format!(
            "Started a new project from {} and imported {imported} MIDI notes",
            self.new_project_template_path.as_deref().map_or_else(
                || "the Empty template".to_owned(),
                |path| path.display().to_string()
            )
        );
    }

    fn recover_from_backup(&mut self, prompt: &RecoveryPrompt) {
        let original_hash = fs::read(&prompt.source_path)
            .ok()
            .and_then(|bytes| FlpDocument::parse(&bytes).ok())
            .and_then(|document| project_hash(&document));
        if self.load_project_from(&prompt.backup_path, &prompt.source_path, false) {
            if prompt.is_untitled {
                let recent_key = fs::canonicalize(&prompt.source_path)
                    .unwrap_or_else(|_| prompt.source_path.clone());
                self.browser_recent_projects
                    .retain(|recent| recent != &recent_key);
                let _ = save_recent_projects(&self.browser_recent_projects);
                self.current_path = None;
                self.package_workspace = None;
                self.new_project_template_path = prompt.template_path.clone();
                self.untitled_autosave_key = Some(prompt.source_path.clone());
                self.refresh_audio_waveform_paths();
            }
            self.saved_project_hash = original_hash;
            self.dirty = true;
            self.last_autosave_path = Some(prompt.backup_path.clone());
            self.status = if prompt.is_untitled {
                "Recovered untitled project from autosave".to_owned()
            } else {
                format!("Recovered autosave for {}", prompt.source_path.display())
            };
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
        let Some(document) = self.document.as_ref() else {
            self.status = "Open a project before editing Project settings".to_owned();
            return;
        };
        let settings = document.project_settings();
        self.project_settings_advanced_supported = settings.is_some();
        if let Some(settings) = settings {
            self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
            self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
        } else {
            self.project_settings_play_truncated = false;
            self.project_settings_fast_declick = false;
        }
        self.project_settings_pan_law_raw = document.metadata().pan_law_raw().unwrap_or(0);
        self.project_settings_master_pitch_cents =
            document.metadata().master_pitch_cents().unwrap_or(0);
        self.project_settings_master_pitch_changed = false;
        if let Some((numerator, denominator)) = document.metadata().time_signature() {
            self.project_settings_time_signature_enabled = true;
            self.project_settings_time_signature_numerator = numerator;
            self.project_settings_time_signature_denominator = denominator;
        } else {
            self.project_settings_time_signature_enabled = false;
            self.project_settings_time_signature_numerator = 4;
            self.project_settings_time_signature_denominator = 4;
        }
        self.project_settings_open = true;
    }

    fn apply_project_settings(&mut self) {
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before editing Project settings".to_owned();
            return;
        };
        match document.set_project_settings(ProjectSettingsEdit {
            play_truncated_notes_in_clips: self
                .project_settings_advanced_supported
                .then_some(self.project_settings_play_truncated),
            fast_declick_for_cut_groups: self
                .project_settings_advanced_supported
                .then_some(self.project_settings_fast_declick),
            pan_law_raw: Some(self.project_settings_pan_law_raw),
            master_pitch_cents: self
                .project_settings_master_pitch_changed
                .then_some(self.project_settings_master_pitch_cents),
            time_signature: self.project_settings_time_signature_enabled.then_some((
                self.project_settings_time_signature_numerator,
                self.project_settings_time_signature_denominator,
            )),
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

    fn duplicate_pattern(&mut self, pattern_id: u16) {
        let Some(document) = self.document.as_mut() else {
            self.status = "Open a project before duplicating a pattern".to_owned();
            return;
        };
        match document.duplicate_pattern(pattern_id) {
            Ok(new_pattern_id) => {
                self.selected_pattern = Some(new_pattern_id);
                self.dirty = true;
                self.status = format!("Duplicated pattern {pattern_id} as {new_pattern_id}");
            }
            Err(error) => self.status = format!("Could not duplicate pattern: {error}"),
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
                if self.project_settings_advanced_supported {
                    ui.checkbox(
                        &mut self.project_settings_play_truncated,
                        "Play truncated notes in clips",
                    );
                    ui.checkbox(
                        &mut self.project_settings_fast_declick,
                        "Fast declick for cut groups",
                    );
                } else {
                    ui.label(
                        egui::RichText::new(
                            "Advanced project settings are not recognized for this file.",
                        )
                        .color(MUTED),
                    );
                }
                ui.horizontal(|ui| {
                    let pan_law_label = match self.project_settings_pan_law_raw {
                        0 => "Circular".to_owned(),
                        2 => "Triangular".to_owned(),
                        raw => format!("Unknown ({raw})"),
                    };
                    ui.label("Pan law");
                    egui::ComboBox::from_id_salt("project-settings-pan-law")
                        .selected_text(pan_law_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.project_settings_pan_law_raw,
                                0,
                                "Circular",
                            );
                            ui.selectable_value(
                                &mut self.project_settings_pan_law_raw,
                                2,
                                "Triangular",
                            );
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Master pitch");
                    if ui.add(
                        egui::DragValue::new(&mut self.project_settings_master_pitch_cents)
                            .range(-1200..=1200)
                            .suffix(" cents"),
                    ).changed() {
                        self.project_settings_master_pitch_changed = true;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Affects supported Sampler and VST3 note playback and rendering, plus unstretched Playlist Audio Clips (scale unset or 1.0), when Enable main pitch is on. Native instruments and stretched Audio Clips remain incomplete."
                    )
                    .color(MUTED),
                );
                ui.separator();
                if self.project_settings_time_signature_enabled {
                    ui.horizontal(|ui| {
                        ui.label("Project time signature");
                        ui.add(
                            egui::DragValue::new(
                                &mut self.project_settings_time_signature_numerator,
                            )
                            .range(1..=u8::MAX),
                        );
                        ui.label("/");
                        ui.add(
                            egui::DragValue::new(
                                &mut self.project_settings_time_signature_denominator,
                            )
                            .range(1..=u8::MAX),
                        );
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Project time signature");
                        if ui.button("Set as time signatureâ€¦").clicked() {
                            self.project_settings_time_signature_enabled = true;
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "No unique project signature is stored; time division or marker settings apply.",
                        )
                        .color(MUTED),
                    );
                }
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

    fn playlist_render_options_dialog(&mut self, context: &egui::Context) {
        if !self.playlist_render_options_open {
            return;
        }
        let mut current_track_options = Vec::new();
        if let Some(document) = self.document.as_ref() {
            let inserts = document.mixer_inserts();
            if inserts
                .iter()
                .find(|insert| insert.ordinal() == 0)
                .is_none_or(|insert| insert.enabled() != Some(false))
            {
                current_track_options.push((0, "Master".to_owned()));
            }
            current_track_options.extend(
                inserts
                    .into_iter()
                    .filter(|insert| {
                        insert.ordinal() > 0
                            && insert.ordinal() < MIXER_TRACK_METER_COUNT
                            && insert.enabled() != Some(false)
                    })
                    .map(|insert| {
                        let track = insert.ordinal();
                        let name = insert
                            .name()
                            .filter(|name| !name.trim().is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Insert {track}"));
                        (track, format!("Insert {track:02} - {name}"))
                    }),
            );
        }
        let has_current_track = self.playlist_render_current_track.is_some_and(|track| {
            current_track_options
                .iter()
                .any(|(option, _)| *option == track)
        });
        if !has_current_track {
            self.playlist_render_current_track = self
                .selected_mixer_insert
                .filter(|track| {
                    current_track_options
                        .iter()
                        .any(|(option, _)| option == track)
                })
                .or_else(|| {
                    current_track_options
                        .iter()
                        .find(|(track, _)| *track != 0)
                        .map(|(track, _)| *track)
                })
                .or_else(|| current_track_options.first().map(|(track, _)| *track));
        }
        if current_track_options.is_empty() {
            self.playlist_render_include_current_stem = false;
        }
        let mut open = self.playlist_render_options_open;
        let mut start_render = false;
        let mut cancel = false;
        egui::Window::new(if self.playlist_render_mixer_stems {
            "Render Mixer stems"
        } else {
            "Render Playlist mix"
        })
            .id(Id::new("playlist-render-options-dialog"))
            .open(&mut open)
            .resizable(false)
            .default_width(360.0)
            .show(context, |ui| {
                let render_format = if self.playlist_render_mixer_stems {
                    ui.label("Output format: WAV stems");
                    PlaylistSongOutputFormat::Wav
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Output format");
                        egui::ComboBox::from_id_salt("playlist-song-output-format")
                            .selected_text(self.playlist_song_output_format.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.playlist_song_output_format,
                                    PlaylistSongOutputFormat::Wav,
                                    "WAV",
                                );
                                ui.selectable_value(
                                    &mut self.playlist_song_output_format,
                                    PlaylistSongOutputFormat::Flac,
                                    "FLAC",
                                );
                                ui.selectable_value(
                                    &mut self.playlist_song_output_format,
                                    PlaylistSongOutputFormat::Ogg,
                                    "OGG",
                                );
                                ui.selectable_value(
                                    &mut self.playlist_song_output_format,
                                    PlaylistSongOutputFormat::Mp3,
                                    "MP3",
                                );
                            });
                    });
                    self.playlist_song_output_format
                };
                match render_format {
                    PlaylistSongOutputFormat::Wav => {
                        ui.horizontal(|ui| {
                            ui.label("WAV format");
                            egui::ComboBox::from_id_salt("playlist-render-wav-format")
                                .selected_text(self.playlist_render_format.label())
                                .show_ui(ui, |ui| {
                                    for format in [
                                        WavSampleFormat::Pcm16,
                                        WavSampleFormat::Pcm24,
                                        WavSampleFormat::Float32,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.playlist_render_format,
                                            format,
                                            format.label(),
                                        );
                                    }
                                });
                        });
                        let dither_enabled =
                            self.playlist_render_format == WavSampleFormat::Pcm16;
                        ui.add_enabled_ui(dither_enabled, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Dither");
                                egui::ComboBox::from_id_salt("playlist-render-dither")
                                    .selected_text(self.playlist_render_dither_mode.label())
                                    .show_ui(ui, |ui| {
                                        for mode in [
                                            WavDitherMode::Off,
                                            WavDitherMode::Tpdf,
                                            WavDitherMode::HighPassTpdf,
                                        ] {
                                            ui.selectable_value(
                                                &mut self.playlist_render_dither_mode,
                                                mode,
                                                mode.label(),
                                            );
                                        }
                                    });
                            });
                        });
                        ui.label(
                            "High-pass TPDF shifts most dither noise toward high frequencies.",
                        );
                        ui.label(match self.playlist_render_format {
                            WavSampleFormat::Pcm16 | WavSampleFormat::Pcm24 => {
                                "Integer PCM clips samples to the [-1, 1] range."
                            }
                            WavSampleFormat::Float32 => {
                                "Float WAV preserves levels outside [-1, 1]."
                            }
                        });
                    }
                    PlaylistSongOutputFormat::Flac => {
                        ui.horizontal(|ui| {
                            ui.label("FLAC bit depth");
                            egui::ComboBox::from_id_salt("playlist-render-flac-depth")
                                .selected_text(format!(
                                    "{}-bit",
                                    self.playlist_render_flac_bits_per_sample
                                ))
                                .show_ui(ui, |ui| {
                                    for depth in [16, 24] {
                                        ui.selectable_value(
                                            &mut self.playlist_render_flac_bits_per_sample,
                                            depth,
                                            format!("{depth}-bit"),
                                        );
                                    }
                                });
                        });
                        ui.label("FLAC is lossless compressed audio. WAV dither is not applied.");
                    }
                    PlaylistSongOutputFormat::Ogg => {
                        ui.horizontal(|ui| {
                            ui.label("OGG bit rate");
                            ui.add(
                                egui::Slider::new(
                                    &mut self.playlist_render_ogg_bitrate_kbps,
                                    64..=450,
                                )
                                .suffix(" kbps"),
                            );
                        });
                        ui.label("Ogg Vorbis is lossy. This sets the target average bit rate.");
                    }
                    PlaylistSongOutputFormat::Mp3 => {
                        ui.horizontal(|ui| {
                            ui.label("MP3 bit rate");
                            egui::ComboBox::from_id_salt("playlist-render-mp3-bitrate")
                                .selected_text(format!(
                                    "{} kbps",
                                    self.playlist_render_mp3_bitrate_kbps
                                ))
                                .show_ui(ui, |ui| {
                                    for bitrate in MP3_BITRATES_KBPS {
                                        ui.selectable_value(
                                            &mut self.playlist_render_mp3_bitrate_kbps,
                                            bitrate,
                                            format!("{bitrate} kbps"),
                                        );
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Dither");
                            egui::ComboBox::from_id_salt("playlist-render-mp3-dither")
                                .selected_text(self.playlist_render_dither_mode.label())
                                .show_ui(ui, |ui| {
                                    for mode in [
                                        WavDitherMode::Off,
                                        WavDitherMode::Tpdf,
                                        WavDitherMode::HighPassTpdf,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.playlist_render_dither_mode,
                                            mode,
                                            mode.label(),
                                        );
                                    }
                                });
                        });
                        ui.label("Dither is applied during 16-bit quantization before MP3 encoding.");
                        ui.label("MP3 is lossy; supported render rates are 32, 44.1, and 48 kHz.");
                    }
                }
                let wav_markers_available = self.playlist_render_mixer_stems
                    || render_format == PlaylistSongOutputFormat::Wav;
                ui.add_enabled_ui(wav_markers_available, |ui| {
                    ui.checkbox(
                        &mut self.playlist_render_save_markers,
                        "Save Playlist markers",
                    );
                    ui.checkbox(
                        &mut self.playlist_render_save_note_markers,
                        "Save note markers",
                    );
                });
                if wav_markers_available {
                    ui.label(
                        "Playlist markers save time markers; note markers include note onsets and Audio Clip starts.",
                    );
                }
                let mut render_quality_changed = false;
                ui.horizontal(|ui| {
                    ui.label("Resampling");
                    egui::ComboBox::from_id_salt("playlist-render-resampling-quality")
                        .selected_text(self.playlist_render_quality.label())
                        .show_ui(ui, |ui| {
                            for quality in ResamplingQuality::RENDER_OPTIONS {
                                render_quality_changed |= ui
                                    .selectable_value(
                                        &mut self.playlist_render_quality,
                                        quality,
                                        quality.label(),
                                    )
                                    .changed();
                            }
                        });
                });
                if render_quality_changed
                    && let Err(error) = save_playlist_render_quality(self.playlist_render_quality)
                {
                    self.status = format!("Could not save Playlist render quality: {error}");
                }
                ui.label("Hermite gives curved interpolation at low CPU cost; sinc reduces aliasing during pitch shifts and downsampling.");
                ui.horizontal(|ui| {
                    ui.label("Channels");
                    egui::ComboBox::from_id_salt("playlist-render-channel-mode")
                        .selected_text(self.playlist_render_channel_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in [
                                WavChannelMode::Stereo,
                                WavChannelMode::MonoMerged,
                                WavChannelMode::MonoLeft,
                                WavChannelMode::MonoRight,
                            ] {
                                ui.selectable_value(
                                    &mut self.playlist_render_channel_mode,
                                    mode,
                                    mode.label(),
                                );
                            }
                        });
                });
                ui.label("Merged mono averages both channels; left/right modes select one side.");
                ui.horizontal(|ui| {
                    ui.label("Tail length");
                    egui::ComboBox::from_id_salt("playlist-render-tail-length")
                        .selected_text(format!("{} seconds", self.playlist_render_tail_seconds))
                        .show_ui(ui, |ui| {
                            for seconds in [0, 1, 2, 5] {
                                ui.selectable_value(
                                    &mut self.playlist_render_tail_seconds,
                                    seconds,
                                    format!("{seconds} seconds"),
                                );
                            }
                        });
                });
                ui.label("Adds this much time after the last clip for instrument releases.");
                if self.playlist_render_mixer_stems {
                    ui.checkbox(
                        &mut self.playlist_render_include_master_stem,
                        "Include Master stem",
                    );
                    ui.label("Master uses the final supported mix; insert and Current stems are pre-effect.");
                    ui.add_enabled_ui(!current_track_options.is_empty(), |ui| {
                        ui.checkbox(
                            &mut self.playlist_render_include_current_stem,
                            "Include Current stem",
                        );
                    });
                    if current_track_options.is_empty() {
                        ui.label("No recognized, unmuted Mixer tracks are available as Current.");
                    } else if self.playlist_render_include_current_stem {
                        ui.horizontal(|ui| {
                            ui.label("Current track");
                            let selected_text = self
                                .playlist_render_current_track
                                .and_then(|track| {
                                    current_track_options
                                        .iter()
                                        .find(|(option, _)| *option == track)
                                })
                                .map(|(_, label)| label.as_str())
                                .unwrap_or("Select a track");
                            egui::ComboBox::from_id_salt("playlist-render-current-track")
                                .selected_text(selected_text)
                                .show_ui(ui, |ui| {
                                    for (track, label) in &current_track_options {
                                        ui.selectable_value(
                                            &mut self.playlist_render_current_track,
                                            Some(*track),
                                            label.as_str(),
                                        );
                                    }
                                });
                        });
                    }
                    ui.label("Mixer effects, automation, and route PDC are not included.");
                }
                ui.checkbox(
                    &mut self.playlist_render_include_plugin_tails,
                    "Use VST3 instrument tail reports",
                )
                .on_hover_text(format!(
                    "Extends to the longest finite VST3 instrument tail, up to {:.0} seconds. Infinite reports use the selected tail length.",
                    MAX_REPORTED_TAIL_SECONDS
                ));
                ui.horizontal(|ui| {
                    if ui.button("Renderâ€¦").clicked() {
                        start_render = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        let render_mixer_stems = self.playlist_render_mixer_stems;
        self.playlist_render_options_open = open && !start_render && !cancel;
        if !self.playlist_render_options_open {
            self.playlist_render_mixer_stems = false;
        }
        if start_render {
            if render_mixer_stems {
                self.render_mixer_stems_dialog();
            } else {
                self.render_playlist_dialog();
            }
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
        match self.encode_history_snapshot() {
            Ok(Some(current)) if current != snapshot => self.remember_undo_snapshot(snapshot),
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => self.status = format!("Could not capture undo state: {error}"),
        }
    }

    fn encode_history_snapshot(&self) -> Result<Option<Vec<u8>>, String> {
        let Some(mut document) = self.document.clone() else {
            return Ok(None);
        };
        // Host-controlled VST3 parameter edits change the live instance before the
        // FLP document. Capture that state into the clone so history includes it
        // without changing the project's saved bytes during ordinary editing.
        self.capture_loaded_vst3_state(&mut document);
        document
            .encode_lossless()
            .map(Some)
            .map_err(|error| error.to_string())
    }

    fn restore_loaded_vst3_state(&mut self) -> Vec<String> {
        let states = self
            .document
            .as_ref()
            .map(FlpDocument::channel_plugin_states)
            .unwrap_or_default();
        let instances = self.channel_vst3_instances.clone();
        let Some(host) = self.vst3_host.as_mut() else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        for state in states {
            let Some(instance_id) = instances.get(&state.channel_id()).copied() else {
                continue;
            };
            if let Err(error) = host.restore_flp_channel_state(instance_id, &state) {
                warnings.push(format!("channel {}: {error}", state.channel_id()));
            }
        }
        warnings
    }

    fn undo_document(&mut self) {
        self.finish_midi_mixer_control_undo_session();
        self.history_navigation_during_frame = true;
        let Some(snapshot) = self.undo_history.pop() else {
            self.status = "Nothing to undo".to_owned();
            return;
        };
        let current = match self.encode_history_snapshot() {
            Ok(Some(current)) => current,
            Ok(None) => {
                self.undo_history.push(snapshot);
                self.status = "Could not capture the current project for redo".to_owned();
                return;
            }
            Err(error) => {
                self.undo_history.push(snapshot);
                self.status = format!("Could not capture the current project for redo: {error}");
                return;
            }
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
        self.finish_midi_mixer_control_undo_session();
        self.history_navigation_during_frame = true;
        let Some(snapshot) = self.redo_history.pop() else {
            self.status = "Nothing to redo".to_owned();
            return;
        };
        let current = match self.encode_history_snapshot() {
            Ok(Some(current)) => current,
            Ok(None) => {
                self.redo_history.push(snapshot);
                self.status = "Could not capture the current project for undo".to_owned();
                return;
            }
            Err(error) => {
                self.redo_history.push(snapshot);
                self.status = format!("Could not capture the current project for undo: {error}");
                return;
            }
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
        let plugin_restore_warnings = self.restore_loaded_vst3_state();
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
            let project_settings = document.project_settings();
            self.project_settings_advanced_supported = project_settings.is_some();
            if let Some(settings) = project_settings {
                self.project_settings_play_truncated = settings.play_truncated_notes_in_clips;
                self.project_settings_fast_declick = settings.fast_declick_for_cut_groups;
            } else {
                self.project_settings_play_truncated = false;
                self.project_settings_fast_declick = false;
            }
            self.project_settings_pan_law_raw = document.metadata().pan_law_raw().unwrap_or(0);
            self.project_settings_master_pitch_cents =
                document.metadata().master_pitch_cents().unwrap_or(0);
            self.project_settings_master_pitch_changed = false;
            if let Some((numerator, denominator)) = document.metadata().time_signature() {
                self.project_settings_time_signature_enabled = true;
                self.project_settings_time_signature_numerator = numerator;
                self.project_settings_time_signature_denominator = denominator;
            } else {
                self.project_settings_time_signature_enabled = false;
                self.project_settings_time_signature_numerator = 4;
                self.project_settings_time_signature_denominator = 4;
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
        self.selected_playlist_clips.clear();
        self.playlist_selection_anchor = None;
        self.selected_playlist_track = None;
        self.selected_playlist_tracks.clear();
        self.playlist_track_selection_anchor = None;
        self.active_playlist_clip_drag = None;
        self.selected_time_marker = None;
        self.selected_automation_point = None;
        self.automation_add_mode = false;
        self.automation_interpolate_mode = false;
        self.automation_interpolation_anchor = None;
        self.active_note_drag = None;
        self.selected_piano_notes.clear();
        self.piano_roll_time_selection = None;
        self.piano_roll_time_selection_drag = None;
        self.piano_roll_pitch_selection_drag = None;
        self.piano_roll_selection_drag = None;
        self.dirty = self
            .document
            .as_ref()
            .and_then(project_hash)
            .zip(self.saved_project_hash)
            .is_none_or(|(current, saved)| current != saved);
        self.status = format!("{action} project edit");
        if !plugin_restore_warnings.is_empty() {
            self.status.push_str(&format!(
                " Â· VST3 state restore issue: {}",
                plugin_restore_warnings.join("; ")
            ));
        }
    }

    fn capture_loaded_vst3_state(&self, document: &mut FlpDocument) -> Vec<String> {
        let Some(host) = self.vst3_host.as_ref() else {
            return Vec::new();
        };
        let instances = &self.channel_vst3_instances;
        let mut warnings = Vec::new();
        for state in document.channel_plugin_states() {
            let Some(instance_id) = instances.get(&state.channel_id()).copied() else {
                continue;
            };
            let updated_state = match host.save_flp_channel_state(instance_id, &state) {
                Ok(updated_state) => updated_state,
                Err(error) => {
                    warnings.push(format!("channel {}: {error}", state.channel_id()));
                    continue;
                }
            };
            if let Err(error) =
                document.replace_vst3_channel_state_bytes(state.channel_id(), &updated_state)
            {
                warnings.push(format!("channel {}: {error}", state.channel_id()));
            }
        }
        warnings
    }

    fn write_project(&mut self, path: &Path) {
        self.finish_active_midi_recording();
        let Some(mut document) = self.document.as_ref().cloned() else {
            self.status = "Open a project before saving".to_owned();
            return;
        };
        let plugin_state_warnings = self.capture_loaded_vst3_state(&mut document);
        let encoded = match document.encode_lossless() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.status = format!("Could not encode project: {error}");
                return;
            }
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let backups = self.backup_before_manual_save(path);
        let mut package_workspace_after_save = None;
        let mut document_after_save = Some(document.clone());
        let mut sample_bundle_summary = None;
        let mut saved_bytes = encoded.clone();
        let save_result = if has_extension(path, "zip") {
            let source_project_path = self
                .sample_project_path()
                .or_else(|| self.current_path.clone())
                .unwrap_or_else(|| path.to_path_buf());
            let use_existing_workspace = self.package_workspace.is_some();
            let workspace = if let Some(workspace) = self.package_workspace.as_ref() {
                Ok(workspace.clone())
            } else {
                let project_name = path
                    .file_stem()
                    .map(|stem| format!("{}.flp", stem.to_string_lossy()))
                    .unwrap_or_else(|| "Project.flp".to_owned());
                ProjectPackageWorkspace::single_project(
                    &project_name,
                    &encoded,
                    &source_project_path,
                )
            };
            workspace.and_then(|workspace| {
                let mut packaged_document = document.clone();
                let summary = workspace
                    .bundle_channel_samples(&mut packaged_document, &source_project_path)?;
                let packaged_bytes = packaged_document
                    .encode_lossless()
                    .map_err(|error| error.to_string())?;
                workspace.write_to(path, &packaged_bytes)?;
                let saved_workspace = if use_existing_workspace {
                    workspace
                } else {
                    ProjectPackageWorkspace::open(path).map(|(workspace, _)| workspace)?
                };
                saved_bytes = packaged_bytes;
                package_workspace_after_save = Some(saved_workspace);
                document_after_save = Some(packaged_document);
                sample_bundle_summary = Some(summary);
                Ok(())
            })
        } else {
            fs::write(path, &encoded).map_err(|error| error.to_string())
        };
        match save_result {
            Ok(()) => {
                saved_bytes.hash(&mut hasher);
                let saved_hash = hasher.finish();
                self.clear_untitled_autosave();
                self.current_path = Some(path.to_path_buf());
                self.new_project_template_path = None;
                if let Some(workspace) = package_workspace_after_save {
                    self.package_workspace = Some(workspace);
                }
                if let Some(document) = document_after_save {
                    self.document = Some(document);
                }
                self.refresh_audio_waveform_paths();
                self.saved_project_hash = Some(saved_hash);
                self.dirty = false;
                self.autosave_deadline = None;
                self.last_autosave_path = latest_project_autosave(path);
                self.status = match backups {
                    Ok(paths) if !paths.is_empty() => {
                        format!("Saved {} Â· backup {}", path.display(), paths[0].display())
                    }
                    Err(error) => format!(
                        "Saved {} Â· previous version backup failed: {error}",
                        path.display()
                    ),
                    _ => format!("Saved {}", path.display()),
                };
                self.status
                    .push_str(&plugin_state_warning_suffix(&plugin_state_warnings));
                if let Some(summary) = sample_bundle_summary {
                    if summary.bundled_files > 0 {
                        self.status.push_str(&format!(
                            " Â· bundled {} channel sample{}",
                            summary.bundled_files,
                            if summary.bundled_files == 1 { "" } else { "s" }
                        ));
                    }
                    if summary.unresolved_references > 0 {
                        self.status.push_str(&format!(
                            " Â· {} channel sample reference{} not found",
                            summary.unresolved_references,
                            if summary.unresolved_references == 1 {
                                " was"
                            } else {
                                "s were"
                            }
                        ));
                    }
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

    fn queue_performance_track_stop(&mut self, track_id: u32) {
        let stop_position = self
            .audio_engine
            .as_ref()
            .zip(self.playlist_tempo_map.as_ref())
            .and_then(|(engine, tempo_map)| {
                let sample_rate = engine.sample_rate();
                let current_frame = engine.project_playback_position_frames();
                let safe_frame = current_frame.saturating_add(u64::from(sample_rate) / 8);
                let safe_tick = tempo_map.tick_at_frame(safe_frame, sample_rate)?;
                if !safe_tick.is_finite() || safe_tick < 0.0 || safe_tick > f64::from(u32::MAX) {
                    return None;
                }
                let start_tick = safe_tick.ceil() as u32;
                let start_frame = tempo_map
                    .frame_at_tick(u64::from(start_tick), sample_rate)
                    .ok()?;
                Some((start_tick, start_frame))
            });
        match (stop_position, self.performance_launch_sender.as_ref()) {
            (Some((start_tick, start_frame)), Some(sender)) => {
                match sender.send(PlaylistPerformanceCommand::StopTrack {
                    track_id,
                    start_tick: u64::from(start_tick),
                    start_frame,
                }) {
                    Ok(()) => {
                        self.status = format!(
                            "Stopping Performance Mode clip on track {track_id} at tick {start_tick}"
                        );
                    }
                    Err(_) => {
                        self.status = "Performance Mode audio worker is no longer available".into();
                    }
                }
            }
            _ => {
                self.status = "Could not map the Performance Mode stop position".into();
            }
        }
    }

    fn queue_performance_track_stop_at_tick(&mut self, track_id: u32, start_tick: u32) {
        let start_frame = self
            .audio_engine
            .as_ref()
            .zip(self.playlist_tempo_map.as_ref())
            .and_then(|(engine, tempo_map)| {
                tempo_map
                    .frame_at_tick(u64::from(start_tick), engine.sample_rate())
                    .ok()
            });
        match (start_frame, self.performance_launch_sender.as_ref()) {
            (Some(start_frame), Some(sender)) => {
                if sender
                    .send(PlaylistPerformanceCommand::StopTrack {
                        track_id,
                        start_tick: u64::from(start_tick),
                        start_frame,
                    })
                    .is_err()
                {
                    self.status = "Performance Mode audio worker is no longer available".into();
                }
            }
            _ => {
                self.status = "Could not map the Performance Mode scene stop position".into();
            }
        }
    }

    fn performance_clip_launch_request(
        &self,
        context: PerformanceLaunchContext<'_>,
        clip_index: usize,
        explicit_queue: bool,
    ) -> Option<(PlaylistPerformanceCommand, u32, bool, &'static str)> {
        let PerformanceLaunchContext {
            arrangement,
            tracks,
            ppq,
            initial_time_signature,
            signature_changes,
        } = context;
        let clip = arrangement.clips.get(clip_index)?;
        let clip_track_id = clip.playlist_track_id()?;
        let clip_track = tracks.iter().find(|track| track.id == clip_track_id);
        let clip_trigger_sync = clip_track.and_then(|track| track.trigger_sync);
        let clip_position_sync = clip_track.and_then(|track| track.position_sync);
        let clip_position_sync_label = clip_position_sync
            .map(playlist_track_sync_label)
            .unwrap_or("Off");
        let clip_tolerant = clip_track.and_then(|track| track.tolerant).unwrap_or(true);
        let queue_launch =
            explicit_queue || clip_track.is_some_and(|track| track.queued.unwrap_or(false));
        let (engine, tempo_map) = self
            .audio_engine
            .as_ref()
            .zip(self.playlist_tempo_map.as_ref())?;
        let sample_rate = engine.sample_rate();
        let current_device_frame = engine.project_playback_position_frames();
        let safe_device_frame = current_device_frame.saturating_add(u64::from(sample_rate) / 8);
        let current_tick = tempo_map.tick_at_frame(current_device_frame, sample_rate)?;
        let safe_tick = tempo_map.tick_at_frame(safe_device_frame, sample_rate)?;
        let global_snap_override = self
            .midi_performance_global_snap_mode
            .then_some(self.performance_global_snap);
        let trigger_snap_active = global_snap_override.map_or(
            clip_trigger_sync != Some(PlaylistTrackSync::Off),
            PerformanceGlobalSnap::uses_timing_grid,
        );
        let trigger_tick = |tick| {
            if let Some(global_snap) = global_snap_override {
                playlist_performance_global_snap_tick(
                    tick,
                    ppq,
                    initial_time_signature,
                    signature_changes,
                    global_snap,
                )
            } else {
                playlist_performance_trigger_tick(
                    tick,
                    ppq,
                    initial_time_signature,
                    signature_changes,
                    clip_trigger_sync,
                    clip.length_ticks,
                )
            }
        };
        let target_tick = if queue_launch {
            u32::try_from(safe_tick.ceil() as u64).ok()?
        } else {
            let mut target_tick = trigger_tick(safe_tick)?;
            if clip_tolerant && trigger_snap_active {
                let boundary_from_current = trigger_tick(current_tick)?;
                if f64::from(boundary_from_current) < safe_tick.ceil() {
                    target_tick = u32::try_from(safe_tick.ceil() as u64).ok()?;
                }
            }
            target_tick
        };
        let start_frame = tempo_map
            .frame_at_tick(u64::from(target_tick), sample_rate)
            .ok()?;
        let position_offset_ticks = playlist_performance_position_offset_ticks(
            target_tick,
            clip.position_ticks,
            clip.length_ticks,
            ppq,
            initial_time_signature,
            signature_changes,
            clip_position_sync,
        );
        let launch = PlaylistPerformanceLaunch {
            clip_index,
            start_tick: u64::from(target_tick),
            start_frame,
            position_offset_ticks,
        };
        let command = if queue_launch {
            PlaylistPerformanceCommand::Enqueue(launch)
        } else {
            PlaylistPerformanceCommand::Launch(launch)
        };
        Some((command, target_tick, queue_launch, clip_position_sync_label))
    }

    fn performance_supported_clip_indices(
        &self,
        document: &FlpDocument,
        arrangement_id: u16,
        performance_zone_end_tick: u32,
    ) -> Result<BTreeSet<usize>, String> {
        let loaded_vst3_channels = self
            .channel_vst3_instances
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        playlist_performance_clip_indices_with_vst3(
            document,
            arrangement_id,
            performance_zone_end_tick,
            &loaded_vst3_channels,
        )
    }

    fn queue_performance_hold_release(
        &mut self,
        arrangement_id: u16,
        track_id: u32,
        clip_index: usize,
    ) {
        let press = self.document.as_ref().and_then(|document| {
            document
                .playlist_tracks_in_arrangement(arrangement_id)
                .into_iter()
                .find(|track| track.id == track_id)
                .and_then(|track| track.press)
        });
        if press != Some(PlaylistTrackPress::HoldMotion) {
            self.queue_performance_track_stop(track_id);
            return;
        }

        let motion_position = self
            .audio_engine
            .as_ref()
            .zip(self.playlist_tempo_map.as_ref())
            .and_then(|(engine, tempo_map)| {
                let sample_rate = engine.sample_rate();
                let current_frame = engine.project_playback_position_frames();
                let safe_frame = current_frame.saturating_add(u64::from(sample_rate) / 8);
                let safe_tick = tempo_map.tick_at_frame(safe_frame, sample_rate)?;
                if !safe_tick.is_finite() || safe_tick < 0.0 || safe_tick > f64::from(u32::MAX) {
                    return None;
                }
                let start_tick = safe_tick.ceil() as u32;
                let start_frame = tempo_map
                    .frame_at_tick(u64::from(start_tick), sample_rate)
                    .ok()?;
                Some((u64::from(start_tick), start_frame))
            });
        match (motion_position, self.performance_launch_sender.as_ref()) {
            (Some((start_tick, start_frame)), Some(sender)) => {
                match sender.send(PlaylistPerformanceCommand::AdvanceMotion {
                    clip_index,
                    track_id,
                    start_tick,
                    start_frame,
                }) {
                    Ok(()) => {
                        self.status = format!(
                            "Advancing Performance Mode motion on track {track_id} at tick {start_tick}"
                        );
                    }
                    Err(_) => {
                        self.status = "Performance Mode audio worker is no longer available".into();
                    }
                }
            }
            _ => {
                self.status = "Could not map Performance Mode motion timing".into();
            }
        }
    }

    fn release_all_midi_performance_clips(&mut self) {
        let held = std::mem::take(&mut self.midi_held_performance_clips);
        self.midi_performance_page_navigation = false;
        self.midi_performance_global_snap_mode = false;
        self.midi_performance_scene_mode = false;
        self.midi_performance_plus_scene_mode = false;
        for (_, held_clips) in held {
            for clip in held_clips {
                self.queue_performance_hold_release(
                    clip.arrangement_id,
                    clip.track_id,
                    clip.clip_index,
                );
            }
        }
    }

    fn handle_midi_performance_note(&mut self, message: ReceivedMidiMessage) {
        let performance_channel = self.performance_midi_channel & 0x0F;
        match message.event {
            MidiInputEvent::NoteOn {
                channel,
                key: key @ 120..=124,
                velocity,
            } if channel == performance_channel
                && velocity > 0
                && self.performance_mode_active
                && self.playing =>
            {
                self.navigate_midi_performance_playlist(key);
            }
            MidiInputEvent::NoteOn {
                channel,
                key: 127,
                velocity,
            } if channel == performance_channel
                && velocity > 0
                && self.performance_mode_active
                && self.playing =>
            {
                self.midi_performance_global_snap_mode = true;
                self.status = format!(
                    "MIDI Global Snap held: {}",
                    self.performance_global_snap.label()
                );
            }
            MidiInputEvent::NoteOff {
                channel, key: 127, ..
            }
            | MidiInputEvent::NoteOn {
                channel,
                key: 127,
                velocity: 0,
            } if channel == performance_channel => {
                self.midi_performance_global_snap_mode = false;
            }
            MidiInputEvent::NoteOn {
                channel,
                key: 125,
                velocity,
            } if channel == performance_channel
                && velocity > 0
                && self.performance_mode_active
                && self.playing =>
            {
                self.midi_performance_scene_mode = true;
            }
            MidiInputEvent::NoteOff {
                channel, key: 125, ..
            }
            | MidiInputEvent::NoteOn {
                channel,
                key: 125,
                velocity: 0,
            } if channel == performance_channel => {
                self.midi_performance_scene_mode = false;
            }
            MidiInputEvent::NoteOn {
                channel,
                key: 126,
                velocity,
            } if channel == performance_channel
                && velocity > 0
                && self.performance_mode_active
                && self.playing =>
            {
                self.midi_performance_plus_scene_mode = true;
            }
            MidiInputEvent::NoteOff {
                channel, key: 126, ..
            }
            | MidiInputEvent::NoteOn {
                channel,
                key: 126,
                velocity: 0,
            } if channel == performance_channel => {
                self.midi_performance_plus_scene_mode = false;
            }
            MidiInputEvent::NoteOn {
                channel,
                key,
                velocity,
            } if channel == performance_channel && velocity > 0 => {
                if self.midi_performance_scene_mode || self.midi_performance_plus_scene_mode {
                    self.trigger_performance_midi_scene(
                        channel,
                        key,
                        self.midi_performance_plus_scene_mode,
                    );
                } else {
                    self.trigger_performance_midi_clip(channel, key);
                }
            }
            MidiInputEvent::NoteOff { channel, key, .. }
            | MidiInputEvent::NoteOn {
                channel,
                key,
                velocity: 0,
            } if channel == performance_channel => {
                if let Some(held_clips) = self.midi_held_performance_clips.remove(&(channel, key)) {
                    for clip in held_clips {
                        self.queue_performance_hold_release(
                            clip.arrangement_id,
                            clip.track_id,
                            clip.clip_index,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn navigate_midi_performance_playlist(&mut self, key: u8) {
        if key == 124 {
            self.midi_performance_page_navigation = !self.midi_performance_page_navigation;
            self.status = if self.midi_performance_page_navigation {
                "MIDI Performance Mode navigation moves 12 cells per press".to_owned()
            } else {
                "MIDI Performance Mode navigation moves one cell per press".to_owned()
            };
            return;
        }

        let step_count = if self.midi_performance_page_navigation {
            12
        } else {
            1
        };
        let current_offset = self
            .playlist_pending_scroll_offset
            .unwrap_or(self.playlist_grid_scroll_offset);
        match key {
            120 | 121 => {
                let row_height = (27.0 * self.playlist_track_zoom).max(1.0);
                let current_track = ((current_offset.y - 34.0).max(0.0) / row_height).floor();
                let target_track = if key == 120 {
                    (current_track as i64 - i64::from(step_count)).max(0)
                } else {
                    current_track as i64 + i64::from(step_count)
                };
                let target_y = if target_track == 0 {
                    0.0
                } else {
                    34.0 + target_track as f32 * row_height
                };
                self.playlist_pending_scroll_offset = Some(Vec2::new(current_offset.x, target_y));
            }
            122 | 123 => {
                let Some(document) = self.document.as_ref() else {
                    return;
                };
                let arrangements = document.arrangements().unwrap_or_default();
                let Some(arrangement) = arrangements
                    .iter()
                    .find(|arrangement| Some(arrangement.id) == self.selected_arrangement)
                    .or_else(|| arrangements.first())
                else {
                    return;
                };
                let ppq = document.header().ppq().max(1);
                let initial_time_signature = document.metadata().time_signature();
                let signature_changes = arrangement
                    .time_markers
                    .iter()
                    .filter(|marker| marker.is_signature())
                    .filter_map(|marker| {
                        Some((
                            marker.position_ticks(),
                            marker.numerator()?.max(1),
                            marker.denominator()?.max(1),
                        ))
                    })
                    .collect::<Vec<_>>();
                let timeline_zoom = self.timeline_zoom.max(0.001);
                let current_tick = ((current_offset.x - PLAYLIST_LABEL_WIDTH).max(0.0)
                    / timeline_zoom)
                    .floor()
                    .clamp(0.0, u32::MAX as f32) as u32;
                let target_tick = playlist_performance_navigation_target_tick(
                    arrangement,
                    current_tick,
                    key == 123,
                    step_count,
                    ppq,
                    initial_time_signature,
                    &signature_changes,
                );
                let target_x = if target_tick == 0 {
                    0.0
                } else {
                    PLAYLIST_LABEL_WIDTH + target_tick as f32 * timeline_zoom
                };
                self.playlist_pending_scroll_offset = Some(Vec2::new(target_x, current_offset.y));
            }
            _ => return,
        }
        self.status = format!(
            "Scrolled the Performance Mode Playlist {}",
            match key {
                120 => "up",
                121 => "down",
                122 => "left",
                123 => "right",
                _ => unreachable!(),
            }
        );
    }

    fn trigger_performance_midi_scene(&mut self, channel: u8, key: u8, additive: bool) {
        if (channel & 0x0F) != (self.performance_midi_channel & 0x0F)
            || !self.performance_mode_active
            || !self.playing
            || self.performance_launch_sender.is_none()
            || key >= 120
        {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let Ok(arrangements) = document.arrangements() else {
            return;
        };
        let Some(arrangement) = arrangements
            .iter()
            .find(|arrangement| Some(arrangement.id) == self.selected_arrangement)
            .or_else(|| arrangements.first())
            .cloned()
        else {
            return;
        };
        let Some(performance_zone_end_tick) =
            playlist_performance_zone_start_tick(&arrangement.time_markers)
        else {
            return;
        };
        let Ok(supported_clip_indices) = self.performance_supported_clip_indices(
            document,
            arrangement.id,
            performance_zone_end_tick,
        ) else {
            return;
        };
        let mut tracks = document.playlist_tracks_in_arrangement(arrangement.id);
        tracks.sort_by_key(|track| track.id);
        let project_time_signature = document.metadata().time_signature().unwrap_or((4, 4));
        let signature_changes = arrangement
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature())
            .filter_map(|marker| {
                Some((
                    marker.position_ticks(),
                    marker.numerator()?.max(1),
                    marker.denominator()?.max(1),
                ))
            })
            .collect::<Vec<_>>();
        let ppq = document.header().ppq().max(1);
        let Some(scene_tick) = playlist_performance_midi_scene_tick(
            &arrangement,
            key % 12,
            performance_zone_end_tick,
            ppq,
            Some(project_time_signature),
            &signature_changes,
        ) else {
            return;
        };
        let Some((scene_key, scene_clip_groups)) = playlist_performance_scene_clip_indices(
            &arrangement,
            &supported_clip_indices,
            scene_tick,
            PlaylistPerformanceSceneContext {
                performance_zone_end_tick,
                ppq,
                initial_time_signature: Some(project_time_signature),
                signature_changes: &signature_changes,
                clip_group: None,
                track_group_range: None,
            },
        ) else {
            return;
        };
        let mut launch_actions = Vec::new();
        for (&track_id, clip_indices) in &scene_clip_groups {
            if clip_indices.is_empty() {
                continue;
            }
            let round_robin_key = (arrangement.id, scene_key, track_id);
            let next_position = self
                .performance_scene_round_robin
                .get(&round_robin_key)
                .copied()
                .unwrap_or(0)
                % clip_indices.len();
            let clip_index = clip_indices[next_position];
            self.performance_scene_round_robin
                .insert(round_robin_key, (next_position + 1) % clip_indices.len());
            let Some(track) = tracks.iter().find(|track| track.id == track_id) else {
                continue;
            };
            let Some((command, start_tick, queued, position_sync_label)) = self
                .performance_clip_launch_request(
                    PerformanceLaunchContext {
                        arrangement: &arrangement,
                        tracks: &tracks,
                        ppq,
                        initial_time_signature: Some(project_time_signature),
                        signature_changes: &signature_changes,
                    },
                    clip_index,
                    false,
                )
            else {
                continue;
            };
            launch_actions.push((
                track_id,
                clip_index,
                command,
                start_tick,
                queued,
                position_sync_label,
                track.press,
            ));
        }
        if launch_actions.is_empty() {
            self.status = "No supported clips in the MIDI-selected Performance Mode scene".into();
            return;
        }

        let Some(scene_start_tick) = launch_actions
            .iter()
            .map(|(_, _, _, start_tick, _, _, _)| *start_tick)
            .min()
        else {
            return;
        };
        if !additive {
            let launched_track_ids = launch_actions
                .iter()
                .map(|(track_id, ..)| *track_id)
                .collect::<BTreeSet<_>>();
            for track in &tracks {
                if !launched_track_ids.contains(&track.id) {
                    self.queue_performance_track_stop_at_tick(track.id, scene_start_tick);
                }
            }
        }

        let Some(sender) = self.performance_launch_sender.as_ref() else {
            return;
        };
        let mut launched_tracks = 0usize;
        for (track_id, clip_index, command, start_tick, queued, position_sync_label, press) in
            launch_actions
        {
            if sender.send(command).is_err() {
                self.status = "Performance Mode audio worker is no longer available".into();
                return;
            }
            launched_tracks += 1;
            if matches!(
                press,
                Some(PlaylistTrackPress::HoldStop | PlaylistTrackPress::HoldMotion)
            ) {
                self.midi_held_performance_clips
                    .entry((channel, key))
                    .or_default()
                    .push(MidiHeldPerformanceClip {
                        arrangement_id: arrangement.id,
                        track_id,
                        clip_index,
                    });
            }
            if queued {
                self.status = format!("MIDI queued Playlist Clip {} for its track", clip_index + 1);
            } else {
                self.status = format!(
                    "MIDI scheduled Playlist Clip {} for tick {start_tick} with Position Sync {position_sync_label}",
                    clip_index + 1
                );
            }
        }
        let scene_kind = if additive { "+Scene" } else { "Scene" };
        let scene_label = match scene_key {
            PlaylistPerformanceSceneKey::Marker(tick) => format!("at tick {tick}"),
            PlaylistPerformanceSceneKey::ClipGroup(group) => format!("for Clip Group {group}"),
        };
        self.status = format!(
            "MIDI {scene_kind} triggered {scene_label} across {launched_tracks} Playlist tracks"
        );
    }

    fn trigger_performance_midi_clip(&mut self, channel: u8, key: u8) {
        if (channel & 0x0F) != (self.performance_midi_channel & 0x0F)
            || !self.performance_mode_active
            || !self.playing
            || self.performance_launch_sender.is_none()
            || key >= 120
        {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let Ok(arrangements) = document.arrangements() else {
            return;
        };
        let Some(arrangement) = arrangements
            .iter()
            .find(|arrangement| Some(arrangement.id) == self.selected_arrangement)
            .or_else(|| arrangements.first())
            .cloned()
        else {
            return;
        };
        let Some(performance_zone_end_tick) =
            playlist_performance_zone_start_tick(&arrangement.time_markers)
        else {
            return;
        };
        let Ok(supported_clip_indices) = self.performance_supported_clip_indices(
            document,
            arrangement.id,
            performance_zone_end_tick,
        ) else {
            return;
        };
        let mut tracks = document.playlist_tracks_in_arrangement(arrangement.id);
        tracks.sort_by_key(|track| track.id);
        let track_index = usize::from(key / 12);
        if track_index >= 10 {
            return;
        }
        let Some(track) = tracks.get(track_index) else {
            return;
        };
        let project_time_signature = document.metadata().time_signature().unwrap_or((4, 4));
        let signature_changes = arrangement
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature())
            .filter_map(|marker| {
                Some((
                    marker.position_ticks(),
                    marker.numerator()?.max(1),
                    marker.denominator()?.max(1),
                ))
            })
            .collect::<Vec<_>>();
        let ppq = document.header().ppq().max(1);
        let Some(scene_tick) = playlist_performance_midi_scene_tick(
            &arrangement,
            key % 12,
            performance_zone_end_tick,
            ppq,
            Some(project_time_signature),
            &signature_changes,
        ) else {
            return;
        };
        let Some((scene_key, scene_clip_groups)) = playlist_performance_scene_clip_indices(
            &arrangement,
            &supported_clip_indices,
            scene_tick,
            PlaylistPerformanceSceneContext {
                performance_zone_end_tick,
                ppq,
                initial_time_signature: Some(project_time_signature),
                signature_changes: &signature_changes,
                clip_group: None,
                track_group_range: None,
            },
        ) else {
            return;
        };
        let Some(scene_clips) = scene_clip_groups
            .get(&track.id)
            .filter(|clips| !clips.is_empty())
        else {
            return;
        };
        let round_robin_key = (arrangement.id, scene_key, track.id);
        let next_position = self
            .performance_scene_round_robin
            .get(&round_robin_key)
            .copied()
            .unwrap_or(0)
            % scene_clips.len();
        let clip_index = scene_clips[next_position];
        self.performance_scene_round_robin
            .insert(round_robin_key, (next_position + 1) % scene_clips.len());
        let press = track.press;
        let Some((command, start_tick, queued, position_sync_label)) = self
            .performance_clip_launch_request(
                PerformanceLaunchContext {
                    arrangement: &arrangement,
                    tracks: &tracks,
                    ppq,
                    initial_time_signature: Some(project_time_signature),
                    signature_changes: &signature_changes,
                },
                clip_index,
                false,
            )
        else {
            self.status = "Could not map MIDI Performance Mode launch timing or position".into();
            return;
        };
        let Some(sender) = self.performance_launch_sender.as_ref() else {
            return;
        };
        match sender.send(command) {
            Ok(()) => {
                if matches!(
                    press,
                    Some(PlaylistTrackPress::HoldStop | PlaylistTrackPress::HoldMotion)
                ) {
                    self.midi_held_performance_clips
                        .entry((channel, key))
                        .or_default()
                        .push(MidiHeldPerformanceClip {
                            arrangement_id: arrangement.id,
                            track_id: track.id,
                            clip_index,
                        });
                }
                self.status = if queued {
                    format!(
                        "MIDI triggered Playlist Clip {} into its track queue",
                        clip_index + 1
                    )
                } else {
                    format!(
                        "MIDI triggered Playlist Clip {} for tick {start_tick} with Position Sync {position_sync_label}",
                        clip_index + 1
                    )
                };
            }
            Err(_) => {
                self.status = "Performance Mode audio worker is no longer available".into();
            }
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
        if self.pattern_playback_mode {
            self.start_pattern_mode_playback();
            return;
        }
        if self.performance_mode_active {
            self.start_performance_mode_playback();
            return;
        }
        self.stop_live_vst3_input();
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
        self.initialize_live_mixer_insert_fader_targets(
            self.document.as_ref().expect("project was checked above"),
        );
        let options = PlaylistRenderOptions {
            arrangement_id: self.selected_arrangement.unwrap_or(0),
            sample_rate,
            resampling_quality: self.live_resampling_quality,
            tail_seconds: 2,
            soloed_playlist_track_range: self.soloed_playlist_track_range,
            read_sample_root_note: self.read_sample_root_note,
            ..PlaylistRenderOptions::default()
        };
        let arrangement_id = options.arrangement_id;
        let playlist_tempo_map = match PlaylistTempoMap::for_arrangement(
            self.document.as_ref().expect("project was checked above"),
            options.arrangement_id,
        ) {
            Ok(tempo_map) => tempo_map,
            Err(error) => {
                self.status = format!("Could not prepare Playlist tempo map: {error}");
                return;
            }
        };
        let vst3_processor = self
            .vst3_host
            .as_ref()
            .map(|host| {
                host.prepare_playlist_stream(
                    self.document.as_ref().expect("project was checked above"),
                    options,
                    &self.channel_vst3_instances,
                    true,
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
        let live_midi_senders = vst3_processor
            .as_ref()
            .map(Vst3PlaylistStreamProcessor::live_midi_input_senders)
            .unwrap_or_default();
        let start_frame = match playlist_seek_tick_to_frame(
            self.playlist_seek_tick,
            &playlist_tempo_map,
            sample_rate,
        ) {
            Ok(frame) => frame,
            Err(error) => {
                self.status = format!("Could not map Playlist seek position: {error}");
                return;
            }
        };
        if let Some(session) = self.live_sampler_input.take() {
            session.cancelled.store(true, Ordering::Release);
            self.live_sampler_workers.push(session.worker);
        }
        self.midi_live_sampler_note_mapping.clear();
        self.midi_live_layer_note_mapping.clear();
        if let Some(session) = self.live_layer_input.take() {
            session.cancelled.store(true, Ordering::Release);
            self.live_vst3_workers.push(session.worker);
        }
        if let Some(session) = self.live_vst3_input.take() {
            session.cancelled.store(true, Ordering::Release);
            self.live_vst3_workers.push(session.worker);
        }
        self.midi_live_vst3_note_mapping.clear();
        let writer = match engine.begin_streaming_playback_from_frame(start_frame) {
            Ok(writer) => writer,
            Err(error) => {
                self.status = format!("Could not start Playlist audio output: {error}");
                return;
            }
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        if let Some(document) = self.document.as_ref() {
            self.initialize_live_master_mixer_targets(document);
        }
        let worker_mixer_meters = Arc::clone(&self.mixer_track_peak_meters);
        worker_mixer_meters.clear();
        self.mixer_insert_peak_holds.fill(0.0);
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("project-audio-render".to_owned())
            .spawn(move || {
                let result = FlpDocument::parse(&project_bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|document| {
                        stream_playlist_with_vst3_to_device_with_monitor(
                            &document,
                            &project_path,
                            options,
                            &writer,
                            &worker_cancelled,
                            vst3_processor,
                            PlaylistStreamMonitor::with_meters(start_frame, &worker_mixer_meters),
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
                self.playlist_tempo_map = Some(playlist_tempo_map);
                self.pending_audio_render = Some(PendingAudioRender {
                    receiver,
                    cancelled,
                    worker,
                    live_midi_senders,
                });
                self.stop_midi_pattern_preview();
                self.status =
                    "Preparing Playlist audio, Samplers, and VST3 instrumentsâ€¦".to_owned();
                self.start_midi_clock(false);
                self.start_midi_playlist_playback(
                    arrangement_id,
                    u64::from(self.playlist_seek_tick),
                    start_frame,
                );
            }
            Err(error) => {
                self.stop_project_playback();
                self.status = format!("Could not start audio streaming: {error}");
            }
        }
    }

    fn initialize_live_mixer_insert_fader_targets(&self, document: &FlpDocument) {
        let parameters = document.mixer_parameter_records().unwrap_or_default();
        for track in 1..=i8::MAX as usize {
            let volume_raw =
                unique_candidate_mixer_parameter(&parameters, track, MixerParameterKind::Volume)
                    .map(MixerParameterRecord::value)
                    .filter(|value| (0..=16_000).contains(value));
            self.mixer_track_peak_meters
                .set_live_mixer_insert_fader_raw(track, volume_raw);
        }
    }

    fn start_pattern_mode_playback(&mut self) {
        let Some(pattern_id) = self.selected_pattern else {
            self.status = "Select a Pattern before starting Pattern mode".to_owned();
            return;
        };
        let loop_length_ticks = match self.selected_pattern_loop_length_ticks(pattern_id) {
            Ok(length_ticks) => length_ticks,
            Err(error) => {
                self.status = format!("Could not determine the Pattern loop length: {error}");
                return;
            }
        };
        let loop_start_tick = self.pattern_seek_tick % loop_length_ticks;
        self.stream_selected_sampler_pattern_from_tick(Some(loop_length_ticks), loop_start_tick);
        self.pattern_seek_tick = loop_start_tick;
        if self.playing {
            self.pattern_mode_playback_source = Some((pattern_id, loop_length_ticks));
            self.start_midi_clock(false);
            self.start_midi_pattern_transport(pattern_id, loop_length_ticks);
        }
    }

    fn seek_pattern_mode_to_tick(&mut self, pattern_id: u16, requested_tick: u32) {
        if !self.pattern_playback_mode || self.selected_pattern != Some(pattern_id) {
            return;
        }
        let loop_length_ticks = match self.selected_pattern_loop_length_ticks(pattern_id) {
            Ok(length_ticks) if length_ticks > 0 => length_ticks,
            Ok(_) => {
                self.status = "Could not seek a Pattern with an empty loop".to_owned();
                return;
            }
            Err(error) => {
                self.status = format!("Could not seek Pattern playback: {error}");
                return;
            }
        };
        let seek_tick = u64::from(requested_tick) % loop_length_ticks;
        let restart_playback = self.playing;
        let playback_loaded = self.project_playback_loaded
            || self.pending_audio_render.is_some()
            || self.pending_vst3_stream.is_some()
            || self.pending_sampler_stream.is_some();
        if restart_playback || playback_loaded {
            self.stop_project_playback();
        }
        self.pattern_seek_tick = seek_tick;
        self.status = format!("Pattern {pattern_id} position set to tick {seek_tick}");
        if restart_playback {
            self.start_pattern_mode_playback();
        }
    }

    fn selected_pattern_loop_length_ticks(&self, pattern_id: u16) -> Result<u64, String> {
        let document = self
            .document
            .as_ref()
            .ok_or_else(|| "open a project first".to_owned())?;
        let pattern = document
            .patterns()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|pattern| pattern.id == pattern_id)
            .ok_or_else(|| format!("Pattern {pattern_id} was not found"))?;
        let signature = pattern
            .time_markers
            .iter()
            .filter(|marker| marker.is_signature() && marker.position_ticks() == 0)
            .find_map(|marker| Some((marker.numerator()?, marker.denominator()?)))
            .or_else(|| document.metadata().time_signature())
            .unwrap_or((4, 4));
        let ppq = u64::from(document.header().ppq().max(1));
        let bar_ticks =
            (ppq * u64::from(signature.0.max(1)) * 4 / u64::from(signature.1.max(1))).max(1);
        let last_note_end = pattern
            .notes
            .iter()
            .map(|note| u64::from(note.position).saturating_add(u64::from(note.length.max(1))))
            .max()
            .unwrap_or(0);
        let explicit_length = pattern.length_ticks.map(u64::from).unwrap_or(0);
        Ok(if explicit_length == 0 {
            last_note_end.max(bar_ticks)
        } else {
            explicit_length.max(last_note_end)
        })
    }

    fn initialize_live_master_mixer_targets(&self, document: &FlpDocument) {
        let live_master_fader_gain = candidate_mixer_master_fader_gain(document);
        self.mixer_track_peak_meters
            .set_live_master_fader_gain(live_master_fader_gain);
        self.mixer_track_peak_meters
            .set_live_master_pan_raw(candidate_mixer_master_pan_raw(document));
        self.mixer_track_peak_meters
            .set_live_master_stereo_separation_raw(candidate_mixer_master_stereo_separation_raw(
                document,
            ));
        self.mixer_track_peak_meters
            .set_live_master_enabled(candidate_mixer_master_enabled(document));
        self.mixer_track_peak_meters
            .set_live_master_signal_flags(candidate_mixer_master_signal_flags(document));

        let parameters = document.mixer_parameter_records().unwrap_or_default();
        let inserts = document.mixer_inserts();
        let soloed_tracks = inserts
            .iter()
            .filter(|insert| insert.ordinal() > 0 && insert.soloed() == Some(true))
            .map(MixerInsertSummary::ordinal)
            .collect::<Vec<_>>();
        self.mixer_track_peak_meters
            .set_live_mixer_soloed_tracks(&soloed_tracks);
        for track in 1..=i8::MAX as usize {
            let live_fader_raw =
                unique_candidate_mixer_parameter(&parameters, track, MixerParameterKind::Volume)
                    .map(MixerParameterRecord::value)
                    .filter(|value| (0..=16_000).contains(value));
            let live_pan_raw =
                unique_candidate_mixer_parameter(&parameters, track, MixerParameterKind::Pan)
                    .map(MixerParameterRecord::value)
                    .filter(|value| (-6_400..=6_400).contains(value));
            let live_stereo_separation_raw = candidate_mixer_stereo_separation(&parameters, track)
                .map(MixerParameterRecord::value);
            self.mixer_track_peak_meters
                .set_live_mixer_insert_fader_raw(track, live_fader_raw);
            self.mixer_track_peak_meters
                .set_live_mixer_insert_pan_raw(track, live_pan_raw);
            self.mixer_track_peak_meters
                .set_live_mixer_insert_stereo_separation_raw(track, live_stereo_separation_raw);
            self.mixer_track_peak_meters
                .set_live_mixer_insert_signal_flags(
                    track,
                    candidate_mixer_insert_signal_flags(&inserts, track),
                );
            self.mixer_track_peak_meters.set_live_mixer_insert_enabled(
                track,
                candidate_mixer_insert_enabled(&inserts, track),
            );
        }
        let sends = document.mixer_send_summaries().unwrap_or_default();
        for send in &sends {
            if let Some((source_track, destination_track)) =
                candidate_live_mixer_send_key(&sends, send)
            {
                self.mixer_track_peak_meters.set_live_mixer_send_level_raw(
                    source_track,
                    destination_track,
                    Some(send.value()),
                );
                self.mixer_track_peak_meters.set_live_mixer_send_enabled(
                    source_track,
                    destination_track,
                    send.enabled(),
                );
            }
        }
    }

    fn start_performance_mode_playback(&mut self) {
        if self.pending_song_render.is_some() {
            self.status = "Stop the Playlist render before starting playback".to_owned();
            return;
        }
        if self.pending_audio_render.is_some() {
            self.status = "Project audio is already being prepared".to_owned();
            return;
        }
        self.stop_live_vst3_input();
        let Some(document) = self.document.as_ref() else {
            self.status = "Open an FL Studio project before playing".to_owned();
            return;
        };
        let Some(project_path) = self.sample_project_path() else {
            self.status = "Save the project to a file before playing its audio clips".to_owned();
            return;
        };
        let arrangement_id = self.selected_arrangement.unwrap_or(0);
        let arrangement = match document
            .arrangements()
            .map_err(|error| error.to_string())
            .and_then(|arrangements| {
                arrangements
                    .into_iter()
                    .find(|arrangement| arrangement.id == arrangement_id)
                    .ok_or_else(|| format!("arrangement {arrangement_id} was not found"))
            }) {
            Ok(arrangement) => arrangement,
            Err(error) => {
                self.status = format!("Could not find the selected arrangement: {error}");
                return;
            }
        };
        let Some(start_tick) = playlist_performance_zone_start_tick(&arrangement.time_markers)
        else {
            self.status = "Performance Mode needs a non-signature time marker named Start".into();
            return;
        };
        let loaded_vst3_channels = self
            .channel_vst3_instances
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let performance_clip_indices =
            match self.performance_supported_clip_indices(document, arrangement_id, start_tick) {
                Ok(clip_indices) => clip_indices,
                Err(error) => {
                    self.status = format!("Could not prepare Performance Mode clips: {error}");
                    return;
                }
            };
        if performance_clip_indices.is_empty() {
            self.status =
                "Add a supported Audio Clip, Sampler Pattern Clip, or loaded VST3 Pattern Clip before Start"
                    .into();
            return;
        }
        let performance_vst3_clip_indices = match playlist_performance_vst3_clip_indices(
            document,
            arrangement_id,
            start_tick,
            &loaded_vst3_channels,
        ) {
            Ok(indices) => indices
                .intersection(&performance_clip_indices)
                .copied()
                .collect::<BTreeSet<_>>(),
            Err(error) => {
                self.status = format!("Could not prepare Performance Mode VST3 clips: {error}");
                return;
            }
        };
        let performance_vst3_channel_ids = match playlist_performance_vst3_channel_ids(
            document,
            arrangement_id,
            &performance_vst3_clip_indices,
        ) {
            Ok(channels) => channels,
            Err(error) => {
                self.status = format!("Could Û}·ãÎÊ×¬¢h­µç]]œÚ[\‹œš[X\žWÙÝÛŠ
Bˆ[œ]œÚ[\‹œš[X\žWÜ™[X\ÙY

Bˆ[œ]œÚ[\‹˜]Û—Ü™\ÜÙY
Ú[\]ÛŽŽ”ÙXÛÛ™\žJBˆ[œ]œÚ[\‹˜]Û—ÙÝÛŠÚ[\]ÛŽŽ”ÙXÛÛ™\žJBˆ[œ]œÚ[\‹˜]Û—Ü™[X\ÙY
Ú[\]ÛŽŽ”ÙXÛÛ™\žJBˆ[œ]™]™[Ëš]\Š
K˜[žJ]™[ÂˆX]Ú\ÈJˆ]™[ˆYÝZNŽ‘]™[Ž’Ù^HÈ™\ÜÙYˆYK‹ˆBˆYÝZNŽ‘]™[Ž•^
ÊBˆYÝZNŽ‘]™[Ž”\ÝJÊBˆYÝZNŽ‘]™[Ž“[Ý\ÙUÚY[È‹ˆBˆ
BˆJBˆJNÂˆYˆZ\ÝÜžWÛ˜]šYØ][Û‚ˆ	‰ˆœ˜[YWÜÛ˜\ÚÝš\×Û›Û™J
Bˆ	‰ˆÙ[‹œ[™[™×Ú\ÝÜžWÜÛ˜\ÚÝš\×Û›Û™J
Bˆ	‰ˆÚ[ÙY]Ú[œ]Ü™XÙZ]™YˆÂˆœ˜[YWÜÛ˜\ÚÝHX]ÚÙ[‹™[˜ÛÙWÚ\ÝÜžWÜÛ˜\ÚÝ

HÂˆÚÊÛ˜\ÚÝ
HOˆÛ˜\ÚÝˆ\œŠ\œ›ÜŠHOˆÂˆÙ[‹œÝ]\ÈH›Ü›X]JÛÝ[›ÝØ\\™H[™ÈÝ]NˆÙ\œ›ÜŸHŠNÂˆ›Û™BˆBˆNÂˆBˆÙ[‹š\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›WÝ\×Ùœ˜[YHBˆœ˜[YWÜÛ˜\ÚÝš\×ÜÛÛYJ
HÙ[‹œ[™[™×Ú\ÝÜžWÜÛ˜\ÚÝš\×ÜÛÛYJ
NÂˆÙ[‹œ^[\Ý
šY]ÜÜÝZJNÂˆJNÂˆBˆYˆÙ[‹œYÚ[œ×ÝÚ[™Ý×Ù]XÚYÂˆ]šY]ÜÜÚYHYÝZNŽ•šY]ÜÜYŽ™œ›ÛWÚ\ÚÛÙŠ™]XÚY\YÚ[œÈŠNÂˆ]šY]ÜÜH]XÚYÝÚ[™Ý×ÝšY]ÜÜØZ[\Šˆ]XÚYÚ[™ÝÒÚ[™Ž”YÚ[œËˆ”YËZ[œÈ‹ˆ™XÌŽŽ›™]ÊLŒÌŒ
KˆÙ[‹™]XÚYÝÚ[™Ý×ÜXÙ[Y[Âˆ™Ù]
]XÚYÚ[™ÝÒÚ[™Ž”YÚ[œÊKˆ
NÂˆ]ÛÛ^HZK˜Ý

K˜ÛÛ™J
NÂˆÛÛ^œÚÝ×ÝšY]ÜÜÚ[[YYX]JšY]ÜÜÚYšY]ÜÜšY]ÜÜÝZKØÛ\ÜßÂˆÙ[‹œÚÝ×Ù]XÚYØ]^[X\žWÝšY]Êˆ]XÚYÚ[™ÝÒÚ[™Ž”YÚ[œËˆšY]ÜÜÝZKˆ	›]]Ú[\—ÙÝÛ‹ˆ	›]]\ÝÜžWÛ˜]šYØ][Û‹ˆ	›]]œ˜[YWÜÛ˜\ÚÝˆ
NÂˆJNÂˆBˆYˆÙ[‹˜]Y[×ÝÚ[™Ý×Ù]XÚYÂˆ]šY]ÜÜÚYHYÝZNŽ•šY]ÜÜYŽ™œ›ÛWÚ\ÚÛÙŠ™]XÚYX]Y[ÈŠNÂˆ]šY]ÜÜH]XÚYÝÚ[™Ý×ÝšY]ÜÜØZ[\Šˆ]XÚYÚ[™ÝÒÚ[™Ž]Y[Ëˆ]Y[È‹ˆ™XÌŽŽ›™]ÊLŒÌŒŒ
KˆÙ[‹™]XÚYÝÚ[™Ý×ÜXÙ[Y[Âˆ™Ù]
]XÚYÚ[™ÝÒÚ[™Ž]Y[ÊKˆ
NÂˆ]ÛÛ^HZK˜Ý

K˜ÛÛ™J
NÂˆÛÛ^œÚÝ×ÝšY]ÜÜÚ[[YYX]JšY]ÜÜÚYšY]ÜÜšY]ÜÜÝZKØÛ\ÜßÂˆÙ[‹œÚÝ×Ù]XÚYØ]^[X\žWÝšY]Êˆ]XÚYÚ[™ÝÒÚ[™Ž]Y[ËˆšY]ÜÜÝZKˆ	›]]Ú[\—ÙÝÛ‹ˆ	›]]\ÝÜžWÛ˜]šYØ][Û‹ˆ	›]]œ˜[YWÜÛ˜\ÚÝˆ
NÂˆJNÂˆBˆYˆÙ[‹˜]]ÛX][Û—ÝÚ[™Ý×Ù]XÚYÂˆ]šY]ÜÜÚYHYÝZNŽ•šY]ÜÜYŽ™œ›ÛWÚ\ÚÛÙŠ™]XÚYX]]ÛX][ÛˆŠNÂˆ]šY]ÜÜH]XÚYÝÚ[™Ý×ÝšY]ÜÜØZ[\Šˆ]XÚYÚ[™ÝÒÚ[™Ž]]ÛX][Û‹ˆ]]ÛX][Ûˆ‹ˆ™XÌŽŽ›™]ÊLŒŒÌŒ
KˆÙ[‹™]XÚYÝÚ[™Ý×ÜXÙ[Y[Âˆ™Ù]
]XÚYÚ[™ÝÒÚ[™Ž]]ÛX][ÛŠKˆ
NÂˆ]ÛÛ^HZK˜Ý

K˜ÛÛ™J
NÂˆÛÛ^œÚÝ×ÝšY]ÜÜÚ[[YYX]JšY]ÜÜÚYšY]ÜÜšY]ÜÜÝZKØÛ\ÜßÂˆÙ[‹œÚÝ×Ù]XÚYØ]^[X\žWÝšY]Êˆ]XÚYÚ[™ÝÒÚ[™Ž]]ÛX][Û‹ˆšY]ÜÜÝZKˆ	›]]Ú[\—ÙÝÛ‹ˆ	›]]\ÝÜžWÛ˜]šYØ][Û‹ˆ	›]]œ˜[YWÜÛ˜\ÚÝˆ
NÂˆJNÂˆBˆ]\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›HHÙ[‹š\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›WÝ\×Ùœ˜[YBˆœ˜[YWÜÛ˜\ÚÝš\×ÜÛÛYJ
BˆÙ[‹œ[™[™×Ú\ÝÜžWÜÛ˜\ÚÝš\×ÜÛÛYJ
NÂˆYˆÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÚ[œ]Ü™Yœ™\ÚÜ™\]Y\ÝY
HÂˆÙ[‹œ™Yœ™\ÚÛZYWÚ[œ]Ù]šXÙ\Ê
NÂˆBˆYˆÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÚ[œ]ÝÙÙÛWÜ™\]Y\ÝY
HÂˆÙ[‹ÙÙÛWÛZYWÚ[œ]ØÛÛ›™XÝ[ÛŠ
NÂˆBˆ]]Y[×Ü™XÛÜ™ÜÝÜÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹˜]Y[×Ü™XÛÜ™ÜÝÜÜ™\]Y\ÝY
NÂˆ]]Y[×Ü™XÛÜ™ÜÝ\Ü™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹˜]Y[×Ü™XÛÜ™ÜÝ\Ü™\]Y\ÝY
NÂˆYˆ]Y[×Ü™XÛÜ™ÜÝÜÜ™\]Y\ÝYÂˆÙ[‹œÝÜØ]Y[×Ü™XÛÜ™[™Ê
NÂˆH[ÙHYˆ]Y[×Ü™XÛÜ™ÜÝ\Ü™\]Y\ÝYÂˆÙ[‹œÝ\Ø]Y[×Ü™XÛÜ™[™Ê
NÂˆBˆYˆÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÜ™XÛÜ™ÝÙÙÛWÜ™\]Y\ÝY
HÂˆÙ[‹ÙÙÛWÛZYWÜ™XÛÜ™[™Ê\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›JNÂˆBˆ]ZYWÛÝ]]Ü™Yœ™\ÚÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÛÝ]]Ü™Yœ™\ÚÜ™\]Y\ÝY
NÂˆ]ZYWÛÝ]]ÝÙÙÛWÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÛÝ]]ÝÙÙÛWÜ™\]Y\ÝY
NÂˆ]ZYWÛÝ]]Ý\ÝÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÛÝ]]Ý\ÝÜ™\]Y\ÝY
NÂˆ]ZYWÜ]\›—Ü^WÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÜ]\›—Ü^WÜ™\]Y\ÝY
NÂˆ]ZYWÜ]\›—ÜÝÜÜ™\]Y\ÝYHÝŽ›Y[NŽZÙJ	›]]Ù[‹›ZYWÜ]\›—ÜÝÜÜ™\]Y\ÝY
NÂˆYˆZYWÛÝ]]Ü™Yœ™\ÚÜ™\]Y\ÝYÂˆÙ[‹œ™Yœ™\ÚÛZYWÛÝ]]Ù]šXÙ\Ê
NÂˆBˆYˆZYWÛÝ]]ÝÙÙÛWÜ™\]Y\ÝYÂˆÙ[‹ÙÙÛWÛZYWÛÝ]]ØÛÛ›™XÝ[ÛŠ
NÂˆBˆYˆZYWÛÝ]]Ý\ÝÜ™\]Y\ÝYÂˆÙ[‹\ÝÛZYWÛÝ]]

NÂˆZK˜Ý

Kœ™\]Y\ÝÜ™\Z[ØY\Š\˜][ÛŽŽ™œ›ÛWÛZ[\ÊÌÊJNÂˆBˆYˆZYWÜ]\›—ÜÝÜÜ™\]Y\ÝYÂˆÙ[‹œÝÜÛZYWÜ]\›—Ü™]šY]Ê
NÂˆBˆYˆZYWÜ]\›—Ü^WÜ™\]Y\ÝYÂˆÙ[‹œÝ\ÛZYWÜ]\›—Ü™]šY]Ê
NÂˆBˆYˆZYWÛÝ]]Ü™Yœ™\ÚÜ™\]Y\ÝYˆZYWÛÝ]]ÝÙÙÛWÜ™\]Y\ÝYˆZYWÛÝ]]Ý\ÝÜ™\]Y\ÝYˆZYWÜ]\›—Ü^WÜ™\]Y\ÝYˆZYWÜ]\›—ÜÝÜÜ™\]Y\ÝYˆÂˆZK˜Ý

Kœ™\]Y\ÝÜ™\Z[

NÂˆBˆÙ[‹œ›Ú™XÝÚ[™›×ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œ›Ú™XÝÜÙ][™Ü×ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜Ú[›™[ÙÜ›Ý\ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œ]\›—Ü™[˜[YWÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œ]\›—Ù[]WÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œ^[\ÝÜ™[™\—ÛÜ[Ûœ×ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜œ›ÝÜÙ\—ÝY×ÙY]Ü—ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜œ›ÝÜÙ\—Ø[×ÝY×ÙY]Ü—ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜œ›ÝÜÙ\—ÜÙX\˜ÚÜØ]™WÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜œ›ÝÜÙ\—ÝX—ØÝ\ÝÛZ^™WÙX[ÙÊZK˜Ý

JNÂˆÙ[‹˜œ›ÝÜÙ\—ÙœÝÙ]Z[×ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œ™XÛÝ™\žWÜ›Û\ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹[œØ]™YØÚ[™Ù\×ÙX[ÙÊZK˜Ý

JNÂˆÙ[‹œÛØ]Y[×Ü™XÛÜ™[™Ê\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›JNÂˆÙ[‹œÛØ]Y[×ÝØ]™Y›Ü›\ÊZK˜Ý

JNÂˆÙ[‹œ›ØÙ\Ü×Ü[™[™×Ü^[\ÝØ]Y[×Ù›ÜÊZK˜Ý

K\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›JNÂˆÙ[‹™š[š\ÚÚ\ÝÜžWÙœ˜[YJœ˜[YWÜÛ˜\ÚÝZÙJ
KÚ[\—ÙÝÛ‹\ÝÜžWÛ˜]šYØ][ÛŠNÂˆÙ[‹š\ÝÜžWÜÛ˜\ÚÝØ]˜Z[X›WÝ\×Ùœ˜[YHH˜[ÙNÂˆBŸB‚š[\›Ü›Üˆ]ÕZHÂˆ›ˆ›Ü
	›]]Ù[ŠHÂˆÙ[‹œÝÜÜ›Ú™XÝÜ^X˜XÚÊ
NÂˆYˆ]ÛÛYJÙ\ÜÚ[ÛŠHHÙ[‹˜]Y[×Ü™XÛÜ™[™ËZÙJ
HÂˆYˆÙ\ÜÚ[Û‹˜Ø\\™WØXÝ]™Bˆ	‰ˆ]ÛÛYJ[™Ú[™JHHÙ[‹˜]Y[×Ù[™Ú[™K˜\×Ü™YŠ
BˆÂˆ[™Ú[™KœÝÜÚ[œ]Ü™XÛÜ™[™Ê
NÂˆBˆ]ÈHÙ\ÜÚ[Û‹ÛÜšÙ\‹š›Ú[Š
NÂˆBˆYˆ]ÛÛYJÙ\ÜÚ[ÛŠHHÙ[‹œØ[\WÙY]Ü—Ü™XÛÜ™[™ËZÙJ
HÂˆYˆÙ\ÜÚ[Û‹˜Ø\\™WØXÝ]™Bˆ	‰ˆ]ÛÛYJ[™Ú[™JHHÙ[‹˜]Y[×Ù[™Ú[™K˜\×Ü™YŠ
BˆÂˆ[™Ú[™KœÝÜÚ[œ]Ü™XÛÜ™[™Ê
NÂˆBˆ]ÈHÙ\ÜÚ[Û‹ÛÜšÙ\‹š›Ú[Š
NÂˆBˆ›ÜˆÛÜšÙ\ˆ[ˆÙ[‹˜]Y[×Ü™[™\—ÝÛÜšÙ\œË™˜Z[Š‹ŠHÂˆ]ÈHÛÜšÙ\‹ÛÜšÙ\‹š›Ú[Š
NÂˆBˆ›Üˆ™[™\ˆ[ˆÙ[‹œÛÛ™×Ü™[™\—ÝÛÜšÙ\œË™˜Z[Š‹ŠHÂˆ]ÈH™[™\‹ÛÜšÙ\‹š›Ú[Š
NÂˆBˆYˆ]ÛÛYJÝ™X[JHHÙ[‹œ[™[™×ÝœÝ×ÜÝ™X[KZÙJ
HÂˆÙ[‹œÝ×ÝÛÜšÙ\œËœ\Ú
Ý™X[JNÂˆBˆ›ÜˆÛÜšÙ\ˆ[ˆÙ[‹œÝ×ÝÛÜšÙ\œË™˜Z[Š‹ŠHÂˆ]ÈHÛÜšÙ\‹š›Ú[Š
NÂˆBˆ›ÜˆÛÜšÙ\ˆ[ˆÙ[‹›]™WÝœÝ×ÝÛÜšÙ\œË™˜Z[Š‹ŠHÂˆ]ÈHÛÜšÙ\‹š›Ú[Š
NÂˆBˆYˆ]ÛÛYJ˜XÚÝ\
HHÙ[‹œ[™[™×Ø˜XÚÝ\ÝÜš]KZÙJ
HÂˆ]ÈH˜XÚÝ\ÛÜšÙ\‹š›Ú[Š
NÂˆBˆBŸB‚™›ˆ[\WÝšY]ÊZNˆ	›]]YÝZNŽ•ZKY\ÜØYÙNˆ	œÝŠHÂˆZK˜Ù[\™YØ[™Ú\ÝYšYY
Z_ÂˆZK›X™[
YÝZNŽ”šXÚ^Ž›™]ÊY\ÜØYÙJKœÚ^™JNŒ
K˜ÛÛÜŠUUQ
JNÂˆJNÂŸB‚™›ˆYÚ[—ÜÝ]WÝØ\›š[™×ÜÝY™š^
Ø\›š[™ÜÎˆ	–ÔÝš[™×JHOˆÝš[™ÈÂˆYˆØ\›š[™ÜËš\×Ù[\J
HÂˆÝš[™ÎŽ›™]Ê
BˆH[ÙHÂˆ›Ü›X]Jˆˆ0­È”ÕÈÝ]HÙ\œ›ÛH]È™]š[Ý\ÈØ]™H›ÜˆßH‹ˆØ\›š[™ÜËš›Ú[ŠŽÈŠBˆ
BˆBŸB‚™›ˆ^Y\—ØÚ[ÜÙ[XÝ[Û—ÛX™[
Ú[ÚYÎˆ	–ÝLM—KÚ[›™[Îˆ	–ÐÚ[›™[Ý[[X\žWJHOˆÝš[™ÈÂˆYˆÚ[ÚYËš\×Ù[\J
HÂˆ™]\›ˆ“›ÈÚ[Ú[›™[È‹×ÛÝÛ™Y

NÂˆBˆÚ[ÚYÂˆš]\Š
Bˆ›X\
Ú[ÚYÂˆÚ[›™[Âˆš]\Š
Bˆ™š[™
Ú[›™[Ú[›™[šY

HOH
˜Ú[ÚY
Bˆ›X\
Ú[›™[Âˆ›Ü›X]JˆžßH
QØÚ[ÚYJH‹ˆÚ[›™[™\Ü^WÛ˜[YJ
K[Ü˜\ÛÜŠŠ[›˜[YYÚ[›™[
HŠBˆ
BˆJBˆ[Ü˜\ÛÜ—Ù[ÙJ›Ü›X]J“Z\ÜÚ[™ÈÚ[›™[
QØÚ[ÚYJHŠJBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š‹ŠBŸB‚™›ˆ\]WÛ^Y\—ØÚ[ÜÙ[XÝ[ÛŠÚ[ÚYÎˆ	›]]™XÏLM‹Ú[ÚYˆLM‹Ù[XÝYˆ›ÛÛ
HÂˆYˆÙ[XÝYÂˆYˆXÚ[ÚYË˜ÛÛZ[œÊ	˜Ú[ÚY
HÂˆÚ[ÚYËœ\Ú
Ú[ÚY
NÂˆBˆH[ÙHÂˆÚ[ÚYËœ™]Z[ŠY
šYOHÚ[ÚY
NÂˆBŸB‚™›ˆ\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[ÛŠˆÚ[›™[ÚYÎˆ	–ÝLM—KˆÙ[XÝYˆ	›]]•™YTÙ]LM‹ˆ[˜ÚÜŽˆ	›]]Ü[ÛLM‹ˆÚ[›™[ÚYˆLM‹ˆY]]™Nˆ›ÛÛˆ^[™Ü˜[™ÙNˆ›ÛÛŠHÂˆ]ÛÛYJÚ[›™[Ú[™^
HHÚ[›™[ÚYËš]\Š
KœÜÚ][ÛŠY
šYOHÚ[›™[ÚY
H[ÙHÂˆ™]\›ŽÂˆNÂˆYˆ^[™Ü˜[™ÙHÂˆ][˜ÚÜ—Ú[™^H[˜ÚÜ‚ˆ˜[™Ý[Š[˜ÚÜ—ÚYÚ[›™[ÚYËš]\Š
KœÜÚ][ÛŠY
šYOH[˜ÚÜ—ÚY
JBˆ[Ü˜\ÛÜŠÚ[›™[Ú[™^
NÂˆYˆXY]]™HÂˆÙ[XÝY˜ÛX\Š
NÂˆBˆÙ[XÝY™^[™
ˆÚ[›™[ÚYÂˆš]\Š
BˆZÙJ[˜ÚÜ—Ú[™^›X^
Ú[›™[Ú[™^
H
ÈJBˆœÚÚ\
[˜ÚÜ—Ú[™^›Z[ŠÚ[›™[Ú[™^
JBˆ˜ÛÜYY

Kˆ
NÂˆYˆ[˜ÚÜ‹š\×Û›Û™J
HÂˆ
˜[˜ÚÜˆHÛÛYJÚ[›™[ÚY
NÂˆBˆH[ÙHYˆY]]™HÂˆYˆÙ[XÝY˜ÛÛZ[œÊ	˜Ú[›™[ÚY
HÂˆYˆÙ[XÝY›[Š
HˆHÂˆÙ[XÝYœ™[[Ý™J	˜Ú[›™[ÚY
NÂˆBˆH[ÙHÂˆÙ[XÝYš[œÙ\
Ú[›™[ÚY
NÂˆBˆ
˜[˜ÚÜˆHÛÛYJÚ[›™[ÚY
NÂˆH[ÙHÂˆÙ[XÝY˜ÛX\Š
NÂˆÙ[XÝYš[œÙ\
Ú[›™[ÚY
NÂˆ
˜[˜ÚÜˆHÛÛYJÚ[›™[ÚY
NÂˆBŸB‚™›ˆÚ[›™[Ý\WÚXÛÛ—Ù\ØÜš\[ÛŠÚ[›™[Ý\NˆÜ[ÛÚ[›™[\OŠHOˆÝš[™ÈÂˆX]ÚÚ[›™[Ý\HÂˆÛÛYJÚ[›™[\NŽ”Ø[\\ŠHOˆ”Ø[\\ˆÚ[›™[‹×ÛÝÛ™Y

KˆÛÛYJÚ[›™[\NŽ“˜]]™JHOˆ‘Ù[™\˜]ÜˆÚ[›™[‹×ÛÝÛ™Y

KˆÛÛYJÚ[›™[\NŽ“^Y\ŠHOˆ“^Y\ˆÚ[›™[‹×ÛÝÛ™Y

KˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HOˆ’[œÝ[Y[Ú[›™[‹×ÛÝÛ™Y

KˆÛÛYJÚ[›™[\NŽ]]ÛX][ÛŠHOˆ]]ÛX][ÛˆÚ[›™[‹×ÛÝÛ™Y

KˆÛÛYJÚ[›™[\NŽ•[šÛ›ÝÛŠ˜]ÊJHOˆ›Ü›X]J•[šÛ›ÝÛˆÚ[›™[\HÜ˜]ßHŠKˆ›Û™HOˆ•[˜Û\ÜÚYšYYÚ[›™[‹×ÛÝÛ™Y

KˆBŸB‚™›ˆ˜]×ÜØ[\\—Ù[™[ÜWÛ›×ÙY]ÜŠˆZNˆ	›]]YÝZNŽ•ZKˆÚ[›™[ÚYˆLM‹ˆ™XÛÜ™ˆØ[\\‘[™[ÜS›ËŠHOˆÜ[ÛØ[\\‘[™[ÜS›ÑY]ˆÂˆ]]][˜X›YH™XÛÜ™™[˜X›Y

NÂˆ]]][™[ÜWÝ[\×ÜÞ[˜ÈH™XÛÜ™[\×ÜÞ[˜×Ù[˜X›Y

NÂˆ]]]›×Ý[\×ÜÞ[˜ÈH™XÛÜ™››×Ý[\×ÜÞ[˜×Ù[˜X›Y

NÂˆ]]]›×Ü\ÙWÜ™]šYÙÙ\ˆH™XÛÜ™››×Ü\ÙWÜ™]šYÙÙ\—Ù[˜X›Y

NÂˆ][˜X›YØÚ[™ÙYHZK˜ÚXÚØ›Þ
	›]][˜X›Y‘[˜X›YŠK˜Ú[™ÙY

NÂˆZKšÜš^›Û[ÝÜ˜\Y
Z_ÂˆZK˜ÚXÚØ›Þ
	›]][™[ÜWÝ[\×ÜÞ[˜Ë‘[™[ÜH[\ÈÞ[˜ÈŠNÂˆZK˜ÚXÚØ›Þ
	›]]›×Ý[\×ÜÞ[˜Ë““È[\ÈÞ[˜ÈŠNÂˆZK˜ÚXÚØ›Þ
	›]]›×Ü\ÙWÜ™]šYÙÙ\‹““È\ÙH™]šYÙÙ\ˆŠNÂˆJNÂ‚ˆXXÜ›×Ü[\ÈHY]Ü˜]ÈÂˆ
	ZNšY[	X™[›]\˜[	Ø]™Y™^‹	˜[™ÙN™^ŠHOˆÞÂˆYˆ]ÛÛYJ]]˜[YJHH	Ø]™YÂˆ]Ú[™ÙYH	ZBˆ˜Y
ˆYÝZNŽ‘˜YÕ˜[YNŽ›™]Ê	›]]˜[YJBˆœ˜[™ÙJ	˜[™ÙJBˆœ™Yš^
ÛÛ˜Ø]J	X™[ˆŠJKˆ
Bˆ˜Ú[™ÙY

NÂˆÚ[™ÙY[—ÜÛÛYJ˜[YJBˆH[ÙHÂˆ	ZK›X™[
ÛÛ˜Ø]J	X™[ˆ[˜]˜Z[X›HŠJNÂˆ›Û™BˆBˆ_NÂˆB‚ˆ]
ˆ™Y[^WÜ˜]Ëˆ]XÚ×Ü˜]ËˆÛÜ˜]ËˆXØ^WÜ˜]ËˆÝ\ÝZ[—Ü˜]Ëˆ™[X\ÙWÜ˜]Ëˆ[[Ý[Ü˜]Ëˆ]XÚ×Ý[œÚ[Û—Ü˜]ËˆXØ^WÝ[œÚ[Û—Ü˜]Ëˆ™[X\ÙWÝ[œÚ[Û—Ü˜]Ëˆ
HHYÝZNŽ‘ÜšYŽ›™]Ê
œØ[\\‹Y[™[ÜK\˜]È‹Ú[›™[ÚY™XÛÜ™\™Ù]

JJBˆ›[WØÛÛ[[œÊŠBˆœÚÝÊZKZ_ÂˆZK›X™[
‘[™[ÜH˜]È˜[Y\ÈŠNÂˆZK™[™Ü›ÝÊ
NÂˆ]™Y[^HHY]Ü˜]ÈJZK”™Y[^H‹™XÛÜ™œ™Y[^WÜ˜]Ê
KL‹MWÍLÍŠNÂˆ]]XÚÈHY]Ü˜]ÈJZK]XÚÈ‹™XÛÜ™˜]XÚ×Ü˜]Ê
KL‹MWÍLÍŠNÂˆ]ÛHY]Ü˜]ÈJZK’Û‹™XÛÜ™šÛÜ˜]Ê
KL‹MWÍLÍŠNÂˆ]XØ^HHY]Ü˜]ÈJZK‘XØ^H‹™XÛÜ™™XØ^WÜ˜]Ê
KL‹MWÍLÍŠNÂˆ]Ý\ÝZ[ˆHY]Ü˜]ÈJZK”Ý\ÝZ[ˆ‹™XÛÜ™œÝ\ÝZ[—Ü˜]Ê
K‹LLŽ
NÂˆ]™[X\ÙHHY]Ü˜]ÈJZK”™[X\ÙH‹™XÛÜ™œ™[X\ÙWÜ˜]Ê
KL‹MWÍLÍŠNÂˆ][[Ý[HY]Ü˜]ÈJZK[[Ý[‹™XÛÜ™˜[[Ý[Ü˜]Ê
KLLŽ‹LLŽ
NÂˆ]]XÚ×Ý[œÚ[ÛˆHY]Ü˜]ÈJˆZKˆ]XÚÈ[œÚ[Ûˆ‹ˆ™XÛÜ™˜]XÚ×Ý[œÚ[Û—Ü˜]Ê
KˆLLŽ‹LLŽˆ
NÂˆ]XØ^WÝ[œÚ[ÛˆBˆY]Ü˜]ÈJZK‘XØ^H[œÚ[Ûˆ‹™XÛÜ™™XØ^WÝ[œÚ[Û—Ü˜]Ê
KLLŽ‹LLŽ
NÂˆ]™[X\ÙWÝ[œÚ[ÛˆHY]Ü˜]ÈJˆZKˆ”™[X\ÙH[œÚ[Ûˆ‹ˆ™XÛÜ™œ™[X\ÙWÝ[œÚ[Û—Ü˜]Ê
KˆLLŽ‹LLŽˆ
NÂˆZK™[™Ü›ÝÊ
NÂˆ
ˆ™Y[^Kˆ]XÚËˆÛˆXØ^KˆÝ\ÝZ[‹ˆ™[X\ÙKˆ[[Ý[ˆ]XÚ×Ý[œÚ[Û‹ˆXØ^WÝ[œÚ[Û‹ˆ™[X\ÙWÝ[œÚ[Û‹ˆ
BˆJBˆš[›™\ŽÂˆ]
›×Ü™Y[^WÜ˜]Ë›×Ø]XÚ×Ü˜]Ë›×Ø[[Ý[Ü˜]Ë›×ÜÜYYÜ˜]Ë›×ÜÚ\WÜ˜]ÊHBˆYÝZNŽ‘ÜšYŽ›™]Ê
œØ[\\‹[›Ë\˜]È‹Ú[›™[ÚY™XÛÜ™\™Ù]

JJBˆ›[WØÛÛ[[œÊŠBˆœÚÝÊZKZ_ÂˆZK›X™[
““È˜]È˜[Y\ÈŠNÂˆZK™[™Ü›ÝÊ
NÂˆ]™Y[^HHY]Ü˜]ÈJZK”™Y[^H‹™XÛÜ™››×Ü™Y[^WÜ˜]Ê
KL‹MWÍLÍŠNÂˆ]]XÚÈHY]Ü˜]ÈJZK]XÚÈ‹™XÛÜ™››×Ø]XÚ×Ü˜]Ê
KL‹MWÍLÍŠNÂˆ][[Ý[HY]Ü˜]ÈJZK[[Ý[‹™XÛÜ™››×Ø[[Ý[Ü˜]Ê
KLLŽ‹LLŽ
NÂˆ]ÜYYHY]Ü˜]ÈJZK”ÜYY‹™XÛÜ™››×ÜÜYYÜ˜]Ê
KŒ‹MWÍLÍŠNÂˆ]Ú\HHY]Ü˜]ÈJˆZKˆ”Ú\H
Ú[™KHšX[™ÛKˆ[ÙJH‹ˆ™XÛÜ™››×ÜÚ\WÜ˜]Ê
Kˆ‹L‚ˆ
NÂˆZK™[™Ü›ÝÊ
NÂˆ
™Y[^K]XÚË[[Ý[ÜYYÚ\JBˆJBˆš[›™\ŽÂ‚ˆ][\×ÜÞ[˜×ØÚ[™ÙYH[™[ÜWÝ[\×ÜÞ[˜ÈOH™XÛÜ™[\×ÜÞ[˜×Ù[˜X›Y

NÂˆ]›×Ý[\×ÜÞ[˜×ØÚ[™ÙYH›×Ý[\×ÜÞ[˜ÈOH™XÛÜ™››×Ý[\×ÜÞ[˜×Ù[˜X›Y

NÂˆ]›×Ü\ÙWÜ™]šYÙÙ\—ØÚ[™ÙYH›×Ü\ÙWÜ™]šYÙÙ\ˆOH™XÛÜ™››×Ü\ÙWÜ™]šYÙÙ\—Ù[˜X›Y

NÂˆ]Y]HØ[\\‘[™[ÜS›ÑY]Âˆ[˜X›Yˆ[˜X›YØÚ[™ÙY[—ÜÛÛYJ[˜X›Y
Kˆ[\×ÜÞ[˜Îˆ[\×ÜÞ[˜×ØÚ[™ÙY[—ÜÛÛYJ[™[ÜWÝ[\×ÜÞ[˜ÊKˆ›×Ý[\×ÜÞ[˜Îˆ›×Ý[\×ÜÞ[˜×ØÚ[™ÙY[—ÜÛÛYJ›×Ý[\×ÜÞ[˜ÊKˆ›×Ü\ÙWÜ™]šYÙÙ\Žˆ›×Ü\ÙWÜ™]šYÙÙ\—ØÚ[™ÙY[—ÜÛÛYJ›×Ü\ÙWÜ™]šYÙÙ\ŠKˆ™Y[^WÜ˜]Ëˆ]XÚ×Ü˜]ËˆÛÜ˜]ËˆXØ^WÜ˜]ËˆÝ\ÝZ[—Ü˜]Ëˆ™[X\ÙWÜ˜]Ëˆ[[Ý[Ü˜]Ëˆ]XÚ×Ý[œÚ[Û—Ü˜]ËˆXØ^WÝ[œÚ[Û—Ü˜]Ëˆ™[X\ÙWÝ[œÚ[Û—Ü˜]Ëˆ›×Ü™Y[^WÜ˜]Ëˆ›×Ø]XÚ×Ü˜]Ëˆ›×Ø[[Ý[Ü˜]Ëˆ›×ÜÜYYÜ˜]Ëˆ›×ÜÚ\WÜ˜]ËˆNÂˆ
Y]™[˜X›Yš\×ÜÛÛYJ
BˆY][\×ÜÞ[˜Ëš\×ÜÛÛYJ
BˆY]››×Ý[\×ÜÞ[˜Ëš\×ÜÛÛYJ
BˆY]››×Ü\ÙWÜ™]šYÙÙ\‹š\×ÜÛÛYJ
BˆY]œ™Y[^WÜ˜]Ëš\×ÜÛÛYJ
BˆY]˜]XÚ×Ü˜]Ëš\×ÜÛÛYJ
BˆY]šÛÜ˜]Ëš\×ÜÛÛYJ
BˆY]™XØ^WÜ˜]Ëš\×ÜÛÛYJ
BˆY]œÝ\ÝZ[—Ü˜]Ëš\×ÜÛÛYJ
BˆY]œ™[X\ÙWÜ˜]Ëš\×ÜÛÛYJ
BˆY]˜[[Ý[Ü˜]Ëš\×ÜÛÛYJ
BˆY]˜]XÚ×Ý[œÚ[Û—Ü˜]Ëš\×ÜÛÛYJ
BˆY]™XØ^WÝ[œÚ[Û—Ü˜]Ëš\×ÜÛÛYJ
BˆY]œ™[X\ÙWÝ[œÚ[Û—Ü˜]Ëš\×ÜÛÛYJ
BˆY]››×Ü™Y[^WÜ˜]Ëš\×ÜÛÛYJ
BˆY]››×Ø]XÚ×Ü˜]Ëš\×ÜÛÛYJ
BˆY]››×Ø[[Ý[Ü˜]Ëš\×ÜÛÛYJ
BˆY]››×ÜÜYYÜ˜]Ëš\×ÜÛÛYJ
BˆY]››×ÜÚ\WÜ˜]Ëš\×ÜÛÛYJ
JBˆ[—ÜÛÛYJY]
BŸB‚™›ˆÚ[›™[ÜÝ™]ÚÛ[ÙWÛX™[
˜]ÎˆLÌŠHOˆÝš[™ÈÂˆ]˜[YHHX]ÚÚ[›™[Ý™]Ú[ÙNŽ™œ›ÛWÜ˜]Ê˜]ÊHÂˆÚ[›™[Ý™]Ú[ÙNŽ”Ý™]ÚOˆ”Ý™]Ú‹ˆÚ[›™[Ý™]Ú[ÙNŽ”™\Ø[\HOˆ”™\Ø[\H‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YLÑÙ[™\šXÈOˆ‘[\Ý\]YHÈÙ[™\šXÈ‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YLÓ[Û›ÈOˆ‘[\Ý\]YHÈ[Û›È‹ˆÚ[›™[Ý™]Ú[ÙNŽ”ÛXÙTÝ™]ÚOˆ”ÛXÙHÝ™]Ú‹ˆÚ[›™[Ý™]Ú[ÙNŽ”ÛXÙSX\Oˆ”ÛXÙHX\‹ˆÚ[›™[Ý™]Ú[ÙNŽ]]ÈOˆ]]È‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YL‘Ù[™\šXÈOˆ‘[\Ý\]YHˆÙ[™\šXÈ‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YL•˜[œÚY[Oˆ‘[\Ý\]YHˆ˜[œÚY[‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YL“[Û›ÈOˆ‘[\Ý\]YHˆ[Û›È‹ˆÚ[›™[Ý™]Ú[ÙNŽ‘[\Ý\]YL”ÜYXÚOˆ‘[\Ý\]YHˆÜYXÚ‹ˆÚ[›™[Ý™]Ú[ÙNŽ•[šÛ›ÝÛŠÊHOˆ™]\›ˆ›Ü›X]J•[šÛ›ÝÛˆ
Ü˜]ßJHŠKˆNÂˆ›Ü›X]JžÛ˜[Y_H
Ü˜]ßJHŠBŸB‚™›ˆZ[ØÚ[›™[Ý\WÚXÛÛŠˆZ[\Žˆ	™YÝZNŽ”Z[\‹ˆ™XÝˆYÝZNŽ”™XÝˆÚ[›™[Ý\NˆÜ[ÛÚ[›™[\O‹ˆÛÛÜŽˆÛÛÜŒÌ‹ŠHÂˆ]XÛÛ—Ü™XÝH™XÝœÚš[šÊ‹Œ
NÂˆZ[\‹œ™XÝÙš[Y
XÛÛ—Ü™XÝËŒS‘SÑT’ÊNÂˆZ[\‹œ™XÝÜÝ›ÚÙJˆXÛÛ—Ü™XÝˆËŒˆÝ›ÚÙNŽ›™]ÊKŒÛÛÜŠKˆYÝZNŽ”Ý›ÚÙRÚ[™Ž’[œÚYKˆ
NÂˆ]Ù[\ˆHXÛÛ—Ü™XÝ˜Ù[\Š
NÂˆX]ÚÚ[›™[Ý\HÂˆÛÛYJÚ[›™[\NŽ”Ø[\\ŠHOˆÂˆ›Üˆ
Ù™œÙ][—ÚZYÚ
H[ˆÊMKËŒ
K
LKK‹Œ
K
KKŒ
K
K‹Œ
WHÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠÙ[\‹ž
ÈÙ™œÙ]Ù[\‹žHH[—ÚZYÚ
KˆYÝZNŽœÜÌŠÙ[\‹ž
ÈÙ™œÙ]Ù[\‹žH
È[—ÚZYÚ
KˆKˆÝ›ÚÙNŽ›™]ÊKÛÛÜŠKˆ
NÂˆBˆBˆÛÛYJÚ[›™[\NŽ“˜]]™JHOˆÂˆ]Ú[ÈHÂˆYÝZNŽœÜÌŠÙ[\‹žH‹ŒÙ[\‹žJKˆYÝZNŽœÜÌŠÙ[\‹žHËŒÙ[\‹žHHËŒ
KˆYÝZNŽœÜÌŠÙ[\‹žÙ[\‹žH
ÈËŒ
KˆYÝZNŽœÜÌŠÙ[\‹ž
ÈËŒÙ[\‹žHHËŒ
KˆYÝZNŽœÜÌŠÙ[\‹ž
È‹ŒÙ[\‹žJKˆNÂˆ›ÜˆZ\ˆ[ˆÚ[ËÚ[™ÝÜÊŠHÂˆZ[\‹›[™WÜÙYÛY[
ÜZ\–ÌKZ\–ÌWWKÝ›ÚÙNŽ›™]ÊKÛÛÜŠJNÂˆBˆBˆÛÛYJÚ[›™[\NŽ“^Y\ŠHOˆÂˆ›Üˆ
Ù™œÙ]ÞÙ™œÙ]ÞJH[ˆÊL‹ŒL‹Œ
K
‹Œ‹Œ
WHÂˆZ[\‹œ™XÝÜÝ›ÚÙJˆYÝZNŽ”™XÝŽ™œ›ÛWØÙ[\—ÜÚ^™JˆÙ[\ˆ
È™XÌŽŽ›™]ÊÙ™œÙ]ÞÙ™œÙ]ÞJKˆ™XÌŽŽ›™]ÊKŒËŒ
Kˆ
KˆKŒˆÝ›ÚÙNŽ›™]ÊKŒ‹ÛÛÜŠKˆYÝZNŽ”Ý›ÚÙRÚ[™Ž’[œÚYKˆ
NÂˆBˆBˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HOˆÂˆ›Üˆ
Ù™œÙ][—ÚZYÚ
H[ˆÊMK‹Œ
K
LKKŒ
K
KKËŒ
K
K‹Œ
WHÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠÙ[\‹ž
ÈÙ™œÙ]Ù[\‹žHH[—ÚZYÚ
KˆYÝZNŽœÜÌŠÙ[\‹ž
ÈÙ™œÙ]Ù[\‹žH
È[—ÚZYÚ
KˆKˆÝ›ÚÙNŽ›™]ÊKÛÛÜŠKˆ
NÂˆBˆBˆÛÛYJÚ[›™[\NŽ]]ÛX][ÛŠHOˆÂˆ]Ú[ÈHÂˆYÝZNŽœÜÌŠÙ[\‹žHKŒÙ[\‹žH
ÈŒ
KˆYÝZNŽœÜÌŠÙ[\‹žHKŒÙ[\‹žH
ÈKŒ
KˆYÝZNŽœÜÌŠÙ[\‹ž
È‹ŒÙ[\‹žHHŒ
KˆYÝZNŽœÜÌŠÙ[\‹ž
ÈKŒÙ[\‹žHH‹Œ
KˆNÂˆ›ÜˆZ\ˆ[ˆÚ[ËÚ[™ÝÜÊŠHÂˆZ[\‹›[™WÜÙYÛY[
ÜZ\–ÌKZ\–ÌWWKÝ›ÚÙNŽ›™]ÊKÛÛÜŠJNÂˆBˆ›ÜˆÚ[[ˆÜÚ[ÖÌKÚ[ÖÌ—KÚ[ÖÌ×WHÂˆZ[\‹˜Ú\˜ÛWÙš[Y
Ú[KKÛÛÜŠNÂˆBˆBˆÛÛYJÚ[›™[\NŽ•[šÛ›ÝÛŠ˜]ÊJHOˆÂˆZ[\‹^
ˆÙ[\‹ˆ[YÛŒŽŽÑS•T—ÐÑS•T‹ˆ›Ü›X]JžÜ˜]Î–HŠKˆ›ÛYŽœ›ÜÜ[Û˜[
KŒ
KˆÛÛÜ‹ˆ
NÂˆBˆ›Û™HOˆÂˆZ[\‹^
ˆÙ[\‹ˆ[YÛŒŽŽÑS•T—ÐÑS•T‹ˆÈ‹ˆ›ÛYŽœ›ÜÜ[Û˜[
LKŒ
KˆÛÛÜ‹ˆ
NÂˆBˆBŸB‚ˆÖÙ\š]™JÛÛ™KÛÜKXYË\K\X[\KÜ™\X[Ü™
WBœÝXÝX[›Ô›ÛÚÜÝ›ÝHÂˆÛÝ\˜ÙWØÛ\ˆX[›Ô›ÛÚÜÝÛ\Ù^KˆÚ[›™[ÚYˆLM‹ˆÙ^NˆLM‹ˆÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆ[™ÝÝXÚÜÎˆLÌ‹ŸB‚™›ˆX[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›Jˆ›ÝNˆ	”X[›Ô›ÛÚÜÝ›ÝKˆY[—Ý˜XÚ×ÚÙ^\Îˆ	•™YTÙ]
LM‹LÌŠO‹ˆY[—ØÛ\ÚÙ^\Îˆ	•™YTÙ]X[›Ô›ÛÚÜÝÛ\Ù^O‹ŠHOˆ›ÛÛÂˆZY[—ØÛ\ÚÙ^\Ë˜ÛÛZ[œÊ	››ÝKœÛÝ\˜ÙWØÛ\
Bˆ	‰ˆ›ÝKœÛÝ\˜ÙWØÛ\˜XÚ×ÚYš\×Û›Û™WÛÜŠ˜XÚ×ÚYÂˆZY[—Ý˜XÚ×ÚÙ^\Ë˜ÛÛZ[œÊ	Š›ÝKœÛÝ\˜ÙWØÛ\˜\œ˜[™Ù[Y[ÚY˜XÚ×ÚY
JBˆJBŸB‚™›ˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊˆÝ\œ™[Ü]\›—ÚYˆLM‹ˆ]\›œÎˆ	–Ô]\›—Kˆ\œ˜[™Ù[Y[ˆ	\œ˜[™Ù[Y[ˆ\ØX›YÝ˜XÚ×ÚYÎˆ	•™YTÙ]LÌ‹ŠHOˆ™XÏX[›Ô›ÛÚÜÝ›ÝOˆÂˆÛÛœÝPVÑÒÔÕÓ“ÕTÎˆ\Ú^™HHŒÌÂ‚ˆ]]\›œ×ØžWÚYH]\›œÂˆš]\Š
Bˆ›X\
]\›Ÿ
]\›‹šY]\›ŠJBˆ˜ÛÛXÝŽ•™YSX\ËÏŠ
NÂˆ]Û\Ý˜XÚ×Ú\×Ù[˜X›YHÛ\ˆ	”^[\ÝÛ\ÂˆÛ\œ^[\ÝÝ˜XÚ×ÚY

Bˆš\×Û›Û™WÛÜŠ˜XÚ×ÚYY\ØX›YÝ˜XÚ×ÚYË˜ÛÛZ[œÊ	˜XÚ×ÚY
JBˆNÂˆ]Û\Ú\×ÙY˜][ÜØØ[HHÛ\ˆ	”^[\ÝÛ\ÂˆÛ\œØØ[Bˆš\×Û›Û™WÛÜŠØØ[_ØØ[Kš\×Ùš[š]J
H	‰ˆ
ØØ[HHKŒ
K˜XœÊ
HHYKNJBˆNÂˆ]]]ÚÜÝÈH•™YTÙ]Ž›™]Ê
NÂ‚ˆ	Ø[˜ÚÜœÎˆ›Üˆ[˜ÚÜˆ[ˆ	˜\œ˜[™Ù[Y[˜Û\ÈÂˆYˆ[˜ÚÜ‹š\×Û]]Y

BˆXÛ\Ý˜XÚ×Ú\×Ù[˜X›Y
[˜ÚÜŠBˆXÛ\Ú\×ÙY˜][ÜØØ[J[˜ÚÜŠBˆ[˜ÚÜ‹›[™ÝÝXÚÜÈOHˆ[X]Ú\ÈJˆ[˜ÚÜ‹\™Ù]

Kˆ^[\ÝÛ\\™Ù]Ž”]\›ˆÈYHYˆYOHÝ\œ™[Ü]\›—ÚYˆ
BˆÂˆÛÛ[YNÂˆBˆ][˜ÚÜ—ÜÝ\HMŽ™œ›ÛJ[˜ÚÜ‹œÜÚ][Û—ÝXÚÜÊNÂˆ][˜ÚÜ—Ù[™H[˜ÚÜ—ÜÝ\œØ]\˜][™×ØY
MŽ™œ›ÛJ[˜ÚÜ‹›[™ÝÝXÚÜÊJNÂ‚ˆ›Üˆ
ÛÝ\˜ÙWØÛ\Ú[™^ÛÝ\˜ÙWØÛ\
H[ˆ\œ˜[™Ù[Y[˜Û\Ëš]\Š
K™[[Y\˜]J
HÂˆYˆÛÝ\˜ÙWØÛ\š\×Û]]Y

BˆXÛ\Ý˜XÚ×Ú\×Ù[˜X›Y
ÛÝ\˜ÙWØÛ\
BˆXÛ\Ú\×ÙY˜][ÜØØ[JÛÝ\˜ÙWØÛ\
BˆÛÝ\˜ÙWØÛ\›[™ÝÝXÚÜÈOHˆÂˆÛÛ[YNÂˆBˆ]^[\ÝÛ\\™Ù]Ž”]\›ˆÂˆYˆÛÝ\˜ÙWÜ]\›—ÚYˆHHÛÝ\˜ÙWØÛ\\™Ù]

Bˆ[ÙHÂˆÛÛ[YNÂˆNÂˆYˆÛÝ\˜ÙWÜ]\›—ÚYOHÝ\œ™[Ü]\›—ÚYÂˆÛÛ[YNÂˆBˆ]ÛÝ\˜ÙWØÛ\ÚÙ^HHX[›Ô›ÛÚÜÝÛ\Ù^HÂˆ\œ˜[™Ù[Y[ÚYˆ\œ˜[™Ù[Y[šYˆÛ\Ú[™^ˆÛÝ\˜ÙWØÛ\Ú[™^ˆÛ\ÚYˆÛÝ\˜ÙWØÛ\˜Û\ÚYˆ]\›—ÚYˆÛÝ\˜ÙWÜ]\›—ÚYˆÜÚ][Û—ÝXÚÜÎˆÛÝ\˜ÙWØÛ\œÜÚ][Û—ÝXÚÜËˆ[™ÝÝXÚÜÎˆÛÝ\˜ÙWØÛ\›[™ÝÝXÚÜËˆ˜XÚ×ÚYˆÛÝ\˜ÙWØÛ\œ^[\ÝÝ˜XÚ×ÚY

KˆNÂˆ]ÛÝ\˜ÙWÜÝ\HMŽ™œ›ÛJÛÝ\˜ÙWØÛ\œÜÚ][Û—ÝXÚÜÊNÂˆ]ÛÝ\˜ÙWÛ[™ÝHMŽ™œ›ÛJÛÝ\˜ÙWØÛ\›[™ÝÝXÚÜÊNÂˆ]ÛÝ\˜ÙWÙ[™HÛÝ\˜ÙWÜÝ\œØ]\˜][™×ØY
ÛÝ\˜ÙWÛ[™Ý
NÂˆYˆÛÝ\˜ÙWÜÝ\H[˜ÚÜ—Ù[™[˜ÚÜ—ÜÝ\HÛÝ\˜ÙWÙ[™ÂˆÛÛ[YNÂˆBˆ]ÛÛYJÛÝ\˜ÙWÜ]\›ŠHH]\›œ×ØžWÚY™Ù]
	œÛÝ\˜ÙWÜ]\›—ÚY
K˜ÛÜYY

H[ÙHÂˆÛÛ[YNÂˆNÂˆ][™™\œ™YÛ[™ÝHÛÝ\˜ÙWÜ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK›[™Ýˆ
Bˆ›X\
›Ý_MŽ™œ›ÛJ›ÝKœÜÚ][ÛŠKœØ]\˜][™×ØY
MŽ™œ›ÛJ›ÝK›[™Ý
JJBˆ›X^

Bˆ[Ü˜\ÛÜŠ
NÂˆ]ÛÜÛ[™ÝHÛÝ\˜ÙWÜ]\›‚ˆ›[™ÝÝXÚÜÂˆ›X\
MŽ™œ›ÛJBˆ™š[\Š[™Ý
›[™Ýˆ
Bˆ[Ü˜\ÛÜŠYˆ[™™\œ™YÛ[™ÝˆÂˆ[™™\œ™YÛ[™ÝˆH[ÙHÂˆÛÝ\˜ÙWÛ[™ÝˆJNÂˆYˆÛÜÛ[™ÝOHÂˆÛÛ[YNÂˆB‚ˆ›Üˆ›ÝH[ˆÛÝ\˜ÙWÜ]\›‹››Ý\Ëš]\Š
K™š[\Š›Ý_[›ÝKš\×Û]]Y

JHÂˆ]›ÝWÜÜÚ][ÛˆHMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠNÂˆ]›ÝWÙ\˜][ÛˆHMŽ™œ›ÛJ›ÝK›[™Ý›X^
JJNÂˆ]š\œÝÛ›ÝWÜÝ\HÛÝ\˜ÙWÜÝ\œØ]\˜][™×ØY
›ÝWÜÜÚ][ÛŠNÂˆ]š\œÝÜ™\]][ÛˆBˆYˆ[˜ÚÜ—ÜÝ\Hš\œÝÛ›ÝWÜÝ\œØ]\˜][™×ØY
›ÝWÙ\˜][ÛŠHÂˆ
[˜ÚÜ—ÜÝ\Hš\œÝÛ›ÝWÜÝ\H›ÝWÙ\˜][ÛŠHÈÛÜÛ[™Ý
ÈBˆH[ÙHÂˆˆNÂˆ]]]™\]][ÛˆHš\œÝÜ™\]][ÛŽÂˆÛÜÂˆ]™[]]™WÜÝ\H™\]][Û‚ˆ˜ÚXÚÙYÛ][
ÛÜÛ[™Ý
Bˆ˜[™Ý[ŠÙ™œÙ]Ù™œÙ]˜ÚXÚÙYØY
›ÝWÜÜÚ][ÛŠJNÂˆ]ÛÛYJ™[]]™WÜÝ\
HH™[]]™WÜÝ\[ÙHÂˆœ™XZÎÂˆNÂˆYˆ™[]]™WÜÝ\HÛÝ\˜ÙWÛ[™ÝÂˆœ™XZÎÂˆBˆ]›ÝWÜÝ\HÛÝ\˜ÙWÜÝ\œØ]\˜][™×ØY
™[]]™WÜÝ\
NÂˆYˆ›ÝWÜÝ\H[˜ÚÜ—Ù[™Âˆœ™XZÎÂˆBˆ]š\ÚX›WÜÝ\H›ÝWÜÝ\›X^
[˜ÚÜ—ÜÝ\
NÂˆ]š\ÚX›WÙ[™H›ÝWÜÝ\ˆœØ]\˜][™×ØY
›ÝWÙ\˜][ÛŠBˆ›Z[ŠÛÝ\˜ÙWÙ[™
Bˆ›Z[Š[˜ÚÜ—Ù[™
NÂˆYˆš\ÚX›WÜÝ\š\ÚX›WÙ[™Âˆ]ÜÚ][Û—ÝXÚÜÈBˆLÌŽŽžWÙœ›ÛJš\ÚX›WÜÝ\H[˜ÚÜ—ÜÝ\
K[Ü˜\ÛÜŠLÌŽŽ“PV
NÂˆ][™ÝÝXÚÜÈBˆLÌŽŽžWÙœ›ÛJš\ÚX›WÙ[™Hš\ÚX›WÜÝ\
K[Ü˜\ÛÜŠLÌŽŽ“PV
NÂˆÚÜÝËš[œÙ\
X[›Ô›ÛÚÜÝ›ÝHÂˆÛÝ\˜ÙWØÛ\ˆÛÝ\˜ÙWØÛ\ÚÙ^KˆÚ[›™[ÚYˆ›ÝK˜Ú[›™[ÚYˆÙ^Nˆ›ÝKšÙ^KˆÜÚ][Û—ÝXÚÜËˆ[™ÝÝXÚÜËˆJNÂˆYˆÚÜÝË›[Š
HHPVÑÒÔÕÓ“ÕTÈÂˆœ™XZÈ	Ø[˜ÚÜœÎÂˆBˆBˆ]ÛÛYJ™^Ü™\]][ÛŠHH™\]][Û‹˜ÚXÚÙYØY
JH[ÙHÂˆœ™XZÎÂˆNÂˆ™\]][ÛˆH™^Ü™\]][ÛŽÂˆBˆBˆBˆB‚ˆÚÜÝËš[×Ú]\Š
K˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÛY[X™\œÊˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆÚ[›™[Û›ÝWÚ[™^ˆ\Ú^™KŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆ]ÛÛYJ›ÝJHH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ›
Ú[›™[Û›ÝWÚ[™^
Bˆ[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆYˆ›ÝK™Ü›Ý\OHÂˆ™]\›ˆ™XÈVÊ]\›—ÚYÚ[›™[ÚYÚ[›™[Û›ÝWÚ[™^
WNÂˆBˆ]Ü›Ý\H›ÝK™Ü›Ý\Âˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ë›ÝJ_›ÝK™Ü›Ý\OHÜ›Ý\
Bˆ›X\

[™^Ê_
]\›—ÚYÚ[›™[ÚY[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÛÙÛ›ÝWÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆXZ[—ÙÜšYÝXÚÜÎˆLÌ‹ŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆ]XZ[—ÙÜšYÝXÚÜÈHXZ[—ÙÜšYÝXÚÜË›X^
JNÂˆ]]]Ù™—ÙÜšYÙÜ›Ý\ÈH•™YTÙ]Ž
LM‹\Ú^™JOŽŽ›™]Ê
NÂˆ›Üˆ
›ÝWÚ[™^›ÝJH[ˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
BˆÂˆYˆ›ÝKœÜÚ][Ûˆ	HXZ[—ÙÜšYÝXÚÜÈOHÂˆ]Ü›Ý\ÚÙ^HHYˆ›ÝK™Ü›Ý\OHÂˆ
›ÝWÚ[™^
BˆH[ÙHÂˆ
›ÝK™Ü›Ý\
BˆNÂˆÙ™—ÙÜšYÙÜ›Ý\Ëš[œÙ\
Ü›Ý\ÚÙ^JNÂˆBˆB‚ˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\Š
›ÝWÚ[™^›ÝJ_Âˆ]Ü›Ý\ÚÙ^HHYˆ›ÝK™Ü›Ý\OHÂˆ

››ÝWÚ[™^
BˆH[ÙHÂˆ
›ÝK™Ü›Ý\
BˆNÂˆÙ™—ÙÜšYÙÜ›Ý\Ë˜ÛÛZ[œÊ	™Ü›Ý\ÚÙ^JBˆJBˆ›X\

›ÝWÚ[™^Ê_
]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÛÝ™\›\Û›ÝWÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆÝXÚÙYÛÛ›Nˆ›ÛÛŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆ]Ú[›™[Û›Ý\ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
NÂˆ]]]Ù[XÝYÚ[™XÙ\ÈH•™YTÙ]Ž›™]Ê
NÂ‚ˆYˆÝXÚÙYÛÛ›HÂˆ]]]›Ý\×ØžWÜÜ[ˆH•™YSX\Ž
LÌ‹LÌŠK™XÏ
\Ú^™KLMŠOŽŽ›™]Ê
NÂˆ›Üˆ
›ÝWÚ[™^›ÝJH[ˆÚ[›™[Û›Ý\ÈÂˆYˆ›ÝK›[™ÝˆÂˆ›Ý\×ØžWÜÜ[‚ˆ™[žJ
›ÝKœÜÚ][Û‹›ÝK›[™Ý
JBˆ›Ü—ÙY˜][

Bˆœ\Ú

›ÝWÚ[™^›ÝKšÙ^JJNÂˆBˆBˆ›Üˆ›Ý\È[ˆ›Ý\×ØžWÜÜ[‹˜[Y\Ê
HÂˆX[›×Ü›ÛÜÙ[XÝØÛÛ\Û™[ØX›Ý™WÛÝÙ\Ý
›Ý\Ë	›]]Ù[XÝYÚ[™XÙ\ÊNÂˆBˆH[ÙHÂˆ]]][\˜[ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\—ÛX\

›ÝWÚ[™^›ÝJ_Âˆ
›ÝK›[™Ýˆ
K[—ÜÛÛYJ
ˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠKˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠH
ÈMŽ™œ›ÛJ›ÝK›[™Ý
Kˆ›ÝWÚ[™^ˆ›ÝKšÙ^Kˆ
JBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ[\˜[ËœÛÜØžWÚÙ^J
Ý\[™›ÝWÚ[™^Ê_

œÝ\
™[™
››ÝWÚ[™^
JNÂ‚ˆ]]]ÛÛ\Û™[H™XÎŽ›™]Ê
NÂˆ]]]ÛÛ\Û™[Ù[™HÝMÂˆ›Üˆ
Ý\[™›ÝWÚ[™^Ù^JH[ˆ[\˜[ÈÂˆYˆXÛÛ\Û™[š\×Ù[\J
H	‰ˆÝ\HÛÛ\Û™[Ù[™ÂˆX[›×Ü›ÛÜÙ[XÝØÛÛ\Û™[ØX›Ý™WÛÝÙ\Ý
	˜ÛÛ\Û™[	›]]Ù[XÝYÚ[™XÙ\ÊNÂˆÛÛ\Û™[˜ÛX\Š
NÂˆBˆYˆÛÛ\Û™[š\×Ù[\J
HÂˆÛÛ\Û™[Ù[™H[™ÂˆH[ÙHÂˆÛÛ\Û™[Ù[™HÛÛ\Û™[Ù[™›X^
[™
NÂˆBˆÛÛ\Û™[œ\Ú

›ÝWÚ[™^Ù^JJNÂˆBˆX[›×Ü›ÛÜÙ[XÝØÛÛ\Û™[ØX›Ý™WÛÝÙ\Ý
	˜ÛÛ\Û™[	›]]Ù[XÝYÚ[™XÙ\ÊNÂˆB‚ˆ]Ù[XÝYÙÜ›Ý\ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\Š
›ÝWÚ[™^Ê_Ù[XÝYÚ[™XÙ\Ë˜ÛÛZ[œÊ›ÝWÚ[™^
JBˆ›X\

Ë›ÝJ_›ÝK™Ü›Ý\
Bˆ™š[\ŠÜ›Ý\
™Ü›Ý\OH
Bˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂ‚ˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\Š
›ÝWÚ[™^›ÝJ_ÂˆÙ[XÝYÚ[™XÙ\Ë˜ÛÛZ[œÊ›ÝWÚ[™^
Bˆ
›ÝK™Ü›Ý\OH	‰ˆÙ[XÝYÙÜ›Ý\Ë˜ÛÛZ[œÊ	››ÝK™Ü›Ý\
JBˆJBˆ›X\

›ÝWÚ[™^Ê_
]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÜÙ[XÝØÛÛ\Û™[ØX›Ý™WÛÝÙ\Ý
ˆ›Ý\Îˆ	–Ê\Ú^™KLMŠWKˆÙ[XÝYÚ[™XÙ\Îˆ	›]]•™YTÙ]\Ú^™O‹ŠHÂˆ]ÛÛYJ
ÝÙ\ÝÚ[™^ÊJHH›Ý\Âˆš]\Š
Bˆ›Z[—ØžWÚÙ^J
›ÝWÚ[™^Ù^J_

šÙ^K
››ÝWÚ[™^
JBˆ[ÙHÂˆ™]\›ŽÂˆNÂˆÙ[XÝYÚ[™XÙ\Ë™^[™
ˆ›Ý\Âˆš]\Š
Bˆ™š[\—ÛX\

›ÝWÚ[™^Ê_
›ÝWÚ[™^OHÝÙ\ÝÚ[™^
K[—ÜÛÛYJ
››ÝWÚ[™^
JKˆ
NÂŸB‚™›ˆX[›×Ü›ÛÜÙ[XÝ[Û—Ý[YWÜ˜[™ÙJˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÙ[XÝYÛ›Ý\Îˆ	•™YTÙ]
LM‹LM‹\Ú^™JO‹ŠHOˆÜ[Û
LÌ‹LÌŠOˆÂˆ]]]Ý\HLÌŽŽ“PVÂˆ]]][™HÝLÌŽÂˆ]]]›Ý[™Û›ÝHH˜[ÙNÂˆ]]]Ú[›™[Ú[™XÙ\ÈH•™YSX\ŽLM‹\Ú^™OŽŽ›™]Ê
NÂˆ›Üˆ›ÝH[ˆ	œ]\›‹››Ý\ÈÂˆ]›ÝWÚ[™^HÚ[›™[Ú[™XÙ\Ë™[žJ›ÝK˜Ú[›™[ÚY
K›Ü—ÙY˜][

NÂˆYˆÙ[XÝYÛ›Ý\Ë˜ÛÛZ[œÊ	Š]\›—ÚY›ÝK˜Ú[›™[ÚY
››ÝWÚ[™^
JHÂˆ]›ÝWÙ[™H›ÝKœÜÚ][Û‹œØ]\˜][™×ØY
›ÝK›[™Ý
NÂˆYˆ›ÝWÙ[™ˆ›ÝKœÜÚ][ÛˆÂˆÝ\HÝ\›Z[Š›ÝKœÜÚ][ÛŠNÂˆ[™H[™›X^
›ÝWÙ[™
NÂˆ›Ý[™Û›ÝHHYNÂˆBˆBˆ
››ÝWÚ[™^
ÏHNÂˆBˆ›Ý[™Û›ÝK[—ÜÛÛYJ
Ý\[™
JBŸB‚™›ˆX[›×Ü›ÛÝ[YWÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆÝ\ˆLÌ‹ˆ[™ˆLÌ‹ŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆYˆ[™HÝ\Âˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ]Ú[›™[Û›Ý\ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ]]]Ù[XÝYÚ[™XÙ\ÈH•™YTÙ]Ž›™]Ê
NÂˆ]]]Ù[XÝYÙÜ›Ý\ÈH•™YTÙ]Ž›™]Ê
NÂˆ›Üˆ
›ÝWÚ[™^›ÝJH[ˆÚ[›™[Û›Ý\Ëš]\Š
K™[[Y\˜]J
HÂˆYˆ
Ý\‹™[™
K˜ÛÛZ[œÊ	››ÝKœÜÚ][ÛŠHÂˆÙ[XÝYÚ[™XÙ\Ëš[œÙ\
›ÝWÚ[™^
NÂˆYˆ›ÝK™Ü›Ý\OHÂˆÙ[XÝYÙÜ›Ý\Ëš[œÙ\
›ÝK™Ü›Ý\
NÂˆBˆBˆB‚ˆÚ[›™[Û›Ý\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
›ÝWÚ[™^›ÝJ_ÂˆÙ[XÝYÚ[™XÙ\Ë˜ÛÛZ[œÊ›ÝWÚ[™^
Bˆ
›ÝK™Ü›Ý\OH	‰ˆÙ[XÝYÙÜ›Ý\Ë˜ÛÛZ[œÊ	››ÝK™Ü›Ý\
JBˆJBˆ›X\

›ÝWÚ[™^Ê_
]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÜ]ÚÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆš\œÝÚÙ^NˆLM‹ˆ\ÝÚÙ^NˆLM‹ŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆ]Ý×ÚÙ^HHš\œÝÚÙ^K›Z[Š\ÝÚÙ^JNÂˆ]YÚÚÙ^HHš\œÝÚÙ^K›X^
\ÝÚÙ^JNÂˆ]Ú[›™[Û›Ý\ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ]]]Ù[XÝYÚ[™XÙ\ÈH•™YTÙ]Ž›™]Ê
NÂˆ]]]Ù[XÝYÙÜ›Ý\ÈH•™YTÙ]Ž›™]Ê
NÂˆ›Üˆ
›ÝWÚ[™^›ÝJH[ˆÚ[›™[Û›Ý\Ëš]\Š
K™[[Y\˜]J
HÂˆYˆ
Ý×ÚÙ^K‹ZYÚÚÙ^JK˜ÛÛZ[œÊ	››ÝKšÙ^JHÂˆÙ[XÝYÚ[™XÙ\Ëš[œÙ\
›ÝWÚ[™^
NÂˆYˆ›ÝK™Ü›Ý\OHÂˆÙ[XÝYÙÜ›Ý\Ëš[œÙ\
›ÝK™Ü›Ý\
NÂˆBˆBˆB‚ˆÚ[›™[Û›Ý\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
›ÝWÚ[™^›ÝJ_ÂˆÙ[XÝYÚ[™XÙ\Ë˜ÛÛZ[œÊ›ÝWÚ[™^
Bˆ
›ÝK™Ü›Ý\OH	‰ˆÙ[XÝYÙÜ›Ý\Ë˜ÛÛZ[œÊ	››ÝK™Ü›Ý\
JBˆJBˆ›X\

›ÝWÚ[™^Ê_
]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆX[›×Ü›ÛÚÙ^WØ]ÞJˆÚ[\—ÞNˆŒÌ‹ˆÜšYÝÜˆŒÌ‹ˆÙ^WÚZYÚˆŒÌ‹ˆÙ^WÛÝÎˆLM‹ˆÙ^WÚYÚˆLM‹ŠHOˆLMˆÂˆ]›ÝÈH

Ú[\—ÞHHÜšYÝÜ
K›X^
Œ
HÈÙ^WÚZYÚ›X^
ŒÌŽŽ‘TÒSÓŠJBˆ™›ÛÜŠ
Bˆ˜Û[\
ŒŒÌŽŽ™œ›ÛJÙ^WÚYÚœØ]\˜][™×ÜÝXŠÙ^WÛÝÊJJH\ÈLMŽÂˆÙ^WÚYÚœØ]\˜][™×ÜÝXŠ›ÝÊBŸB‚™›ˆX[›×Ü›ÛÜÚYÝ[YWÜ˜[™ÙJÝ\ˆLÌ‹[™ˆLÌ‹\™XÝ[ÛŽˆ[YTÙ[XÝ[Û”ÚY
HOˆ
LÌ‹LÌŠHÂˆ]ÚYH[™œØ]\˜][™×ÜÝXŠÝ\
NÂˆ]Ý\HX]Ú\™XÝ[ÛˆÂˆ[YTÙ[XÝ[Û”ÚYŽ”™]š[Ý\ÈOˆÝ\œØ]\˜][™×ÜÝXŠÚY
Kˆ[YTÙ[XÝ[Û”ÚYŽ“™^OˆÝ\œØ]\˜][™×ØY
ÚY
KˆNÂˆ
Ý\Ý\œØ]\˜][™×ØY
ÚY
JBŸB‚™›ˆX[›×Ü›ÛÝ[YWÝXÚ×Ø]Þ
ˆÚ[\—ÞˆŒÌ‹ˆÜšYÛYˆŒÌ‹ˆXÚ×ÜØØ[NˆŒÌ‹ˆÛ˜\ÝXÚÜÎˆLÌ‹ŠHOˆLÌˆÂˆ]XÚÈH

Ú[\—ÞHÜšYÛY
K›X^
Œ
HÈXÚ×ÜØØ[K›X^
ŒÌŽŽ‘TÒSÓŠJBˆœ›Ý[™

Bˆ˜Û[\
ŒLÌŽŽ“PV\ÈŒÌŠH\ÈMÂˆÛ˜\Û›ÝWÝXÚÊXÚËÛ˜\ÝXÚÜË
BŸB‚™›ˆX[›×Ü›ÛÜÛXÙWÛ[™WØÝ]Þ
ˆÝ\ˆYÝZNŽ”ÜÌ‹ˆ[™ˆYÝZNŽ”ÜÌ‹ˆ™XÝˆYÝZNŽ”™XÝŠHOˆÜ[ÛŒÌˆÂˆ]H[™žHÝ\žÂˆ]HH[™žHHÝ\žNÂˆYˆ˜XœÊ
HHŒÌŽŽ‘TÒSÓˆ	‰ˆK˜XœÊ
HHŒÌŽŽ‘TÒSÓˆÂˆ™]\›ˆ›Û™NÂˆBˆ]]]š\œÝÝHŒŒÌŽÂˆ]]]\ÝÝHKŒŒÌŽÂˆ›Üˆ
JH[ˆÂˆ
YÝ\žH™XÝ›Y

JKˆ
™XÝœšYÚ

HHÝ\ž
Kˆ
YKÝ\žHH™XÝÜ

JKˆ
K™XÝ˜›ÝÛJ
HHÝ\žJKˆHÂˆYˆOHŒÂˆYˆHŒÂˆ™]\›ˆ›Û™NÂˆBˆÛÛ[YNÂˆBˆ]HHÈÂˆYˆŒÂˆYˆˆ\ÝÝÂˆ™]\›ˆ›Û™NÂˆBˆš\œÝÝHš\œÝÝ›X^

NÂˆH[ÙHÂˆYˆš\œÝÝÂˆ™]\›ˆ›Û™NÂˆBˆ\ÝÝH\ÝÝ›Z[Š
NÂˆBˆBˆ
š\œÝÝH\ÝÝ
K[—ÜÛÛYJÝ\ž
È
ˆ

š\œÝÝ
È\ÝÝ
H
ˆJJBŸB‚™›ˆX[›×Ü›ÛÝ[YWÜ˜[™ÙWÙœ›ÛWÙ˜YÊˆÝ\ÝXÚÎˆLÌ‹ˆÝ\œ™[ÝXÚÎˆLÌ‹ˆÛ˜\ÝXÚÜÎˆLÌ‹ŠHOˆ
LÌ‹LÌŠHÂˆ]š\œÝÝXÚÈHÝ\ÝXÚË›Z[ŠÝ\œ™[ÝXÚÊNÂˆ]\ÝÝXÚÈHÝ\ÝXÚË›X^
Ý\œ™[ÝXÚÊNÂˆ]Ý\HÛ˜\Û›ÝWÝXÚÊMŽ™œ›ÛJš\œÝÝXÚÊKÛ˜\ÝXÚÜË
NÂˆ][™HÛ˜\Û›ÝWÝXÚÊMŽ™œ›ÛJ\ÝÝXÚÊKÛ˜\ÝXÚÜËÝ\
Bˆ›X^
Ý\œØ]\˜][™×ØY
Û˜\ÝXÚÜË›X^
JJJNÂˆ
Ý\[™
BŸB‚™›ˆX[›×Ü›ÛÙ\XØ]WÛÙ™œÙ]
ˆš\œÝÜÜÚ][ÛŽˆLÌ‹ˆ\ÝÙ[™ˆLÌ‹ˆ[YWÜÙ[XÝ[ÛŽˆÜ[Û
LÌ‹LÌŠO‹ŠHOˆLÌˆÂˆ[YWÜÙ[XÝ[Û‚ˆ›X\

Ý\[™
_[™œØ]\˜][™×ÜÝXŠÝ\
JBˆ™š[\ŠÚY
ÚYˆ
Bˆ[Ü˜\ÛÜ—Ù[ÙJ\ÝÙ[™œØ]\˜][™×ÜÝXŠš\œÝÜÜÚ][ÛŠK›X^
JJBŸB‚™›ˆX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆÙ[XÝYˆ	•™YTÙ]
LM‹LM‹\Ú^™JO‹ˆÙ[XÝÛ[Ü™Nˆ›ÛÛˆÙYYˆMŠHOˆÜ[Û•™YTÙ]
LM‹LM‹\Ú^™JOˆÂˆ]]]Ü›Ý\ÈH•™YSX\Ž
LM‹\Ú^™JK™XÏ
LM‹LM‹\Ú^™JOŽŽ›™]Ê
NÂˆ›Üˆ
›ÝWÚ[™^›ÝJH[ˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
BˆÂˆ]Ü›Ý\ÚÙ^HHYˆ›ÝK™Ü›Ý\OHÂˆ
›ÝWÚ[™^
BˆH[ÙHÂˆ
›ÝK™Ü›Ý\
BˆNÂˆÜ›Ý\Âˆ™[žJÜ›Ý\ÚÙ^JBˆ›Ü—ÙY˜][

Bˆœ\Ú

]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JNÂˆB‚ˆ]Ø[™Y]\ÈHÜ›Ý\Âˆš[×Ý˜[Y\Ê
Bˆ™š[\ŠY[X™\œß\Ù[XÝÛ[Ü™HY[X™\œËš]\Š
K˜[
Y[X™\Ÿ\Ù[XÝY˜ÛÛZ[œÊY[X™\ŠJJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ]Ü›Ý\Ú[™^HX[›×Ü›ÛÜ˜[™ÛWÚ[™^
Ø[™Y]\Ë›[Š
KÙYY
OÎÂˆ]]]™\Ý[HYˆÙ[XÝÛ[Ü™HÂˆÙ[XÝY˜ÛÛ™J
BˆH[ÙHÂˆ•™YTÙ]Ž›™]Ê
BˆNÂˆ™\Ý[™^[™
Ø[™Y]\ÖÙÜ›Ý\Ú[™^Kš]\Š
K˜ÛÜYY

JNÂˆÛÛYJ™\Ý[
BŸB‚™›ˆX[›×Ü›ÛÜ˜[™ÛWÚ[™^
[™Ýˆ\Ú^™KÙYYˆM
HOˆÜ[Û\Ú^™OˆÂˆYˆ[™ÝOHÂˆ™]\›ˆ›Û™NÂˆB‚ˆ]]]˜[YHHÙYYÜ˜\[™×ØY
QLÍ×ÍÎPŽWÍÑWÍÐÌMJNÂˆ˜[YHH
˜[YHˆ
˜[YHˆÌ
JKÜ˜\[™×Û][
‘NÍÍ‘ÌPÑMÑMPŽJNÂˆ˜[YHH
˜[YHˆ
˜[YHˆÊJKÜ˜\[™×Û][
MÍP—ÌLÌÌWÌLQPŠNÂˆ˜[YHH˜[YHˆÌNÂˆÛÛYJ
˜[YH	H[™Ý\ÈM
H\È\Ú^™JBŸB‚™›ˆX[›×Ü›ÛÛ›ÝWÛYÙWÙY]
ˆ›ÝNˆ	”]\›“›ÝKˆXÚ×Ù[NˆMˆÙ[Z]Û™WÙ[NˆLÌ‹ŠHOˆ]\›“›ÝQY]Âˆ]ÜÚ][ÛˆH
XÚ×Ù[HOH
K[ŠÂˆ][[Ý[HXÚ×Ù[K[œÚYÛ™YØXœÊ
K›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌŽÂˆYˆXÚ×Ù[HÂˆ›ÝKœÜÚ][Û‹œØ]\˜][™×ÜÝXŠ[[Ý[
BˆH[ÙHÂˆ›ÝKœÜÚ][Û‹œØ]\˜][™×ØY
[[Ý[
BˆBˆJNÂˆ]Ù^HH
Ù[Z]Û™WÙ[HOH
K[ŠÂˆLÌŽŽ™œ›ÛJ›ÝKšÙ^JBˆœØ]\˜][™×ØY
Ù[Z]Û™WÙ[JBˆ˜Û[\
LÌJH\ÈLM‚ˆJNÂˆ]\›“›ÝQY]ÂˆÜÚ][ÛŽˆÜÚ][Û‹™š[\ŠÜÚ][ÛŸ
œÜÚ][ÛˆOH›ÝKœÜÚ][ÛŠKˆÙ^NˆÙ^K™š[\ŠÙ^_
šÙ^HOH›ÝKšÙ^JKˆ‹”]\›“›ÝQY]Ž™Y˜][

BˆBŸB‚™›ˆX[›×Ü›ÛÙ\ØØ\™Û[™ÝÙY]
ˆ›ÝNˆ	”]\›“›ÝKˆZ[š[][WÛ[™ÝÝXÚÜÎˆLÌ‹ŠHOˆÜ[Û]\›“›ÝQY]ˆÂˆ][™ÝHZ[š[][WÛ[™ÝÝXÚÜË›X^
JNÂˆ
›ÝK›[™ÝOH[™Ý
K[—ÜÛÛYJ]\›“›ÝQY]Âˆ[™ÝˆÛÛYJ[™Ý
Kˆ‹”]\›“›ÝQY]Ž™Y˜][

BˆJBŸB‚™›ˆX[›×Ü›ÛØÛÛÜ—ÙÜ›Ý\Û›ÝWÚYÊˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆÛÛÜ—ÙÜ›Ý\ˆNŠHOˆ™XÏ
LM‹LM‹\Ú^™JOˆÂˆ]ÛÛÜ—ÙÜ›Ý\HÛÛÜ—ÙÜ›Ý\	ˆŽÂˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ë›ÝJ_›ÝK›ZYWØÚ[›™[	ˆˆOHÛÛÜ—ÙÜ›Ý\
Bˆ›X\

›ÝWÚ[™^Ê_
]\›—ÚYÚ[›™[ÚY›ÝWÚ[™^
JBˆ˜ÛÛXÝ

BŸB‚™›ˆÙÙÛWÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÜÙ[XÝ[ÛŠˆÙ[XÝYˆ	›]]•™YTÙ]
LM‹LM‹\Ú^™JO‹ˆY[X™\œÎˆ	–ÊLM‹LM‹\Ú^™JWKˆY]]™Nˆ›ÛÛŠHOˆ›ÛÛÂˆ]™[[Ý™HBˆY]]™H	‰ˆ[Y[X™\œËš\×Ù[\J
H	‰ˆY[X™\œËš]\Š
K˜[
Y[X™\ŸÙ[XÝY˜ÛÛZ[œÊY[X™\ŠJNÂˆYˆ™[[Ý™HÂˆ›ÜˆY[X™\ˆ[ˆY[X™\œÈÂˆÙ[XÝYœ™[[Ý™JY[X™\ŠNÂˆBˆH[ÙHÂˆÙ[XÝY™^[™
Y[X™\œËš]\Š
K˜ÛÜYY

JNÂˆBˆ\™[[Ý™BŸB‚™›ˆ\]WÜX[›×Ü›ÛØ›ÞÜÙ[XÝ[ÛŠˆ]\›Žˆ	”]\›‹ˆ]\›—ÚYˆLM‹ˆÚ[›™[ÚYˆLM‹ˆ[˜ÛÜÙYÛ›ÝWÚ[™XÙ\Îˆ	–Ý\Ú^™WKˆY]]™Nˆ›ÛÛˆÙ[XÝYˆ	›]]•™YTÙ]
LM‹LM‹\Ú^™JO‹ŠHÂˆ]]]›ØÙ\ÜÙYÙÜ›Ý\ÈH•™YTÙ]Ž›™]Ê
NÂˆ›Üˆ›ÝWÚ[™^[ˆ[˜ÛÜÙYÛ›ÝWÚ[™XÙ\ÈÂˆ]Ü›Ý\H]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY
Bˆ›

››ÝWÚ[™^
Bˆ›X\ÛÜŠ›Ý_›ÝK™Ü›Ý\
NÂˆ]Ù^HHYˆÜ›Ý\OHÂˆ
Ü›Ý\
››ÝWÚ[™^
BˆH[ÙHÂˆ
Ü›Ý\
BˆNÂˆYˆ\›ØÙ\ÜÙYÙÜ›Ý\Ëš[œÙ\
Ù^JHÂˆÛÛ[YNÂˆBˆ]Y[X™\œÈHX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÛY[X™\œÊ]\›‹]\›—ÚYÚ[›™[ÚY
››ÝWÚ[™^
NÂˆÙÙÛWÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÜÙ[XÝ[ÛŠÙ[XÝY	›Y[X™\œËY]]™JNÂˆBŸB‚™›ˆ™^ÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\
]\›Žˆ	”]\›‹Ú[›™[ÚYˆLMŠHOˆÜ[ÛLMˆÂˆ]\ÙYÙÜ›Ý\ÈH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK˜Ú[›™[ÚYOHÚ[›™[ÚY	‰ˆ›ÝK™Ü›Ý\OH
Bˆ›X\
›Ý_›ÝK™Ü›Ý\
Bˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂˆ
K‹]LMŽŽ“PV
K™š[™
Ü›Ý\]\ÙYÙÜ›Ý\Ë˜ÛÛZ[œÊÜ›Ý\
JBŸB‚™›ˆ]]ÛX][Û—ÜÚ[ÜØÜ™Y[—ÜÜÚ][ÛŠˆÚ[ˆ	]]ÛX][Û”Ú[ˆÝÜ™XÝˆYÝZNŽ”™XÝˆš\ÚX›WØ™X]ÎˆŠHOˆYÝZNŽ”ÜÌˆÂˆ]™X]Ùœ˜XÝ[ÛˆHYˆÚ[œÜÚ][Û—Ø™X]Ê
Kš\×Ùš[š]J
HÂˆ
Ú[œÜÚ][Û—Ø™X]Ê
HÈš\ÚX›WØ™X]ÊK˜Û[\
ŒKŒ
H\ÈŒÌ‚ˆH[ÙHÂˆŒˆNÂˆ]˜[YWÙœ˜XÝ[ÛˆHYˆÚ[˜[YJ
Kš\×Ùš[š]J
HÂˆÚ[˜[YJ
K˜Û[\
ŒKŒ
H\ÈŒÌ‚ˆH[ÙHÂˆBˆNÂˆYÝZNŽœÜÌŠˆYÝZNŽ›\œ
ÝÜ™XÝ›Y

K‹\ÝÜ™XÝœšYÚ

K™X]Ùœ˜XÝ[ÛŠKˆYÝZNŽ›\œ
ÝÜ™XÝ˜›ÝÛJ
K‹\ÝÜ™XÝÜ

K˜[YWÙœ˜XÝ[ÛŠKˆ
BŸB‚™›ˆ[™^Øœ›ÝÜÙ\—Ù›Û\œÊ›ÛÝÎˆ™XÏ]Y‹[Ü›ÛÝÎˆ›ÛÛ
HOˆ™\Ý[œ›ÝÜÙ\’[™^Ýš[™ÏˆÂˆ]]][šY\ÈH™XÎŽ›™]Ê
NÂˆ]]]ÙY[—Ü]ÈH•™YTÙ]Ž›™]Ê
NÂˆ]]]ØØ[—Ù[šY\ÈH\Ú^™NÂˆ]]][˜Ø]YH˜[ÙNÂˆ]]][˜]˜Z[X›WÜ›ÛÝÈH™XÎŽ›™]Ê
NÂ‚ˆ	Ü›ÛÝÎˆ›Üˆ›ÛÝ[ˆ	œ›ÛÝÈÂˆ]]]\™XÝÜšY\ÈH™XÑ\]YNŽ™œ›ÛJÜ›ÛÝ˜ÛÛ™J
WJNÂˆ	Ý™YNˆÚ[H]ÛÛYJ\™XÝÜžJHH\™XÝÜšY\ËœÜÙœ›Û

HÂˆ]Ú[™[ˆHX]ÚœÎŽœ™XYÙ\Š	™\™XÝÜžJHÂˆÚÊÚ[™[ŠHOˆÚ[™[‹ˆ\œŠ\œ›ÜŠHYˆ\™XÝÜžHOH
œ›ÛÝOˆÂˆYˆ[Ü›ÛÝÈÂˆ[˜]˜Z[X›WÜ›ÛÝËœ\Ú
›ÛÝ˜ÛÛ™J
JNÂˆœ™XZÈ	Ý™YNÂˆBˆ™]\›ˆ\œŠ›Ü›X]JÛÝ[›ÝÙX\˜ÚßNˆÙ\œ›ÜŸH‹›ÛÝ™\Ü^J
JJNÂˆBˆ\œŠÊHOˆÛÛ[YKˆNÂˆ›ÜˆÚ[[ˆÚ[™[ˆÂˆ]ÚÊÚ[
HHÚ[[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÚÊš[WÝ\JHHÚ[™š[WÝ\J
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]]HÚ[œ]

NÂˆ]˜[YHHÚ[™š[WÛ˜[YJ
K×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

NÂˆYˆœ›ÝÜÙ\—Ù[žWÚ\×ÚY[Š	›˜[YJHÂˆÛÛ[YNÂˆBˆYˆØØ[—Ù[šY\ÈHPVÐ”“ÕÔÑT—Ô‘PÕT”ÒU‘WÔÐÐS—ÑS•’QTÈÂˆ[˜Ø]YHYNÂˆœ™XZÈ	Ü›ÛÝÎÂˆBˆØØ[—Ù[šY\È
ÏHNÂˆYˆš[WÝ\Kš\×Ù\Š
HÂˆ\™XÝÜšY\Ëœ\ÚØ˜XÚÊ]
NÂˆH[ÙHYˆš[WÝ\Kš\×Ùš[J
Bˆ	‰ˆœ›ÝÜÙ\—Ùš[WÚÚ[™
	œ]
Kš\×ÜÛÛYJ
Bˆ	‰ˆÙY[—Ü]Ëš[œÙ\
]˜ÛÛ™J
JBˆÂˆ]™[]]™WÜ]H]œÝš\Ü™Yš^
›ÛÝ
K[Ü˜\ÛÜŠ	œ]
NÂˆ]˜[YHHYˆ[Ü›ÛÝÈÂˆ]›ÛÝÛ˜[YHH›ÛÝˆ™š[WÛ˜[YJ
Bˆ›X\
˜[Y_˜[YK×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

JBˆ[Ü˜\ÛÜ—Ù[ÙJ›ÛÝ™\Ü^J
K×ÜÝš[™Ê
JNÂˆ›Ü›X]JžÜ›ÛÝÛ˜[Y_KÞßH‹™[]]™WÜ]™\Ü^J
JBˆH[ÙHÂˆ™[]]™WÜ]×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

BˆNÂˆ[šY\Ëœ\Ú
œ›ÝÜÙ\‘[žHÂˆ˜[YKˆ]ˆ\×Ù\™XÝÜžNˆ˜[ÙKˆJNÂˆBˆBˆBˆB‚ˆ[šY\ËœÛÜØžJYšYÚÂˆY›˜[YBˆ×ÛÝÙ\˜Ø\ÙJ
Bˆ˜Û\
	œšYÚ›˜[YK×ÛÝÙ\˜Ø\ÙJ
JBˆ[—ÝÚ]
Yœ]˜Û\
	œšYÚœ]
JBˆJNÂˆÚÊœ›ÝÜÙ\’[™^Âˆ›ÛÝËˆ[Ü›ÛÝËˆ[šY\Ëˆ[˜Ø]Yˆ[˜]˜Z[X›WÜ›ÛÝËˆJBŸB‚™›ˆœ›ÝÜÙ\—Ù[žWÚ\×ÚY[Š˜[YNˆ	œÝŠHOˆ›ÛÛÂˆ˜[YKœÝ\×ÝÚ]
	Ë‰ÊBŸB‚™›ˆY˜][Øœ›ÝÜÙ\—Ù\™XÝÜžJ
HOˆ]YˆÂˆ]ÛYHHÝŽ™[ŽŽ˜\—ÛÜÊ’ÓQHŠBˆ›Ü—Ù[ÙJÝŽ™[ŽŽ˜\—ÛÜÊ•TÑT”“Ñ’SHŠJBˆ›X\
]YŽŽ™œ›ÛJBˆ™š[\Š]]š\×Ù\Š
JNÂˆœ›ÝÜÙ\—Ù˜XÝÜžWÜXÚÜ×Ù\™XÝÜžJ
Bˆ›Ü—Ù[ÙJÂˆÛYK˜\×Ü™YŠ
Bˆ›X\
]]š›Ú[Š“]\ÚXÈŠJBˆ™š[\Š]]š\×Ù\Š
JBˆJBˆ›ÜŠÛYJBˆ›Ü—Ù[ÙJÝŽ™[ŽŽ˜Ý\œ™[Ù\Š
K›ÚÊ
JBˆ[Ü˜\ÛÜ—ÙY˜][

BŸB‚™›ˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
HOˆÜ[Û]YˆÂˆ]›ÛÝHYˆÙ™ÈJ\™Ù]ÛÜÈHÚ[™ÝÜÈŠHÂˆÝŽ™[ŽŽ˜\—ÛÜÊTUHŠK›X\
]YŽŽ™œ›ÛJK›Ü—Ù[ÙJÂˆÝŽ™[ŽŽ˜\—ÛÜÊ•TÑT”“Ñ’SHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š\]HŠKš›Ú[Š”›Ø[Z[™ÈŠJBˆJBˆH[ÙHYˆÙ™ÈJ\™Ù]ÛÜÈH›XXÛÜÈŠHÂˆÝŽ™[ŽŽ˜\—ÛÜÊ’ÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š“Xœ˜\žHŠKš›Ú[Š\XØ][ÛˆÝ\ÜŠJBˆH[ÙHÂˆÝŽ™[ŽŽ˜\—ÛÜÊ–×ÐÓÓ‘’Q×ÒÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›Ü—Ù[ÙJÂˆÝŽ™[ŽŽ˜\—ÛÜÊ’ÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š‹˜ÛÛ™šYÈŠJBˆJBˆOÎÂˆÛÛYJ›ÛÝš›Ú[Š™›\ÝY[Ë\™XZ[ŠKš›Ú[Š˜œ›ÝÜÙ\‹Y˜]›Üš]\ËŠJBŸB‚™›ˆœ›ÝÜÙ\—ÜÙX\˜ÚÜ›ÛÝ×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜œ›ÝÜÙ\‹\ÙX\˜Ú\›ÛÝËŠJBŸB‚™›ˆœ›ÝÜÙ\—ÝYÜ×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜œ›ÝÜÙ\‹]YÜËŠJBŸB‚™›ˆœ›ÝÜÙ\—ÜØ]™YÜÙX\˜Ú\×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜œ›ÝÜÙ\‹\Ø]™Y\ÙX\˜Ú\ËŠJBŸB‚™›ˆ™XÙ[Ü›Ú™XÝ×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Šœ™XÙ[\›Ú™XÝËŠJBŸB‚™›ˆZYWÙ]šXÙWÜÙ[XÝ[Ûœ×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š›ZYKY]šXÙK\Ù[XÝ[ÛœËŠJBŸB‚™›ˆZYWÝWÜ™Y™\™[˜Ù\×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š›ZYK]K\™Y™\™[˜Ù\ËŠJBŸB‚™›ˆZYWÛÝ]]ÙØZ[—ÛX\[™×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYK[Ý]]YØZ[‹[X\[™ËŠBŸB‚™›ˆZYWÛZ^\—ØÛÛ›ÛÛX\[™×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYK[Z^\‹XÛÛ›Û[X\[™ÜËŠBŸB‚™›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYK]œÝË\\˜[Y]\‹[X\[™ÜËŠBŸB‚™›ˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜ×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[Jœ]\›‹]œÝË\\˜[Y]\‹[[šÜËŠBŸB‚™›ˆYØXÞWÛZYWÛZ^\—Ù˜Y\—ÛX\[™×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYK[Z^\‹Y˜Y\‹[X\[™ËŠBŸB‚™›ˆZYWÜ^WÜ]\ÙWÛX\[™×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYK\^K\]\ÙK[X\[™ËŠBŸB‚™›ˆZYWØÛÛ›Û\—Ý[\]\×Ùš[J
HOˆÜ[Û]YˆÂˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[J›ZYKXÛÛ›Û\‹][\]\ËŠBŸB‚™›ˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ùš[Jš[[˜[YNˆ	œÝŠHOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Šš[[˜[YJJBŸB‚™›ˆØYÛZYWÛÝ]]ÙØZ[—ÛX\[™Ê
HOˆÜ[ÛZYPÛÛ›ÛÚ[™ÙSX\[™ÏˆÂˆ]ÛÛ[ÈHœÎŽœ™XYÝ×ÜÝš[™ÊZYWÛÝ]]ÙØZ[—ÛX\[™×Ùš[J
OÊK›ÚÊ
OÎÂˆ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Ê	˜ÛÛ[ÊBŸB‚™›ˆØYÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ
HOˆ™XÏZYSZ^\ÛÛ›ÛX\[™ÏˆÂˆZYWÛZ^\—ØÛÛ›ÛÛX\[™×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›Ü—Ù[ÙJÂˆYØXÞWÛZYWÛZ^\—Ù˜Y\—ÛX\[™×Ùš[J
K˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆJBˆ›X\
ÛÛ[ß\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ	˜ÛÛ[ÊJBˆ[Ü˜\ÛÜ—ÙY˜][

BŸB‚™›ˆØYÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ
HOˆ™XÏZYUœÝÔ\˜[Y]\“X\[™ÏˆÂˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›X\
ÛÛ[ß\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ	˜ÛÛ[ÊJBˆ[Ü˜\ÛÜ—ÙY˜][

BŸB‚™›ˆØYÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ
HOˆ™XÏ]\›•œÝÔ\˜[Y]\“[šÏˆÂˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜ×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›X\
ÛÛ[ß\œÙWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ	˜ÛÛ[ÊJBˆ[Ü˜\ÛÜ—ÙY˜][

BŸB‚™›ˆØYÛZYWÜ^WÜ]\ÙWÛX\[™Ê
HOˆÜ[ÛZYPÛÛ›ÛÚ[™ÙSX\[™ÏˆÂˆ]ÛÛ[ÈHœÎŽœ™XYÝ×ÜÝš[™ÊZYWÜ^WÜ]\ÙWÛX\[™×Ùš[J
OÊK›ÚÊ
OÎÂˆ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Ê	˜ÛÛ[ÊBŸB‚™›ˆØYÛZYWØÛÛ›Û\—Ý[\]\Ê
HOˆ™XÏZYPÛÛ›Û\•[\]OˆÂˆ]]][\]\ÈHZYWØÛÛ›Û\—Ý[\]\×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›X\
ÛÛ[ß\œÙWÛZYWØÛÛ›Û\—Ý[\]\Ê	˜ÛÛ[ÊJBˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ›ÜˆZ[Ú[ˆ[ˆZ[Ú[—ÛZYWØÛÛ›Û\—Ý[\]\Ê
HÂˆYˆ][\]\Âˆš]\Š
Bˆ˜[žJ[\]_[\]K›˜[YK™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ	˜Z[Ú[‹›˜[YJJBˆÂˆ[\]\Ëœ\Ú
Z[Ú[ŠNÂˆBˆBˆ[\]\ÂŸB‚™›ˆZ[Ú[—ÛZYWØÛÛ›Û\—Ý[\]\Ê
HOˆ™XÏZYPÛÛ›Û\•[\]OˆÂˆ]]][\]\ÈH™XÈVÂˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ‘Ù[™\˜[RQHÚˆH
ÐÈÈ
ÈÐÈ
H‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆJKˆ^WÜ]\ÙNˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆˆJKˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ“RQH^™\ÜÚ[ÛˆÚˆH
ÐÈLJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆLKˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ–UÕPÒRS’H™\Ù]H
ÚˆLJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆLˆÛÛ›Û\ŽˆKˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ–UÕPÒRS’H™\Ù]ˆ
ÚˆLJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆLˆÛÛ›Û\ŽˆLˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ–UÕPÒÓÓTPÕ™\Ù]^Y\ˆH˜Y\ˆH
ÚˆJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆKˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆZØZHRQ[Z^X\Ý\ˆ˜Y\ˆ
ÚˆJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆŒ‹ˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆZYPÛÛ›Û\•[\]HÂˆ˜[YNˆZØZHRQ[Z^˜Y\ˆH
ÚˆJH‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆNKˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆKˆNÂˆ[\]\Ë™^[™
ˆÂˆ
‹ŒÊKˆ
ËÊKˆ
ÌJKˆ
KJKˆ
‹LÊKˆ
ËMÊKˆ
ŒJKˆBˆš[×Ú]\Š
Bˆ›X\

˜Y\‹ÛÛ›Û\Š_ZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ›Ü›X]JZØZHRQ[Z^˜Y\ˆÙ˜Y\ŸH
ÚˆJHŠKˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\‹ˆJKˆ^WÜ]\ÙNˆ›Û™KˆZ[Ú[ŽˆYKˆJKˆ
NÂˆ[\]\ÂŸB‚™›ˆØ]™WÛZYWÛÝ]]ÙØZ[—ÛX\[™ÊX\[™ÎˆÜ[Û	“ZYPÛÛ›ÛÚ[™ÙSX\[™ÏŠHOˆ™\Ý[

KÝš[™ÏˆÂˆØ]™WÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™ÊZYWÛÝ]]ÙØZ[—ÛX\[™×Ùš[J
KX\[™ÊBŸB‚™›ˆØ]™WÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊX\[™ÜÎˆ	–ÓZYSZ^\ÛÛ›ÛX\[™×JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HZYWÛZ^\—ØÛÛ›ÛÛX\[™×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQHZ^\ˆÛÛ›ÛX\[™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]Ù\šX[^™WÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊX\[™ÜÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÛZYWÜ^WÜ]\ÙWÛX\[™ÊX\[™ÎˆÜ[Û	“ZYPÛÛ›ÛÚ[™ÙSX\[™ÏŠHOˆ™\Ý[

KÝš[™ÏˆÂˆØ]™WÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™ÊZYWÜ^WÜ]\ÙWÛX\[™×Ùš[J
KX\[™ÊBŸB‚™›ˆØ]™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊX\[™ÜÎˆ	–ÓZYUœÝÔ\˜[Y]\“X\[™×JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH”ÕÈ\˜[Y]\ˆX\[™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]Ù\šX[^™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊX\[™ÜÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ[šÜÎˆ	–Ô]\›•œÝÔ\˜[Y]\“[š×JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜ×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH]\›ˆ”ÕÈ\˜[Y]\ˆ[šÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]Ù\šX[^™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ[šÜÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÛZYWØÛÛ›Û\—Ý[\]\Ê[\]\Îˆ	–ÓZYPÛÛ›Û\•[\]WJHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HZYWØÛÛ›Û\—Ý[\]\×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQHÛÛ›Û\ˆ[\]H]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]Ù\šX[^™WÛZYWØÛÛ›Û\—Ý[\]\Ê[\]\ÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Êˆ]ˆÜ[Û]Y‹ˆX\[™ÎˆÜ[Û	“ZYPÛÛ›ÛÚ[™ÙSX\[™Ï‹ŠHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H]›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQHÝ]]X\[™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈHX\[™Ë›X\ÛÜ—Ù[ÙJˆš[œ]W˜Ú[›™[W˜ÛÛ›Û\Wˆ‹×ÛÝÛ™Y

KˆX\[™ßÂˆ›Ü›X]Jˆš[œ]^ßW˜Ú[›™[^ßW˜ÛÛ›Û\^ßWˆ‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ëš[œ]Ù]šXÙWÚY
KˆX\[™Ë˜Ú[›™[ˆX\[™Ë˜ÛÛ›Û\‚ˆ
BˆKˆ
NÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™ÊÛÛ[Îˆ	œÝŠHOˆÜ[ÛZYPÛÛ›ÛÚ[™ÙSX\[™ÏˆÂˆ]]][œ]Ù]šXÙWÚYH›Û™NÂˆ]]]Ú[›™[H›Û™NÂˆ]]]ÛÛ›Û\ˆH›Û™NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆYˆ]ÛÛYJ[œ]
HH[™KœÝš\Ü™Yš^
š[œ]HŠHÂˆ[œ]Ù]šXÙWÚYHXÛÙWÛZYWÙ]šXÙWÚY
[œ]
NÂˆH[ÙHYˆ]ÛÛYJ˜[YJHH[™KœÝš\Ü™Yš^
˜Ú[›™[HŠHÂˆÚ[›™[H˜[YKœ\œÙNŽNŠ
K›ÚÊ
NÂˆH[ÙHYˆ]ÛÛYJ˜[YJHH[™KœÝš\Ü™Yš^
˜ÛÛ›Û\HŠHÂˆÛÛ›Û\ˆH˜[YKœ\œÙNŽNŠ
K›ÚÊ
NÂˆBˆBˆ][œ]Ù]šXÙWÚYH[œ]Ù]šXÙWÚYÎÂˆ]Ú[›™[HÚ[›™[ÎÂˆ]ÛÛ›Û\ˆHÛÛ›Û\ÎÂˆ
Ú[›™[Mˆ	‰ˆÛÛ›Û\ˆLŽ
K[—ÜÛÛYJZYPÛÛ›ÛÚ[™ÙSX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆÚ[›™[ˆÛÛ›Û\‹ˆJBŸB‚™›ˆ˜[YÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÊX\[™Îˆ	“ZYUœÝÔ\˜[Y]\“X\[™ÊHOˆ›ÛÛÂˆ[X\[™Ëš[œ]Ù]šXÙWÚYš\×Ù[\J
Bˆ	‰ˆX\[™Ë˜Ú[›™[M‚ˆ	‰ˆX\[™Ë˜ÛÛ›Û\ˆLŽˆ	‰ˆ[X\[™Ë\™Ù]œYÚ[—ÝZYš\×Ù[\J
Bˆ	‰ˆ[X\[™Ë\™Ù]œ\˜[Y]\—Û˜[YKš\×Ù[\J
BŸB‚™›ˆØ[YWÛZYWÝœÝ×Ü\˜[Y]\—Ý\™Ù]
ˆYˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ˆšYÚˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHOˆ›ÛÛÂˆYœ›Ú™XÝØÚ[›™[ÚYOHšYÚœ›Ú™XÝØÚ[›™[ÚYˆ	‰ˆYœYÚ[—ÝZYOHšYÚœYÚ[—ÝZYˆ	‰ˆYœ\˜[Y]\—ÚYOHšYÚœ\˜[Y]\—ÚYŸB‚™›ˆ\Ù\ÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÊˆX\[™ÜÎˆ	›]]™XÏZYUœÝÔ\˜[Y]\“X\[™Ï‹ˆX\[™ÎˆZYUœÝÔ\˜[Y]\“X\[™ËŠHÂˆYˆ]˜[YÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ê	›X\[™ÊHÂˆ™]\›ŽÂˆBˆX\[™ÜËœ™]Z[Š^\Ý[™ß\Ø[YWÛZYWÝœÝ×Ü\˜[Y]\—Ý\™Ù]
	™^\Ý[™Ë\™Ù]	›X\[™Ë\™Ù]
JNÂˆX\[™ÜËœ\Ú
X\[™ÊNÂŸB‚™›ˆ™[[Ý™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÊˆX\[™ÜÎˆ	›]]™XÏZYUœÝÔ\˜[Y]\“X\[™Ï‹ˆ\™Ù]ˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHÂˆX\[™ÜËœ™]Z[ŠX\[™ß\Ø[YWÛZYWÝœÝ×Ü\˜[Y]\—Ý\™Ù]
	›X\[™Ë\™Ù]\™Ù]
JNÂŸB‚™›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™×Ù›Ü—Ý\™Ù]	ØOŠˆX\[™ÜÎˆ	‰ØHÓZYUœÝÔ\˜[Y]\“X\[™×Kˆ\™Ù]ˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHOˆÜ[Û	‰ØHZYUœÝÔ\˜[Y]\“X\[™ÏˆÂˆX\[™ÜÂˆš]\Š
Bˆ™š[™
X\[™ßØ[YWÛZYWÝœÝ×Ü\˜[Y]\—Ý\™Ù]
	›X\[™Ë\™Ù]\™Ù]
JBŸB‚™›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ù›Ü—ØØÏ	ØOŠˆX\[™ÜÎˆ	‰ØHÓZYUœÝÔ\˜[Y]\“X\[™×Kˆ[œ]Ù]šXÙWÚYˆ	œÝ‹ˆÚ[›™[ˆNˆÛÛ›Û\ŽˆNŠHOˆ™XÏ	‰ØHZYUœÝÔ\˜[Y]\“X\[™ÏˆÂˆX\[™ÜÂˆš]\Š
Bˆ™š[\ŠX\[™ßÂˆX\[™Ëš[œ]Ù]šXÙWÚYOH[œ]Ù]šXÙWÚYˆ	‰ˆX\[™Ë˜Ú[›™[OHÚ[›™[ˆ	‰ˆX\[™Ë˜ÛÛ›Û\ˆOHÛÛ›Û\‚ˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊÛÛ[Îˆ	œÝŠHOˆ™XÏZYUœÝÔ\˜[Y]\“X\[™ÏˆÂˆ]]]X\[™ÜÈH™XÎŽ›™]Ê
NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]ÛÛYJ[˜ÛÙY
HH[™KœÝš\Ü™Yš^
›X\[™ÏHŠH[ÙHÂˆÛÛ[YNÂˆNÂˆ]]]šY[ÈH[˜ÛÙYœÜ]
	×	ÊNÂˆ]ÛÛYJ[œ]Ù]šXÙWÚY
HHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÚ[›™[
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÛÛ›Û\ŠHHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ›Ú™XÝØÚ[›™[ÚY
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽLMŠ
K›ÚÊ
JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJYÚ[—ÝZY
HHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ\˜[Y]\—ÚY
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽLÌŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ\˜[Y]\—Û˜[YJHHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆYˆšY[Ë›™^

Kš\×ÜÛÛYJ
HÂˆÛÛ[YNÂˆBˆ\Ù\ÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Êˆ	›]]X\[™ÜËˆZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆÚ[›™[ˆÛÛ›Û\‹ˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆYÚ[—ÝZYˆ\˜[Y]\—ÚYˆ\˜[Y]\—Û˜[YKˆKˆKˆ
NÂˆBˆX\[™ÜÂŸB‚™›ˆÙ\šX[^™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊX\[™ÜÎˆ	–ÓZYUœÝÔ\˜[Y]\“X\[™×JHOˆÝš[™ÈÂˆ]]]ÛÛ[ÈHÝš[™ÎŽ™œ›ÛJ™\œÚ[ÛLWˆŠNÂˆ]]]›Ü›X[^™YH™XÎŽ›™]Ê
NÂˆ›ÜˆX\[™È[ˆX\[™ÜÈÂˆ\Ù\ÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ê	›]]›Ü›X[^™YX\[™Ë˜ÛÛ™J
JNÂˆBˆ›ÜˆX\[™È[ˆ›Ü›X[^™YÂˆÛÛ[Ëœ\ÚÜÝŠ›X\[™ÏHŠNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ëš[œ]Ù]šXÙWÚY
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë˜Ú[›™[×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë˜ÛÛ›Û\‹×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë\™Ù]œ›Ú™XÝØÚ[›™[ÚY×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ë\™Ù]œYÚ[—ÝZY
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë\™Ù]œ\˜[Y]\—ÚY×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ë\™Ù]œ\˜[Y]\—Û˜[YJJNÂˆÛÛ[Ëœ\Ú
	×‰ÊNÂˆBˆÛÛ[ÂŸB‚™›ˆ˜[YÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊ[šÎˆ	”]\›•œÝÔ\˜[Y]\“[šÊHOˆ›ÛÛÂˆ[[šËœ›Ú™XÝÜ]š\×Ù[\J
Bˆ	‰ˆ[[šË\™Ù]œYÚ[—ÝZYš\×Ù[\J
Bˆ	‰ˆ[[šË\™Ù]œ\˜[Y]\—Û˜[YKš\×Ù[\J
BŸB‚™›ˆØ[YWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ý\™Ù]
ˆ[šÎˆ	”]\›•œÝÔ\˜[Y]\“[šËˆ›Ú™XÝÜ]ˆ	”]ˆ]\›—ÚYˆLM‹ˆ\™Ù]ˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHOˆ›ÛÛÂˆ›Ú™XÝÜ]×Ù\]X[
]Ž›™]Ê	›[šËœ›Ú™XÝÜ]
K›Ú™XÝÜ]
Bˆ	‰ˆ[šËœ]\›—ÚYOH]\›—ÚYˆ	‰ˆØ[YWÛZYWÝœÝ×Ü\˜[Y]\—Ý\™Ù]
	›[šË\™Ù]\™Ù]
BŸB‚™›ˆ\Ù\Ü]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊˆ[šÜÎˆ	›]]™XÏ]\›•œÝÔ\˜[Y]\“[šÏ‹ˆ[šÎˆ]\›•œÝÔ\˜[Y]\“[šËŠHÂˆYˆ]˜[YÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊ	›[šÊHÂˆ™]\›ŽÂˆBˆ[šÜËœ™]Z[Š^\Ý[™ßÂˆ\Ø[YWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ý\™Ù]
ˆ^\Ý[™Ëˆ]Ž›™]Ê	›[šËœ›Ú™XÝÜ]
Kˆ[šËœ]\›—ÚYˆ	›[šË\™Ù]ˆ
BˆJNÂˆ[šÜËœ\Ú
[šÊNÂŸB‚™›ˆ™[[Ý™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊˆ[šÜÎˆ	›]]™XÏ]\›•œÝÔ\˜[Y]\“[šÏ‹ˆ›Ú™XÝÜ]ˆ	”]ˆ]\›—ÚYˆLM‹ˆ\™Ù]ˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHÂˆ[šÜËœ™]Z[Š[šßÂˆ\Ø[YWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ý\™Ù]
[šË›Ú™XÝÜ]]\›—ÚY\™Ù]
BˆJNÂŸB‚™›ˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ù›Ü—Ý\™Ù]	ØOŠˆ[šÜÎˆ	‰ØHÔ]\›•œÝÔ\˜[Y]\“[š×Kˆ›Ú™XÝÜ]ˆ	”]ˆ]\›—ÚYˆLM‹ˆ\™Ù]ˆ	“ZYUœÝÔ\˜[Y]\•\™Ù]ŠHOˆÜ[Û	‰ØH]\›•œÝÔ\˜[Y]\“[šÏˆÂˆ[šÜËš]\Š
K™š[™
[šßÂˆØ[YWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ý\™Ù]
[šË›Ú™XÝÜ]]\›—ÚY\™Ù]
BˆJBŸB‚™›ˆ]\›—ÝœÝ×Ü\˜[Y]\—Ø]]ÛX][Û—Ù›Ü—ØÚ[›™[
ˆ[šÜÎˆ	–Ô]\›•œÝÔ\˜[Y]\“[š×Kˆ›Ú™XÝÜ]ˆÜ[Û	”]‹ˆ]\›—ÚYˆLM‹ˆÛÛ›Û\œÎˆ	–Ô]\›ÛÛ›Û\—Kˆ\™Ù]ØÚ[›™[ÚYˆLM‹ˆYÚ[—ÝZYˆ	œÝ‹ˆNˆLM‹ŠHOˆ™XÏœÝÔ\˜[Y]\]]ÛX][ÛˆÂˆ]ÛÛYJ›Ú™XÝÜ]
HH›Ú™XÝÜ][ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆYˆHOHYÚ[—ÝZYš\×Ù[\J
HÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ[šÜÂˆš]\Š
Bˆ™š[\Š[šßÂˆ›Ú™XÝÜ]×Ù\]X[
]Ž›™]Ê	›[šËœ›Ú™XÝÜ]
K›Ú™XÝÜ]
Bˆ	‰ˆ[šËœ]\›—ÚYOH]\›—ÚYˆ	‰ˆ[šË\™Ù]œ›Ú™XÝØÚ[›™[ÚYOH\™Ù]ØÚ[›™[ÚYˆ	‰ˆ[šË\™Ù]œYÚ[—ÝZYOHYÚ[—ÝZYˆJBˆ™š[\—ÛX\
[šßÂˆ]Ú[ÈHÛÛ›Û\œÂˆš]\Š
Bˆ™š[\ŠÚ[ÂˆÚ[˜Ú[›™[OH[šËœÝ™X[WØÚ[›™[	‰ˆÚ[™›YÜÈOH[šËœÝ™X[WÙ›YÜÂˆJBˆ™š[\—ÛX\
Ú[Âˆ]˜[YHHÚ[˜[YJ
NÂˆ
˜[YKš\×Ùš[š]J
H	‰ˆ
Œ‹LKŒ
K˜ÛÛZ[œÊ	˜[YJJK[—ÜÛÛYJˆœÝÐ]]ÛX][Û”Ú[ÂˆÜÚ][Û—Ø™X]ÎˆŽ™œ›ÛJÚ[œÜÚ][ÛŠHÈŽ™œ›ÛJJKˆ˜[YNˆŽ™œ›ÛJ˜[YJKˆKˆ
BˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ
\Ú[Ëš\×Ù[\J
JK[—ÜÛÛYJœÝÔ\˜[Y]\]]ÛX][ÛˆÂˆ\˜[Y]\—ÚYˆ[šË\™Ù]œ\˜[Y]\—ÚYˆÚ[ËˆJBˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ\œÙWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊÛÛ[Îˆ	œÝŠHOˆ™XÏ]\›•œÝÔ\˜[Y]\“[šÏˆÂˆ]]][šÜÈH™XÎŽ›™]Ê
NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]ÛÛYJ[˜ÛÙY
HH[™KœÝš\Ü™Yš^
›[šÏHŠH[ÙHÂˆÛÛ[YNÂˆNÂˆ]]]šY[ÈH[˜ÛÙYœÜ]
	×	ÊNÂˆ]ÛÛYJ›Ú™XÝÜ]
HHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ]\›—ÚY
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽLMŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÝ™X[WØÚ[›™[
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÝ™X[WÙ›YÜÊHHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ›Ú™XÝØÚ[›™[ÚY
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽLMŠ
K›ÚÊ
JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJYÚ[—ÝZY
HHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ\˜[Y]\—ÚY
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽLÌŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ\˜[Y]\—Û˜[YJHHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆYˆšY[Ë›™^

Kš\×ÜÛÛYJ
HÂˆÛÛ[YNÂˆBˆ\Ù\Ü]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊˆ	›]][šÜËˆ]\›•œÝÔ\˜[Y]\“[šÈÂˆ›Ú™XÝÜ]ˆ]\›—ÚYˆÝ™X[WØÚ[›™[ˆÝ™X[WÙ›YÜËˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆYÚ[—ÝZYˆ\˜[Y]\—ÚYˆ\˜[Y]\—Û˜[YKˆKˆKˆ
NÂˆBˆ[šÜÂŸB‚™›ˆÙ\šX[^™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ[šÜÎˆ	–Ô]\›•œÝÔ\˜[Y]\“[š×JHOˆÝš[™ÈÂˆ]]]ÛÛ[ÈHÝš[™ÎŽ™œ›ÛJ™\œÚ[ÛLWˆŠNÂˆ]]]›Ü›X[^™YH™XÎŽ›™]Ê
NÂˆ›Üˆ[šÈ[ˆ[šÜÈÂˆ\Ù\Ü]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊ	›]]›Ü›X[^™Y[šË˜ÛÛ™J
JNÂˆBˆ›Üˆ[šÈ[ˆ›Ü›X[^™YÂˆÛÛ[Ëœ\ÚÜÝŠ›[šÏHŠNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›[šËœ›Ú™XÝÜ]
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›[šËœ]\›—ÚY×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›[šËœÝ™X[WØÚ[›™[×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›[šËœÝ™X[WÙ›YÜË×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›[šË\™Ù]œ›Ú™XÝØÚ[›™[ÚY×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›[šË\™Ù]œYÚ[—ÝZY
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›[šË\™Ù]œ\˜[Y]\—ÚY×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›[šË\™Ù]œ\˜[Y]\—Û˜[YJJNÂˆÛÛ[Ëœ\Ú
	×‰ÊNÂˆBˆÛÛ[ÂŸB‚™›ˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊÛÛ[Îˆ	œÝŠHOˆ™XÏZYSZ^\ÛÛ›ÛX\[™ÏˆÂˆ]]]X\[™ÜÈH™XÎŽ›™]Ê
NÂˆ]]]›Ý[™Ý™\œÚ[Û™YÛX\[™ÈH˜[ÙNÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]ÛÛYJ[˜ÛÙY
HH[™KœÝš\Ü™Yš^
›X\[™ÏHŠH[ÙHÂˆÛÛ[YNÂˆNÂˆ›Ý[™Ý™\œÚ[Û™YÛX\[™ÈHYNÂˆ]]]šY[ÈH[˜ÛÙYœÜ]
	×	ÊNÂˆ]ÛÛYJ[œ]Ù]šXÙWÚY
HHšY[Ë›™^

K˜[™Ý[ŠXÛÙWÛZYWÙ]šXÙWÚY
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÚ[›™[
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÛÛ›Û\ŠHHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽNŠ
K›ÚÊ
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ[œÙ\ÛÜ™[˜[
HHšY[Ë›™^

K˜[™Ý[ŠšY[šY[œ\œÙNŽ\Ú^™OŠ
K›ÚÊ
JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆ]\™Ù]HX]ÚšY[Ë›™^

HÂˆÛÛYJ˜[YJHOˆX]ÚZYSZ^\ÛÛ›Û\™Ù]Ž™œ›ÛWÜÙ][™Ê˜[YJHÂˆÛÛYJ\™Ù]
HOˆ\™Ù]ˆ›Û™HOˆÛÛ[YKˆKˆ›Û™HOˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆYˆšY[Ë›™^

Kš\×ÜÛÛYJ
HÂˆÛÛ[YNÂˆBˆ]X\[™ÈHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆÚ[›™[ˆÛÛ›Û\‹ˆ[œÙ\ÛÜ™[˜[ˆ\™Ù]ˆNÂˆYˆ˜[YÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›X\[™ÊHÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜËX\[™ÊNÂˆBˆBˆYˆ›Ý[™Ý™\œÚ[Û™YÛX\[™ÈÂˆX\[™ÜÂˆH[ÙHÂˆ\œÙWÛYØXÞWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÊÛÛ[ÊBˆš[×Ú]\Š
Bˆ˜ÛÛXÝ

BˆBŸB‚™›ˆ\œÙWÛYØXÞWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÊÛÛ[Îˆ	œÝŠHOˆÜ[ÛZYSZ^\ÛÛ›ÛX\[™ÏˆÂˆ]]][œ]Ù]šXÙWÚYH›Û™NÂˆ]]]Ú[›™[H›Û™NÂˆ]]]ÛÛ›Û\ˆH›Û™NÂˆ]]][œÙ\ÛÜ™[˜[H›Û™NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆYˆ]ÛÛYJ[œ]
HH[™KœÝš\Ü™Yš^
š[œ]HŠHÂˆ[œ]Ù]šXÙWÚYHXÛÙWÛZYWÙ]šXÙWÚY
[œ]
NÂˆH[ÙHYˆ]ÛÛYJ˜[YJHH[™KœÝš\Ü™Yš^
˜Ú[›™[HŠHÂˆÚ[›™[H˜[YKœ\œÙNŽNŠ
K›ÚÊ
NÂˆH[ÙHYˆ]ÛÛYJ˜[YJHH[™KœÝš\Ü™Yš^
˜ÛÛ›Û\HŠHÂˆÛÛ›Û\ˆH˜[YKœ\œÙNŽNŠ
K›ÚÊ
NÂˆH[ÙHYˆ]ÛÛYJ˜[YJHH[™KœÝš\Ü™Yš^
š[œÙ\HŠHÂˆ[œÙ\ÛÜ™[˜[H˜[YKœ\œÙNŽ\Ú^™OŠ
K›ÚÊ
NÂˆBˆBˆ][œ]Ù]šXÙWÚYH[œ]Ù]šXÙWÚYÎÂˆ]Ú[›™[HÚ[›™[ÎÂˆ]ÛÛ›Û\ˆHÛÛ›Û\ÎÂˆ][œÙ\ÛÜ™[˜[H[œÙ\ÛÜ™[˜[ÎÂˆ]X\[™ÈHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆÚ[›™[ˆÛÛ›Û\‹ˆ[œÙ\ÛÜ™[˜[ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ˜[YÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›X\[™ÊK[—ÜÛÛYJX\[™ÊBŸB‚™›ˆ˜[YÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÊX\[™Îˆ	“ZYSZ^\ÛÛ›ÛX\[™ÊHOˆ›ÛÛÂˆX\[™Ë˜Ú[›™[M‚ˆ	‰ˆX\[™Ë˜ÛÛ›Û\ˆLŽˆ	‰ˆX\[™Ëš[œÙ\ÛÜ™[˜[RVT—ÕPÒ×ÓQUT—ÐÓÕS•ˆ	‰ˆ
X\[™Ë\™Ù]OHZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛÈX\[™Ëš[œÙ\ÛÜ™[˜[ˆ
BŸB‚™›ˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÊˆX\[™ÜÎˆ	›]]™XÏZYSZ^\ÛÛ›ÛX\[™Ï‹ˆX\[™ÎˆZYSZ^\ÛÛ›ÛX\[™ËŠHÂˆX\[™ÜËœ™]Z[Š^\Ý[™ßÂˆJ^\Ý[™Ëš[œÙ\ÛÜ™[˜[OHX\[™Ëš[œÙ\ÛÜ™[˜[	‰ˆ^\Ý[™Ë\™Ù]OHX\[™Ë\™Ù]
Bˆ	‰ˆJ^\Ý[™Ëš[œ]Ù]šXÙWÚYOHX\[™Ëš[œ]Ù]šXÙWÚYˆ	‰ˆ^\Ý[™Ë˜Ú[›™[OHX\[™Ë˜Ú[›™[ˆ	‰ˆ^\Ý[™Ë˜ÛÛ›Û\ˆOHX\[™Ë˜ÛÛ›Û\ŠBˆJNÂˆX\[™ÜËœ\Ú
X\[™ÊNÂŸB‚™›ˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊˆX\[™ÜÎˆ	–ÓZYSZ^\ÛÛ›ÛX\[™×Kˆ[œ]Ù]šXÙWÚYˆ	œÝ‹ˆÚ[›™[ˆNˆÛÛ›Û\ŽˆNŠHOˆÜ[Û
\Ú^™KZYSZ^\ÛÛ›Û\™Ù]
OˆÂˆX\[™ÜÂˆš]\Š
Bˆ™š[™
X\[™ßÂˆX\[™Ëš[œ]Ù]šXÙWÚYOH[œ]Ù]šXÙWÚYˆ	‰ˆX\[™Ë˜Ú[›™[OHÚ[›™[ˆ	‰ˆX\[™Ë˜ÛÛ›Û\ˆOHÛÛ›Û\‚ˆJBˆ›X\
X\[™ß
X\[™Ëš[œÙ\ÛÜ™[˜[X\[™Ë\™Ù]
JBŸB‚™›ˆÙ\šX[^™WÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊX\[™ÜÎˆ	–ÓZYSZ^\ÛÛ›ÛX\[™×JHOˆÝš[™ÈÂˆ]]]ÛÛ[ÈHÝš[™ÎŽ™œ›ÛJ™\œÚ[ÛL×ˆŠNÂˆ]]]›Ü›X[^™YH™XÎŽ›™]Ê
NÂˆ›ÜˆX\[™È[ˆX\[™ÜÂˆš]\Š
Bˆ™š[\ŠX\[™ß˜[YÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÊX\[™ÊJBˆÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]›Ü›X[^™YX\[™Ë˜ÛÛ™J
JNÂˆBˆ›ÜˆX\[™È[ˆ›Ü›X[^™YÂˆÛÛ[Ëœ\ÚÜÝŠ›X\[™ÏHŠNÂˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ëš[œ]Ù]šXÙWÚY
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë˜Ú[›™[×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ë˜ÛÛ›Û\‹×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	›X\[™Ëš[œÙ\ÛÜ™[˜[×ÜÝš[™Ê
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠX\[™Ë\™Ù]œÙ][™Ê
JNÂˆÛÛ[Ëœ\Ú
	×‰ÊNÂˆBˆÛÛ[ÂŸB‚™›ˆÙ\šX[^™WÛZYWØÛÛ›Û\—Ý[\]\Ê[\]\Îˆ	–ÓZYPÛÛ›Û\•[\]WJHOˆÝš[™ÈÂˆ]]]ÛÛ[ÈHÝš[™ÎŽ›™]Ê
NÂˆ›Üˆ[\]H[ˆ[\]\ÈÂˆYˆ[\]K˜Z[Ú[ˆÂˆÛÛ[YNÂˆBˆÛÛ[Ëœ\ÚÜÝŠ	™[˜ÛÙWÛZYWÙ]šXÙWÚY
	[\]K›˜[YJJNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	œÙ\šX[^™WÛZYWØÛÛ›Û\—Øš[™[™Êˆ[\]K›Ý]]ÙØZ[‹˜\×Ü™YŠ
Kˆ
JNÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆÛÛ[Ëœ\ÚÜÝŠ	œÙ\šX[^™WÛZYWØÛÛ›Û\—Øš[™[™Êˆ[\]Kœ^WÜ]\ÙK˜\×Ü™YŠ
Kˆ
JNÂˆÛÛ[Ëœ\Ú
	×‰ÊNÂˆBˆÛÛ[ÂŸB‚™›ˆÙ\šX[^™WÛZYWØÛÛ›Û\—Øš[™[™Êš[™[™ÎˆÜ[Û	“ZYPÛÛ›Û\ØÐš[™[™ÏŠHOˆÝš[™ÈÂˆš[™[™Ë›X\ÛÜ—Ù[ÙJˆ‹H‹×ÛÝÛ™Y

Kˆš[™[™ß›Ü›X]JžßNžßH‹š[™[™Ë˜Ú[›™[š[™[™Ë˜ÛÛ›Û\ŠKˆ
BŸB‚™›ˆ\œÙWÛZYWØÛÛ›Û\—Ý[\]\ÊÛÛ[Îˆ	œÝŠHOˆ™XÏZYPÛÛ›Û\•[\]OˆÂˆ]]][\]\ÈH™XÎŽ›™]Ê
NÂˆ]]]˜[Y\ÈH•™YTÙ]Ž›™]Ê
NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]]]šY[ÈH[™KœÜ]
	×	ÊNÂˆ]
ÛÛYJ[˜ÛÙYÛ˜[YJKÛÛYJÝ]]ÙØZ[ŠKÛÛYJ^WÜ]\ÙJK›Û™JHBˆ
šY[Ë›™^

KšY[Ë›™^

KšY[Ë›™^

KšY[Ë›™^

JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJ˜[YJHHXÛÙWÛZYWÙ]šXÙWÚY
[˜ÛÙYÛ˜[YJH[ÙHÂˆÛÛ[YNÂˆNÂˆ]˜[YHH˜[YKš[J
NÂˆ]˜[YWÚÙ^HH˜[YK×ÛÝÙ\˜Ø\ÙJ
NÂˆYˆ˜[YKš\×Ù[\J
H˜[YK˜Ú\œÊ
K˜ÛÝ[

HˆÂˆÛÛ[YNÂˆBˆ]Ý]]ÙØZ[ˆH\œÙWÛZYWØÛÛ›Û\—Øš[™[™ÊÝ]]ÙØZ[ŠNÂˆ]^WÜ]\ÙHH\œÙWÛZYWØÛÛ›Û\—Øš[™[™Ê^WÜ]\ÙJNÂˆYˆ
Ý]]ÙØZ[‹š\×Û›Û™J
H	‰ˆ^WÜ]\ÙKš\×Û›Û™J
JH[˜[Y\Ëš[œÙ\
˜[YWÚÙ^JHÂˆÛÛ[YNÂˆBˆ[\]\Ëœ\Ú
ZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ˜[YK×ÛÝÛ™Y

KˆÝ]]ÙØZ[‹ˆ^WÜ]\ÙKˆZ[Ú[Žˆ˜[ÙKˆJNÂˆBˆ[\]\ÂŸB‚™›ˆ\œÙWÛZYWØÛÛ›Û\—Øš[™[™Ê˜[YNˆ	œÝŠHOˆÜ[ÛZYPÛÛ›Û\ØÐš[™[™ÏˆÂˆYˆ˜[YHOH‹HˆÂˆ™]\›ˆ›Û™NÂˆBˆ]
Ú[›™[ÛÛ›Û\ŠHH˜[YKœÜ]ÛÛ˜ÙJ	Î‰ÊOÎÂˆYˆÛÛ›Û\‹˜ÛÛZ[œÊ	Î‰ÊHÂˆ™]\›ˆ›Û™NÂˆBˆ]Ú[›™[HÚ[›™[œ\œÙNŽNŠ
K›ÚÊ
OÎÂˆ]ÛÛ›Û\ˆHÛÛ›Û\‹œ\œÙNŽNŠ
K›ÚÊ
OÎÂˆ
Ú[›™[Mˆ	‰ˆÛÛ›Û\ˆLŽ
K[—ÜÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆÛÛ›Û\‹ˆJBŸB‚™›ˆZYWØÛÛ›Û\—Øš[™[™×Ù›Ü—Ù]šXÙJˆX\[™ÎˆÜ[Û	“ZYPÛÛ›ÛÚ[™ÙSX\[™Ï‹ˆ]šXÙWÚYˆ	œÝ‹ŠHOˆÜ[ÛZYPÛÛ›Û\ØÐš[™[™ÏˆÂˆX\[™Âˆ™š[\ŠX\[™ßX\[™Ëš[œ]Ù]šXÙWÚYOH]šXÙWÚY
Bˆ›X\
X\[™ßZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆX\[™Ë˜Ú[›™[ˆÛÛ›Û\ŽˆX\[™Ë˜ÛÛ›Û\‹ˆJBŸB‚™›ˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]Êˆ]Y[×Ùš[\—Ù[˜X›Yˆ›ÛÛˆ›Ý\×Ùš[\—Ù[˜X›Yˆ›ÛÛˆ]]ÛX][Û—Ùš[\—Ù[˜X›Yˆ›ÛÛˆ]Y[×Ø]˜Z[X›Nˆ›ÛÛˆZYWÚ[œ]Ø]˜Z[X›Nˆ›ÛÛŠHOˆ
›ÛÛ›ÛÛ›ÛÛ
HÂˆ
ˆ]Y[×Ùš[\—Ù[˜X›Y	‰ˆ]Y[×Ø]˜Z[X›Kˆ›Ý\×Ùš[\—Ù[˜X›Y	‰ˆZYWÚ[œ]Ø]˜Z[X›Kˆ]]ÛX][Û—Ùš[\—Ù[˜X›Y	‰ˆZYWÚ[œ]Ø]˜Z[X›Kˆ
BŸB‚ˆÖÙ\š]™JXYË\K\X[\JWBœÝXÝZYQ]šXÙTÙ[XÝ[ÛœÈÂˆ[œ]ÚYˆÜ[ÛÝš[™Ï‹ˆÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\ˆ›ÛÛˆÝ]]ÚYˆÜ[ÛÝš[™Ï‹ˆÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\ˆ›ÛÛˆÝ]]ØÚ[›™[Ü›Ý]\ÎˆÓÜ[ÛÝš[™ÏŽÈM—Kˆ˜[š×Û\ØŽˆÓÜ[ÛNŽÈM—Kˆ˜[š×ÛØŽˆÓÜ[ÛNŽÈM—Kˆ›ÙÜ˜[\ÎˆÓÜ[ÛNŽÈM—KˆÙ[™ØÛØÚÎˆ›ÛÛˆÙ[™Ü^[\ÝÛ›Ý\Îˆ›ÛÛˆÙ[™Ü]\›—Û›Ý\Îˆ›ÛÛˆš^YÝ™[ØÚ]Nˆ›ÛÛˆ\™›Ü›X[˜ÙWØÚ[›™[ˆNˆ™XÛÜ™Ø]Y[Îˆ›ÛÛˆ™XÛÜ™Û›Ý\Îˆ›ÛÛˆ™XÛÜ™Ø]]ÛX][ÛŽˆ›ÛÛˆ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\ŽˆNˆ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[ˆNˆ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÎˆNŸB‚š[\Y˜][›ÜˆZYQ]šXÙTÙ[XÝ[ÛœÈÂˆ›ˆY˜][

HOˆÙ[ˆÂˆÙ[ˆÂˆ[œ]ÚYˆ›Û™KˆÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\ˆ˜[ÙKˆÝ]]ÚYˆ›Û™KˆÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\ˆ˜[ÙKˆÝ]]ØÚ[›™[Ü›Ý]\ÎˆÝŽ˜\œ˜^NŽ™œ›ÛWÙ›Šß›Û™JKˆ˜[š×Û\ØŽˆÓ›Û™NÈM—Kˆ˜[š×ÛØŽˆÓ›Û™NÈM—Kˆ›ÙÜ˜[\ÎˆÓ›Û™NÈM—KˆÙ[™ØÛØÚÎˆ˜[ÙKˆÙ[™Ü^[\ÝÛ›Ý\Îˆ˜[ÙKˆÙ[™Ü]\›—Û›Ý\Îˆ˜[ÙKˆš^YÝ™[ØÚ]Nˆ˜[ÙKˆ\™›Ü›X[˜ÙWØÚ[›™[ˆˆ™XÛÜ™Ø]Y[ÎˆYKˆ™XÛÜ™Û›Ý\ÎˆYKˆ™XÛÜ™Ø]]ÛX][ÛŽˆ˜[ÙKˆ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\ŽˆKˆ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[ˆˆ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÎˆˆBˆBŸB‚™›ˆØYÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊ
HOˆZYQ]šXÙTÙ[XÝ[ÛœÈÂˆ]ÛÛYJ]
HHZYWÙ]šXÙWÜÙ[XÝ[Ûœ×Ùš[J
H[ÙHÂˆ™]\›ˆZYQ]šXÙTÙ[XÝ[ÛœÎŽ™Y˜][

NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆZYQ]šXÙTÙ[XÝ[ÛœÎŽ™Y˜][

NÂˆNÂˆ\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊ	˜ÛÛ[ÊBŸB‚™›ˆØ]™WÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊÙ[XÝ[ÛœÎˆ	“ZYQ]šXÙTÙ[XÝ[ÛœÊHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HZYWÙ]šXÙWÜÙ[XÝ[Ûœ×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQH]šXÙHÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ][˜ÛÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊÙ[XÝ[ÛœÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYÛZYWÝWÜ™Y™\™[˜Ù\Ê
HOˆ
›ÛÛÜ[ÛNŠHÂˆZYWÝWÜ™Y™\™[˜Ù\×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›X\
ÛÛ[ß\œÙWÛZYWÝWÜ™Y™\™[˜Ù\Ê	˜ÛÛ[ÊJBˆ[Ü˜\ÛÜŠ
˜[ÙK›Û™JJBŸB‚™›ˆØ]™WÛZYWÝWÜ™Y™\™[˜Ù\ÊˆÛ˜\Ý×ÜØØ[Nˆ›ÛÛˆÝ]]ØÚ[›™[ˆÜ[ÛN‹ŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆÝ]]ØÚ[›™[š\×ÜÛÛYWØ[™
Ú[›™[Ú[›™[ˆMJHÂˆ™]\›ˆ\œŠ“RQHHÝ]]Ú[›™[]\Ý™H™]ÙY[ˆ[™MH‹×ÛÝÛ™Y

JNÂˆBˆ]]HZYWÝWÜ™Y™\™[˜Ù\×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQHH™Y™\™[˜Ù\È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]Ý]]ØÚ[›™[BˆÝ]]ØÚ[›™[›X\ÛÜ—Ù[ÙJœ™\Ù\™H‹×ÛÝÛ™Y

K˜[Y_˜[YK×ÜÝš[™Ê
JNÂˆœÎŽÜš]Jˆ	œ]ˆ›Ü›X]JˆœÛ˜\Ý×ÜØØ[O^ßW›Ý]]ØÚ[›™[^ÛÝ]]ØÚ[›™[Wˆ‹ˆNŽ™œ›ÛJÛ˜\Ý×ÜØØ[JBˆ
Kˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ\œÙWÛZYWÝWÜ™Y™\™[˜Ù\ÊÛÛ[Îˆ	œÝŠHOˆ
›ÛÛÜ[ÛNŠHÂˆ]]]Û˜\Ý×ÜØØ[HH˜[ÙNÂˆ]]]Ý]]ØÚ[›™[H›Û™NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œÛ˜\Ý×ÜØØ[OHŠHÂˆÛ˜\Ý×ÜØØ[HH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJÚ[›™[
HH[™KœÝš\Ü™Yš^
›Ý]]ØÚ[›™[HŠHÂˆÝ]]ØÚ[›™[HX]ÚÚ[›™[Âˆœ™\Ù\™HˆOˆ›Û™KˆÈOˆÚ[›™[œ\œÙNŽNŠ
K›ÚÊ
K™š[\ŠÚ[›™[
˜Ú[›™[HMJKˆNÂˆBˆBˆ
Û˜\Ý×ÜØØ[KÝ]]ØÚ[›™[
BŸB‚™›ˆ[˜ÛÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊÙ[XÝ[ÛœÎˆ	“ZYQ]šXÙTÙ[XÝ[ÛœÊHOˆÝš[™ÈÂˆ›Ü›X]Jˆš[œ]^ßW˜ÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\^ßW›Ý]]^ßW˜ÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\^ßW›Ý]]Ü›Ý]\Ï^ßW˜˜[š×Û\Ø^ßW˜˜[š×ÛØ^ßWœ›ÙÜ˜[\Ï^ßW˜ÛØÚÏ^ßWœ^[\ÝÛ›Ý\Ï^ßWœ]\›—Û›Ý\Ï^ßW™š^YÝ™[ØÚ]O^ßWœ\™›Ü›X[˜ÙWØÚ[›™[^ßWœ™XÛÜ™Ø]Y[Ï^ßWœ™XÛÜ™Û›Ý\Ï^ßWœ™XÛÜ™Ø]]ÛX][Û^ßWœ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\^ßWœ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[^ßWœ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÏ^ßWˆ‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ù[XÝ[ÛœËš[œ]ÚY˜\×Ù\™YŠ
K[Ü˜\ÛÜ—ÙY˜][

JKˆNŽ™œ›ÛJÙ[XÝ[ÛœË˜ÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\
Kˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ù[XÝ[ÛœË›Ý]]ÚY˜\×Ù\™YŠ
K[Ü˜\ÛÜ—ÙY˜][

JKˆNŽ™œ›ÛJÙ[XÝ[ÛœË˜ÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\
Kˆ[˜ÛÙWÛZYWÛÝ]]Ü›Ý]\Ê	œÙ[XÝ[ÛœË›Ý]]ØÚ[›™[Ü›Ý]\ÊKˆ[˜ÛÙWÛZYWÛÝ]]Ý˜[Y\Ê	œÙ[XÝ[ÛœË˜˜[š×Û\ØŠKˆ[˜ÛÙWÛZYWÛÝ]]Ý˜[Y\Ê	œÙ[XÝ[ÛœË˜˜[š×ÛØŠKˆ[˜ÛÙWÛZYWÛÝ]]Ü›ÙÜ˜[\Ê	œÙ[XÝ[ÛœËœ›ÙÜ˜[\ÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœÙ[™ØÛØÚÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœÙ[™Ü^[\ÝÛ›Ý\ÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœÙ[™Ü]\›—Û›Ý\ÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœË™š^YÝ™[ØÚ]JKˆÙ[XÝ[ÛœËœ\™›Ü›X[˜ÙWØÚ[›™[›Z[ŠMJKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœ™XÛÜ™Ø]Y[ÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœ™XÛÜ™Û›Ý\ÊKˆNŽ™œ›ÛJÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][ÛŠKˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\‹›Z[ŠLÊKˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[ˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÂˆ
BŸB‚™›ˆ\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊÛÛ[Îˆ	œÝŠHOˆZYQ]šXÙTÙ[XÝ[ÛœÈÂˆ]]]Ù[XÝ[ÛœÈHZYQ]šXÙTÙ[XÝ[ÛœÎŽ™Y˜][

NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆYˆ]ÛÛYJ[˜ÛÙYÚY
HH[™KœÝš\Ü™Yš^
š[œ]HŠHÂˆÙ[XÝ[ÛœËš[œ]ÚYHXÛÙWÛZYWÙ]šXÙWÚY
[˜ÛÙYÚY
NÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
˜ÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\HŠHÂˆÙ[XÝ[ÛœË˜ÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\H[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜ÛÙYÚY
HH[™KœÝš\Ü™Yš^
›Ý]]HŠHÂˆÙ[XÝ[ÛœË›Ý]]ÚYHXÛÙWÛZYWÙ]šXÙWÚY
[˜ÛÙYÚY
NÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
˜ÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\HŠHÂˆÙ[XÝ[ÛœË˜ÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\H[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ›Ý]\ÊHH[™KœÝš\Ü™Yš^
›Ý]]Ü›Ý]\ÏHŠHÂˆYˆ]ÛÛYJ›Ý]\ÊHH\œÙWÛZYWÛÝ]]Ü›Ý]\Ê›Ý]\ÊHÂˆÙ[XÝ[ÛœË›Ý]]ØÚ[›™[Ü›Ý]\ÈH›Ý]\ÎÂˆBˆH[ÙHYˆ]ÛÛYJ˜[š×Û\ØŠHH[™KœÝš\Ü™Yš^
˜˜[š×Û\ØHŠHÂˆYˆ]ÛÛYJ˜[š×Û\ØŠHH\œÙWÛZYWÛÝ]]Ý˜[Y\Ê˜[š×Û\ØŠHÂˆÙ[XÝ[ÛœË˜˜[š×Û\ØˆH˜[š×Û\ØŽÂˆBˆH[ÙHYˆ]ÛÛYJ˜[š×ÛØŠHH[™KœÝš\Ü™Yš^
˜˜[š×ÛØHŠHÂˆYˆ]ÛÛYJ˜[š×ÛØŠHH\œÙWÛZYWÛÝ]]Ý˜[Y\Ê˜[š×ÛØŠHÂˆÙ[XÝ[ÛœË˜˜[š×ÛØˆH˜[š×ÛØŽÂˆBˆH[ÙHYˆ]ÛÛYJ›ÙÜ˜[\ÊHH[™KœÝš\Ü™Yš^
œ›ÙÜ˜[\ÏHŠHÂˆYˆ]ÛÛYJ›ÙÜ˜[\ÊHH\œÙWÛZYWÛÝ]]Ü›ÙÜ˜[\Ê›ÙÜ˜[\ÊHÂˆÙ[XÝ[ÛœËœ›ÙÜ˜[\ÈH›ÙÜ˜[\ÎÂˆBˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
˜ÛØÚÏHŠHÂˆÙ[XÝ[ÛœËœÙ[™ØÛØÚÈH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œ^[\ÝÛ›Ý\ÏHŠHÂˆÙ[XÝ[ÛœËœÙ[™Ü^[\ÝÛ›Ý\ÈH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œ]\›—Û›Ý\ÏHŠHÂˆÙ[XÝ[ÛœËœÙ[™Ü]\›—Û›Ý\ÈH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
™š^YÝ™[ØÚ]OHŠHÂˆÙ[XÝ[ÛœË™š^YÝ™[ØÚ]HH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJÚ[›™[
HH[™KœÝš\Ü™Yš^
œ\™›Ü›X[˜ÙWØÚ[›™[HŠHÂˆYˆ]ÚÊÚ[›™[
HHÚ[›™[œ\œÙNŽNŠ
Bˆ	‰ˆÚ[›™[HMBˆÂˆÙ[XÝ[ÛœËœ\™›Ü›X[˜ÙWØÚ[›™[HÚ[›™[ÂˆBˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œ™XÛÜ™Ø]Y[ÏHŠHÂˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]Y[ÈH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œ™XÛÜ™Û›Ý\ÏHŠHÂˆÙ[XÝ[ÛœËœ™XÛÜ™Û›Ý\ÈH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJ[˜X›Y
HH[™KœÝš\Ü™Yš^
œ™XÛÜ™Ø]]ÛX][ÛHŠHÂˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][ÛˆH[˜X›YOHŒHŽÂˆH[ÙHYˆ]ÛÛYJÛÛ›Û\ŠHH[™KœÝš\Ü™Yš^
œ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\HŠHÂˆYˆ]ÚÊÛÛ›Û\ŠHHÛÛ›Û\‹œ\œÙNŽNŠ
Bˆ	‰ˆÛÛ›Û\ˆHLÂˆÂˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\ˆHÛÛ›Û\ŽÂˆBˆH[ÙHYˆ]ÛÛYJÚ[›™[
HH[™KœÝš\Ü™Yš^
œ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[HŠHÂˆYˆ]ÚÊÚ[›™[
HHÚ[›™[œ\œÙNŽNŠ
HÂˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[HÚ[›™[ÂˆBˆH[ÙHYˆ]ÛÛYJ›YÜÊHH[™KœÝš\Ü™Yš^
œ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÏHŠBˆ	‰ˆ]ÚÊ›YÜÊHH›YÜËœ\œÙNŽNŠ
BˆÂˆÙ[XÝ[ÛœËœ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÈH›YÜÎÂˆBˆBˆÙ[XÝ[ÛœÂŸB‚™›ˆ[˜ÛÙWÛZYWÛÝ]]Ü›ÙÜ˜[\Ê›ÙÜ˜[\Îˆ	–ÓÜ[ÛNŽÈM—JHOˆÝš[™ÈÂˆ[˜ÛÙWÛZYWÛÝ]]Ý˜[Y\Ê›ÙÜ˜[\ÊBŸB‚™›ˆ[˜ÛÙWÛZYWÛÝ]]Ü›Ý]\Ê›Ý]\Îˆ	–ÓÜ[ÛÝš[™ÏŽÈM—JHOˆÝš[™ÈÂˆ›Ý]\Âˆš]\Š
Bˆ›X\
›Ý]_Âˆ›Ý]Bˆ˜\×Ù\™YŠ
Bˆ›X\
[˜ÛÙWÛZYWÙ]šXÙWÚY
Bˆ[Ü˜\ÛÜ—Ù[ÙJ‹H‹×ÛÝÛ™Y

JBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š‹ŠBŸB‚™›ˆ\œÙWÛZYWÛÝ]]Ü›Ý]\Ê[˜ÛÙYˆ	œÝŠHOˆÜ[ÛÓÜ[ÛÝš[™ÏŽÈM—OˆÂˆ]›Ý]\ÈH[˜ÛÙYˆœÜ]
	Ë	ÊBˆ›X\
›Ý]_X]Ú›Ý]HÂˆ‹HˆOˆÛÛYJ›Û™JKˆÈOˆXÛÙWÛZYWÙ]šXÙWÚY
›Ý]JK›X\
ÛÛYJKˆJBˆ˜ÛÛXÝŽÜ[Û™XÏÏŠ
OÎÂˆ›Ý]\ËžWÚ[Ê
K›ÚÊ
BŸB‚™›ˆ[˜ÛÙWÛZYWÛÝ]]Ý˜[Y\Ê˜[Y\Îˆ	–ÓÜ[ÛNŽÈM—JHOˆÝš[™ÈÂˆ˜[Y\Âˆš]\Š
Bˆ›X\
˜[Y_˜[YK›X\ÛÜ—Ù[ÙJ‹H‹×ÛÝÛ™Y

K˜[Y_˜[YK×ÜÝš[™Ê
JJBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š‹ŠBŸB‚™›ˆ\œÙWÛZYWÛÝ]]Ü›ÙÜ˜[\Ê[˜ÛÙYˆ	œÝŠHOˆÜ[ÛÓÜ[ÛNŽÈM—OˆÂˆ\œÙWÛZYWÛÝ]]Ý˜[Y\Ê[˜ÛÙY
BŸB‚™›ˆ\œÙWÛZYWÛÝ]]Ý˜[Y\Ê[˜ÛÙYˆ	œÝŠHOˆÜ[ÛÓÜ[ÛNŽÈM—OˆÂˆ]˜[Y\ÈH[˜ÛÙYˆœÜ]
	Ë	ÊBˆ›X\
šY[X]ÚšY[Âˆ‹HˆOˆÛÛYJ›Û™JKˆÈOˆšY[ˆœ\œÙNŽNŠ
Bˆ›ÚÊ
Bˆ™š[\Š˜[Y_
˜[YHHLÊBˆ›X\
ÛÛYJKˆJBˆ˜ÛÛXÝŽÜ[Û™XÏÏŠ
OÎÂˆ˜[Y\ËžWÚ[Ê
K›ÚÊ
BŸB‚™›ˆÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]
ˆÛÛ›™XÝÛÛ—ÜÝ\\ˆ›ÛÛˆÙ[XÝYÚYˆÜ[Û	œÝ‹ˆ]šXÙ\Îˆ	–ÓZYR[œ]]šXÙWKŠHOˆ›ÛÛÂˆÛÛ›™XÝÛÛ—ÜÝ\\ˆ	‰ˆÙ[XÝYÚYˆš\×ÜÛÛYWØ[™
Ù[XÝYÚY]šXÙ\Ëš]\Š
K˜[žJ]šXÙ_]šXÙKšYOHÙ[XÝYÚY
JBŸB‚™›ˆÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]
ˆÛÛ›™XÝÛÛ—ÜÝ\\ˆ›ÛÛˆÙ[XÝYÚYˆÜ[Û	œÝ‹ˆ]šXÙ\Îˆ	–ÓZYSÝ]]]šXÙWKŠHOˆ›ÛÛÂˆÛÛ›™XÝÛÛ—ÜÝ\\ˆ	‰ˆÙ[XÝYÚYˆš\×ÜÛÛYWØ[™
Ù[XÝYÚY]šXÙ\Ëš]\Š
K˜[žJ]šXÙ_]šXÙKšYOHÙ[XÝYÚY
JBŸB‚™›ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Yˆ	œÝŠHOˆÝš[™ÈÂˆÛÛœÝVˆ	–ÝNÈM—HHˆŒLŒÍMÎXX˜ÙYˆŽÂˆ]]][˜ÛÙYHÝš[™ÎŽÚ]ØØ\XÚ]JY›[Š
H
ˆŠNÂˆ›Üˆž]H[ˆY˜ž]\Ê
HÂˆ[˜ÛÙYœ\Ú
Ú\ŽŽ™œ›ÛJVÝ\Ú^™NŽ™œ›ÛJž]Hˆ
WJJNÂˆ[˜ÛÙYœ\Ú
Ú\ŽŽ™œ›ÛJVÝ\Ú^™NŽ™œ›ÛJž]H	ˆŠWJJNÂˆBˆ[˜ÛÙYŸB‚™›ˆXÛÙWÛZYWÙ]šXÙWÚY
[˜ÛÙYˆ	œÝŠHOˆÜ[ÛÝš[™ÏˆÂˆ]
Z\œË™[XZ[™\ŠHH[˜ÛÙY˜\×Øž]\Ê
K˜\×ØÚ[šÜÎŽŠ
NÂˆYˆZ\œËš\×Ù[\J
H\™[XZ[™\‹š\×Ù[\J
HÂˆ™]\›ˆ›Û™NÂˆBˆ]]]ž]\ÈH™XÎŽÚ]ØØ\XÚ]JZ\œË›[Š
JNÂˆ›ÜˆZ\ˆ[ˆZ\œÈÂˆ]YÚH^ÙYÚ]
Z\–ÌJOÎÂˆ]ÝÈH^ÙYÚ]
Z\–ÌWJOÎÂˆž]\Ëœ\Ú

YÚ
HÝÊNÂˆBˆÝš[™ÎŽ™œ›ÛWÝ]Ž
ž]\ÊK›ÚÊ
K™š[\ŠYZYš\×Ù[\J
JBŸB‚™›ˆ^ÙYÚ]
ž]NˆN
HOˆÜ[ÛNˆÂˆX]Úž]HÂˆ‰Ì	Ë‹X‰ÎIÈOˆÛÛYJž]HH‰Ì	ÊKˆ‰ØIË‹X‰Ù‰ÈOˆÛÛYJž]HH‰ØIÈ
ÈL
Kˆ‰ÐIË‹X‰Ñ‰ÈOˆÛÛYJž]HH‰ÐIÈ
ÈL
KˆÈOˆ›Û™KˆBŸB‚™›ˆØYÜ™XÙ[Ü›Ú™XÝÊ
HOˆ™XÏ]YˆÂˆ]ÛÛYJ]
HH™XÙ[Ü›Ú™XÝ×Ùš[J
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]]]™XÙ[H™XÎŽ›™]Ê
NÂˆ›Üˆ[žH[ˆÛÛ[Ë›[™\Ê
K™š[\Š[ž_Y[žKš\×Ù[\J
JHÂˆ]]H]YŽŽ™œ›ÛJ[žJNÂˆ]]HœÎŽ˜Ø[›ÛšXØ[^™J	œ]
K[Ü˜\ÛÜŠ]
NÂˆYˆ\™XÙ[˜ÛÛZ[œÊ	œ]
HÂˆ™XÙ[œ\Ú
]
NÂˆBˆYˆ™XÙ[›[Š
HH‘PÑS•Ô“Ò‘PÕÓSRUÂˆœ™XZÎÂˆBˆBˆ™XÙ[ŸB‚™›ˆØ]™WÜ™XÙ[Ü›Ú™XÝÊ›Ú™XÝÎˆ	–Ô]Y—JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H™XÙ[Ü›Ú™XÝ×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH™XÙ[›Ú™XÝÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈH›Ú™XÝÂˆš]\Š
BˆZÙJ‘PÑS•Ô“Ò‘PÕÓSRU
Bˆ›X\
]]×ÜÝš[™×ÛÜÜÞJ
JBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š—ˆŠNÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ]]ÜØ]™WÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜]]ÜØ]™K\Ù][™ÜËŠJBŸB‚™›ˆZWÜØØ[WÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[ŠZK\ØØ[KŠJBŸB‚™›ˆ]XÚYÝÚ[™Ý×ÜXÙ[Y[×ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š™]XÚY]Ú[™ÝË\XÙ[Y[ËŠJBŸB‚™›ˆÚ[™Ý×Û^[Ý]Ü™\Ù]×ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[ŠÚ[™ÝË[^[Ý]\™\Ù]ËŠJBŸB‚™›ˆØYÙ]XÚYÝÚ[™Ý×ÜXÙ[Y[Ê
HOˆ]XÚYÚ[™ÝÔXÙ[Y[ÈÂˆ]ÛÛYJ]
HH]XÚYÝÚ[™Ý×ÜXÙ[Y[×ÜÙ][™Ü×Ùš[J
H[ÙHÂˆ™]\›ˆ]XÚYÚ[™ÝÔXÙ[Y[ÎŽ™Y˜][

NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆ]XÚYÚ[™ÝÔXÙ[Y[ÎŽ™Y˜][

NÂˆNÂˆ]]]XÙ[Y[ÈH]XÚYÚ[™ÝÔXÙ[Y[ÎŽ™Y˜][

NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]ÛÛYJ
Ù^K˜[Y\ÊJHH[™KœÜ]ÛÛ˜ÙJ	ÏIÊH[ÙHÂˆÛÛ[YNÂˆNÂˆ]Ú[™HX]ÚÙ^HÂˆœ^[\ÝˆOˆ]XÚYÚ[™ÝÒÚ[™Ž”^[\Ýˆ˜Ú[›™[Ü˜XÚÈˆOˆ]XÚYÚ[™ÝÒÚ[™ŽÚ[›™[˜XÚËˆœX[›×Ü›ÛˆOˆ]XÚYÚ[™ÝÒÚ[™Ž”X[›Ô›Ûˆ›Z^\ˆˆOˆ]XÚYÚ[™ÝÒÚ[™Ž“Z^\‹ˆœYÚ[œÈˆOˆ]XÚYÚ[™ÝÒÚ[™Ž”YÚ[œËˆ˜]Y[ÈˆOˆ]XÚYÚ[™ÝÒÚ[™Ž]Y[Ëˆ˜]]ÛX][ÛˆˆOˆ]XÚYÚ[™ÝÒÚ[™Ž]]ÛX][Û‹ˆÈOˆÛÛ[YKˆNÂˆ]]]˜[Y\ÈH˜[Y\ËœÜ]
	Ë	ÊNÂˆ]
ÛÛYJ
KÛÛYJJKÛÛYJÚY
KÛÛYJZYÚ
JHBˆ
˜[Y\Ë›™^

K˜[Y\Ë›™^

K˜[Y\Ë›™^

K˜[Y\Ë›™^

JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆYˆ˜[Y\Ë›™^

Kš\×ÜÛÛYJ
HÂˆÛÛ[YNÂˆBˆ]\œÙWØÛÛÜ™[˜]HH˜[YNˆ	œÝŸÂˆYˆ˜[YHOH‹HˆÂˆ›Û™BˆH[ÙHÂˆ˜[YBˆœ\œÙNŽŒÌŠ
Bˆ›ÚÊ
Bˆ™š[\Š[X™\Ÿ[X™\‹š\×Ùš[š]J
JBˆBˆNÂˆ]XÙ[Y[H]XÚYÚ[™ÝÔXÙ[Y[ÂˆÜÚ][ÛŽˆ\œÙWØÛÛÜ™[˜]J
Bˆžš\
\œÙWØÛÛÜ™[˜]JJJBˆ›X\

J_YÝZNŽœÜÌŠJJKˆ[›™\—ÜÚ^™Nˆ™XÌŽŽ›™]ÊˆÚYœ\œÙNŽŒÌŠ
K[Ü˜\ÛÜ—ÙY˜][

KˆZYÚœ\œÙNŽŒÌŠ
K[Ü˜\ÛÜ—ÙY˜][

Kˆ
KˆNÂˆYˆXÙ[Y[š\×Ý˜[Y
Ú[™
HÂˆXÙ[Y[ËœÙ]
Ú[™XÙ[Y[
NÂˆBˆBˆXÙ[Y[ÂŸB‚™›ˆØ]™WÙ]XÚYÝÚ[™Ý×ÜXÙ[Y[ÊXÙ[Y[Îˆ]XÚYÚ[™ÝÔXÙ[Y[ÊHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H]XÚYÝÚ[™Ý×ÜXÙ[Y[×ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]œ\™[

K›Ú×ÛÜ—Ù[ÙJÂˆH]XÚYÚ[™ÝÈXÙ[Y[Ù][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

BˆJOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈH]XÚYÚ[™ÝÒÚ[™ŽSˆš[×Ú]\Š
Bˆ™š[\—ÛX\
Ú[™ÂˆXÙ[Y[Ë™Ù]
Ú[™
K›X\
XÙ[Y[Âˆ]
JHHXÙ[Y[ˆœÜÚ][Û‚ˆ›X\ÛÜŠ
‹H‹×ÛÝÛ™Y

K‹H‹×ÛÝÛ™Y

JKÜßÂˆ
›Ü›X]JžÎ‹Œ_H‹ÜËž
K›Ü›X]JžÎ‹Œ_H‹ÜËžJJBˆJNÂˆ›Ü›X]JˆžßO^ÞKÞ_KÎ‹Œ_KÎ‹Œ_Wˆ‹ˆÚ[™œÙ][™Ü×ÚÙ^J
KˆXÙ[Y[š[›™\—ÜÚ^™KžˆXÙ[Y[š[›™\—ÜÚ^™KžKˆ
BˆJBˆJBˆ˜ÛÛXÝŽÝš[™ÏŠ
NÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ[˜ÛÙWÝÚ[™Ý×Û^[Ý]Û˜[YJ˜[YNˆ	œÝŠHOˆÝš[™ÈÂˆ˜[YK˜\×Øž]\Ê
Bˆš]\Š
Bˆ›X\
ž]_›Ü›X]JžØž]NŒžHŠJBˆ˜ÛÛXÝ

BŸB‚™›ˆXÛÙWÝÚ[™Ý×Û^[Ý]Û˜[YJ[˜ÛÙYˆ	œÝŠHOˆÜ[ÛÝš[™ÏˆÂˆ][˜ÛÙYH[˜ÛÙY˜\×Øž]\Ê
NÂˆ]
Z\œË™[XZ[™\ŠHH[˜ÛÙY˜\×ØÚ[šÜÎŽŠ
NÂˆYˆ\™[XZ[™\‹š\×Ù[\J
HÂˆ™]\›ˆ›Û™NÂˆBˆ]XÛÙWÛšX˜›HHž]_X]Úž]HÂˆ‰Ì	Ë‹X‰ÎIÈOˆÛÛYJž]HH‰Ì	ÊKˆ‰ØIË‹X‰Ù‰ÈOˆÛÛYJž]HH‰ØIÈ
ÈL
Kˆ‰ÐIË‹X‰Ñ‰ÈOˆÛÛYJž]HH‰ÐIÈ
ÈL
KˆÈOˆ›Û™KˆNÂˆ]ž]\ÈHZ\œÂˆš]\Š
Bˆ›X\
Z\ŸÛÛYJ
XÛÙWÛšX˜›JZ\–ÌJOÈ
HXÛÙWÛšX˜›JZ\–ÌWJOÊJBˆ˜ÛÛXÝŽÜ[Û™XÏÏŠ
OÎÂˆÝš[™ÎŽ™œ›ÛWÝ]Ž
ž]\ÊK›ÚÊ
BŸB‚™›ˆ\œÙWÝÚ[™Ý×Û^[Ý]ÜXÙ[Y[
ˆ˜[YNˆ	œÝ‹ˆÚ[™ˆ]XÚYÚ[™ÝÒÚ[™ŠHOˆÜ[ÛÜ[Û]XÚYÚ[™ÝÔXÙ[Y[ˆÂˆYˆ˜[YHOH‹HˆÂˆ™]\›ˆÛÛYJ›Û™JNÂˆBˆ]]]šY[ÈH˜[YKœÜ]
	Ë	ÊNÂˆ]
ÛÛYJ
KÛÛYJJKÛÛYJÚY
KÛÛYJZYÚ
JHBˆ
šY[Ë›™^

KšY[Ë›™^

KšY[Ë›™^

KšY[Ë›™^

JBˆ[ÙHÂˆ™]\›ˆ›Û™NÂˆNÂˆYˆšY[Ë›™^

Kš\×ÜÛÛYJ
HÂˆ™]\›ˆ›Û™NÂˆBˆ]ÜÚ][ÛˆHX]Ú
JHÂˆ
‹H‹‹HŠHOˆ›Û™Kˆ
‹H‹ÊH
Ë‹HŠHOˆ™]\›ˆ›Û™Kˆ
JHOˆÛÛYJYÝZNŽœÜÌŠœ\œÙJ
K›ÚÊ
OËKœ\œÙJ
K›ÚÊ
OÊJKˆNÂˆ]XÙ[Y[H]XÚYÚ[™ÝÔXÙ[Y[ÂˆÜÚ][Û‹ˆ[›™\—ÜÚ^™Nˆ™XÌŽŽ›™]ÊÚYœ\œÙJ
K›ÚÊ
OËZYÚœ\œÙJ
K›ÚÊ
OÊKˆNÂˆXÙ[Y[š\×Ý˜[Y
Ú[™
K[—ÜÛÛYJÛÛYJXÙ[Y[
JBŸB‚™›ˆ\œÙWÝÚ[™Ý×Û^[Ý]Ü™\Ù]ÊÛÛ[Îˆ	œÝŠHOˆ™XÏÚ[™ÝÓ^[Ý]™\Ù]ˆÂˆ]]][™\ÈHÛÛ[Ë›[™\Ê
NÂˆYˆ[™\Ë›™^

HOHÛÛYJÒS‘Õ×ÓVSÕUÔ‘TÑU×ÒPQTŠHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆBˆ]]]™\Ù]ÈH™XÎŽ›™]Ê
NÂˆ›Üˆ[™H[ˆ[™\ÈÂˆYˆ™\Ù]Ë›[Š
HHÒS‘Õ×ÓVSÕUÔ‘TÑUÓSRUÂˆœ™XZÎÂˆBˆ]šY[ÈH[™KœÜ]
	×	ÊK˜ÛÛXÝŽ™XÏÏŠ
NÂˆYˆšY[Ë›[Š
HOH
È]XÚYÚ[™ÝÒÚ[™ŽS›[Š
HÂˆÛÛ[YNÂˆBˆ]ÛÛYJ˜[YJHHXÛÙWÝÚ[™Ý×Û^[Ý]Û˜[YJšY[ÖÌJH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJXZ[—ÝšY]ÊHHXZ[•šY]ÎŽ™œ›ÛWÜÙ][™Ü×ÚÙ^JšY[ÖÌWJH[ÙHÂˆÛÛ[YNÂˆNÂˆ]œ›ÝÜÙ\—Ýš\ÚX›HHX]ÚšY[ÖÌ—HÂˆŒˆOˆ˜[ÙKˆŒHˆOˆYKˆÈOˆÛÛ[YKˆNÂˆ]ÚÊ]XÚYÛX\ÚÊHHNŽ™œ›ÛWÜÝ—Ü˜Y^
šY[ÖÌ×KMŠH[ÙHÂˆÛÛ[YNÂˆNÂˆYˆ]XÚYÛX\ÚÈ	ˆLÙˆOHÂˆÛÛ[YNÂˆBˆ]]XÚYHÝŽ˜\œ˜^NŽ™œ›ÛWÙ›Š[™^]XÚYÛX\ÚÈ	ˆ
H[™^
HOH
NÂˆ]]]XÙ[Y[ÈH]XÚYÚ[™ÝÔXÙ[Y[ÎŽ™Y˜][

NÂˆ]]]XÙ[Y[×Ý˜[YHYNÂˆ›ÜˆÚ[™[ˆ]XÚYÚ[™ÝÒÚ[™ŽSÂˆX]Ú\œÙWÝÚ[™Ý×Û^[Ý]ÜXÙ[Y[
šY[ÖÍ
ÈÚ[™š[™^

WKÚ[™
HÂˆÛÛYJÛÛYJXÙ[Y[
JHOˆXÙ[Y[ËœÙ]
Ú[™XÙ[Y[
KˆÛÛYJ›Û™JHOˆßBˆ›Û™HOˆXÙ[Y[×Ý˜[YH˜[ÙKˆBˆBˆYˆ\XÙ[Y[×Ý˜[YÂˆÛÛ[YNÂˆBˆ]™\Ù]HÚ[™ÝÓ^[Ý]™\Ù]Âˆ˜[YKˆXZ[—ÝšY]Ëˆœ›ÝÜÙ\—Ýš\ÚX›Kˆ]XÚYˆXÙ[Y[ËˆNÂˆYˆ\™\Ù]š\×Ý˜[Y

Bˆ™\Ù]Ëš]\Š
K˜[žJ^\Ý[™Îˆ	•Ú[™ÝÓ^[Ý]™\Ù]Âˆ^\Ý[™Ë›˜[YK™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ	œ™\Ù]›˜[YJBˆJBˆÂˆÛÛ[YNÂˆBˆ™\Ù]Ëœ\Ú
™\Ù]
NÂˆBˆ™\Ù]ÂŸB‚™›ˆÙ\šX[^™WÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê™\Ù]Îˆ	–ÕÚ[™ÝÓ^[Ý]™\Ù]JHOˆÝš[™ÈÂˆ]]]ÛÛ[ÈH›Ü›X]JžÕÒS‘Õ×ÓVSÕUÔ‘TÑU×ÒPQTŸWˆŠNÂˆ›Üˆ™\Ù][ˆ™\Ù]Âˆš]\Š
Bˆ™š[\Š™\Ù]™\Ù]š\×Ý˜[Y

JBˆZÙJÒS‘Õ×ÓVSÕUÔ‘TÑUÓSRU
BˆÂˆ]]XÚYÛX\ÚÈBˆ™\Ù]ˆ™]XÚYˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™›Û
NX\ÚË
[™^\×Ù]XÚY
_ÂˆYˆ
š\×Ù]XÚYÂˆX\ÚÈ
H[™^
BˆH[ÙHÂˆX\ÚÂˆBˆJNÂˆ]ÈHÝŽ™›]Ž•Üš]NŽÜš]WÙ›]
ˆ	›]]ÛÛ[Ëˆ›Ü›X]Ø\™ÜÈJˆžßWßWßWÙ]XÚYÛX\ÚÎŒžH‹ˆ[˜ÛÙWÝÚ[™Ý×Û^[Ý]Û˜[YJ	œ™\Ù]›˜[YJKˆ™\Ù]›XZ[—ÝšY]ËœÙ][™Ü×ÚÙ^J
KˆNŽ™œ›ÛJ™\Ù]˜œ›ÝÜÙ\—Ýš\ÚX›JKˆ
Kˆ
NÂˆ›ÜˆÚ[™[ˆ]XÚYÚ[™ÝÒÚ[™ŽSÂˆÛÛ[Ëœ\Ú
	×	ÊNÂˆYˆ]ÛÛYJXÙ[Y[
HH™\Ù]œXÙ[Y[Ë™Ù]
Ú[™
HÂˆ]
JHHXÙ[Y[ˆœÜÚ][Û‚ˆ›X\ÛÜŠ
‹H‹×ÛÝÛ™Y

K‹H‹×ÛÝÛ™Y

JKÜßÂˆ
›Ü›X]JžÎ‹Œ_H‹ÜËž
K›Ü›X]JžÎ‹Œ_H‹ÜËžJJBˆJNÂˆ]ÈHÝŽ™›]Ž•Üš]NŽÜš]WÙ›]
ˆ	›]]ÛÛ[Ëˆ›Ü›X]Ø\™ÜÈJˆžÞKÞ_KÎ‹Œ_KÎ‹Œ_H‹ˆXÙ[Y[š[›™\—ÜÚ^™KžXÙ[Y[š[›™\—ÜÚ^™KžBˆ
Kˆ
NÂˆH[ÙHÂˆÛÛ[Ëœ\Ú
	ËIÊNÂˆBˆBˆÛÛ[Ëœ\Ú
	×‰ÊNÂˆBˆÛÛ[ÂŸB‚™›ˆØYÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê
HOˆ™XÏÚ[™ÝÓ^[Ý]™\Ù]ˆÂˆÚ[™Ý×Û^[Ý]Ü™\Ù]×ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ›X\ÛÜ—Ù[ÙJ™XÎŽ›™]ËÛÛ[ß\œÙWÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê	˜ÛÛ[ÊJBŸB‚™›ˆØ]™WÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê™\Ù]Îˆ	–ÕÚ[™ÝÓ^[Ý]™\Ù]JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HÚ[™Ý×Û^[Ý]Ü™\Ù]×ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHÚ[™ÝÈ^[Ý]™\Ù]È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]Ù\šX[^™WÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê™\Ù]ÊJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆX[›×Ü›ÛÛ[Ý\ÙWÝÚY[ÛYÙWÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[ŠœX[›Ë\›Û[[Ý\ÙK]ÚY[[YÙKŠJBŸB‚™›ˆØYÜX[›×Ü›ÛÛYÙWÛ›Ý\×ÝÚ]Û[Ý\ÙWÝÚY[

HOˆ›ÛÛÂˆX[›×Ü›ÛÛ[Ý\ÙWÝÚY[ÛYÙWÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠ˜[ÙJBŸB‚™›ˆØ]™WÜX[›×Ü›ÛÛYÙWÛ›Ý\×ÝÚ]Û[Ý\ÙWÝÚY[
[˜X›Yˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HX[›×Ü›ÛÛ[Ý\ÙWÝÚY[ÛYÙWÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHX[›È›Û[Ý\ÙK]ÚY[Ù][™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÙ[˜X›YWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆÚ[›™[Ü˜XÚ×Û]]WÜ™[[Ý™YÜÝ\×ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜Ú[›™[\˜XÚË[]]K\™[[Ý™Y\Ý\ËŠJBŸB‚™›ˆØYØÚ[›™[Ü˜XÚ×Û]]WÜ™[[Ý™YÜÝ\Ê
HOˆ›ÛÛÂˆÚ[›™[Ü˜XÚ×Û]]WÜ™[[Ý™YÜÝ\×ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠ˜[ÙJBŸB‚™›ˆØ]™WØÚ[›™[Ü˜XÚ×Û]]WÜ™[[Ý™YÜÝ\Ê[˜X›Yˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HÚ[›™[Ü˜XÚ×Û]]WÜ™[[Ý™YÜÝ\×ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHÚ[›™[˜XÚÈÙ][™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÙ[˜X›YWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆYWØÛÛ\ÙYÜ^[\ÝÙÜ›Ý\×ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[ŠšYKXÛÛ\ÙY\^[\ÝYÜ›Ý\ËŠJBŸB‚™›ˆØYÚYWØÛÛ\ÙYÜ^[\ÝÙÜ›Ý\Ê
HOˆ›ÛÛÂˆYWØÛÛ\ÙYÜ^[\ÝÙÜ›Ý\×ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠYJBŸB‚™›ˆØ]™WÚYWØÛÛ\ÙYÜ^[\ÝÙÜ›Ý\Ê[˜X›Yˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]HYWØÛÛ\ÙYÜ^[\ÝÙÜ›Ý\×ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH^[\ÝšY]ÈÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÙ[˜X›YWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ^[\ÝÞ™\›×ØÜ›ÜÜÚ[™×ÜÜ]×ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Šœ^[\Ý^™\›ËXÜ›ÜÜÚ[™Ë\Ü]ËŠJBŸB‚™›ˆØYÜ^[\ÝÞ™\›×ØÜ›ÜÜÚ[™×ÜÜ]Ê
HOˆ›ÛÛÂˆ^[\ÝÞ™\›×ØÜ›ÜÜÚ[™×ÜÜ]×ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠ˜[ÙJBŸB‚™›ˆØ]™WÜ^[\ÝÞ™\›×ØÜ›ÜÜÚ[™×ÜÜ]Ê[˜X›Yˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H^[\ÝÞ™\›×ØÜ›ÜÜÚ[™×ÜÜ]×ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH^[\ÝÜ]Ù][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÙ[˜X›YWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆœ›ÝÜÙ\—ØÛÛ[[—ÝÚYÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜œ›ÝÜÙ\‹XÛÛ[[‹]ÚYŠJBŸB‚™›ˆœ›ÝÜÙ\—Ýš\ÚXš[]WÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜œ›ÝÜÙ\‹]š\ÚX›KŠJBŸB‚™›ˆ™XYÜØ[\WÜ›ÛÝÛ›ÝWÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Šœ™XY\Ø[\K\›ÛÝ[›ÝKŠJBŸB‚™›ˆZYWÜ™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š›ZYK\™XÛÜ™[™Ë[Ù™œÙ][\ËŠJBŸB‚™›ˆ]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š˜]Y[Ë\™XÛÜ™[™Ë[Ù™œÙ][\ËŠJBŸB‚™›ˆ]™WÜ™\Ø[\[™×Ü]X[]WÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š›]™K\™\Ø[\[™Ë\]X[]KŠJBŸB‚™›ˆ^[\ÝÜ™[™\—Ü]X[]WÜÙ][™Ü×Ùš[J
HOˆÜ[Û]YˆÂˆœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
OÂˆœ\™[

Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Šœ^[\Ý\™[™\‹\]X[]KŠJBŸB‚™›ˆØYÛ]™WÜ™\Ø[\[™×Ü]X[]J
HOˆ™\Ø[\[™Ô]X[]HÂˆ]™WÜ™\Ø[\[™×Ü]X[]WÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ß™\Ø[\[™Ô]X[]NŽ™œ›ÛWÜÙ][™Ü×ÚÙ^JÛÛ[Ëš[J
JJBˆ[Ü˜\ÛÜŠ™\Ø[\[™Ô]X[]NŽ“U‘WÑQUS
BŸB‚™›ˆØYÜ^[\ÝÜ™[™\—Ü]X[]J
HOˆ™\Ø[\[™Ô]X[]HÂˆ^[\ÝÜ™[™\—Ü]X[]WÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ß™\Ø[\[™Ô]X[]NŽ™œ›ÛWÜÙ][™Ü×ÚÙ^JÛÛ[Ëš[J
JJBˆ[Ü˜\ÛÜŠ™\Ø[\[™Ô]X[]NŽ”‘S‘T—ÑQUS
BŸB‚™›ˆØ]™WÛ]™WÜ™\Ø[\[™×Ü]X[]J]X[]Nˆ™\Ø[\[™Ô]X[]JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H]™WÜ™\Ø[\[™×Ü]X[]WÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH]™H™\Ø[\[™ÈÙ][™È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžßWˆ‹]X[]KœÙ][™Ü×ÚÙ^J
JJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÜ^[\ÝÜ™[™\—Ü]X[]J]X[]Nˆ™\Ø[\[™Ô]X[]JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H^[\ÝÜ™[™\—Ü]X[]WÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH^[\Ý™[™\ˆ]X[]H]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžßWˆ‹]X[]KœÙ][™Ü×ÚÙ^J
JJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYÜ™XYÜØ[\WÜ›ÛÝÛ›ÝJ
HOˆ›ÛÛÂˆ™XYÜØ[\WÜ›ÛÝÛ›ÝWÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠYJBŸB‚™›ˆØ]™WÜ™XYÜØ[\WÜ›ÛÝÛ›ÝJ[˜X›Yˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H™XYÜØ[\WÜ›ÛÝÛ›ÝWÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHØ[\H›ÛÝ[›ÝH™Y™\™[˜ÙH]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÙ[˜X›YWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYÛZYWÜ™XÛÜ™[™×ÛÙ™œÙ]Û\Ê
HOˆLMˆÂˆZYWÜ™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽLMŠ
K›ÚÊ
JBˆ™š[\ŠÙ™œÙ]Û\ßÂˆ
RQWÔ‘PÓÔ‘S‘×ÓÑ‘”ÑUÓRS—ÓTË‹SRQWÔ‘PÓÔ‘S‘×ÓÑ‘”ÑUÓPVÓTÊK˜ÛÛZ[œÊÙ™œÙ]Û\ÊBˆJBˆ[Ü˜\ÛÜŠ
BŸB‚™›ˆØYØ]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]Û\Ê
HOˆLMˆÂˆ]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽLMŠ
K›ÚÊ
JBˆ™š[\ŠÙ™œÙ]Û\ßÂˆ
UQS×Ô‘PÓÔ‘S‘×ÓÑ‘”ÑUÓRS—ÓTË‹PUQS×Ô‘PÓÔ‘S‘×ÓÑ‘”ÑUÓPVÓTÊK˜ÛÛZ[œÊÙ™œÙ]Û\ÊBˆJBˆ[Ü˜\ÛÜŠ
BŸB‚™›ˆØ]™WØ]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]Û\ÊÙ™œÙ]Û\ÎˆLMŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆJUQS×Ô‘PÓÔ‘S‘×ÓÑ‘”ÑUÓRS—ÓTË‹PUQS×Ô‘PÓÔ‘S‘×ÓÑ‘”ÑUÓPVÓTÊK˜ÛÛZ[œÊ	›Ù™œÙ]Û\ÊHÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ[œÝ\ÜY]Y[È[œ][Z[™ÈÙ™œÙ]ˆÛÙ™œÙ]Û\ßH\È‚ˆ
JNÂˆBˆ]]H]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]œ\™[

K›Ú×ÛÜ—Ù[ÙJÂˆH]Y[È™XÛÜ™[™ÈÙ™œÙ]Ù][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

BˆJOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÛÙ™œÙ]Û\ßWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WÛZYWÜ™XÛÜ™[™×ÛÙ™œÙ]Û\ÊÙ™œÙ]Û\ÎˆLMŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆJRQWÔ‘PÓÔ‘S‘×ÓÑ‘”ÑUÓRS—ÓTË‹SRQWÔ‘PÓÔ‘S‘×ÓÑ‘”ÑUÓPVÓTÊK˜ÛÛZ[œÊ	›Ù™œÙ]Û\ÊHÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ[œÝ\ÜYRQH[œ][Z[™ÈÙ™œÙ]ˆÛÙ™œÙ]Û\ßH\È‚ˆ
JNÂˆBˆ]]HZYWÜ™XÛÜ™[™×ÛÙ™œÙ]ÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRQH™XÛÜ™[™ÈÙ™œÙ]Ù][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÛÙ™œÙ]Û\ßWˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYØœ›ÝÜÙ\—Ýš\ÚXš[]J
HOˆ›ÛÛÂˆœ›ÝÜÙ\—Ýš\ÚXš[]WÜÙ][™Ü×Ùš[J
Bˆ˜[™Ý[Š]œÎŽœ™XYÝ×ÜÝš[™Ê]
K›ÚÊ
JBˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠYJBŸB‚™›ˆØ]™WØœ›ÝÜÙ\—Ýš\ÚXš[]Jš\ÚX›Nˆ›ÛÛ
HOˆ™\Ý[

KÝš[™ÏˆÂˆ]]Hœ›ÝÜÙ\—Ýš\ÚXš[]WÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHœ›ÝÜÙ\ˆš\ÚXš[]HÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÝš\ÚX›_WˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYØœ›ÝÜÙ\—ØÛÛ[[—ÝÚY

HOˆŒÌˆÂˆ]ÛÛYJ]
HHœ›ÝÜÙ\—ØÛÛ[[—ÝÚYÜÙ][™Ü×Ùš[J
H[ÙHÂˆ™]\›ˆQUSÐ”“ÕÔÑT—ÐÓÓSS—ÕÒQÂˆNÂˆœÎŽœ™XYÝ×ÜÝš[™Ê]
Bˆ›ÚÊ
Bˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽŒÌŠ
K›ÚÊ
JBˆ™š[\ŠÚYÂˆÚYš\×Ùš[š]J
Bˆ	‰ˆ
RS—Ð”“ÕÔÑT—ÐÓÓSS—ÕÒQ‹SPVÐ”“ÕÔÑT—ÐÓÓSS—ÕÒQ
K˜ÛÛZ[œÊÚY
BˆJBˆ[Ü˜\ÛÜŠQUSÐ”“ÕÔÑT—ÐÓÓSS—ÕÒQ
BŸB‚™›ˆØ]™WØœ›ÝÜÙ\—ØÛÛ[[—ÝÚY
ÚYˆŒÌŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆ]ÚYš\×Ùš[š]J
HJRS—Ð”“ÕÔÑT—ÐÓÓSS—ÕÒQ‹SPVÐ”“ÕÔÑT—ÐÓÓSS—ÕÒQ
K˜ÛÛZ[œÊ	ÚY
BˆÂˆ™]\›ˆ\œŠ›Ü›X]J[œÝ\ÜYœ›ÝÜÙ\ˆÚYˆÝÚYHŠJNÂˆBˆ]]Hœ›ÝÜÙ\—ØÛÛ[[—ÝÚYÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHœ›ÝÜÙ\ˆÚYÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÝÚY‹Œ_WˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYÝZWÜØØ[J
HOˆŒÌˆÂˆ]ÛÛYJ]
HHZWÜØØ[WÜÙ][™Ü×Ùš[J
H[ÙHÂˆ™]\›ˆKŒÂˆNÂˆœÎŽœ™XYÝ×ÜÝš[™Ê]
Bˆ›ÚÊ
Bˆ˜[™Ý[ŠÛÛ[ßÛÛ[Ëš[J
Kœ\œÙNŽŒÌŠ
K›ÚÊ
JBˆ™š[\ŠØØ[_RWÔÐÐSWÔÕTË˜ÛÛZ[œÊØØ[JJBˆ[Ü˜\ÛÜŠKŒ
BŸB‚™›ˆØ]™WÝZWÜØØ[JØØ[NˆŒÌŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆURWÔÐÐSWÔÕTË˜ÛÛZ[œÊ	œØØ[JHÂˆ™]\›ˆ\œŠ›Ü›X]J[œÝ\ÜYRHØØ[NˆÜØØ[_HŠJNÂˆBˆ]]HZWÜØØ[WÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHRHØØ[HÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]J	œ]›Ü›X]JžÜØØ[N‹Œ_WˆŠJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYØ]]ÜØ]™WÜÙ][™ÜÊ
HOˆÜ[Û
N›ÛÛ\Ú^™JOˆÂˆ]ÛÛ[ÈHœÎŽœ™XYÝ×ÜÝš[™Ê]]ÜØ]™WÜÙ][™Ü×Ùš[J
OÊK›ÚÊ
OÎÂˆ]]][™\ÈHÛÛ[Ë›[™\Ê
NÂˆ]]]ÜØ]™WÛZ[]\ÈH[™\Ë›™^

OËœ\œÙNŽNŠ
K›ÚÊ
OÎÂˆ]˜XÚÝ\Ü™][[ÛˆH[™\Ë›™^

OËœ\œÙNŽ\Ú^™OŠ
K›ÚÊ
OÎÂˆ]]]ÜØ]™WØ™Y›Ü™WÜš\ÚÞHH[™\Âˆ›™^

Bˆ˜[™Ý[Š˜[Y_˜[YKœ\œÙNŽ›ÛÛŠ
K›ÚÊ
JBˆ[Ü˜\ÛÜŠ˜[ÙJNÂˆYˆPUUÔÐU‘WÒS•T•S×ÓRS•UTË˜ÛÛZ[œÊ	˜]]ÜØ]™WÛZ[]\ÊBˆPPÒÕTÔ‘US•SÓ—ÓÔSÓ”Ë˜ÛÛZ[œÊ	˜˜XÚÝ\Ü™][[ÛŠBˆÂˆ™]\›ˆ›Û™NÂˆBˆÛÛYJ
]]ÜØ]™WÛZ[]\Ë]]ÜØ]™WØ™Y›Ü™WÜš\ÚÞK˜XÚÝ\Ü™][[ÛŠJBŸB‚™›ˆØ]™WØ]]ÜØ]™WÜÙ][™ÜÊˆ]]ÜØ]™WÛZ[]\ÎˆNˆ]]ÜØ]™WØ™Y›Ü™WÜš\ÚÞNˆ›ÛÛˆ˜XÚÝ\Ü™][[ÛŽˆ\Ú^™KŠHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]H]]ÜØ]™WÜÙ][™Ü×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH]]ÜØ]™HÙ][™ÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆœÎŽÜš]Jˆ	œ]ˆ›Ü›X]JžØ]]ÜØ]™WÛZ[]\ßWžØ˜XÚÝ\Ü™][[ÛŸWžØ]]ÜØ]™WØ™Y›Ü™WÜš\ÚÞ_WˆŠKˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆ\Ù\—Ù]WÙ\™XÝÜžJ
HOˆÜ[Û]YˆÂˆYˆÙ™ÈJ\™Ù]ÛÜÈHÚ[™ÝÜÈŠHÂˆÝŽ™[ŽŽ˜\—ÛÜÊTUHŠK›X\
]YŽŽ™œ›ÛJK›Ü—Ù[ÙJÂˆÝŽ™[ŽŽ˜\—ÛÜÊ•TÑT”“Ñ’SHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š\]HŠKš›Ú[Š”›Ø[Z[™ÈŠJBˆJBˆH[ÙHYˆÙ™ÈJ\™Ù]ÛÜÈH›XXÛÜÈŠHÂˆÝŽ™[ŽŽ˜\—ÛÜÊ’ÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š“Xœ˜\žHŠKš›Ú[Š\XØ][ÛˆÝ\ÜŠJBˆH[ÙHÂˆÝŽ™[ŽŽ˜\—ÛÜÊ–×ÑUWÒÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›Ü—Ù[ÙJÂˆÝŽ™[ŽŽ˜\—ÛÜÊ’ÓQHŠBˆ›X\
]YŽŽ™œ›ÛJBˆ›X\
ÛY_ÛYKš›Ú[Š‹›ØØ[ŠKš›Ú[ŠœÚ\™HŠJBˆJBˆBˆ›X\
›ÛÝ›ÛÝš›Ú[Š™›\ÝY[Ë\™XZ[ŠJBŸB‚™›ˆ™]×Ý[]YÜ›Ú™XÝØ]]ÜØ]™WÚÙ^J
HOˆÜ[Û]YˆÂˆ]›Û˜ÙHHÞ\Ý[U[YNŽ››ÝÊ
Bˆ™\˜][Û—ÜÚ[˜ÙJS’VÑTÐÒ
Bˆ›ÚÊ
OÂˆ˜\×Û˜[›ÜÊ
NÂˆ]\™XÝÜžHH\Ù\—Ù]WÙ\™XÝÜžJ
OËš›Ú[Š•[]Y›Ú™XÝÈŠNÂˆœÎŽ˜Ü™X]WÙ\—Ø[
	™\™XÝÜžJK›ÚÊ
OÎÂˆÛÛYJ\™XÝÜžKš›Ú[Š›Ü›X]J[]Y^ßK^Û›Û˜Ù_K™›‹ÝŽœ›ØÙ\ÜÎŽšY

JJJBŸB‚™›ˆ›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]ˆ	”]
HOˆÜ[Û]YˆÂˆÛÛYJˆ\Ù\—Ù]WÙ\™XÝÜžJ
OÂˆš›Ú[Š˜XÚÝ\ÈŠBˆš›Ú[ŠÝX›WÜ›Ú™XÝÚY
›Ú™XÝÜ]
JKˆ
BŸB‚™›ˆÜš]WÝ[]YØ˜XÚÝ\ÛY]Y]Jˆ›Ú™XÝÜ]ˆ	”]ˆ[\]WÜ]ˆÜ[Û	”]‹ŠHOˆ™\Ý[

KÝš[™ÏˆÂˆ]\™XÝÜžHH›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆ]H›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
	™\™XÝÜžJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™XÝÜžK™\Ü^J
JJOÎÂˆÜš]WÙš[WØ]ÛZXØ[J	™\™XÝÜžKš›Ú[Š[]Yœ›Ú™XÝŠKˆ[]YˆŠBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›Ý™XÛÜ™[]Y›Ú™XÝY]Y]NˆÙ\œ›ÜŸHŠJOÎÂˆ][\]WÛY]Y]HH\™XÝÜžKš›Ú[Š[\]Kœ]ŠNÂˆYˆ]ÛÛYJ[\]WÜ]
HH[\]WÜ]ÂˆÜš]WÙš[WØ]ÛZXØ[Jˆ	[\]WÛY]Y]Kˆ[\]WÜ]×ÜÝš[™×ÛÜÜÞJ
K˜\×Øž]\Ê
Kˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›Ý™XÛÜ™›Ú™XÝ[\]H]ˆÙ\œ›ÜŸHŠJOÎÂˆH[ÙHÂˆ]ÈHœÎŽœ™[[Ý™WÙš[J[\]WÛY]Y]JNÂˆBˆÚÊ

JBŸB‚™›ˆ™[[Ý™WÝ[]YÜ›Ú™XÝØ˜XÚÝ\
›Ú™XÝÜ]ˆ	”]
HÂˆYˆ]ÛÛYJ\™XÝÜžJHH›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]
HÂˆ]ÈHœÎŽœ™[[Ý™WÙ\—Ø[
\™XÝÜžJNÂˆBŸB‚™›ˆÝX›WÜ›Ú™XÝÚY
›Ú™XÝÜ]ˆ	”]
HOˆÝš[™ÈÂˆ]›Ü›X[^™YHœÎŽ˜Ø[›ÛšXØ[^™J›Ú™XÝÜ]
K[Ü˜\ÛÜ—Ù[ÙJßÂˆ]XœÛÛ]HH›Ü›X[^™YØXœÛÛ]WÜ]
›Ú™XÝÜ]
NÂˆX]Ú
XœÛÛ]Kœ\™[

KXœÛÛ]K™š[WÛ˜[YJ
JHÂˆ
ÛÛYJ\™[
KÛÛYJ˜[YJJHOˆœÎŽ˜Ø[›ÛšXØ[^™J\™[
Bˆ[Ü˜\ÛÜ—Ù[ÙJß›Ü›X[^™YØXœÛÛ]WÜ]
\™[
JBˆš›Ú[Š˜[YJKˆÈOˆXœÛÛ]KˆBˆJNÂˆ]]]˜[YHH›Ü›X[^™Y×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

NÂˆYˆÙ™ÈJ\™Ù]ÛÜÈHÚ[™ÝÜÈŠHÂˆ˜[YK›XZÙWØ\ØÚZWÛÝÙ\˜Ø\ÙJ
NÂˆBˆ]]]\ÚHØ™ŒŽXÙMŒŒŒÌ]MÂˆ›Üˆž]H[ˆ˜[YK˜\×Øž]\Ê
HÂˆ\ÚHMŽ™œ›ÛJ
˜ž]JNÂˆ\ÚH\ÚÜ˜\[™×Û][
LXŒÊNÂˆBˆ›Ü›X]JžÚ\ÚŒMžHŠBŸB‚™›ˆÜš]WÜ›Ú™XÝØ˜XÚÝ\
ˆ›Ú™XÝÜ]ˆ	”]ˆž]\Îˆ	–ÝNKˆ\œÜÙNˆ˜XÚÝ\\œÜÙKˆ™][[ÛŽˆ\Ú^™KŠHOˆ™\Ý[]Y‹Ýš[™ÏˆÂˆ]\™XÝÜžHH›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆ]H›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
	™\™XÝÜžJBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™XÝÜžK™\Ü^J
JJOÎÂˆ]™XÛÜ™YÜ]BˆœÎŽ˜Ø[›ÛšXØ[^™J›Ú™XÝÜ]
K[Ü˜\ÛÜ—Ù[ÙJß›Ü›X[^™YØXœÛÛ]WÜ]
›Ú™XÝÜ]
JNÂˆœÎŽÜš]Jˆ\™XÝÜžKš›Ú[Šœ›Ú™XÝœ]ŠKˆ™XÛÜ™YÜ]×ÜÝš[™×ÛÜÜÞJ
K˜\×Øž]\Ê
Kˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›Ý™XÛÜ™›Ú™XÝ]ˆÙ\œ›ÜŸHŠJOÎÂ‚ˆ]Ú[™HX]Ú\œÜÙHÂˆ˜XÚÝ\\œÜÙNŽ]]ÜØ]™HOˆ˜]]ÜØ]™H‹ˆ˜XÚÝ\\œÜÙNŽ“X[X[Oˆ˜˜XÚÝ\‹ˆNÂˆ][Y\Ý[\HÞ\Ý[U[YNŽ››ÝÊ
Bˆ™\˜][Û—ÜÚ[˜ÙJS’VÑTÐÒ
Bˆ[Ü˜\ÛÜ—ÙY˜][

Bˆ˜\×Û˜[›ÜÊ
NÂˆ]^[œÚ[ÛˆHYˆž]\ËœÝ\×ÝÚ]
ˆ”××ŠBˆž]\ËœÝ\×ÝÚ]
ˆ”×WˆŠBˆž]\ËœÝ\×ÝÚ]
ˆ”××ŠBˆÂˆžš\‚ˆH[ÙHÂˆ™›‚ˆNÂˆ]]H\™XÝÜžKš›Ú[Š›Ü›X]JˆžÚÚ[™K^Ý[Y\Ý[\K^ßKžÙ^[œÚ[ÛŸH‹ˆÝŽœ›ØÙ\ÜÎŽšY

Bˆ
JNÂˆÜš]WÙš[WØ]ÛZXØ[J	œ]ž]\ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJOÎÂˆ[™WÙÛØ˜[Ü›Ú™XÝØ˜XÚÝ\Ê	™\™XÝÜžK™][[ÛŠNÂˆÚÊ]
BŸB‚™›ˆÜš]WÙš[WØ]ÛZXØ[J]ˆ	”]ž]\Îˆ	–ÝNJHOˆÝŽš[ÎŽ”™\Ý[

OˆÂˆ]˜[YHH]ˆ™š[WÛ˜[YJ
Bˆ›X\
˜[Y_˜[YK×ÜÝš[™×ÛÜÜÞJ
JBˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ][Y\Ý[\HÞ\Ý[U[YNŽ››ÝÊ
Bˆ™\˜][Û—ÜÚ[˜ÙJS’VÑTÐÒ
Bˆ[Ü˜\ÛÜ—ÙY˜][

Bˆ˜\×Û˜[›ÜÊ
NÂˆ][\Ü˜\žHH]Ú]Ùš[WÛ˜[YJ›Ü›X]J‹žÛ˜[Y_K\^ßK^Ý[Y\Ý[\H‹ÝŽœ›ØÙ\ÜÎŽšY

JJNÂˆœÎŽÜš]J	[\Ü˜\žKž]\ÊOÎÂˆX]ÚœÎŽœ™[˜[YJ	[\Ü˜\žK]
HÂˆÚÊ

JHOˆÚÊ

JKˆ\œŠ\œ›ÜŠHOˆÂˆ]ÈHœÎŽœ™[[Ý™WÙš[J[\Ü˜\žJNÂˆ\œŠ\œ›ÜŠBˆBˆBŸB‚™›ˆ[™WÙÛØ˜[Ü›Ú™XÝØ˜XÚÝ\Ê›Ú™XÝÙ\™XÝÜžNˆ	”]™][[ÛŽˆ\Ú^™JHÂˆ]ÛÛYJ›ÛÝ
HH›Ú™XÝÙ\™XÝÜžKœ\™[

H[ÙHÂˆ™]\›ŽÂˆNÂˆ]]]˜XÚÝ\ÈHœÎŽœ™XYÙ\Š›ÛÝ
Bˆ›ÚÊ
Bˆš[×Ú]\Š
Bˆ™›][Š
Bˆ™š[\—ÛX\
™\Ý[Ž›ÚÊBˆ™š[\—ÛX\
[ž_œÎŽœ™XYÙ\Š[žKœ]

JK›ÚÊ
JBˆ™›][Š
Bˆ™š[\—ÛX\
™\Ý[Ž›ÚÊBˆ›X\
[ž_[žKœ]

JBˆ™š[\Š]Âˆ]™^[œÚ[ÛŠ
Kš\×ÜÛÛYWØ[™
^[œÚ[ÛŸÂˆ^[œÚ[Û‹™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ™›ŠH^[œÚ[Û‹™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJžš\ŠBˆJH	‰ˆ]™š[WÛ˜[YJ
Kš\×ÜÛÛYWØ[™
˜[Y_Âˆ˜[YK×ÜÝš[™×ÛÜÜÞJ
KœÝ\×ÝÚ]
˜]]ÜØ]™KHŠBˆ˜[YK×ÜÝš[™×ÛÜÜÞJ
KœÝ\×ÝÚ]
˜˜XÚÝ\HŠBˆJBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ˜XÚÝ\ËœÛÜØžWÚÙ^J]ÂˆœÎŽ›Y]Y]J]
Bˆ˜[™Ý[ŠY]Y]_Y]Y]K›[ÙYšYY

JBˆ[Ü˜\ÛÜŠS’VÑTÐÒ
BˆJNÂˆ]™[[Ý™WØÛÝ[H˜XÚÝ\Ë›[Š
KœØ]\˜][™×ÜÝXŠ™][[ÛŠNÂˆ›Üˆ][ˆ˜XÚÝ\Ëš[×Ú]\Š
KZÙJ™[[Ý™WØÛÝ[
HÂˆ]ÈHœÎŽœ™[[Ý™WÙš[J]
NÂˆBŸB‚™›ˆ›Ú™XÝØ]]ÜØ]™WÙš[\Ê›Ú™XÝÜ]ˆ	”]
HOˆ™XÏ]YˆÂˆ]ÛÛYJ\™XÝÜžJHH›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]ÚÊ[šY\ÊHHœÎŽœ™XYÙ\Š\™XÝÜžJH[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]]]]ÈH[šY\Âˆ™š[\—ÛX\
™\Ý[Ž›ÚÊBˆ›X\
[ž_[žKœ]

JBˆ™š[\Š]Âˆ]™^[œÚ[ÛŠ
Bˆš\×ÜÛÛYWØ[™
^[œÚ[ÛŸ^[œÚ[Û‹™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ™›ŠJBˆ	‰ˆ]ˆ™š[WÛ˜[YJ
Bˆš\×ÜÛÛYWØ[™
˜[Y_˜[YK×ÜÝš[™×ÛÜÜÞJ
KœÝ\×ÝÚ]
˜]]ÜØ]™KHŠJBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ]ËœÛÜØžWÚÙ^J]ÂˆÝŽ˜Û\Ž”™]™\œÙJˆœÎŽ›Y]Y]J]
Bˆ˜[™Ý[ŠY]Y]_Y]Y]K›[ÙYšYY

JBˆ[Ü˜\ÛÜŠS’VÑTÐÒ
Kˆ
BˆJNÂˆ]ÂŸB‚™›ˆ]\ÝÜ›Ú™XÝØ]]ÜØ]™J›Ú™XÝÜ]ˆ	”]
HOˆÜ[Û]YˆÂˆ›Ú™XÝØ]]ÜØ]™WÙš[\Ê›Ú™XÝÜ]
Bˆš[×Ú]\Š
Bˆ™š[™
]ÂˆœÎŽœ™XY
]
Bˆ›ÚÊ
Bˆš\×ÜÛÛYWØ[™
ž]\ß›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊKš\×ÛÚÊ
JBˆJBŸB‚™›ˆ™XÛÝ™\žWÜ›Û\Ù›Ü—Ü›Ú™XÝ
›Ú™XÝÜ]ˆ	”]
HOˆÜ[Û™XÛÝ™\žT›Û\ˆÂˆ]\™XÝÜžHH›Ú™XÝØ˜XÚÝ\Ù\™XÝÜžJ›Ú™XÝÜ]
OÎÂˆ]™XÛÜ™YÜ]HœÎŽœ™XYÝ×ÜÝš[™Ê\™XÝÜžKš›Ú[Šœ›Ú™XÝœ]ŠJK›ÚÊ
OÎÂˆYˆ\›Ú™XÝÜ]×Ù\]X[
]Ž›™]Ê™XÛÜ™YÜ]š[J
JK›Ú™XÝÜ]
HÂˆ™]\›ˆ›Û™NÂˆBˆ]˜XÚÝ\Ü]H]\ÝÜ›Ú™XÝØ]]ÜØ]™J›Ú™XÝÜ]
OÎÂˆ]˜XÚÝ\Û[ÙYšYYHœÎŽ›Y]Y]J	˜˜XÚÝ\Ü]
K›ÚÊ
OË›[ÙYšYY

K›ÚÊ
OÎÂˆ]ÛÝ\˜ÙWÛ[ÙYšYYHœÎŽ›Y]Y]J›Ú™XÝÜ]
Bˆ›ÚÊ
Bˆ˜[™Ý[ŠY]Y]_Y]Y]K›[ÙYšYY

K›ÚÊ
JNÂˆYˆÛÝ\˜ÙWÛ[ÙYšYYš\×ÜÛÛYWØ[™
[ÙYšYY˜XÚÝ\Û[ÙYšYY[ÙYšYY
HÂˆ™]\›ˆ›Û™NÂˆBˆYˆœÎŽœ™XY
›Ú™XÝÜ]
K›ÚÊ
Kš\×ÜÛÛYWØ[™
ÛÝ\˜ÙWØž]\ßÂˆœÎŽœ™XY
	˜˜XÚÝ\Ü]
Kš\×ÛÚ×Ø[™
˜XÚÝ\˜XÚÝ\OHÛÝ\˜ÙWØž]\ÊBˆJHÂˆ™]\›ˆ›Û™NÂˆBˆÛÛYJ™XÛÝ™\žT›Û\ÂˆÛÝ\˜ÙWÜ]ˆ›Ú™XÝÜ]×Ü]ØYŠ
Kˆ˜XÚÝ\Ü]ˆ\×Ü™]™\ˆ˜[ÙKˆ\×Ý[]Yˆ\™XÝÜžKš›Ú[Š[]Yœ›Ú™XÝŠKš\×Ùš[J
Kˆ[\]WÜ]ˆœÎŽœ™XYÝ×ÜÝš[™Ê\™XÝÜžKš›Ú[Š[\]Kœ]ŠJBˆ›ÚÊ
Bˆ›X\
]]š[J
K×ÛÝÛ™Y

JBˆ™š[\Š]\]š\×Ù[\J
JBˆ›X\
]YŽŽ™œ›ÛJKˆJBŸB‚™›ˆ]\ÝÜ™XÛÝ™\žWÜ›Û\

HOˆÜ[Û™XÛÝ™\žT›Û\ˆÂˆ]›ÛÝH\Ù\—Ù]WÙ\™XÝÜžJ
OËš›Ú[Š˜XÚÝ\ÈŠNÂˆ]\™XÝÜšY\ÈHœÎŽœ™XYÙ\Š›ÛÝ
K›ÚÊ
OÎÂˆ\™XÝÜšY\Âˆ™š[\—ÛX\
™\Ý[Ž›ÚÊBˆ™š[\—ÛX\
[ž_Âˆ]\™XÝÜžHH[žKœ]

NÂˆ]ÛÝ\˜ÙWÜ]HœÎŽœ™XYÝ×ÜÝš[™Ê\™XÝÜžKš›Ú[Šœ›Ú™XÝœ]ŠJK›ÚÊ
OÎÂˆ™XÛÝ™\žWÜ›Û\Ù›Ü—Ü›Ú™XÝ
]Ž›™]ÊÛÝ\˜ÙWÜ]š[J
JJBˆJBˆ›X^ØžWÚÙ^J›Û\ÂˆœÎŽ›Y]Y]J	œ›Û\˜˜XÚÝ\Ü]
Bˆ˜[™Ý[ŠY]Y]_Y]Y]K›[ÙYšYY

JBˆ[Ü˜\ÛÜŠS’VÑTÐÒ
BˆJBŸB‚™›ˆ›Ú™XÝÜ]×Ù\]X[
Yˆ	”]šYÚˆ	”]
HOˆ›ÛÛÂˆ]YHœÎŽ˜Ø[›ÛšXØ[^™JY
K[Ü˜\ÛÜ—Ù[ÙJßXœÛÛ]WÜ]
Y
JNÂˆ]šYÚHœÎŽ˜Ø[›ÛšXØ[^™JšYÚ
K[Ü˜\ÛÜ—Ù[ÙJßXœÛÛ]WÜ]
šYÚ
JNÂˆYˆÙ™ÈJ\™Ù]ÛÜÈHÚ[™ÝÜÈŠHÂˆY×ÜÝš[™×ÛÜÜÞJ
Bˆ™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ	œšYÚ×ÜÝš[™×ÛÜÜÞJ
JBˆH[ÙHÂˆYOHšYÚˆBŸB‚™›ˆ\×Ù^[œÚ[ÛŠ]ˆ	”]^[œÚ[ÛŽˆ	œÝŠHOˆ›ÛÛÂˆ]™^[œÚ[ÛŠ
Bˆš\×ÜÛÛYWØ[™
Ø[™Y]_Ø[™Y]K™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ^[œÚ[ÛŠJBŸB‚™›ˆXœÛÛ]WÜ]
]ˆ	”]
HOˆ]YˆÂˆYˆ]š\×ØXœÛÛ]J
HÂˆ]×Ü]ØYŠ
BˆH[ÙHÂˆÝŽ™[ŽŽ˜Ý\œ™[Ù\Š
Bˆ›X\
\™XÝÜž_\™XÝÜžKš›Ú[Š]
JBˆ[Ü˜\ÛÜ—Ù[ÙJß]×Ü]ØYŠ
JBˆBŸB‚™›ˆ›Ü›X[^™YØXœÛÛ]WÜ]
]ˆ	”]
HOˆ]YˆÂˆ]XœÛÛ]HHXœÛÛ]WÜ]
]
NÂˆ]]]›Ü›X[^™YH]YŽŽ›™]Ê
NÂˆ›ÜˆÛÛ\Û™[[ˆXœÛÛ]K˜ÛÛ\Û™[Ê
HÂˆX]ÚÛÛ\Û™[ÂˆÛÛ\Û™[ŽÝ\‘\ˆOˆßBˆÛÛ\Û™[Ž”\™[\ˆOˆÂˆ›Ü›X[^™YœÜ

NÂˆBˆÝ\ˆOˆ›Ü›X[^™Yœ\Ú
Ý\‹˜\×ÛÜ×ÜÝŠ
JKˆBˆBˆ›Ü›X[^™YŸB‚™›ˆ™^Ü›Ú™XÝÝ™\œÚ[Û—Ü]
]ˆ	”]
HOˆ]YˆÂˆ]Ý[HH]ˆ™š[WÜÝ[J
Bˆ›X\
Ý[_Ý[K×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

JBˆ™š[\ŠÝ[_\Ý[Kš\×Ù[\J
JBˆ[Ü˜\ÛÜ—Ù[ÙJœ›Ú™XÝ‹×ÛÝÛ™Y

JNÂˆ]^[œÚ[ÛˆH]ˆ™^[œÚ[ÛŠ
Bˆ›X\
^[œÚ[ÛŸ^[œÚ[Û‹×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

JBˆ™š[\Š^[œÚ[ÛŸY^[œÚ[Û‹š\×Ù[\J
JBˆ[Ü˜\ÛÜ—Ù[ÙJ™›‹×ÛÝÛ™Y

JNÂˆ]
˜\ÙK]]™\œÚ[ÛŠHHÝ[BˆœœÜ]ÛÛ˜ÙJ	×ÉÊBˆ˜[™Ý[Š
˜\ÙKÝY™š^
_ÂˆÝY™š^ˆœ\œÙNŽLÌŠ
Bˆ›ÚÊ
Bˆ™š[\Š™\œÚ[ÛŸ
™\œÚ[ÛˆHŠBˆ›X\
™\œÚ[ÛŸ
˜\ÙK×ÛÝÛ™Y

K™\œÚ[Ûˆ
ÈJJBˆJBˆ[Ü˜\ÛÜŠ
Ý[KŠJNÂˆ]\™XÝÜžHH]œ\™[

K[Ü˜\ÛÜ—Ù[ÙJ]Ž›™]Ê‹ˆŠJNÂˆÛÜÂˆ]Ø[™Y]HH\™XÝÜžKš›Ú[Š›Ü›X]JžØ˜\Ù_WÞÝ™\œÚ[ÛŸKžÙ^[œÚ[ÛŸHŠJNÂˆYˆXØ[™Y]K™^\ÝÊ
HÂˆ™]\›ˆØ[™Y]NÂˆBˆ™\œÚ[ÛˆH™\œÚ[Û‹œØ]\˜][™×ØY
JNÂˆBŸB‚™›ˆØYØœ›ÝÜÙ\—Ù˜]›Üš]\Ê
HOˆ•™YTÙ]]YˆÂˆ]ÛÛYJ]
HHœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
H[ÙHÂˆ™]\›ˆ•™YTÙ]Ž›™]Ê
NÂˆNÂˆœÎŽœ™XYÝ×ÜÝš[™Ê]
Bˆ›X\
ÛÛ[ßÂˆÛÛ[Âˆ›[™\Ê
Bˆ™š[\Š[™_[[™Kš\×Ù[\J
JBˆ›X\
]YŽŽ™œ›ÛJBˆ˜ÛÛXÝ

BˆJBˆ[Ü˜\ÛÜ—ÙY˜][

BŸB‚™›ˆØYØœ›ÝÜÙ\—ÜÙX\˜ÚÜ›ÛÝÊ
HOˆ™XÏ]YˆÂˆ]ÛÛYJ]
HHœ›ÝÜÙ\—ÜÙX\˜ÚÜ›ÛÝ×Ùš[J
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]]]›ÛÝÈH™XÎŽ›™]Ê
NÂˆ›Üˆ[žH[ˆÛÛ[Ë›[™\Ê
K™š[\Š[ž_Y[žKš\×Ù[\J
JHÂˆ]]H]YŽŽ™œ›ÛJ[žJNÂˆ]]HœÎŽ˜Ø[›ÛšXØ[^™J	œ]
K[Ü˜\ÛÜŠ]
NÂˆYˆ\›ÛÝË˜ÛÛZ[œÊ	œ]
HÂˆ›ÛÝËœ\Ú
]
NÂˆBˆYˆ›ÛÝË›[Š
HH”“ÕÔÑT—ÔÑPTÒÔ“ÓÕÓSRUÂˆœ™XZÎÂˆBˆBˆ›ÛÝÂŸB‚™›ˆØ]™WØœ›ÝÜÙ\—ÜÙX\˜ÚÜ›ÛÝÊ›ÛÝÎˆ	–Ô]Y—JHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]Hœ›ÝÜÙ\—ÜÙX\˜ÚÜ›ÛÝ×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHœ›ÝÜÙ\ˆÙX\˜Ú›Û\œÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈH›ÛÝÂˆš]\Š
BˆZÙJ”“ÕÔÑT—ÔÑPTÒÔ“ÓÕÓSRU
Bˆ›X\
]]×ÜÝš[™×ÛÜÜÞJ
JBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š—ˆŠNÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØ]™WØœ›ÝÜÙ\—Ù˜]›Üš]\Ê˜]›Üš]\Îˆ	•™YTÙ]]YŠHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]Hœ›ÝÜÙ\—Ù˜]›Üš]\×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈH˜]›Üš]\Âˆš]\Š
Bˆ›X\
]]×ÜÝš[™×ÛÜÜÞJ
JBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š—ˆŠNÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆØYØœ›ÝÜÙ\—ÝYÜÊ
HOˆ•™YSX\]Y‹•™YTÙ]Ýš[™ÏˆÂˆ]ÛÛYJ]
HHœ›ÝÜÙ\—ÝYÜ×Ùš[J
H[ÙHÂˆ™]\›ˆ•™YSX\Ž›™]Ê
NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆ•™YSX\Ž›™]Ê
NÂˆNÂˆ]]]YÜ×ØžWÜ]H•™YSX\Ž›™]Ê
NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]]]šY[ÈH[™KœÜ]
	×	ÊNÂˆ]ÛÛYJ]
HHšY[Ë›™^

K™š[\Š]\]š\×Ù[\J
JH[ÙHÂˆÛÛ[YNÂˆNÂˆ]YÜÈHšY[Âˆ™›]ÛX\
šY[šY[œÜ]
	Ë	ÊJBˆ›X\
ÝŽŽš[JBˆ™š[\ŠYß]YËš\×Ù[\J
JBˆ›X\
ÝŽŽ×ÛÝÛ™Y
Bˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂˆYˆ]YÜËš\×Ù[\J
HÂˆYÜ×ØžWÜ]š[œÙ\
]YŽŽ™œ›ÛJ]
KYÜÊNÂˆBˆBˆYÜ×ØžWÜ]ŸB‚™›ˆØ]™WØœ›ÝÜÙ\—ÝYÜÊYÜ×ØžWÜ]ˆ	•™YSX\]Y‹•™YTÙ]Ýš[™ÏŠHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]Hœ›ÝÜÙ\—ÝYÜ×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHœ›ÝÜÙ\ˆYÜÈ]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈHYÜ×ØžWÜ]ˆš]\Š
Bˆ™š[\Š
ËYÜÊ_]YÜËš\×Ù[\J
JBˆ›X\

]YÜÊ_Âˆ]]][™HH]×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

NÂˆ›ÜˆYÈ[ˆYÜÈÂˆ[™Kœ\Ú
	×	ÊNÂˆ[™Kœ\ÚÜÝŠ	YËœ™\XÙJÉ×	Ë	×‰Ë	×‰×KˆŠJNÂˆBˆ[™BˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š—ˆŠNÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆœ›ÝÜÙ\—ÜÙX\˜ÚÙš[\—ÚÙ^Jš[\Žˆœ›ÝÜÙ\‘š[\ŠHOˆ	‰ÜÝ]XÈÝˆÂˆX]Úš[\ˆÂˆœ›ÝÜÙ\‘š[\ŽŽ[Oˆ˜[‹ˆœ›ÝÜÙ\‘š[\ŽŽ]Y[ÈOˆ˜]Y[È‹ˆœ›ÝÜÙ\‘š[\ŽŽ”›Ú™XÝÈOˆœ›Ú™XÝÈ‹ˆœ›ÝÜÙ\‘š[\ŽŽ”™\Ù]ÈOˆœ™\Ù]È‹ˆBŸB‚™›ˆ\œÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙš[\Š˜[YNˆ	œÝŠHOˆÜ[Ûœ›ÝÜÙ\‘š[\ˆÂˆX]Ú˜[YHÂˆ˜[ˆOˆÛÛYJœ›ÝÜÙ\‘š[\ŽŽ[
Kˆ˜]Y[ÈˆOˆÛÛYJœ›ÝÜÙ\‘š[\ŽŽ]Y[ÊKˆœ›Ú™XÝÈˆOˆÛÛYJœ›ÝÜÙ\‘š[\ŽŽ”›Ú™XÝÊKˆœ™\Ù]ÈˆOˆÛÛYJœ›ÝÜÙ\‘š[\ŽŽ”™\Ù]ÊKˆÈOˆ›Û™KˆBŸB‚™›ˆœ›ÝÜÙ\—ÝY×ÛÙÚX×ÚÙ^JÙÚXÎˆœ›ÝÜÙ\•YÓÙÚXÊHOˆ	‰ÜÝ]XÈÝˆÂˆX]ÚÙÚXÈÂˆœ›ÝÜÙ\•YÓÙÚXÎŽ[žHOˆ˜[žH‹ˆœ›ÝÜÙ\•YÓÙÚXÎŽ[Oˆ˜[‹ˆBŸB‚™›ˆ\œÙWØœ›ÝÜÙ\—ÝY×ÛÙÚXÊ˜[YNˆ	œÝŠHOˆÜ[Ûœ›ÝÜÙ\•YÓÙÚXÏˆÂˆX]Ú˜[YHÂˆ˜[žHˆOˆÛÛYJœ›ÝÜÙ\•YÓÙÚXÎŽ[žJKˆ˜[ˆOˆÛÛYJœ›ÝÜÙ\•YÓÙÚXÎŽ[
KˆÈOˆ›Û™KˆBŸB‚™›ˆ[˜ÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
˜[YNˆ	œÝŠHOˆÝš[™ÈÂˆ]]][˜ÛÙYHÝš[™ÎŽÚ]ØØ\XÚ]J˜[YK›[Š
JNÂˆ›ÜˆÚ\˜XÝ\ˆ[ˆ˜[YK˜Ú\œÊ
HÂˆX]ÚÚ\˜XÝ\ˆÂˆ	ÉIÈOˆ[˜ÛÙYœ\ÚÜÝŠ‰LHŠKˆ	×	ÈOˆ[˜ÛÙYœ\ÚÜÝŠ‰LHŠKˆ	×‰ÈOˆ[˜ÛÙYœ\ÚÜÝŠ‰LHŠKˆ	×‰ÈOˆ[˜ÛÙYœ\ÚÜÝŠ‰LŠKˆÈOˆ[˜ÛÙYœ\Ú
Ú\˜XÝ\ŠKˆBˆBˆ[˜ÛÙYŸB‚™›ˆXÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
˜[YNˆ	œÝŠHOˆÝš[™ÈÂˆ]ž]\ÈH˜[YK˜\×Øž]\Ê
NÂˆ]]]XÛÙYH™XÎŽÚ]ØØ\XÚ]Jž]\Ë›[Š
JNÂˆ]]][™^HÂˆÚ[H[™^ž]\Ë›[Š
HÂˆYˆž]\ÖÚ[™^HOH‰ÉIÈ	‰ˆ[™^
Èˆž]\Ë›[Š
HÂˆ]YÚH
ž]\ÖÚ[™^
ÈWH\ÈÚ\ŠK×ÙYÚ]
MŠNÂˆ]ÝÈH
ž]\ÖÚ[™^
È—H\ÈÚ\ŠK×ÙYÚ]
MŠNÂˆYˆ]
ÛÛYJYÚ
KÛÛYJÝÊJHH
YÚÝÊHÂˆXÛÙYœ\Ú


YÚ
HÝÊH\ÈN
NÂˆ[™^
ÏHÎÂˆÛÛ[YNÂˆBˆBˆXÛÙYœ\Ú
ž]\ÖÚ[™^JNÂˆ[™^
ÏHNÂˆBˆÝš[™ÎŽ™œ›ÛWÝ]ŽÛÜÜÞJ	™XÛÙY
Kš[×ÛÝÛ™Y

BŸB‚™›ˆØYØœ›ÝÜÙ\—ÜØ]™YÜÙX\˜Ú\Ê
HOˆ™XÏØ]™Yœ›ÝÜÙ\”ÙX\˜ÚˆÂˆ]ÛÛYJ]
HHœ›ÝÜÙ\—ÜØ]™YÜÙX\˜Ú\×Ùš[J
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]ÚÊÛÛ[ÊHHœÎŽœ™XYÝ×ÜÝš[™Ê]
H[ÙHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆNÂˆ]]]ÙX\˜Ú\ÈH™XÎŽ›™]Ê
NÂˆ›Üˆ[™H[ˆÛÛ[Ë›[™\Ê
HÂˆ]]]šY[ÈH[™KœÜ]
	×	ÊNÂˆ]™\œÚ[ÛˆHšY[Ë›™^

NÂˆ]
ÛÛYJ˜[YJKÛÛYJ]Y\žJKÛÛYJš[\ŠKÛÛYJ[Ü›ÛÝÊKÛÛYJ™XÝ\œÚ]™JKÛÛYJ]
JHH
ˆšY[Ë›™^

KˆšY[Ë›™^

KˆšY[Ë›™^

KˆšY[Ë›™^

KˆšY[Ë›™^

KˆšY[Ë›™^

Kˆ
H[ÙHÂˆÛÛ[YNÂˆNÂˆ]
Y×ÛÙÚXËY[‹ÛÛÜ‹XÛÛ‹Ù[XÝYÝYÜÊHHX]Ú™\œÚ[ÛˆÂˆÛÛYJŒHŠHYˆšY[Ë›™^

Kš\×Û›Û™J
HOˆ
ˆœ›ÝÜÙ\•YÓÙÚXÎŽ[žKˆ˜[ÙKˆœ›ÝÜÙ\•XÛÛÜŽŽ™Y˜][

Kˆœ›ÝÜÙ\•X’XÛÛŽŽ™Y˜][

Kˆ•™YTÙ]Ž›™]Ê
Kˆ
KˆÛÛYJŒˆŠHOˆÂˆ]ÛÛYJY×ÛÙÚXÊHHšY[Ë›™^

K˜[™Ý[Š\œÙWØœ›ÝÜÙ\—ÝY×ÛÙÚXÊH[ÙHÂˆÛÛ[YNÂˆNÂˆ]Ù[XÝYÝYÜÈHšY[Âˆ›X\
XÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
Bˆ›X\
YßYËš[J
K×ÛÝÛ™Y

JBˆ™š[\ŠYß]YËš\×Ù[\J
JBˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂˆ
ˆY×ÛÙÚXËˆ˜[ÙKˆœ›ÝÜÙ\•XÛÛÜŽŽ™Y˜][

Kˆœ›ÝÜÙ\•X’XÛÛŽŽ™Y˜][

KˆÙ[XÝYÝYÜËˆ
BˆBˆÛÛYJ™\œÚ[Ûˆ
ŒÈˆŠJHOˆÂˆ]ÛÛYJY×ÛÙÚXÊHHšY[Ë›™^

K˜[™Ý[Š\œÙWØœ›ÝÜÙ\—ÝY×ÛÙÚXÊH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJÚÊY[ŠJHHšY[Ë›™^

K›X\
ÝŽŽœ\œÙNŽ›ÛÛŠH[ÙHÂˆÛÛ[YNÂˆNÂˆ]
ÛÛÜ‹XÛÛŠHHYˆ™\œÚ[ÛˆOHˆÂˆ]ÛÛYJÛÛÜŠHHšY[Ë›™^

K˜[™Ý[Šœ›ÝÜÙ\•XÛÛÜŽŽœ\œÙJH[ÙHÂˆÛÛ[YNÂˆNÂˆ]ÛÛYJXÛÛŠHHšY[Ë›™^

K˜[™Ý[Šœ›ÝÜÙ\•X’XÛÛŽŽœ\œÙJH[ÙHÂˆÛÛ[YNÂˆNÂˆ
ÛÛÜ‹XÛÛŠBˆH[ÙHÂˆ
œ›ÝÜÙ\•XÛÛÜŽŽ™Y˜][

Kœ›ÝÜÙ\•X’XÛÛŽŽ™Y˜][

JBˆNÂˆ]Ù[XÝYÝYÜÈHšY[Âˆ›X\
XÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
Bˆ›X\
YßYËš[J
K×ÛÝÛ™Y

JBˆ™š[\ŠYß]YËš\×Ù[\J
JBˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂˆ
Y×ÛÙÚXËY[‹ÛÛÜ‹XÛÛ‹Ù[XÝYÝYÜÊBˆBˆÈOˆÛÛ[YKˆNÂˆ]˜[YHHXÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
˜[YJNÂˆ]]Y\žHHXÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
]Y\žJNÂˆ]ÛÛYJš[\ŠHH\œÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙš[\Šš[\ŠH[ÙHÂˆÛÛ[YNÂˆNÂˆ]
ÚÊ[Ü›ÛÝÊKÚÊ™XÝ\œÚ]™JJHH
[Ü›ÛÝËœ\œÙNŽ›ÛÛŠ
K™XÝ\œÚ]™Kœ\œÙNŽ›ÛÛŠ
JBˆ[ÙHÂˆÛÛ[YNÂˆNÂˆ]]HXÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
]
NÂˆYˆ˜[YKš[J
Kš\×Ù[\J
H
X[Ü›ÛÝÈ	‰ˆ]š\×Ù[\J
JHÂˆÛÛ[YNÂˆBˆ]ÙX\˜ÚHØ]™Yœ›ÝÜÙ\”ÙX\˜ÚÂˆ˜[YNˆ˜[YKš[J
K×ÛÝÛ™Y

Kˆ]Y\žKˆš[\‹ˆ[Ü›ÛÝËˆ™XÝ\œÚ]™Kˆ]ˆYˆ[Ü›ÛÝÈÂˆ]YŽŽ›™]Ê
BˆH[ÙHÂˆ]YŽŽ™œ›ÛJ]
BˆKˆY×ÛÙÚXËˆÙ[XÝYÝYÜËˆY[‹ˆÛÛÜ‹ˆXÛÛ‹ˆNÂˆYˆ]ÛÛYJ^\Ý[™ÊHHÙX\˜Ú\Âˆš]\—Û]]

Bˆ™š[™
^\Ý[™Îˆ	‰›]]Ø]™Yœ›ÝÜÙ\”ÙX\˜ÚÂˆ^\Ý[™Ë›˜[YK™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ	œÙX\˜Ú›˜[YJBˆJBˆÂˆ
™^\Ý[™ÈHÙX\˜ÚÂˆH[ÙHÂˆÙX\˜Ú\Ëœ\Ú
ÙX\˜Ú
NÂˆBˆBˆÙX\˜Ú\ÂŸB‚™›ˆØ]™WØœ›ÝÜÙ\—ÜØ]™YÜÙX\˜Ú\ÊÙX\˜Ú\Îˆ	–ÔØ]™Yœ›ÝÜÙ\”ÙX\˜ÚJHOˆ™\Ý[

KÝš[™ÏˆÂˆ]]Hœ›ÝÜÙ\—ÜØ]™YÜÙX\˜Ú\×Ùš[J
Bˆ›Ú×ÛÜ—Ù[ÙJH\Ù\ˆÛÛ™šYÝ\˜][Ûˆ›Û\ˆ\È›Ý]˜Z[X›H‹×ÛÝÛ™Y

JOÎÂˆ]\™[H]ˆœ\™[

Bˆ›Ú×ÛÜ—Ù[ÙJHœ›ÝÜÙ\ˆØ]™YÙX\˜Ú\È]\È›È\™[›Û\ˆ‹×ÛÝÛ™Y

JOÎÂˆœÎŽ˜Ü™X]WÙ\—Ø[
\™[
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜ™X]HßNˆÙ\œ›ÜŸH‹\™[™\Ü^J
JJOÎÂˆ]ÛÛ[ÈHÙX\˜Ú\Âˆš]\Š
Bˆ›X\
ÙX\˜ÚÂˆ]]HYˆÙX\˜Ú˜[Ü›ÛÝÈÂˆÝš[™ÎŽ›™]Ê
BˆH[ÙHÂˆÙX\˜Úœ]×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

BˆNÂˆ]]]šY[ÈH™XÈVÂˆ‹×ÛÝÛ™Y

Kˆ[˜ÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
	œÙX\˜Ú›˜[YJKˆ[˜ÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
	œÙX\˜Úœ]Y\žJKˆœ›ÝÜÙ\—ÜÙX\˜ÚÙš[\—ÚÙ^JÙX\˜Ú™š[\ŠK×ÛÝÛ™Y

KˆÙX\˜Ú˜[Ü›ÛÝË×ÜÝš[™Ê
KˆÙX\˜Úœ™XÝ\œÚ]™K×ÜÝš[™Ê
Kˆ[˜ÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
	œ]
Kˆœ›ÝÜÙ\—ÝY×ÛÙÚX×ÚÙ^JÙX\˜ÚY×ÛÙÚXÊK×ÛÝÛ™Y

KˆÙX\˜ÚšY[‹×ÜÝš[™Ê
KˆÙX\˜Ú˜ÛÛÜ‹šÙ^J
K×ÛÝÛ™Y

KˆÙX\˜ÚšXÛÛ‹šÙ^J
K×ÛÝÛ™Y

KˆNÂˆšY[Ë™^[™
ˆÙX\˜ÚˆœÙ[XÝYÝYÜÂˆš]\Š
Bˆ›X\
Yß[˜ÛÙWØœ›ÝÜÙ\—ÜÙX\˜ÚÙšY[
YÊJKˆ
NÂˆšY[Ëš›Ú[Š—ŠBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
Bˆš›Ú[Š—ˆŠNÂˆœÎŽÜš]J	œ]ÛÛ[ÊBˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]J˜ÛÝ[›ÝÜš]HßNˆÙ\œ›ÜŸH‹]™\Ü^J
JJBŸB‚™›ˆœ›ÝÜÙ\—Ù˜XÝÜžWÜXÚÜ×Ù\™XÝÜžJ
HOˆÜ[Û]YˆÂˆ]™\ÛÛ™\ˆHØ[\T]™\ÛÛ™\ŽŽ›™]Ê]Ž›™]Ê˜œ›ÝÜÙ\‹\›ÛÝ™›ŠJNÂˆ›Üˆ›ÛÝ[ˆ™\ÛÛ™\‹™˜XÝÜžWÜ›ÛÝÊ
HÂˆ›ÜˆØ[™Y]H[ˆÂˆ›ÛÝš›Ú[Š‘]HŠKš›Ú[Š”]Ú\ÈŠKš›Ú[Š”XÚÜÈŠKˆ›ÛÝš›Ú[Š”]Ú\ÈŠKš›Ú[Š”XÚÜÈŠKˆ›ÛÝš›Ú[Š”XÚÜÈŠKˆHÂˆYˆØ[™Y]Kš\×Ù\Š
HÂˆ™]\›ˆÛÛYJØ[™Y]JNÂˆBˆBˆYˆ›ÛÝˆ™š[WÛ˜[YJ
Bˆš\×ÜÛÛYWØ[™
˜[Y_˜[YK×ÜÝš[™×ÛÜÜÞJ
K™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ”XÚÜÈŠJBˆ	‰ˆ›ÛÝš\×Ù\Š
BˆÂˆ™]\›ˆÛÛYJ›ÛÝ˜ÛÛ™J
JNÂˆBˆBˆ›Û™BŸB‚™›ˆœ›ÝÜÙ\—Ùš[WÚÚ[™
]ˆ	”]
HOˆÜ[Ûœ›ÝÜÙ\‘š[RÚ[™ˆÂˆ]^[œÚ[ÛˆH]ˆ™^[œÚ[ÛŠ
Bˆ˜[™Ý[Š^[œÚ[ÛŸ^[œÚ[Û‹×ÜÝŠ
JOÂˆ×Ø\ØÚZWÛÝÙ\˜Ø\ÙJ
NÂˆX]Ú^[œÚ[Û‹˜\×ÜÝŠ
HÂˆØ]ˆˆØ]™Hˆ›\Èˆ›MHˆ›ÙÙÈˆ™›XÈˆ˜ZYˆˆ˜ZY™ˆˆÝˆˆOˆÂˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž]Y[ÊBˆBˆ™›ˆžš\ˆOˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”›Ú™XÝ
Kˆ™œÝˆ™žˆ™žˆˆœÝ™\Ù]ˆOˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”™\Ù]
Kˆ›ZYˆ›ZYHˆOˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž“ZYJKˆÈOˆ›Û™KˆBŸB‚™›ˆœ›ÝÜÙ\—Ùš[\—ÛX]Ú\Ê]ˆ	”]š[\Žˆœ›ÝÜÙ\‘š[\ŠHOˆ›ÛÛÂˆX]Ú\ÈJˆ
š[\‹œ›ÝÜÙ\—Ùš[WÚÚ[™
]
JKˆ
œ›ÝÜÙ\‘š[\ŽŽ[ÛÛYJÊJBˆ
œ›ÝÜÙ\‘š[\ŽŽ]Y[ËÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž]Y[ÊJBˆ
œ›ÝÜÙ\‘š[\ŽŽ”›Ú™XÝËÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”›Ú™XÝ
JBˆ
œ›ÝÜÙ\‘š[\ŽŽ”™\Ù]ËÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”™\Ù]
JBˆ
BŸB‚™›ˆ\œÙWØœ›ÝÜÙ\—ÝY×Ú[œ]
˜[YNˆ	œÝŠHOˆ™XÏÝš[™ÏˆÂˆ]]]ÙY[ˆH•™YTÙ]Ž›™]Ê
NÂˆ˜[YBˆœÜ]
	Ë	ÊBˆ›X\
YßYËœ™\XÙJÉ×	Ë	×‰Ë	×‰×KˆŠJBˆ›X\
YßYËš[J
K×ÛÝÛ™Y

JBˆ™š[\ŠYß]YËš\×Ù[\J
JBˆ™š[\ŠYßÙY[‹š[œÙ\
YË×ÛÝÙ\˜Ø\ÙJ
JJBˆ˜ÛÛXÝ

BŸB‚™›ˆœ›ÝÜÙ\—ÝY×ÜÙ[XÝ[Û—ÛX]Ú\ÊˆÙÚXÎˆœ›ÝÜÙ\•YÓÙÚXËˆÙ[XÝYÝYÜÎˆ	•™YTÙ]Ýš[™Ï‹ˆš[WÝYÜÎˆÜ[Û	•™YTÙ]Ýš[™Ï‹ŠHOˆ›ÛÛÂˆYˆÙ[XÝYÝYÜËš\×Ù[\J
HÂˆ™]\›ˆYNÂˆBˆ]ÛÛYJš[WÝYÜÊHHš[WÝYÜÈ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆX]ÚÙÚXÈÂˆœ›ÝÜÙ\•YÓÙÚXÎŽ[žHOˆÙ[XÝYÝYÜËš]\Š
K˜[žJÙ[XÝYÂˆ]Ù[XÝYHÙ[XÝY×ÛÝÙ\˜Ø\ÙJ
NÂˆš[WÝYÜËš]\Š
K˜[žJYßYË×ÛÝÙ\˜Ø\ÙJ
HOHÙ[XÝY
BˆJKˆœ›ÝÜÙ\•YÓÙÚXÎŽ[OˆÙ[XÝYÝYÜËš]\Š
K˜[
Ù[XÝYÂˆ]Ù[XÝYHÙ[XÝY×ÛÝÙ\˜Ø\ÙJ
NÂˆš[WÝYÜËš]\Š
K˜[žJYßYË×ÛÝÙ\˜Ø\ÙJ
HOHÙ[XÝY
BˆJKˆBŸB‚™›ˆœ›ÝÜÙ\—Ùš[WÚXÛÛŠ]ˆ	”]
HOˆ	‰ÜÝ]XÈÝˆÂˆX]Úœ›ÝÜÙ\—Ùš[WÚÚ[™
]
HÂˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž]Y[ÊHOˆ¸¦jÈ‹ˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”›Ú™XÝ
HOˆ‘“‹ˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž”™\Ù]
HOˆ‘”Õ‹ˆÛÛYJœ›ÝÜÙ\‘š[RÚ[™Ž“ZYJHOˆ“RQH‹ˆ›Û™HOˆ°­È‹ˆBŸB‚™›ˆÛ\Û˜[YJÛ\ˆ	”^[\ÝÛ\˜XÚÜÎˆ	–Ô^[\Ý˜XÚ×JHOˆÝš[™ÈÂˆX]ÚÛ\\™Ù]

HÂˆ›Ü™XZ[Ž”^[\ÝÛ\\™Ù]Ž”]\›ˆÈYHOˆ›Ü›X]J”]\›ˆßH‹Y
Kˆ›Ü™XZ[Ž”^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHOˆ˜XÚÜÂˆš]\Š
Bˆ™š[™
˜XÚßÛÛYJ˜XÚËšY
HOHÛ\œ^[\ÝÝ˜XÚ×ÚY

JBˆ˜[™Ý[Š˜XÚß˜XÚË›˜[YK˜\×Ù\™YŠ
JBˆ›X\
ÝŽŽ×ÛÝÛ™Y
Bˆ[Ü˜\ÛÜ—Ù[ÙJ›Ü›X]JÚ[›™[ßH‹Y
JKˆBŸB‚™›ˆ›ÝWÛ˜[YJÙ^NˆLMŠHOˆÝš[™ÈÂˆÛÛœÝSQTÎˆÉœÝŽÈL—HHÂˆÈ‹ÈÈ‹‘‹‘È‹‘H‹‘ˆ‹‘ˆÈ‹‘È‹‘ÈÈ‹H‹HÈ‹ˆ‹ˆNÂˆ›Ü›X]Jžß^ßH‹SQTÖÊÙ^H	HLŠH\È\Ú^™WKÙ^HÈLŠBŸB‚™›ˆZ[ÜØ[\WÙY]Ü—ÝØ]™Y›Ü›JˆZNˆ	›]]YÝZNŽ•ZKˆXZÜÎˆ	–ÕØ]™Y›Ü›TXZ×Kˆœ˜[YWØÛÝ[ˆ\Ú^™KˆÙ[XÝ[ÛŽˆÝŽ›ÜÎŽ”˜[™ÙO\Ú^™O‹ˆÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜŽˆ	›]]Ü[Û\Ú^™O‹ŠHOˆÜ[ÛÝŽ›ÜÎŽ”˜[™ÙO\Ú^™OˆÂˆ]ÚYHZK˜]˜Z[X›WÝÚY

K›X^
NŒ
NÂˆ]
™\ÜÛœÙKZ[\ŠHHZK˜[ØØ]WÜZ[\Š™XÌŽŽ›™]ÊÚYMŒ
KÙ[œÙNŽ˜ÛXÚ×Ø[™Ù˜YÊ
JNÂˆ]™XÝH™\ÜÛœÙKœ™XÝÂˆZ[\‹œ™XÝÙš[Y
™XÝYÝZNŽÛÜ›™\”˜Y]\ÎŽœØ[YJJKS‘S
NÂˆYˆœ˜[YWØÛÝ[OHXZÜËš\×Ù[\J
HÂˆ™]\›ˆ›Û™NÂˆBˆ]œ˜[YWÜØØ[HH™XÝÚY

HÈœ˜[YWØÛÝ[\ÈŒÌŽÂˆ]Ù[XÝ[Û—Ü™XÝHYÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYÝZNŽœÜÌŠˆ™XÝ›Y

H
ÈÙ[XÝ[Û‹œÝ\\ÈŒÌˆ
ˆœ˜[YWÜØØ[Kˆ™XÝÜ

Kˆ
KˆYÝZNŽœÜÌŠˆ™XÝ›Y

H
ÈÙ[XÝ[Û‹™[™\ÈŒÌˆ
ˆœ˜[YWÜØØ[Kˆ™XÝ˜›ÝÛJ
Kˆ
Kˆ
NÂˆZ[\‹œ™XÝÙš[Y
ˆÙ[XÝ[Û—Ü™XÝˆYÝZNŽÛÜ›™\”˜Y]\ÎŽ–‘T“ËˆÛÛÜŒÌŽŽ™œ›ÛWÜ™Ø˜WÝ[›][\YY
ÍKNMKÌ
Kˆ
NÂˆ]Ù[\—ÞHH™XÝ˜Ù[\Š
KžNÂˆ][\]YHH
™XÝšZYÚ

H
ˆÊK›X^
KŒ
NÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠ™XÝ›Y

KÙ[\—ÞJKˆYÝZNŽœÜÌŠ™XÝœšYÚ

KÙ[\—ÞJKˆKˆÝ›ÚÙNŽ›™]ÊKŒ“Ô‘TŠKˆ
NÂˆ›Üˆ
[™^XZÊH[ˆXZÜËš]\Š
K™[[Y\˜]J
HÂˆ]H™XÝ›Y

H
È
[™^\ÈŒÌˆ
ÈJH
ˆ™XÝÚY

HÈXZÜË›[Š
H\ÈŒÌŽÂˆ]Ø[\WÜÝ\H[™^
ˆœ˜[YWØÛÝ[ÈXZÜË›[Š
NÂˆ]Ø[\WÙ[™H
[™^
ÈJH
ˆœ˜[YWØÛÝ[ÈXZÜË›[Š
NÂˆ]Ù[XÝYHØ[\WÜÝ\Ù[XÝ[Û‹™[™	‰ˆÙ[XÝ[Û‹œÝ\Ø[\WÙ[™Âˆ]ÛÛÜˆHYˆÙ[XÝYÂˆÛÛÜŒÌŽŽ™œ›ÛWÜ™ØŠLL‹NL‹MŽ
BˆH[ÙHÂˆÛÛÜŒÌŽŽ™œ›ÛWÜ™ØŠLMËMKMLJBˆNÂˆ]Z[š[][HHXZË›Z[š[][K˜Û[\
LKŒKŒ
NÂˆ]X^[][HHXZË›X^[][K˜Û[\
LKŒKŒ
NÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠÙ[\—ÞHHX^[][H
ˆ[\]YJKˆYÝZNŽœÜÌŠÙ[\—ÞHHZ[š[][H
ˆ[\]YJKˆKˆÝ›ÚÙNŽ›™]ÊKŒÛÛÜŠKˆ
NÂˆBˆZK˜YÜÜXÙJŒ
NÂˆØ[\WÙY]Ü—ØØ[˜\×ÜÙ[XÝ[ÛŠ	œ™\ÜÛœÙKœ˜[YWØÛÝ[Ù[XÝ[Û—Ù˜Y×Ø[˜ÚÜŠBŸB‚™›ˆZ[ÜØ[\WÙY]Ü—ÜÜXÝ[JˆZNˆ	›]]YÝZNŽ•ZKˆØ]™Y›Ü›Nˆ	]Y[ÕØ]™Y›Ü›Kˆœ˜[YWØÛÝ[ˆ\Ú^™KˆÙ[XÝ[ÛŽˆÝŽ›ÜÎŽ”˜[™ÙO\Ú^™O‹ˆÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜŽˆ	›]]Ü[Û\Ú^™O‹ŠHOˆÜ[ÛÝŽ›ÜÎŽ”˜[™ÙO\Ú^™OˆÂˆ]ÚYHZK˜]˜Z[X›WÝÚY

K›X^
NŒ
NÂˆ]
™\ÜÛœÙKZ[\ŠHHZK˜[ØØ]WÜZ[\Š™XÌŽŽ›™]ÊÚYMŒ
KÙ[œÙNŽ˜ÛXÚ×Ø[™Ù˜YÊ
JNÂˆ]™XÝH™\ÜÛœÙKœ™XÝÂˆZ[\‹œ™XÝÙš[Y
™XÝYÝZNŽÛÜ›™\”˜Y]\ÎŽœØ[YJJKS‘SÑT’ÊNÂˆYˆœ˜[YWØÛÝ[OHØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ëš\×Ù[\J
HÂˆ™]\›ˆ›Û™NÂˆBˆ]š\œÝØÛÛ[[ˆHØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ë™š\œÝ

OÎÂˆ]˜[™ØÛÝ[Hš\œÝØÛÛ[[‹›[Š
NÂˆYˆ˜[™ØÛÝ[OHÂˆ™]\›ˆ›Û™NÂˆBˆ]ÛÛ[[—ØÛÝ[HØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ë›[Š
NÂˆ]˜[™ÚZYÚH™XÝšZYÚ

HÈ˜[™ØÛÝ[\ÈŒÌŽÂˆ›Üˆ
ÛÛ[[—Ú[™^˜[™ÊH[ˆØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ëš]\Š
K™[[Y\˜]J
HÂˆ]ÛÛ[[—ÛYH™XÝ›Y

H
ÈÛÛ[[—Ú[™^\ÈŒÌˆ
ˆ™XÝÚY

HÈÛÛ[[—ØÛÝ[\ÈŒÌŽÂˆ]ÛÛ[[—ÜšYÚBˆ™XÝ›Y

H
È
ÛÛ[[—Ú[™^
ÈJH\ÈŒÌˆ
ˆ™XÝÚY

HÈÛÛ[[—ØÛÝ[\ÈŒÌŽÂˆ]ÛÛ[[—ÜÝ\Bˆ
ÛÛ[[—Ú[™^\ÈLLŽ
ˆœ˜[YWØÛÝ[\ÈLLŽÈÛÛ[[—ØÛÝ[\ÈLLŽ
H\È\Ú^™NÂˆ]ÛÛ[[—Ù[™H


ÛÛ[[—Ú[™^
ÈJH\ÈLLŽ
ˆœ˜[YWØÛÝ[\ÈLLŽ
Bˆ™]—ØÙZ[
ÛÛ[[—ØÛÝ[\ÈLLŽ
JH\È\Ú^™NÂˆ]Ù[XÝYHÛÛ[[—ÜÝ\Ù[XÝ[Û‹™[™	‰ˆÙ[XÝ[Û‹œÝ\ÛÛ[[—Ù[™Âˆ›Üˆ
˜[™	š[[œÚ]JH[ˆ˜[™Ëš]\Š
K™[[Y\˜]J
HÂˆ][[œÚ]HHŒÌŽŽ™œ›ÛJ[[œÚ]JHÈMKŒÂˆYˆ[[œÚ]HŒˆÂˆÛÛ[YNÂˆBˆ]ÛÛÜˆHYˆ[[œÚ]HŒˆÂˆ›[™ØÛÛÜŠS‘SÑT’ËT”K[[œÚ]HÈŒŠBˆH[ÙHÂˆ›[™ØÛÛÜŠT”K“QK
[[œÚ]HHŒŠHÈŒÎ
BˆNÂˆ]ÛÛÜˆHYˆÙ[XÝYÂˆÛÛÜ‚ˆH[ÙHÂˆ›[™ØÛÛÜŠS‘SÑT’ËÛÛÜ‹
BˆNÂˆ]ÜH™XÝ˜›ÝÛJ
HH
˜[™
ÈJH\ÈŒÌˆ
ˆ˜[™ÚZYÚÂˆZ[\‹œ™XÝÙš[Y
ˆYÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYÝZNŽœÜÌŠÛÛ[[—ÛYÜ
KˆYÝZNŽœÜÌŠÛÛ[[—ÜšYÚ
ÈKÜ
È˜[™ÚZYÚ
ÈJKˆ
KˆYÝZNŽÛÜ›™\”˜Y]\ÎŽ–‘T“ËˆÛÛÜ‹ˆ
NÂˆBˆBˆ]œ˜[YWÜØØ[HH™XÝÚY

HÈœ˜[YWØÛÝ[\ÈŒÌŽÂˆ]Ù[XÝ[Û—Ü™XÝHYÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYÝZNŽœÜÌŠˆ™XÝ›Y

H
ÈÙ[XÝ[Û‹œÝ\\ÈŒÌˆ
ˆœ˜[YWÜØØ[Kˆ™XÝÜ

Kˆ
KˆYÝZNŽœÜÌŠˆ™XÝ›Y

H
ÈÙ[XÝ[Û‹™[™\ÈŒÌˆ
ˆœ˜[YWÜØØ[Kˆ™XÝ˜›ÝÛJ
Kˆ
Kˆ
NÂˆZ[\‹œ™XÝÙš[Y
ˆÙ[XÝ[Û—Ü™XÝˆYÝZNŽÛÜ›™\”˜Y]\ÎŽ–‘T“ËˆÛÛÜŒÌŽŽ™œ›ÛWÜ™Ø˜WÝ[›][\YY
ÍKNMKŽ
Kˆ
NÂˆ›Üˆ[ˆÜÙ[XÝ[Û—Ü™XÝ›Y

KÙ[XÝ[Û—Ü™XÝœšYÚ

WHÂˆZ[\‹›[™WÜÙYÛY[
ˆÙYÝZNŽœÜÌŠ™XÝÜ

JKYÝZNŽœÜÌŠ™XÝ˜›ÝÛJ
JWKˆÝ›ÚÙNŽ›™]ÊKKÛÛÜŒÌŽŽ™œ›ÛWÜ™ØŠLL‹NL‹MŽ
JKˆ
NÂˆBˆZK˜YÜÜXÙJŒ
NÂˆØ[\WÙY]Ü—ØØ[˜\×ÜÙ[XÝ[ÛŠ	œ™\ÜÛœÙKœ˜[YWØÛÝ[Ù[XÝ[Û—Ù˜Y×Ø[˜ÚÜŠBŸB‚™›ˆØ[\WÙY]Ü—ØØ[˜\×ÜÙ[XÝ[ÛŠˆ™\ÜÛœÙNˆ	™YÝZNŽ”™\ÜÛœÙKˆœ˜[YWØÛÝ[ˆ\Ú^™KˆÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜŽˆ	›]]Ü[Û\Ú^™O‹ŠHOˆÜ[ÛÝŽ›ÜÎŽ”˜[™ÙO\Ú^™OˆÂˆYˆœ˜[YWØÛÝ[OHÂˆ
œÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜˆH›Û™NÂˆ™]\›ˆ›Û™NÂˆBˆ]Ú[\—Ùœ˜[YHH™\ÜÛœÙKš[\˜XÝÜÚ[\—ÜÜÊ
K›X\
Ú[\ŸÂˆ]ÚYH™\ÜÛœÙKœ™XÝÚY

K›X^
ŒÌŽŽ‘TÒSÓŠNÂˆ


Ú[\‹žH™\ÜÛœÙKœ™XÝ›Y

JK˜Û[\
ŒÚY
HÈÚY
H
ˆœ˜[YWØÛÝ[\ÈŒÌŠBˆ™›ÛÜŠ
H\È\Ú^™BˆJNÂˆYˆ™\ÜÛœÙK™˜Y×ÜÝ\Y

HÂˆ
œÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜˆHÚ[\—Ùœ˜[YK›X\
œ˜[Y_œ˜[YK›Z[Šœ˜[YWØÛÝ[HJJNÂˆBˆ]Ù[XÝ[ÛˆHYˆ™\ÜÛœÙK™˜YÙÙY

H™\ÜÛœÙK™˜Y×ÜÝÜY

HÂˆÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜ‚ˆžš\
Ú[\—Ùœ˜[YJBˆ˜[™Ý[Š
[˜ÚÜ‹Ý\œ™[
_ÂˆØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊ[˜ÚÜ‹Ý\œ™[œ˜[YWØÛÝ[
BˆJBˆH[ÙHÂˆ›Û™BˆNÂˆYˆ™\ÜÛœÙK™˜Y×ÜÝÜY

HÂˆ
œÙ[XÝ[Û—Ù˜Y×Ø[˜ÚÜˆH›Û™NÂˆBˆÙ[XÝ[Û‚ŸB‚™›ˆØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊˆ[˜ÚÜ—Ùœ˜[YNˆ\Ú^™KˆÝ\œ™[Ùœ˜[YNˆ\Ú^™Kˆœ˜[YWØÛÝ[ˆ\Ú^™KŠHOˆÜ[ÛÝŽ›ÜÎŽ”˜[™ÙO\Ú^™OˆÂˆYˆœ˜[YWØÛÝ[OHÂˆ™]\›ˆ›Û™NÂˆBˆ]Ý\H[˜ÚÜ—Ùœ˜[YK›Z[ŠÝ\œ™[Ùœ˜[YJK›Z[Šœ˜[YWØÛÝ[HJNÂˆ][™H[˜ÚÜ—Ùœ˜[YBˆ›X^
Ý\œ™[Ùœ˜[YJBˆ›Z[Šœ˜[YWØÛÝ[HJBˆœØ]\˜][™×ØY
JNÂˆÛÛYJÝ\‹™[™
BŸB‚™›ˆ˜]×Ø]Y[×ØÛ\ÝØ]™Y›Ü›JˆZ[\Žˆ	™YÝZNŽ”Z[\‹ˆÛ\Ü™XÝˆYÝZNŽ”™XÝˆÛ\ˆ	”^[\ÝÛ\ˆØ]™Y›Ü›Nˆ	]Y[ÕØ]™Y›Ü›Kˆ[ÙNˆ^[\ÝØ]™Y›Ü›S[ÙKˆNˆLM‹ˆ˜\ÙWÝ[\×ØœNˆŠHÂˆYˆØ]™Y›Ü›KœXZÜËš\×Ù[\J
HØ]™Y›Ü›K™œ˜[YWØÛÝ[OHØ]™Y›Ü›KœØ[\WÜ˜]HOHÂˆ™]\›ŽÂˆBˆ][Ùœ˜[YWØÛÝ[HØ]™Y›Ü›K™œ˜[YWØÛÝ[Âˆ]×Ùœ˜[YHHZ[\ÙXÛÛ™ÎˆOˆÜ[ÛMˆÂˆ]œ˜[YHHZ[\ÙXÛÛ™È
ˆŽ™œ›ÛJØ]™Y›Ü›KœØ[\WÜ˜]JHÈLŒÂˆ
œ˜[YKš\×Ùš[š]J
H	‰ˆœ˜[YHHŒ	‰ˆœ˜[YHHMŽ“PV\È
Bˆ[—ÜÛÛYJœ˜[YKœ›Ý[™

H\ÈM
BˆNÂˆ]ÛÝ\˜ÙWÝÚ[™ÝÈHYˆ]ÛÛYJ
Ý\ÝXÚË[™ÝXÚÊJHHÛ\œÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÊ
HÂˆYˆÝ\ÝXÚÈOH	‰ˆ[™ÝXÚÈOHÂˆÛÛYJ
[Ùœ˜[YWØÛÝ[
JBˆH[ÙHYˆHOHX˜\ÙWÝ[\×ØœKš\×Ùš[š]J
H˜\ÙWÝ[\×ØœHHŒÂˆ›Û™BˆH[ÙHÂˆ]XÚÜ×Ý×Ùœ˜[Y\ÈHXÚÜÎˆLÌŸÂˆ]œ˜[Y\ÈHŽ™œ›ÛJXÚÜÊH
ˆŒŒ
ˆŽ™œ›ÛJØ]™Y›Ü›KœØ[\WÜ˜]JBˆÈ
Ž™œ›ÛJJH
ˆ˜\ÙWÝ[\×ØœJNÂˆ
œ˜[Y\Ëš\×Ùš[š]J
H	‰ˆœ˜[Y\ÈHŒ	‰ˆœ˜[Y\ÈHMŽ“PV\È
Bˆ[—ÜÛÛYJœ˜[Y\Ëœ›Ý[™

H\ÈM
BˆNÂˆXÚÜ×Ý×Ùœ˜[Y\ÊÝ\ÝXÚÊBˆžš\
XÚÜ×Ý×Ùœ˜[Y\Ê[™ÝXÚÊJBˆ›X\

Ý\[™
_
Ý\›Z[Š[Ùœ˜[YWØÛÝ[
K[™›Z[Š[Ùœ˜[YWØÛÝ[
JJBˆ™š[\Š
Ý\[™
_Ý\[™
BˆBˆH[ÙHYˆÛ\\Ù\×Ù[ÜÛÝ\˜ÙWÝÚ[™ÝÊ
HÂˆÛÛYJ
[Ùœ˜[YWØÛÝ[
JBˆH[ÙHÂˆÛ\œÛÝ\˜ÙWÝÚ[™Ý×ÛZ[\ÙXÛÛ™Ê
Bˆ˜[™Ý[Š
Ý\Û\Ë[™Û\Ê_×Ùœ˜[YJÝ\Û\ÊKžš\
×Ùœ˜[YJ[™Û\ÊJJBˆ›X\

Ý\[™
_
Ý\›Z[Š[Ùœ˜[YWØÛÝ[
K[™›Z[Š[Ùœ˜[YWØÛÝ[
JJBˆ™š[\Š
Ý\[™
_Ý\[™
BˆNÂˆ]ÛÛYJ
ÛÝ\˜ÙWÜÝ\ÛÝ\˜ÙWÙ[™
JHHÛÝ\˜ÙWÝÚ[™ÝÈ[ÙHÂˆ™]\›ŽÂˆNÂˆYˆ[ÙHOH^[\ÝØ]™Y›Ü›S[ÙNŽ”ÜXÝ˜[Âˆ˜]×ÜÜXÝ˜[Ø]Y[×ØÛ\
Z[\‹Û\Ü™XÝØ]™Y›Ü›KÛÝ\˜ÙWÜÝ\ÛÝ\˜ÙWÙ[™
NÂˆ™]\›ŽÂˆBˆ]ÛÛ[[œÈHÛ\Ü™XÝÚY

K˜ÙZ[

K˜Û[\
KŒLŒ
H\È\Ú^™NÂˆ]œ˜[YWØÛÝ[HLLŽŽ™œ›ÛJ[Ùœ˜[YWØÛÝ[
NÂˆ]Ý\™[ÈH[ÙHOH^[\ÝØ]™Y›Ü›S[ÙNŽ”Ý\™[È	‰ˆØ]™Y›Ü›K˜Ú[›™[ÜXZÜË›[Š
HHŽÂˆ]\Ü^WØÚ[›™[ÈHYˆÝ\™[ÈÈˆH[ÙHÈHNÂˆYˆÝ\™[ÈÂˆ]Ù[\—ÞHHÛ\Ü™XÝ˜Ù[\Š
KžNÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠÛ\Ü™XÝ›Y

KÙ[\—ÞJKˆYÝZNŽœÜÌŠÛ\Ü™XÝœšYÚ

KÙ[\—ÞJKˆKˆÝ›ÚÙNŽ›™]ÊKÛÛÜŒÌŽŽ™œ›ÛWÜ™Ø˜WÝ[›][\YY
ŒŒŒLMJJKˆ
NÂˆBˆ›Üˆ\Ü^WØÚ[›™[[ˆ‹™\Ü^WØÚ[›™[ÈÂˆ]XZÜÈHYˆÝ\™[ÈÂˆ	Ø]™Y›Ü›K˜Ú[›™[ÜXZÜÖÙ\Ü^WØÚ[›™[BˆH[ÙHÂˆ	Ø]™Y›Ü›KœXZÜÂˆNÂˆYˆXZÜËš\×Ù[\J
HÂˆÛÛ[YNÂˆBˆ]XZ×ØÛÝ[HXZÜË›[Š
H\ÈLLŽÂˆ][™HHYˆÝ\™[ÈÂˆ][™WÚZYÚHÛ\Ü™XÝšZYÚ

H
ˆNÂˆYÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYÝZNŽœÜÌŠˆÛ\Ü™XÝ›Y

KˆÛ\Ü™XÝÜ

H
È\Ü^WØÚ[›™[\ÈŒÌˆ
ˆ[™WÚZYÚˆ
KˆYÝZNŽœÜÌŠˆÛ\Ü™XÝœšYÚ

KˆÛ\Ü™XÝÜ

H
È
\Ü^WØÚ[›™[
ÈJH\ÈŒÌˆ
ˆ[™WÚZYÚˆ
Kˆ
BˆH[ÙHÂˆÛ\Ü™XÝˆNÂˆ][\]YHH
[™KšZYÚ

H
ˆŠK›X^
KŒ
NÂˆ]Ù[\—ÞHH[™K˜Ù[\Š
KžNÂˆ]ÛÛÜˆHYˆÝ\™[È	‰ˆ\Ü^WØÚ[›™[OHHÂˆÛÛÜŒÌŽŽ™œ›ÛWÜ™Ø˜WÝ[›][\YY
ŒŒ‹ŒL‹MKMMJBˆH[ÙHÂˆÛÛÜŒÌŽŽ™œ›ÛWÜ™Ø˜WÝ[›][\YY
ŒKŒŒMKMMJBˆNÂˆ›ÜˆÛÛ[[ˆ[ˆ‹˜ÛÛ[[œÈÂˆ]œ˜[YWÜÝ\HLLŽŽ™œ›ÛJÛÝ\˜ÙWÜÝ\
Bˆ
ÈLLŽŽ™œ›ÛJÛÝ\˜ÙWÙ[™HÛÝ\˜ÙWÜÝ\
H
ˆÛÛ[[ˆ\ÈLLŽÈÛÛ[[œÈ\ÈLLŽÂˆ]œ˜[YWÙ[™HLLŽŽ™œ›ÛJÛÝ\˜ÙWÜÝ\
Bˆ
ÈLLŽŽ™œ›ÛJÛÝ\˜ÙWÙ[™HÛÝ\˜ÙWÜÝ\
H
ˆ
ÛÛ[[ˆ
ÈJH\ÈLLŽÈÛÛ[[œÈ\ÈLLŽÂˆ]š\œÝÜXZÈH
œ˜[YWÜÝ\
ˆXZ×ØÛÝ[Èœ˜[YWØÛÝ[
H\È\Ú^™NÂˆ]\ÝÜXZÈH

œ˜[YWÙ[™
ˆXZ×ØÛÝ[Èœ˜[YWØÛÝ[
H\È\Ú^™JBˆ›X^
š\œÝÜXZÈ
ÈJBˆ›Z[ŠXZÜË›[Š
JNÂˆ]]]Z[š[][HHŒÌŽŽ’S‘’S’UNÂˆ]]]X^[][HHŒÌŽŽ“‘Q×ÒS‘’S’UNÂˆ›ÜˆXZÈ[ˆ	œXZÜÖÙš\œÝÜXZË›Z[ŠXZÜË›[Š
HHJK‹›\ÝÜXZ×HÂˆZ[š[][HHZ[š[][K›Z[ŠXZË›Z[š[][JNÂˆX^[][HHX^[][K›X^
XZË›X^[][JNÂˆBˆYˆ[Z[š[][Kš\×Ùš[š]J
H[X^[][Kš\×Ùš[š]J
HÂˆÛÛ[YNÂˆBˆ]HÛ\Ü™XÝ›Y

H
È
ÛÛ[[ˆ\ÈŒÌˆ
ÈJH
ˆÛ\Ü™XÝÚY

HÈÛÛ[[œÈ\ÈŒÌŽÂˆZ[\‹›[™WÜÙYÛY[
ˆÂˆYÝZNŽœÜÌŠÙ[\—ÞHHX^[][H
ˆ[\]YJKˆYÝZNŽœÜÌŠÙ[\—ÞHHZ[š[][H
ˆ[\]YJKˆKˆÝ›ÚÙNŽ›™]ÊKŒÛÛÜŠKˆ
NÂˆBˆBŸB‚™›ˆ˜]×ÜÜXÝ˜[Ø]Y[×ØÛ\
ˆZ[\Žˆ	™YÝZNŽ”Z[\‹ˆÛ\Ü™XÝˆYÝZNŽ”™XÝˆØ]™Y›Ü›Nˆ	]Y[ÕØ]™Y›Ü›KˆÛÝ\˜ÙWÜÝ\ˆMˆÛÝ\˜ÙWÙ[™ˆMŠHÂˆYˆØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ëš\×Ù[\J
HÛÝ\˜ÙWÜÝ\HÛÝ\˜ÙWÙ[™Âˆ™]\›ŽÂˆBˆ]ÛÛ[[—ØÛÝ[HØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ë›[Š
NÂˆ]ÛÛYJš\œÝØ˜[™ÊHHØ]™Y›Ü›KœÜXÝ˜[Ø˜[™Ë™š\œÝ

H[ÙHÂˆ™]\›ŽÂˆNÂˆ]˜[™ØÛÝ[Hš\œÝØ˜[™Ë›[Š
NÂˆYˆ˜[™ØÛÝ[OHÂˆ™]\›ŽÂˆBˆ]Ý[Ùœ˜[Y\ÈHLLŽŽ™œ›ÛJØ]™Y›Ü›K™œ˜[YWØÛÝ[
NÂˆ]š\œÝØÛÛ[[ˆH
LLŽŽ™œ›ÛJÛÝ\˜ÙWÜÝ\
H
ˆÛÛ[[—ØÛÝ[\ÈLLŽÈÝ[Ùœ˜[Y\ÊH\È\Ú^™NÂˆ]\ÝØÛÛ[[ˆH

LLŽŽ™œ›ÛJÛÝ\˜ÙWÙ[™
H
ˆÛÛ[[—ØÛÝ[\ÈLLŽ
K™]—ØÙZ[
Ý[Ùœ˜[Y\ÊBˆ\È\Ú^™JBˆ˜Û[\
š\œÝØÛÛ[[ˆ
ÈKÛÛ[[—ØÛÝ[
NÂˆ]š\ÚX›WØÛÛ[[œÈH\ÝØÛÛ[[‹œØ]\˜][™×ÜÝXŠš\œÝØÛÛ[[ŠK›X^
JNÂˆ]ÛÝ\˜ÙWÜÜ[ˆHLLŽŽ™œ›ÛJÛÝ\˜ÙWÙ[™HÛÝ\˜ÙWÜÝ\
NÂˆ]˜[™ÚZYÚHÛ\Ü™XÝšZYÚ

HÈ˜[™ØÛÝ[\ÈŒÌŽÂ‚ˆ›ÜˆÛÛ[[—Ú[™^[ˆš\œÝØÛÛ[[‹›Z[ŠÛÛ[[—ØÛÝ[HJK‹›\ÝØÛÛ[[ˆÂˆ]Ù[\—Ùœ˜[YHH

ÛÛ[[—Ú[™^\È
ÈJH
ˆØ]™Y›Ü›K™œ˜[YWØÛÝ[\ÈˆÈÛÛ[[—ØÛÝ[\È
H\ÈMÂˆ]ÜÚ][ÛˆHLLŽŽ™œ›ÛJˆÙ[\—Ùœ˜[YBˆœØ]\˜][™×ÜÝXŠÛÝ\˜ÙWÜÝ\
Bˆ›Z[ŠÛÝ\˜ÙWÙ[™HJKˆ
NÂˆ]HÛ\Ü™XÝ›Y

H
ÈÜÚ][Ûˆ\ÈŒÌˆ
ˆÛ\Ü™XÝÚY

HÈÛÝ\˜ÙWÜÜ[ˆ\ÈŒÌŽÂˆ]ÚYH
Û\Ü™XÝÚY

HÈš\ÚX›WØÛÛ[[œÈ\ÈŒÌŠK›X^
KŒ
NÂˆ]YH
HÚY
ˆJK›X^
Û\Ü™XÝ›Y

JNÂˆ]šYÚH

ÈÚY
ˆJK›Z[ŠÛ\Ü™XÝœšYÚ

JNÂˆYˆšYÚHYÂˆÛÛ[YNÂˆBˆ›Üˆ˜[™[ˆ‹˜˜[™ØÛÝ[Âˆ][[œÚ]HHŒÌŽŽ™œ›ÛJØ]™Y›Ü›KœÜXÝ˜[Ø˜[™ÖØÛÛ[[—Ú[™^VØ˜[™JHÈMKŒÂˆYˆ[[œÚ]HŒˆÂˆÛÛ[YNÂˆBˆ]ÛÛÜˆHYˆ[[œÚ]HŒˆÂˆ›[™ØÛÛÜŠS‘SÑT’ËT”K[[œÚ]HÈŒŠBˆH[ÙHÂˆ›[™ØÛÛÜŠT”K“QK
[[œÚ]HHŒŠHÈŒÎ
BˆNÂˆ]ÜHÛ\Ü™XÝ˜›ÝÛJ
HH
˜[™
ÈJH\ÈŒÌˆ
ˆ˜[™ÚZYÚÂˆ]Ù[HYÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYÝZNŽœÜÌŠYÜ
KˆYÝZNŽœÜÌŠšYÚÜ
È˜[™ÚZYÚ
ÈJKˆ
NÂˆZ[\‹œ™XÝÙš[Y
Ù[YÝZNŽÛÜ›™\”˜Y]\ÎŽ–‘T“ËÛÛÜŠNÂˆBˆBŸB‚™›ˆ›[™ØÛÛÜŠÝ\ˆÛÛÜŒÌ‹[™ˆÛÛÜŒÌ‹[[Ý[ˆŒÌŠHOˆÛÛÜŒÌˆÂˆ][[Ý[H[[Ý[˜Û[\
ŒKŒ
NÂˆ]›[™Hš\œÝˆNÙXÛÛ™ˆNÂˆ
ŒÌŽŽ™œ›ÛJš\œÝ
H
È
ŒÌŽŽ™œ›ÛJÙXÛÛ™
HHŒÌŽŽ™œ›ÛJš\œÝ
JH
ˆ[[Ý[
Kœ›Ý[™

H\ÈNˆNÂˆÛÛÜŒÌŽŽ™œ›ÛWÜ™ØŠˆ›[™
Ý\œŠ
K[™œŠ
JKˆ›[™
Ý\™Ê
K[™™Ê
JKˆ›[™
Ý\˜Š
K[™˜Š
JKˆ
BŸB‚™›ˆÛ˜\Û›ÝWÝXÚÊ˜[YNˆM]X[[NˆLÌ‹Z[š[][NˆLÌŠHOˆLÌˆÂˆ]]X[[HHMŽ™œ›ÛJ]X[[K›X^
JJNÂˆ]˜[YHH˜[YK›X^

H\ÈMÂˆ]Û˜\YH˜[YBˆœØ]\˜][™×ØY
]X[[HÈŠBˆ˜ÚXÚÙYÙ]Š]X[[JBˆ[Ü˜\ÛÜŠ
BˆœØ]\˜][™×Û][
]X[[JBˆ˜Û[\
MŽ™œ›ÛJZ[š[][JKMŽ™œ›ÛJLÌŽŽ“PV
JNÂˆÛ˜\Y\ÈLÌ‚ŸB‚™›ˆX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWÜ˜[™ÙJˆÛÛ›Û\œÎˆ	–Ô]\›ÛÛ›Û\—KˆÝ™X[NˆÜ[Û
NN
O‹ŠHOˆ

HÂˆ]]]Z[š[][HHŽ’S‘’S’UNÂˆ]]]X^[][HHŽ“‘Q×ÒS‘’S’UNÂˆ›ÜˆÛÛ›Û\ˆ[ˆÛÛ›Û\œËš]\Š
K™š[\ŠÛÛ›Û\ŸÂˆÝ™X[Kš\×Û›Û™WÛÜŠ
Ú[›™[›YÜÊ_ÂˆÛÛ›Û\‹˜Ú[›™[OHÚ[›™[	‰ˆÛÛ›Û\‹™›YÜÈOH›YÜÂˆJBˆJHÂˆ]˜[YHHŽ™œ›ÛJÛÛ›Û\‹˜[YJ
JNÂˆYˆ˜[YKš\×Ùš[š]J
HÂˆZ[š[][HHZ[š[][K›Z[Š˜[YJNÂˆX^[][HHX^[][K›X^
˜[YJNÂˆBˆB‚ˆYˆ[Z[š[][Kš\×Ùš[š]J
H[X^[][Kš\×Ùš[š]J
HÂˆ™]\›ˆ
ŒKŒ
NÂˆBˆYˆZ[š[][HHŒ	‰ˆX^[][HHKŒÂˆ™]\›ˆ
ŒKŒ
NÂˆB‚ˆ]Y[™ÈH
X^[][HHZ[š[][JK›X^
KŒ
H
ˆŒÂˆ
ˆ
Z[š[][HHY[™ÊK›X^
YŽ™œ›ÛJŒÌŽŽ“PV
JKˆ
X^[][H
ÈY[™ÊK›Z[ŠŽ™œ›ÛJŒÌŽŽ“PV
JKˆ
BŸB‚™›ˆX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWØ]
›Ü›X[^™YˆŒÌ‹Z[š[][NˆX^[][Nˆ
HOˆŒÌˆÂˆ]›Ü›X[^™YHŽ™œ›ÛJ›Ü›X[^™Y˜Û[\
ŒKŒ
JNÂˆ]˜[YHHZ[š[][H
È›Ü›X[^™Y
ˆ
X^[][HHZ[š[][JNÂˆ˜[YK˜Û[\
YŽ™œ›ÛJŒÌŽŽ“PV
KŽ™œ›ÛJŒÌŽŽ“PV
JH\ÈŒÌ‚ŸB‚™›ˆX[›×Ü›ÛØÛÛ›Û\—ÜÝ›ÚÙWÜÚ[ÊˆÝ\ÝXÚÎˆLÌ‹ˆÝ\Ý˜[YNˆŒÌ‹ˆ[™ÝXÚÎˆLÌ‹ˆ[™Ý˜[YNˆŒÌ‹ˆXÚ×ÜÝ\ˆLÌ‹ŠHOˆ™XÏ
LÌ‹ŒÌŠOˆÂˆÛÛœÝPVÔÕ“ÒÑWÔÐSTTÎˆMHMŽÂ‚ˆYˆÝ\ÝXÚÈOH[™ÝXÚÈÂˆ™]\›ˆ™XÈVÊ[™ÝXÚË[™Ý˜[YJWNÂˆBˆ][HHMŽ™œ›ÛJ[™ÝXÚÊHHMŽ™œ›ÛJÝ\ÝXÚÊNÂˆ]\Ý[˜ÙHH[K[œÚYÛ™YØXœÊ
NÂˆ]XÚ×ÜÝ\HMŽ™œ›ÛJXÚ×ÜÝ\›X^
JJBˆ›X^
\Ý[˜ÙK™]—ØÙZ[
PVÔÕ“ÒÑWÔÐSTTÊJBˆ›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
JH\ÈMÂˆ]\™XÝ[ÛˆH[KœÚYÛ[J
NÂˆ]]]XÚÈHMŽ™œ›ÛJÝ\ÝXÚÊNÂˆ]Ý\Ý˜[YHHŽ™œ›ÛJÝ\Ý˜[YJNÂˆ][™Ý˜[YHHŽ™œ›ÛJ[™Ý˜[YJNÂˆ][›ÛZ[˜]ÜˆH[H\ÈÂˆ]Ø\XÚ]HH\Ý[˜ÙK™]—ØÙZ[
XÚ×ÜÝ\\ÈM
K›Z[ŠPVÔÕ“ÒÑWÔÐSTTÊH\È\Ú^™NÂˆ]]]Ú[ÈH™XÎŽÚ]ØØ\XÚ]JØ\XÚ]JNÂ‚ˆÚ[HXÚÈOHMŽ™œ›ÛJ[™ÝXÚÊHÂˆ]™[XZ[š[™ÈH
MŽ™œ›ÛJ[™ÝXÚÊHHXÚÊK[œÚYÛ™YØXœÊ
H\ÈMÂˆXÚÈ
ÏH\™XÝ[Ûˆ
ˆXÚ×ÜÝ\›Z[Š™[XZ[š[™ÊNÂˆ]œ˜XÝ[ÛˆH
XÚÈHMŽ™œ›ÛJÝ\ÝXÚÊJH\ÈÈ[›ÛZ[˜]ÜŽÂˆ]˜[YHH
Ý\Ý˜[YH
È
[™Ý˜[YHHÝ\Ý˜[YJH
ˆœ˜XÝ[ÛŠH\ÈŒÌŽÂˆÚ[Ëœ\Ú

XÚË˜Û[\
MŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌ‹˜[YJJNÂˆBˆÚ[ÂŸB‚™›ˆÛ˜\ÜÚYÛ™YÝXÚ×Ù[J˜[YNˆM]X[[NˆLÌŠHOˆMÂˆ]]X[[HHMŽ™œ›ÛJ]X[[K›X^
JJNÂˆ]XYÛš]YHH˜[YK[œÚYÛ™YØXœÊ
NÂˆ]Û˜\YHXYÛš]YBˆœØ]\˜][™×ØY
]X[[HÈŠBˆ˜ÚXÚÙYÙ]Š]X[[JBˆ[Ü˜\ÛÜŠ
BˆœØ]\˜][™×Û][
]X[[JBˆ›Z[ŠMŽ“PV\ÈM
H\ÈMÂˆYˆ˜[YHÈ\Û˜\YH[ÙHÈÛ˜\YBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[˜X›JˆYˆ	”^[\ÝÛ\ˆšYÚˆ	”^[\ÝÛ\ˆÚ[›™[ÚÚ[™Îˆ	•™YSX\LM‹Ü[ÛN‹ŠHOˆ›ÛÛÂˆ]
ˆ^[\ÝÛ\\™Ù]ŽÚ[›™[ÂˆYˆYØÚ[›™[ÚYˆKˆ^[\ÝÛ\\™Ù]ŽÚ[›™[ÂˆYˆšYÚØÚ[›™[ÚYˆKˆ
HH
Y\™Ù]

KšYÚ\™Ù]

JBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆYØÚ[›™[ÚYOHšYÚØÚ[›™[ÚYˆÚ[›™[ÚÚ[™Ë™Ù]
	›YØÚ[›™[ÚY
K˜ÛÜYY

K™›][Š
HOHÛÛYJ
BˆYœ˜]×Ý˜XÚ×Ú[™^OHšYÚœ˜]×Ý˜XÚ×Ú[™^ˆYœ™XÛÜ™ÜÚ^™HOHšYÚœ™XÛÜ™ÜÚ^™BˆYœ]\›—Ø˜\ÙHOHšYÚœ]\›—Ø˜\ÙBˆYš][WÚ[™^OHšYÚš][WÚ[™^ˆY˜XÚ×Ú[™^OHšYÚ˜XÚ×Ú[™^ˆY™Ü›Ý\OHšYÚ™Ü›Ý\ˆY[šÛ›ÝÛ—ÝÛÜ™OHšYÚ[šÛ›ÝÛ—ÝÛÜ™ˆYš][WÙ›YÜÈOHšYÚš][WÙ›YÜÂˆYšXY\—Øž]\ÈOHšYÚšXY\—Øž]\ÂˆYœØØ[HOHšYÚœØØ[BˆY˜Z[[™×Øž]\ÈOHšYÚ˜Z[[™×Øž]\ÂˆY›[™ÝÝXÚÜÈOHˆšYÚ›[™ÝÝXÚÜÈOHˆÂˆ™]\›ˆ˜[ÙNÂˆBˆYˆYš\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

HÂˆ]
ˆÛÛYJ
YÜÛÝ\˜ÙWÜÝ\YÜÛÝ\˜ÙWÙ[™
JKˆÛÛYJ
šYÚÜÛÝ\˜ÙWÜÝ\šYÚÜÛÝ\˜ÙWÙ[™
JKˆ
HH
YœÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÊ
KšYÚœÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÊ
JBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆYÜÛÝ\˜ÙWÙ[™˜ÚXÚÙYÜÝXŠYÜÛÝ\˜ÙWÜÝ\
HOHÛÛYJY›[™ÝÝXÚÜÊBˆšYÚÜÛÝ\˜ÙWÙ[™˜ÚXÚÙYÜÝXŠšYÚÜÛÝ\˜ÙWÜÝ\
HOHÛÛYJšYÚ›[™ÝÝXÚÜÊBˆYÜÛÝ\˜ÙWÙ[™OHšYÚÜÛÝ\˜ÙWÜÝ\ˆYœ™\Ù\™Y›[Š
HOHÌ‚ˆšYÚœ™\Ù\™Y›[Š
HOHÌ‚ˆYœ™\Ù\™YË‹ŽHOHšYÚœ™\Ù\™YË‹ŽBˆYœ™\Ù\™YÌM‹‹—HOHšYÚœ™\Ù\™YÌM‹‹—BˆÂˆ™]\›ˆ˜[ÙNÂˆBˆH[ÙHYˆYœ™\Ù\™YOHšYÚœ™\Ù\™YÂˆ™]\›ˆ˜[ÙNÂˆBˆYˆYˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
H
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]ÛÛYJYÙ[™ÝXÚÜÊHHYœÜÚ][Û—ÝXÚÜË˜ÚXÚÙYØY
Y›[™ÝÝXÚÜÊH[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆYÙ[™ÝXÚÜÈOHšYÚœÜÚ][Û—ÝXÚÜÂˆšYÚˆœÜÚ][Û—ÝXÚÜÂˆ˜ÚXÚÙYØY
šYÚ›[™ÝÝXÚÜÊBˆš\×Û›Û™J
BˆÂˆ™]\›ˆ˜[ÙNÂˆBˆYˆ[Yš\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

Bˆ	‰ˆ
[YœÝ\ÛÙ™œÙ]š\×Ùš[š]J
Bˆ[Y™[™ÛÙ™œÙ]š\×Ùš[š]J
Bˆ\šYÚœÝ\ÛÙ™œÙ]š\×Ùš[š]J
Bˆ\šYÚ™[™ÛÙ™œÙ]š\×Ùš[š]J
BˆYœÝ\ÛÙ™œÙ]ŒˆšYÚœÝ\ÛÙ™œÙ]ŒˆY™[™ÛÙ™œÙ]HYœÝ\ÛÙ™œÙ]ˆšYÚ™[™ÛÙ™œÙ]HšYÚœÝ\ÛÙ™œÙ]ˆY™[™ÛÙ™œÙ]×Øš]Ê
HOHšYÚœÝ\ÛÙ™œÙ]×Øš]Ê
JBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆYBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[—ØØ[™Y]\ÊˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™^ˆ\Ú^™KˆÚ[›™[ÚÚ[™Îˆ	•™YSX\LM‹Ü[ÛN‹ˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ
Ü[Û\Ú^™O‹Ü[Û\Ú^™OŠHÂˆ]ÛÛYJÛ\
HHÛ\Ë™Ù]
Û\Ú[™^
H[ÙHÂˆ™]\›ˆ
›Û™K›Û™JNÂˆNÂˆ]]]™]š[Ý\ÈHÛ\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ø[™Y]WÚ[™^Ø[™Y]J_Âˆ
˜Ø[™Y]WÚ[™^OHÛ\Ú[™^ˆ	‰ˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[˜X›JØ[™Y]KÛ\Ú[›™[ÚÚ[™ÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆØ[™Y]KœÜÚ][Û—ÝXÚÜËˆÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊKˆ
BˆJBˆ›X\

Ø[™Y]WÚ[™^Ê_Ø[™Y]WÚ[™^
NÂˆ]™]š[Ý\×ØÛ\Ú[™^H™]š[Ý\Ë›™^

NÂˆ]™]š[Ý\×ØÛ\Ú[™^HYˆ™]š[Ý\Ë›™^

Kš\×Û›Û™J
HÂˆ™]š[Ý\×ØÛ\Ú[™^ˆH[ÙHÂˆ›Û™BˆNÂ‚ˆ]]]™^HÛ\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ø[™Y]WÚ[™^Ø[™Y]J_Âˆ
˜Ø[™Y]WÚ[™^OHÛ\Ú[™^ˆ	‰ˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[˜X›JÛ\Ø[™Y]KÚ[›™[ÚÚ[™ÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆØ[™Y]BˆœÜÚ][Û—ÝXÚÜÂˆœØ]\˜][™×ØY
Ø[™Y]K›[™ÝÝXÚÜÊKˆ
BˆJBˆ›X\

Ø[™Y]WÚ[™^Ê_Ø[™Y]WÚ[™^
NÂˆ]™^ØÛ\Ú[™^H™^›™^

NÂˆ]™^ØÛ\Ú[™^HYˆ™^›™^

Kš\×Û›Û™J
HÂˆ™^ØÛ\Ú[™^ˆH[ÙHÂˆ›Û™BˆNÂ‚ˆ
™]š[Ý\×ØÛ\Ú[™^™^ØÛ\Ú[™^
BŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\ÜÙ[XÝ[Û—Ú›Ú[˜X›JˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™XÙ\Îˆ	–Ý\Ú^™WKˆÚ[›™[ÚÚ[™Îˆ	•™YSX\LM‹Ü[ÛN‹ˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ›ÛÛÂˆYˆÛ\Ú[™XÙ\Ë›[Š
HˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]]]Ù[XÝYØÛ\ÈHÛ\Ú[™XÙ\Âˆš]\Š
Bˆ™š[\—ÛX\
Û\Ú[™^Û\Ë™Ù]

˜Û\Ú[™^
K›X\
Û\

˜Û\Ú[™^Û\
JJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆYˆÙ[XÝYØÛ\Ë›[Š
HOHÛ\Ú[™XÙ\Ë›[Š
HÂˆ™]\›ˆ˜[ÙNÂˆBˆÙ[XÝYØÛ\ËœÛÜØžWÚÙ^J
ËÛ\
_Û\œÜÚ][Û—ÝXÚÜÊNÂ‚ˆÙ[XÝYØÛ\ËÚ[™ÝÜÊŠK˜[
Z\ŸÂˆ]ÊËY
K
ËšYÚ
WHHZ\ˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[˜X›JYšYÚÚ[›™[ÚÚ[™ÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆYœÜÚ][Û—ÝXÚÜËˆšYÚœÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
šYÚ›[™ÝÝXÚÜÊKˆ
BˆJBŸB‚œÝXÝ^[\Ý]Y[ÔÜ]ÛÛ^	ØOˆÂˆ[ÜÛÝ\˜ÙWÛ[™Ý×Û\Îˆ	‰ØH•™YSX\\Ú^™KÜ[ÛŒÌ‹ˆÜ]ÜÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆÚ[›™[ÚÚ[™Îˆ	‰ØH•™YSX\LM‹Ü[ÛN‹ˆ[\×ØÚ[›™[ÚYÎˆ	‰ØH•™YTÙ]LM‹ˆNˆLM‹ˆ[\×ØœNˆŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\ÜÙ[XÝ[Û—ÜÜ]ÜÝ\ÜY
ˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™XÙ\Îˆ	–Ý\Ú^™WKˆÛÛ^ˆ	”^[\Ý]Y[ÔÜ]ÛÛ^	×Ï‹ŠHOˆ›ÛÛÂˆÛ\Ú[™XÙ\Ë›[Š
HH‚ˆ	‰ˆÛ\Ú[™XÙ\Ëš]\Š
K˜[
Û\Ú[™^Âˆ]ÛÛYJÛ\
HHÛ\Ë™Ù]

˜Û\Ú[™^
H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ^[\ÝØ]Y[×ØÛ\ÜÜ]ÜÝ\ÜY

˜Û\Ú[™^Û\Û\ËÛÛ^
BˆJBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\Ú[™^ˆ\Ú^™KˆÛ\ˆ	”^[\ÝÛ\ˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛÛ^ˆ	”^[\Ý]Y[ÔÜ]ÛÛ^	×Ï‹ŠHOˆ›ÛÛÂˆ]^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHHÛ\\™Ù]

H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆÛÛ^˜Ú[›™[ÚÚ[™Ë™Ù]
	šY
K˜ÛÜYY

K™›][Š
HOHÛÛYJ
BˆÛ\ˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
HØØ[HHŒ
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]ÛÛYJ™[]]™WÜÜ]ÝXÚÜÊHHÛÛ^ˆœÜ]ÜÜÚ][Û—ÝXÚÜÂˆ˜ÚXÚÙYÜÝXŠÛ\œÜÚ][Û—ÝXÚÜÊBˆ™š[\ŠXÚÜß
XÚÜÈˆ	‰ˆ
XÚÜÈÛ\›[™ÝÝXÚÜÊBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆÛ\Ëš]\Š
K˜[žJØ[™Y]_Âˆ]^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHHØ[™Y]K\™Ù]

H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆÛÛ^[\×ØÚ[›™[ÚYË˜ÛÛZ[œÊ	šY
Bˆ	‰ˆØ[™Y]KœÜÚ][Û—ÝXÚÜÈHÛÛ^œÜ]ÜÜÚ][Û—ÝXÚÜÂˆ	‰ˆØ[™Y]BˆœÜÚ][Û—ÝXÚÜÂˆœØ]\˜][™×ØY
Ø[™Y]K›[™ÝÝXÚÜÊBˆˆÛ\œÜÚ][Û—ÝXÚÜÂˆJHÂˆ™]\›ˆ˜[ÙNÂˆB‚ˆYˆÛ\œ™XÛÜ™ÜÚ^™HOHÂˆ™]\›ˆÛ\ˆœÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÊ
Bˆš\×ÜÛÛYWØ[™

ÛÝ\˜ÙWÜÝ\ÛÝ\˜ÙWÙ[™
_ÂˆÛÝ\˜ÙWÙ[™ˆÛÝ\˜ÙWÜÝ\ˆ	‰ˆÛÝ\˜ÙWÙ[™˜ÚXÚÙYÜÝXŠÛÝ\˜ÙWÜÝ\
HOHÛÛYJÛ\›[™ÝÝXÚÜÊBˆ	‰ˆÛÝ\˜ÙWÜÝ\ˆ˜ÚXÚÙYØY
™[]]™WÜÜ]ÝXÚÜÊBˆš\×ÜÛÛYWØ[™
Ü]ÜÛÝ\˜Ù_ÂˆÜ]ÜÛÝ\˜ÙHˆÛÝ\˜ÙWÜÝ\	‰ˆÜ]ÜÛÝ\˜ÙHÛÝ\˜ÙWÙ[™ˆJBˆJNÂˆB‚ˆ]ÛÝ\˜ÙWÝÚ[™ÝÈHÛ\œÛÝ\˜ÙWÝÚ[™Ý×ÛZ[\ÙXÛÛ™Ê
K›Ü—Ù[ÙJÂˆÛ\\Ù\×Ù[ÜÛÝ\˜ÙWÝÚ[™ÝÊ
Bˆ[ŠÂˆÛÛ^ˆ™[ÜÛÝ\˜ÙWÛ[™Ý×Û\Âˆ™Ù]
	˜Û\Ú[™^
Bˆ˜ÛÜYY

Bˆ™›][Š
BˆJBˆ™›][Š
Bˆ™š[\Š[™ÝÛ\ß[™ÝÛ\Ëš\×Ùš[š]J
H	‰ˆ
›[™ÝÛ\ÈˆŒ
Bˆ›X\
[™ÝÛ\ß
ŒŽ™œ›ÛJ[™ÝÛ\ÊJJBˆJNÂˆ]ÛÛYJ
ÛÝ\˜ÙWÜÝ\Û\ËÛÝ\˜ÙWÙ[™Û\ÊJHHÛÝ\˜ÙWÝÚ[™ÝÈ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆÛÛ^œHOHXÛÛ^[\×ØœKš\×Ùš[š]J
HÛÛ^[\×ØœHHŒÂˆ™]\›ˆ˜[ÙNÂˆBˆ]Ü]ÜÛÝ\˜ÙWÛ\ÈH
ÛÝ\˜ÙWÜÝ\Û\Âˆ
ÈŽ™œ›ÛJ™[]]™WÜÜ]ÝXÚÜÊH
ˆŒÌŒÈ
Ž™œ›ÛJÛÛ^œJH
ˆÛÛ^[\×ØœJJBˆ\ÈŒÌŽÂˆÜ]ÜÛÝ\˜ÙWÛ\Ëš\×Ùš[š]J
Bˆ	‰ˆÜ]ÜÛÝ\˜ÙWÛ\ÈˆÛÝ\˜ÙWÜÝ\Û\È\ÈŒÌ‚ˆ	‰ˆÜ]ÜÛÝ\˜ÙWÛ\ÈÛÝ\˜ÙWÙ[™Û\È\ÈŒÌ‚ŸB‚™›ˆ^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\ˆ	”^[\ÝÛ\ˆÜ]ÜÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆÛ\Îˆ	–Ô^[\ÝÛ\Kˆ]\›œÎˆ	–Ô]\›—Kˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ›ÛÛÂˆ]^[\ÝÛ\\™Ù]Ž”]\›ˆÈYˆ]\›—ÚYHHÛ\\™Ù]

H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆXÛ\˜Ø[—Ü™\Ú^™WÝ[Y[[™WÛ[™Ý

BˆÛ\›[™ÝÝXÚÜÈOHˆÛ\ˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
H
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]ÛÛYJ™[]]™WÜÜ]ÝXÚÜÊHHÜ]ÜÜÚ][Û—ÝXÚÜÂˆ˜ÚXÚÙYÜÝXŠÛ\œÜÚ][Û—ÝXÚÜÊBˆ™š[\ŠXÚÜß
XÚÜÈˆ	‰ˆ
XÚÜÈÛ\›[™ÝÝXÚÜÊBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ]]]X]Ú[™×Ü]\›œÈH]\›œËš]\Š
K™š[\Š]\›Ÿ]\›‹šYOH]\›—ÚY
NÂˆ]ÛÛYJ]\›ŠHHX]Ú[™×Ü]\›œË›™^

H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆX]Ú[™×Ü]\›œË›™^

Kš\×ÜÛÛYJ
HÂˆ™]\›ˆ˜[ÙNÂˆBˆ][™™\œ™YÛ[™ÝH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK›[™Ýˆ
Bˆ›X\
›Ý_MŽ™œ›ÛJ›ÝKœÜÚ][ÛŠH
ÈMŽ™œ›ÛJ›ÝK›[™Ý
JBˆ›X^

Bˆ[Ü˜\ÛÜŠ
NÂˆ]ÛÛYJ™\X]Û[™Ý
HH]\›‚ˆ›[™ÝÝXÚÜÂˆ›X\
MŽ™œ›ÛJBˆ™š[\Š[™Ý
›[™Ýˆ
Bˆ›Ü—Ù[ÙJ
[™™\œ™YÛ[™Ýˆ
K[—ÜÛÛYJ[™™\œ™YÛ[™Ý
JBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆ]MŽ™œ›ÛJ™[]]™WÜÜ]ÝXÚÜÊKš\×Û][\WÛÙŠ™\X]Û[™Ý
Bˆ]\›‹››Ý\Ëš]\Š
K˜[žJ›Ý_ÂˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠHH™\X]Û[™Ýˆ
›ÝK›[™Ýˆˆ	‰ˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠH	H™\X]Û[™Ý
ÈMŽ™œ›ÛJ›ÝK›[™Ý
Bˆˆ™\X]Û[™Ý
BˆJBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]ÛÛYJÛ\Ù[™ÝXÚÜÊHHÛ\œÜÚ][Û—ÝXÚÜË˜ÚXÚÙYØY
Û\›[™ÝÝXÚÜÊH[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\Ù[™ÝXÚÜËˆ
BŸB‚™›ˆ^[\ÝÜ]\›—ØÛ\ÜÙ[XÝ[Û—ÜÜ]ÜÝ\ÜY
ˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™XÙ\Îˆ	–Ý\Ú^™WKˆÜ]ÜÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆ]\›œÎˆ	–Ô]\›—Kˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ›ÛÛÂˆÛ\Ú[™XÙ\Ë›[Š
HH‚ˆ	‰ˆÛ\Ú[™XÙ\Ëš]\Š
K˜[
Û\Ú[™^ÂˆÛ\Ë™Ù]

˜Û\Ú[™^
Kš\×ÜÛÛYWØ[™
Û\Âˆ^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\ˆÜ]ÜÜÚ][Û—ÝXÚÜËˆÛ\Ëˆ]\›œËˆ[\×ØÚ[›™[ÚYËˆ
BˆJBˆJBŸB‚™›ˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[˜X›JˆYˆ	”^[\ÝÛ\ˆšYÚˆ	”^[\ÝÛ\ˆ]\›œÎˆ	–Ô]\›—KŠHOˆ›ÛÛÂˆYˆYš\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

HšYÚš\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

HÂˆ™]\›ˆ˜[ÙNÂˆBˆ]
ˆ›Ü™XZ[Ž”^[\ÝÛ\\™Ù]Ž”]\›ˆÂˆYˆYÜ]\›—ÚYˆKˆ›Ü™XZ[Ž”^[\ÝÛ\\™Ù]Ž”]\›ˆÂˆYˆšYÚÜ]\›—ÚYˆKˆ
HH
Y\™Ù]

KšYÚ\™Ù]

JBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆYÜ]\›—ÚYOHšYÚÜ]\›—ÚYˆYœ˜]×Ý˜XÚ×Ú[™^OHšYÚœ˜]×Ý˜XÚ×Ú[™^ˆYœ™XÛÜ™ÜÚ^™HOHšYÚœ™XÛÜ™ÜÚ^™BˆYœ]\›—Ø˜\ÙHOHšYÚœ]\›—Ø˜\ÙBˆYš][WÚ[™^OHšYÚš][WÚ[™^ˆY˜XÚ×Ú[™^OHšYÚ˜XÚ×Ú[™^ˆY™Ü›Ý\OHšYÚ™Ü›Ý\ˆY[šÛ›ÝÛ—ÝÛÜ™OHšYÚ[šÛ›ÝÛ—ÝÛÜ™ˆYš][WÙ›YÜÈOHšYÚš][WÙ›YÜÂˆYšXY\—Øž]\ÈOHšYÚšXY\—Øž]\ÂˆYœÝ\ÛÙ™œÙ]×Øš]Ê
HOHšYÚœÝ\ÛÙ™œÙ]×Øš]Ê
BˆY™[™ÛÙ™œÙ]×Øš]Ê
HOHšYÚ™[™ÛÙ™œÙ]×Øš]Ê
BˆY˜Û\ÚYOHšYÚ˜Û\ÚYˆYœ™\Ù\™YOHšYÚœ™\Ù\™YˆYœØØ[K›X\
Ž×Øš]ÊHOHšYÚœØØ[K›X\
Ž×Øš]ÊBˆY˜Z[[™×Øž]\ÈOHšYÚ˜Z[[™×Øž]\ÂˆY›[™ÝÝXÚÜÈOHˆšYÚ›[™ÝÝXÚÜÈOHˆÂˆ™]\›ˆ˜[ÙNÂˆBˆYˆYˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
H
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]ÛÛYJYÙ[™ÝXÚÜÊHHYœÜÚ][Û—ÝXÚÜË˜ÚXÚÙYØY
Y›[™ÝÝXÚÜÊH[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆYˆYÙ[™ÝXÚÜÈOHšYÚœÜÚ][Û—ÝXÚÜÂˆšYÚˆœÜÚ][Û—ÝXÚÜÂˆ˜ÚXÚÙYØY
šYÚ›[™ÝÝXÚÜÊBˆš\×Û›Û™J
BˆÂˆ™]\›ˆ˜[ÙNÂˆB‚ˆ]ÛÛYJ]\›ŠHH]\›œÂˆš]\Š
Bˆ™š[™
]\›Ÿ]\›‹šYOHYÜ]\›—ÚY
Bˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ][™™\œ™YÛ[™ÝH]\›‚ˆ››Ý\Âˆš]\Š
Bˆ™š[\Š›Ý_›ÝK›[™Ýˆ
Bˆ›X\
›Ý_MŽ™œ›ÛJ›ÝKœÜÚ][ÛŠH
ÈMŽ™œ›ÛJ›ÝK›[™Ý
JBˆ›X^

Bˆ[Ü˜\ÛÜŠ
NÂˆ]ÛÛYJ™\X]Û[™Ý
HH]\›‚ˆ›[™ÝÝXÚÜÂˆ›X\
MŽ™œ›ÛJBˆ™š[\Š[™Ý
›[™Ýˆ
Bˆ›Ü—Ù[ÙJ
[™™\œ™YÛ[™Ýˆ
K[—ÜÛÛYJ[™™\œ™YÛ[™Ý
JBˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆMŽ™œ›ÛJY›[™ÝÝXÚÜÊKš\×Û][\WÛÙŠ™\X]Û[™Ý
Bˆ	‰ˆ\]\›‹››Ý\Ëš]\Š
K˜[žJ›Ý_ÂˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠHH™\X]Û[™Ýˆ
›ÝK›[™Ýˆˆ	‰ˆMŽ™œ›ÛJ›ÝKœÜÚ][ÛŠH	H™\X]Û[™Ý
ÈMŽ™œ›ÛJ›ÝK›[™Ý
Bˆˆ™\X]Û[™Ý
BˆJBŸB‚™›ˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\ÊˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™^ˆ\Ú^™Kˆ]\›œÎˆ	–Ô]\›—Kˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ
Ü[Û\Ú^™O‹Ü[Û\Ú^™OŠHÂˆ]ÛÛYJÛ\
HHÛ\Ë™Ù]
Û\Ú[™^
H[ÙHÂˆ™]\›ˆ
›Û™K›Û™JNÂˆNÂˆ]]]™]š[Ý\ÈHÛ\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ø[™Y]WÚ[™^Ø[™Y]J_Âˆ
˜Ø[™Y]WÚ[™^OHÛ\Ú[™^ˆ	‰ˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[˜X›JØ[™Y]KÛ\]\›œÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆØ[™Y]KœÜÚ][Û—ÝXÚÜËˆÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊKˆ
BˆJBˆ›X\

Ø[™Y]WÚ[™^Ê_Ø[™Y]WÚ[™^
NÂˆ]™]š[Ý\×ØÛ\Ú[™^H™]š[Ý\Ë›™^

NÂˆ]™]š[Ý\×ØÛ\Ú[™^HYˆ™]š[Ý\Ë›™^

Kš\×Û›Û™J
HÂˆ™]š[Ý\×ØÛ\Ú[™^ˆH[ÙHÂˆ›Û™BˆNÂ‚ˆ]]]™^HÛ\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\Š
Ø[™Y]WÚ[™^Ø[™Y]J_Âˆ
˜Ø[™Y]WÚ[™^OHÛ\Ú[™^ˆ	‰ˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[˜X›JÛ\Ø[™Y]K]\›œÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆØ[™Y]BˆœÜÚ][Û—ÝXÚÜÂˆœØ]\˜][™×ØY
Ø[™Y]K›[™ÝÝXÚÜÊKˆ
BˆJBˆ›X\

Ø[™Y]WÚ[™^Ê_Ø[™Y]WÚ[™^
NÂˆ]™^ØÛ\Ú[™^H™^›™^

NÂˆ]™^ØÛ\Ú[™^HYˆ™^›™^

Kš\×Û›Û™J
HÂˆ™^ØÛ\Ú[™^ˆH[ÙHÂˆ›Û™BˆNÂ‚ˆ
™]š[Ý\×ØÛ\Ú[™^™^ØÛ\Ú[™^
BŸB‚™›ˆ^[\ÝÜ]\›—ØÛ\ÜÙ[XÝ[Û—Ú›Ú[˜X›JˆÛ\Îˆ	–Ô^[\ÝÛ\KˆÛ\Ú[™XÙ\Îˆ	–Ý\Ú^™WKˆ]\›œÎˆ	–Ô]\›—Kˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ŠHOˆ›ÛÛÂˆYˆÛ\Ú[™XÙ\Ë›[Š
HˆÂˆ™]\›ˆ˜[ÙNÂˆBˆ]]]Ù[XÝYØÛ\ÈHÛ\Ú[™XÙ\Âˆš]\Š
Bˆ™š[\—ÛX\
Û\Ú[™^Û\Ë™Ù]

˜Û\Ú[™^
K›X\
Û\

˜Û\Ú[™^Û\
JJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆYˆÙ[XÝYØÛ\Ë›[Š
HOHÛ\Ú[™XÙ\Ë›[Š
HÂˆ™]\›ˆ˜[ÙNÂˆBˆÙ[XÝYØÛ\ËœÛÜØžWÚÙ^J
ËÛ\
_Û\œÜÚ][Û—ÝXÚÜÊNÂ‚ˆÙ[XÝYØÛ\ËÚ[™ÝÜÊŠK˜[
Z\ŸÂˆ]ÊËY
K
ËšYÚ
WHHZ\ˆ[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[˜X›JYšYÚ]\›œÊBˆ	‰ˆ\^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Ëˆ[\×ØÚ[›™[ÚYËˆYœÜÚ][Û—ÝXÚÜËˆšYÚœÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
šYÚ›[™ÝÝXÚÜÊKˆ
BˆJBŸB‚™›ˆ^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛ\Îˆ	–Ô^[\ÝÛ\Kˆ[\×ØÚ[›™[ÚYÎˆ	•™YTÙ]LM‹ˆÝ\ÝXÚÜÎˆLÌ‹ˆ[™ÝXÚÜÎˆLÌ‹ŠHOˆ›ÛÛÂˆÛ\Ëš]\Š
K˜[žJØ[™Y]_Âˆ]^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHHØ[™Y]K\™Ù]

H[ÙHÂˆ™]\›ˆ˜[ÙNÂˆNÂˆ[\×ØÚ[›™[ÚYË˜ÛÛZ[œÊ	šY
Bˆ	‰ˆØ[™Y]KœÜÚ][Û—ÝXÚÜÈ[™ÝXÚÜÂˆ	‰ˆØ[™Y]BˆœÜÚ][Û—ÝXÚÜÂˆœØ]\˜][™×ØY
Ø[™Y]K›[™ÝÝXÚÜÊBˆˆÝ\ÝXÚÜÂˆJBŸB‚™›ˆ^[\ÝØÛ\ÜØ[\WÛ[™ÝÛ\ÊˆÛ\ˆ	”^[\ÝÛ\ˆÚ[›™[ÚÚ[™Îˆ	•™YSX\LM‹Ü[ÛN‹ˆ]Y[×ÝØ]™Y›Ü›WÜ]Îˆ	•™YSX\LM‹]Y‹ˆ]Y[×ÝØ]™Y›Ü›\Îˆ	•™YSX\]Y‹\˜Ï]Y[ÕØ]™Y›Ü›O‹ŠHOˆÜ[ÛŒÌˆÂˆ]^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHHÛ\\™Ù]

H[ÙHÂˆ™]\›ˆ›Û™NÂˆNÂˆYˆÚ[›™[ÚÚ[™Ë™Ù]
	šY
K˜ÛÜYY

K™›][Š
HOHÛÛYJ
HÂˆ™]\›ˆ›Û™NÂˆBˆ]Y[×ÝØ]™Y›Ü›WÜ]Âˆ™Ù]
	šY
Bˆ˜[™Ý[Š]]Y[×ÝØ]™Y›Ü›\Ë™Ù]
]
JBˆ™š[\ŠØ]™Y›Ü›_Ø]™Y›Ü›KœØ[\WÜ˜]Hˆ
Bˆ›X\
Ø]™Y›Ü›_Âˆ
Ø]™Y›Ü›K™œ˜[YWØÛÝ[\È
ˆWÌŒÈŽ™œ›ÛJØ]™Y›Ü›KœØ[\WÜ˜]JJH\ÈŒÌ‚ˆJBˆ™š[\Š[™ÝÛ\ß[™ÝÛ\Ëš\×Ùš[š]J
H	‰ˆ
›[™ÝÛ\ÈˆŒ
BŸB‚™›ˆ^[\ÝØ]Y[×Ù\˜][Û—ÝXÚÜÊˆØ[\WÛ[™ÝÛ\ÎˆÜ[ÛŒÌ‹ˆNˆLM‹ˆ[\×ØœNˆŠHOˆÜ[ÛLÌˆÂˆYˆHOH][\×ØœKš\×Ùš[š]J
H[\×ØœHHŒÂˆ™]\›ˆ›Û™NÂˆBˆ]Ø[\WÛ[™ÝÛ\ÈHØ[\WÛ[™ÝÛ\ÏÎÂˆ]XÚÜÈHŽ™œ›ÛJØ[\WÛ[™ÝÛ\ÊH
ˆŽ™œ›ÛJJH
ˆ[\×ØœHÈŒÌŒÂˆ
XÚÜËš\×Ùš[š]J
H	‰ˆXÚÜÈˆŒ	‰ˆXÚÜÈHŽ™œ›ÛJLÌŽŽ“PV
JBˆ[—ÜÛÛYJXÚÜË™›ÛÜŠ
H\ÈLÌŠBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\Ü™\Ú^™WÝXÚÜÊˆÛ\ˆ	”^[\ÝÛ\ˆØ[\WÛ[™ÝÝXÚÜÎˆÜ[ÛLÌ‹ˆØØ[WÜÝ\ÜYˆ›ÛÛˆ[\×Ø]]ÛX][Û—ÛÝ™\›\×ØÛ\ˆ›ÛÛŠHOˆÜ[Û
LÌ‹LÌ‹LÌŠOˆÂˆYˆXÛ\š\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

H\ØØ[WÜÝ\ÜY[\×Ø]]ÛX][Û—ÛÝ™\›\×ØÛ\Âˆ™]\›ˆ›Û™NÂˆBˆ]
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚÊHHÛ\œÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÊ
OÎÂˆ]Ø[\WÛ[™ÝÝXÚÜÈHØ[\WÛ[™ÝÝXÚÜÏÎÂˆ
ÛÝ\˜ÙWÙ[™ÝXÚÈˆÛÝ\˜ÙWÜÝ\ÝXÚÂˆ	‰ˆÛÝ\˜ÙWÙ[™ÝXÚË˜ÚXÚÙYÜÝXŠÛÝ\˜ÙWÜÝ\ÝXÚÊHOHÛÛYJÛ\›[™ÝÝXÚÜÊBˆ	‰ˆÛÝ\˜ÙWÙ[™ÝXÚÈHØ[\WÛ[™ÝÝXÚÜÊBˆ[—ÜÛÛYJ
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚËØ[\WÛ[™ÝÝXÚÜÊJBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\Ü™\Ú^™WÝXÚÜ×Ùœ›ÛWÜ›Ú™XÝ
ˆÛ\ˆ	”^[\ÝÛ\ˆÛÛ^ˆ^[\Ý]Y[Ô™\Ú^™PÛÛ^	×Ï‹ŠHOˆÜ[Û
LÌ‹LÌ‹LÌŠOˆÂˆ]Ø[\WÛ[™ÝÛ\ÈH^[\ÝØÛ\ÜØ[\WÛ[™ÝÛ\ÊˆÛ\ˆÛÛ^˜Ú[›™[ÚÚ[™ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›WÜ]ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›\Ëˆ
NÂˆ]Ø[\WÛ[™ÝÝXÚÜÈBˆ^[\ÝØ]Y[×Ù\˜][Û—ÝXÚÜÊØ[\WÛ[™ÝÛ\ËÛÛ^œKÛÛ^[\×ØœJNÂˆ]ØØ[WÜÝ\ÜYHÛ\ˆœØØ[Bˆš\×Û›Û™WÛÜŠØØ[_ØØ[Kš\×Ùš[š]J
H	‰ˆØØ[HˆŒ	‰ˆ
ØØ[HHKŒ
K˜XœÊ
HHYKNJNÂˆ][\×Ø]]ÛX][Û—ÛÝ™\›\×ØÛ\H^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛÛ^˜\œ˜[™Ù[Y[ØÛ\ËˆÛÛ^[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊKˆ
NÂˆ^[\ÝØ]Y[×ØÛ\Ü™\Ú^™WÝXÚÜÊˆÛ\ˆØ[\WÛ[™ÝÝXÚÜËˆØØ[WÜÝ\ÜYˆ[\×Ø]]ÛX][Û—ÛÝ™\›\×ØÛ\ˆ
Bˆ™š[\ŠßÂˆX]Ú\ÈJÛ\\™Ù]

K^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHYˆÛÛ^˜Ú[›™[ÚÚ[™Ë™Ù]
	šY
K˜ÛÜYY

K™›][Š
HOHÛÛYJ
JBˆJBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\Ü™\Ú^™WÛZ[\ÙXÛÛ™×Ùœ›ÛWÜ›Ú™XÝ
ˆÛ\ˆ	”^[\ÝÛ\ˆÛÛ^ˆ^[\Ý]Y[Ô™\Ú^™PÛÛ^	×Ï‹ŠHOˆÜ[Û
ŒÌ‹ŒÌ‹ŒÌ‹ŒÌ‹ŒÌŠOˆÂˆYˆÛ\š\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

BˆÛ\›[™ÝÝXÚÜÈOHˆÛÛ^œHOHˆXÛÛ^[\×ØœKš\×Ùš[š]J
BˆÛÛ^[\×ØœHHŒˆÛ\ˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
HØØ[HHŒ
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆ^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛÛ^˜\œ˜[™Ù[Y[ØÛ\ËˆÛÛ^[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊKˆ
BˆÂˆ™]\›ˆ›Û™NÂˆB‚ˆ]Ø[\WÛ[™ÝÛ\ÈH^[\ÝØÛ\ÜØ[\WÛ[™ÝÛ\ÊˆÛ\ˆÛÛ^˜Ú[›™[ÚÚ[™ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›WÜ]ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›\Ëˆ
OÎÂˆ]
ÛÝ\˜ÙWÜÝ\Û\ËÛÝ\˜ÙWÙ[™Û\ÊHHÛ\ˆœÛÝ\˜ÙWÝÚ[™Ý×ÛZ[\ÙXÛÛ™Ê
Bˆ›X\

Ý\[™
_
Ý\\ÈŒÌ‹[™\ÈŒÌŠJBˆ›Ü—Ù[ÙJÂˆÛ\\Ù\×Ù[ÜÛÝ\˜ÙWÝÚ[™ÝÊ
Bˆ[—ÜÛÛYJ
ŒØ[\WÛ[™ÝÛ\ÊJBˆJOÎÂˆ]ÛÝ\˜ÙWÛ[™ÝÝXÚÜÈBˆŽ™œ›ÛJÛÝ\˜ÙWÙ[™Û\ÈHÛÝ\˜ÙWÜÝ\Û\ÊH
ˆŽ™œ›ÛJÛÛ^œJH
ˆÛÛ^[\×ØœBˆÈŒÌŒÂˆËÈHYØXÞHZ[\ÙXÛÛ™Ú[™ÝÈ]\ÝX\ÈHÛ\	ÜÈÝ\œ™[[Y[[™HÜ[ˆ]˜\ÙH[\Ë‚ˆYˆ\ÛÝ\˜ÙWÜÝ\Û\Ëš\×Ùš[š]J
Bˆ\ÛÝ\˜ÙWÙ[™Û\Ëš\×Ùš[š]J
BˆÛÝ\˜ÙWÜÝ\Û\ÈŒˆÛÝ\˜ÙWÙ[™Û\ÈHÛÝ\˜ÙWÜÝ\Û\ÂˆÛÝ\˜ÙWÙ[™Û\ÈˆØ[\WÛ[™ÝÛ\Âˆ\ÛÝ\˜ÙWÛ[™ÝÝXÚÜËš\×Ùš[š]J
BˆÛÝ\˜ÙWÛ[™ÝÝXÚÜÈHŒˆÛÝ\˜ÙWÛ[™ÝÝXÚÜÈˆŽ™œ›ÛJLÌŽŽ“PV
BˆÛÝ\˜ÙWÛ[™ÝÝXÚÜËœ›Ý[™

H\ÈLÌˆOHÛ\›[™ÝÝXÚÜÂˆÂˆ™]\›ˆ›Û™NÂˆB‚ˆÛÛYJ
ˆÛÝ\˜ÙWÜÝ\Û\ËˆÛÝ\˜ÙWÙ[™Û\ËˆØ[\WÛ[™ÝÛ\ËˆÛ\œÝ\ÛÙ™œÙ]ˆÛ\™[™ÛÙ™œÙ]ˆ
JBŸB‚™›ˆ^[\ÝØ]Y[×ØÛ\ÜÛ\ÝÚ[™Ý×Ùœ›ÛWÜ›Ú™XÝ
ˆÛ\ˆ	”^[\ÝÛ\ˆÛÛ^ˆ^[\Ý]Y[Ô™\Ú^™PÛÛ^	×Ï‹ŠHOˆÜ[Û^[\ÝÛ\Û\Ú[™ÝÏˆÂˆYˆÛ\š\×ÛÜ\]YWÜ™XÛÜ™ÝZ[

HÂˆ]
Ý\ÝXÚË[™ÝXÚËØ[\WÛ[™ÝÝXÚÜÊHBˆ^[\ÝØ]Y[×ØÛ\Ü™\Ú^™WÝXÚÜ×Ùœ›ÛWÜ›Ú™XÝ
Û\ÛÛ^
OÎÂˆ™]\›ˆ
[™ÝXÚËœØ]\˜][™×ÜÝXŠÝ\ÝXÚÊHØ[\WÛ[™ÝÝXÚÜÊK[—ÜÛÛYJˆ^[\ÝÛ\Û\Ú[™ÝÎŽ•XÚÜÈÂˆÝ\ÝXÚËˆ[™ÝXÚËˆØ[\WÛ[™ÝÝXÚÜËˆKˆ
NÂˆB‚ˆYˆÛÛ^œHOHˆXÛÛ^[\×ØœKš\×Ùš[š]J
BˆÛÛ^[\×ØœHHŒˆÛ\ˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
HØØ[HHŒ
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆ^[\ÝÝ[\×ÛÝ™\›\×Ü˜[™ÙJˆÛÛ^˜\œ˜[™Ù[Y[ØÛ\ËˆÛÛ^[\×ØÚ[›™[ÚYËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊKˆ
BˆÂˆ™]\›ˆ›Û™NÂˆB‚ˆ]Ø[\WÛ[™ÝÛ\ÈH^[\ÝØÛ\ÜØ[\WÛ[™ÝÛ\ÊˆÛ\ˆÛÛ^˜Ú[›™[ÚÚ[™ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›WÜ]ËˆÛÛ^˜]Y[×ÝØ]™Y›Ü›\Ëˆ
OÎÂˆ]
Ý\Û\Ë[™Û\ÊHHÛ\ˆœÛÝ\˜ÙWÝÚ[™Ý×ÛZ[\ÙXÛÛ™Ê
Bˆ›X\

Ý\[™
_
Ý\\ÈŒÌ‹[™\ÈŒÌŠJBˆ›Ü—Ù[ÙJÂˆÛ\\Ù\×Ù[ÜÛÝ\˜ÙWÝÚ[™ÝÊ
Bˆ[—ÜÛÛYJ
ŒØ[\WÛ[™ÝÛ\ÊJBˆJOÎÂˆ
Ý\Û\Ëš\×Ùš[š]J
Bˆ	‰ˆ[™Û\Ëš\×Ùš[š]J
Bˆ	‰ˆÝ\Û\ÈHŒˆ	‰ˆ[™Û\ÈˆÝ\Û\Âˆ	‰ˆ[™Û\ÈHØ[\WÛ[™ÝÛ\Âˆ	‰ˆ[™Û\ÈHÝ\Û\ÈØ[\WÛ[™ÝÛ\ÊBˆ[—ÜÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ“Z[\ÙXÛÛ™ÈÂˆÝ\Û\Ëˆ[™Û\ËˆØ[\WÛ[™ÝÛ\ËˆJBŸB‚™›ˆ^[\ÝØÛ\ÜÜ]ÜÜÚ][ÛŠˆÚ[\ŽˆYÝZNŽ”ÜÌ‹ˆÛ\Ü™XÝˆYÝZNŽ”™XÝˆÛ\ÜÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆÛ\Û[™ÝÝXÚÜÎˆLÌ‹ˆXÚ×ÜØØ[NˆŒÌ‹ˆNˆLM‹ˆÛ˜\ˆ›ÛÛŠHOˆÜ[ÛLÌˆÂˆYˆXÛ\Ü™XÝ˜ÛÛZ[œÊÚ[\ŠH]XÚ×ÜØØ[Kš\×Ùš[š]J
HXÚ×ÜØØ[HHŒÂˆ™]\›ˆ›Û™NÂˆBˆ]™[]]™WÝXÚÜÈH

Ú[\‹žHÛ\Ü™XÝ›Y

JHÈXÚ×ÜØØ[JKœ›Ý[™

H\ÈMÂˆ]ÜÚ][Û—ÝXÚÜÈHMŽ™œ›ÛJÛ\ÜÜÚ][Û—ÝXÚÜÊKœØ]\˜][™×ØY
™[]]™WÝXÚÜÊNÂˆ]Û˜\ÝXÚÜÈHYˆÛ˜\Âˆ
LÌŽŽ™œ›ÛJK›X^
JJHÈ
K›X^
JBˆH[ÙHÂˆBˆNÂˆ]ÜÚ][Û—ÝXÚÜÈHÛ˜\Û›ÝWÝXÚÊÜÚ][Û—ÝXÚÜËÛ˜\ÝXÚÜË
NÂˆ]Û\Ù[™ÝXÚÜÈHÛ\ÜÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\Û[™ÝÝXÚÜÊNÂˆ
ÜÚ][Û—ÝXÚÜÈˆÛ\ÜÜÚ][Û—ÝXÚÜÈ	‰ˆÜÚ][Û—ÝXÚÜÈÛ\Ù[™ÝXÚÜÊBˆ[—ÜÛÛYJÜÚ][Û—ÝXÚÜÊBŸB‚™›ˆ^[\ÝÛYX\Ý\™WØ›Ý[™\šY\ÊˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆX^ÝXÚÎˆLÌ‹ŠHOˆ™XÏ
LÌ‹M
OˆÂˆ]]]Ú[™Ù\ÈHÚYÛ˜]\™WØÚ[™Ù\Ë×Ý™XÊ
NÂˆÚ[™Ù\ËœÛÜØžWÚÙ^J
ÜÚ][Û‹ËÊ_
œÜÚ][ÛŠNÂˆ]]]ÚYÛ˜]\™HH[š]X[Ý[YWÜÚYÛ˜]\™K[Ü˜\ÛÜŠ

JNÂˆÚYÛ˜]\™KŒHÚYÛ˜]\™KŒ›X^
JNÂˆÚYÛ˜]\™KŒHHÚYÛ˜]\™KŒK›X^
JNÂˆ]]]›Ý[™\šY\ÈH™XÈVÊJWNÂˆ]]]ÙYÛY[ÜÝ\HLÌŽÂˆ]]]YX\Ý\™WÛ[X™\ˆH]MÂˆ]]]Ú[™ÙWÚ[™^H\Ú^™NÂ‚ˆÚ[HÚ[™Ù\Âˆ™Ù]
Ú[™ÙWÚ[™^
Bˆš\×ÜÛÛYWØ[™

ÜÚ][Û‹ËÊ_
œÜÚ][ÛˆOH
BˆÂˆ]
Ë[Y\˜]Ü‹[›ÛZ[˜]ÜŠHHÚ[™Ù\ÖØÚ[™ÙWÚ[™^NÂˆÚYÛ˜]\™HH
[Y\˜]Ü‹›X^
JK[›ÛZ[˜]Ü‹›X^
JJNÂˆÚ[™ÙWÚ[™^
ÏHNÂˆB‚ˆÚ[H]ÛÛYJ
ÜÚ][Û‹ËÊJHHÚ[™Ù\Ë™Ù]
Ú[™ÙWÚ[™^
K˜ÛÜYY

HÂˆYˆÜÚ][ÛˆˆX^ÝXÚÈÂˆœ™XZÎÂˆBˆYˆÜÚ][ÛˆˆÙYÛY[ÜÝ\Âˆ]˜\—ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JNÂˆ\[™Ü^[\ÝÛYX\Ý\™\Êˆ	›]]›Ý[™\šY\Ëˆ	›]]YX\Ý\™WÛ[X™\‹ˆÙYÛY[ÜÝ\ˆÜÚ][Û‹ˆ˜\—ÝXÚÜËˆ
NÂˆYˆ›Ý[™\šY\Ë›\Ý

Kš\×Û›Û™WÛÜŠ
XÚËÊ_
XÚÈÜÚ][ÛŠHÂˆYX\Ý\™WÛ[X™\ˆHYX\Ý\™WÛ[X™\‹œØ]\˜][™×ØY
JNÂˆ›Ý[™\šY\Ëœ\Ú

ÜÚ][Û‹YX\Ý\™WÛ[X™\ŠJNÂˆBˆÙYÛY[ÜÝ\HÜÚ][ÛŽÂˆBˆÚ[HÚ[™Ù\Âˆ™Ù]
Ú[™ÙWÚ[™^
Bˆš\×ÜÛÛYWØ[™

Ú[™ÙWÜÜÚ][Û‹ËÊ_
˜Ú[™ÙWÜÜÚ][ÛˆOHÜÚ][ÛŠBˆÂˆ]
Ë[Y\˜]Ü‹[›ÛZ[˜]ÜŠHHÚ[™Ù\ÖØÚ[™ÙWÚ[™^NÂˆÚYÛ˜]\™HH
[Y\˜]Ü‹›X^
JK[›ÛZ[˜]Ü‹›X^
JJNÂˆÚ[™ÙWÚ[™^
ÏHNÂˆBˆB‚ˆYˆÙYÛY[ÜÝ\X^ÝXÚÈÂˆ\[™Ü^[\ÝÛYX\Ý\™\Êˆ	›]]›Ý[™\šY\Ëˆ	›]]YX\Ý\™WÛ[X™\‹ˆÙYÛY[ÜÝ\ˆX^ÝXÚËˆ^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JKˆ
NÂˆBˆ›Ý[™\šY\ÂŸB‚™›ˆ^[\ÝØ˜\—ÝXÚÜÊNˆLM‹[YWÜÚYÛ˜]\™Nˆ
NN
JHOˆMÂˆ
MŽ™œ›ÛJK›X^
JJH
ˆMŽ™œ›ÛJ[YWÜÚYÛ˜]\™KŒ›X^
JJH
ˆˆÈMŽ™œ›ÛJ[YWÜÚYÛ˜]\™KŒK›X^
JJJBˆ›X^
JBŸB‚™›ˆ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJ˜XÚ×ÚYˆLÌ‹˜XÚÜÎˆ	–Ô^[\Ý˜XÚ×JHOˆÜ[Û
LÌ‹LÌŠOˆÂˆ]]]Ü™\™YÝ˜XÚÜÈH˜XÚÜËš]\Š
K˜ÛÛXÝŽ™XÏÏŠ
NÂˆÜ™\™YÝ˜XÚÜËœÛÜØžWÚÙ^J˜XÚß˜XÚËšY
NÂˆ]]]š\œÝÚ[™^HÜ™\™YÝ˜XÚÜÂˆš]\Š
BˆœÜÚ][ÛŠ˜XÚß˜XÚËšYOH˜XÚ×ÚY
OÎÂˆÚ[Hš\œÝÚ[™^ˆ	‰ˆÜ™\™YÝ˜XÚÜÖÙš\œÝÚ[™^K™Ü›Ý\YOHÛÛYJYJHÂˆš\œÝÚ[™^OHNÂˆBˆ]]]\ÝÚ[™^Hš\œÝÚ[™^ÂˆÚ[H\ÝÚ[™^
ÈHÜ™\™YÝ˜XÚÜË›[Š
Bˆ	‰ˆÜ™\™YÝ˜XÚÜÖÛ\ÝÚ[™^
ÈWK™Ü›Ý\YOHÛÛYJYJBˆÂˆ\ÝÚ[™^
ÏHNÂˆBˆÛÛYJ
ˆÜ™\™YÝ˜XÚÜÖÙš\œÝÚ[™^KšYˆÜ™\™YÝ˜XÚÜÖÛ\ÝÚ[™^KšYˆ
JBŸB‚™›ˆ^[\ÝÙÜ›Ý\Ü\™[ÚYÊ˜XÚÜÎˆ	–Ô^[\Ý˜XÚ×JHOˆ™XÏLÌˆÂˆ]]]Ü™\™YÝ˜XÚÜÈH˜XÚÜËš]\Š
K˜ÛÛXÝŽ™XÏÏŠ
NÂˆÜ™\™YÝ˜XÚÜËœÛÜØžWÚÙ^J˜XÚß˜XÚËšY
NÂˆÜ™\™YÝ˜XÚÜÂˆš[×Ú]\Š
Bˆ™š[\Š˜XÚß˜XÚË™Ü›Ý\YOHÛÛYJYJJBˆ™š[\—ÛX\
˜XÚßÂˆ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJ˜XÚËšY˜XÚÜÊBˆ™š[\Š
š\œÝ\Ý
_
™š\œÝOH˜XÚËšY	‰ˆš\œÝ\Ý
Bˆ›X\
ß˜XÚËšY
BˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ^[\ÝÝ˜XÚ×Ú\×ÚY[Šˆ˜XÚ×ÚYˆLÌ‹ˆ˜XÚÜÎˆ	–Ô^[\Ý˜XÚ×KˆÛÛ\ÙYÙÜ›Ý\ÚYÎˆ	•™YTÙ]LÌ‹ˆYWØÛÛ\ÙYÙÜ›Ý\Îˆ›ÛÛŠHOˆ›ÛÛÂˆYWØÛÛ\ÙYÙÜ›Ý\Âˆ	‰ˆ˜XÚÜÂˆš]\Š
Bˆ™š[™
˜XÚß˜XÚËšYOH˜XÚ×ÚY
Bˆš\×ÜÛÛYWØ[™
˜XÚß˜XÚË™Ü›Ý\YOHÛÛYJYJJBˆ	‰ˆ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJ˜XÚ×ÚY˜XÚÜÊBˆš\×ÜÛÛYWØ[™

\™[ÚYÊ_ÛÛ\ÙYÙÜ›Ý\ÚYË˜ÛÛZ[œÊ	œ\™[ÚY
JBŸB‚™›ˆ^[\ÝÜÙYZ×ÝXÚ×Ý×Ùœ˜[YJˆXÚÎˆLÌ‹ˆ[\×ÛX\ˆ	”^[\Ý[\ÓX\ˆØ[\WÜ˜]NˆLÌ‹ŠHOˆ™\Ý[MÝš[™ÏˆÂˆ[\×ÛX\™œ˜[YWØ]ÝXÚÊMŽ™œ›ÛJXÚÊKØ[\WÜ˜]JBŸB‚™›ˆš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚÊˆ\œ˜[™Ù[Y[ˆ	\œ˜[™Ù[Y[ˆÝ\ÝXÚÎˆLÌ‹ˆ[™ÝÝXÚÜÎˆLÌ‹ŠHOˆÜ[ÛLMˆÂˆ][™ÝXÚÈHÝ\ÝXÚË˜ÚXÚÙYØY
[™ÝÝXÚÜÊOÎÂˆ
‹MNJK™š[™
˜XÚ×Ú[™^Âˆ\œ˜[™Ù[Y[˜Û\Ëš]\Š
K˜[
Û\Âˆ]Û\Ù[™HÛ\œÜÚ][Û—ÝXÚÜËœØ]\˜][™×ØY
Û\›[™ÝÝXÚÜÊNÂˆ]Ý™\›\ÈHÛ\œÜÚ][Û—ÝXÚÜÈ[™ÝXÚÈ	‰ˆÝ\ÝXÚÈÛ\Ù[™Âˆ[Ý™\›\ÂˆÛ\ˆ˜XÚ×Ú[™^ˆš\×ÜÛÛYWØ[™
Û\Ý˜XÚßÛ\Ý˜XÚÈOH
˜XÚ×Ú[™^
BˆJBˆJBŸB‚™›ˆ›Ú™XÝÚ\×Ø]Y[×ØÛ\Ý[\]JØÝ[Y[ˆ	‘›ØÝ[Y[
HOˆ›ÛÛÂˆ]]Y[×ØÚ[›™[ÚYÈHØÝ[Y[ˆ˜Ú[›™[Ê
Bˆš[×Ú]\Š
Bˆ™š[\ŠÚ[›™[Ú[›™[šÚ[™

HOHÛÛYJ
H	‰ˆÚ[›™[œØ[\WÜ]

Kš\×ÜÛÛYJ
JBˆ›X\
Ú[›™[Ú[›™[šY

JBˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂˆYˆ]Y[×ØÚ[›™[ÚYËš\×Ù[\J
HÂˆ™]\›ˆ˜[ÙNÂˆBˆØÝ[Y[˜\œ˜[™Ù[Y[Ê
K›ÚÊ
Kš\×ÜÛÛYWØ[™
\œ˜[™Ù[Y[ßÂˆ\œ˜[™Ù[Y[Ëš]\Š
K˜[žJ\œ˜[™Ù[Y[Âˆ\œ˜[™Ù[Y[˜Û\Ëš]\Š
K˜[žJÛ\ÂˆX]Ú\ÈJÛ\\™Ù]

K^[\ÝÛ\\™Ù]ŽÚ[›™[ÈYHYˆ]Y[×ØÚ[›™[ÚYË˜ÛÛZ[œÊ	šY
JBˆJBˆJBˆJBŸB‚™›ˆ^[\ÝÙœ™Y^™WÜ]\›—ÜÛÝ\˜Ù\×ÜÝ\ÜY
ˆ]\›Žˆ	”]\›‹ˆÚ[›™[Îˆ	–ÐÚ[›™[Ý[[X\žWKˆØYYÝœÝ×ØÚ[›™[Îˆ	•™YTÙ]LM‹ŠHOˆ™\Ý[

KÝš[™ÏˆÂˆYˆ\]\›‹˜ÛÛ›Û\œËš\×Ù[\J
HÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆßHÛÛZ[œÈÛÛ›Û\ˆÚ[È]\È™[™\™\ˆÙ\È›Ý›ØÙ\ÜÈ‹ˆ]\›‹šYˆ
JNÂˆBˆ]Ú[›™[×ØžWÚYHÚ[›™[Âˆš]\Š
Bˆ›X\
Ú[›™[
Ú[›™[šY

KÚ[›™[
JBˆ˜ÛÛXÝŽ•™YSX\ËÏŠ
NÂˆ›ˆ˜[Y]WÛ^Y\—ØÚ[™[Šˆ]\›—ÚYˆLM‹ˆÙ^NˆLM‹ˆ^Y\—ÚYˆLM‹ˆÚ[›™[×ØžWÚYˆ	•™YSX\LM‹	Ú[›™[Ý[[X\žO‹ˆØYYÝœÝ×ØÚ[›™[Îˆ	•™YTÙ]LM‹ˆXÝ]™WÛ^Y\œÎˆ	›]]•™YTÙ]LM‹ˆ
HOˆ™\Ý[

KÝš[™ÏˆÂˆYˆXXÝ]™WÛ^Y\œËš[œÙ\
^Y\—ÚY
HÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆÜ]\›—ÚYHÛÛZ[œÈHÞXÛXÈ^Y\ˆÚ[™Y™\™[˜ÙH‚ˆ
JNÂˆBˆ]ÛÛYJ^Y\ŠHHÚ[›™[×ØžWÚY™Ù]
	›^Y\—ÚY
K˜ÛÜYY

H[ÙHÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆÜ]\›—ÚYH\Ù\ÈH^Y\ˆÚ]HZ\ÜÚ[™ÈÚ[Ú[›™[‚ˆ
JNÂˆNÂˆ]]]ÙY[ˆH•™YTÙ]Ž›™]Ê
NÂˆ›ÜˆÚ[ÚY[ˆ^Y\‚ˆ›^Y\—ØÚ[ÚYÊ
Bˆš[×Ú]\Š
Bˆ™›][Š
Bˆ˜ÛÜYY

Bˆ™š[\ŠÚ[ÚYÙY[‹š[œÙ\

˜Ú[ÚY
JBˆÂˆ]ÛÛYJÚ[
HHÚ[›™[×ØžWÚY™Ù]
	˜Ú[ÚY
K˜ÛÜYY

H[ÙHÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆÜ]\›—ÚYH\Ù\ÈH^Y\ˆÚ]HZ\ÜÚ[™ÈÚ[Ú[›™[‚ˆ
JNÂˆNÂˆ]Ú[ØXØÙ\×Û›ÝHHÚ[™[˜X›Y

HOHÛÛYJ˜[ÙJBˆ	‰ˆÚ[ˆšÙ^X›Ø\™ÚÙ^WÜ™YÚ[ÛŠ
Bˆš\×Û›Û™WÛÜŠ
ÝËYÚ
_
ÝË‹ZYÚ
K˜ÛÛZ[œÊ	šÙ^JJNÂˆYˆXÚ[ØXØÙ\×Û›ÝHÂˆÛÛ[YNÂˆBˆX]ÚÚ[˜Ú[›™[Ý\J
HÂˆÛÛYJÚ[›™[\NŽ“^Y\ŠHOˆ˜[Y]WÛ^Y\—ØÚ[™[Šˆ]\›—ÚYˆÙ^KˆÚ[ÚYˆÚ[›™[×ØžWÚYˆØYYÝœÝ×ØÚ[›™[ËˆXÝ]™WÛ^Y\œËˆ
OËˆÛÛYJÚ[›™[\NŽ”Ø[\\ŠHOˆßBˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HYˆØYYÝœÝ×ØÚ[›™[Ë˜ÛÛZ[œÊ	˜Ú[ÚY
HOˆßBˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HOˆÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆÜ]\›—ÚYH\Ù\È[ˆ[›ØYY[œÝ[Y[Ú[›™[‚ˆ
JNÂˆBˆÈOˆÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆÜ]\›—ÚYH\ÈH^Y\ˆÚ[]\È™[™\™\ˆØ[››Ý›ØÙ\ÜÈ‚ˆ
JNÂˆBˆBˆBˆXÝ]™WÛ^Y\œËœ™[[Ý™J	›^Y\—ÚY
NÂˆÚÊ

JBˆBˆ›Üˆ›ÝH[ˆ]\›‹››Ý\Ëš]\Š
K™š[\Š›Ý_[›ÝKš\×Û]]Y

JHÂˆ]ÛÛYJÛÝ\˜ÙJHHÚ[›™[×ØžWÚY™Ù]
	››ÝK˜Ú[›™[ÚY
K˜ÛÜYY

H[ÙHÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆßH\ÈH›ÝHÚ][ˆ[šÛ›ÝÛˆÛÝ\˜ÙHÚ[›™[‹ˆ]\›‹šYˆ
JNÂˆNÂˆ]ÛÝ\˜ÙWØXØÙ\×Û›ÝHHÛÝ\˜ÙK™[˜X›Y

HOHÛÛYJ˜[ÙJBˆ	‰ˆÛÝ\˜ÙBˆšÙ^X›Ø\™ÚÙ^WÜ™YÚ[ÛŠ
Bˆš\×Û›Û™WÛÜŠ
ÝËYÚ
_
ÝË‹ZYÚ
K˜ÛÛZ[œÊ	››ÝKšÙ^JJNÂˆYˆ\ÛÝ\˜ÙWØXØÙ\×Û›ÝHÂˆÛÛ[YNÂˆBˆX]ÚÛÝ\˜ÙK˜Ú[›™[Ý\J
HÂˆÛÛYJÚ[›™[\NŽ”Ø[\\ŠHOˆßBˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HYˆØYYÝœÝ×ØÚ[›™[Ë˜ÛÛZ[œÊ	œÛÝ\˜ÙKšY

JHOˆßBˆÛÛYJÚ[›™[\NŽ’[œÝ[Y[
HOˆÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆßH\Ù\È[ˆ[›ØYY[œÝ[Y[Ú[›™[‹ˆ]\›‹šYˆ
JNÂˆBˆÛÛYJÚ[›™[\NŽ“^Y\ŠHOˆÂˆ˜[Y]WÛ^Y\—ØÚ[™[Šˆ]\›‹šYˆ›ÝKšÙ^KˆÛÝ\˜ÙKšY

Kˆ	˜Ú[›™[×ØžWÚYˆØYYÝœÝ×ØÚ[›™[Ëˆ	›]]•™YTÙ]Ž›™]Ê
Kˆ
OÎÂˆBˆÈOˆÂˆ™]\›ˆ\œŠ›Ü›X]Jˆ”]\›ˆßH\Ù\ÈHÚ[›™[\H]\È™[™\™\ˆØ[››Ý›ØÙ\ÜÈ‹ˆ]\›‹šYˆ
JNÂˆBˆBˆBˆÚÊ

JBŸB‚™›ˆ^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—ÝXÚÜÊˆÚ[\—ÞˆŒÌ‹ˆÜšYÛYˆŒÌ‹ˆXÚ×ÜØØ[NˆŒÌ‹ˆNˆLM‹ˆÛ˜\ˆ›ÛÛŠHOˆLÌˆÂˆ]™[]]™WÝXÚÈH

Ú[\—ÞHÜšYÛY
K›X^
Œ
HÈXÚ×ÜØØ[K›X^
ŒÌŽŽ‘TÒSÓŠJBˆœ›Ý[™

Bˆ˜Û[\
ŒLÌŽŽ“PV\ÈŒÌŠH\ÈMÂˆ]Û˜\ÝXÚÜÈHYˆÛ˜\Âˆ
LÌŽŽ™œ›ÛJK›X^
JJHÈ
K›X^
JBˆH[ÙHÂˆBˆNÂˆÛ˜\Û›ÝWÝXÚÊ™[]]™WÝXÚËÛ˜\ÝXÚÜË
BŸB‚™›ˆ^[\ÝÝ˜XÚ×ÜÞ[˜×ÛX™[
Þ[˜Îˆ^[\Ý˜XÚÔÞ[˜ÊHOˆ	‰ÜÝ]XÈÝˆÂˆX]ÚÞ[˜ÈÂˆ^[\Ý˜XÚÔÞ[˜ÎŽ“Ù™ˆOˆ“Ù™ˆ‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ”]X\\™X]OˆŒKÍ™X]‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ’[™X]OˆŒKÌˆ™X]‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ™X]OˆŒH™X]‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ•ÛÐ™X]ÈOˆŒˆ™X]È‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ‘›Ý\™X]ÈOˆ™X]È‹ˆ^[\Ý˜XÚÔÞ[˜ÎŽ]]ÈOˆ]]È‹ˆBŸB‚™›ˆ^[\ÝÝ˜XÚ×Ü™\Ü×ÛX™[
™\ÜÎˆ^[\Ý˜XÚÔ™\ÜÊHOˆ	‰ÜÝ]XÈÝˆÂˆX]Ú™\ÜÈÂˆ^[\Ý˜XÚÔ™\ÜÎŽ”™]šYÙÙ\ˆOˆ”™]šYÙÙ\ˆ‹ˆ^[\Ý˜XÚÔ™\ÜÎŽ’ÛÝÜOˆ’Û	ˆÝÜ‹ˆ^[\Ý˜XÚÔ™\ÜÎŽ’Û[Ý[ÛˆOˆ’Û	ˆ[Ý[Ûˆ‹ˆ^[\Ý˜XÚÔ™\ÜÎŽ“]ÚOˆ“]Ú‹ˆBŸB‚™›ˆ^[\ÝÝ˜XÚ×Û[Ý[Û—ÛX™[
[Ý[ÛŽˆ^[\Ý˜XÚÓ[Ý[ÛŠHOˆ	‰ÜÝ]XÈÝˆÂˆX]Ú[Ý[ÛˆÂˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ”Ý^HOˆ”Ý^H‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ“Û™TÚÝOˆ“Û™HÚÝ‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ“X\˜ÚÜ˜\Oˆ“X\˜Ú	ˆÜ˜\‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ“X\˜ÚÝ^HOˆ“X\˜Ú	ˆÝ^H‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ“X\˜ÚÝÜOˆ“X\˜Ú	ˆÝÜ‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ”˜[™ÛHOˆ”˜[™ÛH‹ˆ^[\Ý˜XÚÓ[Ý[ÛŽŽ‘^Û\Ú]™T˜[™ÛHOˆ‘^Û\Ú]™H˜[™ÛH‹ˆBŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÞ›Û™WÜÝ\ÝXÚÊX\šÙ\œÎˆ	–Õ[YSX\šÙ\—JHOˆÜ[ÛLÌˆÂˆX\šÙ\œÂˆš]\Š
Bˆ™š[\ŠX\šÙ\ŸÂˆ[X\šÙ\‹š\×ÜÚYÛ˜]\™J
Bˆ	‰ˆX\šÙ\‚ˆ›˜[YJ
Bˆš\×ÜÛÛYWØ[™
˜[Y_˜[YK™\WÚYÛ›Ü™WØ\ØÚZWØØ\ÙJ”Ý\ŠJBˆJBˆ›X\
[YSX\šÙ\ŽŽœÜÚ][Û—ÝXÚÜÊBˆ›Z[Š
BŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÛ˜]šYØ][Û—Ý\™Ù]ÝXÚÊˆ\œ˜[™Ù[Y[ˆ	\œ˜[™Ù[Y[ˆÝ\œ™[ÝXÚÎˆLÌ‹ˆ›ÜØ\™ˆ›ÛÛˆÝ\ÎˆNˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKŠHOˆLÌˆÂˆ]]]X\šÙ\—ÜÜÚ][ÛœÈH\œ˜[™Ù[Y[ˆ[YWÛX\šÙ\œÂˆš]\Š
Bˆ™š[\ŠX\šÙ\Ÿ[X\šÙ\‹š\×ÜÚYÛ˜]\™J
JBˆ›X\
[YSX\šÙ\ŽŽœÜÚ][Û—ÝXÚÜÊBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆX\šÙ\—ÜÜÚ][ÛœËœÛÜÝ[œÝX›J
NÂˆX\šÙ\—ÜÜÚ][ÛœË™Y\

NÂˆYˆX\šÙ\—ÜÜÚ][ÛœÂˆ™š\œÝ

Bˆš\×ÜÛÛYWØ[™
ÜÚ][ÛŸ
œÜÚ][Ûˆˆ
BˆÂˆX\šÙ\—ÜÜÚ][ÛœËš[œÙ\

NÂˆB‚ˆ]]]\™Ù]ÝXÚÈHÝ\œ™[ÝXÚÎÂˆ›ÜˆÈ[ˆ‹œÝ\ÈÂˆ]™^ÝXÚÈHYˆ›ÜØ\™ÂˆX\šÙ\—ÜÜÚ][ÛœÂˆš]\Š
Bˆ˜ÛÜYY

Bˆ™š[™
ÜÚ][ÛŸ
œÜÚ][Ûˆˆ\™Ù]ÝXÚÊBˆ›Ü—Ù[ÙJÂˆ^[\ÝÛ™^Ø˜\—ÝXÚÊˆŽ™œ›ÛJ\™Ù]ÝXÚÊKˆKˆ[š]X[Ý[YWÜÚYÛ˜]\™KˆÚYÛ˜]\™WØÚ[™Ù\Ëˆ
BˆJBˆH[ÙHÂˆX\šÙ\—ÜÜÚ][ÛœÂˆš]\Š
Bˆ˜ÛÜYY

Bˆ™š[\ŠÜÚ][ÛŸ
œÜÚ][Ûˆ\™Ù]ÝXÚÊBˆ›X^

Bˆ›Ü—Ù[ÙJÂˆ
\™Ù]ÝXÚÈˆ
K[ŠÂˆ^[\ÝÛYX\Ý\™WØ]ÝXÚÊˆKˆ[š]X[Ý[YWÜÚYÛ˜]\™KˆÚYÛ˜]\™WØÚ[™Ù\Ëˆ\™Ù]ÝXÚËœØ]\˜][™×ÜÝXŠJKˆ
BˆŒˆJBˆJBˆNÂˆ]ÛÛYJ™^ÝXÚÊHH™^ÝXÚË™š[\ŠÜÚ][ÛŸÂˆYˆ›ÜØ\™Âˆ
œÜÚ][Ûˆˆ\™Ù]ÝXÚÂˆH[ÙHÂˆ
œÜÚ][Ûˆ\™Ù]ÝXÚÂˆBˆJH[ÙHÂˆœ™XZÎÂˆNÂˆ\™Ù]ÝXÚÈH™^ÝXÚÎÂˆBˆ\™Ù]ÝXÚÂŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÛZYWÜØÙ[™WÝXÚÊˆ\œ˜[™Ù[Y[ˆ	\œ˜[™Ù[Y[ˆØÙ[™WÚ[™^ˆNˆ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÎˆLÌ‹ˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKŠHOˆÜ[ÛLÌˆÂˆ]]]X\šÙ\—ÜÜÚ][ÛœÈH\œ˜[™Ù[Y[ˆ[YWÛX\šÙ\œÂˆš]\Š
Bˆ™š[\ŠX\šÙ\ŸÂˆ[X\šÙ\‹š\×ÜÚYÛ˜]\™J
H	‰ˆX\šÙ\‹œÜÚ][Û—ÝXÚÜÊ
H\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÂˆJBˆ›X\
[YSX\šÙ\ŽŽœÜÚ][Û—ÝXÚÜÊBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆX\šÙ\—ÜÜÚ][ÛœËœÛÜÝ[œÝX›J
NÂˆX\šÙ\—ÜÜÚ][ÛœË™Y\

NÂˆYˆ[X\šÙ\—ÜÜÚ][ÛœËš\×Ù[\J
HÂˆYˆX\šÙ\—ÜÜÚ][ÛœÖÌHˆÂˆX\šÙ\—ÜÜÚ][ÛœËš[œÙ\

NÂˆBˆ™]\›ˆX\šÙ\—ÜÜÚ][ÛœÂˆ™Ù]
\Ú^™NŽ™œ›ÛJØÙ[™WÚ[™^
JBˆ˜ÛÜYY

Bˆ™š[\ŠXÚß
XÚÈ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÊNÂˆB‚ˆ]]]ØÙ[™WÝXÚÈHÂˆ›ÜˆÈ[ˆ‹œØÙ[™WÚ[™^ÂˆØÙ[™WÝXÚÈH^[\ÝÛ™^Ø˜\—ÝXÚÊˆŽ™œ›ÛJØÙ[™WÝXÚÊKˆKˆ[š]X[Ý[YWÜÚYÛ˜]\™KˆÚYÛ˜]\™WØÚ[™Ù\Ëˆ
OÎÂˆBˆ
ØÙ[™WÝXÚÈ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÊK[—ÜÛÛYJØÙ[™WÝXÚÊBŸB‚ˆÖÙ\š]™JÛÛ™KÛÜKXYË\KÜ™\X[\K\X[Ü™
WB™[[H^[\Ý\™›Ü›X[˜ÙTØÙ[™RÙ^HÂˆX\šÙ\ŠLÌŠKˆÛ\Ü›Ý\
LMŠKŸB‚œÝXÝ^[\Ý\™›Ü›X[˜ÙTØÙ[™PÛÛ^	ØOˆÂˆ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÎˆLÌ‹ˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	‰ØHÊLÌ‹NN
WKˆÛ\ÙÜ›Ý\ˆÜ[ÛLM‹ˆ˜XÚ×ÙÜ›Ý\Ü˜[™ÙNˆÜ[Û
LÌ‹LÌŠO‹ŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÜØÙ[™WØÛ\Ú[™XÙ\Êˆ\œ˜[™Ù[Y[ˆ	\œ˜[™Ù[Y[ˆÝ\ÜYØÛ\Ú[™XÙ\Îˆ	•™YTÙ]\Ú^™O‹ˆØÙ[™WÝXÚÎˆLÌ‹ˆÛÛ^ˆ^[\Ý\™›Ü›X[˜ÙTØÙ[™PÛÛ^	×Ï‹ŠHOˆÜ[Û
^[\Ý\™›Ü›X[˜ÙTØÙ[™RÙ^K•™YSX\LÌ‹™XÏ\Ú^™OŠOˆÂˆYˆØÙ[™WÝXÚÈHÛÛ^œ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÈÂˆ™]\›ˆ›Û™NÂˆB‚ˆYˆ]ÛÛYJÜ›Ý\ÚY
HHÛÛ^˜Û\ÙÜ›Ý\Âˆ]]]Û\×ØžWÝ˜XÚÈH•™YSX\ŽLÌ‹™XÏ
LÌ‹\Ú^™JOŽŽ›™]Ê
NÂˆ›Üˆ	˜Û\Ú[™^[ˆÝ\ÜYØÛ\Ú[™XÙ\ÈÂˆ]ÛÛYJÛ\
HH\œ˜[™Ù[Y[˜Û\Ë™Ù]
Û\Ú[™^
H[ÙHÂˆÛÛ[YNÂˆNÂˆYˆÛ\™Ü›Ý\OHÜ›Ý\ÚYÂˆÛÛ[YNÂˆBˆ]ÛÛYJ˜XÚ×ÚY
HHÛ\œ^[\ÝÝ˜XÚ×ÚY

H[ÙHÂˆÛÛ[YNÂˆNÂˆÛ\×ØžWÝ˜XÚÂˆ™[žJ˜XÚ×ÚY
Bˆ›Ü—ÙY˜][

Bˆœ\Ú

Û\œÜÚ][Û—ÝXÚÜËÛ\Ú[™^
JNÂˆBˆ]Û\×ØžWÝ˜XÚÈHÛ\×ØžWÝ˜XÚÂˆš[×Ú]\Š
Bˆ›X\

˜XÚ×ÚY]]Û\Ê_ÂˆÛ\ËœÛÜÝ[œÝX›J
NÂˆ
ˆ˜XÚ×ÚYˆÛ\Âˆš[×Ú]\Š
Bˆ›X\

ËÛ\Ú[™^
_Û\Ú[™^
Bˆ˜ÛÛXÝ

Kˆ
BˆJBˆ˜ÛÛXÝ

NÂˆ™]\›ˆÛÛYJ
ˆ^[\Ý\™›Ü›X[˜ÙTØÙ[™RÙ^NŽÛ\Ü›Ý\
Ü›Ý\ÚY
KˆÛ\×ØžWÝ˜XÚËˆ
JNÂˆB‚ˆ]]]X\šÙ\—ÜÜÚ][ÛœÈH\œ˜[™Ù[Y[ˆ[YWÛX\šÙ\œÂˆš]\Š
Bˆ™š[\ŠX\šÙ\ŸÂˆ[X\šÙ\‹š\×ÜÚYÛ˜]\™J
H	‰ˆX\šÙ\‹œÜÚ][Û—ÝXÚÜÊ
HÛÛ^œ\™›Ü›X[˜ÙWÞ›Û™WÙ[™ÝXÚÂˆJBˆ›X\
[YSX\šÙ\ŽŽœÜÚ][Û—ÝXÚÜÊBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆX\šÙ\—ÜÜÚ][ÛœËœÛÜÝ[œÝX›J
NÂˆX\šÙ\—ÜÜÚ][ÛœË™Y\

NÂˆ]Ü›Ý\ÜÝ\ÝXÚÈHXÚÎˆLÌŸÂˆYˆX\šÙ\—ÜÜÚ][ÛœËš\×Ù[\J
HÂˆ^[\ÝÛYX\Ý\™WØ]ÝXÚÊˆÛÛ^œKˆÛÛ^š[š]X[Ý[YWÜÚYÛ˜]\™KˆÛÛ^œÚYÛ˜]\™WØÚ[™Ù\ËˆXÚËˆ
BˆŒˆH[ÙHÂˆX\šÙ\—ÜÜÚ][ÛœÂˆš]\Š
Bˆ˜ÛÜYY

BˆZÙWÝÚ[JÜÚ][ÛŸ
œÜÚ][ÛˆHXÚÊBˆ›\Ý

Bˆ[Ü˜\ÛÜŠ
BˆBˆNÂˆ]Ù[XÝYÙÜ›Ý\HÜ›Ý\ÜÝ\ÝXÚÊØÙ[™WÝXÚÊNÂˆ]]]Û\×ØžWÝ˜XÚÈH•™YSX\ŽLÌ‹™XÏ
LÌ‹\Ú^™JOŽŽ›™]Ê
NÂˆ›Üˆ	˜Û\Ú[™^[ˆÝ\ÜYØÛ\Ú[™XÙ\ÈÂˆ]ÛÛYJÛ\
HH\œ˜[™Ù[Y[˜Û\Ë™Ù]
Û\Ú[™^
H[ÙHÂˆÛÛ[YNÂˆNÂˆYˆÜ›Ý\ÜÝ\ÝXÚÊÛ\œÜÚ][Û—ÝXÚÜÊHOHÙ[XÝYÙÜ›Ý\ÂˆÛÛ[YNÂˆBˆ]ÛÛYJ˜XÚ×ÚY
HHÛ\œ^[\ÝÝ˜XÚ×ÚY

H[ÙHÂˆÛÛ[YNÂˆNÂˆYˆÛÛ^ˆ˜XÚ×ÙÜ›Ý\Ü˜[™ÙBˆš\×ÜÛÛYWØ[™

š\œÝ\Ý
_Jš\œÝ‹[\Ý
K˜ÛÛZ[œÊ	˜XÚ×ÚY
JBˆÂˆÛÛ[YNÂˆBˆÛ\×ØžWÝ˜XÚÂˆ™[žJ˜XÚ×ÚY
Bˆ›Ü—ÙY˜][

Bˆœ\Ú

Û\œÜÚ][Û—ÝXÚÜËÛ\Ú[™^
JNÂˆBˆÛÛYJ
ˆ^[\Ý\™›Ü›X[˜ÙTØÙ[™RÙ^NŽ“X\šÙ\ŠÙ[XÝYÙÜ›Ý\
KˆÛ\×ØžWÝ˜XÚÂˆš[×Ú]\Š
Bˆ›X\

˜XÚ×ÚY]]Û\Ê_ÂˆÛ\ËœÛÜÝ[œÝX›J
NÂˆ
ˆ˜XÚ×ÚYˆÛ\Âˆš[×Ú]\Š
Bˆ›X\

ËÛ\Ú[™^
_Û\Ú[™^
Bˆ˜ÛÛXÝ

Kˆ
BˆJBˆ˜ÛÛXÝ

Kˆ
JBŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÝšYÙÙ\—ÝXÚÊˆXÚÎˆˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆšYÙÙ\—ÜÞ[˜ÎˆÜ[Û^[\Ý˜XÚÔÞ[˜Ï‹ˆÛ\Û[™ÝÝXÚÜÎˆLÌ‹ŠHOˆÜ[ÛLÌˆÂˆ]ÛÛYJšYÙÙ\—ÜÞ[˜ÊHHšYÙÙ\—ÜÞ[˜È[ÙHÂˆ™]\›ˆ^[\ÝÛ™^Ø˜\—ÝXÚÊXÚËK[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ÊNÂˆNÂˆYˆ]XÚËš\×Ùš[š]J
HXÚÈŒXÚÈˆŽ™œ›ÛJLÌŽŽ“PV
HÂˆ™]\›ˆ›Û™NÂˆBˆ]Ý\œ™[ÝXÚÈHXÚË™›ÛÜŠ
H\ÈLÌŽÂˆ]ØY™WÝXÚÈHXÚË˜ÙZ[

H\ÈMÂˆYˆšYÙÙ\—ÜÞ[˜ÈOH^[\Ý˜XÚÔÞ[˜ÎŽ“Ù™ˆÂˆ™]\›ˆLÌŽŽžWÙœ›ÛJØY™WÝXÚÊK›ÚÊ
NÂˆB‚ˆ]
Ë[›ÛZ[˜]ÜŠHBˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊ[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÝ\œ™[ÝXÚÊNÂˆ]™X]ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊK
K[›ÛZ[˜]ÜŠJNÂˆ]›Ý\—Ø™X]ÈH™X]ÝXÚÜËœØ]\˜][™×Û][

K›X^
JNÂˆ][\˜[ÝXÚÜÈHX]ÚšYÙÙ\—ÜÞ[˜ÈÂˆ^[\Ý˜XÚÔÞ[˜ÎŽ“Ù™ˆOˆKˆ^[\Ý˜XÚÔÞ[˜ÎŽ”]X\\™X]Oˆ™X]ÝXÚÜË™]—ØÙZ[

Kˆ^[\Ý˜XÚÔÞ[˜ÎŽ’[™X]Oˆ™X]ÝXÚÜË™]—ØÙZ[
ŠKˆ^[\Ý˜XÚÔÞ[˜ÎŽ™X]Oˆ™X]ÝXÚÜËˆ^[\Ý˜XÚÔÞ[˜ÎŽ•ÛÐ™X]ÈOˆ™X]ÝXÚÜËœØ]\˜][™×Û][
ŠKˆ^[\Ý˜XÚÔÞ[˜ÎŽ‘›Ý\™X]ÈOˆ›Ý\—Ø™X]Ëˆ^[\Ý˜XÚÔÞ[˜ÎŽ]]ÈOˆMŽ™œ›ÛJÛ\Û[™ÝÝXÚÜÊK›Z[Š›Ý\—Ø™X]ÊK›X^
JKˆBˆ›X^
JNÂˆ]\ÙWÜÝ\HÚYÛ˜]\™WØÚ[™Ù\Âˆš]\Š
Bˆ™š[\Š
ÜÚ][Û‹ËÊ_
œÜÚ][ÛˆHÝ\œ™[ÝXÚÊBˆ›X\

ÜÚ][Û‹ËÊ_MŽ™œ›ÛJ
œÜÚ][ÛŠJBˆ›X^

Bˆ[Ü˜\ÛÜŠ
NÂˆ][\ÙYHØY™WÝXÚËœØ]\˜][™×ÜÝXŠ\ÙWÜÝ\
NÂˆ][\˜[ÈH[\ÙY™]—ØÙZ[
[\˜[ÝXÚÜÊNÂˆ]]]\™Ù]ÝXÚÈH\ÙWÜÝ\œØ]\˜][™×ØY
[\˜[ËœØ]\˜][™×Û][
[\˜[ÝXÚÜÊJNÂˆYˆ\™Ù]ÝXÚÈØY™WÝXÚÈÂˆ\™Ù]ÝXÚÈH\™Ù]ÝXÚËœØ]\˜][™×ØY
[\˜[ÝXÚÜÊNÂˆBˆYˆ]ÛÛYJ™^ÜÚYÛ˜]\™WÛX\šÙ\ŠHHÚYÛ˜]\™WØÚ[™Ù\Âˆš]\Š
Bˆ›X\

ÜÚ][Û‹ËÊ_MŽ™œ›ÛJ
œÜÚ][ÛŠJBˆ™š[\ŠÜÚ][ÛŸ
œÜÚ][ÛˆˆMŽ™œ›ÛJÝ\œ™[ÝXÚÊH	‰ˆ
œÜÚ][Ûˆ\™Ù]ÝXÚÊBˆ›Z[Š
BˆÂˆ\™Ù]ÝXÚÈH™^ÜÚYÛ˜]\™WÛX\šÙ\ŽÂˆBˆLÌŽŽžWÙœ›ÛJ\™Ù]ÝXÚÊK›ÚÊ
BŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÙÛØ˜[ÜÛ˜\ÝXÚÊˆXÚÎˆˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆÛØ˜[ÜÛ˜\ˆ\™›Ü›X[˜ÙQÛØ˜[Û˜\ŠHOˆÜ[ÛLÌˆÂˆYˆ]XÚËš\×Ùš[š]J
HXÚÈŒXÚÈˆŽ™œ›ÛJLÌŽŽ“PV
HÂˆ™]\›ˆ›Û™NÂˆBˆ]Ý\œ™[ÝXÚÈHXÚË™›ÛÜŠ
H\ÈLÌŽÂˆ]ØY™WÝXÚÈHXÚË˜ÙZ[

H\ÈMÂˆ]ÚYÛ˜]\™HBˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊ[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÝ\œ™[ÝXÚÊNÂˆ]ÛÛYJ[\˜[ÝXÚÜÊHHÛØ˜[ÜÛ˜\š[\˜[ÝXÚÜÊKÚYÛ˜]\™JH[ÙHÂˆ™]\›ˆLÌŽŽžWÙœ›ÛJØY™WÝXÚÊK›ÚÊ
NÂˆNÂˆ]\ÙWÜÝ\HÚYÛ˜]\™WØÚ[™Ù\Âˆš]\Š
Bˆ™š[\Š
ÜÚ][Û‹ËÊ_
œÜÚ][ÛˆHÝ\œ™[ÝXÚÊBˆ›X\

ÜÚ][Û‹ËÊ_MŽ™œ›ÛJ
œÜÚ][ÛŠJBˆ›X^

Bˆ[Ü˜\ÛÜŠ
NÂˆ][\ÙYHØY™WÝXÚËœØ]\˜][™×ÜÝXŠ\ÙWÜÝ\
NÂˆ][\˜[ÈH[\ÙY™]—ØÙZ[
[\˜[ÝXÚÜÊNÂˆ]]]\™Ù]ÝXÚÈH\ÙWÜÝ\œØ]\˜][™×ØY
[\˜[ËœØ]\˜][™×Û][
[\˜[ÝXÚÜÊJNÂˆYˆ\™Ù]ÝXÚÈØY™WÝXÚÈÂˆ\™Ù]ÝXÚÈH\™Ù]ÝXÚËœØ]\˜][™×ØY
[\˜[ÝXÚÜÊNÂˆBˆYˆ]ÛÛYJ™^ÜÚYÛ˜]\™WÛX\šÙ\ŠHHÚYÛ˜]\™WØÚ[™Ù\Âˆš]\Š
Bˆ›X\

ÜÚ][Û‹ËÊ_MŽ™œ›ÛJ
œÜÚ][ÛŠJBˆ™š[\ŠÜÚ][ÛŸ
œÜÚ][ÛˆˆMŽ™œ›ÛJÝ\œ™[ÝXÚÊH	‰ˆ
œÜÚ][Ûˆ\™Ù]ÝXÚÊBˆ›Z[Š
BˆÂˆ\™Ù]ÝXÚÈH™^ÜÚYÛ˜]\™WÛX\šÙ\ŽÂˆBˆLÌŽŽžWÙœ›ÛJ\™Ù]ÝXÚÊK›ÚÊ
BŸB‚™›ˆ^[\ÝÜ\™›Ü›X[˜ÙWÜÜÚ][Û—ÛÙ™œÙ]ÝXÚÜÊˆÛÛ™×ÝXÚÎˆLÌ‹ˆÛ\ÜÝ\ÝXÚÎˆLÌ‹ˆÛ\Û[™ÝÝXÚÜÎˆLÌ‹ˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆÜÚ][Û—ÜÞ[˜ÎˆÜ[Û^[\Ý˜XÚÔÞ[˜Ï‹ŠHOˆMÂˆ]Û\Û[™ÝHMŽ™œ›ÛJÛ\Û[™ÝÝXÚÜÊNÂˆYˆÛ\Û[™ÝOHÂˆ™]\›ˆÂˆBˆ]ÛÛYJÜÚ][Û—ÜÞ[˜ÊHHÜÚ][Û—ÜÞ[˜È[ÙHÂˆ™]\›ˆÂˆNÂˆ]Û\Ü\ÙHHMŽ™œ›ÛJÛÛ™×ÝXÚËœØ]\˜][™×ÜÝXŠÛ\ÜÝ\ÝXÚÊJH	HÛ\Û[™ÝÂˆYˆÜÚ][Û—ÜÞ[˜ÈOH^[\Ý˜XÚÔÞ[˜ÎŽ]]ÈÂˆ™]\›ˆÛ\Ü\ÙNÂˆBˆ]
˜\—ÜÝ\ÝXÚËÊHBˆ^[\ÝÛYX\Ý\™WØ]ÝXÚÊK[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÛÛ™×ÝXÚÊNÂˆ]ÚYÛ˜]\™HBˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊ[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÛÛ™×ÝXÚÊNÂˆ]
Ë[›ÛZ[˜]ÜŠHHÚYÛ˜]\™NÂˆ]˜\—ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JNÂˆ]™X]ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊK
K[›ÛZ[˜]ÜŠJNÂˆ][\˜[ÝXÚÜÈHX]ÚÜÚ][Û—ÜÞ[˜ÈÂˆ^[\Ý˜XÚÔÞ[˜ÎŽ“Ù™ˆOˆ™]\›ˆˆ^[\Ý˜XÚÔÞ[˜ÎŽ”]X\\™X]Oˆ™X]ÝXÚÜË™]—ØÙZ[

Kˆ^[\Ý˜XÚÔÞ[˜ÎŽ’[™X]Oˆ™X]ÝXÚÜË™]—ØÙZ[
ŠKˆ^[\Ý˜XÚÔÞ[˜ÎŽ™X]Oˆ™X]ÝXÚÜËˆ^[\Ý˜XÚÔÞ[˜ÎŽ•ÛÐ™X]ÈOˆ™X]ÝXÚÜËœØ]\˜][™×Û][
ŠKˆ^[\Ý˜XÚÔÞ[˜ÎŽ‘›Ý\™X]ÈOˆ™X]ÝXÚÜËœØ]\˜][™×Û][

Kˆ^[\Ý˜XÚÔÞ[˜ÎŽ]]ÈOˆ[œ™XXÚX›HJ]]È\È[™YX›Ý™HŠKˆBˆ›X^
JNÂˆ]˜\—Û[™ÝH˜\—ÝXÚÜË›X^
JNÂˆ]\ÙWÚ[—Ø˜\ˆHMŽ™œ›ÛJÛÛ™×ÝXÚËœØ]\˜][™×ÜÝXŠ˜\—ÜÝ\ÝXÚÊJNÂˆ]ÜšYØÛÝ[H˜\—Û[™Ý™]—ØÙZ[
[\˜[ÝXÚÜÊK›X^
JNÂˆ]ÝÙ\—Ú[™^H
\ÙWÚ[—Ø˜\ˆÈ[\˜[ÝXÚÜÊK›Z[ŠÜšYØÛÝ[HJNÂˆ]\\—Ú[™^H
ÝÙ\—Ú[™^
ÈJH	HÜšYØÛÝ[Âˆ]ÝÙ\—ÝXÚÈHÝÙ\—Ú[™^œØ]\˜][™×Û][
[\˜[ÝXÚÜÊNÂˆ]\\—ÝXÚÈH\\—Ú[™^œØ]\˜][™×Û][
[\˜[ÝXÚÜÊNÂˆ]ÝÙ\—Ù\Ý[˜ÙHH\ÙWÚ[—Ø˜\‹˜Xœ×ÙY™ŠÝÙ\—ÝXÚÊNÂˆ]\\—Ù\Ý[˜ÙHHYˆ\\—ÝXÚÈH\ÙWÚ[—Ø˜\ˆÂˆ\\—ÝXÚÈH\ÙWÚ[—Ø˜\‚ˆH[ÙHÂˆ˜\—Û[™ÝˆœØ]\˜][™×ÜÝXŠ\ÙWÚ[—Ø˜\ŠBˆœØ]\˜][™×ØY
\\—ÝXÚÊBˆNÂˆYˆ\\—Ù\Ý[˜ÙHÝÙ\—Ù\Ý[˜ÙHÂˆ\\—ÝXÚÈ	HÛ\Û[™ÝˆH[ÙHÂˆÝÙ\—ÝXÚÈ	HÛ\Û[™ÝˆBŸB‚™›ˆ^[\ÝÛ™^Ø˜\—ÝXÚÊˆXÚÎˆˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKŠHOˆÜ[ÛLÌˆÂˆYˆ]XÚËš\×Ùš[š]J
HXÚÈŒXÚÈˆŽ™œ›ÛJLÌŽŽ“PV
HÂˆ™]\›ˆ›Û™NÂˆBˆ]Ý\œ™[ÝXÚÈHXÚË™›ÛÜŠ
H\ÈLÌŽÂˆ]
˜\—ÜÝ\ÊHBˆ^[\ÝÛYX\Ý\™WØ]ÝXÚÊK[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÝ\œ™[ÝXÚÊNÂˆ]ÚYÛ˜]\™HBˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊ[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÝ\œ™[ÝXÚÊNÂˆ]™^Ø˜\ˆHMŽ™œ›ÛJ˜\—ÜÝ\
KœØ]\˜][™×ØY
^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JJNÂˆ]™^ÜÚYÛ˜]\™WÛX\šÙ\ˆHÚYÛ˜]\™WØÚ[™Ù\Âˆš]\Š
Bˆ›X\

ÜÚ][Û‹ËÊ_
œÜÚ][ÛŠBˆ™š[\ŠÜÚ][ÛŸ
œÜÚ][ÛˆˆÝ\œ™[ÝXÚÈ	‰ˆMŽ™œ›ÛJ
œÜÚ][ÛŠHH™^Ø˜\ŠBˆ›Z[Š
Bˆ›X\
MŽ™œ›ÛJNÂˆLÌŽŽžWÙœ›ÛJ™^ÜÚYÛ˜]\™WÛX\šÙ\‹[Ü˜\ÛÜŠ™^Ø˜\ŠJK›ÚÊ
BŸB‚™›ˆ^[\ÝÛYX\Ý\™WØ]ÝXÚÊˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆXÚÎˆLÌ‹ŠHOˆ
LÌ‹M
HÂˆ]]]Ú[™Ù\ÈHÚYÛ˜]\™WØÚ[™Ù\Ë×Ý™XÊ
NÂˆÚ[™Ù\ËœÛÜØžWÚÙ^J
ÜÚ][Û‹ËÊ_
œÜÚ][ÛŠNÂˆ]]]ÚYÛ˜]\™HH[š]X[Ý[YWÜÚYÛ˜]\™K[Ü˜\ÛÜŠ

JNÂˆÚYÛ˜]\™KŒHÚYÛ˜]\™KŒ›X^
JNÂˆÚYÛ˜]\™KŒHHÚYÛ˜]\™KŒK›X^
JNÂˆ]]]ÙYÛY[ÜÝ\HLÌŽÂˆ]]]YX\Ý\™WÛ[X™\ˆH]MÂˆ]]]Ú[™ÙWÚ[™^H\Ú^™NÂ‚ˆÚ[H]ÛÛYJ
ÜÚ][Û‹ËÊJHHÚ[™Ù\Ë™Ù]
Ú[™ÙWÚ[™^
K˜ÛÜYY

HÂˆYˆÜÚ][ÛˆˆXÚÈÂˆœ™XZÎÂˆBˆYˆÜÚ][ÛˆˆÙYÛY[ÜÝ\Âˆ]˜\—ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JNÂˆ][\ÙYÝXÚÜÈHMŽ™œ›ÛJÜÚ][ÛˆHÙYÛY[ÜÝ\
NÂˆ]ÛÛ\]YØ˜\œÈH[\ÙYÝXÚÜÈÈ˜\—ÝXÚÜÎÂˆYX\Ý\™WÛ[X™\ˆHYX\Ý\™WÛ[X™\‹œØ]\˜][™×ØY
ÛÛ\]YØ˜\œÊNÂˆ]\ÝØ›Ý[™\žHBˆMŽ™œ›ÛJÙYÛY[ÜÝ\
KœØ]\˜][™×ØY
ÛÛ\]YØ˜\œËœØ]\˜][™×Û][
˜\—ÝXÚÜÊJNÂˆYˆ\ÝØ›Ý[™\žHMŽ™œ›ÛJÜÚ][ÛŠHÂˆYX\Ý\™WÛ[X™\ˆHYX\Ý\™WÛ[X™\‹œØ]\˜][™×ØY
JNÂˆBˆÙYÛY[ÜÝ\HÜÚ][ÛŽÂˆBˆÚ[HÚ[™Ù\Âˆ™Ù]
Ú[™ÙWÚ[™^
Bˆš\×ÜÛÛYWØ[™

Ú[™ÙWÜÜÚ][Û‹ËÊ_
˜Ú[™ÙWÜÜÚ][ÛˆOHÜÚ][ÛŠBˆÂˆ]
Ë[Y\˜]Ü‹[›ÛZ[˜]ÜŠHHÚ[™Ù\ÖØÚ[™ÙWÚ[™^NÂˆÚYÛ˜]\™HH
[Y\˜]Ü‹›X^
JK[›ÛZ[˜]Ü‹›X^
JJNÂˆÚ[™ÙWÚ[™^
ÏHNÂˆBˆB‚ˆ]˜\—ÝXÚÜÈH^[\ÝØ˜\—ÝXÚÜÊKÚYÛ˜]\™JNÂˆ][\ÙYÝXÚÜÈHMŽ™œ›ÛJXÚÈHÙYÛY[ÜÝ\
NÂˆ]ÛÛ\]YØ˜\œÈH[\ÙYÝXÚÜÈÈ˜\—ÝXÚÜÎÂˆ]˜\—ÜÝ\HMŽ™œ›ÛJÙYÛY[ÜÝ\
KœØ]\˜][™×ØY
ÛÛ\]YØ˜\œÈ
ˆ˜\—ÝXÚÜÊNÂˆ
ˆ˜\—ÜÝ\›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌ‹ˆYX\Ý\™WÛ[X™\‹œØ]\˜][™×ØY
ÛÛ\]YØ˜\œÊKˆ
BŸB‚™›ˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
ˆXÚÎˆˆNˆLM‹ˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKŠHOˆÝš[™ÈÂˆ]XÚÈHYˆXÚËš\×Ùš[š]J
HÂˆXÚË˜Û[\
ŒŽ™œ›ÛJLÌŽŽ“PV
JBˆH[ÙHÂˆŒˆNÂˆ]ÜÚ][Û—ÝXÚÜÈHXÚË™›ÛÜŠ
H\ÈLÌŽÂˆ]
˜\—ÜÝ\˜\ŠHH^[\ÝÛYX\Ý\™WØ]ÝXÚÊˆKˆ[š]X[Ý[YWÜÚYÛ˜]\™KˆÚYÛ˜]\™WØÚ[™Ù\ËˆÜÚ][Û—ÝXÚÜËˆ
NÂˆ]
Ë[›ÛZ[˜]ÜŠHBˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊ[š]X[Ý[YWÜÚYÛ˜]\™KÚYÛ˜]\™WØÚ[™Ù\ËÜÚ][Û—ÝXÚÜÊNÂˆ]™X]ÝXÚÜÈHŽ™œ›ÛJK›X^
JJH
ˆŒÈŽ™œ›ÛJ[›ÛZ[˜]Ü‹›X^
JJNÂˆ]Ú][—Ø˜\ˆHXÚÈHŽ™œ›ÛJ˜\—ÜÝ\
NÂˆ]™X]H
Ú][—Ø˜\ˆÈ™X]ÝXÚÜÊK™›ÛÜŠ
H\ÈM
ÈNÂˆ]XÚ×Ú[—Ø™X]H
Ú][—Ø˜\ˆ	H™X]ÝXÚÜÊK™›ÛÜŠ
H\ÈMÂˆ›Ü›X]JžØ˜\ŸNžØ™X]ŒŸNžÝXÚ×Ú[—Ø™X]ŒßHŠBŸB‚™›ˆ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJˆœ˜[YNˆMˆØ[\WÜ˜]NˆLÌ‹ˆNˆLM‹ˆ[\×ØœNˆˆÛÜÛ[™ÝÝXÚÜÎˆMŠHOˆÜ[ÛˆÂˆYˆÛÜÛ[™ÝÝXÚÜÈOHÂˆ™]\›ˆ›Û™NÂˆBˆ][\×ÛX\H^[\Ý[\ÓX\Ž˜ÛÛœÝ[
K[\×ØœJK›ÚÊ
OÎÂˆ]ÛÜÛ[™ÝÙœ˜[Y\ÈH[\×ÛX\ˆ™œ˜[YWØ]ÝXÚÊÛÜÛ[™ÝÝXÚÜËØ[\WÜ˜]JBˆ›ÚÊ
OÂˆ›X^
JNÂˆ[\×ÛX\XÚ×Ø]Ùœ˜[YJœ˜[YH	HÛÜÛ[™ÝÙœ˜[Y\ËØ[\WÜ˜]JBŸB‚™›ˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊˆXÚÎˆMˆØ[\WÜ˜]NˆLÌ‹ˆNˆLM‹ˆ[\×ØœNˆˆÛÜÛ[™ÝÝXÚÜÎˆMŠHOˆÜ[ÛMˆÂˆYˆÛÜÛ[™ÝÝXÚÜÈOHÂˆ™]\›ˆ›Û™NÂˆBˆ][\×ÛX\H^[\Ý[\ÓX\Ž˜ÛÛœÝ[
K[\×ØœJK›ÚÊ
OÎÂˆ]ÛÜÛ[™ÝÙœ˜[Y\ÈH[\×ÛX\ˆ™œ˜[YWØ]ÝXÚÊÛÜÛ[™ÝÝXÚÜËØ[\WÜ˜]JBˆ›ÚÊ
OÂˆ›X^
JNÂˆ]XÚ×Ùœ˜[YHH[\×ÛX\ˆ™œ˜[YWØ]ÝXÚÊXÚÈ	HÛÜÛ[™ÝÝXÚÜËØ[\WÜ˜]JBˆ›ÚÊ
OÎÂˆÛÛYJXÚ×Ùœ˜[YH	HÛÜÛ[™ÝÙœ˜[Y\ÊBŸB‚™›ˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊˆ[š]X[Ý[YWÜÚYÛ˜]\™NˆÜ[Û
NN
O‹ˆÚYÛ˜]\™WØÚ[™Ù\Îˆ	–ÊLÌ‹NN
WKˆXÚÎˆLÌ‹ŠHOˆ
NN
HÂˆ]]]ÚYÛ˜]\™HH[š]X[Ý[YWÜÚYÛ˜]\™K[Ü˜\ÛÜŠ

JNÂˆÚYÛ˜]\™KŒHÚYÛ˜]\™KŒ›X^
JNÂˆÚYÛ˜]\™KŒHHÚYÛ˜]\™KŒK›X^
JNÂˆ]]]Ú[™Ù\ÈHÚYÛ˜]\™WØÚ[™Ù\Ë×Ý™XÊ
NÂˆÚ[™Ù\ËœÛÜØžWÚÙ^J
ÜÚ][Û‹ËÊ_
œÜÚ][ÛŠNÂˆ›Üˆ
ÜÚ][Û‹[Y\˜]Ü‹[›ÛZ[˜]ÜŠH[ˆÚ[™Ù\ÈÂˆYˆÜÚ][ÛˆˆXÚÈÂˆœ™XZÎÂˆBˆÚYÛ˜]\™HH
[Y\˜]Ü‹›X^
JK[›ÛZ[˜]Ü‹›X^
JJNÂˆBˆÚYÛ˜]\™BŸB‚™›ˆ\[™Ü^[\ÝÛYX\Ý\™\Êˆ›Ý[™\šY\Îˆ	›]]™XÏ
LÌ‹M
O‹ˆYX\Ý\™WÛ[X™\Žˆ	›]]MˆÙYÛY[ÜÝ\ˆLÌ‹ˆÙYÛY[Ù[™ˆLÌ‹ˆ˜\—ÝXÚÜÎˆMŠHÂˆ]]]XÚÈHMŽ™œ›ÛJÙYÛY[ÜÝ\
KœØ]\˜][™×ØY
˜\—ÝXÚÜË›X^
JJNÂˆÚ[HXÚÈHMŽ™œ›ÛJÙYÛY[Ù[™
HÂˆYˆ›Ý[™\šY\Âˆ›\Ý

Bˆš\×Û›Û™WÛÜŠ
\ÝÝXÚËÊ_
›\ÝÝXÚÈXÚÈ\ÈLÌŠBˆÂˆ
›YX\Ý\™WÛ[X™\ˆH

›YX\Ý\™WÛ[X™\ŠKœØ]\˜][™×ØY
JNÂˆ›Ý[™\šY\Ëœ\Ú

XÚÈ\ÈLÌ‹
›YX\Ý\™WÛ[X™\ŠJNÂˆBˆXÚÈHXÚËœØ]\˜][™×ØY
˜\—ÝXÚÜË›X^
JJNÂˆYˆXÚÈOHMŽ“PVÂˆœ™XZÎÂˆBˆBŸB‚™›ˆ^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YÎˆXÝ]™T^[\ÝÛ\˜YËˆÚ[\ŽˆYÝZNŽ”ÜÌ‹ˆXÚ×ÜØØ[NˆŒÌ‹ˆ›Ý×ÚZYÚˆŒÌ‹ˆNˆLM‹ˆÛ˜\ˆ›ÛÛˆX^Ý˜XÚ×Ú[™^ˆLM‹ŠHOˆ^[\ÝÛ\Y]Âˆ]XÚ×ÜØØ[HHXÚ×ÜØØ[K›X^
ŒÌŽŽ‘TÒSÓŠNÂˆ]XÚ×Ù[HH

Ú[\‹žH˜YËœÝ\ÜÚ[\‹ž
HÈXÚ×ÜØØ[JKœ›Ý[™

H\ÈMÂˆ]Û˜\ÝXÚÜÈHYˆÛ˜\Âˆ
LÌŽŽ™œ›ÛJK›X^
JJHÈ
K›X^
JBˆH[ÙHÂˆBˆNÂ‚ˆX]Ú˜YËšÚ[™Âˆ^[\ÝÛ\˜YÒÚ[™Ž“[Ý™HOˆÂˆ]˜XÚ×Ù[HHYˆ›Ý×ÚZYÚˆŒÂˆ

Ú[\‹žHH˜YËœÝ\ÜÚ[\‹žJHÈ›Ý×ÚZYÚ
Kœ›Ý[™

H\ÈLÌ‚ˆH[ÙHÂˆˆNÂˆ]X^Ý˜XÚ×Ú[™^HX^Ý˜XÚ×Ú[™^ˆ›Z[ŠNJBˆ›X^
˜YËœÝ\Ý˜XÚ×Ú[™^›Z[ŠNJJNÂˆ]˜XÚ×Ú[™^H
LÌŽŽ™œ›ÛJ˜YËœÝ\Ý˜XÚ×Ú[™^
H
È˜XÚ×Ù[JBˆ˜Û[\
LÌŽŽ™œ›ÛJX^Ý˜XÚ×Ú[™^
JH\ÈLMŽÂˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJÛ˜\Û›ÝWÝXÚÊˆMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊKœØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆˆ
JKˆ˜]×Ý˜XÚ×Ú[™^ˆÛÛYJNHH˜XÚ×Ú[™^
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆ^[\ÝÛ\˜YÒÚ[™Ž”™\Ú^™HOˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJÛ˜\Û›ÝWÝXÚÊˆMŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊKœØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆKˆ
JKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ^[\ÝÛ\˜YÒÚ[™Ž”™\Ú^™P]Y[ÕXÚÜÈÂˆYÙKˆÛÝ\˜ÙWÜÝ\ÝXÚËˆÛÝ\˜ÙWÙ[™ÝXÚËˆØ[\WÛ[™ÝÝXÚÜËˆHOˆX]ÚYÙHÂˆ^[\ÝÛ\™\Ú^™QYÙNŽ”šYÚOˆÂˆ]X^Û[™ÝÝXÚÜÈHØ[\WÛ[™ÝÝXÚÜËœØ]\˜][™×ÜÝXŠÛÝ\˜ÙWÜÝ\ÝXÚÊNÂˆ][™ÝÝXÚÜÈHÛ˜\Û›ÝWÝXÚÊˆMŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊKœØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆKˆ
Bˆ›Z[ŠX^Û[™ÝÝXÚÜÊNÂˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆÛÛYJ
ˆÛÝ\˜ÙWÜÝ\ÝXÚËˆÛÝ\˜ÙWÜÝ\ÝXÚËœØ]\˜][™×ØY
[™ÝÝXÚÜÊKˆ
JKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆ^[\ÝÛ\™\Ú^™QYÙNŽ“YOˆÂˆ]Ý\ÜÜÚ][ÛˆHMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ][™ÜÜÚ][ÛˆHÝ\ÜÜÚ][Ûˆ
ÈMŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ]Z[š[][WÜÜÚ][ÛˆHÚMˆ›X^
Ý\ÜÜÚ][ÛˆHMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊJBˆ›X^
[™ÜÜÚ][ÛˆHMŽ™œ›ÛJLÌŽŽ“PV
JNÂˆ]X^[][WÜÜÚ][ÛˆH
[™ÜÜÚ][ÛˆHJK›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
JNÂˆ]ÜÚ][Û—ÝXÚÜÈHMŽ™œ›ÛJÛ˜\Û›ÝWÝXÚÊˆÝ\ÜÜÚ][Û‹œØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆˆ
JBˆ˜Û[\
Z[š[][WÜÜÚ][Û‹X^[][WÜÜÚ][ÛŠNÂˆ]ÛÝ\˜ÙWÙ[HHÜÚ][Û—ÝXÚÜÈHÝ\ÜÜÚ][ÛŽÂˆ]ÛÝ\˜ÙWÜÝ\ÝXÚÈH
MŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊH
ÈÛÝ\˜ÙWÙ[JH\ÈLÌŽÂˆ][™ÝÝXÚÜÈH
[™ÜÜÚ][ÛˆHÜÚ][Û—ÝXÚÜÊH\ÈLÌŽÂˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJÜÚ][Û—ÝXÚÜÈ\ÈLÌŠKˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆÛÛYJ
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚÊJKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆKˆ^[\ÝÛ\˜YÒÚ[™Ž”™\Ú^™P]Y[ÓZ[\ÙXÛÛ™ÈÂˆYÙKˆÛÝ\˜ÙWÜÝ\Û\ËˆÛÝ\˜ÙWÙ[™Û\ËˆØ[\WÛ[™ÝÛ\ËˆÝÜ™YÜÝ\ÛÙ™œÙ]ˆÝÜ™YÙ[™ÛÙ™œÙ]ˆHOˆÂˆ]Z[\ÙXÛÛ™×Ü\—ÝXÚÈBˆŒÌŒÈ
Ž™œ›ÛJK›X^
JJH
ˆ˜YË[\×ØœK›X^
Ž“RS—ÔÔÒUU‘JJNÂˆX]ÚYÙHÂˆ^[\ÝÛ\™\Ú^™QYÙNŽ”šYÚOˆÂˆ]X^ÜØ[\WÛ[™ÝBˆ

Ž™œ›ÛJØ[\WÛ[™ÝÛ\ÊHHŽ™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊJBˆÈZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ™›ÛÜŠ
Bˆ˜Û[\
ŒŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌŽÂˆ]X^Û[™ÝHX^ÜØ[\WÛ[™Ý›X^
˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ][™ÝÝXÚÜÈHÛ˜\Û›ÝWÝXÚÊˆMŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊKœØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆKˆ
Bˆ›Z[ŠX^Û[™Ý
NÂˆYˆ[™ÝÝXÚÜÈOH˜YËœÝ\Û[™ÝÝXÚÜÈÂˆ™]\›ˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJ˜YËœÝ\Û[™ÝÝXÚÜÊKˆÝ\ÛÙ™œÙ]ˆÛÛYJÝÜ™YÜÝ\ÛÙ™œÙ]
Kˆ[™ÛÙ™œÙ]ˆÛÛYJÝÜ™YÙ[™ÛÙ™œÙ]
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆNÂˆBˆ][™ÛÙ™œÙ]H
Ž™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊBˆ
ÈŽ™œ›ÛJ[™ÝÝXÚÜÊH
ˆZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ›Z[ŠŽ™œ›ÛJØ[\WÛ[™ÝÛ\ÊJBˆ\ÈŒÌŽÂˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÝ\ÛÙ™œÙ]ˆÛÛYJÛÝ\˜ÙWÜÝ\Û\ÊKˆ[™ÛÙ™œÙ]ˆÛÛYJ[™ÛÙ™œÙ]
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆ^[\ÝÛ\™\Ú^™QYÙNŽ“YOˆÂˆ]™\]Y\ÝYÜÜÚ][ÛˆHMŽ™œ›ÛJÛ˜\Û›ÝWÝXÚÊˆMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊKœØ]\˜][™×ØY
XÚ×Ù[JKˆÛ˜\ÝXÚÜËˆˆ
JNÂˆ]™\]Y\ÝYÙ[HH™\]Y\ÝYÜÜÚ][ÛˆHMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]ÛÝ\˜ÙWÛZ[š[][WÙ[HH
YŽ™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊHÈZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ˜ÙZ[

Bˆ˜Û[\
MŽ“RSˆ\ÈMŽ“PV\È
Bˆ\ÈMÂˆ]Z[š[][WÙ[HH
ZMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊJBˆ›X^
MŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊHHMŽ™œ›ÛJLÌŽŽ“PV
JBˆ›X^
ÛÝ\˜ÙWÛZ[š[][WÙ[JNÂˆ]X^[][WÙ[HH
MŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊHHJBˆ›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
HHMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊJNÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂˆYˆ[HOHÂˆ™]\›ˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊKˆ[™ÝÝXÚÜÎˆÛÛYJ˜YËœÝ\Û[™ÝÝXÚÜÊKˆÝ\ÛÙ™œÙ]ˆÛÛYJÝÜ™YÜÝ\ÛÙ™œÙ]
Kˆ[™ÛÙ™œÙ]ˆÛÛYJÝÜ™YÙ[™ÛÙ™œÙ]
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆNÂˆBˆ]ÛÝ\˜ÙWÜÝ\Û\ÈBˆ
Ž™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊH
È[H\È
ˆZ[\ÙXÛÛ™×Ü\—ÝXÚÊH\ÈŒÌŽÂˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJ
MŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊH
È[JH\ÈLÌŠKˆ[™ÝÝXÚÜÎˆÛÛYJ
MŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊHH[JH\ÈLÌŠKˆÝ\ÛÙ™œÙ]ˆÛÛYJÛÝ\˜ÙWÜÝ\Û\Ë›X^
Œ
JKˆ[™ÛÙ™œÙ]ˆÛÛYJÛÝ\˜ÙWÙ[™Û\ÊKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆBˆBˆ^[\ÝÛ\˜YÒÚ[™Ž”Û\OˆÂˆ]XÚ×Ù[HHYˆÛ˜\ÂˆÛ˜\ÜÚYÛ™YÝXÚ×Ù[JXÚ×Ù[KÛ˜\ÝXÚÜÊBˆH[ÙHÂˆXÚ×Ù[BˆNÂˆ][\×ØœHH˜YË[\×ØœK›X^
Ž“RS—ÔÔÒUU‘JNÂˆ][WÛ\ÈHXÚ×Ù[H\È
ˆŒÌŒÈ
Ž™œ›ÛJK›X^
JJH
ˆ[\×ØœJNÂˆ]Ú[™Ý×Û[™ÝÛ\ÈH
˜YËœÝ\ÜÛÝ\˜ÙWÙ[™Û\ÈH˜YËœÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÊK›X^
Œ
NÂˆ]X^ÜÝ\Û\ÈH
˜YËœÛÝ\˜ÙWÛ[™ÝÛ\ÈHÚ[™Ý×Û[™ÝÛ\ÊK›X^
Œ
NÂˆ]Ý\ÛÙ™œÙ]Bˆ
˜YËœÝ\ÜÛÝ\˜ÙWÜÝ\Û\È
È[WÛ\È\ÈŒÌŠK˜Û[\
ŒX^ÜÝ\Û\ÊNÂˆ^[\ÝÛ\Y]ÂˆÝ\ÛÙ™œÙ]ˆÛÛYJÝ\ÛÙ™œÙ]
Kˆ[™ÛÙ™œÙ]ˆÛÛYJÝ\ÛÙ™œÙ]
ÈÚ[™Ý×Û[™ÝÛ\ÊKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆ^[\ÝÛ\˜YÒÚ[™Ž”Û\XÚÜÈÂˆÛÝ\˜ÙWÜÝ\ÝXÚËˆÛÝ\˜ÙWÙ[™ÝXÚËˆÛÝ\˜ÙWÛ[™ÝÝXÚÜËˆHOˆÂˆ]XÚ×Ù[HHYˆÛ˜\ÂˆÛ˜\ÜÚYÛ™YÝXÚ×Ù[JXÚ×Ù[KÛ˜\ÝXÚÜÊBˆH[ÙHÂˆXÚ×Ù[BˆNÂˆ]Ú[™Ý×Û[™ÝÝXÚÜÈHÛÝ\˜ÙWÙ[™ÝXÚËœØ]\˜][™×ÜÝXŠÛÝ\˜ÙWÜÝ\ÝXÚÊNÂˆ]X^ÜÝ\ÝXÚÈHÛÝ\˜ÙWÛ[™ÝÝXÚÜËœØ]\˜][™×ÜÝXŠÚ[™Ý×Û[™ÝÝXÚÜÊNÂˆ]™^ÜÝ\ÝXÚÈHMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊBˆœØ]\˜][™×ØY
XÚ×Ù[JBˆ˜Û[\
MŽ™œ›ÛJX^ÜÝ\ÝXÚÊJH\ÈLÌŽÂˆ]™^Ù[™ÝXÚÈH™^ÜÝ\ÝXÚËœØ]\˜][™×ØY
Ú[™Ý×Û[™ÝÝXÚÜÊNÂˆ^[\ÝÛ\Y]ÂˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆÛÛYJ
™^ÜÝ\ÝXÚË™^Ù[™ÝXÚÊJKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆBˆBˆBŸB‚™›ˆ^[\ÝØÛ\ÙÜ›Ý\Ù˜Y×ÙY]Êˆ˜YÎˆXÝ]™T^[\ÝÛ\˜YËˆ\™Ù]Îˆ	–Ô^[\ÝÛ\˜YÕ\™Ù]KˆÛÛ^ˆ^[\ÝÛ\˜YÐÛÛ^ŠHOˆ™XÏ
\Ú^™K^[\ÝÛ\Y]
OˆÂˆYˆ\™Ù]Ëš\×Ù[\J
HÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ][˜ÚÜ—ÙY]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆÛÛ^œÚ[\‹ˆÛÛ^XÚ×ÜØØ[KˆÛÛ^œ›Ý×ÚZYÚˆÛÛ^œKˆÛÛ^œÛ˜\ˆÛÛ^›X^Ý˜XÚ×Ú[™^ˆ
NÂˆ][˜ÚÜ—ÜÜÚ][ÛˆH[˜ÚÜ—ÙY]ˆœÜÚ][Û—ÝXÚÜÂˆ[Ü˜\ÛÜŠ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]™\]Y\ÝYÝXÚ×Ù[HHMŽ™œ›ÛJ[˜ÚÜ—ÜÜÚ][ÛŠHHMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]Z[—ÜÝ\ÜÜÚ][ÛˆH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊBˆ›Z[Š
Bˆ[Ü˜\ÛÜŠ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]X^ÜÝ\ÜÜÚ][ÛˆH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊBˆ›X^

Bˆ[Ü˜\ÛÜŠ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]XÚ×Ù[HH™\]Y\ÝYÝXÚ×Ù[K˜Û[\
ˆZMŽ™œ›ÛJZ[—ÜÝ\ÜÜÚ][ÛŠKˆMŽ™œ›ÛJLÌŽŽ“PVHX^ÜÝ\ÜÜÚ][ÛŠKˆ
NÂ‚ˆ][˜ÚÜ—Ý˜XÚÈH[˜ÚÜ—ÙY]ˆœ˜]×Ý˜XÚ×Ú[™^ˆ›X\
˜]×Ý˜XÚ×Ú[™^NHH˜]×Ý˜XÚ×Ú[™^
Bˆ[Ü˜\ÛÜŠ˜YËœÝ\Ý˜XÚ×Ú[™^
NÂˆ]™\]Y\ÝYÝ˜XÚ×Ù[HHLÌŽŽ™œ›ÛJ[˜ÚÜ—Ý˜XÚÊHHLÌŽŽ™œ›ÛJ˜YËœÝ\Ý˜XÚ×Ú[™^
NÂˆ]Z[—ÜÝ\Ý˜XÚÈH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\Ý˜XÚ×Ú[™^
Bˆ›Z[Š
Bˆ[Ü˜\ÛÜŠ˜YËœÝ\Ý˜XÚ×Ú[™^
NÂˆ]X^ÜÝ\Ý˜XÚÈH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\Ý˜XÚ×Ú[™^
Bˆ›X^

Bˆ[Ü˜\ÛÜŠ˜YËœÝ\Ý˜XÚ×Ú[™^
NÂˆ]X^Ý˜XÚ×Ú[™^HÛÛ^›X^Ý˜XÚ×Ú[™^›Z[ŠNJK›X^
X^ÜÝ\Ý˜XÚÊNÂˆ]˜XÚ×Ù[HH™\]Y\ÝYÝ˜XÚ×Ù[K˜Û[\
ˆZLÌŽŽ™œ›ÛJZ[—ÜÝ\Ý˜XÚÊKˆLÌŽŽ™œ›ÛJX^Ý˜XÚ×Ú[™^HX^ÜÝ\Ý˜XÚÊKˆ
NÂ‚ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÜÚ][Û—ÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊH
ÈXÚ×Ù[JH\ÈLÌŽÂˆ]˜XÚ×Ú[™^H
LÌŽŽ™œ›ÛJ\™Ù]œÝ\Ý˜XÚ×Ú[™^
H
È˜XÚ×Ù[JH\ÈLMŽÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJÜÚ][Û—ÝXÚÜÊKˆ˜]×Ý˜XÚ×Ú[™^ˆÛÛYJNHH˜XÚ×Ú[™^
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ^[\ÝØÛ\ÙÜ›Ý\Ü™\Ú^™WÙY]Êˆ˜YÎˆXÝ]™T^[\ÝÛ\˜YËˆ\™Ù]Îˆ	–Ô^[\ÝÛ\˜YÕ\™Ù]KˆÛÛ^ˆ^[\ÝÛ\˜YÐÛÛ^ŠHOˆ™XÏ
\Ú^™K^[\ÝÛ\Y]
OˆÂˆYˆ\™Ù]Ëš\×Ù[\J
HÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ][˜ÚÜ—ÙY]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆÛÛ^œÚ[\‹ˆÛÛ^XÚ×ÜØØ[KˆÛÛ^œ›Ý×ÚZYÚˆÛÛ^œKˆÛÛ^œÛ˜\ˆÛÛ^›X^Ý˜XÚ×Ú[™^ˆ
NÂˆ][˜ÚÜ—Û[™ÝH[˜ÚÜ—ÙY]›[™ÝÝXÚÜË[Ü˜\ÛÜŠ˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ]™\]Y\ÝYÙ[HHMŽ™œ›ÛJ[˜ÚÜ—Û[™Ý
HHMŽ™œ›ÛJ˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ]Z[š[][WÛ[™ÝH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\Û[™ÝÝXÚÜÊBˆ›Z[Š
Bˆ[Ü˜\ÛÜŠ˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ]X^[][WÛ[™ÝH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]\™Ù]œÝ\Û[™ÝÝXÚÜÊBˆ›X^

Bˆ[Ü˜\ÛÜŠ˜YËœÝ\Û[™ÝÝXÚÜÊNÂˆ]Z[š[][WÙ[HHHHMŽ™œ›ÛJZ[š[][WÛ[™Ý
NÂˆ]Z[\ÙXÛÛ™×Ü\—ÝXÚÈBˆŒÌŒÈ
Ž™œ›ÛJÛÛ^œK›X^
JJH
ˆ˜YË[\×ØœK›X^
Ž“RS—ÔÔÒUU‘JJNÂˆ]X^[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]X^[][WÝ\™Ù]Û[™ÝHYˆ]ÛÛYJ
ÛÝ\˜ÙWÜÝ\ÝXÚËËØ[\WÛ[™ÝÝXÚÜÊJHBˆ\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜÂˆÂˆØ[\WÛ[™ÝÝXÚÜËœØ]\˜][™×ÜÝXŠÛÝ\˜ÙWÜÝ\ÝXÚÊBˆH[ÙHYˆ]ÛÛYJ
ÛÝ\˜ÙWÜÝ\Û\ËËØ[\WÛ[™ÝÛ\ËËÊJHBˆ\™Ù]˜]Y[×Ü™\Ú^™WÛZ[\ÙXÛÛ™ÂˆÂˆ


Ž™œ›ÛJØ[\WÛ[™ÝÛ\ÊHHŽ™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊJBˆÈZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ™›ÛÜŠ
Bˆ˜Û[\
ŒŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌŠBˆ›X^
\™Ù]œÝ\Û[™ÝÝXÚÜÊBˆH[ÙHÂˆLÌŽŽ“PVˆNÂˆMŽ™œ›ÛJX^[][WÝ\™Ù]Û[™Ý
HHMŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊBˆJBˆ›Z[Š
Bˆ[Ü˜\ÛÜ—Ù[ÙJMŽ™œ›ÛJLÌŽŽ“PV
HHMŽ™œ›ÛJX^[][WÛ[™Ý
JNÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂ‚ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ][™ÝÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊH
È[JH\ÈLÌŽÂˆ]YØXÞWÜÛÝ\˜ÙWÝÚ[™ÝÈH\™Ù]˜]Y[×Ü™\Ú^™WÛZ[\ÙXÛÛ™Ë›X\
ˆ
ÛÝ\˜ÙWÜÝ\Û\ËËØ[\WÛ[™ÝÛ\ËÝÜ™YÜÝ\ÛÙ™œÙ]ÝÜ™YÙ[™ÛÙ™œÙ]
_ÂˆYˆ[HOHÂˆ™]\›ˆ
ÝÜ™YÜÝ\ÛÙ™œÙ]ÝÜ™YÙ[™ÛÙ™œÙ]
NÂˆBˆ]ÛÝ\˜ÙWÙ[™Û\ÈH
Ž™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊBˆ
ÈŽ™œ›ÛJ[™ÝÝXÚÜÊH
ˆZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ›Z[ŠŽ™œ›ÛJØ[\WÛ[™ÝÛ\ÊJBˆ\ÈŒÌŽÂˆ
ÛÝ\˜ÙWÜÝ\Û\ËÛÝ\˜ÙWÙ[™Û\ÊBˆKˆ
NÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÝ\ÛÙ™œÙ]ˆYØXÞWÜÛÝ\˜ÙWÝÚ[™ÝË›X\

Ý\Ê_Ý\
Kˆ[™ÛÙ™œÙ]ˆYØXÞWÜÛÝ\˜ÙWÝÚ[™ÝË›X\

Ë[™
_[™
KˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆ\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜË›X\
ˆ
ÛÝ\˜ÙWÜÝ\ÝXÚËËÊ_Âˆ
ˆÛÝ\˜ÙWÜÝ\ÝXÚËˆÛÝ\˜ÙWÜÝ\ÝXÚËœØ]\˜][™×ØY
[™ÝÝXÚÜÊKˆ
BˆKˆ
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ^[\ÝØÛ\ÙÜ›Ý\ÛYÜ™\Ú^™WÙY]Êˆ˜YÎˆXÝ]™T^[\ÝÛ\˜YËˆ\™Ù]Îˆ	–Ô^[\ÝÛ\˜YÕ\™Ù]KˆÛÛ^ˆ^[\ÝÛ\˜YÐÛÛ^ŠHOˆ™XÏ
\Ú^™K^[\ÝÛ\Y]
OˆÂˆYˆ\™Ù]Ëš\×Ù[\J
HÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ][˜ÚÜ—ÙY]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆÛÛ^œÚ[\‹ˆÛÛ^XÚ×ÜØØ[KˆÛÛ^œ›Ý×ÚZYÚˆÛÛ^œKˆÛÛ^œÛ˜\ˆÛÛ^›X^Ý˜XÚ×Ú[™^ˆ
NÂˆ][˜ÚÜ—ÜÜÚ][ÛˆH[˜ÚÜ—ÙY]ˆœÜÚ][Û—ÝXÚÜÂˆ[Ü˜\ÛÜŠ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]™\]Y\ÝYÙ[HHMŽ™œ›ÛJ[˜ÚÜ—ÜÜÚ][ÛŠHHMŽ™œ›ÛJ˜YËœÝ\ÜÜÚ][Û—ÝXÚÜÊNÂˆYˆ\™Ù]Âˆš]\Š
Bˆ˜[
\™Ù]\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜËš\×ÜÛÛYJ
JBˆÂˆ]Z[š[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]
ÛÝ\˜ÙWÜÝ\ÝXÚËËÊHH\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜË[Ü˜\ÛÜ—ÙY˜][

NÂˆ
ZMŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊJBˆ›X^
ZMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊJBˆ›X^
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHHMŽ™œ›ÛJLÌŽŽ“PV
JBˆJBˆ›X^

Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ]X^[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚËÊHBˆ\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜË[Ü˜\ÛÜ—ÙY˜][

NÂˆ
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHHJBˆ›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
HHMŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊJBˆ›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
HHMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊJBˆ›Z[ŠMŽ™œ›ÛJÛÝ\˜ÙWÙ[™ÝXÚÊHHMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊHHJBˆJBˆ›Z[Š
Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂ‚ˆ™]\›ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚËÊHBˆ\™Ù]˜]Y[×Ü™\Ú^™WÝXÚÜË[Ü˜\ÛÜ—ÙY˜][

NÂˆ]ÜÚ][Û—ÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊH
È[JH\ÈLÌŽÂˆ][™ÝÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHH[JH\ÈLÌŽÂˆ]ÛÝ\˜ÙWÜÝ\ÝXÚÈH
MŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊH
È[JH\ÈLÌŽÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJÜÚ][Û—ÝXÚÜÊKˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆÛÛYJ
ÛÝ\˜ÙWÜÝ\ÝXÚËÛÝ\˜ÙWÙ[™ÝXÚÊJKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

NÂˆB‚ˆYˆ\™Ù]Âˆš]\Š
Bˆ˜[žJ\™Ù]\™Ù]˜]Y[×Ü™\Ú^™WÛZ[\ÙXÛÛ™Ëš\×Û›Û™J
JBˆÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆBˆ]Z[\ÙXÛÛ™×Ü\—ÝXÚÈBˆŒÌŒÈ
Ž™œ›ÛJÛÛ^œK›X^
JJH
ˆ˜YË[\×ØœK›X^
Ž“RS—ÔÔÒUU‘JJNÂˆ]Z[š[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]
ÛÝ\˜ÙWÜÝ\Û\ËËËËÊHBˆ\™Ù]˜]Y[×Ü™\Ú^™WÛZ[\ÙXÛÛ™Ë[Ü˜\ÛÜ—ÙY˜][

NÂˆ]ÛÝ\˜ÙWÙ[HH
YŽ™œ›ÛJÛÝ\˜ÙWÜÝ\Û\ÊHÈZ[\ÙXÛÛ™×Ü\—ÝXÚÊBˆ˜ÙZ[

Bˆ˜Û[\
MŽ“RSˆ\ÈMŽ“PV\È
H\ÈMÂˆ
ZMŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊJBˆ›X^
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHHMŽ™œ›ÛJLÌŽŽ“PV
JBˆ›X^
ÛÝ\˜ÙWÙ[JBˆJBˆ›X^

Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ]X^[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHHJBˆ›Z[ŠMŽ™œ›ÛJLÌŽŽ“PV
HHMŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊJBˆJBˆ›Z[Š
Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂˆ]ÛÝ\˜ÙWÙ[WÛ\ÈH
[H\È
ˆZ[\ÙXÛÛ™×Ü\—ÝXÚÊH\ÈŒÌŽÂ‚ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]
ÛÝ\˜ÙWÜÝ\Û\ËÛÝ\˜ÙWÙ[™Û\ËËÝÜ™YÜÝ\ÛÙ™œÙ]ÝÜ™YÙ[™ÛÙ™œÙ]
HBˆ\™Ù]˜]Y[×Ü™\Ú^™WÛZ[\ÙXÛÛ™Ë[Ü˜\ÛÜ—ÙY˜][

NÂˆ]ÜÚ][Û—ÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\ÜÜÚ][Û—ÝXÚÜÊH
È[JH\ÈLÌŽÂˆ][™ÝÝXÚÜÈH
MŽ™œ›ÛJ\™Ù]œÝ\Û[™ÝÝXÚÜÊHH[JH\ÈLÌŽÂˆ]
™^ÜÝ\ÛÙ™œÙ]™^Ù[™ÛÙ™œÙ]
HHYˆ[HOHÂˆ
ÝÜ™YÜÝ\ÛÙ™œÙ]ÝÜ™YÙ[™ÛÙ™œÙ]
BˆH[ÙHÂˆ

ÛÝ\˜ÙWÜÝ\Û\È
ÈÛÝ\˜ÙWÙ[WÛ\ÊK›X^
Œ
KÛÝ\˜ÙWÙ[™Û\ÊBˆNÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]ÂˆÜÚ][Û—ÝXÚÜÎˆÛÛYJÜÚ][Û—ÝXÚÜÊKˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆÝ\ÛÙ™œÙ]ˆÛÛYJ™^ÜÝ\ÛÙ™œÙ]
Kˆ[™ÛÙ™œÙ]ˆÛÛYJ™^Ù[™ÛÙ™œÙ]
Kˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

BŸB‚™›ˆ^[\ÝØÛ\ÙÜ›Ý\ÜÛ\ÙY]Êˆ˜YÎˆXÝ]™T^[\ÝÛ\˜YËˆ\™Ù]Îˆ	–Ô^[\ÝÛ\˜YÕ\™Ù]KˆÛÛ^ˆ^[\ÝÛ\˜YÐÛÛ^ŠHOˆ™XÏ
\Ú^™K^[\ÝÛ\Y]
OˆÂˆYˆ\™Ù]Ëš\×Ù[\J
HÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆB‚ˆ][˜ÚÜ—ÙY]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆÛÛ^œÚ[\‹ˆÛÛ^XÚ×ÜØØ[KˆÛÛ^œ›Ý×ÚZYÚˆÛÛ^œKˆÛÛ^œÛ˜\ˆÛÛ^›X^Ý˜XÚ×Ú[™^ˆ
NÂˆX]Ú˜YËšÚ[™Âˆ^[\ÝÛ\˜YÒÚ[™Ž”Û\XÚÜÈÂˆÛÝ\˜ÙWÜÝ\ÝXÚË‹‚ˆHOˆÂˆYˆ\™Ù]Ëš]\Š
K˜[žJ\™Ù]Âˆ[X]Ú\ÈJˆ\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝËˆÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ•XÚÜÈÈ‹ˆJBˆ
BˆJHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆBˆ][˜ÚÜ—ÜÛÝ\˜ÙWÜÝ\ÝXÚÈH[˜ÚÜ—ÙY]ˆœÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÂˆ›X\

Ý\Ê_Ý\
Bˆ[Ü˜\ÛÜŠÛÝ\˜ÙWÜÝ\ÝXÚÊNÂˆ]™\]Y\ÝYÙ[HBˆMŽ™œ›ÛJ[˜ÚÜ—ÜÛÝ\˜ÙWÜÝ\ÝXÚÊHHMŽ™œ›ÛJÛÝ\˜ÙWÜÝ\ÝXÚÊNÂˆ]Z[š[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ•XÚÜÈÈÝ\ÝXÚË‹ˆJHBˆ\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆÂˆNÂˆZMŽ™œ›ÛJÝ\ÝXÚÊBˆJBˆ›X^

Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ]X^[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ•XÚÜÈÂˆ[™ÝXÚËˆØ[\WÛ[™ÝÝXÚÜËˆ‹‚ˆJHH\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆÂˆNÂˆMŽ™œ›ÛJØ[\WÛ[™ÝÝXÚÜÊHHMŽ™œ›ÛJ[™ÝXÚÊBˆJBˆ›Z[Š
Bˆ[Ü˜\ÛÜ—ÙY˜][

NÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂ‚ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ•XÚÜÈÂˆÝ\ÝXÚËˆ[™ÝXÚËˆ‹‚ˆJHH\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆ
\™Ù]˜Û\Ú[™^^[\ÝÛ\Y]Ž™Y˜][

JNÂˆNÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]ÂˆÛÝ\˜ÙWÝÚ[™Ý×ÝXÚÜÎˆÛÛYJ
ˆ
MŽ™œ›ÛJÝ\ÝXÚÊH
È[JH\ÈLÌ‹ˆ
MŽ™œ›ÛJ[™ÝXÚÊH
È[JH\ÈLÌ‹ˆ
JKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

BˆBˆ^[\ÝÛ\˜YÒÚ[™Ž”Û\OˆÂˆYˆ\™Ù]Ëš]\Š
K˜[žJ\™Ù]Âˆ[X]Ú\ÈJˆ\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝËˆÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ“Z[\ÙXÛÛ™ÈÈ‹ˆJBˆ
BˆJHÂˆ™]\›ˆ™XÎŽ›™]Ê
NÂˆBˆ][˜ÚÜ—ÜÛÝ\˜ÙWÜÝ\Û\ÈH[˜ÚÜ—ÙY]ˆœÝ\ÛÙ™œÙ]ˆ[Ü˜\ÛÜŠ˜YËœÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÊNÂˆ]™\]Y\ÝYÙ[HH[˜ÚÜ—ÜÛÝ\˜ÙWÜÝ\Û\ÈH˜YËœÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÎÂˆ]Z[š[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ“Z[\ÙXÛÛ™ÈÈÝ\Û\Ë‹ˆJHBˆ\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆŒÂˆNÂˆ\Ý\Û\ÂˆJBˆ™›Û
ŒÙŒÌ‹ŒÌŽŽ›X^
NÂˆ]X^[][WÙ[HH\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ“Z[\ÙXÛÛ™ÈÂˆ[™Û\ËˆØ[\WÛ[™ÝÛ\Ëˆ‹‚ˆJHH\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆŒÂˆNÂˆØ[\WÛ[™ÝÛ\ÈH[™Û\ÂˆJBˆ™›Û
ŒÌŽŽ“PVŒÌŽŽ›Z[ŠNÂˆ][HH™\]Y\ÝYÙ[K˜Û[\
Z[š[][WÙ[KX^[][WÙ[JNÂ‚ˆ\™Ù]Âˆš]\Š
Bˆ›X\
\™Ù]Âˆ]ÛÛYJ^[\ÝÛ\Û\Ú[™ÝÎŽ“Z[\ÙXÛÛ™ÈÂˆÝ\Û\Ë[™Û\Ë‹‚ˆJHH\™Ù]˜]Y[×ÜÛ\ÝÚ[™ÝÂˆ[ÙHÂˆ™]\›ˆ
\™Ù]˜Û\Ú[™^^[\ÝÛ\Y]Ž™Y˜][

JNÂˆNÂˆ
ˆ\™Ù]˜Û\Ú[™^ˆ^[\ÝÛ\Y]ÂˆÝ\ÛÙ™œÙ]ˆÛÛYJÝ\Û\È
È[JKˆ[™ÛÙ™œÙ]ˆÛÛYJ[™Û\È
È[JKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
BˆJBˆ˜ÛÛXÝ

BˆBˆÈOˆ™XÎŽ›™]Ê
KˆBŸB‚™›ˆ›ÝWÙœ›ÛWÙÜšYÜÜÚ][ÛŠˆÚ[\ŽˆYÝZNŽ”ÜÌ‹ˆÜšYˆX[›Ô›ÛÜšYˆÚ[›™[ÚYˆLM‹ŠHOˆÜ[Û]\›“›ÝOˆÂˆ]Û˜\ÝXÚÜÈHÜšYœÛ˜\ÝXÚÜÎÂˆ›ÝWÙœ›ÛWÙÜšYÜÜÚ][Û—ÝÚ]ÜÛ˜\
Ú[\‹ÜšYÚ[›™[ÚYÛ˜\ÝXÚÜÊBŸB‚™›ˆ›ÝWÙœ›ÛWÙÜšYÜÜÚ][Û—ÝÚ]ÜÛ˜\
ˆÚ[\ŽˆYÝZNŽ”ÜÌ‹ˆÜšYˆX[›Ô›ÛÜšYˆÚ[›™[ÚYˆLM‹ˆÛ˜\ÝXÚÜÎˆLÌ‹ŠHOˆÜ[Û]\›“›ÝOˆÂˆ]ÜšYÛYHÜšYœ™XÝ›Y

H
ÈÜšYšÙ^X›Ø\™ÝÚYÂˆYˆYÜšYœ™XÝ˜ÛÛZ[œÊÚ[\ŠBˆÚ[\‹žÜšYÛYˆÜšYXÚ×ÜØØ[HHŒˆÜšYšÙ^WÚZYÚHŒˆÂˆ™]\›ˆ›Û™NÂˆBˆ]XÚÜ×Ùœ›ÛWÜÝ\H

Ú[\‹žHÜšYÛY
HÈÜšYXÚ×ÜØØ[JKœ›Ý[™

H\ÈMÂˆ]›ÝÈH

Ú[\‹žHHÜšYœ™XÝÜ

JHÈÜšYšÙ^WÚZYÚ
K™›ÛÜŠ
H\ÈLMŽÂˆ]Ù^HHÜšYˆšÙ^WÚYÚˆœØ]\˜][™×ÜÝXŠ›ÝÊBˆ˜Û[\
ÜšYšÙ^WÛÝËÜšYšÙ^WÚYÚ
NÂˆÛÛYJ]\›“›ÝHÂˆÜÚ][ÛŽˆÛ˜\Û›ÝWÝXÚÊXÚÜ×Ùœ›ÛWÜÝ\Û˜\ÝXÚÜË
KˆÚ[›™[ÚYˆ[™ÝˆLÌŽŽ™œ›ÛJÜšYœJK›X^
JKˆÙ^Kˆ™[ØÚ]NˆLˆ‹”]\›“›ÝNŽ™Y˜][

BˆJBŸB‚™›ˆ]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]
ˆÝ\ÝXÚÎˆLÌ‹ˆÙ™œÙ]Û\ÎˆLM‹ˆ[\×ÛX\ˆ	”^[\Ý[\ÓX\ˆØ[\WÜ˜]NˆLÌ‹ŠHOˆ™\Ý[LÌ‹Ýš[™ÏˆÂˆYˆØ[\WÜ˜]HOHÂˆ™]\›ˆ\œŠš[œ]Ø[\H˜]H]\Ý™HÜ™X]\ˆ[ˆ™\›È‹×ÛÝÛ™Y

JNÂˆBˆ]Ý\Ùœ˜[YHH[\×ÛX\™œ˜[YWØ]ÝXÚÊMŽ™œ›ÛJÝ\ÝXÚÊKØ[\WÜ˜]JOÎÂˆ]Ù™œÙ]Ùœ˜[Y\ÈHMŽ™œ›ÛJÙ™œÙ]Û\ÊH
ˆMŽ™œ›ÛJØ[\WÜ˜]JHÈWÌÂˆ]ÛÛ\[œØ]YÙœ˜[YHHYˆÙ™œÙ]Ùœ˜[Y\ÈHÂˆÝ\Ùœ˜[YKœØ]\˜][™×ÜÝXŠÙ™œÙ]Ùœ˜[Y\È\ÈM
BˆH[ÙHÂˆÝ\Ùœ˜[YBˆ˜ÚXÚÙYØY
Ù™œÙ]Ùœ˜[Y\Ë[œÚYÛ™YØXœÊ
JBˆ›Ú×ÛÜ—Ù[ÙJ˜]Y[È[œ][Z[™ÈÙ™œÙ]^ÙYYÈH™[™\˜X›H[Y[[™H‹×ÛÝÛ™Y

JOÂˆNÂˆ]ÛÛ\[œØ]YÝXÚÈH[\×ÛX\ˆXÚ×Ø]Ùœ˜[YJÛÛ\[œØ]YÙœ˜[YKØ[\WÜ˜]JBˆ›Ú×ÛÜ—Ù[ÙJ˜]Y[È[œ][Z[™ÈÙ™œÙ]\ÈÝ]ÚYHH™[™\˜X›H[Y[[™H‹×ÛÝÛ™Y

JOÎÂˆYˆXÛÛ\[œØ]YÝXÚËš\×Ùš[š]J
HÂˆ™]\›ˆ\œŠ˜]Y[È[œ][Z[™ÈÙ™œÙ]\ÈÝ]ÚYHH™[™\˜X›H[Y[[™H‹×ÛÝÛ™Y

JNÂˆBˆÚÊÛÛ\[œØ]YÝXÚËœ›Ý[™

K˜Û[\
ŒŽ™œ›ÛJLÌŽŽ“PV
JH\ÈLÌŠBŸB‚™›ˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÛÙ™œÙ]
ˆ^ZXYÝXÚÎˆˆ]\›—ÚYˆLM‹ˆÛ\Îˆ	–Ô^[\ÝÛ\KŠHOˆÜ[ÛLÌˆÂˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ØÛ\
^ZXYÝXÚË]\›—ÚYÛ\ÊBˆ›X\

ËØØ[ÝXÚÊ_ØØ[ÝXÚÊBŸB‚™›ˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ØÛ\
ˆ^ZXYÝXÚÎˆˆ]\›—ÚYˆLM‹ˆÛ\Îˆ	–Ô^[\ÝÛ\KŠHOˆÜ[Û
\Ú^™KLÌŠOˆÂˆÛ\Âˆš]\Š
Bˆ™[[Y\˜]J
Bˆ™š[\—ÛX\
Û\Âˆ]
Û\Ú[™^Û\
HHÛ\ÂˆYˆÛ\˜XÚ×Ú[™^š\×Û›Û™J
BˆÛ\›[™ÝÝXÚÜÈOHˆÛ\ˆœØØ[Bˆš\×ÜÛÛYWØ[™
ØØ[_\ØØ[Kš\×Ùš[š]J
H
ØØ[HHKŒ
K˜XœÊ
HˆYKNJBˆ[X]Ú\ÈJÛ\\™Ù]

K^[\ÝÛ\\™Ù]Ž”]\›ˆÈYHYˆYOH]\›—ÚY
BˆÂˆ™]\›ˆ›Û™NÂˆBˆ]ØØ[ÝXÚÈH^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
ˆ^ZXYÝXÚËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\›[™ÝÝXÚÜËˆ
OÎÂˆÛÛYJ
Û\Ú[™^Û\œÜÚ][Û—ÝXÚÜËØØ[ÝXÚÊJBˆJBˆ›X^ØžWÚÙ^J
ËÛ\ÜÝ\Ê_
˜Û\ÜÝ\
Bˆ›X\

Û\Ú[™^ËØØ[ÝXÚÊ_
Û\Ú[™^ØØ[ÝXÚÊJBŸB‚™›ˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊˆ^ZXYÝXÚÎˆˆ]\›—ÚYˆLM‹ˆÛ\Îˆ	–Ô^[\ÝÛ\KŠHOˆÜ[ÛLÌˆÂˆ]X]Ú[™×ØÛ\Ú\×ØXÝ]™HHÛ\Ëš]\Š
K˜[žJÛ\ÂˆÛ\˜XÚ×Ú[™^š\×ÜÛÛYJ
Bˆ	‰ˆÛ\›[™ÝÝXÚÜÈˆˆ	‰ˆX]Ú\ÈJÛ\\™Ù]

K^[\ÝÛ\\™Ù]Ž”]\›ˆÈYHYˆYOH]\›—ÚY
Bˆ	‰ˆ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
ˆ^ZXYÝXÚËˆÛ\œÜÚ][Û—ÝXÚÜËˆÛ\›[™ÝÝXÚÜËˆ
Bˆš\×ÜÛÛYJ
BˆJNÂˆYˆX]Ú[™×ØÛ\Ú\×ØXÝ]™Bˆ\^ZXYÝXÚËš\×Ùš[š]J
Bˆ^ZXYÝXÚÈŒˆ^ZXYÝXÚÈˆŽ™œ›ÛJLÌŽŽ“PV
BˆÂˆ™]\›ˆ›Û™NÂˆBˆÛÛYJ^ZXYÝXÚËœ›Ý[™

H\ÈLÌŠBŸB‚™›ˆ^[™Ü™XÛÜ™YÜ]\›—Ü^[\ÝØÛ\
ˆØÝ[Y[ˆ	›]]›ØÝ[Y[ˆ]\›—ÚYˆLM‹ˆ^[œÚ[ÛŽˆZYT^[\ÝÛ\^[œÚ[Û‹ˆ›Ý\Îˆ	–Ô]\›“›ÝWKŠHOˆ™\Ý[Ü[ÛLÌ‹Ýš[™ÏˆÂˆ]™\]Z\™YÛ[™ÝÝXÚÜÈH›Ý\Âˆš]\Š
BˆžWÙ›Û
ÝLÌ‹[™Ý›Ý_Âˆ›ÝKœÜÚ][Û‚ˆ˜ÚXÚÙYØY
›ÝK›[™Ý
Bˆ›X\
›ÝWÙ[™[™Ý›X^
›ÝWÙ[™
JBˆJBˆ›Ú×ÛÜ—Ù[ÙJH™XÛÜ™Y]\›ˆ›ÝH^ÙYYÈH^[\ÝXÚÈ˜[™ÙH‹×ÛÝÛ™Y

JOÎÂˆ]\œ˜[™Ù[Y[ÈHØÝ[Y[ˆ˜\œ˜[™Ù[Y[Ê
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›Ý[œÜXÝH™XÛÜ™[™È^[\ÝˆÙ\œ›ÜŸHŠJOÎÂˆ]\œ˜[™Ù[Y[H\œ˜[™Ù[Y[Âˆš]\Š
Bˆ™š[™
\œ˜[™Ù[Y[\œ˜[™Ù[Y[šYOH^[œÚ[Û‹˜\œ˜[™Ù[Y[ÚY
Bˆ›Ú×ÛÜ—Ù[ÙJ•H™XÛÜ™[™È\œ˜[™Ù[Y[›ÈÛ™Ù\ˆ^\ÝÈ‹×ÛÝÛ™Y

JOÎÂˆ]Û\H\œ˜[™Ù[Y[ˆ˜Û\Âˆ™Ù]
^[œÚ[Û‹˜Û\Ú[™^
Bˆ›Ú×ÛÜ—Ù[ÙJ•HXÝ]™H^[\ÝÛ\Ú[™ÙYÚ[H™XÛÜ™[™È‹×ÛÝÛ™Y

JOÎÂˆYˆÛ\œÜÚ][Û—ÝXÚÜÈOH^[œÚ[Û‹œÜÚ][Û—ÝXÚÜÂˆÛ\˜XÚ×Ú[™^OHÛÛYJ^[œÚ[Û‹˜XÚ×Ú[™^
Bˆ[X]Ú\ÈJÛ\\™Ù]

K^[\ÝÛ\\™Ù]Ž”]\›ˆÈYHYˆYOH]\›—ÚY
BˆÂˆ™]\›ˆ\œŠ•HXÝ]™H^[\ÝÛ\Ú[™ÙYÚ[H™XÛÜ™[™È‹×ÛÝÛ™Y

JNÂˆBˆYˆÛ\›[™ÝÝXÚÜÈH™\]Z\™YÛ[™ÝÝXÚÜÈÂˆ™]\›ˆÚÊ›Û™JNÂˆBˆYˆÛ\›[™ÝÝXÚÜÈOH^[œÚ[Û‹›[™ÝÝXÚÜÈÂˆ™]\›ˆ\œŠ•HXÝ]™H^[\ÝÛ\[™ÝÚ[™ÙYÚ[H™XÛÜ™[™È‹×ÛÝÛ™Y

JNÂˆBˆYˆXÛ\˜Ø[—Ü™\Ú^™WÝ[Y[[™WÛ[™Ý

HÂˆ™]\›ˆ\œŠHXÝ]™H^[\ÝÛ\\È[ˆ[œ™XÛÙÛš^™YÛÝ\˜ÙHÚ[™ÝÈ‹×ÛÝÛ™Y

JNÂˆBˆYˆ^[œÚ[Û‚ˆœÜÚ][Û—ÝXÚÜÂˆ˜ÚXÚÙYØY
™\]Z\™YÛ[™ÝÝXÚÜÊBˆš\×Û›Û™J
BˆÂˆ™]\›ˆ\œŠ•H™XÛÜ™Y]\›ˆ^[™È™^[Û™H^[\Ý[Y[[™H‹×ÛÝÛ™Y

JNÂˆBˆØÝ[Y[ˆ™Y]Ü^[\ÝØÛ\
ˆ^[œÚ[Û‹˜\œ˜[™Ù[Y[ÚYˆ^[œÚ[Û‹˜Û\Ú[™^ˆ^[\ÝÛ\Y]Âˆ[™ÝÝXÚÜÎˆÛÛYJ™\]Z\™YÛ[™ÝÝXÚÜÊKˆ‹”^[\ÝÛ\Y]Ž™Y˜][

BˆKˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›Ý^[™H^[\Ý]\›ˆÛ\ˆÙ\œ›ÜŸHŠJOÎÂˆÚÊÛÛYJ™\]Z\™YÛ[™ÝÝXÚÜÊJBŸB‚™›ˆÜ™X]WÜ™XÛÜ™YÜ]\›—Ü^[\ÝØÛ\
ˆØÝ[Y[ˆ	›]]›ØÝ[Y[ˆ]\›—ÚYˆLM‹ˆXÙ[Y[ˆZYT^[\ÝÛ\XÙ[Y[ŠHOˆ™\Ý[
\Ú^™KLMŠKÝš[™ÏˆÂˆ]]\›ˆHØÝ[Y[ˆœ]\›œÊ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›Ý[œÜXÝH™XÛÜ™Y]\›ŽˆÙ\œ›ÜŸHŠJOÂˆš[×Ú]\Š
Bˆ™š[™
]\›Ÿ]\›‹šYOH]\›—ÚY
Bˆ›Ú×ÛÜ—Ù[ÙJ›Ü›X]J”™XÛÜ™Y]\›ˆÜ]\›—ÚYH›ÈÛ™Ù\ˆ^\ÝÈŠJOÎÂˆ]\œ˜[™Ù[Y[HØÝ[Y[ˆ˜\œ˜[™Ù[Y[Ê
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›Ý[œÜXÝH^[\ÝˆÙ\œ›ÜŸHŠJOÂˆš[×Ú]\Š
Bˆ™š[™
\œ˜[™Ù[Y[\œ˜[™Ù[Y[šYOHXÙ[Y[˜\œ˜[™Ù[Y[ÚY
Bˆ›Ú×ÛÜ—Ù[ÙJ•H™XÛÜ™[™È\œ˜[™Ù[Y[›ÈÛ™Ù\ˆ^\ÝÈ‹×ÛÝÛ™Y

JOÎÂˆ]ÚYÛ˜]\™WØÚ[™Ù\ÈHØÝ[Y[ˆ[YWÛX\šÙ\œÊ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›Ý[œÜXÝ^[\Ý[YHÚYÛ˜]\™\ÎˆÙ\œ›ÜŸHŠJOÂˆš[×Ú]\Š
Bˆ™š[\Š
\œ˜[™Ù[Y[ÚYX\šÙ\Š_Âˆ
˜\œ˜[™Ù[Y[ÚYOHXÙ[Y[˜\œ˜[™Ù[Y[ÚY	‰ˆX\šÙ\‹š\×ÜÚYÛ˜]\™J
BˆJBˆ™š[\—ÛX\

ËX\šÙ\Š_ÂˆÛÛYJ
ˆX\šÙ\‹œÜÚ][Û—ÝXÚÜÊ
KˆX\šÙ\‹›[Y\˜]ÜŠ
OËˆX\šÙ\‹™[›ÛZ[˜]ÜŠ
OËˆ
JBˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ]ÚYÛ˜]\™HH^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊˆØÝ[Y[›Y]Y]J
K[YWÜÚYÛ˜]\™J
Kˆ	œÚYÛ˜]\™WØÚ[™Ù\ËˆXÙ[Y[œÝ\ÝXÚËˆ
NÂˆ]˜\—Û[™ÝH^[\ÝØ˜\—ÝXÚÜÊØÝ[Y[šXY\Š
KœJ
KÚYÛ˜]\™JNÂˆ]]\›—Û[™ÝH]\›‹›[™ÝÝXÚÜË[Ü˜\ÛÜ—ÙY˜][

K›X^
ˆ]\›‚ˆ››Ý\Âˆš]\Š
Bˆ›X\
›Ý_›ÝKœÜÚ][Û‹œØ]\˜][™×ØY
›ÝK›[™Ý
JBˆ›X^

Bˆ[Ü˜\ÛÜ—ÙY˜][

Kˆ
NÂˆ][™ÝÝXÚÜÈHLÌŽŽžWÙœ›ÛJMŽ™œ›ÛJ]\›—Û[™Ý
K›X^
˜\—Û[™Ý
K›X^
JJBˆ›X\Ù\œŠß”™XÛÜ™Y]\›ˆ\ÈÛÈÛ™È›ÜˆH^[\ÝÛ\‹×ÛÝÛ™Y

JOÎÂˆYˆXÙ[Y[œÝ\ÝXÚË˜ÚXÚÙYØY
[™ÝÝXÚÜÊKš\×Û›Û™J
HÂˆ™]\›ˆ\œŠ•H™XÛÜ™Y]\›ˆ^[™È™^[Û™H^[\Ý[Y[[™H‹×ÛÝÛ™Y

JNÂˆBˆ]˜XÚ×Ú[™^Bˆš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚÊ	˜\œ˜[™Ù[Y[XÙ[Y[œÝ\ÝXÚË[™ÝÝXÚÜÊBˆ›Ú×ÛÜ—Ù[ÙJÂˆ“›Èœ™YH^[\Ý˜XÚÈ\È]˜Z[X›H]H™XÛÜ™[™ÈÜÚ][Ûˆ‹×ÛÝÛ™Y

BˆJOÎÂˆ]Û\Ú[™^HØÝ[Y[ˆ˜Ü™X]WÜ^[\ÝÜ]\›—ØÛ\
ˆXÙ[Y[˜\œ˜[™Ù[Y[ÚYˆ]\›—ÚYˆXÙ[Y[œÝ\ÝXÚËˆ[™ÝÝXÚÜËˆ˜XÚ×Ú[™^ˆ
Bˆ›X\Ù\œŠ\œ›ÜŸ›Ü›X]JÛÝ[›ÝÜ™X]HH^[\Ý]\›ˆÛ\ˆÙ\œ›ÜŸHŠJOÎÂˆÚÊ
Û\Ú[™^˜XÚ×Ú[™^
JBŸB‚™›ˆ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
ˆ^ZXYÝXÚÎˆˆÛ\ÜÜÚ][Û—ÝXÚÜÎˆLÌ‹ˆÛ\Û[™ÝÝXÚÜÎˆLÌ‹ŠHOˆÜ[ÛLÌˆÂˆYˆ\^ZXYÝXÚËš\×Ùš[š]J
H^ZXYÝXÚÈŒÛ\Û[™ÝÝXÚÜÈOHÂˆ™]\›ˆ›Û™NÂˆBˆ]Û\ÜÝ\HŽ™œ›ÛJÛ\ÜÜÚ][Û—ÝXÚÜÊNÂˆ]Û\Ù[™HÛ\ÜÝ\
ÈŽ™œ›ÛJÛ\Û[™ÝÝXÚÜÊNÂˆYˆ^ZXYÝXÚÈÛ\ÜÝ\^ZXYÝXÚÈHÛ\Ù[™Âˆ™]\›ˆ›Û™NÂˆBˆ]ØØ[ÝXÚÈH
^ZXYÝXÚÈHÛ\ÜÝ\
Bˆœ›Ý[™

Bˆ˜Û[\
ŒŽ™œ›ÛJÛ\Û[™ÝÝXÚÜËœØ]\˜][™×ÜÝXŠJJJNÂˆÛÛYJØØ[ÝXÚÈ\ÈLÌŠBŸB‚ˆÖØÙ™Ê\Ý
WB›[Ù\ÝÈÂˆ\ÙHÝŽ˜ÛÛXÝ[ÛœÎŽžÐ•™YSX\•™YTÙ]NÂˆ\ÙHÝŽœ]Ž”]YŽÂ‚ˆ\ÙHÝ\\ŽŽžÂˆXÝ]™T^[\ÝÛ\˜YË\œ˜[™Ù[Y[Ú[›™[\Ü^Qš[\‹Ú[›™[›ÝT›Ý]\‹ˆ]XÚYÚ[™ÝÒÚ[™]XÚYÚ[™ÝÔXÙ[Y[]XÚYÚ[™ÝÔXÙ[Y[Ëˆ]XÝYX[›Ô›ÛØØ[K›ØÝ[Y[XZ[•šY]ËZYP]]ÛX][Û”™XÛÜ™[™ËˆZYPÛÛ›ÛÚ[™ÙSX\[™ËZYPÛÛ›Û\ØÐš[™[™ËZYPÛÛ›Û\•[\]KˆZYQ]šXÙTÙ[XÝ[ÛœËZYR[œ]]šXÙKZYR[œ]]™[ZYSZ^\ÛÛ›ÛX\[™ËˆZYSZ^\ÛÛ›Û\™Ù]ZYSÝ]]]šXÙKZYUœÝÔ\˜[Y]\“X\[™ËˆZYUœÝÔ\˜[Y]\•\™Ù]PS“×Ô“ÓÔ•ST—ÒRQÒ]\›‹]\›ÛÛ›Û\‹]\›“›ÝKˆ]\›•œÝÔ\˜[Y]\“[šËX[›Ô›ÛÚÜ™Ý[\X[›Ô›ÛÜšYX[›Ô›ÛØØ[KˆX[›Ô›ÛÛ˜\^[\ÝÛ\^[\ÝÛ\˜YÒÚ[™^[\Ý[\ÓX\^[\Ý˜XÚËˆYÚ[Ø[™Y]KYÚ[‘›Ü›X][YTÙ[XÝ[Û”ÚYÝXÚÙ\Ý\™KÝXÚ\˜XÚÙ\‹ˆÝXÚ›ÛÛPÛÛ™šYËÒS‘Õ×ÓVSÕUÔ‘TÑU×ÒPQT‹Ú[™ÝÓ^[Ý]™\Ù]ˆ]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]Z[Ú[—ÛZYWØÛÛ›Û\—Ý[\]\ËˆØ[™Y]WÛ]™WÛZ^\—ÜÙ[™ÚÙ^KØ[™Y]WÛX]Ú\×ÝœÝÚY[]KØ[™Y]WÛZ^\—ØÛÛ›ÛËˆØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û‹]XÝØÚÜ™Û˜[YK]XÝÜX[›×Ü›ÛØÚ[›™[ÜØØ[Kˆ]XÝÜX[›×Ü›ÛÜØØ[K[˜ÛÙWÛZYWÙ]šXÙWÚY[˜ÛÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœËˆ[˜ÛÙWÛZYWÛÝ]]Ü›Ý]\Ëš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚËˆ\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[]™WÜ^[\ÝÝœÝ×Û›ÝWÝ\™Ù]ËˆZYWØ]Û—Ü™\Ü×ÙYÙKZYWØØ×Ý×ÛZ^\—Ù˜Y\—Ü˜]ËZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]ËˆZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]ËZYWØÛÛ›Û\—Øš[™[™×Ù›Ü—Ù]šXÙKˆZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙKZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØËˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™×Ù›Ü—Ý\™Ù]ZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ù›Ü—ØØËˆ™^ÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\›ÝWÙœ›ÛWÙÜšYÜÜÚ][Û‹\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Ëˆ\œÙWÛZYWØÛÛ›Û\—Ý[\]\Ë\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœËˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜË\œÙWÛZYWÛÝ]]Ü›ÙÜ˜[\Ë\œÙWÛZYWÛÝ]]Ü›Ý]\Ëˆ\œÙWÛZYWÛÝ]]Ý˜[Y\Ë\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜËˆ\œÙWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜË\œÙWÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ëˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚË]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YKˆ]\›—ÝœÝ×Ü\˜[Y]\—Ø]]ÛX][Û—Ù›Ü—ØÚ[›™[]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ù›Ü—Ý\™Ù]ˆX[›×Ü›ÛØÛÛÜ—ÙÜ›Ý\Û›ÝWÚYËX[›×Ü›ÛØÛÛ›Û\—ÜÝ›ÚÙWÜÚ[ËˆX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWØ]X[›×Ü›ÛØÛÛ›Û\—Ý˜[YWÜ˜[™ÙKˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ËX[›×Ü›ÛÙ\ØØ\™Û[™ÝÙY]ˆX[›×Ü›ÛÙ\XØ]WÛÙ™œÙ]X[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›KX[›×Ü›ÛÚÙ^WØ]ÞKˆX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÛY[X™\œËX[›×Ü›ÛÛ›ÝWÛYÙWÙY]X[›×Ü›ÛÛÙÛ›ÝWÜÙ[XÝ[Û‹ˆX[›×Ü›ÛÛÝ™\›\Û›ÝWÜÙ[XÝ[Û‹X[›×Ü›ÛÜ]ÚÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[Û‹ˆX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[Û‹X[›×Ü›ÛÜÙ[XÝ[Û—Ý[YWÜ˜[™ÙKˆX[›×Ü›ÛÜÚYÝ[YWÜ˜[™ÙKX[›×Ü›ÛÝ[YWÜ˜[™ÙWÙœ›ÛWÙ˜YËˆX[›×Ü›ÛÝ[YWÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[Û‹X[›×Ü›ÛÝ[YWÝXÚ×Ø]Þˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[—ØØ[™Y]\Ë^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—ÝXÚÜËˆ^[\ÝØ˜\—ÝXÚÜË^[\ÝØÛ\Ù˜Y×ÙY]^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]ˆ^[\ÝØÛ\ÜÜ]ÜÜÚ][Û‹^[\ÝÙÜ›Ý\Ü\™[ÚYË^[\ÝÛYX\Ý\™WØ›Ý[™\šY\Ëˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\Ë^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜYˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚË^[\ÝÜÙYZ×ÝXÚ×Ý×Ùœ˜[YKˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚË^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙKˆ^[\ÝÝ˜XÚ×Ú\×ÚY[‹™XÛÛÜ—ÜX[›×Ü›ÛÛ›Ý\Ë™XÛÜ™Ü]\›—ØÛÛ›Û\—ÜÚ[Ëˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]Ë™[[Ý™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ëˆ™[[Ý™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šËØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YËˆÙ\šX[^™WÛZYWØÛÛ›Û\—Ý[\]\ËÙ\šX[^™WÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜËˆÙ\šX[^™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜËÙ\šX[^™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜËˆÙ\šX[^™WÝÚ[™Ý×Û^[Ý]Ü™\Ù]ËÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]ˆÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]Û˜\Û›ÝWÝXÚËÛ˜\ÜX[›×Ü›ÛÚÙ^WÝ×ÜØØ[KˆÛ˜\Ü™XÛÜ™YÛZYWÛ›Ý\×Ý×ÜØØ[KÙÙÛWÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÜÙ[XÝ[Û‹ˆÝXÚÜ[—Þ›ÛÛWÜØÜ›ÛÛÙ™œÙ]˜XÚ×ÝÛ×Ùš[™Ù\—Ý\\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[Û‹ˆ\]WÛ^Y\—ØÚ[ÜÙ[XÝ[Û‹\]WÜX[›×Ü›ÛØ›ÞÜÙ[XÝ[Û‹ˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ë\Ù\ÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ëˆ\Ù\Ü]\›—ÝœÝ×Ü\˜[Y]\—Û[šË›ÛÛWÝ×Ùš]ˆNÂ‚ˆ›ˆZ^\—ÜÙ[™Ü›Ý]WÙš^\™J
HOˆ›ØÝ[Y[Âˆ›ˆ\[™Ù]WÙ]™[
]™[ÜÝ™X[Nˆ	›]]™XÏN‹ÜÛÙNˆN^[ØYˆ	–ÝNJHÂˆ]™[ÜÝ™X[Kœ\Ú
ÜÛÙJNÂˆ]]]Ú^™HH^[ØY›[Š
H\ÈLÌŽÂˆÚ[HÚ^™HHÂˆ]™[ÜÝ™X[Kœ\Ú

Ú^™H\ÈN
H
NÂˆÚ^™HHÎÂˆBˆ]™[ÜÝ™X[Kœ\Ú
Ú^™H\ÈN
NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ^[ØY
NÂˆB‚ˆ]]]]™[ÜÝ™X[HH™XÎŽ›™]Ê
NÂˆ›Üˆ
Ü™[˜[ÛÛÜ‹›Ý][™×Ù›YÜÊH[ˆÂˆ
Ý\Ú^™KP×ÌQŽ×ÝLÌ‹	–×VË‹—JKˆ
KQÌÎL‹	–ÌVË‹—JKˆ
‹QWÌÐNK	–ÌWVË‹—JKˆHÂˆ]™[ÜÝ™X[Kœ\Ú
PJNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	ŠLWÚLÌŠK×ÛWØž]\Ê
JNÂˆ]™[ÜÝ™X[Kœ\Ú
LÊNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	ŠLWÚLÌŠK×ÛWØž]\Ê
JNÂˆ]™[ÜÝ™X[Kœ\Ú
MJNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	˜ÛÛÜ‹×ÛWØž]\Ê
JNÂˆ\[™Ù]WÙ]™[
	›]]]™[ÜÝ™X[KMË	–Ø‰ÐIÈ
ÈÜ™[˜[\ÈNJNÂˆ\[™Ù]WÙ]™[
	›]]]™[ÜÝ™X[KP‹›Ý][™×Ù›YÜÊNÂˆB‚ˆ]]]\˜[Y]\—Ü^[ØYH™XÎŽ›™]Ê
NÂˆ›Üˆ
ÛÝ\˜ÙWÚ[œÙ\›Ý]WÚY˜[YJH[ˆÊWÝLM‹—ÝN—ÍÚLÌŠK
‹L—Î
WHÂˆ\˜[Y]\—Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	–ÌPK‹ÐËJNÂˆ\˜[Y]\—Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	–Ü›Ý]WÚYJNÂˆ\˜[Y]\—Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	ŠÛÝ\˜ÙWÚ[œÙ\ŠK×ÛWØž]\Ê
JNÂˆ\˜[Y]\—Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	˜[YK×ÛWØž]\Ê
JNÂˆBˆ\[™Ù]WÙ]™[
	›]]]™[ÜÝ™X[KLK	œ\˜[Y]\—Ü^[ØY
NÂ‚ˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	—ÝLÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽM—ÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊK™^XÝ
“Z^\ˆÙ[™›Ý]Hš^\™HÚÝ[\œÙHŠBˆB‚ˆ›ˆ˜[™ÛWÛ^Y\—Ü›Ý][™×ÙØÝ[Y[

HOˆ›ØÝ[Y[Âˆ]]]]™[ÜÝ™X[HH™XÈVÌMKËLNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	Œ]LÌ‹×ÛWØž]\Ê
JNÂˆ›ÜˆÚ[ÚY[ˆÌWÝLM‹—HÂˆ]™[ÜÝ™X[Kœ\Ú
QJNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	˜Ú[ÚY×ÛWØž]\Ê
JNÂˆBˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	–ÌKMKJNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	–Ì‹MKŒ‹JNÂ‚ˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	LÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽMLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊK™^XÝ
œ˜[™ÛH^Y\ˆ›Ý][™Èš^\™HÚÝ[\œÙHŠBˆB‚ˆÖÝ\ÝBˆ›ˆXÝ]™WÜ^[\ÝÜ˜[™ÛWÛ^Y\—ÜÜ]×ÙXXÚÜÙYYYÛ›ÝWØ™]ÙY[—Ù\Ý[˜][ÛœÊ
HÂˆ]ØÝ[Y[H˜[™ÛWÛ^Y\—Ü›Ý][™×ÙØÝ[Y[

NÂˆ]›Ý]\ˆHÚ[›™[›ÝT›Ý]\ŽŽ›™]ÊØÝ[Y[˜Ú[›™[Ê
JNÂˆ]Ø[\\—ØÚ[›™[ÚYÈH•™YTÙ]Ž™œ›ÛJÌWÝLM—JNÂˆ]œÝ×ØÚ[›™[ÚYÈH•™YTÙ]Ž™œ›ÛJÌ—ÝLM—JNÂ‚ˆ›Üˆ›Ý]WÜÙYY[ˆ‹Âˆ][Ý\™Ù]ÈH›Ý]\‹\™Ù]×Ù›Ü—Û›ÝJ›Ý]WÜÙYYŒ
NÂˆ]Ø[\\—Ý\™Ù]ÈBˆ]™WÜ^[\ÝÝœÝ×Û›ÝWÝ\™Ù]Ê	œ›Ý]\‹	œØ[\\—ØÚ[›™[ÚYË›Ý]WÜÙYYŒ
NÂˆ]œÝ×Ý\™Ù]ÈBˆ]™WÜ^[\ÝÝœÝ×Û›ÝWÝ\™Ù]Ê	œ›Ý]\‹	œÝ×ØÚ[›™[ÚYË›Ý]WÜÙYYŒ
NÂ‚ˆ\ÜÙ\Ù\HJ[Ý\™Ù]Ë›[Š
KJNÂˆ\ÜÙ\Û™HJØ[\\—Ý\™Ù]Ëš\×Ù[\J
KœÝ×Ý\™Ù]Ëš\×Ù[\J
JNÂˆ\ÜÙ\Ù\HJˆØ[\\—Ý\™Ù]Âˆ[š[ÛŠ	œÝ×Ý\™Ù]ÊBˆ˜ÛÜYY

Bˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
Kˆ[Ý\™Ù]Ëš[×Ú]\Š
K˜ÛÛXÝ

Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]™WÜ^[\ÝÝœÝ×Û›ÝWÝ\™Ù]Ê	œ›Ý]\‹	œÝ×ØÚ[›™[ÚYË›Ý]WÜÙYYŒ
KˆœÝ×Ý\™Ù]Âˆ
NÂˆBˆB‚ˆÖÝ\ÝBˆ›ˆ[š\]YWÙ\ØX›YÛZ^\—ÜÙ[™Ú\×ØWÛ]™WÝ\™Ù]ÚÙ^J
HÂˆ]ØÝ[Y[HZ^\—ÜÙ[™Ü›Ý]WÙš^\™J
NÂˆ]Ù[™ÈHØÝ[Y[ˆ›Z^\—ÜÙ[™ÜÝ[[X\šY\Ê
Bˆ™^XÝ
“Z^\ˆÙ[™Ý[[X\žHÚÝ[XÛÙHŠNÂˆ]Ù[™HÙ[™Âˆš]\Š
Bˆ™š[™
Ù[™Ù[™š[œÙ\ÛÜ™[˜[

HOHH	‰ˆÙ[™œ›Ý]WÜ\˜[Y]\—ÚY

HOHŠBˆ™^XÝ
H\ØX›YÙ[™ÚÝ[™H™\Ù[ŠNÂ‚ˆ\ÜÙ\Ù\HJÙ[™™[˜X›Y

KÛÛYJ˜[ÙJJNÂˆ\ÜÙ\Ù\HJØ[™Y]WÛ]™WÛZ^\—ÜÙ[™ÚÙ^J	œÙ[™ËÙ[™
KÛÛYJ
KŠJJNÂˆB‚ˆ›ˆ^[\ÝÝ˜XÚ×Ø\œ˜[™Ù[Y[Ùš^\™JÛ\Îˆ	–ÊLÌ‹LÌ‹LMŠWJHOˆ\œ˜[™Ù[Y[Âˆ]]]]™[ÜÝ™X[HH™XÈVÌŒËËNÂˆ]]]Û\Ü^[ØYH™XÎŽÚ]ØØ\XÚ]JÛ\Ë›[Š
H
ˆ
NÂˆ›Üˆ
ÜÚ][Û‹[™Ý˜]×Ý˜XÚ×Ú[™^
H[ˆÛ\ÈÂˆ]]]™XÛÜ™HÌNÈNÂˆ™XÛÜ™Ì‹K˜ÛÜWÙœ›ÛWÜÛXÙJ	œÜÚ][Û‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLLM‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹‹ŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLÝLM‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Î‹ŒL—K˜ÛÜWÙœ›ÛWÜÛXÙJ	›[™Ý×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌL‹‹ŒMK˜ÛÜWÙœ›ÛWÜÛXÙJ	œ˜]×Ý˜XÚ×Ú[™^×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌŒ‹ŒK˜ÛÜWÙœ›ÛWÜÛXÙJ	–ÌJNÂˆ™XÛÜ™Ì‹ŒŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒŒŒÌ‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌŽ‹ŒÌ—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒKŒŒÌ‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹Ì—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒKŒ×ÛWØž]\Ê
JNÂˆÛ\Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	œ™XÛÜ™
NÂˆBˆ]™[ÜÝ™X[Kœ\Ú
NJNÂˆ]]][˜ÛÙYÛ[™ÝHÛ\Ü^[ØY›[Š
H\ÈLÌŽÂˆÚ[H[˜ÛÙYÛ[™ÝHÂˆ]™[ÜÝ™X[Kœ\Ú

[˜ÛÙYÛ[™Ý\ÈN
H
NÂˆ[˜ÛÙYÛ[™ÝHÎÂˆBˆ]™[ÜÝ™X[Kœ\Ú
[˜ÛÙYÛ[™Ý\ÈN
NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	˜Û\Ü^[ØY
NÂ‚ˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	LÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽMLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊBˆ™^XÝ
”^[\Ý˜XÚÈš^\™HÚÝ[\œÙHŠBˆ˜\œ˜[™Ù[Y[Ê
Bˆ™^XÝ
”^[\Ýš^\™HÚÝ[]™H[ˆ\œ˜[™Ù[Y[ŠBˆœ™[[Ý™J
BˆB‚ˆ›ˆ^[\ÝÜ]\›—ÙÚÜÝØ\œ˜[™Ù[Y[Ùš^\™JÛ\Îˆ	–ÊLÌ‹LÌ‹LM‹LMŠWJHOˆ\œ˜[™Ù[Y[Âˆ]]]]™[ÜÝ™X[HH™XÈVÌŒËËNÂˆ]]]Û\Ü^[ØYH™XÎŽÚ]ØØ\XÚ]JÛ\Ë›[Š
H
ˆ
NÂˆ›Üˆ
ÜÚ][Û‹[™Ý]\›—ÚY˜XÚ×Ú[™^
H[ˆÛ\ÈÂˆ]]]™XÛÜ™HÌNÈNÂˆ™XÛÜ™Ì‹K˜ÛÜWÙœ›ÛWÜÛXÙJ	œÜÚ][Û‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLLM‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹‹ŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŠLLMˆ
È]\›—ÚY
K×ÛWØž]\Ê
JNÂˆ™XÛÜ™Î‹ŒL—K˜ÛÜWÙœ›ÛWÜÛXÙJ	›[™Ý×ÛWØž]\Ê
JNÂˆ]˜]×Ý˜XÚ×Ú[™^HN]LM‹œØ]\˜][™×ÜÝXŠ
˜XÚ×Ú[™^
NÂˆ™XÛÜ™ÌL‹‹ŒMK˜ÛÜWÙœ›ÛWÜÛXÙJ	œ˜]×Ý˜XÚ×Ú[™^×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌŒ‹ŒK˜ÛÜWÙœ›ÛWÜÛXÙJ	–ÌJNÂˆ™XÛÜ™Ì‹ŒŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŠLKŒŒÌŠK×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌŽ‹ŒÌ—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŠLKŒŒÌŠK×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹Ì—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒKŒ×ÛWØž]\Ê
JNÂˆÛ\Ü^[ØY™^[™Ùœ›ÛWÜÛXÙJ	œ™XÛÜ™
NÂˆBˆ]™[ÜÝ™X[Kœ\Ú
NJNÂˆ]]][˜ÛÙYÛ[™ÝHÛ\Ü^[ØY›[Š
H\ÈLÌŽÂˆÚ[H[˜ÛÙYÛ[™ÝHÂˆ]™[ÜÝ™X[Kœ\Ú

[˜ÛÙYÛ[™Ý\ÈN
H
NÂˆ[˜ÛÙYÛ[™ÝHÎÂˆBˆ]™[ÜÝ™X[Kœ\Ú
[˜ÛÙYÛ[™Ý\ÈN
NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	˜Û\Ü^[ØY
NÂ‚ˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	LÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽMLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊBˆ™^XÝ
”]\›ˆÚÜÝš^\™HÚÝ[\œÙHŠBˆ˜\œ˜[™Ù[Y[Ê
Bˆ™^XÝ
”]\›ˆÚÜÝš^\™HÚÝ[]™H[ˆ\œ˜[™Ù[Y[ŠBˆœ™[[Ý™J
BˆB‚ˆ›ˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›ŠˆYˆLM‹ˆ[™ÝÝXÚÜÎˆLÌ‹ˆ›Ý\Îˆ™XÏ]\›“›ÝO‹ˆ
HOˆ]\›ˆÂˆ]\›ˆÂˆYˆ[™ÝÝXÚÜÎˆÛÛYJ[™ÝÝXÚÜÊKˆ›Ý\Ëˆ‹”]\›ŽŽ™Y˜][

BˆBˆB‚ˆ›ˆX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJˆÚ[›™[ÚYˆLM‹ˆÙ^NˆLM‹ˆÜÚ][ÛŽˆLÌ‹ˆ[™ÝˆLÌ‹ˆ
HOˆ]\›“›ÝHÂˆ]\›“›ÝHÂˆÜÚ][Û‹ˆ›YÜÎˆˆÚ[›™[ÚYˆ[™ÝˆÙ^KˆÜ›Ý\ˆˆš[™WÜ]Úˆˆ™\Ù\™Yˆˆ™[X\ÙNˆˆZYWØÚ[›™[ˆˆ[Žˆˆ™[ØÚ]NˆLˆ[ÙÞˆˆ[ÙÞNˆˆBˆB‚ˆ›ˆZ^\—Ü\˜[Y]\—Ùš^\™J™XÛÜ™Îˆ	–ÊNLM‹LÌŠWJHOˆ›ØÝ[Y[Âˆ]]]^[ØYH™XÎŽÚ]ØØ\XÚ]J™XÛÜ™Ë›[Š
H
ˆLŠNÂˆ›Üˆ
\˜[Y]\—ÚYÚ[›™[Ù]K˜[YJH[ˆ™XÛÜ™ÈÂˆ^[ØY™^[™Ùœ›ÛWÜÛXÙJ	–ÌPK‹ÐËJNÂˆ^[ØY™^[™Ùœ›ÛWÜÛXÙJ	–Êœ\˜[Y]\—ÚYJNÂˆ^[ØY™^[™Ùœ›ÛWÜÛXÙJ	˜Ú[›™[Ù]K×ÛWØž]\Ê
JNÂˆ^[ØY™^[™Ùœ›ÛWÜÛXÙJ	˜[YK×ÛWØž]\Ê
JNÂˆBˆ]]]]™[ÜÝ™X[HH™XÈVÌLK^[ØY›[Š
H\ÈNNÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	œ^[ØY
NÂˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	—ÝLÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽM—ÝLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊK™^XÝ
“Z^\ˆ\˜[Y]\ˆš^\™HÚÝ[\œÙHŠBˆB‚ˆ›ˆX[›×Ü›ÛØÛÛÜ—Ùš^\™J
HOˆ›ØÝ[Y[Âˆ]]]›ÝWÜ™XÛÜ™ÈH™XÎŽ›™]Ê
NÂˆ›Üˆ
Ú[›™[ÚYZYWØÚ[›™[
H[ˆÊLM‹LÊK

K
KÍJWHÂˆ]]]™XÛÜ™HÌÈNÂˆ™XÛÜ™Í‹—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™Í‹‹ŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	˜Ú[›™[ÚY×ÛWØž]\Ê
JNÂˆ™XÛÜ™Î‹ŒL—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒNLLÌ‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌL‹‹ŒMK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆ™XÛÜ™ÌNWHHZYWØÚ[›™[Âˆ™XÛÜ™ÌŒWHHLÂˆ›ÝWÜ™XÛÜ™Ë™^[™Ùœ›ÛWÜÛXÙJ	œ™XÛÜ™
NÂˆBˆ]]]]™[ÜÝ™X[HH™XÈVÌKKËÌ—NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	››ÝWÜ™XÛÜ™ÊNÂˆ]]]ž]\ÈHˆ‘“‹×Ý™XÊ
NÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	LÌ‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	ŽMLM‹×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆž]\Ë™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂˆ›ØÝ[Y[Žœ\œÙJ	˜ž]\ÊK™^XÝ
”X[›È›ÛÛÛÜˆš^\™HÚÝ[\œÙHŠBˆB‚ˆÖÝ\ÝBˆ›ˆ\™›Ü›X[˜ÙWÛ[ÙWÜ™\Ù\™\×Ú]×ØÛÛ™šYÝ\™YØÛ\ÝšYÙÙ\—ØÚ[›™[Ùœ›ÛWÛ]™WØ]Y][ÛŠ
HÂˆ\ÜÙ\J\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž“›ÝSÛˆÂˆÚ[›™[ˆˆÙ^NˆŒˆ™[ØÚ]NˆLˆKˆˆ
JNÂˆ\ÜÙ\J\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž“›ÝSÙ™ˆÂˆÚ[›™[ˆˆÙ^NˆŒˆ™[ØÚ]NˆˆKˆˆ
JNÂˆ\ÜÙ\J\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[ŽÛÛ›ÛÚ[™ÙHÂˆÚ[›™[ˆˆÛÛ›Û\Žˆˆ˜[YNˆLËˆKˆˆ
JNÂˆ\ÜÙ\JZ\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž“›ÝSÛˆÂˆÚ[›™[ˆKˆÙ^NˆŒˆ™[ØÚ]NˆLˆKˆˆ
JNÂˆ\ÜÙ\J\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž“›ÝSÛˆÂˆÚ[›™[ˆ‹ˆÙ^NˆŒˆ™[ØÚ]NˆLˆKˆ‹ˆ
JNÂˆ\ÜÙ\JZ\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž“›ÝSÛˆÂˆÚ[›™[ˆ‹ˆÙ^NˆŒˆ™[ØÚ]NˆLˆKˆˆ
JNÂˆ\ÜÙ\JZ\×Ü\™›Ü›X[˜ÙWØÛ\ÝšYÙÙ\—ØÚ[›™[Ù]™[
ˆZYR[œ]]™[Ž•[Z[™ÐÛØÚËˆˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØÚÜ™Ù]XÝ[Û—Û˜[Y\×Ú[™\œÚ[Ûœ×Ø[™ØÛÛ[[Û—Ù^[œÚ[ÛœÊ
HÂˆ]›ÝHHÙ^KÚ[›™[ÚY]\›“›ÝHÂˆÜÚ][ÛŽˆM‹ˆÚ[›™[ÚYˆÙ^Kˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]š\œÝÚ[™\œÚ[ÛˆH™XÈVÛ›ÝJL‹ŠK›ÝJŒŠK›ÝJËŠK›ÝJÍ‹ÊWNÂˆ]XZ›Ü—Ûš[H™XÈVÂˆ›ÝJŒŠKˆ›ÝJŠKˆ›ÝJËŠKˆ›ÝJÌKŠKˆ›ÝJÍŠKˆNÂˆ]×ÜÚ^H™XÈVÛ›ÝJŒŠK›ÝJŠK›ÝJËŠK›ÝJŽKŠWNÂˆ]WÛZ[›Ü—ÜÙ]™[H™XÈVÛ›ÝJMËŠK›ÝJŒŠK›ÝJŠK›ÝJËŠWNÂ‚ˆ\ÜÙ\Ù\HJ]XÝØÚÜ™Û˜[YJ	™š\œÝÚ[™\œÚ[Û‹‹MŠKÈXZ›Ü‹ÑHŠNÂˆ\ÜÙ\Ù\HJ]XÝØÚÜ™Û˜[YJ	›XZ›Ü—Ûš[‹MŠKÈXZŽHŠNÂˆ\ÜÙ\Ù\HJ]XÝØÚÜ™Û˜[YJ	˜×ÜÚ^‹MŠKÈˆŠNÂˆ\ÜÙ\Ù\HJ]XÝØÚÜ™Û˜[YJ	˜WÛZ[›Ü—ÜÙ]™[‹MŠKHMÈŠNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØY˜[˜ÙYØÚÜ™ÜÝ[\×Ý\ÙWÝZ\—ØÚ›ÛX]X×Ú[\˜[Ê
HÂˆ]Ø\Ù\ÈHÂˆ
X[›Ô›ÛÚÜ™Ý[\Ž“XZ›Ü‹™XÈVÍŒËŽWJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž“Z[›Ü‹™XÈVÍŒŒËËŽWJKˆ
X[›Ô›ÛÚÜ™Ý[\ŽYK™XÈVÍŒËÍJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž“Z[›ÜYK™XÈVÍŒŒËËÍJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž‘[Z[š\ÚYË™XÈVÍŒŒË‹ŽWJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž’[‘[Z[š\ÚYË™XÈVÍŒŒË‹ÌJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž“Z[›Ü“XZ›ÜË™XÈVÍŒŒËËÌWJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[Ñ›]K™XÈVÍŒ‹ÌJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[ÔÚ\œK™XÈVÍŒŽÌJKˆ
ˆX[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[Ñ›]Kˆ™XÈVÍŒËÌÌ×Kˆ
Kˆ
ˆX[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[ÔÚ\œKˆ™XÈVÍŒËÌÍWKˆ
Kˆ
X[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[K™XÈVÍŒËÌÍJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž“XZ›ÜŽK™XÈVÍŒËÌKÍJKˆ
X[›Ô›ÛÚÜ™Ý[\Ž“Z[›ÜŽK™XÈVÍŒŒËËÌÍJKˆ
ˆX[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[LKˆ™XÈVÍŒËÌÍÍ×Kˆ
Kˆ
X[›Ô›ÛÚÜ™Ý[\Ž“Z[›ÜŒLK™XÈVÍŒŒËËÌÍÍ×JKˆ
ˆX[›Ô›ÛÚÜ™Ý[\Ž‘ÛZ[˜[LËˆ™XÈVÍŒËÌÍÍËWKˆ
KˆNÂ‚ˆ›Üˆ
Ý[\^XÝY
H[ˆØ\Ù\ÈÂˆ\ÜÙ\Ù\HJˆÝ[\œ]Ú\ÊŒX[›Ô›ÛØØ[NŽ“›Û™K
Kˆ^XÝYˆžßH‹ˆÝ[\›X™[

Kˆ
NÂˆBˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÝÜÙÝÛ—ØÚÜ™ÜÝ[\×ÚÙY\ÝWØÛXÚÙYÜ]ÚÛÛ—ÝÜ

HÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž•ÜÝÛ•šXYœ]Ú\ÊX[›Ô›ÛØØ[NŽ“XZ›Ü‹
Kˆ™XÈVÍMËŒKˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž•ÜÝÛ”Ù]™[œ]Ú\ÊX[›Ô›ÛØØ[NŽ“XZ›Ü‹
Kˆ™XÈVÍLËMËŒKˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž•ÜÝÛ•šXYœ]Ú\ÊŽKX[›Ô›ÛØØ[NŽ“XZ›Ü‹ŠKˆ™XÈVÍŒ‹‹ŽWKˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž•ÜÝÛ•šXYœ]Ú\ÊX[›Ô›ÛØØ[NŽ“›Û™K
Kˆ™XÈVÍMËŒKˆ˜[ˆ[œÙ]ØØ[H˜[È˜XÚÈÈHÙ[XÝYÙ^IÜÈXZ›ÜˆØØ[H‹ˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž•ÜÝÛ•šXYœ]Ú\ÊX[›Ô›ÛØØ[NŽ“XZ›Ü‹
Kˆ™XÎŽLMŽŽ›™]Ê
KˆHÝÙ\ÝRQHÙ^HØ[››Ýš]HÚÜ™[™\›™X]]‹ˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜØØ[WØÚÜ™ÜÝ[\×Ù›ÛÝ×Û›Û—Ø×ÚÙ^WÜ›ÛÝÊ
HÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž”ØØ[UšXYœ]Ú\ÊŽKX[›Ô›ÛØØ[NŽ“XZ›Ü‹ŠKˆ™XÈVÍŽKÌËÍ—KˆH\ÈHšYØØ[HYÜ™YH[ˆXZ›Üˆ‹ˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž”ØØ[TÙ]™[œ]Ú\ÊŽKX[›Ô›ÛØØ[NŽ“XZ›Ü‹ŠKˆ™XÈVÍŽKÌËÍ‹ÎWKˆ
NÂˆ\ÜÙ\Ù\HJˆX[›Ô›ÛÚÜ™Ý[\Ž”ØØ[UšXYœ]Ú\ÊŒX[›Ô›ÛØØ[NŽ“˜]\˜[Z[›Ü‹JKˆ™XÈVÍŒ×KˆÈ\ÈH\™ØØ[HYÜ™YH[ˆH˜]\˜[Z[›Üˆ‹ˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ]]ÛX]X×ÜØØ[WÙ]XÝ[Û—Ý\Ù\×ÜØØ[WÙš]Ø[™ÝÛšX×ØÛÛ^

HÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍŒŒ‹KËŽKÌKÌ—JKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆˆØØ[NˆX[›Ô›ÛØØ[NŽ“XZ›Ü‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍMËNKŒŒ‹KËŽWJKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆKˆØØ[NˆX[›Ô›ÛØØ[NŽ“˜]\˜[Z[›Ü‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍMËNKŒŒ‹KŽŽWJKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆKˆØØ[NˆX[›Ô›ÛØØ[NŽ’\›[ÛšXÓZ[›Ü‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍŒŒ‹ËŽKÌ—JKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆˆØØ[NˆX[›Ô›ÛØØ[NŽ“XZ›Ü”[]ÛšXËˆJBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜØØ[\×Ý\ÙWÙØÝ[Y[YÚ[\˜[×Ø[™Ø]]×Ù]XÝ[ÛŠ
HÂˆ]Ø\Ù\ÎˆÊX[›Ô›ÛØØ[KNÝLMŽÈK	‰ÜÝ]XÈÝNJNÈÎWHHÂˆ
ˆX[›Ô›ÛØØ[NŽ‘ÜšX[‹ˆ‹ˆÍŒ‹KËŽKÌKÌ‹ÍKˆ	–Ì‹ËKËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[ÛšX[”Ú\œš]™KˆˆÍŒŒ‹KŽŽKÌKÌ—Kˆ	–Ì‹KKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[ÛšX[]YÛY[YÚ\œÛËˆˆÍŒŒËKŽŽKÌKÌ—Kˆ	–ÌËKKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ‘ÝX›R\›[ÛšXÓXZ›Ü‹ˆˆÍŒŒKKËŽÌKÌ—Kˆ	–ÌKKËLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[”Ú\œÛËˆˆÍŒŒË‹ËŽKÌKÌ—Kˆ	–ÌË‹ËKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[”Ú\œÛÔÚ\œÚ^ˆˆÍŒŒË‹ËÌÌKÌ—Kˆ	–ÌË‹ËLLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[‘›]™YKˆˆÍŒŒ‹ŒË‹ËŽKÌKÌ—Kˆ	–Ì‹Ë‹ËKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[]YÛY[YÚ\œÛËˆˆÍŒŒË‹ŽŽKÌKÌ—Kˆ	–ÌË‹KLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“™X\Û][“Z[›Ü‹ˆˆÍŒŒKŒËKËŽÌKÌ—Kˆ	–ÌKËKËLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“™X\Û][“XZ›Ü‹ˆˆÍŒŒKŒËKËŽKÌKÌ—Kˆ	–ÌKËKËKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[™Ø\šX[“Z[›Ü‹ˆˆÍŒŒ‹ŒË‹ËŽÌKÌ—Kˆ	–Ì‹Ë‹ËLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[™Ø\šX[“XZ›Ü‹ˆˆÍŒŒË‹ËŽKÌÌ—Kˆ	–ÌË‹ËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[™Ø\šX[“XZ›Ü“[ÙUÛËˆˆÍŒŒKŒË‹ËŽKÌ—Kˆ	–ÌKË‹ËWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ•ZÜ˜Z[šX[‘ÜšX[‘›]ÛËˆˆÍŒŒKŒË‹ËŽKÌÌ—Kˆ	–ÌKË‹ËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’\›[ÛšXÓZ[›Ü‘›]š]™KˆˆÍŒŒ‹ŒËK‹ŽÌKÌ—Kˆ	–Ì‹ËK‹LWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“Y[ÙXÓZ[›Ü”Ú\œš]™KˆˆÍŒŒ‹ŒËKŽŽKÌKÌ—Kˆ	–Ì‹ËKKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ[\™YÛZ[˜[˜]\˜[Ú^ˆˆÍŒŒKŒË‹ŽKÌÌ—Kˆ	–ÌKË‹KLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[]YÛY[YÚ\œ™YKˆˆÍŒŒ‹K‹ŽŽKÌKÌ—Kˆ	–Ì‹K‹KLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ”\œÚX[‹ˆˆÍŒŒKK‹ŽÌKÌ—Kˆ	–ÌKK‹LWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ÜšY[[ˆˆÍŒŒKK‹ŽKÌÌ—Kˆ	–ÌKK‹KLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ‘ÜšX[”Ú\œ›Ý\‹ˆ‹ˆÍŒ‹KŽŽKÌKÌ‹ÍKˆ	–Ì‹Ë‹ËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ‘ÜšX[‘›]š]™Kˆ‹ˆÍŒ‹KËŽÌKÌ‹ÍKˆ	–Ì‹ËK‹KLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ”žYÚX[‹ˆˆÍKËŽKÌKÌ‹ÍÍ—Kˆ	–ÌKËKËLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ”žYÚX[‘›]›Ý\‹ˆˆÍŒŒKŒËËŽÌÌ—Kˆ	–ÌKËËLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ•[˜\žYÚX[‹ˆˆÍŒŒKŒËËŽŽKÌ—Kˆ	–ÌKËËWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“YX[‹ˆKˆÍKËŽKÌKÌ‹ÍÍ‹Í×Kˆ	–Ì‹‹ËKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“Z^ÛYX[‹ˆËˆÍËŽKÌKÌ‹ÍÍ‹ÍËÎWKˆ	–Ì‹KËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“Z^ÛYX[‘›]ÛËˆˆÍŒŒKKËŽKÌÌ—Kˆ	–ÌKKËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ØÜšX[‹ˆLKˆÍÌKÌ‹ÍÍ‹ÍËÎKK×Kˆ	–ÌKËK‹LKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ØÜšX[“˜]\˜[ÛËˆˆÍŒŒ‹ŒËK‹ŽÌÌ—Kˆ	–Ì‹ËK‹LKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ØÜšX[“˜]\˜[Ú^ˆˆÍŒŒKŒËK‹ŽKÌÌ—Kˆ	–ÌKËK‹KLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ØÜšX[‘ÝX›Q›]Ù]™[‹ˆˆÍŒŒKŒËK‹ŽŽKÌ—Kˆ	–ÌKËK‹WKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“ØÜšX[‘ÝX›Q›]™YQÝX›Q›]Ù]™[‹ˆˆÍŒŒKŒ‹K‹ŽŽKÌ—Kˆ	–ÌK‹K‹WKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ•ÚÛUÛ™KˆˆÍŒŒ‹‹ŽÌÌ‹ÍKˆ	–Ì‹‹LKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ›Y\ËˆˆÍŒŒËK‹ËÌÌ‹ÍWKˆ	–ÌËK‹ËLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ“Y[ÙXÓZ[›Ü‹ˆKˆÍŽKÌKÌ‹ÍÍ‹ÎWKˆ	–Ì‹ËKËKLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ•ÚÛR[‘[Z[š\ÚYˆˆÍŒŒ‹ŒËK‹ŽŽKÌWKˆ	–Ì‹ËK‹KLWKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ’[•ÚÛQ[Z[š\ÚYˆˆÍŒŒKŒË‹ËŽKÌKˆ	–ÌKË‹ËKLKˆ
Kˆ
ˆX[›Ô›ÛØØ[NŽ”Ý\\“ØÜšX[‘ÝX›Q›]Ù]™[‹ˆˆÍŒŒKŒË‹ŽŽKÌ—Kˆ	–ÌKË‹WKˆ
KˆNÂ‚ˆ›Üˆ
ØØ[K›ÛÝ]Ú\Ë[\˜[ÊH[ˆØ\Ù\ÈÂˆ\ÜÙ\Ù\HJØØ[Kš[\˜[Ê
K[\˜[ÊNÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛÜØØ[J	œ]Ú\ÊKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÈ›ÛÝØØ[HJBˆ
NÂˆBˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜØØ[WÜÛ˜\ØÚÛÜÙ\×ÝWÛ™X\™\ÝÚ[—Ü˜[™ÙWÛ›ÝJ
HÂˆ\ÜÙ\Ù\HJˆÛ˜\ÜX[›×Ü›ÛÚÙ^WÝ×ÜØØ[JŒKX[›Ô›ÛØØ[NŽ“XZ›Ü‹Í‹ÊKˆŒˆ˜[ˆ\]X[Y\Ý[˜ÙHYH™\ÛÛ™\ÈÝÛØ\™‹ˆ
NÂˆ\ÜÙ\Ù\HJˆÛ˜\ÜX[›×Ü›ÛÚÙ^WÝ×ÜØØ[JŒËX[›Ô›ÛØØ[NŽ“XZ›Ü‹Í‹ÊKˆŒ‹ˆ
NÂˆ\ÜÙ\Ù\HJˆÛ˜\ÜX[›×Ü›ÛÚÙ^WÝ×ÜØØ[JŒKX[›Ô›ÛØØ[NŽ“XZ›Ü‹ŒKÌ
KˆŒ‹ˆH™\Ý[Ý^\ÈÚ][ˆHš\ÚX›HÙ^X›Ø\™˜[™ÙH‹ˆ
NÂˆ\ÜÙ\Ù\HJˆÛ˜\ÜX[›×Ü›ÛÚÙ^WÝ×ÜØØ[JŒKX[›Ô›ÛØØ[NŽ“›Û™KÍ‹ÊKˆŒKˆ“›Û™HXØÙ\È]™\žH]Ú‹ˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÜ™XÛÜ™ÜØØ[WÜÛ˜\ØÚ[™Ù\×ÛÛ›WÛÝ]ÛÙ—ÜØØ[WÜ]Ú\Ê
HÂˆ]]]›Ý\ÈH™XÈVÂˆ]\›“›ÝHÂˆÙ^NˆŒKˆ™[ØÚ]NˆLËˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÙ^NˆËˆ™[ØÚ]NˆÌ‹ˆ[™ÝˆM‹ˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÙ^NˆLËˆ™[ØÚ]NˆLKˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆNÂˆÛ˜\Ü™XÛÜ™YÛZYWÛ›Ý\×Ý×ÜØØ[Jˆ	›]]›Ý\Ëˆ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆˆØØ[NˆX[›Ô›ÛØØ[NŽ“XZ›Ü‹ˆKˆ
NÂ‚ˆ\ÜÙ\Ù\HJˆ›Ý\Ëš]\Š
K›X\
›Ý_›ÝKšÙ^JK˜ÛÛXÝŽ™XÏÏŠ
Kˆ™XÈVÍŒËL×Bˆ
NÂˆ\ÜÙ\Ù\HJˆ›Ý\Ëš]\Š
K›X\
›Ý_›ÝK™[ØÚ]JK˜ÛÛXÝŽ™XÏÏŠ
Kˆ™XÈVÎLËÌ‹LWBˆ
NÂˆ\ÜÙ\Ù\HJˆ›Ý\Ëš]\Š
K›X\
›Ý_›ÝK›[™Ý
K˜ÛÛXÝŽ™XÏÏŠ
Kˆ™XÈVÍM‹Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ]]ÛX]X×ÜØØ[WÙ]XÝ[Û—ÝØZ]×Ù›Ü—Ý™YWÛ›Ý\×Ú[—Ù\Ý[˜ÝÜ]ÚØÛ\ÜÙ\Ê
HÂˆ\ÜÙ\Ù\HJ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍŒŒ—JK›Û™JNÂˆ\ÜÙ\Ù\HJ]XÝÜX[›×Ü›ÛÜØØ[J	–ÍŒŒJK›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ]]ÛX]X×ÜØØ[WÙ]XÝ[Û—Ý\Ù\×ÜÙ[XÝYÛ›Ý\×ÛÜ—ÝWÝ\™Ù]ØÚ[›™[ÜØÛÜ™J
HÂˆ]›ÝHHÜÚ][Û‹Ú[›™[ÚYÙ^_]\›“›ÝHÂˆÜÚ][Û‹ˆÚ[›™[ÚYˆÙ^Kˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ˜[YNˆ›Û™Kˆ[™ÝÝXÚÜÎˆ›Û™Kˆ›Ý\Îˆ™XÈVÂˆ›ÝJŒ
Kˆ›ÝJKŒJKˆ›ÝJM‹
Kˆ›ÝJNL‹ÊKˆ›ÝJŽMÊKˆ›ÝJÎŒ
Kˆ›ÝJ
Kˆ›ÝJMÍ‹ŽJKˆKˆÛÛ›Û\œÎˆ™XÎŽ›™]Ê
Kˆ[YWÛX\šÙ\œÎˆ™XÎŽ›™]Ê
KˆNÂˆ]Ù[XÝYH•™YTÙ]Ž™œ›ÛJÊËÊK
Ë
K
ËJK
ËŠWJNÂ‚ˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛØÚ[›™[ÜØØ[J	œ]\›‹Ë	œÙ[XÝY
KˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆKˆØØ[NˆX[›Ô›ÛØØ[NŽ“˜]\˜[Z[›Ü‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJˆ]XÝÜX[›×Ü›ÛØÚ[›™[ÜØØ[J	œ]\›‹Ë	•™YTÙ]Ž›™]Ê
JKˆÛÛYJ]XÝYX[›Ô›ÛØØ[HÂˆ›ÛÝˆˆØØ[NˆX[›Ô›ÛØØ[NŽ“XZ›Ü‹ˆJBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØÛÛÜ—ØÚ[™ÙWÜ™XÛÛÜœ×ÜÙ[XÝ[Û—ÛÜ—Ý\™Ù]ØÚ[›™[Ø[™Ü™\Ù\™\×Ý[šÛ›ÝÛ—Øš]Ê
HÂˆ]]]ØÝ[Y[HX[›×Ü›ÛØÛÛÜ—Ùš^\™J
NÂˆ]Ù[XÝYH•™YTÙ]Ž™œ›ÛJÊË
WJNÂˆ\ÜÙ\Ù\HJˆ™XÛÛÜ—ÜX[›×Ü›ÛÛ›Ý\Ê	›]]ØÝ[Y[Ë	œÙ[XÝYŠKˆÚÊJBˆ
NÂ‚ˆ]›Ý\ÈHØÝ[Y[œ]\›œÊ
K[Ü˜\

Kœ™[[Ý™J
K››Ý\ÎÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌK›ZYWØÚ[›™[LŠNÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌWK›ZYWØÚ[›™[
NÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌ—K›ZYWØÚ[›™[ÍJNÂ‚ˆ\ÜÙ\Ù\HJˆ™XÛÛÜ—ÜX[›×Ü›ÛÛ›Ý\Ê	›]]ØÝ[Y[Ë	•™YTÙ]Ž›™]Ê
KŠKˆÚÊŠBˆ
NÂˆ]›Ý\ÈHØÝ[Y[œ]\›œÊ
K[Ü˜\

Kœ™[[Ý™J
K››Ý\ÎÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌK›ZYWØÚ[›™[MŠNÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌWK›ZYWØÚ[›™[ŠNÂˆ\ÜÙ\Ù\HJ›Ý\ÖÌ—K›ZYWØÚ[›™[ÍJNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜÙ[XÝØžWØÛÛÜ—Ý\Ù\×ÝWÜXÚÙ\—Ø[™ØXÝ]™WØÚ[›™[

HÂˆ]]\›ˆHX[›×Ü›ÛØÛÛÜ—Ùš^\™J
Kœ]\›œÊ
K[Ü˜\

Kœ™[[Ý™J
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛÜ—ÙÜ›Ý\Û›ÝWÚYÊ	œ]\›‹ËÊKˆ™XÈVÊË
WBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛÜ—ÙÜ›Ý\Û›ÝWÚYÊ	œ]\›‹Ë
Kˆ™XÈVÊËJWBˆ
NÂˆ\ÜÙ\JX[›×Ü›ÛØÛÛÜ—ÙÜ›Ý\Û›ÝWÚYÊ	œ]\›‹ËJKš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜÙ[XÝÛÙÝ\Ù\×ÝWÛXZ[—ÙÜšYØ[™ÚÙY\×Û›ÝWÙÜ›Ý\×ÝÙÙ]\Š
HÂˆ]›ÝHHÜÚ][Û‹Ú[›™[ÚYÜ›Ý\]\›“›ÝHÂˆÜÚ][Û‹ˆÚ[›™[ÚYˆÜ›Ý\ˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ›ÝJ‹
Kˆ›ÝJK‹
Kˆ›ÝJ‹JKˆ›ÝJK‹JKˆ›ÝJKË
KˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÛÙÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹
Kˆ™XÈVÊË‹JK
Ë‹ŠK
Ë‹ÊWBˆ
NÂˆ\ÜÙ\JX[›×Ü›ÛÛÙÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹
Kš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÛÝ™\›\ÜÙ[XÝ[Û—Ú\×ØÚ[›™[ÜØÛÜYØ[™ÚÙY\×ÙÜ›Ý\×ÝÙÙ]\Š
HÂˆ]›ÝHHÜÚ][Û‹[™ÝÙ^KÚ[›™[ÚYÜ›Ý\]\›“›ÝHÂˆÜÚ][Û‹ˆ[™ÝˆÙ^KˆÚ[›™[ÚYˆÜ›Ý\ˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ›ÝJM‹‹
Kˆ›ÝJŒ‹ÊKˆ›ÝJ‹
Kˆ›ÝJM‹MK‹
Kˆ›ÝJLË
Kˆ›ÝJŒ‹ÊKˆ›ÝJL‹
KˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÛÝ™\›\Û›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹˜[ÙJKˆ™XÈVÊË‹JK
Ë‹ŠK
Ë‹
WBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÛÝ™\›\Û›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹YJKˆ™XÈVÊË‹JK
Ë‹
WBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÝ[YWÜ˜[™ÙWÜÙ[XÝ×Û›ÝWÜÝ\×Ø[™Ú[˜ÛY\×ÝZ\—ÙÜ›Ý\Ê
HÂˆ]›ÝHHÜÚ][Û‹Ú[›™[ÚYÜ›Ý\]\›“›ÝHÂˆÜÚ][Û‹ˆ[™ÝˆNL‹ˆÚ[›™[ÚYˆÜ›Ý\ˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ›ÝJ‹
Kˆ›ÝJ‹
Kˆ›ÝJM‹‹
Kˆ›ÝJLŒ‹
Kˆ›ÝJË
Kˆ›ÝJ‹
KˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÝ[YWÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹LŒ
Kˆ™XÈVÊË‹JK
Ë‹ŠK
Ë‹
WBˆ
NÂˆ\ÜÙ\JX[›×Ü›ÛÝ[YWÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹LŒLŒ
Kš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜ]ÚÜ˜[™ÙWÜÙ[XÝ[Û—Ú\×Ú[˜Û\Ú]™WØÚ[›™[ÜØÛÜYØ[™ÙÜ›Ý\Ø]Ø\™J
HÂˆ]›ÝHHÙ^KÚ[›™[ÚYÜ›Ý\]\›“›ÝHÂˆÙ^KˆÚ[›™[ÚYˆÜ›Ý\ˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ›ÝJŒ‹
Kˆ›ÝJ‹
Kˆ›ÝJÌ‹‹
Kˆ›ÝJŽ‹
Kˆ›ÝJŒ‹Ë
KˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜ]ÚÜ˜[™ÙWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹Œ
Kˆ™XÈVÊË‹
K
Ë‹JK
Ë‹ÊWBˆ
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÚÙ^WØ]ÞJLŒLŒLËŒÍ‹ÊKÊNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÚÙ^WØ]ÞJÌLKŒLŒLËŒÍ‹ÊKŒ
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÚÙ^WØ]ÞJ—ÌŒLŒLËŒÍ‹ÊKÍŠNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÝ[YWÜÙ[XÝ[Û—ØÛÝ™\œ×ÜÙ[XÝYÛ›Ý\×ØXÜ›ÜÜ×ØÚ[›™[Ê
HÂˆ]›ÝHHÜÚ][Û‹[™ÝÚ[›™[ÚY]\›“›ÝHÂˆÜÚ][Û‹ˆ[™ÝˆÚ[›™[ÚYˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ›ÝJŠKˆ›ÝJM‹ÊKˆ›ÝJM‹ŠKˆ›ÝJŒŠKˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂˆ]Ù[XÝYHÊË‹
K
ËË
K
Ë‹JK
Ë‹ŠWBˆš[×Ú]\Š
Bˆ˜ÛÛXÝŽ•™YTÙ]ÏŠ
NÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜÙ[XÝ[Û—Ý[YWÜ˜[™ÙJ	œ]\›‹Ë	œÙ[XÝY
KˆÛÛYJ
M
JBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜÙ[XÝ[Û—Ý[YWÜ˜[™ÙJ	œ]\›‹	œÙ[XÝY
Kˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÝ[YWÜÙ[XÝ[Û—Û[Ý™\×ØžWÚ]×ÛÝÛ—ÝÚY

HÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜÚYÝ[YWÜ˜[™ÙJM‹M[YTÙ[XÝ[Û”ÚYŽ”™]š[Ý\ÊKˆ
MŠBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜÚYÝ[YWÜ˜[™ÙJM‹M[YTÙ[XÝ[Û”ÚYŽ“™^
Kˆ
MNLŠBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜÚYÝ[YWÜ˜[™ÙJÌ‹[YTÙ[XÝ[Û”ÚYŽ”™]š[Ý\ÊKˆ

Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÝ[YWÜ[\—Ù˜Y×ÜÛ˜\×Ý×ÙÜšYØ[™ÚÙY\×ØWÜÜÚ]]™WÜ˜[™ÙJ
HÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÝ[YWÝXÚ×Ø]Þ
MLŒLŒKŒ
K
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÝ[YWÝXÚ×Ø]Þ
ŒLŒKŒ
K
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÝ[YWÜ˜[™ÙWÙœ›ÛWÙ˜YÊ
K
ÌŠJNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÝ[YWÜ˜[™ÙWÙœ›ÛWÙ˜YÊM‹M‹
K
M‹LŒ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÙ\XØ]WÝ\Ù\×Ý[YWÜÙ[XÝ[Û—ÝÚYÝÚ[—Ü™\Ù[

HÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÙ\XØ]WÛÙ™œÙ]
M‹›Û™JKÌŠNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÙ\XØ]WÛÙ™œÙ]
M‹ÛÛYJ
NLŠJJKNLŠNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÙ\XØ]WÛÙ™œÙ]
M‹ÛÛYJ
NL‹NLŠJJKÌŠNÂˆB‚ˆÖÝ\ÝBˆ›ˆZ^\—Ù˜Y\—ØØ[™Y]WÜ™\]Z\™\×Ý[š\]YWÞ™\›×ÜØÛÜWÝ›Û[YWØ[™Ü[—Ü™XÛÜ™Ê
HÂˆ]\™Ù]HÝLMˆŽÂˆ]ØÝ[Y[HZ^\—Ü\˜[Y]\—Ùš^\™J	–ÊNL‹\™Ù]L—Î
K
NLË\™Ù]
WJNÂˆ]\˜[Y]\œÈHØÝ[Y[ˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ØÛÛ›ÛÊ	œ\˜[Y]\œË
Kš\×ÜÛÛYJ
JNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ØÛÛ›ÛÊ	œ\˜[Y]\œËJKš\×Û›Û™J
JNÂ‚ˆ]\XØ]WÝ›Û[YHHZ^\—Ü\˜[Y]\—Ùš^\™J	–Âˆ
NL‹\™Ù]L—Î
Kˆ
NL‹\™Ù]LWÌ
Kˆ
NLË\™Ù]
KˆJNÂˆ]\˜[Y]\œÈH\XØ]WÝ›Û[YBˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ØÛÛ›ÛÊ	œ\˜[Y]\œË
Kš\×Û›Û™J
JNÂ‚ˆ]›Ûž™\›×ÜØÛÜHHZ^\—Ü\˜[Y]\—Ùš^\™J	–Âˆ
NL‹\™Ù]
HLÊKL—Î
Kˆ
NLË\™Ù]
HLÊK
KˆJNÂˆ]\˜[Y]\œÈH›Ûž™\›×ÜØÛÜBˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ØÛÛ›ÛÊ	œ\˜[Y]\œË
Kš\×Û›Û™J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZ^\—ÜÝ\™[×ÜÙ\\˜][Û—ØÛÛ›ÛÜ™\]Z\™\×Ý[š\]YWÞ™\›×ÜØÛÜWÜ™XÛÜ™

HÂˆ]\™Ù]HÝLMˆŽÂˆ]ØÝ[Y[HZ^\—Ü\˜[Y]\—Ùš^\™J	–ÊNM\™Ù]ÌŠWJNÂˆ]\˜[Y]\œÈHØÝ[Y[ˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\Ù\HJˆØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][ÛŠ	œ\˜[Y]\œË
K›X\
Ÿ‹˜[YJ
JKˆÛÛYJÌŠBˆ
NÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][ÛŠ	œ\˜[Y]\œËJKš\×Û›Û™J
JNÂ‚ˆ]\XØ]HHZ^\—Ü\˜[Y]\—Ùš^\™J	–ÊNM\™Ù]ÌŠK
NM\™Ù]LÌŠWJNÂˆ]\˜[Y]\œÈH\XØ]Bˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][ÛŠ	œ\˜[Y]\œË
Kš\×Û›Û™J
JNÂ‚ˆ]ØÛÜYHZ^\—Ü\˜[Y]\—Ùš^\™J	–ÊNM\™Ù]
HLÊKÌŠWJNÂˆ]\˜[Y]\œÈHØÛÜYˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][ÛŠ	œ\˜[Y]\œË
Kš\×Û›Û™J
JNÂ‚ˆ]Ý]ÛÙ—Ü˜[™ÙHHZ^\—Ü\˜[Y]\—Ùš^\™J	–ÊNM\™Ù]JWJNÂˆ]\˜[Y]\œÈHÝ]ÛÙ—Ü˜[™ÙBˆ›Z^\—Ü\˜[Y]\—Ü™XÛÜ™Ê
Bˆ™^XÝ
“Z^\ˆ\˜[Y]\ˆ™XÛÜ™ÈÚÝ[XÛÙHŠNÂˆ\ÜÙ\JØ[™Y]WÛZ^\—ÜÝ\™[×ÜÙ\\˜][ÛŠ	œ\˜[Y]\œË
Kš\×Û›Û™J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÙ]šXÙWÜÙ[XÝ[Ûœ×Ü›Ý[™Ýš\ÛÜ\]YWÜÜÚYÊ
HÂˆ][œ]ÚYH˜[ØN˜Ø\™RÙ^X›Ø\™ÜºfêˆŽÂˆ]Ý]]ÚYHÛÜ™SRQNŒLŒÍMÎPPÑQˆŽÂˆ]]]˜[š×Û\ØˆHÓ›Û™NÈM—NÂˆ]]]˜[š×ÛØˆHÓ›Û™NÈM—NÂˆ]]]›ÙÜ˜[\ÈHÓ›Û™NÈM—NÂˆ˜[š×Û\Ø–ÌHHÛÛYJ
NÂˆ˜[š×ÛØ–ÌHHÛÛYJLÊNÂˆ˜[š×ÛØ–ÌMWHHÛÛYJ
NÂˆ›ÙÜ˜[\ÖÌHHÛÛYJ
NÂˆ›ÙÜ˜[\ÖÌMWHHÛÛYJLÊNÂˆ]Ù[XÝ[ÛœÈHZYQ]šXÙTÙ[XÝ[ÛœÈÂˆ[œ]ÚYˆÛÛYJ[œ]ÚY×ÛÝÛ™Y

JKˆÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\ˆYKˆÝ]]ÚYˆÛÛYJÝ]]ÚY×ÛÝÛ™Y

JKˆÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\ˆYKˆÝ]]ØÚ[›™[Ü›Ý]\ÎˆÂˆ]]]›Ý]\ÈHÝŽ˜\œ˜^NŽ™œ›ÛWÙ›Šß›Û™JNÂˆ›Ý]\ÖÌHHÛÛYJ˜[ØN™]šXÙKÛ™H‹×ÛÝÛ™Y

JNÂˆ›Ý]\ÖÌMWHHÛÛYJ›XXÛÜÎœÜLMH‹×ÛÝÛ™Y

JNÂˆ›Ý]\ÂˆKˆ˜[š×Û\Ø‹ˆ˜[š×ÛØ‹ˆ›ÙÜ˜[\ËˆÙ[™ØÛØÚÎˆYKˆÙ[™Ü^[\ÝÛ›Ý\ÎˆYKˆÙ[™Ü]\›—Û›Ý\ÎˆYKˆš^YÝ™[ØÚ]NˆYKˆ\™›Ü›X[˜ÙWØÚ[›™[ˆ‹ˆ™XÛÜ™Ø]Y[Îˆ˜[ÙKˆ™XÛÜ™Û›Ý\ÎˆYKˆ™XÛÜ™Ø]]ÛX][ÛŽˆYKˆ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\ŽˆÍˆ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[ˆˆ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÎˆLŽKˆNÂˆ][˜ÛÙYH[˜ÛÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊ	œÙ[XÝ[ÛœÊNÂ‚ˆ\ÜÙ\Ù\HJ\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊ	™[˜ÛÙY
KÙ[XÝ[ÛœÊNÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÛÝ]]Ü›Ý]\Ê	™[˜ÛÙWÛZYWÛÝ]]Ü›Ý]\Êˆ	œÙ[XÝ[ÛœË›Ý]]ØÚ[›™[Ü›Ý]\Âˆ
JKˆÛÛYJÙ[XÝ[ÛœË›Ý]]ØÚ[›™[Ü›Ý]\ÊBˆ
NÂˆ\ÜÙ\J\œÙWÛZYWÛÝ]]Ü›Ý]\Ê˜œ›ÚÙ[ˆŠKš\×Û›Û™J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÙ]šXÙWÜÙ[XÝ[Ûœ×ÜÚÚ\ÛX[›Ü›YYÛÜ—Ý[šÛ›ÝÛ—Ù[šY\Ê
HÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊˆš[œ]L›Ý]]^^—›Ý]]Ü›Ý]\ÏXœ›ÚÙ[—˜ÛØÚÏZ[˜[Yœ\™›Ü›X[˜ÙWØÚ[›™[LM—œ™XÛÜ™Ø]]ÛX][Û—ØÛÛ›Û\LLŽœ™XÛÜ™Ø]]ÛX][Û—ØÚ[›™[LM—œ™XÛÜ™Ø]]ÛX][Û—Ù›YÜÏKLW›Ý\LWˆ‚ˆ
KˆZYQ]šXÙTÙ[XÝ[ÛœÎŽ™Y˜][

Bˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊš[œ]W›Ý]]W˜ÛØÚÏLW™š^YÝ™[ØÚ]OLWˆŠKˆZYQ]šXÙTÙ[XÝ[ÛœÈÂˆÙ[™ØÛØÚÎˆYKˆš^YÝ™[ØÚ]NˆYKˆ‹“ZYQ]šXÙTÙ[XÝ[ÛœÎŽ™Y˜][

BˆBˆ
NÂˆ]YØXÞHH\œÙWÛZYWÙ]šXÙWÜÙ[XÝ[ÛœÊˆš[œ]W˜ÛÛ›™XÝÚ[œ]ÛÛ—ÜÝ\\L›Ý]]W˜ÛÛ›™XÝÛÝ]]ÛÛ—ÜÝ\\Lœ›ÙÜ˜[\ÏKKKKKKKKKKKKKKKKWˆ‹ˆ
NÂˆ\ÜÙ\Ù\HJYØXÞK˜˜[š×Û\Ø‹Ó›Û™NÈM—JNÂˆ\ÜÙ\Ù\HJYØXÞK˜˜[š×ÛØ‹Ó›Û™NÈM—JNÂˆ\ÜÙ\Ù\HJYØXÞKœ›ÙÜ˜[\ÖÌWKÛÛYJJJNÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWÛÝ]]Ü›ÙÜ˜[\ÊŒKHŠK›Û™JNÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÛÝ]]Ü›ÙÜ˜[\ÊŒLŽKKKKKKKKKKKKKKHŠKˆ›Û™Bˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÛÝ]]Ý˜[Y\Ê‹KKKKKKKKKKKKKKKLŽŠKˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÚ[œ]Ø]]×ØÛÛ›™XÝÜ™\]Z\™\×ÛÜÚ[—Ø[™ÛX]Ú[™×ÜØ]™YÙ]šXÙJ
HÂˆ]]šXÙ\ÈH™XÈVÓZYR[œ]]šXÙHÂˆYˆ˜[ØNšÙ^X›Ø\™‹×ÛÝÛ™Y

Kˆ˜[YNˆ’Ù^X›Ø\™‹×ÛÝÛ™Y

KˆWNÂ‚ˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]
ˆ˜[ÙKˆÛÛYJ˜[ØNšÙ^X›Ø\™ŠKˆ	™]šXÙ\Âˆ
JNÂˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]
ˆYKˆÛÛYJ˜[ØN›Z\ÜÚ[™ÈŠKˆ	™]šXÙ\Âˆ
JNÂˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]
YK›Û™K	™]šXÙ\ÊJNÂˆ\ÜÙ\JÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÚ[œ]
ˆYKˆÛÛYJ˜[ØNšÙ^X›Ø\™ŠKˆ	™]šXÙ\Âˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛÝ]]Ø]]×ØÛÛ›™XÝÜ™\]Z\™\×ÛÜÚ[—Ø[™ÛX]Ú[™×ÜØ]™YÙ]šXÙJ
HÂˆ]]šXÙ\ÈH™XÈVÓZYSÝ]]]šXÙHÂˆYˆ˜ÛÜ™[ZYNœÞ[‹×ÛÝÛ™Y

Kˆ˜[YNˆ”Þ[‹×ÛÝÛ™Y

KˆWNÂ‚ˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]
ˆ˜[ÙKˆÛÛYJ˜ÛÜ™[ZYNœÞ[ŠKˆ	™]šXÙ\Âˆ
JNÂˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]
ˆYKˆÛÛYJ˜ÛÜ™[ZYN›Z\ÜÚ[™ÈŠKˆ	™]šXÙ\Âˆ
JNÂˆ\ÜÙ\J\ÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]
YK›Û™K	™]šXÙ\ÊJNÂˆ\ÜÙ\JÚÝ[Ø]]×ØÛÛ›™XÝÛZYWÛÝ]]
ˆYKˆÛÛYJ˜ÛÜ™[ZYNœÞ[ŠKˆ	™]šXÙ\Âˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ü›Ý[™Ýš\×Ù]šXÙWØÚ[›™[Ø[™ØÛÛ›Û\Š
HÂˆ]X\[™ÈHZYPÛÛ›ÛÚ[™ÙSX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™:fêˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆÍˆNÂˆ][˜ÛÙYH›Ü›X]Jˆš[œ]^ßW˜Ú[›™[^ßW˜ÛÛ›Û\^ßWˆ‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
	›X\[™Ëš[œ]Ù]šXÙWÚY
KˆX\[™Ë˜Ú[›™[ˆX\[™Ë˜ÛÛ›Û\‚ˆ
NÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Ê	™[˜ÛÙY
KÛÛYJX\[™ÊJNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ü›Ý[™Ýš\Û][\WÝ\™Ù]×ÛÛ—ÛÛ™WØØÊ
HÂˆ]X\[™ÜÈH™XÈVÂˆZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™:fêˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆÍˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆŒËˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[ZY‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆËˆ\˜[Y]\—Û˜[YNˆÝ]Ù™ˆ:fêˆ‹×ÛÝÛ™Y

KˆKˆKˆZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™:fêˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆÍˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[ZY‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆˆ\˜[Y]\—Û˜[YNˆ”™\ÛÛ˜[˜ÙH‹×ÛÝÛ™Y

KˆKˆKˆNÂ‚ˆ][˜ÛÙYHÙ\šX[^™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ	›X\[™ÜÊNÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ	™[˜ÛÙY
KX\[™ÜÊNÂˆ\ÜÙ\Ù\HJˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ù›Ü—ØØÊ	›X\[™ÜË˜[ØNšÙ^X›Ø\™:fêˆ‹MKÍ
K›[Š
Kˆ‚ˆ
NÂˆ\ÜÙ\JZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ü×Ù›Ü—ØØÊ	›X\[™ÜË›Ý\ˆ‹MKÍ
Kš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜ×Ü›Ý[™Ýš\Ø[™Ü™\XÙWÜ\—Ü›Ú™XÝÜ]\›—Ý\™Ù]

HÂˆ]›Ú™XÝÜ]HÝŽ™[ŽŽ[\Ù\Š
Kš›Ú[Š˜]]ÛX][Ûˆ›Ú™XÝ:fê‹™›ŠNÂˆ]›Ú™XÝÚÙ^HH›Ú™XÝÜ]×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

NÂˆ]\™Ù]HZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆŒËˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[ZY‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆËˆ\˜[Y]\—Û˜[YNˆÝ]Ù™ˆ:fêˆ‹×ÛÝÛ™Y

KˆNÂˆ][šÈH]\›•œÝÔ\˜[Y]\“[šÈÂˆ›Ú™XÝÜ]ˆ›Ú™XÝÚÙ^K˜ÛÛ™J
Kˆ]\›—ÚYˆˆÝ™X[WØÚ[›™[ˆËˆÝ™X[WÙ›YÜÎˆˆ\™Ù]ˆ\™Ù]˜ÛÛ™J
KˆNÂˆ]Ý\—Ý\™Ù]H]\›•œÝÔ\˜[Y]\“[šÈÂˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆŒËˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[ZY‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆˆ\˜[Y]\—Û˜[YNˆ”™\ÛÛ˜[˜ÙH‹×ÛÝÛ™Y

KˆKˆÝ™X[WØÚ[›™[ˆKˆÝ™X[WÙ›YÜÎˆNˆ‹›[šË˜ÛÛ™J
BˆNÂˆ][˜ÛÙYHÙ\šX[^™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ	–Û[šË˜ÛÛ™J
KÝ\—Ý\™Ù]˜ÛÛ™J
WJNÂˆ\ÜÙ\Ù\HJˆ\œÙWÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÜÊ	™[˜ÛÙY
Kˆ™XÈVÛ[šË˜ÛÛ™J
KÝ\—Ý\™Ù]˜ÛÛ™J
WBˆ
NÂ‚ˆ]™\XÙ[Y[H]\›•œÝÔ\˜[Y]\“[šÈÂˆÝ™X[WØÚ[›™[ˆKˆÝ™X[WÙ›YÜÎˆˆ‹›[šË˜ÛÛ™J
BˆNÂˆ]]][šÜÈH™XÈVÛ[šËÝ\—Ý\™Ù]˜ÛÛ™J
WNÂˆ\Ù\Ü]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊ	›]][šÜË™\XÙ[Y[˜ÛÛ™J
JNÂˆ\ÜÙ\Ù\HJ[šÜË™XÈVÛÝ\—Ý\™Ù]˜ÛÛ™J
K™\XÙ[Y[JNÂˆ\ÜÙ\Ù\HJˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ù›Ü—Ý\™Ù]
	›[šÜË	œ›Ú™XÝÜ]	\™Ù]
KˆÛÛYJ	›[šÜÖÌWJBˆ
NÂˆ\ÜÙ\Jˆ]\›—ÝœÝ×Ü\˜[Y]\—Û[š×Ù›Ü—Ý\™Ù]
	›[šÜË	œ›Ú™XÝÜ]K	\™Ù]
Kš\×Û›Û™J
Bˆ
NÂ‚ˆ™[[Ý™WÜ]\›—ÝœÝ×Ü\˜[Y]\—Û[šÊ	›]][šÜË	œ›Ú™XÝÜ]	\™Ù]
NÂˆ\ÜÙ\Ù\HJ[šÜË™XÈVÛÝ\—Ý\™Ù]JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ]\›—ÝœÝ×Ü\˜[Y]\—Ø]]ÛX][Û—ÛX\×ÛÛ›WÛ›Ü›X[^™YÜÚ[×Ùœ›ÛWÚ]×Û[šÙYÜÝ™X[J
HÂˆ]›Ú™XÝÜ]HÝŽ™[ŽŽ[\Ù\Š
Kš›Ú[Š˜]]ÛX][Ûˆ›Ú™XÝ™›ŠNÂˆ][šÈH]\›•œÝÔ\˜[Y]\“[šÈÂˆ›Ú™XÝÜ]ˆ›Ú™XÝÜ]×ÜÝš[™×ÛÜÜÞJ
Kš[×ÛÝÛ™Y

Kˆ]\›—ÚYˆ‹ˆÝ™X[WØÚ[›™[ˆ‹ˆÝ™X[WÙ›YÜÎˆÌKˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆMËˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆ‹ˆ\˜[Y]\—Û˜[YNˆÝ]Ù™ˆ‹×ÛÝÛ™Y

KˆKˆNÂˆ]Ú[HÜÚ][Û‹Ú[›™[›YÜË˜[YNˆŒÌŸ]\›ÛÛ›Û\ˆÂˆÜÚ][Û‹ˆÚ[›™[ˆ›YÜËˆ˜[YWØš]Îˆ˜[YK×Øš]Ê
Kˆ‹”]\›ÛÛ›Û\ŽŽ™Y˜][

BˆNÂˆ]ÛÛ›Û\œÈHÂˆÚ[
‹ÌKŒJKˆÚ[
M‹‹ÌKÍJKˆÚ[
KÌKŽJKˆÚ[
‹ÌJKˆÚ[
LŒ‹ÌKKJKˆÚ[
M‹ÌKŒÌŽŽ“SŠKˆNÂ‚ˆ]]]ÛX][ÛˆH]\›—ÝœÝ×Ü\˜[Y]\—Ø]]ÛX][Û—Ù›Ü—ØÚ[›™[
ˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	›[šÊKˆÛÛYJ	œ›Ú™XÝÜ]
Kˆ‹ˆ	˜ÛÛ›Û\œËˆMËˆ˜ÛÛK™^[\KœÞ[‹ˆM‹ˆ
NÂˆ\ÜÙ\Ù\HJˆ]]ÛX][Û‹ˆ™XÈVÙ›Ü™XZ[ŽœÝÎŽ•œÝÔ\˜[Y]\]]ÛX][ÛˆÂˆ\˜[Y]\—ÚYˆ‹ˆÚ[Îˆ™XÈVÂˆ›Ü™XZ[ŽœÝÎŽ•œÝÐ]]ÛX][Û”Ú[ÂˆÜÚ][Û—Ø™X]ÎˆŒˆ˜[YNˆŒKˆKˆ›Ü™XZ[ŽœÝÎŽ•œÝÐ]]ÛX][Û”Ú[ÂˆÜÚ][Û—Ø™X]ÎˆKŒˆ˜[YNˆÍKˆKˆKˆWBˆ
NÂˆ\ÜÙ\Jˆ]\›—ÝœÝ×Ü\˜[Y]\—Ø]]ÛX][Û—Ù›Ü—ØÚ[›™[
ˆ	–Û[š×KˆÛÛYJ	œ›Ú™XÝÜ]
Kˆ‹ˆ	˜ÛÛ›Û\œËˆMËˆ™Y™™\™[\YÚ[ˆ‹ˆM‹ˆ
Bˆš\×Ù[\J
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™×Ü™\XÙ\×ØžWÝ\™Ù]Ø[™ØØ[—Ø™WÜ™[[Ý™Y

HÂˆ]\™Ù]HZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆLKˆ\˜[Y]\—Û˜[YNˆÝ]Ù™ˆ‹×ÛÝÛ™Y

KˆNÂˆ]š\œÝHZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ\™Ù]ˆ\™Ù]˜ÛÛ™J
KˆNÂˆ]ÙXÛÛ™HZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ›Ú™XÝØÚ[›™[ÚYˆˆYÚ[—ÝZYˆ˜ÛÛK™^[\KœÞ[‹×ÛÝÛ™Y

Kˆ\˜[Y]\—ÚYˆL‹ˆ\˜[Y]\—Û˜[YNˆ”™\ÛÛ˜[˜ÙH‹×ÛÝÛ™Y

KˆKˆNÂˆ]™\XÙ[Y[HZYUœÝÔ\˜[Y]\“X\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆ‹ˆÛÛ›Û\ŽˆŒKˆ\™Ù]ˆZYUœÝÔ\˜[Y]\•\™Ù]Âˆ\˜[Y]\—Û˜[YNˆ‘š[\ˆÝ]Ù™ˆ‹×ÛÝÛ™Y

Kˆ‹\™Ù]˜ÛÛ™J
BˆKˆNÂˆ]]]X\[™ÜÈH™XÈVÙš\œÝÙXÛÛ™˜ÛÛ™J
WNÂˆ\Ù\ÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ê	›]]X\[™ÜË™\XÙ[Y[˜ÛÛ™J
JNÂˆ\ÜÙ\Ù\HJX\[™ÜË™XÈVÜÙXÛÛ™˜ÛÛ™J
K™\XÙ[Y[˜ÛÛ™J
WJNÂˆ\ÜÙ\Ù\HJˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™×Ù›Ü—Ý\™Ù]
	›X\[™ÜË	\™Ù]
KˆÛÛYJ	œ™\XÙ[Y[
Bˆ
NÂ‚ˆ™[[Ý™WÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™Ê	›]]X\[™ÜË	\™Ù]
NÂˆ\ÜÙ\Ù\HJX\[™ÜË™XÈVÜÙXÛÛ™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÝœÝ×Ü\˜[Y]\—ÛX\[™×Ü\œÙ\—Ü™Z™XÝ×Ú[˜[YØÛÛ›ÛÊ
HÂˆ]˜[YÙšY[ÈH›Ü›X]JˆžßW×ßWLWßH‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
œÝ\™˜XÙHŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
˜ÛÛK™^[\KœÞ[ŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ý]Ù™ˆŠBˆ
NÂˆ][˜[YØÚ[›™[H›Ü›X]Jˆ›X\[™Ï^ßWM—×ßWLWßH‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
œÝ\™˜XÙHŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
˜ÛÛK™^[\KœÞ[ŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ý]Ù™ˆŠBˆ
NÂˆ][˜[YØÛÛ›Û\ˆH›Ü›X]Jˆ›X\[™Ï^ßWLŽßWLWßH‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
œÝ\™˜XÙHŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
˜ÛÛK™^[\KœÞ[ŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ý]Ù™ˆŠBˆ
NÂˆ][˜[YÜYÚ[—ÝZYH›Ü›X]Jˆ›X\[™Ï^ßW×LWßH‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
œÝ\™˜XÙHŠKˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
Ý]Ù™ˆŠBˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ	™›Ü›X]Jˆ™\œÚ[ÛLWžßWžßWžßWˆ‹ˆ[˜[YØÚ[›™[[˜[YØÛÛ›Û\‹[˜[YÜYÚ[—ÝZYˆ
JKˆ™XÎŽ›™]Ê
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÝœÝ×Ü\˜[Y]\—ÛX\[™ÜÊ	™›Ü›X]J›X\[™Ï^Ý˜[YÙšY[ßW^˜WˆŠJKˆ™XÎŽ›™]Ê
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—ØÛÛ›ÛÛX\[™Ü×Ü›Ý[™Ýš\Û][\WÙ]šXÙ\×Ø[™Ý˜XÚÜÊ
HÂˆ]X\[™ÜÈH™XÈVÂˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™:fêˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆÍˆ[œÙ\ÛÜ™[˜[ˆŒËˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”[‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”Ý\™[ÔÙ\\˜][Û‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLKˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž“]]KˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆL‹ˆ[œÙ\ÛÜ™[˜[ˆ‹ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛËˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLËˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”\ÙR[™\ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ\ØŽœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMKˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›YˆKˆNÂˆ][˜ÛÙYHÙ\šX[^™WÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ	›X\[™ÜÊNÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ	™[˜ÛÙY
KX\[™ÜÊNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—ØÛÛ›ÛÛX\[™×ÛØY\—ÚÙY\×ÛYØXÞWÜÚ[™ÛWÛX\[™×Ùš[\Ê
HÂˆ]YØXÞHHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™:fêˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆÍˆ[œÙ\ÛÜ™[˜[ˆŒËˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ][˜ÛÙYH›Ü›X]Jˆš[œ]^ßW˜Ú[›™[^ßW˜ÛÛ›Û\^ßWš[œÙ\^ßWˆ‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
	›YØXÞKš[œ]Ù]šXÙWÚY
KˆYØXÞK˜Ú[›™[ˆYØXÞK˜ÛÛ›Û\‹ˆYØXÞKš[œÙ\ÛÜ™[˜[ˆ
NÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ	™[˜ÛÙY
K™XÈVÛYØXÞWJNÂˆ]™\œÚ[Û—ÝÛÈH›Ü›X]Jˆ™\œÚ[ÛL—›X\[™Ï^ßWßWßWßWˆ‹ˆ[˜ÛÙWÛZYWÙ]šXÙWÚY
˜[ØNšÙ^X›Ø\™ŠKˆ‹ˆL‹ˆˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ	™\œÚ[Û—ÝÛÊKˆ™XÈVÓZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ˜[ØNšÙ^X›Ø\™‹×ÛÝÛ™Y

KˆÚ[›™[ˆ‹ˆÛÛ›Û\ŽˆL‹ˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆWBˆ
NÂˆ\ÜÙ\Jˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊˆš[œ]MÌ™ÌÍ˜Ú[›™[LM—˜ÛÛ›Û\M×š[œÙ\LWˆ‚ˆ
Bˆš\×Ù[\J
Bˆ
NÂˆ\ÜÙ\Jˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊˆš[œ]MÌ™ÌÍ˜Ú[›™[L˜ÛÛ›Û\LLŽš[œÙ\LWˆ‚ˆ
Bˆš\×Ù[\J
Bˆ
NÂˆ\ÜÙ\Jˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊˆš[œ]MÌ™ÌÍ˜Ú[›™[L˜ÛÛ›Û\M×š[œÙ\NNNNNNWˆ‚ˆ
Bˆš\×Ù[\J
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ\œÙWÛZYWÛZ^\—ØÛÛ›ÛÛX\[™ÜÊ™\œÚ[ÛL×›X\[™ÏMÌ™ÌÍŒÛÛ×ˆŠKˆ™XÎŽ›™]Ê
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—ØÛÛ›ÛÛX\[™×Ü™X\ÜÚYÛ›Y[Ú\×ÛÛ™WÝ×ÛÛ™WØ[™Ü™\Ù\™\×ÛÝ\—ÜÝš\Ê
HÂˆ]š\œÝHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ]ÙXÛÛ™HZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\Žˆˆ[œÙ\ÛÜ™[˜[ˆ‹ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ]™\XÙ[Y[HZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆKˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ]™X\ÜÚYÛ™YØÛÛ›ÛHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\Žˆˆ[œÙ\ÛÜ™[˜[ˆËˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆNÂˆ]]]X\[™ÜÈH™XÈVÙš\œÝÙXÛÛ™˜ÛÛ™J
WNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜË™\XÙ[Y[˜ÛÛ™J
JNÂˆ\ÜÙ\Ù\HJX\[™ÜË™XÈVÜÙXÛÛ™™\XÙ[Y[˜ÛÛ™J
WJNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜË™X\ÜÚYÛ™YØÛÛ›Û˜ÛÛ™J
JNÂˆ\ÜÙ\Ù\HJˆX\[™ÜËˆ™XÈVÜ™\XÙ[Y[˜ÛÛ™J
K™X\ÜÚYÛ™YØÛÛ›Û˜ÛÛ™J
WBˆ
NÂˆ][ˆHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”[‹ˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜË[‹˜ÛÛ™J
JNÂˆ]Ý\™[ÈHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLKˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”Ý\™[ÔÙ\\˜][Û‹ˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜËÝ\™[Ë˜ÛÛ™J
JNÂˆ]]]HHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆL‹ˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž“]]KˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜË]]K˜ÛÛ™J
JNÂˆ]ÛÛÈHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLËˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛËˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜËÛÛË˜ÛÛ™J
JNÂˆ]\ÙWÚ[™\HZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”\ÙR[™\ˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜË\ÙWÚ[™\˜ÛÛ™J
JNÂˆ]ÝØ\ÛYÜšYÚHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMKˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜËÝØ\ÛYÜšYÚ˜ÛÛ™J
JNÂˆ]Y™™XÝ×Ù[˜X›YHZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆM‹ˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›YˆNÂˆ\Ù\ÛZYWÛZ^\—ØÛÛ›ÛÛX\[™Ê	›]]X\[™ÜËY™™XÝ×Ù[˜X›Y˜ÛÛ™J
JNÂˆ\ÜÙ\Ù\HJˆX\[™ÜËˆ™XÈVÂˆ™\XÙ[Y[ˆ™X\ÜÚYÛ™YØÛÛ›Ûˆ[‹ˆÝ\™[Ëˆ]]KˆÛÛËˆ\ÙWÚ[™\ˆÝØ\ÛYÜšYÚˆY™™XÝ×Ù[˜X›YˆBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—Ù˜Y\—ØÛÛ›ÛÛX]Ú\×Ù]šXÙWØÚ[›™[Ø[™ØÛÛ›Û\Š
HÂˆ]X\[™ÜÈH™XÈVÂˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXˆ‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆ[œÙ\ÛÜ™[˜[ˆ‹ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆKˆÛÛ›Û\ŽˆËˆ[œÙ\ÛÜ™[˜[ˆËˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLˆ[œÙ\ÛÜ™[˜[ˆKˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”Ý\™[ÔÙ\\˜][Û‹ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLKˆ[œÙ\ÛÜ™[˜[ˆ‹ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž“]]KˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆL‹ˆ[œÙ\ÛÜ™[˜[ˆ‹ˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛËˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆLËˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”\ÙR[™\ˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚˆKˆZYSZ^\ÛÛ›ÛX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆœÝ\™˜XÙKXH‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆMKˆ[œÙ\ÛÜ™[˜[ˆˆ\™Ù]ˆZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›YˆKˆNÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹ÊKˆÛÛYJ
KZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\ŠJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXˆ‹ÊKˆÛÛYJ
‹ZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\ŠJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹KÊKˆÛÛYJ
ËZYSZ^\ÛÛ›Û\™Ù]Ž‘˜Y\ŠJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹
Kˆ›Û™Bˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹L
KˆÛÛYJ
KZYSZ^\ÛÛ›Û\™Ù]Ž”Ý\™[ÔÙ\\˜][ÛŠJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹LJKˆÛÛYJ
‹ZYSZ^\ÛÛ›Û\™Ù]Ž“]]JJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹LŠKˆÛÛYJ
‹ZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛÊJBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹LÊKˆÛÛYJ
ZYSZ^\ÛÛ›Û\™Ù]Ž”\ÙR[™\
JBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹M
KˆÛÛYJ
ZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚ
JBˆ
NÂˆ\ÜÙ\Ù\HJˆZYWÛZ^\—ØÛÛ›ÛÙ›Ü—ØØÊ	›X\[™ÜËœÝ\™˜XÙKXH‹MJKˆÛÛYJ
ZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›Y
JBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—ÝÙÙÛWÝ\™Ù]×Ý\ÙWØØ×Ü™\Ü×Ý™\ÚÛ

HÂˆ›Üˆ\™Ù][ˆÂˆZYSZ^\ÛÛ›Û\™Ù]Ž“]]KˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÛÛËˆZYSZ^\ÛÛ›Û\™Ù]Ž”\ÙR[™\ˆZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚˆZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›YˆHÂˆ\ÜÙ\J\™Ù]š\×ÝÙÙÛJ
JNÂˆ\ÜÙ\Ù\HJ\™Ù]œ˜]×Ý˜[YJŒÊK
NÂˆ\ÜÙ\Ù\HJ\™Ù]œ˜]×Ý˜[YJ
KJNÂˆ\ÜÙ\J\™Ù]œÝ\Ü×Ü˜]×Ý˜[YJ
JNÂˆ\ÜÙ\J\™Ù]œÝ\Ü×Ü˜]×Ý˜[YJJJNÂˆ\ÜÙ\J]\™Ù]œÝ\Ü×Ü˜]×Ý˜[YJŠJNÂˆBˆ\ÜÙ\Ù\HJˆZYSZ^\ÛÛ›Û\™Ù]Ž™œ›ÛWÜÙ][™ÊœÝØ\[Y\šYÚŠKˆÛÛYJZYSZ^\ÛÛ›Û\™Ù]Ž”ÝØ\YšYÚ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆZYSZ^\ÛÛ›Û\™Ù]Ž™œ›ÛWÜÙ][™Ê™Y™™XÝËY[˜X›YŠKˆÛÛYJZYSZ^\ÛÛ›Û\™Ù]Ž‘Y™™XÝÑ[˜X›Y
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØØ×Ü[—ÛX\[™×Ú\×ØÙ[\™YØ[™ØÛ[\Y

HÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]Ê
KM—Í
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]ÊŒÊKLL
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]Ê
K
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]ÊJKLJNÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]ÊLÊK—Í
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ü[—Ü˜]ÊMJK—Í
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØØ×ÜÝ\™[×ÜÙ\\˜][Û—ÛX\[™×Ú\×ØÙ[\™YØ[™ØÛ[\Y

HÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]Ê
KM
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]ÊŒÊKLJNÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]Ê
K
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]ÊJKJNÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]ÊLÊK
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—ÜÝ\™[×ÜÙ\\˜][Û—Ü˜]ÊMJK
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØØ×Ý˜[Y\×ÛX\Ý×ÝWÜÝ\ÜYÛZ^\—Ù˜Y\—Ü˜[™ÙJ
HÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ù˜Y\—Ü˜]Ê
K
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ù˜Y\—Ü˜]Ê
KÌŒŠNÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ù˜Y\—Ü˜]ÊLÊKM—Ì
NÂˆ\ÜÙ\Ù\HJZYWØØ×Ý×ÛZ^\—Ù˜Y\—Ü˜]ÊMJKM—Ì
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØÛÛ›Û\—Ý[\]\×Ü›Ý[™Ýš\Û˜[YYØØ×Øš[™[™ÜÊ
HÂˆ][\]\ÈH™XÈVÓZYPÛÛ›Û\•[\]HÂˆ˜[YNˆ“][˜ÚÙ^W:fêˆ‹×ÛÝÛ™Y

KˆÝ]]ÙØZ[ŽˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆJKˆ^WÜ]\ÙNˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆMKˆÛÛ›Û\ŽˆˆJKˆZ[Ú[Žˆ˜[ÙKˆWNÂˆ][˜ÛÙYHÙ\šX[^™WÛZYWØÛÛ›Û\—Ý[\]\Ê	[\]\ÊNÂˆ\ÜÙ\Ù\HJ\œÙWÛZYWØÛÛ›Û\—Ý[\]\Ê	™[˜ÛÙY
K[\]\ÊNÂ‚ˆ]X\[™ÈHZYPÛÛ›ÛÚ[™ÙSX\[™ÈÂˆ[œ]Ù]šXÙWÚYˆ›ZYN›][˜ÚÙ^H‹×ÛÝÛ™Y

KˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆNÂˆ\ÜÙ\Ù\HJˆZYWØÛÛ›Û\—Øš[™[™×Ù›Ü—Ù]šXÙJÛÛYJ	›X\[™ÊK›ZYN›][˜ÚÙ^HŠKˆ[\]\ÖÌK›Ý]]ÙØZ[‚ˆ
NÂˆ\ÜÙ\Ù\HJˆZYWØÛÛ›Û\—Øš[™[™×Ù›Ü—Ù]šXÙJÛÛYJ	›X\[™ÊK˜[›Ý\ˆ[œ]ŠKˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZ[Ú[—ÛZYWØÛÛ›Û\—Ý[\]\×Ý\ÙWÙØÝ[Y[YØØ×ÛX\[™ÜÊ
HÂˆ][\]\ÈHZ[Ú[—ÛZYWØÛÛ›Û\—Ý[\]\Ê
NÂˆ\ÜÙ\Ù\HJ[\]\Ë›[Š
KM
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÌK›˜[YK‘Ù[™\˜[RQHÚˆH
ÐÈÈ
ÈÐÈ
HŠNÂˆ\ÜÙ\J[\]\ÖÌK˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÌK›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆËˆJBˆ
NÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÌKœ^WÜ]\ÙKˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÌWK›˜[YK“RQH^™\ÜÚ[ÛˆÚˆH
ÐÈLJHŠNÂˆ\ÜÙ\J[\]\ÖÌWK˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÌWK›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆLKˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÌWKœ^WÜ]\ÙK›Û™JNÂˆ\ÜÙ\Ù\HJ[\]\ÖÌ—K›˜[YK–UÕPÒRS’H™\Ù]H
ÚˆLJHŠNÂˆ\ÜÙ\J[\]\ÖÌ—K˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÌ—K›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆLˆÛÛ›Û\ŽˆKˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÌ—Kœ^WÜ]\ÙK›Û™JNÂˆ\ÜÙ\Ù\HJ[\]\ÖÌ×K›˜[YK–UÕPÒRS’H™\Ù]ˆ
ÚˆLJHŠNÂˆ\ÜÙ\J[\]\ÖÌ×K˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÌ×K›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆLˆÛÛ›Û\ŽˆLˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÌ×Kœ^WÜ]\ÙK›Û™JNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÍK›˜[YKˆ–UÕPÒÓÓTPÕ™\Ù]^Y\ˆH˜Y\ˆH
ÚˆJH‚ˆ
NÂˆ\ÜÙ\J[\]\ÖÍK˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÍK›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆKˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÍKœ^WÜ]\ÙK›Û™JNÂˆ\ÜÙ\Ù\HJ[\]\ÖÍWK›˜[YKZØZHRQ[Z^X\Ý\ˆ˜Y\ˆ
ÚˆJHŠNÂˆ\ÜÙ\J[\]\ÖÍWK˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÍWK›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆŒ‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÍWKœ^WÜ]\ÙK›Û™JNÂˆ\ÜÙ\Ù\HJ[\]\ÖÍ—K›˜[YKZØZHRQ[Z^˜Y\ˆH
ÚˆJHŠNÂˆ\ÜÙ\J[\]\ÖÍ—K˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]\ÖÍ—K›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\ŽˆNKˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]\ÖÍ—Kœ^WÜ]\ÙK›Û™JNÂˆ›Üˆ
[\]K
˜Y\‹ÛÛ›Û\ŠJH[ˆ[\]\ÖÍË‹—Kš]\Š
Kžš\
Âˆ
‹ŒÊKˆ
ËÊKˆ
ÌJKˆ
KJKˆ
‹LÊKˆ
ËMÊKˆ
ŒJKˆJHÂˆ\ÜÙ\Ù\HJ[\]K›˜[YK›Ü›X]JZØZHRQ[Z^˜Y\ˆÙ˜Y\ŸH
ÚˆJHŠJNÂˆ\ÜÙ\J[\]K˜Z[Ú[ŠNÂˆ\ÜÙ\Ù\HJˆ[\]K›Ý]]ÙØZ[‹ˆÛÛYJZYPÛÛ›Û\ØÐš[™[™ÈÂˆÚ[›™[ˆˆÛÛ›Û\‹ˆJBˆ
NÂˆ\ÜÙ\Ù\HJ[\]Kœ^WÜ]\ÙK›Û™JNÂˆBˆ\ÜÙ\JÙ\šX[^™WÛZYWØÛÛ›Û\—Ý[\]\Ê	[\]\ÊKš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØÛÛ›ÛØÚ[™ÙWÛX\[™×Ü™Z™XÝ×ÛÝ]ÛÙ—Ü˜[™ÙWÛÜ—Ú[˜ÛÛ\]WÙ[šY\Ê
HÂˆ\ÜÙ\J\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Êš[œ]W˜Ú[›™[L˜ÛÛ›Û\M×ˆŠKš\×Û›Û™J
JNÂˆ\ÜÙ\Jˆ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Êš[œ]MÌ™ÌÍ˜Ú[›™[LM—˜ÛÛ›Û\M×ˆŠBˆš\×Û›Û™J
Bˆ
NÂˆ\ÜÙ\Jˆ\œÙWÛZYWØÛÛ›ÛØÚ[™ÙWÛX\[™Êš[œ]MÌ™ÌÍ˜Ú[›™[L˜ÛÛ›Û\LLŽˆŠBˆš\×Û›Û™J
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÜ^WÜ]\ÙWØØ×ÝÙÙÛ\×ÛÛ˜ÙWÝ[[ÝWØ]Û—Ú\×Ü™[X\ÙY

HÂˆ]]]XÝ]™HH˜[ÙNÂˆ\ÜÙ\JZYWØ]Û—Ü™\Ü×ÙYÙJLË	›]]XÝ]™JJNÂˆ\ÜÙ\J[ZYWØ]Û—Ü™\Ü×ÙYÙJM‹	›]]XÝ]™JJNÂˆ\ÜÙ\J[ZYWØ]Û—Ü™\Ü×ÙYÙJ	›]]XÝ]™JJNÂˆ\ÜÙ\J[ZYWØ]Û—Ü™\Ü×ÙYÙJ	›]]XÝ]™JJNÂˆ\ÜÙ\JZYWØ]Û—Ü™\Ü×ÙYÙJ	›]]XÝ]™JJNÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÛZ^\—ÝÙÙÛWØØ×ÝÙÙÛ\×ÛÛ˜ÙWÜ\—ÛX\[™×Ü™\ÜÊ
HÂˆ]]]XÝ]™WÛX\[™ÜÈH•™YTÙ]Ž›™]Ê
NÂˆ\ÜÙ\JZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXH‹ˆˆL‹ˆLËˆ
JNÂˆ\ÜÙ\J[ZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXH‹ˆˆL‹ˆM‹ˆ
JNÂˆ\ÜÙ\JZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXH‹ˆˆLËˆLËˆ
JNÂˆ\ÜÙ\J[ZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXˆ‹ˆˆL‹ˆˆ
JNÂˆ\ÜÙ\J[ZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXH‹ˆˆL‹ˆˆ
JNÂˆ\ÜÙ\JZYWÛZ^\—Ø]Û—Ü™\Ü×ÙYÙJˆ	›]]XÝ]™WÛX\[™ÜËˆœÝ\™˜XÙKXH‹ˆˆL‹ˆˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆØ[\WÙY]Ü—Ù˜Y×ÜÙ[XÝ[Û—Û›Ü›X[^™\×Ù\™XÝ[Û—Ø[™ÚÙY\×ÛÛ™WÙœ˜[YJ
HÂˆ\ÜÙ\Ù\HJØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊ‹K
KÛÛYJ‹‹ŠJNÂˆ\ÜÙ\Ù\HJØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊK‹
KÛÛYJ‹‹ŠJNÂˆ\ÜÙ\Ù\HJØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊËË
KÛÛYJË‹Ž
JNÂˆ\ÜÙ\Ù\HJØ[\WÙY]Ü—ÜÙ[XÝ[Û—Ù›Ü—Ù˜YÊ
K›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]×Ù›ÛÝ×Ù[˜X›YÙš[\œ×Ø[™Ø]˜Z[X›WÚ[œ]Ê
HÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]ÊYKYKYKYKYJKˆ
YKYKYJBˆ
NÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]Ê˜[ÙKYKYKYKYJKˆ
˜[ÙKYKYJBˆ
NÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]ÊYKYKYK˜[ÙKYJKˆ
˜[ÙKYKYJBˆ
NÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]ÊYKYKYKYK˜[ÙJKˆ
YK˜[ÙK˜[ÙJBˆ
NÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™×Ú[œ]Ý\™Ù]Ê˜[ÙK˜[ÙK˜[ÙKYKYJKˆ
˜[ÙK˜[ÙK˜[ÙJBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØ]]ÛX][Û—Ü™XÛÜ™[™×Û›Ü›X[^™\×Ø[™ØÛØ[\ØÙ\×ØØ×Ý˜[Y\Ê
HÂˆ]]]™XÛÜ™[™ÈHZYP]]ÛX][Û”™XÛÜ™[™ÎŽ›™]ÊÍ
ËLJJNÂˆ™XÛÜ™[™Ëœ™XÛÜ™

NÂˆ™XÛÜ™[™Ëœ™XÛÜ™
M‹
NÂˆ™XÛÜ™[™Ëœ™XÛÜ™
M‹LÊNÂˆ™XÛÜ™[™Ëœ™XÛÜ™
NL‹LÊNÂˆ™XÛÜ™[™Ëœ™XÛÜ™
ŽÌŠNÂ‚ˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™Ë››Ü›X[^™YÜÚ[Ê
Kˆ™XÈVÊŒ
K
M‹KŒ
K
ŽÌ‹ŒÈLËŒ
WBˆ
NÂˆ\ÜÙ\J\™XÛÜ™[™Ë›Ý™\™›ÝÙY
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWØ]]ÛX][Û—Ü™XÛÜ™[™×ÜÛÜ×ÛÝ]ÛÙ—ÛÜ™\—Ý[Y\Ý[\Ê
HÂˆ]]]™XÛÜ™[™ÈHZYP]]ÛX][Û”™XÛÜ™[™ÎŽ›™]ÊK

JNÂˆ™XÛÜ™[™Ëœ™XÛÜ™
ŽLÊNÂˆ™XÛÜ™[™Ëœ™XÛÜ™

NÂˆ™XÛÜ™[™Ëœ™XÛÜ™
M‹
NÂ‚ˆ\ÜÙ\Ù\HJˆ™XÛÜ™[™Ë››Ü›X[^™YÜÚ[Ê
Kˆ™XÈVÊŒ
K
M‹ŒÈLËŒ
K
ŽKŒ
WBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ™XÛÜ™[™×ÛZYWØØ×ØÜ™X]\×Ý[—Ø\[™×Ý×Ü˜]×Ü]\›—ÜÝ™X[J
HÂˆ]]]ØÝ[Y[HX[›×Ü›ÛØÛÛÜ—Ùš^\™J
NÂˆ]š\œÝÜÚ[ÈHÊŒJK
M‹ÍJWNÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™Ü]\›—ØÛÛ›Û\—ÜÚ[Ê	›]]ØÝ[Y[Ë
ËLJK	™š\œÝÜÚ[ÊKˆÚÊŠBˆ
NÂ‚ˆ]™^ÜÚ[ÈHÊNL‹JWNÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™Ü]\›—ØÛÛ›Û\—ÜÚ[Ê	›]]ØÝ[Y[Ë
ËLJK	›™^ÜÚ[ÊKˆÚÊJBˆ
NÂ‚ˆ]ÛÛ›Û\œÈHØÝ[Y[ˆœ]\›œÊ
Bˆ[Ü˜\

Bˆš[×Ú]\Š
Bˆ™š[™
]\›Ÿ]\›‹šYOHÊBˆ[Ü˜\

Bˆ˜ÛÛ›Û\œÎÂˆ]™XÛÜ™YHÛÛ›Û\œÂˆš]\Š
Bˆ™š[\ŠÚ[
Ú[˜Ú[›™[Ú[™›YÜÊHOH
ËLJJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂˆ\ÜÙ\Ù\HJ™XÛÜ™Y›[Š
KÊNÂˆ\ÜÙ\Ù\HJˆ™XÛÜ™Yˆš]\Š
Bˆ›X\
Ú[
Ú[œÜÚ][Û‹Ú[˜[YJ
JJBˆ˜ÛÛXÝŽ™XÏÏŠ
Kˆ™XÈVÊŒJK
M‹ÍJK
NL‹JWBˆ
NÂˆ]›Ý[™š\H›ØÝ[Y[Žœ\œÙJ	™ØÝ[Y[™[˜ÛÙWÛÜÜÛ\ÜÊ
K[Ü˜\

JK[Ü˜\

NÂˆ\ÜÙ\Ù\HJ›Ý[™š\œ]\›œÊ
K[Ü˜\

VÌK˜ÛÛ›Û\œË›[Š
KÊNÂˆB‚ˆÖÝ\ÝBˆ›ˆœÝ×Ø]]ÛØYÝ\Ù\×Û[Ù[Z[™›×ØÛ\Ü×ÚYØ™Y›Ü™WÛ˜[Y\Ê
HÂˆ]Ø[™Y]HHYÚ[Ø[™Y]HÂˆ›Ü›X]ˆYÚ[‘›Ü›X]Ž•œÝËˆ˜[YNˆ‘^[\H‹×ÛÝÛ™Y

Kˆ]ˆ]YŽŽ™œ›ÛJ‹ÜYÚ[œËÑ^[\KœÝÈŠKˆÛ\Ü×ÚYÎˆ™XÈVÈŒLLŒŒÌÍMMÍÎNPPPÐÑQQ‘ˆ‹×ÛÝÛ™Y

WKˆNÂ‚ˆ\ÜÙ\JØ[™Y]WÛX]Ú\×ÝœÝÚY[]Jˆ	˜Ø[™Y]KˆÛÛYJŒLLŒŒÌÍMMÍÎNXXX˜˜ØÙYY™ˆŠKˆÛÛYJ‹ÛÛÓÝ\‹œÝÈŠKˆÛÛYJ‘Y™™\™[\Ü^H˜[YHŠKˆ
JNÂˆ\ÜÙ\JXØ[™Y]WÛX]Ú\×ÝœÝÚY[]Jˆ	˜Ø[™Y]KˆÛÛYJ‘‘‘QQÐÐPNNNÍÍMMÌÌŒŒLLŠKˆÛÛYJ‹ÛÛÑ^[\KœÝÈŠKˆÛÛYJ‘^[\HŠKˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—Ý\Ù\×ÜÚ^Y[ÜÛ˜\ÛÜ—Ø[Ùœ™YWÜXÙ[Y[

HÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—ÝXÚÜÊŒKŒLŒŒKM‹YJKˆLŒˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—ÝXÚÜÊŒKŒLŒŒKM‹˜[ÙJKˆLLˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×Ù›ÜÜÜÚ][Û—ÝXÚÜÊŒLŒŒKM‹˜[ÙJKˆˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ]Y[×Ü™XÛÜ™[™×Ý\Ù\×Ùš\œÝÛ›Û—ÛÝ™\›\[™×Ü^[\ÝÝ˜XÚÊ
HÂˆ]\œ˜[™Ù[Y[H^[\ÝÝ˜XÚ×Ø\œ˜[™Ù[Y[Ùš^\™J	–ÊMŒNJK
MŒN
WJNÂˆ\ÜÙ\Ù\HJˆš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚÊ	˜\œ˜[™Ù[Y[
KˆÛÛYJŠBˆ
NÂ‚ˆ]]\—ØÛ\H^[\ÝÝ˜XÚ×Ø\œ˜[™Ù[Y[Ùš^\™J	–ÊMŒMŒNJWJNÂˆ\ÜÙ\Ù\HJš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚÊ	›]\—ØÛ\
KÛÛYJ
JNÂ‚ˆ][šÛ›ÝÛ—Ý˜XÚÈH^[\ÝÝ˜XÚ×Ø\œ˜[™Ù[Y[Ùš^\™J	–ÊMŒŒ
WJNÂˆ\ÜÙ\Ù\HJš\œÝØ]˜Z[X›WÜ^[\ÝÝ˜XÚÊ	[šÛ›ÝÛ—Ý˜XÚË
K›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ]Y[×Ü™XÛÜ™[™×ÛÙ™œÙ]Û[Ý™\×Ü^[\ÝÜÝ\Ø[™ØÛ[\×Ø]Þ™\›Ê
HÂˆ][\×ÛX\Bˆ^[\Ý[\ÓX\Ž˜ÛÛœÝ[
LŒŒ
K™^XÝ
˜ÛÛœÝ[[\ÈX\ÚÝ[™H˜[YŠNÂˆ\ÜÙ\Ù\HJˆ]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]
WÌK	[\×ÛX\Ì
KˆÚÊN
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]
WÌLK	[\×ÛX\Ì
KˆÚÊWÌŒ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]
KŒ	[\×ÛX\Ì
KˆÚÊ
Bˆ
NÂˆ\ÜÙ\J]Y[×Ü™XÛÜ™[™×ÜÝ\ÝXÚ×ÝÚ]ÛÙ™œÙ]
WÌ	[\×ÛX\
Kš\×Ù\œŠ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÜ™XÛÜ™[™×ÛÙ™œÙ]Ú\×ÛØØ[Ý×Ø[—ØXÝ]™WÜ]\›—ØÛ\

HÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
MLŒ
KˆÛÛYJ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
ŽNK‹LŒ
KˆÛÛYJNNJBˆ
NÂˆ\ÜÙ\Ù\HJ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
ÌŒLŒ
K›Û™JNÂˆ\ÜÙ\Ù\HJ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
NKŒLŒ
K›Û™JNÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÛØØ[Ü™XÛÜ™[™×ÛÙ™œÙ]
Ž“S‹LŒ
Kˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝ×Ù›ÛÝ×ÛÝ™\›\[™×ØÛ\Ü™\X]×Ø[™Ø›Ý[™\šY\Ê
HÂˆ]\œ˜[™Ù[Y[H^[\ÝÜ]\›—ÙÚÜÝØ\œ˜[™Ù[Y[Ùš^\™J	–Âˆ
LM‹K
Kˆ
MM‹‹JKˆ
NM‹M‹ËŠKˆJNÂˆ]]\›œÈH™XÈVÂˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›ŠKM‹™XÎŽ›™]Ê
JKˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›Šˆ‹ˆˆ™XÈVÂˆX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJKËL‹
KˆX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJ‹MKÌ
KˆKˆ
KˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›ŠË™XÈVÜX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJËŒ
WJKˆNÂ‚ˆ]ÚÜÝÈBˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊK	œ]\›œË	˜\œ˜[™Ù[Y[	•™YTÙ]Ž›™]Ê
JNÂ‚ˆ\ÜÙ\Ù\HJˆÚÜÝÂˆš]\Š
Bˆ›X\
ÚÜÝÂˆ
ˆÚÜÝ˜Ú[›™[ÚYˆÚÜÝšÙ^KˆÚÜÝœÜÚ][Û—ÝXÚÜËˆÚÜÝ›[™ÝÝXÚÜËˆ
BˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
Kˆ™XÈVÊKËŒ
K
‹MKÎN
WBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝ×ØØ[—ÚYWØWÝ˜XÚ×ÛÜ—ÛÛ™WØÛ\

HÂˆ]\œ˜[™Ù[Y[Bˆ^[\ÝÜ]\›—ÙÚÜÝØ\œ˜[™Ù[Y[Ùš^\™J	–ÊLM‹K
K
MM‹‹JWJNÂˆ]]\›œÈH™XÈVÂˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›ŠKM‹™XÎŽ›™]Ê
JKˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›Š‹™XÈVÜX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJKËL‹
WJKˆNÂˆ]ÚÜÝÈBˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊK	œ]\›œË	˜\œ˜[™Ù[Y[	•™YTÙ]Ž›™]Ê
JNÂˆ]ÚÜÝHÚÜÝÂˆ™š\œÝ

Bˆ™^XÝ
HÝ™\›\[™ÈÛ\ÚÝ[XZÙHHÚÜÝ›ÝHŠNÂˆ]˜XÚ×ÚYHÚÜÝˆœÛÝ\˜ÙWØÛ\ˆ˜XÚ×ÚYˆ™^XÝ
Hš^\™HÚÝ[\ÜÚYÛˆHÛÝ\˜ÙHÛ\ÈH˜XÚÈŠNÂ‚ˆ\ÜÙ\JX[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›JˆÚÜÝˆ	•™YTÙ]Ž›™]Ê
Kˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂˆ\ÜÙ\J\X[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›JˆÚÜÝˆ	•™YTÙ]Ž™œ›ÛJÊÚÜÝœÛÝ\˜ÙWØÛ\˜\œ˜[™Ù[Y[ÚY˜XÚ×ÚY
WJKˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂˆ\ÜÙ\J\X[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›JˆÚÜÝˆ	•™YTÙ]Ž›™]Ê
Kˆ	•™YTÙ]Ž™œ›ÛJÙÚÜÝœÛÝ\˜ÙWØÛ\JKˆ
JNÂˆ\ÜÙ\JX[›×Ü›ÛÙÚÜÝÛ›ÝWÚ\×Ýš\ÚX›JˆÚÜÝˆ	•™YTÙ]Ž™œ›ÛJÊÚÜÝœÛÝ\˜ÙWØÛ\˜\œ˜[™Ù[Y[ÚY
ÈK˜XÚ×ÚY
WJKˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝ×ÚYÛ›Ü™WÙ\ØX›YÛ]]YØ[™ÜØØ[YØÛ\Ê
HÂˆ]]\›œÈH™XÈVÂˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›ŠKM‹™XÎŽ›™]Ê
JKˆX[›×Ü›ÛÙÚÜÝÝ\ÝÜ]\›Š‹™XÈVÜX[›×Ü›ÛÙÚÜÝÝ\ÝÛ›ÝJKËL‹
WJKˆNÂˆ]]]\œ˜[™Ù[Y[Bˆ^[\ÝÜ]\›—ÙÚÜÝØ\œ˜[™Ù[Y[Ùš^\™J	–ÊLM‹K
K
MM‹‹JWJNÂ‚ˆ\ÜÙ\JˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊK	œ]\›œË	˜\œ˜[™Ù[Y[	•™YTÙ]Ž™œ›ÛJÌ—JK
Bˆš\×Ù[\J
Bˆ
NÂ‚ˆ\œ˜[™Ù[Y[˜Û\ÖÌWK›]]YHYNÂˆ\ÜÙ\JˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊK	œ]\›œË	˜\œ˜[™Ù[Y[	•™YTÙ]Ž›™]Ê
K
Bˆš\×Ù[\J
Bˆ
NÂ‚ˆ\œ˜[™Ù[Y[˜Û\ÖÌWK›]]YH˜[ÙNÂˆ\œ˜[™Ù[Y[˜Û\ÖÌWKœØØ[HHÛÛYJJNÂˆ\ÜÙ\JˆX[›×Ü›ÛØÜ›ÜÜ×Ü]\›—ÙÚÜÝÛ›Ý\ÊK	œ]\›œË	˜\œ˜[™Ù[Y[	•™YTÙ]Ž›™]Ê
K
Bˆš\×Ù[\J
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆZYWÜÛÛ™×Ü™XÛÜ™[™×ÜXÙ\×ØWØÛ\ÛÛ›WÝÚ[—Û›×ÛX]Ú[™×ØÛ\Ú\×ØXÝ]™J
HÂˆ]\œ˜[™Ù[Y[H^[\ÝÝ˜XÚ×Ø\œ˜[™Ù[Y[Ùš^\™J	–ÊLŒNJWJNÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊMLŒË	˜\œ˜[™Ù[Y[˜Û\ÊKˆ›Û™Bˆ
NÂˆ]]]ØØ[YØÛ\ÈH\œ˜[™Ù[Y[˜Û\Ë˜ÛÛ™J
NÂˆØØ[YØÛ\ÖÌKœØØ[HHÛÛYJJNÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊMLŒË	œØØ[YØÛ\ÊKˆ›Û™Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊMLŒ	˜\œ˜[™Ù[Y[˜Û\ÊKˆÛÛYJML
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊMLË	–×JKˆÛÛYJML
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—Ü™XÛÜ™[™×ÜXÙ[Y[ÝXÚÊŽ“S‹Ë	–×JKˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆœÝ×Ø]]ÛØYÚÙY\×Û˜[YWÙ˜[˜XÚ×ÝÚ[—Û[Ù[Z[™›×Ú\×Ý[˜]˜Z[X›J
HÂˆ]Ø[™Y]HHYÚ[Ø[™Y]HÂˆ›Ü›X]ˆYÚ[‘›Ü›X]Ž•œÝËˆ˜[YNˆ‘^[\H‹×ÛÝÛ™Y

Kˆ]ˆ]YŽŽ™œ›ÛJ‹ÜYÚ[œËÑ^[\KœÝÈŠKˆÛ\Ü×ÚYÎˆ™XÎŽ›™]Ê
KˆNÂ‚ˆ\ÜÙ\JØ[™Y]WÛX]Ú\×ÝœÝÚY[]Jˆ	˜Ø[™Y]KˆÛÛYJŒLLŒŒÌÍMMÍÎNPPPÐÑQQ‘ˆŠKˆÛÛYJ‹ÛÛÑ^[\KœÝÈŠKˆÛÛYJ‘Y™™\™[\Ü^H˜[YHŠKˆ
JNÂˆB‚ˆ›ˆ\ÝÙÜšY

HOˆX[›Ô›ÛÜšYÂˆX[›Ô›ÛÜšYÂˆ™XÝˆYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒŒŒŒ
Kˆ
KˆÙ^X›Ø\™ÝÚYˆŽŒˆXÚ×ÜØØ[NˆŒKˆÙ^WÚZYÚˆLËŒˆÙ^WÛÝÎˆÍ‹ˆÙ^WÚYÚˆËˆÛ˜\ÝXÚÜÎˆˆNˆM‹ˆBˆB‚ˆ›ˆ^[\ÝØ]Y[×ØÛ\Ùš^\™J
HOˆ
™XÏ^[\ÝÛ\‹•™YSX\LM‹Ü[ÛNŠHÂˆ]]]YHÌMNÈNÂˆYÌ‹K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLÌ‹×ÛWØž]\Ê
JNÂˆYÍ‹—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒLLM‹×ÛWØž]\Ê
JNÂˆYÍ‹‹ŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	Ž]LM‹×ÛWØž]\Ê
JNÂˆYÎ‹ŒL—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŽMLÌ‹×ÛWØž]\Ê
JNÂˆYÌL‹‹ŒMK˜ÛÜWÙœ›ÛWÜÛXÙJ	N]LM‹×ÛWØž]\Ê
JNÂˆYÌŒ‹ŒK˜ÛÜWÙœ›ÛWÜÛXÙJ	–ÌJNÂˆYÌ‹ŒŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒŒŒÌ‹×ÛWØž]\Ê
JNÂˆYÌŽ‹ŒÌ—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒŒÌ‹×ÛWØž]\Ê
JNÂˆYÍ‹Ì—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒKŒ×ÛWØž]\Ê
JNÂˆ]]]šYÚHYÂˆšYÚÌ‹K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŽMLÌ‹×ÛWØž]\Ê
JNÂˆšYÚÎ‹ŒL—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒMLÌ‹×ÛWØž]\Ê
JNÂˆšYÚÌ‹ŒŽK˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒŒÌ‹×ÛWØž]\Ê
JNÂˆšYÚÌŽ‹ŒÌ—K˜ÛÜWÙœ›ÛWÜÛXÙJ	ŒWÌŒŒŒÌ‹×ÛWØž]\Ê
JNÂ‚ˆ]]]]™[ÜÝ™X[HH™XÈVÌKMKKŒ‹ŒËËNÂˆ]™[ÜÝ™X[Kœ\Ú
NJNÂˆ]]]^[ØYÛ[™ÝHMŒLÌŽÂˆÚ[H^[ØYÛ[™ÝHÂˆ]™[ÜÝ™X[Kœ\Ú

^[ØYÛ[™Ý\ÈN	ˆÑŠH
NÂˆ^[ØYÛ[™ÝHÎÂˆBˆ]™[ÜÝ™X[Kœ\Ú
^[ØYÛ[™Ý\ÈN
NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	›Y
NÂˆ]™[ÜÝ™X[K™^[™Ùœ›ÛWÜÛXÙJ	œšYÚ
NÂ‚ˆ]]]›Ú™XÝH™XÎŽ›™]Ê
NÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	LÌ‹×ÛWØž]\Ê
JNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	ŒLM‹×ÛWØž]\Ê
JNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	ŽMLM‹×ÛWØž]\Ê
JNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJˆ‘“ŠNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	Š]™[ÜÝ™X[K›[Š
H\ÈLÌŠK×ÛWØž]\Ê
JNÂˆ›Ú™XÝ™^[™Ùœ›ÛWÜÛXÙJ	™]™[ÜÝ™X[JNÂ‚ˆ]ØÝ[Y[H›ØÝ[Y[Žœ\œÙJ	œ›Ú™XÝ
K™^XÝ
H^[\Ýš^\™HÚÝ[\œÙHŠNÂˆ]Û\ÈHØÝ[Y[ˆ˜\œ˜[™Ù[Y[Ê
Bˆ™^XÝ
H\œ˜[™Ù[Y[ÚÝ[XÛÙHŠBˆœ™[[Ý™J
Bˆ˜Û\ÎÂˆ]Ú[›™[ÚÚ[™ÈHØÝ[Y[ˆ˜Ú[›™[Ê
Bˆš[×Ú]\Š
Bˆ›X\
Ú[›™[
Ú[›™[šY

KÚ[›™[šÚ[™

JJBˆ˜ÛÛXÝ

NÂˆ
Û\ËÚ[›™[ÚÚ[™ÊBˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØÛ\Û[Ý™WÜÛ˜\×Ø[™ØÚ[™Ù\×Ý˜XÚÊ
HÂˆ]˜YÈHXÝ]™T^[\ÝÛ\˜YÈÂˆ\œ˜[™Ù[Y[ÚYˆËˆÛ\Ú[™^ˆKˆÝ\ÜÚ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆÝ\ÜÜÚ][Û—ÝXÚÜÎˆLŒˆÝ\Û[™ÝÝXÚÜÎˆÎˆÝ\Ý˜XÚ×Ú[™^ˆ‹ˆÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÎˆŒˆÝ\ÜÛÝ\˜ÙWÙ[™Û\ÎˆŒˆÛÝ\˜ÙWÛ[™ÝÛ\ÎˆŒˆ[\×ØœNˆMŒˆÚ[™ˆ^[\ÝÛ\˜YÒÚ[™Ž“[Ý™KˆNÂˆ]Y]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠLNKŒ‹LŽŒ
KˆŒKˆËŒˆM‹ˆYKˆMKˆ
NÂ‚ˆ\ÜÙ\Ù\HJY]œÜÚ][Û—ÝXÚÜËÛÛYJÌLŠJNÂˆ\ÜÙ\Ù\HJY]œ˜]×Ý˜XÚ×Ú[™^ÛÛYJMŠJNÂˆ\ÜÙ\Ù\HJY]›[™ÝÝXÚÜË›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØÛ\Û[Ý™WÝ\Ù\×Ùœ™YWÝXÚÜ×ÝÚ]Ø[Ø[™ØÛ[\×Ý˜XÚÜÊ
HÂˆ]˜YÈHXÝ]™T^[\ÝÛ\˜YÈÂˆ\œ˜[™Ù[Y[ÚYˆËˆÛ\Ú[™^ˆKˆÝ\ÜÚ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆÝ\ÜÜÚ][Û—ÝXÚÜÎˆLŒˆÝ\Û[™ÝÝXÚÜÎˆÎˆÝ\Ý˜XÚ×Ú[™^ˆ‹ˆÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÎˆŒˆÝ\ÜÛÝ\˜ÙWÙ[™Û\ÎˆŒˆÛÝ\˜ÙWÛ[™ÝÛ\ÎˆŒˆ[\×ØœNˆMŒˆÚ[™ˆ^[\ÝÛ\˜YÒÚ[™Ž“[Ý™KˆNÂˆ]Y]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠLKŒËŒŽŒ
KˆŒKˆËŒˆM‹ˆ˜[ÙKˆËˆ
NÂ‚ˆ\ÜÙ\Ù\HJY]œÜÚ][Û—ÝXÚÜËÛÛYJLÌÊJNÂˆ\ÜÙ\Ù\HJY]œ˜]×Ý˜XÚ×Ú[™^ÛÛYJMŠJNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØÛ\Ü™\Ú^™WÜÛ˜\×Ø[™ÚÙY\×ØWÜÜÚ]]™WÛ[™Ý

HÂˆ]˜YÈHXÝ]™T^[\ÝÛ\˜YÈÂˆ\œ˜[™Ù[Y[ÚYˆËˆÛ\Ú[™^ˆKˆÝ\ÜÚ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆÝ\ÜÜÚ][Û—ÝXÚÜÎˆLŒˆÝ\Û[™ÝÝXÚÜÎˆÎˆÝ\Ý˜XÚ×Ú[™^ˆ‹ˆÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÎˆŒˆÝ\ÜÛÝ\˜ÙWÙ[™Û\ÎˆŒˆÛÝ\˜ÙWÛ[™ÝÛ\ÎˆŒˆ[\×ØœNˆMŒˆÚ[™ˆ^[\ÝÛ\˜YÒÚ[™Ž”™\Ú^™KˆNÂˆ]Y]H^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠLL‹ŒLŒ
KˆŒKˆËŒˆM‹ˆYKˆMKˆ
NÂˆ\ÜÙ\Ù\HJY]›[™ÝÝXÚÜËÛÛYJL
JNÂˆ\ÜÙ\Ù\HJY]œÜÚ][Û—ÝXÚÜË›Û™JNÂ‚ˆ]Z[š[][HH^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠLLŒLŒ
KˆŒKˆËŒˆM‹ˆ˜[ÙKˆMKˆ
NÂˆ\ÜÙ\Ù\HJZ[š[][K›[™ÝÝXÚÜËÛÛYJJJNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØ]Y[×ØÛ\ÜÛ\ÜÛ˜\×ÜÚYÛ™YÝXÚÜ×Ø[™ØÛ[\×Ý×ÜØ[\J
HÂˆ]˜YÈHXÝ]™T^[\ÝÛ\˜YÈÂˆ\œ˜[™Ù[Y[ÚYˆËˆÛ\Ú[™^ˆKˆÝ\ÜÚ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆÝ\ÜÜÚ][Û—ÝXÚÜÎˆLŒˆÝ\Û[™ÝÝXÚÜÎˆÎˆÝ\Ý˜XÚ×Ú[™^ˆ‹ˆÝ\ÜÛÝ\˜ÙWÜÝ\Û\ÎˆÌŒˆÝ\ÜÛÝ\˜ÙWÙ[™Û\ÎˆLŒˆÛÝ\˜ÙWÛ[™ÝÛ\ÎˆWÌŒˆ[\×ØœNˆMŒˆÚ[™ˆ^[\ÝÛ\˜YÒÚ[™Ž”Û\ˆNÂ‚ˆ]Û˜\YH^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠMKŒ‹LŒ
KˆŒKˆËŒˆM‹ˆYKˆMKˆ
NÂˆ]^XÝYÜÝ\Û\ÈHÌŒHŒ
ˆŒÌŒÈ
M‹Œ
ˆMŒ
NÂˆ\ÜÙ\J
Û˜\YœÝ\ÛÙ™œÙ][Ü˜\

HH^XÝYÜÝ\Û\ÊK˜XœÊ
HŒJNÂˆ\ÜÙ\J
Û˜\Y™[™ÛÙ™œÙ][Ü˜\

HH
^XÝYÜÝ\Û\È
ÈŒŒ
JK˜XœÊ
HŒJNÂˆ\ÜÙ\Ù\HJÛ˜\YœÜÚ][Û—ÝXÚÜË›Û™JNÂˆ\ÜÙ\Ù\HJÛ˜\Y›[™ÝÝXÚÜË›Û™JNÂ‚ˆ]œ™YHH^[\ÝØÛ\Ù˜Y×ÙY]
ˆ˜YËˆYœ˜[YNŽ™YÝZNŽœÜÌŠLLŒLŒ
KˆŒKˆËŒˆM‹ˆ˜[ÙKˆMKˆ
NÂˆ\ÜÙ\Ù\HJœ™YKœÝ\ÛÙ™œÙ]ÛÛYJŒ
JNÂˆ\ÜÙ\Ù\HJœ™YK™[™ÛÙ™œÙ]ÛÛYJWÌŒ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØÛ\ÜÜ]Ý\Ù\×ØÝ\œÛÜ—Ø[™Ü™\ÜXÝ×Ø[ÜÛ˜\[™Ê
HÂˆ]Û\Ü™XÝHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÛX^
ˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒŒŒ
KˆYœ˜[YNŽ™YÝZNŽœÜÌŠMŒŒ
Kˆ
NÂˆ]Ú[\ˆHYœ˜[YNŽ™YÝZNŽœÜÌŠLËŒÌŒ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÜÜ]ÜÜÚ][ÛŠÚ[\‹Û\Ü™XÝLŒÎŒKM‹YJKˆÛÛYJNLŠBˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÜÜ]ÜÜÚ][ÛŠÚ[\‹Û\Ü™XÝLŒÎŒKM‹˜[ÙJKˆÛÛYJNL
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØÛ\ÜÜ]ÜÜÚ][ÛŠˆYœ˜[YNŽ™YÝZNŽœÜÌŠLKŒÌŒ
KˆÛ\Ü™XÝˆLŒˆÎˆŒKˆM‹ˆYKˆ
Kˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØ˜\—Ø›Ý[™\šY\×Ù›ÛÝ×Ü›Ú™XÝØ[™ÛX\šÙ\—ÜÚYÛ˜]\™\Ê
HÂˆ\ÜÙ\Ù\HJˆ^[\ÝÛYX\Ý\™WØ›Ý[™\šY\ÊM‹ÛÛYJ

JK	–×KWÍLÍŠKˆ™XÈVÊJK
ÎŠK
ÍŽÊK
WÌML‹
K
WÍLÍ‹JWBˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÛYX\Ý\™WØ›Ý[™\šY\ÊM‹ÛÛYJ
Ë
JK	–×KÌŠKˆ™XÈVÊJK
MŠK
ŽÊK
Ì‹
WBˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÛYX\Ý\™WØ›Ý[™\šY\ÊM‹ÛÛYJ

JK	–ÊÍŽË
WKWÌÍ
Kˆ™XÈVÊJK
ÎŠK
ÍŽÊK
WÌM‹
K
WÌÍJWBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÛZY˜\—ÜÚYÛ˜]\™WÜÝ\×ØWÛ™]×ÛYX\Ý\™WØ]Ú]×ÛX\šÙ\Š
HÂˆ\ÜÙ\Ù\HJˆ^[\ÝÛYX\Ý\™WØ›Ý[™\šY\ÊM‹ÛÛYJ

JK	–ÊMÍ‹Ë
WKWÌMLŠKˆ™XÈVÊJK
ÎŠK
MÍ‹ÊK

K
WÌML‹JWBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝØ˜\—Û[™ÝÝ\Ù\×ÝWØXÝ]™WÛY]\Š
HÂˆ\ÜÙ\Ù\HJ^[\ÝØ˜\—ÝXÚÜÊM‹

JKÎ
NÂˆ\ÜÙ\Ù\HJ^[\ÝØ˜\—ÝXÚÜÊM‹
K
JK
NÂˆ\ÜÙ\Ù\HJ^[\ÝØ˜\—ÝXÚÜÊM‹
ËMŠJKMŽ
NÂˆ\ÜÙ\Ù\HJ^[\ÝØ˜\—ÝXÚÜÊ

JK
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊÛÛYJ

JK	–ÊÎË
K
ÍŽK
WKÌ
Kˆ
Ë
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÚYÛ˜]\™WØ]ÝXÚÊÛÛYJ

JK	–ÊÎË
K
ÍŽK
WKÍŽ
Kˆ
K
Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÙÜ›Ý\Ü˜[™ÙWÚ[˜ÛY\×Ü\™[Ø[™Ù›ÛÝÚ[™×ØÚ[Ý˜XÚÜÊ
HÂˆ]˜XÚÜÈH
K‹M
Bˆ›X\
Y^[\Ý˜XÚÈÂˆYˆÝ]WØž]Nˆ›Û™Kˆ˜[YNˆ›Û™Kˆ[˜X›YˆÛÛYJYJKˆÜ›Ý\YˆÛÛYJX]Ú\ÈJYˆÊJKˆ™\ÜÎˆ›Û™Kˆ[Ý[ÛŽˆ›Û™KˆšYÙÙ\—ÜÞ[˜Îˆ›Û™Kˆ]Y]YYˆ›Û™KˆÜÚ][Û—ÜÞ[˜Îˆ›Û™KˆÛ\˜[ˆ›Û™KˆÝ]WØž]\Îˆ™XÈVÌÈÌKˆJBˆ˜ÛÛXÝŽ™XÏÏŠ
NÂ‚ˆ\ÜÙ\Ù\HJ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJK	˜XÚÜÊKÛÛYJ
KÊJJNÂˆ\ÜÙ\Ù\HJ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJ‹	˜XÚÜÊKÛÛYJ
KÊJJNÂˆ\ÜÙ\Ù\HJ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJË	˜XÚÜÊKÛÛYJ
KÊJJNÂˆ\ÜÙ\Ù\HJ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJ	˜XÚÜÊKÛÛYJ

JJNÂˆ\ÜÙ\Ù\HJ^[\ÝÝ˜XÚ×ÙÜ›Ý\Ü˜[™ÙJK	˜XÚÜÊK›Û™JNÂˆ\ÜÙ\Ù\HJ^[\ÝÙÜ›Ý\Ü\™[ÚYÊ	˜XÚÜÊK™XÈVÌWJNÂ‚ˆ]ÛÛ\ÙYH•™YTÙ]Ž™œ›ÛJÌWJNÂˆ\ÜÙ\J\^[\ÝÝ˜XÚ×Ú\×ÚY[ŠK	˜XÚÜË	˜ÛÛ\ÙYYJJNÂˆ\ÜÙ\J^[\ÝÝ˜XÚ×Ú\×ÚY[Š‹	˜XÚÜË	˜ÛÛ\ÙYYJJNÂˆ\ÜÙ\J^[\ÝÝ˜XÚ×Ú\×ÚY[ŠË	˜XÚÜË	˜ÛÛ\ÙYYJJNÂˆ\ÜÙ\J\^[\ÝÝ˜XÚ×Ú\×ÚY[Š	˜XÚÜË	˜ÛÛ\ÙYYJJNÂˆ\ÜÙ\J\^[\ÝÝ˜XÚ×Ú\×ÚY[ŠK	˜XÚÜË	˜ÛÛ\ÙYYJJNÂˆ\ÜÙ\J\^[\ÝÝ˜XÚ×Ú\×ÚY[Š‹	˜XÚÜË	˜ÛÛ\ÙY˜[ÙJJNÂˆ\ÜÙ\J\^[\ÝÝ˜XÚ×Ú\×ÚY[ŠË	˜XÚÜË	˜ÛÛ\ÙY˜[ÙJJNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÜÙYZ×ÝXÚ×ØÛÛ™\×Ý×ÜÛÛ™×Ùœ˜[Y\Ê
HÂˆ][\×ÛX\H^[\Ý[\ÓX\Ž˜ÛÛœÝ[
M‹LŒŒ
K[Ü˜\

NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÙYZ×ÝXÚ×Ý×Ùœ˜[YJÎ	[\×ÛX\Ì
KˆÚÊM—Ì
Bˆ
NÂˆ\ÜÙ\Ù\HJ^[\ÝÜÙYZ×ÝXÚ×Ý×Ùœ˜[YJ	[\×ÛX\Ì
KÚÊ
JNÂˆ\ÜÙ\J^[\ÝÜÙYZ×ÝXÚ×Ý×Ùœ˜[YJÎ	[\×ÛX\
Kš\×Ù\œŠ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—Ý\Ù\×ØXÝ]™WÛX\šÙ\—ÛY]\Š
HÂˆ]ÚYÛ˜]\™WØÚ[™Ù\ÈHÊË
K
MÍ‹K
WNÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
M‹ŒM‹ÛÛYJ

JK	–×JKˆŒNŒŽŒ‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
ÎŒM‹ÛÛYJ

JK	–×JKˆŒŽŒNŒ‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
ÌŒM‹ÛÛYJ

JK	–ÊMÍ‹Ë
WJKˆŒÎŒŽŒŽ‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
LŒM‹ÛÛYJ

JK	œÚYÛ˜]\™WØÚ[™Ù\ÊKˆŒŽŒÎŒŒ‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
ÌŒM‹ÛÛYJ

JK	œÚYÛ˜]\™WØÚ[™Ù\ÊKˆŒÎŒÎŒŽ‚ˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜÛÛ™×ÜÜÚ][Û—ÛX™[
WÎLŒM‹ÛÛYJ

JK	œÚYÛ˜]\™WØÚ[™Ù\ÊKˆŽŒÎŒŽ‚ˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ]\›—Û[ÙWÜ^ZXYÝÜ˜\×Ø]ÝWÜ]\›—ÛÛÜ

HÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJÌM‹LŒŒÎ
KˆÛÛYJŒ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJÌÌM‹LŒŒÎ
KˆÛÛYJM‹Œ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJLŒÌÌM‹LŒŒÎ
KˆÛÛYJM‹Œ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJM—ÌÌM‹LŒŒÎ
KˆÛÛYJŒ
Bˆ
NÂˆ\ÜÙ\Ù\HJ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJÌM‹LŒŒ
K›Û™JNÂˆ\ÜÙ\Ù\HJ]\›—ÛÛÜÝXÚ×Ø]Ùœ˜[YJM‹LŒŒÎ
K›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ]\›—ÛÛÜÜÙYZ×ÝXÚ×ÛX\×Ý×Ú]×Ø]Y[×Ü\ÙJ
HÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊÌM‹LŒŒÎ
KˆÛÛYJ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊM‹ÌM‹LŒŒÎ
KˆÛÛYJÌ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊÌM‹LŒŒÎ
KˆÛÛYJÌ
Bˆ
NÂˆ\ÜÙ\Ù\HJˆ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊÎÌM‹LŒŒÎ
KˆÛÛYJ
Bˆ
NÂˆ\ÜÙ\Ù\HJ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊÌM‹LŒŒ
K›Û™JNÂˆ\ÜÙ\Ù\HJ]\›—ÛÛÜÙœ˜[YWØ]ÝXÚÊM‹LŒŒÎ
K›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÚ›Ú[—ÛY[WÙš[™×ÛÛ›WÝ[š\]YWØÛÛYÝ[Ý\×Ø]Y[×Û™ZYÚ›ÜœÊ
HÂˆ]
Û\ËÚ[›™[ÚÚ[™ÊHH^[\ÝØ]Y[×ØÛ\Ùš^\™J
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[—ØØ[™Y]\Ê	˜Û\Ë	˜Ú[›™[ÚÚ[™Ë	•™YTÙ]Ž›™]Ê
JKˆ
›Û™KÛÛYJJJBˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[—ØØ[™Y]\Ê	˜Û\ËK	˜Ú[›™[ÚÚ[™Ë	•™YTÙ]Ž›™]Ê
JKˆ
ÛÛYJ
K›Û™JBˆ
NÂˆ\ÜÙ\Ù\HJˆ^[\ÝØ]Y[×ØÛ\Ú›Ú[—ØØ[™Y]\Êˆ	˜Û\ËˆÛ\Ë›[Š
Kˆ	˜Ú[›™[ÚÚ[™Ëˆ	•™YTÙ]Ž›™]Ê
Bˆ
Kˆ
›Û™K›Û™JBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÚ›Ú[—ÛY[WÙš[™×ÛÛ›WÜ™\X]ÜØY™WÜ]\›—Û™ZYÚ›ÜœÊ
HÂˆ]
]]Û\ËÊHH^[\ÝØ]Y[×ØÛ\Ùš^\™J
NÂˆ›ÜˆÛ\[ˆ	›]]Û\ÈÂˆÛ\œ]\›—Ø˜\ÙHHLÂˆÛ\š][WÚ[™^HLÎÂˆÛ\œÝ\ÛÙ™œÙ]HŒÂˆÛ\™[™ÛÙ™œÙ]HKŒÂˆBˆ]]\›ˆH]\›ˆÂˆYˆËˆ˜[YNˆ›Û™Kˆ[™ÝÝXÚÜÎˆÛÛYJMŠKˆ›Ý\Îˆ™XÎŽ›™]Ê
KˆÛÛ›Û\œÎˆ™XÎŽ›™]Ê
Kˆ[YWÛX\šÙ\œÎˆ™XÎŽ›™]Ê
KˆNÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\Êˆ	˜Û\ËˆˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›ŠKˆ	•™YTÙ]Ž›™]Ê
Bˆ
Kˆ
›Û™KÛÛYJJJBˆ
NÂ‚ˆÛ\ÖÌK›[™ÝÝXÚÜÈHLÂˆÛ\ÖÌWKœÜÚ][Û—ÝXÚÜÈHLÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\Êˆ	˜Û\ËˆˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›ŠKˆ	•™YTÙ]Ž›™]Ê
Bˆ
Kˆ
›Û™K›Û™JBˆ
NÂ‚ˆ]]\›—ÝÚ]ØÜ›ÜÜÚ[™×Û›ÝHH]\›ˆÂˆ›Ý\Îˆ™XÈVÔ]\›“›ÝHÂˆÜÚ][ÛŽˆˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆWKˆ‹œ]\›‹˜ÛÛ™J
BˆNÂˆÛ\ÖÌK›[™ÝÝXÚÜÈHNLŽÂˆÛ\ÖÌWKœÜÚ][Û—ÝXÚÜÈHNLŽÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\Êˆ	˜Û\ËˆˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›—ÝÚ]ØÜ›ÜÜÚ[™×Û›ÝJKˆ	•™YTÙ]Ž›™]Ê
Bˆ
Kˆ
›Û™K›Û™JBˆ
NÂ‚ˆ]]\›—ÝÚ]ÛÝ]ÚYWÛÛ™WÜÚÝH]\›ˆÂˆ›Ý\Îˆ™XÈVÔ]\›“›ÝHÂˆÜÚ][ÛŽˆM‹ˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆWKˆ‹œ]\›‹˜ÛÛ™J
BˆNÂˆ\ÜÙ\Ù\HJˆ^[\ÝÜ]\›—ØÛ\Ú›Ú[—ØØ[™Y]\Êˆ	˜Û\ËˆˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›—ÝÚ]ÛÝ]ÚYWÛÛ™WÜÚÝ
Kˆ	•™YTÙ]Ž›™]Ê
Bˆ
Kˆ
›Û™K›Û™JBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^[\ÝÜ]\›—ÜÜ]ÛY[WÜ™\]Z\™\×Ü™\X]Ø›Ý[™\žWØ[™ØÞXÛWÜØY™WÛ›Ý\Ê
HÂˆ]
]]Û\ËÊHH^[\ÝØ]Y[×ØÛ\Ùš^\™J
NÂˆÛ\ÖÌKœÜÚ][Û—ÝXÚÜÈHÂˆÛ\ÖÌK›[™ÝÝXÚÜÈHNLŽÂˆÛ\ÖÌKœ]\›—Ø˜\ÙHHLÂˆÛ\ÖÌKš][WÚ[™^HLÎÂˆ]Û\H	˜Û\ÖÌNÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ[™ÝÝXÚÜÎˆÛÛYJMŠKˆ›Ý\Îˆ™XÈVÔ]\›“›ÝHÂˆÜÚ][ÛŽˆˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆWKˆ‹”]\›ŽŽ™Y˜][

BˆNÂ‚ˆ\ÜÙ\J^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\ˆÌÍ‹ˆ	˜Û\ËˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›ŠKˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂˆ\ÜÙ\J\^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\ˆŽˆ	˜Û\ËˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›ŠKˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂ‚ˆ]]\›—ÝÚ]ØÜ›ÜÜÚ[™×Û›ÝHH]\›ˆÂˆ›Ý\Îˆ™XÈVÔ]\›“›ÝHÂˆÜÚ][ÛŽˆˆ[™Ýˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆWKˆ‹œ]\›‹˜ÛÛ™J
BˆNÂˆ\ÜÙ\J\^[\ÝÜ]\›—ØÛ\ÜÜ]ÜÝ\ÜY
ˆÛ\ˆÌÍ‹ˆ	˜Û\ËˆÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ]\›—ÝÚ]ØÜ›ÜÜÚ[™×Û›ÝJKˆ	•™YTÙ]Ž›™]Ê
Kˆ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜÛ˜\ÜÚ^™\×Ù›ÛÝ×Ü›Ú™XÝÜWØ[™ÛY]\Š
HÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž“›Û™KXÚÜÊM‹›Û™JKJNÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž”]X\\™X]XÚÜÊM‹›Û™JK
NÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•š\]Ú^Y[XÚÜÊM‹›Û™JKMŠNÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž’[™X]XÚÜÊM‹›Û™JK
NÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•š\]ZYÚXÚÜÊM‹›Û™JKÌŠNÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž™X]XÚÜÊM‹›Û™JKMŠNÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•š\]]X\\‹XÚÜÊM‹›Û™JK
NÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•ÛÐ™X]ËXÚÜÊM‹›Û™JKNLŠNÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•š\][‹XÚÜÊM‹›Û™JKLŽ
NÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž˜\‹XÚÜÊM‹ÛÛYJ
Ë
JJKŽ
NÂˆ\ÜÙ\Ù\HJX[›Ô›ÛÛ˜\Ž•š\]Ú^Y[XÚÜÊK›Û™JKJNÂˆB‚ˆÖÝ\ÝBˆ›ˆÚ[›™[Ù\Ü^WÙš[\—ÛX]Ú\×Û˜[YYØ[™Ý[œÛÜYØÚ[›™[Ê
HÂˆ]Û›ÝÛ—ÙÜ›Ý\ÈH•™YTÙ]Ž™œ›ÛJÌWJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ[œÚÝÜÊ›Û™K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ[œÚÝÜÊÛÛYJJK	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ•[œÛÜYœÚÝÜÊ›Û™K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ•[œÛÜYœÚÝÜÊÛÛYJLJK	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ•[œÛÜYœÚÝÜÊÛÛYJ
K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JPÚ[›™[\Ü^Qš[\ŽŽ•[œÛÜYœÚÝÜÊÛÛYJ
K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JKœÚÝÜÊÛÛYJJK	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JPÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JKœÚÝÜÊÛÛYJ
K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆ\ÜÙ\JPÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JKœÚÝÜÊ›Û™K	šÛ›ÝÛ—ÙÜ›Ý\ÊJNÂˆB‚ˆÖÝ\ÝBˆ›ˆYÙWÛ˜]šYØ][Û—ØÞXÛ\×Û˜[YYÙÜ›Ý\×Ùœ›ÛWØ[ÛÜ—Ý[œÛÜY

HÂˆ]Ü›Ý\ÈHÌWNÂˆ\ÜÙ\Ù\HJˆÚ[›™[\Ü^Qš[\ŽŽ[œÝ\ÙÜ›Ý\
	™Ü›Ý\ËJKˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\

Bˆ
NÂˆ\ÜÙ\Ù\HJˆÚ[›™[\Ü^Qš[\ŽŽ•[œÛÜYœÝ\ÙÜ›Ý\
	™Ü›Ý\ËLJKˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JBˆ
NÂˆ\ÜÙ\Ù\HJˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JKœÝ\ÙÜ›Ý\
	™Ü›Ý\ËJKˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\

Bˆ
NÂˆ\ÜÙ\Ù\HJˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\

KœÝ\ÙÜ›Ý\
	™Ü›Ý\ËLJKˆÚ[›™[\Ü^Qš[\ŽŽ‘Ü›Ý\
JBˆ
NÂˆ\ÜÙ\Ù\HJˆÚ[›™[\Ü^Qš[\ŽŽ[œÝ\ÙÜ›Ý\
	–×KJKˆÚ[›™[\Ü^Qš[\ŽŽ[ˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ^Y\—ØÚ[ÙY]Ü—ØY×Ø[™Ü™[[Ý™\×ØÚ[›™[ÜÙ[XÝ[ÛœÊ
HÂˆ]]]Ú[ÚYÈH™XÈVÍ—NÂˆ\]WÛ^Y\—ØÚ[ÜÙ[XÝ[ÛŠ	›]]Ú[ÚYËËYJNÂˆ\ÜÙ\Ù\HJÚ[ÚYËÍ‹×JNÂˆ\]WÛ^Y\—ØÚ[ÜÙ[XÝ[ÛŠ	›]]Ú[ÚYË‹˜[ÙJNÂˆ\ÜÙ\Ù\HJÚ[ÚYËÍ×JNÂˆ\]WÛ^Y\—ØÚ[ÜÙ[XÝ[ÛŠ	›]]Ú[ÚYËËYJNÂˆ\ÜÙ\Ù\HJÚ[ÚYËÍ×JNÂˆB‚ˆÖÝ\ÝBˆ›ˆÚ[›™[Ü˜XÚ×ÜÙ[XÝ[Û—ÝÙÙÛ\×Ø[™Ù^[™×Ü˜[™Ù\×ØžWØÚ[›™[ÚY

HÂˆ]Ú[›™[ÚYÈHÌKË—NÂˆ]]]Ù[XÝYH•™YTÙ]Ž™œ›ÛJÌWJNÂˆ]]][˜ÚÜˆHÛÛYJJNÂ‚ˆ\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[ÛŠ	˜Ú[›™[ÚYË	›]]Ù[XÝY	›]][˜ÚÜ‹ËYK˜[ÙJNÂˆ\ÜÙ\Ù\HJÙ[XÝY•™YTÙ]Ž™œ›ÛJÌK×JJNÂˆ\ÜÙ\Ù\HJ[˜ÚÜ‹ÛÛYJÊJNÂ‚ˆ\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[ÛŠ	˜Ú[›™[ÚYË	›]]Ù[XÝY	›]][˜ÚÜ‹‹˜[ÙKYJNÂˆ\ÜÙ\Ù\HJÙ[XÝY•™YTÙ]Ž™œ›ÛJÌ‹×JJNÂˆ\ÜÙ\Ù\HJ[˜ÚÜ‹ÛÛYJÊJNÂ‚ˆ\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[ÛŠ	˜Ú[›™[ÚYË	›]]Ù[XÝY	›]][˜ÚÜ‹ËYK˜[ÙJNÂˆ\ÜÙ\Ù\HJÙ[XÝY•™YTÙ]Ž™œ›ÛJÌ—JJNÂˆ\]WØÚ[›™[Ü˜XÚ×ÜÙ[XÝ[ÛŠ	˜Ú[›™[ÚYË	›]]Ù[XÝY	›]][˜ÚÜ‹‹YK˜[ÙJNÂˆ\ÜÙ\Ù\HJÙ[XÝY•™YTÙ]Ž™œ›ÛJÌ—JJNÂˆB‚ˆÖÝ\ÝBˆ›ˆ›ÝWÝXÚ×ÜÛ˜\[™×Ü›Ý[™×Ø[™Ü™\ÜXÝ×Û[™ÝÛZ[š[][J
HÂˆ\ÜÙ\Ù\HJÛ˜\Û›ÝWÝXÚÊLL‹
K
NÂˆ\ÜÙ\Ù\HJÛ˜\Û›ÝWÝXÚÊÍK
K
NÂˆ\ÜÙ\Ù\HJÛ˜\Û›ÝWÝXÚÊÍ‹
K
NÂˆ\ÜÙ\Ù\HJÛ˜\Û›ÝWÝXÚÊJKJNÂˆ\ÜÙ\Ù\HJÛ˜\Û›ÝWÝXÚÊMŽ™œ›ÛJLÌŽŽ“PV
KK
KLÌŽŽ“PV
NÂˆB‚ˆÖÝ\ÝBˆ›ˆ˜]×ØÛÛ›Û\—Ù]™[Ü˜[™ÙWÙš[\œ×ÜÝ™X[\×Ø[™Ú[™\×Ý[\ÝX[Ý˜[Y\Ê
HÂˆ]ÛÛ›Û\œÈHÂˆ]\›ÛÛ›Û\ˆÂˆÚ[›™[ˆ‹ˆ›YÜÎˆˆ˜[YWØš]ÎˆŒWÙŒÌ‹×Øš]Ê
Kˆ‹”]\›ÛÛ›Û\ŽŽ™Y˜][

BˆKˆ]\›ÛÛ›Û\ˆÂˆÚ[›™[ˆ‹ˆ›YÜÎˆˆ˜[YWØš]ÎˆÍWÙŒÌ‹×Øš]Ê
Kˆ‹”]\›ÛÛ›Û\ŽŽ™Y˜][

BˆKˆ]\›ÛÛ›Û\ˆÂˆÚ[›™[ˆËˆ›YÜÎˆLˆ˜[YWØš]ÎˆŒÙŒÌ‹×Øš]Ê
Kˆ‹”]\›ÛÛ›Û\ŽŽ™Y˜][

BˆKˆ]\›ÛÛ›Û\ˆÂˆÚ[›™[ˆˆ›YÜÎˆˆ˜[YWØš]ÎˆŒÌŽŽ“S‹×Øš]Ê
Kˆ‹”]\›ÛÛ›Û\ŽŽ™Y˜][

BˆKˆNÂ‚ˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWÜ˜[™ÙJ	˜ÛÛ›Û\œËÛÛYJ
‹
JJKˆ
ŒKŒ
Bˆ
NÂˆ]˜]×Ü˜[™ÙHHX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWÜ˜[™ÙJ	˜ÛÛ›Û\œËÛÛYJ
ËL
JJNÂˆ\ÜÙ\J˜]×Ü˜[™ÙKŒŒ	‰ˆ˜]×Ü˜[™ÙKŒHˆŒ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWÜ˜[™ÙJ	˜ÛÛ›Û\œËÛÛYJ

JJKˆ
ŒKŒ
Bˆ
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWØ]
ŒŒKŒ
KŒ
NÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWØ]
KŒŒKŒ
KKŒ
NÂˆ\ÜÙ\JX[›×Ü›ÛØÛÛ›Û\—Ý˜[YWØ]
K˜]×Ü˜[™ÙKŒ˜]×Ü˜[™ÙKŒJKš\×Ùš[š]J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆ˜]×ØÛÛ›Û\—ÜÝ›ÚÙ\×Ú[\œÛ]WÜØ[\\×Ø[™Ü™\ÜXÝÝXÚ×ÜÝ\

HÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛ›Û\—ÜÝ›ÚÙWÜÚ[ÊŒKŒ
Kˆ™XÈVÊJK
KŒ
WBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛ›Û\—ÜÝ›ÚÙWÜÚ[ÊKŒŒ
Kˆ™XÈVÊJK
Œ
WBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛØÛÛ›Û\—ÜÝ›ÚÙWÜÚ[ÊL‹ŒKL‹ÍK
Kˆ™XÈVÊL‹ÍJWBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÛ›ÝWÙÜ›Ý\×ÜÙ[XÝÝÙÙÛWØ[™Ø[ØØ]WÜ\—ØÚ[›™[

HÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆËˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂˆ]Y[X™\œÈHX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÛY[X™\œÊ	œ]\›‹]\›‹šY‹
NÂˆ\ÜÙ\Ù\HJY[X™\œË™XÈVÊË‹
K
Ë‹JWJNÂ‚ˆ]]]Ù[XÝYH•™YTÙ]Ž›™]Ê
NÂˆ\ÜÙ\JÙÙÛWÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÜÙ[XÝ[ÛŠˆ	›]]Ù[XÝYˆ	›Y[X™\œËˆYBˆ
JNÂˆ\ÜÙ\Ù\HJÙ[XÝY•™YTÙ]Ž™œ›ÛJÊË‹
K
Ë‹JWJJNÂˆ\ÜÙ\J]ÙÙÛWÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\ÜÙ[XÝ[ÛŠˆ	›]]Ù[XÝYˆ	›Y[X™\œËˆYBˆ
JNÂˆ\ÜÙ\JÙ[XÝYš\×Ù[\J
JNÂ‚ˆ]]]Ù[XÝYØžWØ›ÞH•™YTÙ]Ž›™]Ê
NÂˆ\]WÜX[›×Ü›ÛØ›ÞÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	–Ì—KYK	›]]Ù[XÝYØžWØ›Þ
NÂˆ\ÜÙ\Ù\HJˆÙ[XÝYØžWØ›Þˆ•™YTÙ]Ž™œ›ÛJÊË‹
K
Ë‹JK
Ë‹ŠWJBˆ
NÂˆ\]WÜX[›×Ü›ÛØ›ÞÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	–Ì—KYK	›]]Ù[XÝYØžWØ›Þ
NÂˆ\ÜÙ\JÙ[XÝYØžWØ›Þš\×Ù[\J
JNÂ‚ˆ\ÜÙ\Ù\HJ™^ÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\
	œ]\›‹ŠKÛÛYJJJNÂˆ]ØØÝ\YYH]\›ˆÂˆ›Ý\Îˆ™XÈVÔ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆWKˆ‹œ]\›‹˜ÛÛ™J
BˆNÂˆ\ÜÙ\Ù\HJ™^ÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\
	›ØØÝ\YYŠKÛÛYJŠJNÂˆ\ÜÙ\Ù\HJ™^ÜX[›×Ü›ÛÛ›ÝWÙÜ›Ý\
	œ]\›‹ÊKÛÛYJJJNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÜ˜[™ÛWÜÙ[XÝ[Û—Ü™\ÜXÝ×ØÚ[›™[Ø[™Û›ÝWÙÜ›Ý\Ê
HÂˆ]]\›ˆH]\›ˆÂˆYˆËˆ›Ý\Îˆ™XÈVÂˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆËˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆÜ›Ý\ˆKˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆ]\›“›ÝHÂˆÚ[›™[ÚYˆ‹ˆ‹”]\›“›ÝNŽ™Y˜][

BˆKˆKˆ‹”]\›ŽŽ™Y˜][

BˆNÂˆ][\HH•™YTÙ]Ž›™]Ê
NÂˆ]š\œÝHX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	™[\K˜[ÙKLJBˆ™^XÝ
H\™Ù]Ú[›™[\ÈÙ[XÝX›H›Ý\ÈŠNÂˆ\ÜÙ\Jˆš\œÝOH•™YTÙ]Ž™œ›ÛJÊË‹
K
Ë‹JWJHš\œÝOH•™YTÙ]Ž™œ›ÛJÊË‹ŠWJBˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	™[\K˜[ÙKLJKˆÛÛYJš\œÝ˜ÛÛ™J
JBˆ
NÂ‚ˆ][Ü™HHX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	™š\œÝYKLÊBˆ™^XÝ
˜[›Ý\ˆ\™Ù]XÚ[›™[Ü›Ý\™[XZ[œÈŠNÂˆ\ÜÙ\Ù\HJ[Ü™K•™YTÙ]Ž™œ›ÛJÊË‹
K
Ë‹JK
Ë‹ŠWJJNÂˆ\ÜÙ\JX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë‹	›[Ü™KYKMÊKš\×Û›Û™J
JNÂˆ\ÜÙ\JX[›×Ü›ÛÜ˜[™ÛWÛ›ÝWÜÙ[XÝ[ÛŠ	œ]\›‹Ë	™[\K˜[ÙKLJKš\×Û›Û™J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÛ›ÝWÛYÙWÜ™\ÜXÝ×ÙÜšYÙ\Ý[˜ÙWØ[™Ü]ÚØ›Ý[™Ê
HÂˆ]›ÝHH]\›“›ÝHÂˆÜÚ][ÛŽˆŒËˆÙ^NˆŒˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ][Ý™YHX[›×Ü›ÛÛ›ÝWÛYÙWÙY]
	››ÝKJNÂˆ\ÜÙ\Ù\HJ[Ý™YœÜÚ][Û‹ÛÛYJÊJNÂˆ\ÜÙ\Ù\HJ[Ý™YšÙ^KÛÛYJŒJJNÂ‚ˆ]™X\—ÜÝ\H]\›“›ÝHÂˆÜÚ][ÛŽˆLˆ‹››ÝK˜ÛÛ™J
BˆNÂˆ][Ý™YÛYHX[›×Ü›ÛÛ›ÝWÛYÙWÙY]
	›™X\—ÜÝ\L
NÂˆ\ÜÙ\Ù\HJ[Ý™YÛYœÜÚ][Û‹ÛÛYJ
JNÂˆ\ÜÙ\Ù\HJ[Ý™YÛYšÙ^K›Û™JNÂ‚ˆ]ÝÙ\ÝH]\›“›ÝHÂˆÙ^Nˆˆ‹››ÝK˜ÛÛ™J
BˆNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÛ›ÝWÛYÙWÙY]
	›ÝÙ\ÝLJKšÙ^K›Û™JNÂ‚ˆ]YÚ\ÝH]\›“›ÝHÈÙ^NˆLÌK‹››ÝHNÂˆ\ÜÙ\Ù\HJX[›×Ü›ÛÛ›ÝWÛYÙWÙY]
	šYÚ\ÝJKšÙ^K›Û™JNÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×Ü›ÛÙ\ØØ\™Û[™Ý×Ý\Ù\×ÜÛ˜\ÛZ[š[][WØ[™ÜÚÚ\×ÛX]Ú[™×Û›Ý\Ê
HÂˆ]›ÝHH]\›“›ÝHÂˆ[™ÝˆÎˆ‹”]\›“›ÝNŽ™Y˜][

BˆNÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÙ\ØØ\™Û[™ÝÙY]
	››ÝK
Bˆ™^XÝ
˜HÛ™Ù\ˆ›ÝHÚÝ[™HÚÜ[™YŠBˆ›[™ÝˆÛÛYJ
Bˆ
NÂˆ\ÜÙ\JˆX[›×Ü›ÛÙ\ØØ\™Û[™ÝÙY]
ˆ	”]\›“›ÝHÂˆ[™Ýˆˆ‹››ÝK˜ÛÛ™J
BˆKˆˆ
Bˆš\×Û›Û™J
Bˆ
NÂˆ\ÜÙ\Ù\HJˆX[›×Ü›ÛÙ\ØØ\™Û[™ÝÙY]
	”]\›“›ÝHÈ[™Ýˆ‹››ÝHK
Bˆ™^XÝ
˜H™\›ÈÛ˜\Ú^™HÚÝ[Û[\ÈÛ™HXÚÈŠBˆ›[™ÝˆÛÛYJJBˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆÝX›WØÛXÚ×ÙÜšYÛX\[™×Ø\Y\×Ü]ÚØÚ[›™[Ø[™ÜÛ˜\

HÂˆ]›ÝHH›ÝWÙœ›ÛWÙÜšYÜÜÚ][ÛŠYœ˜[YNŽ™YÝZNŽœÜÌŠŒËŒLŒKJK\ÝÙÜšY

KJBˆ™^XÝ
™ÜšYÜÚ][ÛˆÚÝ[›ÙXÙHH›ÝHŠNÂˆ\ÜÙ\Ù\HJ›ÝKœÜÚ][Û‹ÍŒ
NÂˆ\ÜÙ\Ù\HJ›ÝKšÙ^KÎ
NÂˆ\ÜÙ\Ù\HJ›ÝK˜Ú[›™[ÚYJNÂˆ\ÜÙ\Ù\HJ›ÝK›[™ÝMŠNÂˆ\ÜÙ\Ù\HJ›ÝK™[ØÚ]KL
NÂˆB‚ˆÖÝ\ÝBˆ›ˆX[›×ÚÙ^X›Ø\™Ø[™ÛÝ]ÚYWÙÜšYÙ×Û›ÝØÜ™X]WÛ›Ý\Ê
HÂˆ›ÜˆÚ[\ˆ[ˆÂˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒŒLŒ
KˆYœ˜[YNŽ™YÝZNŽœÜÌŠÌKŒLŒ
KˆHÂˆ\ÜÙ\J›ÝWÙœ›ÛWÙÜšYÜÜÚ][ÛŠÚ[\‹\ÝÙÜšY

KJKš\×Û›Û™J
JNÂˆBˆB‚ˆÖÝ\ÝBˆ›ˆÚ[™Ý×Û^[Ý]Ü™\Ù]×Ü›Ý[™Ýš\Ý[šXÛÙWÛ˜[Y\×Ø[™ÝÚ[™Ý×ÙÙ[ÛY]žJ
HÂˆ]]]XÙ[Y[ÈH]XÚYÚ[™ÝÔXÙ[Y[ÎŽ™Y˜][

NÂˆXÙ[Y[ËœÙ]
ˆ]XÚYÚ[™ÝÒÚ[™Ž”X[›Ô›Ûˆ]XÚYÚ[™ÝÔXÙ[Y[ÂˆÜÚ][ÛŽˆÛÛYJYœ˜[YNŽ™YÝZNŽœÜÌŠLLŒKŒ
JKˆ[›™\—ÜÚ^™NˆYœ˜[YNŽ™YÝZNŽ™XÌŠLŽŒÌŒŒ
KˆKˆ
NÂˆ]™\Ù]HÚ[™ÝÓ^[Ý]™\Ù]Âˆ˜[YNˆ“Z^[™È<'ã¦È‹×ÛÝÛ™Y

KˆXZ[—ÝšY]ÎˆXZ[•šY]ÎŽ”^[\Ýˆœ›ÝÜÙ\—Ýš\ÚX›Nˆ˜[ÙKˆ]XÚYˆÝYK˜[ÙKYKYK˜[ÙK˜[ÙK˜[ÙWKˆXÙ[Y[ËˆNÂˆ][˜ÛÙYHÙ\šX[^™WÝÚ[™Ý×Û^[Ý]Ü™\Ù]ÊÝŽœÛXÙNŽ™œ›ÛWÜ™YŠ	œ™\Ù]
JNÂˆ]XÛÙYH\œÙWÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê	™[˜ÛÙY
NÂˆ\ÜÙ\Ù\HJXÛÙY™XÈVÜ™\Ù]JNÂˆB‚ˆÖÝ\ÝBˆ›ˆÚ[™Ý×Û^[Ý]Ü™\Ù]×ÚYÛ›Ü™WÚ[˜[YÜ™XÛÜ™Ê
HÂˆ]ÛÛ[ÈH›Ü›X]JˆžÕÒS‘Õ×ÓVSÕUÔ‘TÑU×ÒPQTŸW››ÝXK\™XÛÜ™žßW[šÛ›ÝÛ—WWWWWWWWˆ‹ˆM™ŒŒÍŽMMÍÈ‚ˆ
NÂˆ\ÜÙ\J\œÙWÝÚ[™Ý×Û^[Ý]Ü™\Ù]Ê	˜ÛÛ[ÊKš\×Ù[\J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆÛ×Ùš[™Ù\—Ý\Ú\×Ù]XÝYÝÚ[—Ø›ÝÝÝXÚ\×Ü™[X\ÙWÜ]ZXÚÛJ
HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂˆ]Ù[\ˆHYœ˜[YNŽ™YÝZNŽœÜÌŠŒŒMLŒ
NÂˆ]]]˜XÚÙ\ŽˆÜ[ÛÝXÚ\˜XÚÙ\ˆH›Û™NÂˆ]ÝXÚHYœ˜[YNŽ™YÝZNŽ“][UÝXÚ[™›ÈÂˆÝ\Ý[YNˆKŒˆÝ\ÜÜÎˆÙ[\‹ˆÙ[\—ÜÜÎˆÙ[\‹ˆ[WÝÝXÚ\Îˆ‹ˆ›ÛÛWÙ[NˆKŒˆ›ÛÛWÙ[WÌ™ˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽœÜ]
KŒ
Kˆ›Ý][Û—Ù[NˆŒˆ˜[œÛ][Û—Ù[NˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽ–‘T“Ëˆ›Ü˜ÙNˆŒˆNÂ‚ˆ\ÜÙ\Ù\HJˆ˜XÚ×ÝÛ×Ùš[™Ù\—Ý\
ˆ	›]]˜XÚÙ\‹ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÛÛYJÝXÚ
KˆYKˆKŒKˆ
Kˆ›Û™Bˆ
NÂˆ\ÜÙ\Ù\HJˆ˜XÚ×ÝÛ×Ùš[™Ù\—Ý\
ˆ	›]]˜XÚÙ\‹ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
Kˆ›Û™Kˆ˜[ÙKˆKŒ‹ˆ
KˆÛÛYJÙ[\ŠBˆ
NÂˆ\ÜÙ\J˜XÚÙ\‹š\×Û›Û™J
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆÛ×Ùš[™Ù\—Ý\Ü™Z™XÝ×ØWÙ˜Y×ÛÜ—Ü[˜Ú

HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂˆ]Ù[\ˆHYœ˜[YNŽ™YÝZNŽœÜÌŠŒŒMLŒ
NÂˆ]]]˜XÚÙ\ŽˆÜ[ÛÝXÚ\˜XÚÙ\ˆH›Û™NÂˆ]]]ÝXÚHYœ˜[YNŽ™YÝZNŽ“][UÝXÚ[™›ÈÂˆÝ\Ý[YNˆKŒˆÝ\ÜÜÎˆÙ[\‹ˆÙ[\—ÜÜÎˆÙ[\‹ˆ[WÝÝXÚ\Îˆ‹ˆ›ÛÛWÙ[NˆKŒˆ›ÛÛWÙ[WÌ™ˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽœÜ]
KŒ
Kˆ›Ý][Û—Ù[NˆŒˆ˜[œÛ][Û—Ù[NˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽ–‘T“Ëˆ›Ü˜ÙNˆŒˆNÂ‚ˆ˜XÚ×ÝÛ×Ùš[™Ù\—Ý\
ˆ	›]]˜XÚÙ\‹ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÛÛYJÝXÚ
KˆYKˆKŒKˆ
NÂˆÝXÚ˜Ù[\—ÜÜËž
ÏHMKŒÂˆÝXÚž›ÛÛWÙ[HHKŒNÂˆ\ÜÙ\Ù\HJˆ˜XÚ×ÝÛ×Ùš[™Ù\—Ý\
ˆ	›]]˜XÚÙ\‹ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÛÛYJÝXÚ
KˆYKˆKŒKˆ
Kˆ›Û™Bˆ
NÂˆ\ÜÙ\Ù\HJˆ˜XÚ×ÝÛ×Ùš[™Ù\—Ý\
ˆ	›]]˜XÚÙ\‹ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
Kˆ›Û™Kˆ˜[ÙKˆKŒ‹ˆ
Kˆ›Û™Bˆ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆš]Þ›ÛÛWÜÝ^\×ÝÚ][—ÙY]Ü—Þ›ÛÛWÛ[Z]Ê
HÂˆ\ÜÙ\J
›ÛÛWÝ×Ùš]
ŒŒŒŒ
HHŒ
K˜XœÊ
HŒÌŽŽ‘TÒSÓŠNÂˆ\ÜÙ\J
›ÛÛWÝ×Ùš]
LŒLÌŒŒŒ
HHŒ
K˜XœÊ
HŒÌŽŽ‘TÒSÓŠNÂˆ\ÜÙ\J
›ÛÛWÝ×Ùš]
ŒŒWÌŒŒŒ
HHŒŠK˜XœÊ
HŒÌŽŽ‘TÒSÓŠNÂˆB‚ˆÖÝ\ÝBˆ›ˆÝXÚÜ[˜ÚÞ›ÛÛWÚÙY\×ÝWÝ[Y[[™WÜÚ[Ý[™\—Ùš[™Ù\œÊ
HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂˆ]
›ÛÛKÙ™œÙ]
HHÝXÚÜ[—Þ›ÛÛWÜØÜ›ÛÛÙ™œÙ]
ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒŒÌŒ
KˆÝXÚÙ\Ý\™HÂˆÙ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠÌŒMLŒ
Kˆ˜[œÛ][ÛŽˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽ–‘T“Ëˆ›ÛÛWÙ˜XÝÜŽˆKKˆ›Ý][Û—Ù[NˆŒˆKˆÝXÚ›ÛÛPÛÛ™šYÈÂˆÜšYÚ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÝ\œ™[ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒLKŒ
KˆZ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒJKˆX^ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹Œ
KˆÜš^›Û[ˆYKˆ™\XØ[ˆYKˆKˆ
Bˆ™^XÝ
œ[˜ÚÚÝ[\]HHšY]ÈŠNÂ‚ˆ\ÜÙ\J
›ÛÛKžHŒMJK˜XœÊ
HŒÌŽŽ‘TÒSÓŠNÂˆ\ÜÙ\Ù\HJ›ÛÛKžKKŒ
NÂˆ\ÜÙ\J
Ù™œÙ]žHÍ‹Œ
K˜XœÊ
HŒJNÂˆ\ÜÙ\Ù\HJÙ™œÙ]žKÌŒ
NÂˆB‚ˆÖÝ\ÝBˆ›ˆÝXÚÜ[—Û[Ý™\×ÝWÝšY]×Ø[™ØÛ[\×Ø]ÝWÛÜšYÚ[Š
HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂˆ]
›ÛÛKÙ™œÙ]
HHÝXÚÜ[—Þ›ÛÛWÜØÜ›ÛÛÙ™œÙ]
ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ™XÌŠL‹ŒÌŒ
KˆÝXÚÙ\Ý\™HÂˆÙ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠÌŒMLŒ
Kˆ˜[œÛ][ÛŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠKŒLL‹Œ
Kˆ›ÛÛWÙ˜XÝÜŽˆKŒˆ›Ý][Û—Ù[NˆŒˆKˆÝXÚ›ÛÛPÛÛ™šYÈÂˆÜšYÚ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠMÎŒÍŒ
KˆÝ\œ™[ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒLKŒ
KˆZ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒJKˆX^ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹Œ
KˆÜš^›Û[ˆYKˆ™\XØ[ˆYKˆKˆ
Bˆ™^XÝ
œ[ˆÚÝ[\]HHšY]ÈŠNÂ‚ˆ\ÜÙ\Ù\HJ›ÛÛKYœ˜[YNŽ™YÝZNŽ™XÌŠŒLKŒ
JNÂˆ\ÜÙ\Ù\HJÙ™œÙ]Yœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹Œ
JNÂˆB‚ˆÖÝ\ÝBˆ›ˆÝXÚÜ›Ý][Û—Ý™\XØ[WÞ›ÛÛ\×Ø\›Ý[™ÝWÝÝXÚÜÚ[

HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂˆ]Ù[\ˆHYœ˜[YNŽ™YÝZNŽœÜÌŠÌŒMLŒ
NÂˆ]Ý\œ™[ÛÙ™œÙ]HYœ˜[YNŽ™YÝZNŽ™XÌŠŒLŒ
NÂˆ]Ý\œ™[ÜØØ[HHYœ˜[YNŽ™YÝZNŽ™XÌŠŒLKŒ
NÂˆ]
™]×ÜØØ[K™]×ÛÙ™œÙ]
HHÝXÚÜ[—Þ›ÛÛWÜØÜ›ÛÛÙ™œÙ]
ˆšY]ÜÜˆÝ\œ™[ÛÙ™œÙ]ˆÝXÚÙ\Ý\™HÂˆÙ[\‹ˆ˜[œÛ][ÛŽˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽ–‘T“Ëˆ›ÛÛWÙ˜XÝÜŽˆKŒˆ›Ý][Û—Ù[NˆÝŽ™ŒÌŽŽ˜ÛÛœÝÎŽ‘”P×ÔWÌ‹ˆKˆÝXÚ›ÛÛPÛÛ™šYÈÂˆÜšYÚ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÝ\œ™[ˆÝ\œ™[ÜØØ[KˆZ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹JKˆX^ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹Œ
KˆÜš^›Û[ˆYKˆ™\XØ[ˆYKˆKˆ
Bˆ™^XÝ
œ›Ý][ÛˆÚÝ[\]H™\XØ[›ÛÛHŠNÂˆ]šY]ÜÜÞHHÙ[\‹žHHšY]ÜÜÜ

NÂˆ]ÜšYÚ[—ÞHHPS“×Ô“ÓÔ•ST—ÒRQÒÂˆ]™]š[Ý\×Ü]ÚØ]ØÙ[\ˆH
Ý\œ™[ÛÙ™œÙ]žH
ÈšY]ÜÜÞHHÜšYÚ[—ÞJHÈÝ\œ™[ÜØØ[KžNÂˆ]™^Ü]ÚØ]ØÙ[\ˆH
™]×ÛÙ™œÙ]žH
ÈšY]ÜÜÞHHÜšYÚ[—ÞJHÈ™]×ÜØØ[KžNÂ‚ˆ\ÜÙ\J™]×ÜØØ[KžHˆKÈ	‰ˆ™]×ÜØØ[KžHKŽJNÂˆ\ÜÙ\J
™]š[Ý\×Ü]ÚØ]ØÙ[\ˆH™^Ü]ÚØ]ØÙ[\ŠK˜XœÊ
HŒJNÂˆB‚ˆÖÝ\ÝBˆ›ˆÝXÚÙÙ\Ý\™\×ÛÝ]ÚYWÝWÙY]Ü—ÝšY]ÜÜØ\™WÚYÛ›Ü™Y

HÂˆ]šY]ÜÜHYœ˜[YNŽ™YÝZNŽ”™XÝŽ™œ›ÛWÛZ[—ÜÚ^™JˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒLŒ
KˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒÌŒ
Kˆ
NÂ‚ˆ\ÜÙ\JˆÝXÚÜ[—Þ›ÛÛWÜØÜ›ÛÛÙ™œÙ]
ˆšY]ÜÜˆYœ˜[YNŽ™YÝZNŽ•™XÌŽŽ–‘T“ËˆÝXÚÙ\Ý\™HÂˆÙ[\ŽˆYœ˜[YNŽ™YÝZNŽœÜÌŠLŒMLŒ
Kˆ˜[œÛ][ÛŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠLŒLŒ
Kˆ›ÛÛWÙ˜XÝÜŽˆKKˆ›Ý][Û—Ù[NˆŒˆKˆÝXÚ›ÛÛPÛÛ™šYÈÂˆÜšYÚ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŽŒPS“×Ô“ÓÔ•ST—ÒRQÒ
KˆÝ\œ™[ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒLKŒ
KˆZ[ŽˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒJKˆX^ˆYœ˜[YNŽ™YÝZNŽ™XÌŠŒ‹Œ
KˆÜš^›Û[ˆYKˆ™\XØ[ˆYKˆKˆ
Bˆš\×Û›Û™J
Bˆ
NÂˆBŸB
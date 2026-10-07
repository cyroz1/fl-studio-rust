use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, FontId, Id, Sense, Stroke, Vec2};
use flp_rebuild::midi::MidiFile;
use flp_rebuild::plugins::{PluginCandidate, PluginFormat, scan_installed_plugins};
use flp_rebuild::vst3::Vst3HostRuntime;
use flp_rebuild::{
    FlpDocument, Pattern, PatternNote, PatternNoteEdit, PlaylistClip, PlaylistClipEdit,
    PlaylistTrack, VstPluginStateMetadata,
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
enum NoteDragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveNoteDrag {
    pattern_id: u16,
    channel_id: u16,
    channel_note_index: usize,
    start_position: u32,
    start_length: u32,
    start_key: u16,
    kind: NoteDragKind,
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

impl MainView {
    const ALL: [Self; 5] = [
        Self::Playlist,
        Self::ChannelRack,
        Self::PianoRoll,
        Self::Mixer,
        Self::Plugins,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Playlist => "Playlist",
            Self::ChannelRack => "Channel Rack",
            Self::PianoRoll => "Piano roll",
            Self::Mixer => "Mixer",
            Self::Plugins => "Plug-ins",
        }
    }
}

struct DawUi {
    document: Option<FlpDocument>,
    current_path: Option<PathBuf>,
    view: MainView,
    status: String,
    dirty: bool,
    playing: bool,
    tempo_bpm: f64,
    selected_pattern: Option<u16>,
    selected_note_channel: Option<u16>,
    selected_note: Option<(u16, u16, usize)>,
    active_note_drag: Option<ActiveNoteDrag>,
    piano_roll_snap: PianoRollSnap,
    pending_midi_import: Option<PendingMidiImport>,
    selected_arrangement: Option<u16>,
    selected_clip: Option<usize>,
    selected_plugin_state_channel: Option<u16>,
    last_plugin_action: Option<String>,
    timeline_zoom: f32,
    plugin_candidates: Vec<PluginCandidate>,
    vst3_host: Option<Vst3HostRuntime>,
    channel_vst3_instances: BTreeMap<u16, u64>,
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
        let mut app = Self {
            document: None,
            current_path: None,
            view: MainView::Playlist,
            status: "Open an FL Studio project to begin".to_owned(),
            dirty: false,
            playing: false,
            tempo_bpm: 140.0,
            selected_pattern: None,
            selected_note_channel: None,
            selected_note: None,
            active_note_drag: None,
            piano_roll_snap: PianoRollSnap::QuarterBeat,
            pending_midi_import: None,
            selected_arrangement: None,
            selected_clip: None,
            selected_plugin_state_channel: None,
            last_plugin_action: None,
            timeline_zoom: 0.10,
            plugin_candidates,
            vst3_host: None,
            channel_vst3_instances: BTreeMap::new(),
        };
        if let Some(path) = initial_project.as_deref() {
            app.open_project(path);
        }
        app
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open FL Studio project")
            .add_filter("FL Studio project", &["flp"])
            .pick_file()
        {
            self.open_project(&path);
        }
    }

    fn open_project(&mut self, path: &Path) {
        match fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| FlpDocument::parse(&bytes).map_err(|error| error.to_string()))
        {
            Ok(document) => {
                self.tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
                self.selected_pattern = document
                    .patterns()
                    .ok()
                    .and_then(|patterns| patterns.first().map(|pattern| pattern.id));
                self.selected_note_channel =
                    document.channels().first().map(|channel| channel.id());
                self.selected_clip = None;
                self.selected_note = None;
                self.active_note_drag = None;
                self.pending_midi_import = None;
                self.selected_plugin_state_channel = document
                    .channel_plugin_states()
                    .first()
                    .map(|state| state.channel_id());
                self.last_plugin_action = None;
                self.channel_vst3_instances.clear();
                self.selected_arrangement = document.arrangements().ok().and_then(|arrangements| {
                    arrangements.first().map(|arrangement| arrangement.id)
                });
                self.current_path = Some(path.to_path_buf());
                self.document = Some(document);
                self.dirty = false;
                self.status = format!("Opened {}", path.display());
            }
            Err(error) => self.status = format!("Could not open project: {error}"),
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
            .add_filter("FL Studio project", &["flp"])
            .save_file()
        {
            self.write_project(&path);
        }
    }

    fn write_project(&mut self, path: &Path) {
        let Some(document) = &self.document else {
            self.status = "Open a project before saving".to_owned();
            return;
        };
        match document
            .encode_lossless()
            .map_err(|error| error.to_string())
            .and_then(|bytes| fs::write(path, bytes).map_err(|error| error.to_string()))
        {
            Ok(()) => {
                self.current_path = Some(path.to_path_buf());
                self.dirty = false;
                self.status = format!("Saved {}", path.display());
            }
            Err(error) => self.status = format!("Could not save project: {error}"),
        }
    }

    fn update_tempo(&mut self, bpm: f64) {
        if !bpm.is_finite() || bpm <= 0.0 {
            return;
        }
        if let Some(document) = &mut self.document {
            let milli_bpm = (bpm * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32;
            if document.set_tempo_milli_bpm(milli_bpm).is_ok() {
                self.dirty = true;
                self.status = format!("Tempo set to {:.3} BPM", f64::from(milli_bpm) / 1000.0);
            }
        }
    }

    fn top_menu(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.strong("FL")
                .on_hover_text("Independent FL Studio project editor");
            ui.separator();
            if ui.small_button("File").clicked() {
                self.open_dialog();
            }
            if ui.small_button("Open").clicked() {
                self.open_dialog();
            }
            if ui.small_button("Save").clicked() {
                self.save();
            }
            if ui.small_button("Save as").clicked() {
                self.save_as();
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

    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.add_space(4.0);
            if ui.button("●").on_hover_text("Record").clicked() {
                self.status = "Recording is not implemented yet".to_owned();
            }
            if ui.button("■").on_hover_text("Stop").clicked() {
                self.playing = false;
            }
            let play_label = if self.playing { "❚❚" } else { "▶" };
            if ui.button(play_label).on_hover_text("Play").clicked() {
                self.playing = !self.playing;
                if self.playing {
                    self.status = "Audio playback is not implemented yet".to_owned();
                }
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
            ui.monospace("1:01:000");
            ui.separator();
            for label in ["Playlist", "Channel Rack", "Piano roll", "Mixer"] {
                if ui.small_button(label).clicked() {
                    self.view = match label {
                        "Channel Rack" => MainView::ChannelRack,
                        "Piano roll" => MainView::PianoRoll,
                        "Mixer" => MainView::Mixer,
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

    fn browser(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Browser");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label("⋮");
            });
        });
        ui.separator();
        for entry in [
            "Current project",
            "Plugin database",
            "Plugin scan",
            "Packs",
            "Recent files",
        ] {
            ui.label(format!("▸ {entry}"));
        }
        ui.separator();
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
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.separator();
            ui.small("Project data stays in its original FLP event stream");
        });
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

        ui.horizontal(|ui| {
            ui.label("Arrangement");
            ui.strong(arrangement.name.as_deref().unwrap_or("Arrangement"));
            ui.separator();
            ui.label(format!("{} clips", arrangement.clips.len()));
            ui.separator();
            ui.label(format!("{} PPQ", ppq));
        });

        let max_tick = arrangement
            .clips
            .iter()
            .map(|clip| clip.position_ticks.saturating_add(clip.length_ticks))
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
                            if response.clicked() {
                                self.selected_arrangement = Some(arrangement.id);
                                self.selected_clip = Some(clip_index);
                                self.status = format!("Selected Playlist clip {}", clip_index + 1);
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
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("Clip {}", index + 1));
            ui.label("Start");
            let start_changed = ui
                .add(egui::DragValue::new(&mut position).speed(1.0))
                .changed();
            ui.label("Length");
            let length_changed = ui
                .add(egui::DragValue::new(&mut length).speed(1.0))
                .changed();
            if (start_changed || length_changed)
                && let Some(document) = &mut self.document
            {
                let edit = PlaylistClipEdit {
                    position_ticks: start_changed.then_some(position),
                    length_ticks: length_changed.then_some(length),
                    ..PlaylistClipEdit::default()
                };
                if document
                    .edit_playlist_clip(arrangement_id, index, edit)
                    .is_ok()
                {
                    self.dirty = true;
                    self.status = "Playlist clip updated".to_owned();
                }
            }
            if ui.button("Deselect").clicked() {
                self.selected_clip = None;
            }
        });
    }

    fn channel_rack(&mut self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            empty_view(ui, "Open a project to see its Channel Rack");
            return;
        };
        let plugin_states = document.channel_plugin_states();
        let mut open_editor = None;
        ui.horizontal(|ui| {
            ui.strong("Channel Rack");
            ui.separator();
            ui.label("All");
            ui.label("Audio");
            ui.label("Automation");
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for channel in document.channels() {
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
                            ui.add_sized(
                                [190.0, 24.0],
                                egui::Button::new(
                                    channel.display_name().unwrap_or("(unnamed channel)"),
                                )
                                .fill(PANEL_LIGHT),
                            );
                            ui.label(
                                plugin_state
                                    .and_then(|state| state.vst_metadata())
                                    .and_then(VstPluginStateMetadata::name)
                                    .or(channel.plugin_identifier())
                                    .unwrap_or("Audio"),
                            );
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
                    });
                ui.add_space(2.0);
            }
            ui.separator();
            ui.label(
                egui::RichText::new("Step data and plug-in editors are not connected yet")
                    .color(MUTED),
            );
        });
        if let Some(instance_id) = open_editor
            && let Some(host) = &mut self.vst3_host
        {
            match host.open_editor(instance_id) {
                Ok(()) => self.status = "VST3 editor opened from Channel Rack".to_owned(),
                Err(error) => self.status = format!("Could not open VST3 editor: {error}"),
            }
        }
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
        let mut add_note_requested = false;
        let mut open_midi_requested = false;
        ui.horizontal(|ui| {
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
            add_note_requested = ui
                .add_enabled(
                    self.selected_note_channel.is_some(),
                    egui::Button::new("Add note"),
                )
                .clicked();
            open_midi_requested = ui.button("Open MIDI…").clicked();
        });

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

    fn draw_notes(&mut self, ui: &mut egui::Ui, pattern: &Pattern, ppq: u16, snap_ticks: u32) {
        let key_low = 36u16;
        let key_high = 83u16;
        let key_height = 13.0;
        let keyboard_width = 68.0;
        let tick_scale = (self.timeline_zoom * 0.9).clamp(0.05, 0.22);
        let max_tick = pattern
            .notes
            .iter()
            .map(|note| note.position.saturating_add(note.length))
            .max()
            .unwrap_or(ppq as u32 * 16)
            .max(ppq as u32 * 16);
        let grid_width = (max_tick as f32 * tick_scale + 160.0).clamp(1400.0, 30000.0);
        let grid_height = f32::from(key_high - key_low + 1) * key_height;
        let mut note_to_add = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let size = Vec2::new(keyboard_width + grid_width, grid_height);
                let (rect, grid_response) = ui.allocate_exact_size(size, Sense::click());
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
                for (note_index, note) in pattern.notes.iter().enumerate() {
                    let channel_index = per_channel.entry(note.channel_id).or_default();
                    let left = rect.left() + keyboard_width + note.position as f32 * tick_scale;
                    let y = rect.top()
                        + f32::from(key_high.saturating_sub(note.key)) * key_height
                        + 1.0;
                    let note_rect = egui::Rect::from_min_size(
                        egui::pos2(left, y),
                        Vec2::new((note.length as f32 * tick_scale).max(4.0), key_height - 2.0),
                    );
                    note_rects.push(note_rect);
                    let selected =
                        self.selected_note == Some((pattern.id, note.channel_id, *channel_index));
                    painter.rect_filled(
                        note_rect,
                        egui::CornerRadius::same(2),
                        if selected {
                            GREEN.gamma_multiply(1.3)
                        } else {
                            GREEN
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
                        Sense::click_and_drag(),
                    );
                    if response.clicked() {
                        self.selected_note = Some((pattern.id, note.channel_id, *channel_index));
                        self.selected_note_channel = Some(note.channel_id);
                        self.status = format!("Selected note {}", note_index + 1);
                    }
                    if response.is_pointer_button_down_on() && self.active_note_drag.is_none() {
                        let resize = response
                            .interact_pointer_pos()
                            .is_some_and(|pointer| pointer.x >= note_rect.right() - 6.0);
                        self.active_note_drag = Some(ActiveNoteDrag {
                            pattern_id: pattern.id,
                            channel_id: note.channel_id,
                            channel_note_index: *channel_index,
                            start_position: note.position,
                            start_length: note.length,
                            start_key: note.key,
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
                        && let Some(drag) = self.active_note_drag.filter(|drag| {
                            drag.pattern_id == pattern.id
                                && drag.channel_id == note.channel_id
                                && drag.channel_note_index == *channel_index
                        })
                    {
                        let delta = response.drag_delta();
                        let tick_delta = (delta.x / tick_scale).round() as i64;
                        let edit = match drag.kind {
                            NoteDragKind::Move => {
                                let position = snap_note_tick(
                                    i64::from(drag.start_position).saturating_add(tick_delta),
                                    snap_ticks,
                                    0,
                                );
                                let semitones = (-delta.y / key_height).round() as i32;
                                let key = i32::from(drag.start_key)
                                    .saturating_add(semitones)
                                    .clamp(i32::from(key_low), i32::from(key_high))
                                    as u16;
                                PatternNoteEdit {
                                    position: Some(position),
                                    key: Some(key),
                                    ..PatternNoteEdit::default()
                                }
                            }
                            NoteDragKind::Resize => PatternNoteEdit {
                                length: Some(snap_note_tick(
                                    i64::from(drag.start_length).saturating_add(tick_delta),
                                    snap_ticks,
                                    1,
                                )),
                                ..PatternNoteEdit::default()
                            },
                        };
                        if let Some(document) = &mut self.document
                            && document
                                .edit_pattern_note(
                                    drag.pattern_id,
                                    drag.channel_id,
                                    drag.channel_note_index,
                                    edit,
                                )
                                .is_ok()
                        {
                            self.dirty = true;
                            self.status = match drag.kind {
                                NoteDragKind::Move => "Piano roll note moved".to_owned(),
                                NoteDragKind::Resize => "Piano roll note length changed".to_owned(),
                            };
                        }
                    }
                    if response.drag_stopped()
                        && self.active_note_drag.is_some_and(|drag| {
                            drag.pattern_id == pattern.id
                                && drag.channel_id == note.channel_id
                                && drag.channel_note_index == *channel_index
                        })
                    {
                        self.active_note_drag = None;
                    }
                    *channel_index += 1;
                }
                if grid_response.double_clicked()
                    && let (Some(pointer), Some(channel_id)) = (
                        grid_response.interact_pointer_pos(),
                        self.selected_note_channel,
                    )
                    && !note_rects
                        .iter()
                        .any(|note_rect| note_rect.contains(pointer))
                {
                    note_to_add = note_from_grid_position(pointer, grid_geometry, channel_id);
                }
            });
        if !ui.input(|input| input.pointer.primary_down()) {
            self.active_note_drag = None;
        }
        if let Some(note) = note_to_add {
            let note_index = pattern
                .notes
                .iter()
                .filter(|existing| existing.channel_id == note.channel_id)
                .count();
            let channel_id = note.channel_id;
            if let Some(document) = &mut self.document {
                match document.add_pattern_note(pattern.id, note) {
                    Ok(()) => {
                        self.selected_note = Some((pattern.id, channel_id, note_index));
                        self.selected_note_channel = Some(channel_id);
                        self.dirty = true;
                        self.status = format!("Added note to pattern {}", pattern.id);
                    }
                    Err(error) => self.status = error.to_string(),
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
        let mut position = note.position;
        let mut length = note.length;
        let mut key = note.key;
        let mut velocity = note.velocity;
        let mut delete_requested = false;
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Note");
            let position_changed = ui
                .add(
                    egui::DragValue::new(&mut position)
                        .prefix("Start ")
                        .speed(1.0),
                )
                .changed();
            let length_changed = ui
                .add(
                    egui::DragValue::new(&mut length)
                        .prefix("Length ")
                        .speed(1.0),
                )
                .changed();
            let key_changed = ui
                .add(egui::DragValue::new(&mut key).prefix("Key ").speed(0.1))
                .changed();
            let velocity_changed = ui
                .add(
                    egui::DragValue::new(&mut velocity)
                        .prefix("Velocity ")
                        .speed(0.1),
                )
                .changed();
            delete_requested = ui.button("Delete note").clicked();
            if !delete_requested
                && (position_changed || length_changed || key_changed || velocity_changed)
            {
                let edit = PatternNoteEdit {
                    position: position_changed.then_some(position),
                    length: length_changed.then_some(length),
                    key: key_changed.then_some(key),
                    velocity: velocity_changed.then_some(velocity),
                    ..PatternNoteEdit::default()
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
            if ui.button("Deselect").clicked() {
                self.selected_note = None;
            }
        });
        if delete_requested && let Some(document) = &mut self.document {
            match document.delete_pattern_note(pattern_id, channel_id, channel_note_index) {
                Ok(()) => {
                    self.selected_note = None;
                    self.dirty = true;
                    self.status = format!("Deleted note from pattern {pattern_id}");
                }
                Err(error) => self.status = error.to_string(),
            }
        }
    }

    fn mixer(&self, ui: &mut egui::Ui) {
        if self.document.is_none() {
            empty_view(ui, "Open a project to see the Mixer");
            return;
        }
        ui.horizontal(|ui| {
            ui.strong("Mixer");
            ui.separator();
            ui.label("Master");
        });
        ui.separator();
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Mixer data is not decoded yet");
                ui.label(
                    egui::RichText::new(
                        "Insert tracks, routing, effect slots, and meters will appear here",
                    )
                    .color(MUTED),
                );
            });
        });
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
                "Plug-ins load only when you open them. Their own editor windows are used.",
            )
            .color(MUTED),
        );
        ui.separator();

        if refresh {
            self.plugin_candidates = scan_installed_plugins().candidates;
        }

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
                if let Some(candidate) = matching_vst3_candidate(&self.plugin_candidates, metadata)
                {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Installed match: {} · {}",
                            candidate.name,
                            candidate.path.display()
                        ));
                        if ui.button("Load match + try FLP state").clicked() {
                            restore_request = Some((
                                candidate.path.clone(),
                                state.channel_id(),
                                metadata.class_uid(),
                            ));
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
                "Or choose an installed VST3 below to try the selected FLP channel's raw 0xD5 state.",
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
                                    if ui.button("Load and open editor").clicked() {
                                        load_path = Some(candidate.path.clone());
                                    }
                                    if let Some(channel_id) = self.selected_plugin_state_channel
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

        let loaded = self
            .vst3_host
            .as_ref()
            .map(Vst3HostRuntime::loaded_plugins)
            .unwrap_or_default();
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
        let state_payload = restore_channel_id.and_then(|channel_id| {
            self.document
                .as_ref()?
                .channel_plugin_states()
                .into_iter()
                .find(|state| state.channel_id() == channel_id)
                .map(|state| state.data_payload().to_vec())
        });
        if restore_channel_id.is_some() && state_payload.is_none() {
            self.status = "Selected channel has no FLP plug-in state".to_owned();
            self.last_plugin_action = Some(self.status.clone());
            return;
        }
        if self.vst3_host.is_none() {
            match Vst3HostRuntime::new(44_100.0, 512) {
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
                let state_message = if let (Some(channel_id), Some(payload)) =
                    (restore_channel_id, state_payload.as_deref())
                {
                    match host.restore_state(info.id, payload) {
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
        if let Some(host) = &mut self.vst3_host
            && let Err(error) = host.service_editors()
        {
            self.status = format!("VST3 editor update failed: {error}");
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
    }
}

fn empty_view(ui: &mut egui::Ui, message: &str) {
    ui.centered_and_justified(|ui| {
        ui.label(egui::RichText::new(message).color(MUTED));
    });
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
    use super::{PianoRollGrid, PianoRollSnap, note_from_grid_position, snap_note_tick};

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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, Color32, FontId, Id, Sense, Stroke, Vec2};
use flp_rebuild::plugins::{PluginCandidate, PluginFormat, scan_installed_plugins};
use flp_rebuild::vst3::Vst3HostRuntime;
use flp_rebuild::{
    FlpDocument, Pattern, PatternNoteEdit, PlaylistClip, PlaylistClipEdit, PlaylistTrack,
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

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FL Studio Rebuild")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FL Studio Rebuild",
        options,
        Box::new(|creation| Ok(Box::new(DawUi::new(creation)))),
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
    selected_note: Option<(u16, u16, usize)>,
    selected_arrangement: Option<u16>,
    selected_clip: Option<usize>,
    timeline_zoom: f32,
    plugin_candidates: Vec<PluginCandidate>,
    vst3_host: Option<Vst3HostRuntime>,
}

impl DawUi {
    fn new(creation: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = PANEL_DARK;
        visuals.faint_bg_color = PANEL_LIGHT;
        visuals.override_text_color = Some(TEXT);
        creation.egui_ctx.set_visuals(visuals);
        let plugin_candidates = scan_installed_plugins().candidates;
        Self {
            document: None,
            current_path: None,
            view: MainView::Playlist,
            status: "Open an FL Studio project to begin".to_owned(),
            dirty: false,
            playing: false,
            tempo_bpm: 140.0,
            selected_pattern: None,
            selected_note: None,
            selected_arrangement: None,
            selected_clip: None,
            timeline_zoom: 0.10,
            plugin_candidates,
            vst3_host: None,
        }
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
                self.selected_clip = None;
                self.selected_note = None;
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

    fn channel_rack(&self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            empty_view(ui, "Open a project to see its Channel Rack");
            return;
        };
        let plugin_states = document.channel_plugin_states();
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
                            ui.add_sized(
                                [190.0, 24.0],
                                egui::Button::new(
                                    channel.display_name().unwrap_or("(unnamed channel)"),
                                )
                                .fill(PANEL_LIGHT),
                            );
                            ui.label(channel.plugin_identifier().unwrap_or("Audio"));
                            ui.separator();
                            let state_bytes = plugin_states
                                .iter()
                                .find(|state| state.channel_id() == channel.id())
                                .map_or(0, |state| state.data_payload().len());
                            if state_bytes > 0 {
                                ui.small(format!("state {state_bytes} B"));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.add_enabled(false, egui::Button::new("Plug-in editor"));
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
    }

    fn piano_roll(&mut self, ui: &mut egui::Ui) {
        let Some(document) = &self.document else {
            empty_view(ui, "Open a project to see its Piano roll");
            return;
        };
        let patterns = document.patterns().unwrap_or_default();
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
            ui.label("Snap");
            ui.label("1/4 beat");
        });
        ui.separator();
        let Some(pattern) = patterns
            .iter()
            .find(|pattern| Some(pattern.id) == self.selected_pattern)
            .cloned()
        else {
            return;
        };
        self.draw_notes(ui, &pattern, document.header().ppq().max(1));
        self.selected_note_editor(ui, &pattern);
    }

    fn draw_notes(&mut self, ui: &mut egui::Ui, pattern: &Pattern, ppq: u16) {
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
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let size = Vec2::new(keyboard_width + grid_width, grid_height);
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
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
                    let response = ui.interact(
                        note_rect,
                        Id::new(("piano-note", pattern.id, note.channel_id, *channel_index)),
                        Sense::click(),
                    );
                    if response.clicked() {
                        self.selected_note = Some((pattern.id, note.channel_id, *channel_index));
                        self.status = format!("Selected note {}", note_index + 1);
                    }
                    *channel_index += 1;
                }
            });
    }

    fn selected_note_editor(&mut self, ui: &mut egui::Ui, pattern: &Pattern) {
        let Some((pattern_id, channel_id, channel_note_index)) = self.selected_note else {
            return;
        };
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
            if position_changed || length_changed || key_changed || velocity_changed {
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

        egui::ScrollArea::vertical().show(ui, |ui| {
            for candidate in &self.plugin_candidates {
                if candidate.format != PluginFormat::Vst3 {
                    continue;
                }
                egui::Frame::new()
                    .fill(PANEL_DARK)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.strong(&candidate.name);
                                ui.small(candidate.path.display().to_string());
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Load and open editor").clicked() {
                                        load_path = Some(candidate.path.clone());
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(3.0);
            }
        });

        if let Some(path) = load_path {
            self.load_installed_vst3(&path);
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
                Ok(()) => self.status = "VST3 editor opened".to_owned(),
                Err(error) => self.status = format!("Could not open VST3 editor: {error}"),
            }
        }
        if let Some(id) = close_id
            && let Some(host) = &mut self.vst3_host
        {
            match host.close_editor(id) {
                Ok(()) => self.status = "VST3 editor closed".to_owned(),
                Err(error) => self.status = format!("Could not close VST3 editor: {error}"),
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

    fn load_installed_vst3(&mut self, path: &Path) {
        if self.vst3_host.is_none() {
            match Vst3HostRuntime::new(44_100.0, 512) {
                Ok(host) => self.vst3_host = Some(host),
                Err(error) => {
                    self.status = format!("Could not initialize the VST3 host: {error}");
                    return;
                }
            }
        }

        let Some(host) = &mut self.vst3_host else {
            return;
        };
        match host.load(path, None) {
            Ok(info) => match host.open_editor(info.id) {
                Ok(()) => self.status = format!("Loaded {}", info.name),
                Err(error) => {
                    self.status =
                        format!("Loaded {}, but its editor did not open: {error}", info.name);
                }
            },
            Err(error) => self.status = format!("Could not load VST3: {error}"),
        }
    }
}

impl eframe::App for DawUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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

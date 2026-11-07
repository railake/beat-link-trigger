//! The desktop user interface, built with egui. It replaces the Swing windows of
//! the Java version: the Triggers window (with its menus and per-trigger gear
//! menus), expression editors, Player Status, Carabiner, the playback simulator,
//! show windows, the OBS overlay and playlist writer settings, Load Track and Load
//! Settings, the MIDI monitor and the log viewer.

use crate::app::{App, OnlineState};
use crate::config::{ClockStart, EnabledMode, MessageKind, PlayerChoice, Settings, SyncMode, TriggerConfig};
use crate::expressions::{GLOBAL_EXPRESSIONS, TRIGGER_EXPRESSIONS};
use crate::shows::{EventMessage, EventSpec, ShowCue, ShowEnabled, ShowRunner, CUE_EXPRESSIONS, TRACK_EXPRESSIONS};
use beat_link::data::{WaveformPreview};
use beat_link::packets::{TrackSourceSlot, TrackType};
use beat_link::player_settings::{PlayerSettings, SETTINGS};
use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Which expression an editor window is editing.
#[derive(Debug, Clone, PartialEq)]
enum ExprTarget {
    Global(String),
    Trigger { uuid: String, kind: String },
    Track { show: usize, signature: String, kind: String },
    Cue { show: usize, signature: String, uuid: String, kind: String },
}

struct ExprEditor {
    target: ExprTarget,
    title: String,
    help: String,
    text: String,
    legacy: Option<String>,
    open: bool,
    message: Option<(bool, String)>,
}

#[derive(Default)]
struct FileDialog {
    open: bool,
    purpose: FilePurpose,
    path: String,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum FilePurpose {
    #[default]
    Open,
    SaveAs,
    OpenShow,
    NewShow,
    Report,
}

struct LoadTrackState {
    target: u8,
    source_player: u8,
    slot: TrackSourceSlot,
    rekordbox_id: u32,
}

/// The GUI state.
pub struct Gui {
    app: Arc<App>,
    editors: Vec<ExprEditor>,
    file_dialog: FileDialog,
    show_status: bool,
    show_carabiner: bool,
    show_simulator: bool,
    show_overlay: bool,
    show_playlist: bool,
    show_logs: bool,
    show_midi: bool,
    show_about: bool,
    show_load_track: bool,
    show_load_settings: bool,
    shows_open: Vec<bool>,
    midi_outputs: Vec<String>,
    last_midi_scan: std::time::Instant,
    load_track: LoadTrackState,
    player_settings: PlayerSettings,
    settings_target: u8,
    carabiner_host: String,
    new_cue: (i64, i64, String),
    status_message: Option<(std::time::Instant, String)>,
}

fn color_for_hue(hue: f64) -> Color32 {
    let h = (hue.rem_euclid(360.0)) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as i32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    Color32::from_rgb((r * 200.0) as u8 + 30, (g * 200.0) as u8 + 30, (b * 200.0) as u8 + 30)
}

impl Gui {
    pub fn new(app: Arc<App>) -> Gui {
        let midi_outputs = app.outputs.midi.list_outputs();
        let host = app.settings.read().carabiner.host.clone();
        Gui {
            app,
            editors: Vec::new(),
            file_dialog: FileDialog::default(),
            show_status: false,
            show_carabiner: false,
            show_simulator: false,
            show_overlay: false,
            show_playlist: false,
            show_logs: false,
            show_midi: false,
            show_about: false,
            show_load_track: false,
            show_load_settings: false,
            shows_open: Vec::new(),
            midi_outputs,
            last_midi_scan: std::time::Instant::now(),
            load_track: LoadTrackState { target: 1, source_player: 1, slot: TrackSourceSlot::UsbSlot, rekordbox_id: 1 },
            player_settings: PlayerSettings::default(),
            settings_target: 1,
            carabiner_host: host,
            new_cue: (1, 17, String::new()),
            status_message: None,
        }
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.status_message = Some((std::time::Instant::now(), text.into()));
    }

    // ------------------------------------------------------------------ expression editors

    fn open_editor(&mut self, target: ExprTarget) {
        if let Some(e) = self.editors.iter_mut().find(|e| e.target == target) {
            e.open = true;
            return;
        }
        let (title, help, text, legacy) = match &target {
            ExprTarget::Global(kind) => {
                let s = self.app.settings.read();
                let (_, title, help) = GLOBAL_EXPRESSIONS.iter().find(|(k, _, _)| k == kind).copied().unwrap_or(("", "Expression", ""));
                (format!("Triggers {title}"), help.to_string(), s.global_expression(kind).to_string(), s.legacy_expressions.get(kind).cloned())
            }
            ExprTarget::Trigger { uuid, kind } => {
                let views = self.app.triggers.views();
                let Some(v) = views.iter().find(|v| &v.config.uuid == uuid) else { return };
                let (_, title, help) = TRIGGER_EXPRESSIONS.iter().find(|(k, _, _)| k == kind).copied().unwrap_or(("", "Expression", ""));
                (
                    format!("Trigger {} {}", v.index + 1, title),
                    help.to_string(),
                    v.config.expression(kind).to_string(),
                    v.config.legacy_expressions.get(kind).cloned(),
                )
            }
            ExprTarget::Track { show, signature, kind } => {
                let Some(runner) = self.app.shows.read().get(*show).cloned() else { return };
                let s = runner.show.read();
                let Some(t) = s.tracks.get(signature) else { return };
                let title = TRACK_EXPRESSIONS.iter().find(|(k, _)| k == kind).map(|(_, t)| *t).unwrap_or("Expression");
                (format!("{}: {}", t.metadata.title, title), String::new(), t.expressions.get(kind).cloned().unwrap_or_default(), t.legacy_expressions.get(kind).cloned())
            }
            ExprTarget::Cue { show, signature, uuid, kind } => {
                let Some(runner) = self.app.shows.read().get(*show).cloned() else { return };
                let s = runner.show.read();
                let Some(c) = s.tracks.get(signature).and_then(|t| t.cues.iter().find(|c| &c.uuid == uuid)) else { return };
                let title = CUE_EXPRESSIONS.iter().find(|(k, _)| k == kind).map(|(_, t)| *t).unwrap_or("Expression");
                (format!("Cue \"{}\": {}", c.comment, title), String::new(), c.expressions.get(kind).cloned().unwrap_or_default(), c.legacy_expressions.get(kind).cloned())
            }
        };
        self.editors.push(ExprEditor { target, title, help, text, legacy, open: true, message: None });
    }

    fn apply_editor(&mut self, index: usize) {
        let (target, text) = {
            let e = &self.editors[index];
            (e.target.clone(), e.text.clone())
        };
        let check = self.app.expressions.compile(&text, "expression");
        let result: Result<(), String> = match &target {
            ExprTarget::Global(kind) => {
                let mut s = self.app.current_settings();
                if text.trim().is_empty() {
                    s.expressions.remove(kind);
                } else {
                    s.expressions.insert(kind.clone(), text.clone());
                }
                self.app.apply_settings(s);
                Ok(())
            }
            ExprTarget::Trigger { uuid, kind } => {
                let views = self.app.triggers.views();
                match views.iter().find(|v| &v.config.uuid == uuid) {
                    Some(v) => {
                        let mut c = v.config.clone();
                        if text.trim().is_empty() {
                            c.expressions.remove(kind);
                        } else {
                            c.expressions.insert(kind.clone(), text.clone());
                        }
                        self.app.triggers.update(v.index, c);
                        Ok(())
                    }
                    None => Err("That trigger no longer exists.".into()),
                }
            }
            ExprTarget::Track { show, signature, kind } => match self.app.shows.read().get(*show).cloned() {
                Some(r) => {
                    if let Some(t) = r.show.write().tracks.get_mut(signature) {
                        t.expressions.insert(kind.clone(), text.clone());
                    }
                    r.compile_all();
                    Ok(())
                }
                None => Err("That show is no longer open.".into()),
            },
            ExprTarget::Cue { show, signature, uuid, kind } => match self.app.shows.read().get(*show).cloned() {
                Some(r) => {
                    if let Some(c) = r.show.write().tracks.get_mut(signature).and_then(|t| t.cues.iter_mut().find(|c| &c.uuid == uuid)) {
                        c.expressions.insert(kind.clone(), text.clone());
                    }
                    r.compile_all();
                    Ok(())
                }
                None => Err("That show is no longer open.".into()),
            },
        };
        let e = &mut self.editors[index];
        e.message = Some(match (result, check) {
            (Err(msg), _) => (false, msg),
            (Ok(()), Err(err)) => (false, format!("Saved, but it does not compile: {}", err.message)),
            (Ok(()), Ok(_)) => (true, "Applied.".into()),
        });
    }

    fn editor_windows(&mut self, ctx: &egui::Context) {
        let mut apply = None;
        for (i, e) in self.editors.iter_mut().enumerate() {
            let mut open = e.open;
            egui::Window::new(&e.title).id(egui::Id::new(("editor", i))).open(&mut open).default_size([640.0, 420.0]).show(ctx, |ui| {
                if !e.help.is_empty() {
                    ui.label(RichText::new(&e.help).italics());
                }
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = Some(i);
                    }
                    if let Some((ok, msg)) = &e.message {
                        ui.colored_label(if *ok { Color32::LIGHT_GREEN } else { Color32::LIGHT_RED }, msg);
                    }
                });
                egui::ScrollArea::vertical().max_height(320.0).id_salt(("editor-scroll", i)).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut e.text).code_editor().desired_rows(16).desired_width(f32::INFINITY));
                });
                if let Some(legacy) = &e.legacy {
                    egui::CollapsingHeader::new("Original Clojure expression (Java version)").id_salt(("legacy", i)).show(ui, |ui| {
                        let mut l = legacy.clone();
                        ui.add(egui::TextEdit::multiline(&mut l).code_editor().interactive(false).desired_width(f32::INFINITY));
                    });
                }
                egui::CollapsingHeader::new("Available values and functions").id_salt(("help", i)).show(ui, |ui| {
                    ui.label(EXPRESSION_HELP);
                });
            });
            e.open = open;
        }
        if let Some(i) = apply {
            self.apply_editor(i);
        }
        self.editors.retain(|e| e.open);
    }

    // ------------------------------------------------------------------ menus

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New Trigger").clicked() {
                        self.app.triggers.add(TriggerConfig::default());
                        ui.close_menu();
                    }
                    if ui.button("Save").clicked() {
                        match self.app.save(None) {
                            Ok(p) => self.notify(format!("Saved to {}", p.display())),
                            Err(e) => self.notify(format!("Unable to save: {e}")),
                        }
                        ui.close_menu();
                    }
                    if ui.button("Save to File…").clicked() {
                        self.file_dialog = FileDialog { open: true, purpose: FilePurpose::SaveAs, path: String::new() };
                        ui.close_menu();
                    }
                    if ui.button("Load from File… (.bltj or Java .blt)").clicked() {
                        self.file_dialog = FileDialog { open: true, purpose: FilePurpose::Open, path: String::new() };
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("New Show…").clicked() {
                        self.file_dialog = FileDialog { open: true, purpose: FilePurpose::NewShow, path: String::new() };
                        ui.close_menu();
                    }
                    if ui.button("Open Show… (.blsj or Java .bls)").clicked() {
                        self.file_dialog = FileDialog { open: true, purpose: FilePurpose::OpenShow, path: String::new() };
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Write Expression Report…").clicked() {
                        self.file_dialog = FileDialog { open: true, purpose: FilePurpose::Report, path: "expression-report.html".into() };
                        ui.close_menu();
                    }
                    if ui.button("Clear Triggers").clicked() {
                        let mut s = self.app.current_settings();
                        s.triggers = vec![TriggerConfig::default()];
                        self.app.apply_settings(s);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Triggers", |ui| {
                    for (kind, title, _) in GLOBAL_EXPRESSIONS {
                        let has = !self.app.settings.read().global_expression(kind).trim().is_empty();
                        let label = if has { format!("Edit {title} *") } else { format!("Edit {title}") };
                        if ui.button(label).clicked() {
                            self.open_editor(ExprTarget::Global(kind.to_string()));
                            ui.close_menu();
                        }
                    }
                });
                ui.menu_button("Network", |ui| {
                    let state = self.app.online_state();
                    let mut online = state != OnlineState::Offline;
                    if ui.checkbox(&mut online, "Online?").changed() {
                        if online {
                            self.app.go_online();
                        } else {
                            self.app.go_offline();
                        }
                    }
                    let mut real = self.app.settings.read().send_status;
                    if ui.checkbox(&mut real, "Use Real Player Number?").on_hover_text("Pose as player 1-4 and send status; needed to become tempo master and for Full Link sync. Takes effect when next going online.").changed() {
                        self.app.settings.write().send_status = real;
                    }
                    ui.separator();
                    if ui.button("Load Track on Player…").clicked() {
                        self.show_load_track = true;
                        ui.close_menu();
                    }
                    if ui.button("Load Settings on Player…").clicked() {
                        self.show_load_settings = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Window", |ui| {
                    ui.checkbox(&mut self.show_status, "Player Status");
                    ui.checkbox(&mut self.show_carabiner, "Ableton Link: Carabiner Connection");
                    ui.checkbox(&mut self.show_simulator, "Shallow Playback Simulator");
                    ui.checkbox(&mut self.show_overlay, "OBS Overlay Web Server");
                    ui.checkbox(&mut self.show_playlist, "Playlist Writer");
                    ui.checkbox(&mut self.show_midi, "MIDI Monitor");
                    ui.checkbox(&mut self.show_logs, "Logs");
                    let shows = self.app.shows.read().clone();
                    self.shows_open.resize(shows.len(), true);
                    if !shows.is_empty() {
                        ui.separator();
                    }
                    for (i, s) in shows.iter().enumerate() {
                        ui.checkbox(&mut self.shows_open[i], format!("Show: {}", s.title()));
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About Beat Link Trigger").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, color) = match self.app.online_state() {
                        OnlineState::Online => (
                            format!("Online as {} #{}", self.app.link.virtual_cdj.device_name(), self.app.link.virtual_cdj.device_number()),
                            Color32::LIGHT_GREEN,
                        ),
                        OnlineState::Connecting => ("Looking for DJ Link devices…".to_string(), Color32::YELLOW),
                        OnlineState::Offline => ("Offline".to_string(), Color32::GRAY),
                    };
                    ui.colored_label(color, text);
                    if self.app.simulator.is_simulating() {
                        ui.colored_label(Color32::LIGHT_BLUE, "Simulating ·");
                    }
                });
            });
        });
    }

    // ------------------------------------------------------------------ triggers

    fn trigger_rows(&mut self, ctx: &egui::Context) {
        let views = self.app.triggers.views();
        let mut open_editor = None;
        let mut to_remove = None;
        let mut to_add = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for v in &views {
                    let mut c = v.config.clone();
                    let mut changed = false;
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}.", v.index + 1)).strong());
                            changed |= ui.add(egui::TextEdit::singleline(&mut c.comment).hint_text("Comment").desired_width(260.0)).changed();
                            ui.label("Watch:");
                            egui::ComboBox::from_id_salt(("players", &c.uuid)).selected_text(c.players.to_string()).show_ui(ui, |ui| {
                                for p in PlayerChoice::choices() {
                                    changed |= ui.selectable_value(&mut c.players, p, p.to_string()).changed();
                                }
                            });
                            match &v.status_summary {
                                Some(s) => ui.label(RichText::new(s).small()),
                                None => ui.label(RichText::new(if self.app.is_online() || self.app.simulator.is_simulating() { "No Player" } else { "Offline." }).small().color(Color32::LIGHT_RED)),
                            };
                        });
                        ui.horizontal(|ui| {
                            ui.label("MIDI Output:");
                            let selected = c.output.clone().unwrap_or_else(|| "(none)".into());
                            let missing = c.output.as_ref().map(|o| !self.midi_outputs.contains(o)).unwrap_or(false);
                            egui::ComboBox::from_id_salt(("output", &c.uuid))
                                .selected_text(if missing { RichText::new(format!("{selected} (missing)")).color(Color32::LIGHT_RED) } else { RichText::new(selected) })
                                .width(180.0)
                                .show_ui(ui, |ui| {
                                    changed |= ui.selectable_value(&mut c.output, None, "(none)").changed();
                                    for name in &self.midi_outputs {
                                        changed |= ui.selectable_value(&mut c.output, Some(name.clone()), name).changed();
                                    }
                                });
                            ui.label("Message:");
                            egui::ComboBox::from_id_salt(("message", &c.uuid)).selected_text(c.message.label()).show_ui(ui, |ui| {
                                for m in MessageKind::ALL {
                                    changed |= ui.selectable_value(&mut c.message, m, m.label()).changed();
                                }
                            });
                            match c.message {
                                MessageKind::Note | MessageKind::Cc => {
                                    changed |= ui.add(egui::DragValue::new(&mut c.note).range(1..=127)).changed();
                                    ui.label("Channel:");
                                    changed |= ui.add(egui::DragValue::new(&mut c.channel).range(1..=16)).changed();
                                }
                                MessageKind::Clock => {
                                    changed |= ui.checkbox(&mut c.send, "").on_hover_text("Send start/continue when activated").changed();
                                    egui::ComboBox::from_id_salt(("start", &c.uuid)).selected_text(format!("{:?}", c.start)).show_ui(ui, |ui| {
                                        changed |= ui.selectable_value(&mut c.start, ClockStart::Start, "Start").changed();
                                        changed |= ui.selectable_value(&mut c.start, ClockStart::Continue, "Continue").changed();
                                    });
                                    changed |= ui.checkbox(&mut c.stop, "Stop").changed();
                                }
                                MessageKind::Link => {
                                    changed |= ui.checkbox(&mut c.bar, "Align bars").changed();
                                    changed |= ui.checkbox(&mut c.start_stop, "Start/Stop").changed();
                                }
                                MessageKind::Custom => {}
                            }
                            ui.label("Enabled:");
                            egui::ComboBox::from_id_salt(("enabled", &c.uuid)).selected_text(c.enabled.label()).show_ui(ui, |ui| {
                                for m in EnabledMode::ALL {
                                    changed |= ui.selectable_value(&mut c.enabled, m, m.label()).changed();
                                }
                            });
                            // State indicator: outer ring when enabled, filled when tripped.
                            let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                            let center = rect.center();
                            let ring = if v.enabled { Color32::from_rgb(0, 200, 0) } else { Color32::from_rgb(200, 0, 0) };
                            ui.painter().circle_stroke(center, 8.0, Stroke::new(2.0_f32, ring));
                            if v.tripped {
                                ui.painter().circle_filled(center, 5.0, Color32::from_rgb(0, 230, 0));
                            } else if v.playing {
                                ui.painter().circle_stroke(center, 4.0, Stroke::new(1.0_f32, Color32::GRAY));
                            }
                            let has_exprs = c.expressions.values().any(|s| !s.trim().is_empty());
                            ui.menu_button(if has_exprs { "⚙*" } else { "⚙" }, |ui| {
                                for (kind, title, _) in TRIGGER_EXPRESSIONS {
                                    let mark = if c.expression(kind).trim().is_empty() { "" } else { " *" };
                                    let err = if v.errors.contains_key(*kind) { " ⚠" } else { "" };
                                    if ui.button(format!("Edit {title}{mark}{err}")).clicked() {
                                        open_editor = Some(ExprTarget::Trigger { uuid: c.uuid.clone(), kind: kind.to_string() });
                                        ui.close_menu();
                                    }
                                }
                                ui.separator();
                                if ui.button("Simulate Activation").clicked() {
                                    self.app.triggers.simulate_message(v.index, true);
                                    ui.close_menu();
                                }
                                if ui.button("Simulate Deactivation").clicked() {
                                    self.app.triggers.simulate_message(v.index, false);
                                    ui.close_menu();
                                }
                                ui.separator();
                                if ui.button("Duplicate Trigger").clicked() {
                                    to_add = Some(c.clone().with_new_uuid());
                                    ui.close_menu();
                                }
                                if ui.button("Delete Trigger").clicked() {
                                    to_remove = Some(v.index);
                                    ui.close_menu();
                                }
                            });
                        });
                        if !v.errors.is_empty() {
                            for (kind, err) in &v.errors {
                                ui.colored_label(Color32::LIGHT_RED, format!("{kind} expression: {err}"));
                            }
                        }
                    });
                    if changed {
                        self.app.triggers.update(v.index, c);
                    }
                }
                if ui.button("➕ New Trigger").clicked() {
                    to_add = Some(TriggerConfig::default());
                }
            });
        });
        if let Some(t) = open_editor {
            self.open_editor(t);
        }
        if let Some(i) = to_remove {
            self.app.triggers.remove(i);
        }
        if let Some(c) = to_add {
            self.app.triggers.add(c);
        }
    }

    // ------------------------------------------------------------------ player status

    fn paint_preview(ui: &mut egui::Ui, preview: &WaveformPreview, fraction: Option<f64>, cues: Option<&beat_link::data::CueList>, duration_ms: Option<i64>) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), 40.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, Color32::BLACK);
        let segments = preview.segment_count().max(1);
        let max = f32::from(preview.max_height().max(1));
        let width = rect.width().max(1.0) as usize;
        for x in 0..width {
            let seg = x * segments / width;
            let h = f32::from(preview.segment_height(seg)) / max * rect.height();
            let (r, g, b) = preview.segment_color(seg);
            let px = rect.left() + x as f32;
            painter.line_segment([egui::pos2(px, rect.bottom()), egui::pos2(px, rect.bottom() - h)], Stroke::new(1.0_f32, Color32::from_rgb(r, g, b)));
        }
        if let (Some(cues), Some(duration)) = (cues, duration_ms) {
            for cue in &cues.entries {
                if duration > 0 {
                    let x = rect.left() + (cue.cue_time as f32 / duration as f32) * rect.width();
                    let (r, g, b) = cue.color();
                    painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.top() + 6.0)], Stroke::new(2.0_f32, Color32::from_rgb(r, g, b)));
                }
            }
        }
        if let Some(f) = fraction {
            let x = rect.left() + f.clamp(0.0, 1.0) as f32 * rect.width();
            painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(2.0_f32, Color32::RED));
        }
    }

    fn player_status_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_status;
        egui::Window::new("Player Status").open(&mut open).default_pos([930.0, 40.0]).default_width(460.0).show(ctx, |ui| {
            let devices = self.app.devices();
            if devices.is_empty() {
                ui.label("No players found. Go online, or add players in the Shallow Playback Simulator.");
            }
            for d in devices {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Player {}", d.number)).heading());
                        ui.label(&d.name);
                        if let Some(s) = &d.status {
                            if s.is_tempo_master() {
                                ui.colored_label(Color32::from_rgb(255, 160, 0), "MASTER");
                            }
                            if s.is_synced() {
                                ui.colored_label(Color32::LIGHT_BLUE, "SYNC");
                            }
                            if s.is_on_air() {
                                ui.colored_label(Color32::RED, "ON AIR");
                            }
                            ui.label(if s.is_playing() { "▶ Playing" } else { "⏸ Stopped" });
                        }
                    });
                    let md = d.track.as_ref().and_then(|t| t.metadata.clone());
                    match &md {
                        Some(md) => {
                            ui.label(RichText::new(&md.title).strong());
                            ui.label(md.artist_label());
                        }
                        None => {
                            ui.label(RichText::new("No track loaded (or metadata unavailable)").italics());
                        }
                    }
                    if let Some(s) = &d.status {
                        let bpm = if s.has_bpm() { format!("{:.1}", s.effective_tempo()) } else { "--.-".into() };
                        ui.label(format!(
                            "BPM {bpm}   Pitch {}   Beat {} ({}/4)",
                            crate::overlay::format_pitch(beat_link::util::pitch_to_percentage(s.pitch)),
                            s.beat_number_or_negative(),
                            s.beat_within_bar()
                        ));
                    }
                    let duration = d.track.as_ref().and_then(|t| t.detail.as_ref().map(|w| w.total_time()).or_else(|| t.metadata.as_ref().map(|m| i64::from(m.duration) * 1000)));
                    if let Some(pos) = d.position_ms {
                        let remain = duration.map(|t| (t - pos).max(0));
                        ui.label(format!(
                            "Played {}   Remaining {}",
                            beat_link::util::format_track_time(pos),
                            remain.map(beat_link::util::format_track_time).unwrap_or_else(|| "--:--:--.-".into())
                        ));
                    }
                    if let Some(preview) = d.track.as_ref().and_then(|t| t.preview.clone()) {
                        let fraction = match (d.position_ms, duration) {
                            (Some(p), Some(t)) if t > 0 => Some(p as f64 / t as f64),
                            _ => None,
                        };
                        let cues = d.track.as_ref().and_then(|t| t.cue_list.clone());
                        Self::paint_preview(ui, &preview, fraction, cues.as_ref(), duration);
                    }
                    if let Some(sig) = d.track.as_ref().and_then(|t| t.signature.clone()) {
                        ui.label(RichText::new(format!("Signature {sig}")).small().weak());
                    }
                });
            }
        });
        self.show_status = open;
    }

    // ------------------------------------------------------------------ carabiner

    fn carabiner_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_carabiner;
        egui::Window::new("Carabiner Connection").open(&mut open).show(ctx, |ui| {
            let state = self.app.carabiner.state();
            let mut cfg = self.app.settings.read().carabiner.clone();
            ui.horizontal(|ui| {
                ui.label("Host:");
                ui.text_edit_singleline(&mut self.carabiner_host);
                ui.label("Port:");
                ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535));
            });
            ui.horizontal(|ui| {
                if state.connected {
                    if ui.button("Disconnect").clicked() {
                        self.app.carabiner.disconnect();
                    }
                    ui.colored_label(Color32::LIGHT_GREEN, format!("Connected (Carabiner {})", state.version.clone().unwrap_or_default()));
                } else if ui.button("Connect").clicked() {
                    cfg.host = self.carabiner_host.clone();
                    if let Err(e) = self.app.carabiner.connect(&cfg.host, cfg.port) {
                        self.notify(format!("Unable to connect to Carabiner: {e}. Is it running?"));
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Latency (ms):");
                ui.add(egui::DragValue::new(&mut cfg.latency_ms).range(0..=500));
                ui.checkbox(&mut cfg.bars, "Align at bar level");
            });
            ui.horizontal(|ui| {
                ui.label("Sync Mode:");
                let mut mode = state.sync_mode;
                egui::ComboBox::from_id_salt("sync-mode").selected_text(mode.label()).show_ui(ui, |ui| {
                    for m in SyncMode::ALL {
                        ui.selectable_value(&mut mode, m, m.label());
                    }
                });
                if mode != state.sync_mode {
                    match self.app.carabiner.set_sync_mode(mode) {
                        Ok(()) => cfg.sync_mode = mode,
                        Err(e) => self.notify(e),
                    }
                }
            });
            if matches!(state.sync_mode, SyncMode::Passive | SyncMode::Full) {
                let vcdj = &self.app.link.virtual_cdj;
                let mut synced = vcdj.is_synced();
                if ui.checkbox(&mut synced, "Sync Ableton Link to DJ Link tempo").changed() {
                    self.app.carabiner.sync_link(synced);
                }
                if state.sync_mode == SyncMode::Full {
                    let mut master = vcdj.is_tempo_master();
                    if ui.checkbox(&mut master, "Ableton Link is Tempo Master").changed() {
                        self.app.carabiner.link_master(master);
                    }
                }
            }
            ui.separator();
            ui.label(format!(
                "Link tempo: {:.2} BPM   Peers: {}   Target: {}",
                state.link_bpm,
                state.link_peers,
                state.target_bpm.map(|t| format!("{t:.2}")).unwrap_or_else(|| "none".into())
            ));
            if cfg != self.app.settings.read().carabiner {
                self.app.carabiner.configure(&cfg);
                self.app.settings.write().carabiner = cfg;
            }
        });
        self.show_carabiner = open;
    }

    // ------------------------------------------------------------------ simulator

    fn simulator_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_simulator;
        egui::Window::new("Shallow Playback Simulator").open(&mut open).default_pos([930.0, 470.0]).default_width(420.0).show(ctx, |ui| {
            if ui.button("Add Simulated Player").clicked() && self.app.simulator.add_player().is_none() {
                self.notify("All six simulated players are in use.");
            }
            let samples = crate::simulator::sample_tracks();
            for p in self.app.simulator.players() {
                let n = p.number;
                let mut q = p.clone();
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Player {n}")).strong());
                        let title = |i: Option<usize>| i.and_then(|i| samples.get(i)).and_then(|t| t.metadata.as_ref().map(|m| m.title.clone())).unwrap_or_else(|| "No Track".into());
                        egui::ComboBox::from_id_salt(("sim-track", n)).selected_text(title(q.track)).show_ui(ui, |ui| {
                            ui.selectable_value(&mut q.track, None, "No Track");
                            for i in 0..samples.len() {
                                ui.selectable_value(&mut q.track, Some(i), title(Some(i)));
                            }
                        });
                        if ui.button("Remove").clicked() {
                            self.app.simulator.remove_player(n);
                        }
                    });
                    ui.horizontal(|ui| {
                        let label = if q.playing { "⏸ Pause" } else { "▶ Play" };
                        ui.toggle_value(&mut q.playing, label);
                        ui.checkbox(&mut q.on_air, "On-Air");
                        ui.checkbox(&mut q.master, "Master");
                        ui.checkbox(&mut q.synced, "Sync");
                    });
                    ui.add(egui::Slider::new(&mut q.pitch_percent, -16.0..=16.0).text("Pitch %"));
                    if let Some(t) = q.track_data() {
                        let duration = t.metadata.as_ref().map(|m| f64::from(m.duration) * 1000.0).unwrap_or(1.0);
                        ui.add(egui::Slider::new(&mut q.position_ms, 0.0..=duration).text("Position ms"));
                        if let Some(preview) = &t.preview {
                            Self::paint_preview(ui, preview, Some(q.position_ms / duration), t.cue_list.as_ref(), Some(duration as i64));
                        }
                    }
                });
                if q.track != p.track || q.playing != p.playing || q.on_air != p.on_air || q.master != p.master || q.synced != p.synced || q.pitch_percent != p.pitch_percent || (q.position_ms - p.position_ms).abs() > 50.0 {
                    let track_changed = q.track != p.track;
                    self.app.simulator.update(n, |s| {
                        s.track = q.track;
                        s.playing = q.playing;
                        s.on_air = q.on_air;
                        s.master = q.master;
                        s.synced = q.synced;
                        s.pitch_percent = q.pitch_percent;
                        s.position_ms = if track_changed { 0.0 } else { q.position_ms };
                    });
                }
            }
        });
        self.show_simulator = open;
    }

    // ------------------------------------------------------------------ overlay / playlist

    fn overlay_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_overlay;
        egui::Window::new("OBS Overlay Web Server").open(&mut open).show(ctx, |ui| {
            let mut cfg = self.app.settings.read().overlay.clone();
            ui.checkbox(&mut cfg.enabled, "Run server");
            ui.horizontal(|ui| {
                ui.label("Port:");
                ui.add(egui::DragValue::new(&mut cfg.port).range(1..=65535));
            });
            let mut template = cfg.template.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label("Template (empty for built-in):");
                if ui.text_edit_singleline(&mut template).lost_focus() {
                    cfg.template = if template.trim().is_empty() { None } else { Some(PathBuf::from(template.trim())) };
                }
            });
            if let Some(port) = self.app.overlay_port() {
                ui.hyperlink_to(format!("http://127.0.0.1:{port}/"), format!("http://127.0.0.1:{port}/"));
                ui.hyperlink_to("params.json", format!("http://127.0.0.1:{port}/params.json"));
            }
            if cfg != self.app.settings.read().overlay {
                self.app.settings.write().overlay = cfg;
                self.app.apply_services();
            }
        });
        self.show_overlay = open;
    }

    fn playlist_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_playlist;
        egui::Window::new("Playlist Writer").open(&mut open).show(ctx, |ui| {
            let mut cfg = self.app.settings.read().playlist.clone();
            let mut file = cfg.file.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label("CSV file:");
                if ui.text_edit_singleline(&mut file).changed() {
                    cfg.file = if file.trim().is_empty() { None } else { Some(PathBuf::from(file.trim())) };
                }
            });
            ui.horizontal(|ui| {
                ui.label("Minimum seconds played:");
                ui.add(egui::DragValue::new(&mut cfg.min_play_seconds).range(0..=600));
            });
            ui.checkbox(&mut cfg.master_only, "Only the tempo master");
            ui.checkbox(&mut cfg.append, "Append to existing file");
            ui.checkbox(&mut cfg.enabled, "Writing");
            if let Some(f) = self.app.playlist_file() {
                ui.label(format!("Writing to {}", f.display()));
            }
            if cfg != self.app.settings.read().playlist {
                let restart = cfg.enabled != self.app.settings.read().playlist.enabled;
                self.app.settings.write().playlist = cfg;
                if restart {
                    self.app.apply_services();
                }
            }
        });
        self.show_playlist = open;
    }

    // ------------------------------------------------------------------ load track / settings

    fn load_track_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_load_track;
        egui::Window::new("Load Track on Player").open(&mut open).show(ctx, |ui| {
            let lt = &mut self.load_track;
            ui.horizontal(|ui| {
                ui.label("Load on player:");
                ui.add(egui::DragValue::new(&mut lt.target).range(1..=6));
            });
            ui.horizontal(|ui| {
                ui.label("Track from player:");
                ui.add(egui::DragValue::new(&mut lt.source_player).range(1..=32));
                egui::ComboBox::from_id_salt("lt-slot").selected_text(lt.slot.java_name()).show_ui(ui, |ui| {
                    for s in [TrackSourceSlot::UsbSlot, TrackSourceSlot::SdSlot, TrackSourceSlot::CdSlot, TrackSourceSlot::Collection, TrackSourceSlot::Usb2Slot] {
                        ui.selectable_value(&mut lt.slot, s, s.java_name());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("rekordbox ID:");
                ui.add(egui::DragValue::new(&mut lt.rekordbox_id).range(1..=u32::MAX));
            });
            if ui.button("Load").clicked() {
                let r = self.app.link.virtual_cdj.send_load_track_command(lt.target, lt.rekordbox_id, lt.source_player, lt.slot, TrackType::Rekordbox);
                let msg = match r {
                    Ok(()) => "Load track command sent.".to_string(),
                    Err(e) => format!("Unable to load track: {e}"),
                };
                self.status_message = Some((std::time::Instant::now(), msg));
            }
            ui.label(RichText::new("Browsing player menus to pick tracks is not yet ported; enter the rekordbox ID.").small().weak());
        });
        self.show_load_track = open;
    }

    fn load_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_load_settings;
        egui::Window::new("Load Settings on Player").open(&mut open).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Player:");
                ui.add(egui::DragValue::new(&mut self.settings_target).range(1..=6));
            });
            egui::Grid::new("player-settings").striped(true).show(ui, |ui| {
                for spec in SETTINGS {
                    ui.label(spec.label);
                    let mut current = self.player_settings.values.get(spec.key).cloned().unwrap_or_default();
                    egui::ComboBox::from_id_salt(("ps", spec.key)).selected_text(&current).show_ui(ui, |ui| {
                        for (label, _) in spec.choices {
                            ui.selectable_value(&mut current, label.to_string(), *label);
                        }
                    });
                    self.player_settings.set(spec.key, &current);
                    ui.end_row();
                }
            });
            if ui.button("Send to Player").clicked() {
                let msg = match self.app.link.virtual_cdj.send_load_settings_command(self.settings_target, &self.player_settings) {
                    Ok(()) => "Settings sent.".to_string(),
                    Err(e) => format!("Unable to send settings: {e}"),
                };
                self.status_message = Some((std::time::Instant::now(), msg));
            }
        });
        self.show_load_settings = open;
    }

    // ------------------------------------------------------------------ shows

    fn event_spec_ui(ui: &mut egui::Ui, id: impl std::hash::Hash, label: &str, spec: &mut EventSpec, allow_same: bool) -> bool {
        let mut changed = false;
        ui.label(label);
        egui::ComboBox::from_id_salt(id).selected_text(spec.message.label()).show_ui(ui, |ui| {
            let mut choices = vec![EventMessage::None, EventMessage::Note, EventMessage::Cc, EventMessage::Custom];
            if allow_same {
                choices.push(EventMessage::Same);
            }
            for m in choices {
                changed |= ui.selectable_value(&mut spec.message, m, m.label()).changed();
            }
        });
        if matches!(spec.message, EventMessage::Note | EventMessage::Cc) {
            changed |= ui.add(egui::DragValue::new(&mut spec.note).range(0..=127)).changed();
            changed |= ui.add(egui::DragValue::new(&mut spec.channel).range(1..=16).prefix("ch ")).changed();
        }
        changed
    }

    fn show_windows(&mut self, ctx: &egui::Context) {
        let shows: Vec<Arc<ShowRunner>> = self.app.shows.read().clone();
        self.shows_open.resize(shows.len(), true);
        let mut open_editor = None;
        let mut close = None;
        for (si, runner) in shows.iter().enumerate() {
            let mut open = self.shows_open[si];
            egui::Window::new(format!("Show: {}", runner.title())).id(egui::Id::new(("show", si))).open(&mut open).default_width(640.0).show(ctx, |ui| {
                let mut show = runner.show.read().clone();
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("Enabled Default:");
                    egui::ComboBox::from_id_salt(("show-enabled", si)).selected_text(show.enabled.label()).show_ui(ui, |ui| {
                        for e in ShowEnabled::SHOW_CHOICES {
                            changed |= ui.selectable_value(&mut show.enabled, e, e.label()).changed();
                        }
                    });
                    if ui.button("Save Show").clicked() {
                        if let Some(p) = &runner.path {
                            let msg = match runner.show.read().save(&p.with_extension(crate::shows::SHOW_FILE_EXTENSION)) {
                                Ok(()) => format!("Saved {}", p.with_extension(crate::shows::SHOW_FILE_EXTENSION).display()),
                                Err(e) => format!("Unable to save show: {e}"),
                            };
                            self.status_message = Some((std::time::Instant::now(), msg));
                        }
                    }
                    if ui.button("Close Show").clicked() {
                        close = Some(si);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Import loaded track from player:");
                    for d in self.app.devices() {
                        if d.track.as_ref().and_then(|t| t.signature.clone()).is_some() && ui.button(d.number.to_string()).clicked() {
                            if let Some(sig) = show.add_track(d.track.as_ref().unwrap()) {
                                changed = true;
                                self.status_message = Some((std::time::Instant::now(), format!("Added track {sig}")));
                            }
                        }
                    }
                });
                let playing: std::collections::BTreeMap<u8, crate::shows::PlayerTrackState> = runner.player_states();
                egui::ScrollArea::vertical().max_height(500.0).id_salt(("show-scroll", si)).show(ui, |ui| {
                    let mut remove_track = None;
                    for (sig, track) in show.tracks.iter_mut() {
                        let active: Vec<String> = playing.iter().filter(|(_, s)| &s.signature == sig).map(|(p, s)| format!("{p}{}", if s.playing { "▶" } else { "" })).collect();
                        let header = format!(
                            "{} — {}{}",
                            track.metadata.title,
                            track.metadata.artist.clone().unwrap_or_default(),
                            if active.is_empty() { String::new() } else { format!("   [players {}]", active.join(", ")) }
                        );
                        egui::CollapsingHeader::new(header).id_salt(("track", si, sig.as_str())).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("MIDI Output:");
                                let selected = track.midi_device.clone().unwrap_or_else(|| "(none)".into());
                                egui::ComboBox::from_id_salt(("track-out", si, sig.as_str())).selected_text(selected).show_ui(ui, |ui| {
                                    changed |= ui.selectable_value(&mut track.midi_device, None, "(none)").changed();
                                    for name in &self.midi_outputs {
                                        changed |= ui.selectable_value(&mut track.midi_device, Some(name.clone()), name).changed();
                                    }
                                });
                                ui.label("Enabled:");
                                egui::ComboBox::from_id_salt(("track-en", si, sig.as_str())).selected_text(track.enabled.label()).show_ui(ui, |ui| {
                                    for e in ShowEnabled::TRACK_CHOICES {
                                        changed |= ui.selectable_value(&mut track.enabled, e, e.label()).changed();
                                    }
                                });
                            });
                            ui.horizontal(|ui| {
                                changed |= Self::event_spec_ui(ui, ("loaded", si, sig.as_str()), "Loaded:", &mut track.loaded, false);
                                changed |= Self::event_spec_ui(ui, ("playing", si, sig.as_str()), "Playing:", &mut track.playing, false);
                            });
                            ui.horizontal_wrapped(|ui| {
                                for (kind, title) in TRACK_EXPRESSIONS {
                                    let mark = if track.expressions.get(*kind).map(|s| !s.trim().is_empty()).unwrap_or(false) { " *" } else { "" };
                                    if ui.small_button(format!("{title}{mark}")).clicked() {
                                        open_editor = Some(ExprTarget::Track { show: si, signature: sig.clone(), kind: kind.to_string() });
                                    }
                                }
                            });
                            ui.separator();
                            ui.label(RichText::new("Cues").strong());
                            let mut remove_cue = None;
                            for (ci, cue) in track.cues.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 16.0), Sense::hover());
                                    ui.painter().rect_filled(rect, 2.0, color_for_hue(cue.hue));
                                    changed |= ui.add(egui::TextEdit::singleline(&mut cue.comment).desired_width(140.0)).changed();
                                    ui.label("beats");
                                    changed |= ui.add(egui::DragValue::new(&mut cue.start).range(1..=100_000)).changed();
                                    ui.label("–");
                                    changed |= ui.add(egui::DragValue::new(&mut cue.end).range(cue.start + 1..=100_001)).changed();
                                    if ui.small_button("🗑").clicked() {
                                        remove_cue = Some(ci);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    changed |= Self::event_spec_ui(ui, ("entered", si, sig.as_str(), ci), "Entered:", &mut cue.entered, false);
                                    changed |= Self::event_spec_ui(ui, ("onbeat", si, sig.as_str(), ci), "On-Beat:", &mut cue.started_on_beat, false);
                                    changed |= Self::event_spec_ui(ui, ("late", si, sig.as_str(), ci), "Late:", &mut cue.started_late, true);
                                });
                                ui.horizontal_wrapped(|ui| {
                                    for (kind, title) in CUE_EXPRESSIONS {
                                        let mark = if cue.expressions.get(*kind).map(|s| !s.trim().is_empty()).unwrap_or(false) { " *" } else { "" };
                                        if ui.small_button(format!("{title}{mark}")).clicked() {
                                            open_editor = Some(ExprTarget::Cue { show: si, signature: sig.clone(), uuid: cue.uuid.clone(), kind: kind.to_string() });
                                        }
                                    }
                                });
                                ui.separator();
                            }
                            if let Some(ci) = remove_cue {
                                track.cues.remove(ci);
                                changed = true;
                            }
                            ui.horizontal(|ui| {
                                ui.label("New cue: beats");
                                ui.add(egui::DragValue::new(&mut self.new_cue.0).range(1..=100_000));
                                ui.add(egui::DragValue::new(&mut self.new_cue.1).range(2..=100_001));
                                ui.add(egui::TextEdit::singleline(&mut self.new_cue.2).hint_text("comment").desired_width(120.0));
                                if ui.button("Add Cue").clicked() {
                                    let uuid = format!("{:x}-{}", beat_link::clock::monotonic_nanos(), track.cues.len());
                                    track.cues.push(ShowCue {
                                        uuid,
                                        comment: self.new_cue.2.clone(),
                                        start: self.new_cue.0,
                                        end: self.new_cue.1.max(self.new_cue.0 + 1),
                                        hue: (track.cues.len() as f64 * 62.5) % 360.0,
                                        ..Default::default()
                                    });
                                    track.cues.sort_by_key(|c| (c.start, c.end));
                                    changed = true;
                                }
                            });
                            if ui.button("Remove Track from Show").clicked() {
                                remove_track = Some(sig.clone());
                            }
                        });
                    }
                    if let Some(sig) = remove_track {
                        show.tracks.remove(&sig);
                        changed = true;
                    }
                });
                if changed {
                    *runner.show.write() = show;
                    runner.compile_all();
                }
            });
            self.shows_open[si] = open;
        }
        if let Some(t) = open_editor {
            self.open_editor(t);
        }
        if let Some(i) = close {
            self.app.close_show(i);
            self.shows_open.remove(i);
        }
    }

    // ------------------------------------------------------------------ logs, MIDI, about, files

    fn logs_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_logs;
        egui::Window::new("Logs").open(&mut open).default_size([700.0, 300.0]).show(ctx, |ui| {
            if ui.button("Clear").clicked() {
                crate::logs::buffer().clear();
            }
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in crate::logs::buffer().lines() {
                    let color = match line.level {
                        log::Level::Error => Color32::LIGHT_RED,
                        log::Level::Warn => Color32::YELLOW,
                        _ => ui.visuals().text_color(),
                    };
                    ui.colored_label(color, RichText::new(format!("{} {:5} {}", line.time, line.level, line.message)).monospace());
                }
            });
        });
        self.show_logs = open;
    }

    fn midi_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_midi;
        egui::Window::new("MIDI Monitor").open(&mut open).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Rescan Outputs").clicked() {
                    self.midi_outputs = self.app.outputs.midi.list_outputs();
                    self.app.outputs.midi.close_all();
                }
                if ui.button("Clear").clicked() {
                    self.app.outputs.midi.clear_history();
                }
            });
            ui.label(format!("Outputs: {}", if self.midi_outputs.is_empty() { "none found".into() } else { self.midi_outputs.join(", ") }));
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for (name, bytes) in self.app.outputs.midi.history() {
                    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
                    ui.label(RichText::new(format!("{name}: {}", hex.join(" "))).monospace());
                }
                for (dest, address) in self.app.outputs.osc.history() {
                    ui.label(RichText::new(format!("OSC {dest} {address}")).monospace());
                }
            });
        });
        self.show_midi = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_about;
        egui::Window::new("About Beat Link Trigger").open(&mut open).show(ctx, |ui| {
            ui.heading("Beat Link Trigger");
            ui.label(format!("Version {} (Rust rewrite)", crate::VERSION));
            ui.label("Trigger events and automate shows in response to CDJ activity.");
            ui.label("Originally by James Elliott / Deep Symmetry. Licensed under the Eclipse Public License 2.0.");
            ui.label("Not affiliated with AlphaTheta Corporation (Pioneer DJ).");
        });
        self.show_about = open;
    }

    fn file_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.file_dialog.open {
            return;
        }
        let title = match self.file_dialog.purpose {
            FilePurpose::Open => "Load Triggers from File",
            FilePurpose::SaveAs => "Save Triggers to File",
            FilePurpose::OpenShow => "Open Show",
            FilePurpose::NewShow => "New Show",
            FilePurpose::Report => "Write Expression Report",
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new(title).open(&mut open).collapsible(false).show(ctx, |ui| {
            ui.label("Path:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.file_dialog.path).desired_width(420.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if ui.button("OK").clicked() {
                go = true;
            }
        });
        if go {
            let path = PathBuf::from(self.file_dialog.path.trim());
            let msg = match self.file_dialog.purpose {
                FilePurpose::Open => self.app.load_file(&path).map(|_| format!("Loaded {}", path.display())),
                FilePurpose::SaveAs => {
                    let p = if path.extension().is_none() { path.with_extension(crate::config::TRIGGER_FILE_EXTENSION) } else { path.clone() };
                    self.app.save(Some(&p)).map(|p| format!("Saved {}", p.display()))
                }
                FilePurpose::OpenShow => self.app.open_show(&path).map(|r| format!("Opened show {}", r.title())),
                FilePurpose::NewShow => self.app.new_show(&path).map(|r| format!("Created show {}", r.title())),
                FilePurpose::Report => std::fs::write(&path, self.app.expression_report()).map(|_| format!("Wrote {}", path.display())),
            };
            self.notify(match msg {
                Ok(m) => m,
                Err(e) => format!("Problem: {e}"),
            });
            open = false;
        }
        self.file_dialog.open = open;
    }
}

const EXPRESSION_HELP: &str = "Expressions are written in Rhai (https://rhai.rs).\n\
Values: status (map), device_number, device_name, address, timestamp, beat_number, bar_number, beat_within_bar, \
bar_meaningful, is_beat, cdj, mixer, effective_tempo, track_bpm, raw_bpm, raw_pitch, pitch_percent, pitch_multiplier, \
playing, paused, cued, looping, on_air, synced, tempo_master, busy, cue_countdown, cue_countdown_text, rekordbox_id, \
track_number, track_source_player, track_source_slot, track_type, track_metadata, track_title, track_artist, \
track_comment, track_genre, track_key, track_label, track_length, track_time_reached, next_cue, previous_cue.\n\
Triggers: trigger_index, trigger_comment, trigger_output, trigger_message, trigger_note, trigger_channel, \
trigger_enabled, trigger_active. Shows: track, cue, show_locals.\n\
State: locals[\"key\"] and globals[\"key\"] (also .get, .set, .increment, .remove, .keys).\n\
Functions: midi_note_on(out, note, vel, ch), midi_note_off(out, note, ch), midi_cc(out, cc, value, ch), \
midi_send(out, [bytes]), midi_outputs(), osc_send(host, port, address, args), udp_send(host, port, text), \
tcp_send(host, port, text), send_sync_mode(player, bool), appoint_tempo_master(player), fader_start([start], [stop]), \
send_on_air([channels]), set_link_tempo(bpm), playback_time(player), now_ms(), sleep_ms(ms), format_time(ms), \
log_info/log_warn/log_error/log_debug(text), print(text). Set globals[\"use_fixed_sync_bpm\"] to override sync tempo.";

impl eframe::App for Gui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_midi_scan.elapsed() > Duration::from_secs(5) {
            self.midi_outputs = self.app.outputs.midi.list_outputs();
            self.last_midi_scan = std::time::Instant::now();
        }
        self.menu_bar(ctx);
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let msg = self.status_message.as_ref().filter(|(t, _)| t.elapsed() < Duration::from_secs(8)).map(|(_, m)| m.clone());
                ui.label(msg.unwrap_or_else(|| {
                    let s = self.app.settings_path.read().clone();
                    s.map(|p| format!("Configuration: {}", p.display())).unwrap_or_else(|| "Unsaved configuration".into())
                }));
            });
        });
        self.trigger_rows(ctx);
        if self.show_status {
            self.player_status_window(ctx);
        }
        if self.show_carabiner {
            self.carabiner_window(ctx);
        }
        if self.show_simulator {
            self.simulator_window(ctx);
        }
        if self.show_overlay {
            self.overlay_window(ctx);
        }
        if self.show_playlist {
            self.playlist_window(ctx);
        }
        if self.show_load_track {
            self.load_track_window(ctx);
        }
        if self.show_load_settings {
            self.load_settings_window(ctx);
        }
        if self.show_logs {
            self.logs_window(ctx);
        }
        if self.show_midi {
            self.midi_window(ctx);
        }
        if self.show_about {
            self.about_window(ctx);
        }
        self.show_windows(ctx);
        self.editor_windows(ctx);
        self.file_dialog_window(ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Open the windows that should be visible at startup.
pub fn configure_startup(gui: &mut Gui, settings: &Settings, show_status: bool, show_simulator: bool) {
    gui.show_status = show_status;
    gui.show_simulator = show_simulator;
    gui.shows_open = vec![true; settings.shows.len()];
}

/// Run the GUI until the window is closed.
pub fn run(app: Arc<App>, show_status: bool, show_simulator: bool) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 900.0]).with_title("Beat Link Trigger"),
        ..Default::default()
    };
    let settings = app.settings.read().clone();
    let mut gui = Gui::new(app);
    configure_startup(&mut gui, &settings, show_status, show_simulator);
    eframe::run_native("Beat Link Trigger", options, Box::new(move |_cc| Ok(Box::new(gui)))).map_err(|e| e.to_string())
}

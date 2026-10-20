//! Application shell: menus, toolbar, tabs, status bar, SQL log dock, and
//! the "pending changes" confirmation flow.

use std::path::PathBuf;
use std::time::Instant;

use egui::{Key, KeyboardShortcut, Modifiers, RichText, Ui};

use crate::db::{as_text, Database, LogSource, ObjKind, SchemaObject};
use crate::ui::browse::BrowseState;
use crate::ui::dialogs::{CreateIndexDialog, CreateTableDialog, ExportDialog, ExportSource, ImportDialog};
use crate::ui::execute::ExecState;
use crate::ui::log::LogUi;
use crate::ui::pragmas::PragmaState;
use crate::ui::structure::StructureState;
use crate::ui::widgets::palette;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Structure,
    Browse,
    Pragmas,
    Execute,
}

/// Actions that must wait for the user to decide what to do with pending changes.
#[derive(Clone)]
pub enum Deferred {
    Close,
    Open(PathBuf, bool),
    New(PathBuf),
    Quit,
}

pub enum Confirm {
    Pending(Deferred),
    Drop(ObjKind, String),
}

pub struct App {
    pub db: Option<Database>,
    pub tab: Tab,
    pub schema: Vec<SchemaObject>,
    pub schema_dirty: bool,

    pub structure: StructureState,
    pub browse: BrowseState,
    pub exec: ExecState,
    pub pragmas: PragmaState,
    pub log_ui: LogUi,

    pub import: Option<ImportDialog>,
    pub export: Option<ExportDialog>,
    pub create_table: Option<CreateTableDialog>,
    pub create_index: Option<CreateIndexDialog>,
    pub confirm: Option<Confirm>,
    pub open_path: Option<(String, bool)>,
    pub about: bool,

    pub errors: Vec<String>,
    pub status: Option<(String, Instant)>,
    pub allow_quit: bool,
    pub sqlite_version: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.0);
        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        cc.egui_ctx.set_global_style(style);

        let mut app = App {
            db: None,
            tab: Tab::Structure,
            schema: Vec::new(),
            schema_dirty: false,
            structure: StructureState::default(),
            browse: BrowseState::default(),
            exec: ExecState::default(),
            pragmas: PragmaState::default(),
            log_ui: LogUi::default(),
            import: None,
            export: None,
            create_table: None,
            create_index: None,
            confirm: None,
            open_path: None,
            about: false,
            errors: Vec::new(),
            status: None,
            allow_quit: false,
            sqlite_version: rusqlite::version().to_string(),
        };
        if let Some(p) = initial {
            app.open_db(p, false);
        }
        app
    }

    // ----- helpers -----------------------------------------------------------

    pub fn notify(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    pub fn has_pending(&self) -> bool {
        self.db.as_ref().is_some_and(|d| d.has_pending_changes())
    }

    /// Call after anything that may have changed the schema or data.
    pub fn invalidate(&mut self) {
        self.schema_dirty = true;
        self.browse.dirty = true;
        self.structure.details = None;
        self.pragmas.loaded = false;
        self.pragmas.journal_mode_cache = None;
    }

    pub fn refresh_schema(&mut self) {
        if let Some(db) = self.db.as_mut() {
            match db.schema_objects() {
                Ok(s) => self.schema = s,
                Err(e) => self.errors.push(e),
            }
        } else {
            self.schema.clear();
        }
        self.schema_dirty = false;
    }

    pub fn tables(&self, include_views: bool) -> Vec<String> {
        self.schema
            .iter()
            .filter(|o| o.kind == ObjKind::Table || (include_views && o.kind == ObjKind::View))
            .map(|o| o.name.clone())
            .collect()
    }

    // ----- open / close -----------------------------------------------------

    pub fn request(&mut self, action: Deferred) {
        if self.has_pending() {
            self.confirm = Some(Confirm::Pending(action));
        } else {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: Deferred) {
        match action {
            Deferred::Close => self.close_db(),
            Deferred::Open(p, ro) => self.open_db(p, ro),
            Deferred::New(p) => {
                let _ = std::fs::remove_file(&p); // the save dialog already confirmed overwrite
                self.open_db(p, false);
                self.create_table = Some(CreateTableDialog::default());
            }
            Deferred::Quit => {
                self.allow_quit = true;
            }
        }
    }

    pub fn open_db(&mut self, path: PathBuf, read_only: bool) {
        self.close_db();
        match Database::open(&path, read_only) {
            Ok(db) => {
                self.db = Some(db);
                self.tab = Tab::Structure;
                self.browse = BrowseState::default();
                self.structure = StructureState::default();
                self.pragmas = PragmaState::default();
                self.exec.outcomes.clear();
                self.exec.result = None;
                self.refresh_schema();
                self.notify(format!("Opened {}", path.display()));
            }
            Err(e) => self.error(format!("Could not open {}:\n{e}", path.display())),
        }
    }

    pub fn close_db(&mut self) {
        if let Some(mut db) = self.db.take() {
            let _ = db.revert_changes();
        }
        self.schema.clear();
        self.browse = BrowseState::default();
        self.structure = StructureState::default();
        self.pragmas = PragmaState::default();
    }

    pub fn write_changes(&mut self) {
        if let Some(db) = self.db.as_mut() {
            match db.write_changes() {
                Ok(()) => self.notify("All changes written to disk."),
                Err(e) => self.errors.push(e),
            }
        }
        self.pragmas.loaded = false;
        self.pragmas.journal_mode_cache = None;
    }

    pub fn revert_changes(&mut self) {
        if let Some(db) = self.db.as_mut() {
            match db.revert_changes() {
                Ok(()) => self.notify("Changes reverted."),
                Err(e) => self.errors.push(e),
            }
        }
        self.invalidate();
    }

    pub fn drop_object(&mut self, kind: ObjKind, name: &str) {
        let Some(db) = self.db.as_mut() else { return };
        let sql = format!("DROP {} {};", kind.sql_word(), crate::db::quote_ident(name));
        match db.execute_edit(&sql, &[]) {
            Ok(_) => self.notify(format!("Dropped {} {name}.", kind.sql_word().to_lowercase())),
            Err(e) => self.errors.push(e),
        }
        if self.browse.table.as_deref() == Some(name) {
            self.browse = BrowseState::default();
        }
        self.structure.selected = None;
        self.invalidate();
    }

    // ----- file pickers --------------------------------------------------------

    fn pick_open(&mut self, read_only: bool) {
        let picked = rfd::FileDialog::new()
            .set_title(if read_only { "Open Database Read-Only" } else { "Open Database" })
            .add_filter("SQLite databases", &["db", "sqlite", "sqlite3", "db3", "s3db", "sl3"])
            .add_filter("All files", &["*"])
            .pick_file();
        match picked {
            Some(p) => self.request(Deferred::Open(p, read_only)),
            // No portal/file chooser available (e.g. minimal Linux setups):
            // fall back to typing a path.
            None => {}
        }
    }

    fn pick_new(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .set_title("New Database")
            .set_file_name("new.db")
            .add_filter("SQLite databases", &["db", "sqlite", "sqlite3"])
            .save_file()
        {
            self.request(Deferred::New(p));
        }
    }

    fn dump_sql(&mut self) {
        let Some(db) = self.db.as_mut() else { return };
        let Some(p) = rfd::FileDialog::new()
            .set_title("Export Database to SQL File")
            .set_file_name(format!("{}.sql", db.file_name().trim_end_matches(".db")))
            .save_file()
        else {
            return;
        };
        let res = std::fs::File::create(&p)
            .map_err(|e| e.to_string())
            .and_then(|f| db.dump_sql(&mut std::io::BufWriter::new(f)));
        match res {
            Ok(n) => self.notify(format!("Exported schema and {n} row(s) to {}", p.display())),
            Err(e) => self.error(e),
        }
    }

    // ----- frame ---------------------------------------------------------------

    fn shortcuts(&mut self, ui: &mut Ui) {
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let (save, open, run, run_cur, run_f5, refresh, log) = ui.input_mut(|i| {
            (
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::S)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::O)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::Enter)),
                i.consume_shortcut(&sc(Modifiers::SHIFT, Key::F5)),
                i.consume_shortcut(&sc(Modifiers::NONE, Key::F5)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::R)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::L)),
            )
        });
        if save && self.has_pending() {
            self.write_changes();
        }
        if open {
            self.pick_open(false);
        }
        if self.tab == Tab::Execute && self.db.is_some() {
            if run || run_f5 {
                self.exec.run_requested = Some(false);
            }
            if run_cur {
                self.exec.run_requested = Some(true);
            }
        }
        if refresh {
            self.invalidate();
        }
        if log {
            self.log_ui.visible = !self.log_ui.visible;
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let has_db = self.db.is_some();
        let pending = self.has_pending();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New Database…").clicked() {
                    self.pick_new();
                    ui.close();
                }
                if ui.button("Open Database…            Ctrl+O").clicked() {
                    self.pick_open(false);
                    ui.close();
                }
                if ui.button("Open Database Read-Only…").clicked() {
                    self.pick_open(true);
                    ui.close();
                }
                if ui.button("Open by Path…").clicked() {
                    self.open_path = Some((String::new(), false));
                    ui.close();
                }
                if ui.add_enabled(has_db, egui::Button::new("Close Database")).clicked() {
                    self.request(Deferred::Close);
                    ui.close();
                }
                ui.separator();
                if ui.add_enabled(pending, egui::Button::new("Write Changes               Ctrl+S")).clicked() {
                    self.write_changes();
                    ui.close();
                }
                if ui.add_enabled(pending, egui::Button::new("Revert Changes")).clicked() {
                    self.revert_changes();
                    ui.close();
                }
                ui.separator();
                ui.menu_button("Import", |ui| {
                    if ui.add_enabled(has_db, egui::Button::new("Table from CSV file…")).clicked() {
                        self.import = Some(ImportDialog::pick());
                        ui.close();
                    }
                });
                ui.menu_button("Export", |ui| {
                    if ui.add_enabled(has_db, egui::Button::new("Table as CSV file…")).clicked() {
                        let t = self.browse.table.clone().or_else(|| self.tables(true).first().cloned());
                        self.export = Some(ExportDialog::new(ExportSource::Table(t.unwrap_or_default())));
                        ui.close();
                    }
                    if ui.add_enabled(has_db, egui::Button::new("Database to SQL file…")).clicked() {
                        self.dump_sql();
                        ui.close();
                    }
                });
                ui.separator();
                if ui.button("Quit").clicked() {
                    self.request(Deferred::Quit);
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(has_db && !self.db.as_ref().unwrap().read_only, egui::Button::new("Create Table…")).clicked() {
                    self.create_table = Some(CreateTableDialog::default());
                    ui.close();
                }
                if ui.add_enabled(has_db, egui::Button::new("Create Index…")).clicked() {
                    self.create_index = Some(CreateIndexDialog::new(self.structure.selected_table(&self.schema)));
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut self.log_ui.visible, "SQL Log  (Ctrl+L)");
                if ui.button("Refresh  (Ctrl+R)").clicked() {
                    self.invalidate();
                    ui.close();
                }
                ui.separator();
                ui.label("Theme");
                let mut pref = ui.ctx().options(|o| o.theme_preference);
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut pref, egui::ThemePreference::System, "System");
                    ui.selectable_value(&mut pref, egui::ThemePreference::Light, "Light");
                    ui.selectable_value(&mut pref, egui::ThemePreference::Dark, "Dark");
                });
                ui.ctx().set_theme(pref);
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About Quarry").clicked() {
                    self.about = true;
                    ui.close();
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let has_db = self.db.is_some();
        let pending = self.has_pending();
        ui.horizontal(|ui| {
            if ui.button("🗋 New Database").clicked() {
                self.pick_new();
            }
            if ui.button("🗁 Open Database").on_hover_text("Ctrl+O").clicked() {
                self.pick_open(false);
            }
            let p = palette(ui);
            let write = egui::Button::new(RichText::new("✔ Write Changes").color(if pending { p.ok } else { ui.visuals().weak_text_color() }));
            if ui.add_enabled(pending, write).on_hover_text("Commit all pending changes (Ctrl+S)").clicked() {
                self.write_changes();
            }
            let revert = egui::Button::new(RichText::new("↶ Revert Changes").color(if pending { p.err } else { ui.visuals().weak_text_color() }));
            if ui.add_enabled(pending, revert).on_hover_text("Roll back to the last write").clicked() {
                self.revert_changes();
            }
            ui.separator();
            if ui.add_enabled(has_db, egui::Button::new("⇣ Import CSV")).clicked() {
                self.import = Some(ImportDialog::pick());
            }
            if ui.add_enabled(has_db, egui::Button::new("⇡ Export CSV")).clicked() {
                let t = self.browse.table.clone().or_else(|| self.tables(true).first().cloned());
                self.export = Some(ExportDialog::new(ExportSource::Table(t.unwrap_or_default())));
            }
            ui.separator();
            ui.toggle_value(&mut self.log_ui.visible, "📜 SQL Log");
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = palette(ui);
        ui.horizontal(|ui| {
            match self.db.as_mut() {
                None => {
                    ui.label(RichText::new("No database open").weak());
                }
                Some(db) => {
                    ui.label(RichText::new(db.path.display().to_string()).monospace().small());
                    if db.read_only {
                        ui.label(RichText::new("READ-ONLY").small().color(p.warn));
                    }
                    ui.separator();
                    let jm = self.pragmas.journal_mode_cache.clone().unwrap_or_else(|| {
                        let v = db.get_pragma("journal_mode").map(|v| as_text(&v)).unwrap_or_default();
                        self.pragmas.journal_mode_cache = Some(v.clone());
                        v
                    });
                    ui.label(RichText::new(format!("journal: {}", jm.to_uppercase())).small());
                    ui.separator();
                    if db.has_pending_changes() {
                        ui.label(RichText::new("● Pending changes").small().color(p.warn));
                    } else {
                        ui.label(RichText::new("● Saved").small().color(p.ok));
                    }
                }
            }
            ui.separator();
            ui.label(RichText::new(format!("SQLite {}", self.sqlite_version)).small().weak());
            if let Some((msg, t)) = &self.status {
                if t.elapsed().as_secs() < 8 {
                    ui.separator();
                    ui.label(RichText::new(msg).small());
                }
            }
        });
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            for (t, label) in [
                (Tab::Structure, "Database Structure"),
                (Tab::Browse, "Browse Data"),
                (Tab::Pragmas, "Edit Pragmas"),
                (Tab::Execute, "Execute SQL"),
            ] {
                let selected = self.tab == t;
                let text = if selected { RichText::new(label).strong() } else { RichText::new(label) };
                if ui.selectable_label(selected, text).clicked() {
                    self.tab = t;
                }
            }
        });
        ui.separator();
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading("Quarry");
            ui.label(RichText::new("A DB Browser for SQLite–style workbench, written in Rust").weak());
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let w = 420.0;
                ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                if ui.button("🗋  New Database…").clicked() {
                    self.pick_new();
                }
                if ui.button("🗁  Open Database…").clicked() {
                    self.pick_open(false);
                }
                if ui.button("Open by Path…").clicked() {
                    self.open_path = Some((String::new(), false));
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("Tip: you can also pass a database path on the command line.").small().weak());
        });
    }

    // ----- small modal windows -------------------------------------------------

    fn modals(&mut self, ctx: &egui::Context) {
        // Errors
        if !self.errors.is_empty() {
            let mut close = false;
            egui::Window::new("Error")
                .collapsible(false)
                .resizable(true)
                .default_width(460.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let p = palette(ui);
                    for e in &self.errors {
                        ui.label(RichText::new(e).color(p.err));
                    }
                    ui.add_space(6.0);
                    if ui.button("OK").clicked() {
                        close = true;
                    }
                });
            if close {
                self.errors.clear();
            }
        }

        // Pending-changes / drop confirmations
        if let Some(c) = self.confirm.take() {
            let mut keep = true;
            match c {
                Confirm::Pending(action) => {
                    egui::Window::new("Pending changes")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                        .show(ctx, |ui| {
                            ui.label("Do you want to save the changes made to the database?");
                            ui.horizontal(|ui| {
                                if ui.button("Save").clicked() {
                                    self.write_changes();
                                    if !self.has_pending() {
                                        self.perform(action.clone());
                                    }
                                    keep = false;
                                }
                                if ui.button("Discard").clicked() {
                                    self.revert_changes();
                                    self.perform(action.clone());
                                    keep = false;
                                }
                                if ui.button("Cancel").clicked() {
                                    keep = false;
                                }
                            });
                        });
                    if keep {
                        self.confirm = Some(Confirm::Pending(action));
                    }
                }
                Confirm::Drop(kind, name) => {
                    egui::Window::new("Confirm drop")
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                        .show(ctx, |ui| {
                            ui.label(format!(
                                "Drop {} '{}'? You can still revert this until you write changes.",
                                kind.sql_word().to_lowercase(),
                                name
                            ));
                            ui.horizontal(|ui| {
                                if ui.button("Drop").clicked() {
                                    self.drop_object(kind, &name);
                                    keep = false;
                                }
                                if ui.button("Cancel").clicked() {
                                    keep = false;
                                }
                            });
                        });
                    if keep {
                        self.confirm = Some(Confirm::Drop(kind, name));
                    }
                }
            }
        }

        // Open by path
        if let Some((mut path, mut ro)) = self.open_path.take() {
            let mut keep = true;
            egui::Window::new("Open Database by Path")
                .collapsible(false)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label("Path to an SQLite database (it will be created if missing):");
                    let r = ui.add(egui::TextEdit::singleline(&mut path).desired_width(f32::INFINITY));
                    ui.checkbox(&mut ro, "Read-only");
                    ui.horizontal(|ui| {
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (ui.button("Open").clicked() || enter) && !path.trim().is_empty() {
                            self.request(Deferred::Open(PathBuf::from(path.trim()), ro));
                            keep = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                });
            if keep {
                self.open_path = Some((path, ro));
            }
        }

        if self.about {
            egui::Window::new("About Quarry")
                .open(&mut self.about)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.heading("Quarry");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.label(format!("SQLite {}", rusqlite::version()));
                    ui.label("A visual tool for creating, designing and editing SQLite databases,");
                    ui.label("modelled on DB Browser for SQLite. Built with Rust, egui and rusqlite.");
                });
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Ask before closing the window with uncommitted changes.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit && self.has_pending() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Pending(Deferred::Quit));
        }
        if self.allow_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.schema_dirty {
            self.refresh_schema();
        }
        self.shortcuts(ui);

        egui::Panel::top("menu").show(ui, |ui| {
            self.menu_bar(ui);
            ui.add_space(2.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.log_ui.visible {
            egui::Panel::bottom("sql_log")
                .resizable(true)
                .default_size(190.0)
                .min_size(90.0)
                .show(ui, |ui| self.log_panel(ui));
        }
        egui::CentralPanel::default_margins().show(ui, |ui| {
            if self.db.is_none() {
                self.welcome(ui);
                return;
            }
            self.tab_bar(ui);
            match self.tab {
                Tab::Structure => self.structure_tab(ui),
                Tab::Browse => self.browse_tab(ui),
                Tab::Pragmas => self.pragmas_tab(ui),
                Tab::Execute => self.execute_tab(ui),
            }
        });

        self.dialogs(&ctx);
        self.modals(&ctx);
    }
}

/// Log helper for UI modules that need to record a statement they ran
/// outside the normal execute path.
#[allow(dead_code)]
pub fn log_app(db: &mut Database, sql: &str) {
    db.push_log(LogSource::App, sql, true, 0.0, String::new());
}

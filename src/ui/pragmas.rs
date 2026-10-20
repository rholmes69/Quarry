//! Edit Pragmas tab: every common PRAGMA with a typed editor, a dedicated
//! Write-Ahead Logging panel (mode, WAL/SHM files, checkpoints,
//! autocheckpoint), and maintenance commands.

use std::collections::HashMap;

use egui::{RichText, Ui};

use crate::app::App;
use crate::db::{as_text, LogSource};
use crate::ui::widgets::palette;

#[derive(Clone, Copy)]
enum PKind {
    /// (label, value); SQLite may report the value by name or by index.
    Choice(&'static [&'static str]),
    Int,
    Bool,
}

struct PragmaDef {
    name: &'static str,
    kind: PKind,
    help: &'static str,
}

const PRAGMAS: &[PragmaDef] = &[
    PragmaDef { name: "journal_mode", kind: PKind::Choice(&["DELETE", "TRUNCATE", "PERSIST", "MEMORY", "WAL", "OFF"]), help: "How the rollback journal works. WAL allows readers and a writer at the same time. Cannot change inside a transaction." },
    PragmaDef { name: "synchronous", kind: PKind::Choice(&["OFF", "NORMAL", "FULL", "EXTRA"]), help: "Durability vs speed. NORMAL is safe and fast in WAL mode." },
    PragmaDef { name: "foreign_keys", kind: PKind::Bool, help: "Enforce FOREIGN KEY constraints on this connection. Ignored inside a transaction." },
    PragmaDef { name: "auto_vacuum", kind: PKind::Choice(&["NONE", "FULL", "INCREMENTAL"]), help: "Reclaim free pages automatically. Switching from NONE requires a VACUUM." },
    PragmaDef { name: "wal_autocheckpoint", kind: PKind::Int, help: "Checkpoint the WAL automatically once it reaches this many pages (0 = never)." },
    PragmaDef { name: "journal_size_limit", kind: PKind::Int, help: "Max bytes kept for the journal / WAL after a transaction or checkpoint (-1 = no limit)." },
    PragmaDef { name: "cache_size", kind: PKind::Int, help: "Page cache size. Positive = pages, negative = KiB." },
    PragmaDef { name: "page_size", kind: PKind::Int, help: "Database page size in bytes (power of two, 512–65536). Applies after VACUUM; cannot change in WAL mode." },
    PragmaDef { name: "busy_timeout", kind: PKind::Int, help: "Milliseconds to wait for a lock before failing with SQLITE_BUSY." },
    PragmaDef { name: "locking_mode", kind: PKind::Choice(&["NORMAL", "EXCLUSIVE"]), help: "EXCLUSIVE keeps the file lock after the first read/write." },
    PragmaDef { name: "temp_store", kind: PKind::Choice(&["DEFAULT", "FILE", "MEMORY"]), help: "Where temporary tables and indices are stored." },
    PragmaDef { name: "mmap_size", kind: PKind::Int, help: "Bytes of the file to memory-map for reads (0 disables)." },
    PragmaDef { name: "secure_delete", kind: PKind::Bool, help: "Overwrite deleted content with zeros." },
    PragmaDef { name: "recursive_triggers", kind: PKind::Bool, help: "Allow triggers to fire other triggers recursively." },
    PragmaDef { name: "automatic_index", kind: PKind::Bool, help: "Let the planner build temporary indices for joins." },
    PragmaDef { name: "ignore_check_constraints", kind: PKind::Bool, help: "Skip CHECK constraints." },
    PragmaDef { name: "defer_foreign_keys", kind: PKind::Bool, help: "Delay FK checks until the outermost transaction commits." },
    PragmaDef { name: "cell_size_check", kind: PKind::Bool, help: "Extra corruption checks when reading b-tree pages." },
    PragmaDef { name: "trusted_schema", kind: PKind::Bool, help: "Allow SQL functions with side effects in views/triggers from the schema." },
    PragmaDef { name: "query_only", kind: PKind::Bool, help: "Refuse all changes to the database file on this connection." },
    PragmaDef { name: "user_version", kind: PKind::Int, help: "Free integer for applications (often used for schema migrations)." },
    PragmaDef { name: "application_id", kind: PKind::Int, help: "Integer identifying the file format of your application." },
];

const INFO: &[&str] = &["page_count", "freelist_count", "encoding", "schema_version", "data_version", "max_page_count"];

#[derive(Default)]
pub struct PragmaState {
    pub loaded: bool,
    pub journal_mode_cache: Option<String>,
    values: HashMap<&'static str, String>,
    edits: HashMap<&'static str, String>,
    info: Vec<(&'static str, String)>,
    wal_result: Option<String>,
    maintenance_output: String,
}

fn normalise(def: &PragmaDef, raw: &str) -> String {
    match def.kind {
        PKind::Choice(opts) => {
            if let Ok(i) = raw.parse::<usize>() {
                // secure_delete=2 etc. fall through as raw
                opts.get(i).map(|s| s.to_string()).unwrap_or_else(|| raw.to_string())
            } else {
                raw.to_uppercase()
            }
        }
        PKind::Bool => if raw == "0" { "0".into() } else { "1".into() },
        PKind::Int => raw.to_string(),
    }
}

impl App {
    fn load_pragmas(&mut self) {
        let Some(db) = self.db.as_mut() else { return };
        self.pragmas.values.clear();
        for d in PRAGMAS {
            if let Ok(v) = db.get_pragma(d.name) {
                self.pragmas.values.insert(d.name, normalise(d, &as_text(&v)));
            }
        }
        self.pragmas.info = INFO
            .iter()
            .map(|n| (*n, db.get_pragma(n).map(|v| as_text(&v)).unwrap_or_else(|e| e)))
            .collect();
        self.pragmas.edits = self.pragmas.values.clone();
        self.pragmas.journal_mode_cache = self.pragmas.values.get("journal_mode").cloned();
        self.pragmas.loaded = true;
    }

    fn save_pragmas(&mut self) {
        let Some(db) = self.db.as_mut() else { return };
        let mut errors = Vec::new();
        // journal_mode last-but-one and page_size last tend to have ordering
        // constraints; apply in table order which already reflects that.
        for d in PRAGMAS {
            let (Some(new), Some(old)) = (self.pragmas.edits.get(d.name), self.pragmas.values.get(d.name)) else { continue };
            if new == old {
                continue;
            }
            let value = match d.kind {
                PKind::Int => match new.trim().parse::<i64>() {
                    Ok(i) => i.to_string(),
                    Err(_) => {
                        errors.push(format!("{}: '{}' is not an integer", d.name, new));
                        continue;
                    }
                },
                _ => new.clone(),
            };
            if let Err(e) = db.set_pragma(d.name, &value) {
                errors.push(format!("{}: {e}", d.name));
            }
        }
        self.pragmas.loaded = false;
        if errors.is_empty() {
            self.notify("Pragmas saved.");
        } else {
            self.errors.extend(errors);
        }
    }

    pub fn pragmas_tab(&mut self, ui: &mut Ui) {
        if !self.pragmas.loaded {
            self.load_pragmas();
        }
        let p = palette(ui);
        let changed = PRAGMAS.iter().any(|d| self.pragmas.edits.get(d.name) != self.pragmas.values.get(d.name));

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.columns(2, |cols| {
                // ---- left: pragma editor
                let ui = &mut cols[0];
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pragmas").strong());
                    if ui.add_enabled(changed, egui::Button::new("Save")).clicked() {
                        self.save_pragmas();
                    }
                    if ui.add_enabled(changed, egui::Button::new("Reset")).clicked() {
                        self.pragmas.edits = self.pragmas.values.clone();
                    }
                    if ui.button("⟳ Reload").clicked() {
                        self.pragmas.loaded = false;
                    }
                });
                if self.has_pending() {
                    ui.label(
                        RichText::new("Pending changes: journal_mode, page_size, auto_vacuum and foreign_keys can't change until you write or revert them.")
                            .small()
                            .color(p.warn),
                    );
                }
                ui.add_space(4.0);
                egui::Grid::new("pragma_grid").num_columns(2).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                    for d in PRAGMAS {
                        let dirty = self.pragmas.edits.get(d.name) != self.pragmas.values.get(d.name);
                        let mut name = RichText::new(d.name).monospace();
                        if dirty {
                            name = name.color(p.warn).strong();
                        }
                        ui.label(name).on_hover_text(d.help);
                        let Some(v) = self.pragmas.edits.get_mut(d.name) else {
                            ui.label(RichText::new("n/a").weak());
                            ui.end_row();
                            continue;
                        };
                        match d.kind {
                            PKind::Choice(opts) => {
                                egui::ComboBox::from_id_salt(d.name).selected_text(v.as_str()).width(140.0).show_ui(ui, |ui| {
                                    for o in opts {
                                        ui.selectable_value(v, o.to_string(), *o);
                                    }
                                });
                            }
                            PKind::Bool => {
                                let mut b = v == "1";
                                if ui.checkbox(&mut b, "").changed() {
                                    *v = if b { "1".into() } else { "0".into() };
                                }
                            }
                            PKind::Int => {
                                ui.add(egui::TextEdit::singleline(v).desired_width(140.0));
                            }
                        }
                        ui.end_row();
                    }
                });
                ui.add_space(8.0);
                ui.label(RichText::new("Read-only info").strong());
                egui::Grid::new("pragma_info").spacing([14.0, 4.0]).show(ui, |ui| {
                    for (n, v) in &self.pragmas.info {
                        ui.label(RichText::new(*n).monospace().weak());
                        ui.label(v);
                        ui.end_row();
                    }
                });

                // ---- right: WAL + maintenance
                let ui = &mut cols[1];
                self.wal_panel(ui);
                ui.add_space(12.0);
                self.maintenance_panel(ui);
            });
        });
    }

    fn wal_panel(&mut self, ui: &mut Ui) {
        let p = palette(ui);
        ui.label(RichText::new("Write-Ahead Logging").strong());
        let Some(db) = self.db.as_mut() else { return };
        let mode = self.pragmas.values.get("journal_mode").cloned().unwrap_or_default();
        let in_wal = mode.eq_ignore_ascii_case("WAL");
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("journal_mode:");
                ui.label(RichText::new(&mode).strong().color(if in_wal { p.ok } else { ui.visuals().text_color() }));
            });
            ui.horizontal(|ui| {
                let read_only = db.read_only;
                if ui.add_enabled(!in_wal && !read_only, egui::Button::new("Switch to WAL")).clicked() {
                    match db.set_pragma("journal_mode", "WAL") {
                        Ok(v) => self.pragmas.wal_result = Some(format!("journal_mode is now {}", as_text(&v).to_uppercase())),
                        Err(e) => self.errors.push(e),
                    }
                    self.pragmas.loaded = false;
                }
                if ui.add_enabled(in_wal && !read_only, egui::Button::new("Switch to DELETE (rollback)")).clicked() {
                    match db.set_pragma("journal_mode", "DELETE") {
                        Ok(v) => self.pragmas.wal_result = Some(format!("journal_mode is now {}", as_text(&v).to_uppercase())),
                        Err(e) => self.errors.push(e),
                    }
                    self.pragmas.loaded = false;
                }
            });
            ui.add_space(4.0);
            egui::Grid::new("wal_files").spacing([14.0, 2.0]).show(ui, |ui| {
                for (name, size) in db.wal_files() {
                    ui.label(RichText::new(name).monospace().small());
                    ui.label(match size {
                        Some(s) => human_bytes(s),
                        None => "not present".into(),
                    });
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Checkpoint").small().weak());
            ui.horizontal_wrapped(|ui| {
                for (mode, tip) in [
                    ("PASSIVE", "Checkpoint as much as possible without waiting for readers/writers"),
                    ("FULL", "Wait for writers, then checkpoint everything"),
                    ("RESTART", "Like FULL, then wait so the next writer restarts the WAL"),
                    ("TRUNCATE", "Like RESTART, then truncate the WAL file to zero bytes"),
                ] {
                    if ui.add_enabled(in_wal, egui::Button::new(mode)).on_hover_text(tip).clicked() {
                        match db.wal_checkpoint(mode) {
                            Ok((busy, log, ck)) => {
                                self.pragmas.wal_result = Some(format!(
                                    "wal_checkpoint({mode}): busy={busy}, WAL frames={log}, checkpointed={ck}"
                                ))
                            }
                            Err(e) => self.pragmas.wal_result = Some(format!("Error: {e}")),
                        }
                    }
                }
            });
            if let Some(r) = &self.pragmas.wal_result {
                ui.label(RichText::new(r).monospace().small());
            }
            if !in_wal {
                ui.label(RichText::new("Checkpoints are only meaningful in WAL mode.").small().weak());
            }
        });
    }

    fn maintenance_panel(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Maintenance").strong());
        let mut run: Option<&str> = None;
        ui.horizontal_wrapped(|ui| {
            for (sql, label, tip) in [
                ("PRAGMA integrity_check;", "Integrity Check", "Full consistency check of the file"),
                ("PRAGMA quick_check;", "Quick Check", "Faster check that skips index content verification"),
                ("PRAGMA foreign_key_check;", "Foreign-Key Check", "List rows violating FOREIGN KEY constraints"),
                ("PRAGMA optimize;", "Optimize", "Run ANALYZE where it is likely to help"),
                ("ANALYZE;", "Analyze", "Gather statistics for the query planner"),
                ("VACUUM;", "Vacuum (Compact)", "Rebuild the file, reclaiming free space"),
            ] {
                if ui.button(label).on_hover_text(tip).clicked() {
                    run = Some(sql);
                }
            }
        });
        if let Some(sql) = run {
            let needs_clean = sql.starts_with("VACUUM");
            if needs_clean && self.has_pending() {
                self.error("VACUUM cannot run inside a transaction. Write or revert pending changes first.");
            } else if let Some(db) = self.db.as_mut() {
                let start = std::time::Instant::now();
                match db.query(LogSource::App, sql, &[]) {
                    Ok(r) => {
                        let mut out = format!("{sql}  ({:.0} ms)\n", start.elapsed().as_secs_f64() * 1000.0);
                        if r.rows.is_empty() {
                            out.push_str(if r.columns.is_empty() { "done\n" } else { "no problems found\n" });
                        }
                        for row in r.rows.iter().take(500) {
                            out.push_str(&row.iter().map(as_text).collect::<Vec<_>>().join(" | "));
                            out.push('\n');
                        }
                        self.pragmas.maintenance_output = out;
                    }
                    Err(e) => self.pragmas.maintenance_output = format!("{sql}\nError: {e}"),
                }
                self.pragmas.loaded = false;
            }
        }
        if !self.pragmas.maintenance_output.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.label(RichText::new(&self.pragmas.maintenance_output).monospace().small());
                });
            });
        }
    }
}

fn human_bytes(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KiB", b as f64 / 1024.0),
        _ => format!("{:.1} MiB", b as f64 / 1_048_576.0),
    }
}

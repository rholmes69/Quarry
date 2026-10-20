//! Execute SQL tab: highlighted editor, run all / run current statement,
//! result grid, per-statement messages, CSV export of results.

use egui::{RichText, Ui};

use crate::app::App;
use crate::db::{split_statements, QueryResult, StatementOutcome};
use crate::ui::dialogs::{ExportDialog, ExportSource};
use crate::ui::widgets::{data_grid, palette, sql_editor, GridArgs};

pub struct ExecState {
    pub sql: String,
    pub outcomes: Vec<StatementOutcome>,
    pub result: Option<QueryResult>,
    pub result_sql: String,
    /// Selected char range in the editor (start, end).
    pub cursor: Option<(usize, usize)>,
    /// Some(false) = run all, Some(true) = run current/selection.
    pub run_requested: Option<bool>,
    pub row_limit: usize,
    pub selected: Option<(usize, usize)>,
}

impl Default for ExecState {
    fn default() -> Self {
        ExecState {
            sql: "-- Write SQL here. Ctrl+Enter / F5 runs everything, Shift+F5 runs the\n\
                  -- statement under the cursor (or the selection).\n\
                  SELECT name, type, sql FROM sqlite_schema ORDER BY type, name;\n"
                .into(),
            outcomes: vec![],
            result: None,
            result_sql: String::new(),
            cursor: None,
            run_requested: None,
            row_limit: 100_000,
            selected: None,
        }
    }
}

impl ExecState {
    /// The selection if any, otherwise the statement containing the cursor.
    fn current_statement(&self) -> String {
        let chars: Vec<char> = self.sql.chars().collect();
        if let Some((a, b)) = self.cursor {
            if a != b {
                return chars[a.min(chars.len())..b.min(chars.len())].iter().collect();
            }
            // Walk the statements and pick the one spanning the cursor.
            let byte_cursor: usize = chars[..a.min(chars.len())].iter().map(|c| c.len_utf8()).sum();
            let mut offset = 0usize;
            for stmt in split_statements(&self.sql) {
                if let Some(pos) = self.sql[offset..].find(&stmt) {
                    let start = offset + pos;
                    let end = start + stmt.len();
                    if byte_cursor <= end + 1 {
                        return stmt;
                    }
                    offset = end;
                }
            }
        }
        split_statements(&self.sql).pop().unwrap_or_default()
    }
}

impl App {
    pub fn execute_tab(&mut self, ui: &mut Ui) {
        let p = palette(ui);

        ui.horizontal(|ui| {
            if ui.button("▶ Execute all").on_hover_text("Ctrl+Enter / F5").clicked() {
                self.exec.run_requested = Some(false);
            }
            if ui.button("▶ Execute current").on_hover_text("Shift+F5 — statement under cursor, or the selection").clicked() {
                self.exec.run_requested = Some(true);
            }
            ui.separator();
            if ui.button("Clear").clicked() {
                self.exec.sql.clear();
            }
            if ui.button("Open .sql…").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("SQL", &["sql", "txt"]).pick_file() {
                    match std::fs::read_to_string(&path) {
                        Ok(s) => self.exec.sql = s,
                        Err(e) => self.error(e.to_string()),
                    }
                }
            }
            if ui.button("Save .sql…").clicked() {
                if let Some(path) = rfd::FileDialog::new().set_file_name("query.sql").save_file() {
                    if let Err(e) = std::fs::write(&path, &self.exec.sql) {
                        self.error(e.to_string());
                    }
                }
            }
            ui.separator();
            let has_result = self.exec.result.as_ref().is_some_and(|r| !r.columns.is_empty());
            if ui.add_enabled(has_result, egui::Button::new("Export results CSV…")).clicked() {
                if let Some(r) = &self.exec.result {
                    self.export = Some(ExportDialog::new(ExportSource::Result(r.columns.clone(), r.rows.clone())));
                }
            }
        });

        if let Some(current) = self.exec.run_requested.take() {
            let script = if current { self.exec.current_statement() } else { self.exec.sql.clone() };
            self.run_script(&script);
        }

        let avail = ui.available_height();
        let editor_h = (avail * 0.38).clamp(120.0, 420.0);

        // ---- editor
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("sql_editor_scroll").max_height(editor_h).show(ui, |ui| {
                let rows = ((editor_h / ui.text_style_height(&egui::TextStyle::Monospace)) as usize).max(6);
                let out = sql_editor(ui, &mut self.exec.sql, rows, true);
                if let Some(range) = out.cursor_range {
                    let r = range.as_sorted_char_range();
                    self.exec.cursor = Some((r.start.0, r.end.0));
                }
            });
        });

        // ---- messages
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("exec_msgs").max_height(70.0).show(ui, |ui| {
            if self.exec.outcomes.is_empty() {
                ui.label(RichText::new("No statements executed yet.").weak().small());
            }
            for o in &self.exec.outcomes {
                ui.horizontal(|ui| {
                    let (icon, c) = if o.ok { ("✔", p.ok) } else { ("✖", p.err) };
                    ui.label(RichText::new(icon).color(c));
                    let short: String = o.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                    let short: String = short.chars().take(90).collect();
                    ui.label(RichText::new(short).monospace().small());
                    ui.label(RichText::new(&o.message).small().color(if o.ok { ui.visuals().text_color() } else { p.err }));
                });
            }
        });
        ui.separator();

        // ---- results
        match &self.exec.result {
            Some(r) if !r.columns.is_empty() => {
                let id = format!("exec_grid_{}_{}", self.exec.result_sql.len(), r.columns.len());
                let resp = data_grid(
                    ui,
                    GridArgs {
                        id,
                        columns: &r.columns,
                        rows: &r.rows,
                        first_row_number: 1,
                        selected: self.exec.selected,
                        sort: None,
                        filters: None,
                    },
                );
                if let Some(c) = resp.clicked {
                    self.exec.selected = Some(c);
                }
                if let Some(v) = resp.copy_value {
                    ui.ctx().copy_text(v);
                }
            }
            _ => {
                ui.label(RichText::new("Results of the last statement that returns rows appear here.").weak().small());
            }
        }
    }

    fn run_script(&mut self, script: &str) {
        let limit = self.exec.row_limit;
        let Some(db) = self.db.as_mut() else { return };
        let outcomes = db.execute_script(script, limit);
        let last_rows = outcomes.iter().rev().find_map(|o| o.result.clone().map(|r| (o.sql.clone(), r)));
        let any_write = outcomes.iter().any(|o| o.ok && o.result.is_none());
        self.exec.outcomes = outcomes;
        self.exec.selected = None;
        match last_rows {
            Some((sql, r)) => {
                self.exec.result_sql = sql;
                self.exec.result = Some(r);
            }
            None if any_write => self.exec.result = None,
            None => {}
        }
        // Unless everything was a plain query, schema/data/pragmas may have changed.
        let read_only_script = self.exec.outcomes.iter().all(|o| {
            let s = crate::db::strip_comments(&o.sql).trim_start().to_ascii_uppercase();
            ["SELECT", "WITH", "EXPLAIN", "VALUES"].iter().any(|k| s.starts_with(k))
        });
        if !read_only_script {
            self.invalidate();
        }
    }
}

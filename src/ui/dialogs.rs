//! Dialog windows: CSV import, CSV export, Create Table, Create Index.

use std::path::PathBuf;

use egui::{Context, RichText, Ui};
use rusqlite::types::Value;

use crate::app::App;
use crate::csv_io::{
    self, create_table_sql, import_csv, infer_types, read_csv, sniff_delimiter, CsvData, CsvOptions, ExportOptions,
    ImportReport, ImportTarget, OnError,
};
use crate::db::quote_ident;
use crate::ui::widgets::{palette, sql_view};

const TYPES: &[&str] = &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB", "ANY", ""];

// ---------------------------------------------------------------------------
// Delimiter picker (shared)
// ---------------------------------------------------------------------------

fn delimiter_picker(ui: &mut Ui, id: &str, d: &mut u8) {
    let label = match *d {
        b',' => "Comma  ,",
        b';' => "Semicolon  ;",
        b'\t' => "Tab",
        b'|' => "Pipe  |",
        _ => "Other",
    };
    egui::ComboBox::from_id_salt(id).selected_text(label).width(130.0).show_ui(ui, |ui| {
        ui.selectable_value(d, b',', "Comma  ,");
        ui.selectable_value(d, b';', "Semicolon  ;");
        ui.selectable_value(d, b'\t', "Tab");
        ui.selectable_value(d, b'|', "Pipe  |");
    });
    if !matches!(*d, b',' | b';' | b'\t' | b'|') {
        let mut s = (*d as char).to_string();
        if ui.add(egui::TextEdit::singleline(&mut s).desired_width(24.0)).changed() {
            if let Some(c) = s.bytes().last() {
                *d = c;
            }
        }
    }
}

fn quote_picker(ui: &mut Ui, id: &str, q: &mut u8) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(if *q == b'"' { "\"" } else { "'" })
        .width(50.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(q, b'"', "\"");
            ui.selectable_value(q, b'\'', "'");
        });
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

pub struct ImportDialog {
    path: String,
    opts: CsvOptions,
    data: Option<Result<CsvData, String>>,
    loaded_key: String,
    loaded_path_seen: String,
    col_names: Vec<String>,
    col_types: Vec<String>,
    new_table: bool,
    table_name: String,
    existing: String,
    strict: bool,
    on_error: OnError,
    report: Option<Result<ImportReport, String>>,
}

impl ImportDialog {
    pub fn pick() -> Self {
        let path = rfd::FileDialog::new()
            .set_title("Import CSV")
            .add_filter("CSV / TSV", &["csv", "tsv", "txt"])
            .add_filter("All files", &["*"])
            .pick_file();
        let mut d = ImportDialog {
            path: String::new(),
            opts: CsvOptions::default(),
            data: None,
            loaded_key: String::new(),
            loaded_path_seen: String::new(),
            col_names: vec![],
            col_types: vec![],
            new_table: true,
            table_name: String::new(),
            existing: String::new(),
            strict: false,
            on_error: OnError::Abort,
            report: None,
        };
        if let Some(p) = path {
            d.set_path(p);
        }
        d
    }

    fn set_path(&mut self, p: PathBuf) {
        self.opts.delimiter = sniff_delimiter(&p);
        self.table_name = csv_io::table_name_from_path(&p);
        self.path = p.display().to_string();
        self.loaded_path_seen = self.path.clone();
    }

    fn key(&self) -> String {
        format!("{}|{}|{}|{}|{}", self.path, self.opts.delimiter, self.opts.quote, self.opts.has_header, self.opts.trim)
    }

    fn reload(&mut self) {
        self.loaded_key = self.key();
        self.report = None;
        if self.path.trim().is_empty() {
            self.data = None;
            return;
        }
        let path = std::path::PathBuf::from(self.path.trim());
        // A path typed by hand (no file chooser) still gets the same smarts.
        if !self.loaded_path_seen.eq(&self.path) && path.is_file() {
            self.loaded_path_seen = self.path.clone();
            let sniffed = sniff_delimiter(&path);
            if sniffed != self.opts.delimiter {
                self.opts.delimiter = sniffed;
                self.loaded_key = self.key();
            }
            if self.table_name.is_empty() {
                self.table_name = csv_io::table_name_from_path(&path);
            }
        }
        let res = read_csv(&path, &self.opts, None);
        if let Ok(d) = &res {
            self.col_names = d.headers.clone();
            self.col_types = infer_types(d).iter().map(|t| t.sql().to_string()).collect();
        }
        self.data = Some(res);
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub enum ExportSource {
    Table(String),
    Result(Vec<String>, Vec<Vec<Value>>),
}

pub struct ExportDialog {
    source: ExportSource,
    opts: ExportOptions,
    result: Option<Result<String, String>>,
}

impl ExportDialog {
    pub fn new(source: ExportSource) -> Self {
        ExportDialog { source, opts: ExportOptions::default(), result: None }
    }
}

// ---------------------------------------------------------------------------
// Create table
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ColDef {
    name: String,
    ty: String,
    pk: bool,
    ai: bool,
    nn: bool,
    unique: bool,
    default: String,
    check: String,
    collate: String,
}

impl ColDef {
    fn new(name: &str, ty: &str) -> Self {
        ColDef {
            name: name.into(),
            ty: ty.into(),
            pk: false,
            ai: false,
            nn: false,
            unique: false,
            default: String::new(),
            check: String::new(),
            collate: String::new(),
        }
    }
}

pub struct CreateTableDialog {
    name: String,
    cols: Vec<ColDef>,
    without_rowid: bool,
    strict: bool,
    if_not_exists: bool,
}

impl Default for CreateTableDialog {
    fn default() -> Self {
        let mut id = ColDef::new("id", "INTEGER");
        id.pk = true;
        id.ai = true;
        id.nn = true;
        CreateTableDialog {
            name: String::new(),
            cols: vec![id, ColDef::new("name", "TEXT")],
            without_rowid: false,
            strict: false,
            if_not_exists: false,
        }
    }
}

/// Quote a DEFAULT value unless it is already a literal/expression.
fn default_expr(s: &str) -> String {
    let t = s.trim();
    let up = t.to_ascii_uppercase();
    let is_number = t.parse::<f64>().is_ok();
    let is_literal = t.starts_with('\'') || t.starts_with('(') || t.starts_with("X'") || t.starts_with("x'");
    let is_keyword = matches!(up.as_str(), "NULL" | "TRUE" | "FALSE" | "CURRENT_TIME" | "CURRENT_DATE" | "CURRENT_TIMESTAMP");
    if is_number || is_literal || is_keyword {
        t.to_string()
    } else {
        format!("'{}'", t.replace('\'', "''"))
    }
}

impl CreateTableDialog {
    fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Enter a table name.".into());
        }
        if self.cols.is_empty() || self.cols.iter().any(|c| c.name.trim().is_empty()) {
            return Err("Every column needs a name.".into());
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.cols {
            if !seen.insert(c.name.trim().to_lowercase()) {
                return Err(format!("Duplicate column name '{}'.", c.name));
            }
        }
        let pks: Vec<&ColDef> = self.cols.iter().filter(|c| c.pk).collect();
        if self.cols.iter().any(|c| c.ai) {
            if pks.len() != 1 || !pks[0].ai || !pks[0].ty.eq_ignore_ascii_case("INTEGER") {
                return Err("AUTOINCREMENT needs exactly one primary key column of type INTEGER.".into());
            }
            if self.without_rowid {
                return Err("AUTOINCREMENT is not allowed on WITHOUT ROWID tables.".into());
            }
        }
        if self.without_rowid && pks.is_empty() {
            return Err("WITHOUT ROWID tables need a PRIMARY KEY.".into());
        }
        if self.strict {
            if let Some(c) = self.cols.iter().find(|c| !["INT", "INTEGER", "REAL", "TEXT", "BLOB", "ANY"].contains(&c.ty.trim().to_ascii_uppercase().as_str())) {
                return Err(format!("STRICT tables only allow INT, INTEGER, REAL, TEXT, BLOB or ANY (column '{}').", c.name));
            }
        }
        Ok(())
    }

    fn sql(&self) -> String {
        let pk_count = self.cols.iter().filter(|c| c.pk).count();
        let mut lines: Vec<String> = self
            .cols
            .iter()
            .map(|c| {
                let mut s = quote_ident(c.name.trim());
                if !c.ty.trim().is_empty() {
                    s.push(' ');
                    s.push_str(c.ty.trim());
                }
                if c.nn {
                    s.push_str(" NOT NULL");
                }
                if c.pk && pk_count == 1 {
                    s.push_str(" PRIMARY KEY");
                    if c.ai {
                        s.push_str(" AUTOINCREMENT");
                    }
                }
                if c.unique {
                    s.push_str(" UNIQUE");
                }
                if !c.default.trim().is_empty() {
                    s.push_str(&format!(" DEFAULT {}", default_expr(&c.default)));
                }
                if !c.check.trim().is_empty() {
                    s.push_str(&format!(" CHECK({})", c.check.trim()));
                }
                if !c.collate.is_empty() {
                    s.push_str(&format!(" COLLATE {}", c.collate));
                }
                s
            })
            .collect();
        if pk_count > 1 {
            let pks: Vec<String> = self.cols.iter().filter(|c| c.pk).map(|c| quote_ident(c.name.trim())).collect();
            lines.push(format!("PRIMARY KEY({})", pks.join(", ")));
        }
        let mut opts = vec![];
        if self.without_rowid {
            opts.push("WITHOUT ROWID");
        }
        if self.strict {
            opts.push("STRICT");
        }
        format!(
            "CREATE TABLE {}{} (\n\t{}\n){};",
            if self.if_not_exists { "IF NOT EXISTS " } else { "" },
            quote_ident(self.name.trim()),
            lines.join(",\n\t"),
            if opts.is_empty() { String::new() } else { format!(" {}", opts.join(", ")) }
        )
    }
}

// ---------------------------------------------------------------------------
// Create index
// ---------------------------------------------------------------------------

pub struct CreateIndexDialog {
    name: String,
    table: String,
    unique: bool,
    if_not_exists: bool,
    /// (column, selected, descending) in display order
    columns: Vec<(String, bool, bool)>,
    loaded_for: String,
    where_clause: String,
    preselect: Vec<String>,
}

impl CreateIndexDialog {
    pub fn new(table: Option<String>) -> Self {
        CreateIndexDialog {
            name: String::new(),
            table: table.unwrap_or_default(),
            unique: false,
            if_not_exists: false,
            columns: vec![],
            loaded_for: String::new(),
            where_clause: String::new(),
            preselect: vec![],
        }
    }

    /// Pre-filled from a query-plan suggestion.
    pub fn suggested(table: String, columns: Vec<String>) -> Self {
        let mut d = Self::new(Some(table));
        d.preselect = columns;
        d
    }

    fn sql(&self) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            .filter(|c| c.1)
            .map(|c| format!("{}{}", quote_ident(&c.0), if c.2 { " DESC" } else { "" }))
            .collect();
        let name = if self.name.trim().is_empty() {
            format!(
                "idx_{}_{}",
                self.table,
                self.columns.iter().filter(|c| c.1).map(|c| c.0.as_str()).collect::<Vec<_>>().join("_")
            )
        } else {
            self.name.trim().to_string()
        };
        let mut s = format!(
            "CREATE {}INDEX {}{} ON {} ({})",
            if self.unique { "UNIQUE " } else { "" },
            if self.if_not_exists { "IF NOT EXISTS " } else { "" },
            quote_ident(&name),
            quote_ident(&self.table),
            cols.join(", ")
        );
        if !self.where_clause.trim().is_empty() {
            s.push_str(&format!(" WHERE {}", self.where_clause.trim()));
        }
        s.push(';');
        s
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl App {
    pub fn dialogs(&mut self, ctx: &Context) {
        if let Some(mut d) = self.import.take() {
            if self.import_window(ctx, &mut d) {
                self.import = Some(d);
            }
        }
        if let Some(mut d) = self.export.take() {
            if self.export_window(ctx, &mut d) {
                self.export = Some(d);
            }
        }
        if let Some(mut d) = self.create_table.take() {
            if self.create_table_window(ctx, &mut d) {
                self.create_table = Some(d);
            }
        }
        if let Some(mut d) = self.create_index.take() {
            if self.create_index_window(ctx, &mut d) {
                self.create_index = Some(d);
            }
        }
        if let Some(mut d) = self.mock.take() {
            if self.mock_window(ctx, &mut d) {
                self.mock = Some(d);
            }
        }
    }

    /// Returns false when the window should close.
    fn import_window(&mut self, ctx: &Context, d: &mut ImportDialog) -> bool {
        let mut open = true;
        let mut close = false;
        let tables = self.tables(false);
        egui::Window::new("Import CSV file")
            .open(&mut open)
            .default_size([820.0, 600.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let p = palette(ui);
                ui.horizontal(|ui| {
                    ui.label("File:");
                    ui.add(egui::TextEdit::singleline(&mut d.path).desired_width(ui.available_width() - 90.0));
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("CSV / TSV", &["csv", "tsv", "txt"]).pick_file() {
                            d.set_path(p);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Separator");
                    delimiter_picker(ui, "imp_delim", &mut d.opts.delimiter);
                    ui.label("Quote");
                    quote_picker(ui, "imp_quote", &mut d.opts.quote);
                    ui.checkbox(&mut d.opts.has_header, "First row is header");
                    ui.checkbox(&mut d.opts.trim, "Trim fields");
                    ui.checkbox(&mut d.opts.empty_as_null, "Empty → NULL");
                });
                if d.key() != d.loaded_key {
                    d.reload();
                }

                let data = match &d.data {
                    None => {
                        ui.label(RichText::new("Choose a CSV file to preview it.").weak());
                        return;
                    }
                    Some(Err(e)) => {
                        ui.colored_label(p.err, e);
                        return;
                    }
                    Some(Ok(data)) => data,
                };

                ui.separator();
                ui.horizontal(|ui| {
                    ui.radio_value(&mut d.new_table, true, "Create new table");
                    ui.radio_value(&mut d.new_table, false, "Append to existing table");
                });
                if d.new_table {
                    ui.horizontal(|ui| {
                        ui.label("Table name:");
                        ui.text_edit_singleline(&mut d.table_name);
                        ui.checkbox(&mut d.strict, "STRICT");
                        if ui.button("All columns TEXT").on_hover_text("Store every value exactly as written in the file").clicked() {
                            d.col_types.iter_mut().for_each(|t| *t = "TEXT".into());
                        }
                        if ui.button("Re-detect types").clicked() {
                            d.col_types = infer_types(data).iter().map(|t| t.sql().to_string()).collect();
                        }
                    });
                } else {
                    if d.existing.is_empty() || !tables.contains(&d.existing) {
                        d.existing = tables.first().cloned().unwrap_or_default();
                    }
                    ui.horizontal(|ui| {
                        ui.label("Table:");
                        egui::ComboBox::from_id_salt("imp_existing").selected_text(&d.existing).width(220.0).show_ui(ui, |ui| {
                            for t in &tables {
                                ui.selectable_value(&mut d.existing, t.clone(), t);
                            }
                        });
                        ui.label(
                            RichText::new("Columns are matched by header name when all headers match, otherwise by position.")
                                .small()
                                .weak(),
                        );
                    });
                }
                ui.horizontal(|ui| {
                    ui.label("If a value doesn't fit a STRICT column:");
                    ui.radio_value(&mut d.on_error, OnError::Abort, "Abort import");
                    ui.radio_value(&mut d.on_error, OnError::SkipRow, "Skip row");
                    ui.radio_value(&mut d.on_error, OnError::UseNull, "Store NULL");
                });

                ui.label(
                    RichText::new(format!(
                        "{} columns · {} rows{}",
                        data.headers.len(),
                        data.total_rows,
                        if d.new_table { " · types detected from all rows (leading zeros, huge IDs and mixed values stay TEXT)" } else { "" }
                    ))
                    .small()
                    .weak(),
                );

                // preview grid with editable header + type for new tables
                let preview_rows = data.rows.len().min(100);
                egui::ScrollArea::both().max_height(ui.available_height() - 120.0).show(ui, |ui| {
                    egui::Grid::new("imp_preview").striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                        for (i, h) in data.headers.iter().enumerate() {
                            ui.vertical(|ui| {
                                if d.new_table {
                                    ui.add_sized([110.0, 22.0], egui::TextEdit::singleline(&mut d.col_names[i]));
                                    egui::ComboBox::from_id_salt(("imp_ty", i))
                                        .selected_text(if d.col_types[i].is_empty() { "(none)" } else { &d.col_types[i] })
                                        .width(110.0)
                                        .show_ui(ui, |ui| {
                                            for t in TYPES {
                                                ui.selectable_value(&mut d.col_types[i], t.to_string(), if t.is_empty() { "(none)" } else { t });
                                            }
                                        });
                                } else {
                                    ui.label(RichText::new(h).strong());
                                }
                            });
                        }
                        ui.end_row();
                        for r in &data.rows[..preview_rows] {
                            for v in r {
                                let shown: String = v.chars().take(40).collect();
                                if v.is_empty() {
                                    ui.label(RichText::new(if d.opts.empty_as_null { "NULL" } else { "" }).italics().weak());
                                } else {
                                    ui.label(shown);
                                }
                            }
                            ui.end_row();
                        }
                    });
                });

                if d.new_table {
                    let cols: Vec<(String, String)> = d.col_names.iter().cloned().zip(d.col_types.iter().cloned()).collect();
                    ui.collapsing("SQL that will run", |ui| {
                        sql_view(ui, &create_table_sql(&d.table_name, &cols, d.strict));
                    });
                }

                if let Some(rep) = &d.report {
                    match rep {
                        Ok(r) => {
                            ui.colored_label(
                                p.ok,
                                format!(
                                    "Imported {} row(s) in {:.0} ms{}{}. Use Write Changes to save.",
                                    r.inserted,
                                    r.millis,
                                    if r.skipped > 0 { format!(", skipped {}", r.skipped) } else { String::new() },
                                    if r.nulled > 0 { format!(", {} value(s) stored as NULL", r.nulled) } else { String::new() },
                                ),
                            );
                            if !r.problems.is_empty() {
                                egui::ScrollArea::vertical().id_salt("imp_problems").max_height(80.0).show(ui, |ui| {
                                    for pr in &r.problems {
                                        ui.label(RichText::new(pr).small().color(p.warn));
                                    }
                                });
                            }
                        }
                        Err(e) => {
                            ui.colored_label(p.err, e);
                        }
                    }
                }

                ui.horizontal(|ui| {
                    let done = matches!(d.report, Some(Ok(_)));
                    if ui.add_enabled(!done, egui::Button::new("Import")).clicked() {
                        let target = if d.new_table {
                            ImportTarget::NewTable {
                                name: d.table_name.trim().to_string(),
                                columns: d.col_names.iter().cloned().zip(d.col_types.iter().cloned()).collect(),
                                strict: d.strict,
                            }
                        } else {
                            ImportTarget::Existing { name: d.existing.clone() }
                        };
                        let res = match self.db.as_mut() {
                            Some(db) => import_csv(db, data, &target, &d.opts, d.on_error),
                            None => Err("No database open.".into()),
                        };
                        if res.is_ok() {
                            let name = match &target {
                                ImportTarget::NewTable { name, .. } | ImportTarget::Existing { name } => name.clone(),
                            };
                            self.invalidate();
                            self.browse.select_table(name);
                        }
                        d.report = Some(res);
                    }
                    if ui.button(if done { "Close" } else { "Cancel" }).clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn export_window(&mut self, ctx: &Context, d: &mut ExportDialog) -> bool {
        let mut open = true;
        let mut close = false;
        let tables = self.tables(true);
        egui::Window::new("Export to CSV")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                let p = palette(ui);
                match &mut d.source {
                    ExportSource::Table(t) => {
                        ui.horizontal(|ui| {
                            ui.label("Table / view:");
                            egui::ComboBox::from_id_salt("exp_table").selected_text(t.as_str()).width(220.0).show_ui(ui, |ui| {
                                for name in &tables {
                                    ui.selectable_value(t, name.clone(), name);
                                }
                            });
                        });
                    }
                    ExportSource::Result(cols, rows) => {
                        ui.label(format!("Query result: {} columns, {} rows", cols.len(), rows.len()));
                    }
                }
                egui::Grid::new("exp_opts").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Separator");
                    ui.horizontal(|ui| delimiter_picker(ui, "exp_delim", &mut d.opts.delimiter));
                    ui.end_row();
                    ui.label("Quote");
                    ui.horizontal(|ui| {
                        quote_picker(ui, "exp_quote", &mut d.opts.quote);
                        ui.checkbox(&mut d.opts.quote_all, "Quote all fields");
                    });
                    ui.end_row();
                    ui.label("Header");
                    ui.checkbox(&mut d.opts.header, "Column names in first row");
                    ui.end_row();
                    ui.label("NULL as");
                    ui.add(egui::TextEdit::singleline(&mut d.opts.null_text).hint_text("(empty)").desired_width(100.0));
                    ui.end_row();
                    ui.label("Line ending");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.opts.crlf, false, "LF (Unix)");
                        ui.radio_value(&mut d.opts.crlf, true, "CRLF (Windows)");
                    });
                    ui.end_row();
                });
                ui.label(RichText::new("Reals are written with full precision; non-UTF-8 BLOBs as 0x-hex.").small().weak());
                if let Some(r) = &d.result {
                    match r {
                        Ok(m) => ui.colored_label(p.ok, m),
                        Err(e) => ui.colored_label(p.err, e),
                    };
                }
                ui.horizontal(|ui| {
                    if ui.button("Save…").clicked() {
                        let default_name = match &d.source {
                            ExportSource::Table(t) => format!("{t}.csv"),
                            ExportSource::Result(..) => "query_result.csv".into(),
                        };
                        if let Some(path) = rfd::FileDialog::new().set_file_name(default_name).add_filter("CSV", &["csv", "tsv", "txt"]).save_file() {
                            let res = match (&d.source, self.db.as_mut()) {
                                (ExportSource::Table(t), Some(db)) => csv_io::export_table(db, t, &path, &d.opts),
                                (ExportSource::Result(c, r), _) => csv_io::write_csv(&path, c, r, &d.opts),
                                _ => Err("No database open.".into()),
                            };
                            d.result = Some(res.map(|n| format!("Wrote {n} row(s) to {}", path.display())));
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn create_table_window(&mut self, ctx: &Context, d: &mut CreateTableDialog) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new("Create Table")
            .open(&mut open)
            .default_size([760.0, 480.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let p = palette(ui);
                ui.horizontal(|ui| {
                    ui.label("Table name:");
                    ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("my_table").desired_width(200.0));
                    ui.checkbox(&mut d.without_rowid, "WITHOUT ROWID");
                    ui.checkbox(&mut d.strict, "STRICT");
                    ui.checkbox(&mut d.if_not_exists, "IF NOT EXISTS");
                });
                ui.separator();
                let mut remove: Option<usize> = None;
                let mut swap: Option<(usize, usize)> = None;
                let n = d.cols.len();
                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                    egui::Grid::new("ct_cols").striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
                        for h in ["Name", "Type", "NN", "PK", "AI", "U", "Default", "Check", "Collate", ""] {
                            ui.label(RichText::new(h).weak()).on_hover_text(match h {
                                "NN" => "NOT NULL",
                                "PK" => "PRIMARY KEY",
                                "AI" => "AUTOINCREMENT",
                                "U" => "UNIQUE",
                                _ => h,
                            });
                        }
                        ui.end_row();
                        for (i, c) in d.cols.iter_mut().enumerate() {
                            ui.add_sized([150.0, 22.0], egui::TextEdit::singleline(&mut c.name));
                            egui::ComboBox::from_id_salt(("ct_ty", i))
                                .selected_text(if c.ty.is_empty() { "(none)" } else { &c.ty })
                                .width(100.0)
                                .show_ui(ui, |ui| {
                                    for t in TYPES {
                                        ui.selectable_value(&mut c.ty, t.to_string(), if t.is_empty() { "(none)" } else { t });
                                    }
                                });
                            ui.checkbox(&mut c.nn, "");
                            ui.checkbox(&mut c.pk, "");
                            ui.add_enabled(c.pk, egui::Checkbox::without_text(&mut c.ai));
                            if !c.pk {
                                c.ai = false;
                            }
                            ui.checkbox(&mut c.unique, "");
                            ui.add_sized([110.0, 22.0], egui::TextEdit::singleline(&mut c.default).hint_text("value"));
                            ui.add_sized([150.0, 22.0], egui::TextEdit::singleline(&mut c.check).hint_text("expression"));
                            egui::ComboBox::from_id_salt(("ct_coll", i))
                                .selected_text(if c.collate.is_empty() { "(default)" } else { &c.collate })
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut c.collate, String::new(), "(default)");
                                    for n in ["BINARY", "NOCASE", "RTRIM"] {
                                        ui.selectable_value(&mut c.collate, n.to_string(), n);
                                    }
                                    ui.separator();
                                    for (n, desc) in crate::collate::BUILTINS {
                                        ui.selectable_value(&mut c.collate, n.to_string(), n).on_hover_text(desc);
                                    }
                                    let user = crate::collate::user_defs();
                                    if !user.is_empty() {
                                        ui.separator();
                                    }
                                    for d in user {
                                        ui.selectable_value(&mut c.collate, d.name.clone(), d.name);
                                    }
                                });
                            ui.horizontal(|ui| {
                                if ui.add_enabled(i > 0, egui::Button::new("▲").small()).clicked() {
                                    swap = Some((i, i - 1));
                                }
                                if ui.add_enabled(i + 1 < n, egui::Button::new("▼").small()).clicked() {
                                    swap = Some((i, i + 1));
                                }
                                if ui.add(egui::Button::new("✖").small()).clicked() {
                                    remove = Some(i);
                                }
                            });
                            ui.end_row();
                        }
                    });
                });
                if let Some(i) = remove {
                    d.cols.remove(i);
                }
                if let Some((a, b)) = swap {
                    d.cols.swap(a, b);
                }
                if ui.button("➕ Add field").clicked() {
                    let n = d.cols.len() + 1;
                    d.cols.push(ColDef::new(&format!("field{n}"), "TEXT"));
                }
                ui.separator();
                ui.label(RichText::new("SQL").weak());
                sql_view(ui, &d.sql());
                let valid = d.validate();
                if let Err(e) = &valid {
                    ui.colored_label(p.warn, e);
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid.is_ok(), egui::Button::new("Create")).clicked() {
                        let sql = d.sql();
                        if let Some(db) = self.db.as_mut() {
                            match db.execute_edit(&sql, &[]) {
                                Ok(_) => {
                                    let name = d.name.trim().to_string();
                                    self.notify(format!("Created table {name}."));
                                    self.invalidate();
                                    self.structure.selected = Some((crate::db::ObjKind::Table, name));
                                    close = true;
                                }
                                Err(e) => self.errors.push(e),
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn create_index_window(&mut self, ctx: &Context, d: &mut CreateIndexDialog) -> bool {
        let mut open = true;
        let mut close = false;
        let tables = self.tables(false);
        if d.table.is_empty() || !tables.contains(&d.table) {
            d.table = tables.first().cloned().unwrap_or_default();
        }
        if d.loaded_for != d.table {
            d.loaded_for = d.table.clone();
            d.columns = self
                .db
                .as_mut()
                .and_then(|db| db.table_details(&d.table).ok())
                .map(|t| t.columns.into_iter().map(|c| (c.name, false, false)).collect())
                .unwrap_or_default();
            // suggested columns first, in suggestion order, ticked
            for (k, want) in d.preselect.iter().enumerate() {
                if let Some(i) = d.columns.iter().position(|c| c.0.eq_ignore_ascii_case(want)) {
                    let mut c = d.columns.remove(i);
                    c.1 = true;
                    d.columns.insert(k.min(d.columns.len()), c);
                }
            }
        }
        egui::Window::new("Create Index")
            .open(&mut open)
            .default_width(520.0)
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("ci_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Index name");
                    ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("(auto)"));
                    ui.end_row();
                    ui.label("Table");
                    egui::ComboBox::from_id_salt("ci_table").selected_text(&d.table).width(220.0).show_ui(ui, |ui| {
                        for t in &tables {
                            ui.selectable_value(&mut d.table, t.clone(), t);
                        }
                    });
                    ui.end_row();
                    ui.label("Options");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut d.unique, "UNIQUE");
                        ui.checkbox(&mut d.if_not_exists, "IF NOT EXISTS");
                    });
                    ui.end_row();
                });
                ui.label(RichText::new("Columns (in key order)").weak());
                let mut swap = None;
                let n = d.columns.len();
                for (i, c) in d.columns.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut c.1, &c.0);
                        ui.add_enabled(c.1, egui::Checkbox::new(&mut c.2, "DESC"));
                        if ui.add_enabled(i > 0, egui::Button::new("▲").small()).clicked() {
                            swap = Some((i, i - 1));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("▼").small()).clicked() {
                            swap = Some((i, i + 1));
                        }
                    });
                }
                if let Some((a, b)) = swap {
                    d.columns.swap(a, b);
                }
                ui.horizontal(|ui| {
                    ui.label("Partial index WHERE");
                    ui.add(egui::TextEdit::singleline(&mut d.where_clause).hint_text("optional").desired_width(260.0));
                });
                ui.separator();
                sql_view(ui, &d.sql());
                let any = d.columns.iter().any(|c| c.1);
                ui.horizontal(|ui| {
                    if ui.add_enabled(any, egui::Button::new("Create")).clicked() {
                        let sql = d.sql();
                        if let Some(db) = self.db.as_mut() {
                            match db.execute_edit(&sql, &[]) {
                                Ok(_) => {
                                    self.notify("Index created.");
                                    self.invalidate();
                                    close = true;
                                }
                                Err(e) => self.errors.push(e),
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_sql_composite_pk_and_defaults() {
        let mut d = CreateTableDialog { name: "orders".into(), ..Default::default() };
        d.cols = vec![ColDef::new("a", "INTEGER"), ColDef::new("b", "TEXT"), ColDef::new("note", "TEXT")];
        d.cols[0].pk = true;
        d.cols[1].pk = true;
        d.cols[2].default = "it's".into();
        d.without_rowid = true;
        assert!(d.validate().is_ok());
        let sql = d.sql();
        assert!(sql.contains("PRIMARY KEY(\"a\", \"b\")"), "{sql}");
        assert!(sql.contains("DEFAULT 'it''s'"));
        assert!(sql.ends_with(" WITHOUT ROWID;"));
        let mut db = crate::db::Database::open_in_memory().unwrap();
        db.execute_edit(&sql, &[]).unwrap();
    }

    #[test]
    fn autoincrement_validation() {
        let mut d = CreateTableDialog { name: "t".into(), ..Default::default() };
        assert!(d.validate().is_ok());
        d.cols[0].ty = "TEXT".into();
        assert!(d.validate().is_err());
    }
}

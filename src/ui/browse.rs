//! Browse Data tab: paged grid with per-column filters, sorting, an
//! "Edit Database Cell" dock, and add/delete record. Every edit becomes a
//! parameterised UPDATE/INSERT/DELETE that shows up in the SQL log.

use egui::{RichText, Ui};
use rusqlite::types::Value;

use crate::app::App;
use crate::db::{as_text, quote_ident, type_name, TableDetails};
use crate::ui::dialogs::{ExportDialog, ExportSource};
use crate::ui::widgets::{data_grid, palette, GridArgs};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellType {
    Text,
    Integer,
    Real,
    Null,
}

pub struct BrowseState {
    pub table: Option<String>,
    pub details: Option<TableDetails>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Row identity (rowid or primary-key values) for each loaded row.
    pub keys: Vec<Vec<Value>>,
    pub key_cols: Vec<String>,
    pub filters: Vec<String>,
    pub sort: Option<(usize, bool)>,
    pub page: usize,
    pub page_size: usize,
    pub total: i64,
    pub selected: Option<(usize, usize)>,
    pub dirty: bool,
    pub error: Option<String>,
    // cell editor
    pub edit_text: String,
    pub edit_type: CellType,
    pub edit_for: Option<(usize, usize)>,
}

impl Default for BrowseState {
    fn default() -> Self {
        BrowseState {
            table: None,
            details: None,
            columns: vec![],
            rows: vec![],
            keys: vec![],
            key_cols: vec![],
            filters: vec![],
            sort: None,
            page: 0,
            page_size: 500,
            total: 0,
            selected: None,
            dirty: true,
            error: None,
            edit_text: String::new(),
            edit_type: CellType::Text,
            edit_for: None,
        }
    }
}

impl BrowseState {
    pub fn select_table(&mut self, name: String) {
        if self.table.as_deref() != Some(&name) {
            let ps = self.page_size;
            *self = BrowseState { page_size: ps, ..Default::default() };
            self.table = Some(name);
        }
        self.dirty = true;
    }

    fn editable(&self) -> bool {
        !self.key_cols.is_empty()
    }

    /// WHERE clause + parameters from the per-column filter boxes.
    fn where_clause(&self) -> (String, Vec<Value>) {
        let mut conds = Vec::new();
        let mut params = Vec::new();
        for (i, f) in self.filters.iter().enumerate() {
            let f = f.trim();
            if f.is_empty() || i >= self.columns.len() {
                continue;
            }
            let col = quote_ident(&self.columns[i]);
            let ops = [">=", "<=", "<>", "!=", "=", ">", "<"];
            if let Some(op) = ops.iter().find(|op| f.starts_with(*op)) {
                let rhs = f[op.len()..].trim();
                if rhs.eq_ignore_ascii_case("NULL") {
                    conds.push(format!("{col} IS {}NULL", if *op == "=" { "" } else { "NOT " }));
                    continue;
                }
                let v = if let Ok(i) = rhs.parse::<i64>() {
                    Value::Integer(i)
                } else if let Ok(r) = rhs.parse::<f64>() {
                    Value::Real(r)
                } else {
                    Value::Text(rhs.trim_matches('\'').to_string())
                };
                params.push(v);
                conds.push(format!("{col} {op} ?{}", params.len()));
            } else {
                let esc = f.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                params.push(Value::Text(format!("%{esc}%")));
                conds.push(format!("{col} LIKE ?{} ESCAPE '\\'", params.len()));
            }
        }
        if conds.is_empty() {
            (String::new(), params)
        } else {
            (format!(" WHERE {}", conds.join(" AND ")), params)
        }
    }

    fn key_where(&self, row: usize, first_param: usize) -> (String, Vec<Value>) {
        let mut conds = Vec::new();
        let mut params = Vec::new();
        for (i, k) in self.key_cols.iter().enumerate() {
            params.push(self.keys[row][i].clone());
            conds.push(format!("{k} = ?{}", first_param + i));
        }
        (conds.join(" AND "), params)
    }
}

impl App {
    fn load_browse(&mut self) {
        self.browse.dirty = false;
        self.browse.error = None;
        let Some(table) = self.browse.table.clone() else { return };
        let Some(db) = self.db.as_mut() else { return };

        let details = match db.table_details(&table) {
            Ok(d) => d,
            Err(e) => {
                self.browse.error = Some(e);
                return;
            }
        };
        self.browse.key_cols = if details.is_view {
            vec![]
        } else if details.has_rowid() {
            vec!["_rowid_".into()]
        } else {
            details.pk_columns().iter().map(|c| quote_ident(c)).collect()
        };
        // Column names first (cheap) so filters line up even with 0 rows.
        if self.browse.columns.is_empty() || self.browse.details.is_none() {
            self.browse.columns = details.columns.iter().map(|c| c.name.clone()).collect();
        }
        self.browse.details = Some(details);

        let (where_sql, params) = self.browse.where_clause();
        self.browse.total = db.row_count(&table, &where_sql, &params).unwrap_or(0);
        let max_page = ((self.browse.total.max(1) as usize - 1) / self.browse.page_size).max(0);
        self.browse.page = self.browse.page.min(max_page);

        let nk = self.browse.key_cols.len();
        let key_sel: String = self
            .browse
            .key_cols
            .iter()
            .enumerate()
            .map(|(i, k)| format!("{k} AS \"__qk{i}\", "))
            .collect();
        let order = match self.browse.sort {
            Some((c, asc)) if c < self.browse.columns.len() => {
                format!(" ORDER BY {} {}", quote_ident(&self.browse.columns[c]), if asc { "ASC" } else { "DESC" })
            }
            _ => String::new(),
        };
        let sql = format!(
            "SELECT {key_sel}* FROM {}{where_sql}{order} LIMIT {} OFFSET {};",
            quote_ident(&table),
            self.browse.page_size,
            self.browse.page * self.browse.page_size
        );
        match db.query(crate::db::LogSource::App, &sql, &params) {
            Ok(r) => {
                self.browse.columns = r.columns[nk..].to_vec();
                self.browse.keys = r.rows.iter().map(|row| row[..nk].to_vec()).collect();
                self.browse.rows = r.rows.into_iter().map(|row| row[nk..].to_vec()).collect();
                if self.browse.selected.is_some_and(|(r, c)| r >= self.browse.rows.len() || c >= self.browse.columns.len()) {
                    self.browse.selected = None;
                }
                self.browse.edit_for = None;
            }
            Err(e) => {
                self.browse.error = Some(e);
                self.browse.rows.clear();
                self.browse.keys.clear();
            }
        }
    }

    pub fn browse_tab(&mut self, ui: &mut Ui) {
        let tables = self.tables(true);
        if self.browse.table.as_ref().is_none_or(|t| !tables.contains(t)) {
            let preferred = self.structure.selected_table(&self.schema).filter(|t| tables.contains(t));
            match preferred.or_else(|| tables.first().cloned()) {
                Some(t) => self.browse.select_table(t),
                None => {
                    ui.label(RichText::new("This database has no tables yet. Create one from the Database Structure tab or import a CSV.").weak());
                    return;
                }
            }
        }
        if self.browse.dirty {
            self.load_browse();
        }
        let read_only = self.db.as_ref().is_some_and(|d| d.read_only);
        let p = palette(ui);

        // ---- toolbar
        ui.horizontal(|ui| {
            ui.label("Table:");
            let current = self.browse.table.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt("browse_table")
                .selected_text(&current)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for t in &tables {
                        if ui.selectable_label(&current == t, t).clicked() {
                            self.browse.select_table(t.clone());
                        }
                    }
                });
            if ui.button("⟳").on_hover_text("Refresh").clicked() {
                self.browse.dirty = true;
            }
            if ui.button("Clear filters").clicked() {
                self.browse.filters.iter_mut().for_each(|f| f.clear());
                self.browse.sort = None;
                self.browse.dirty = true;
            }
            ui.separator();
            let can_edit = self.browse.editable() && !read_only;
            if ui.add_enabled(can_edit, egui::Button::new("➕ New Record")).clicked() {
                self.insert_record();
            }
            let sel_row = self.browse.selected.map(|(r, _)| r);
            if ui.add_enabled(can_edit && sel_row.is_some(), egui::Button::new("➖ Delete Record")).clicked() {
                self.delete_record(sel_row.unwrap());
            }
            if ui.add_enabled(can_edit, egui::Button::new("🎲 Mock Data")).clicked() {
                self.open_mock(self.browse.table.clone());
            }
            ui.separator();
            if ui.button("Export CSV…").clicked() {
                self.export = Some(ExportDialog::new(ExportSource::Table(current.clone())));
            }
            if !self.browse.editable() {
                ui.label(RichText::new("read-only (view)").small().color(p.warn));
            }
        });

        // ---- pager
        let total = self.browse.total;
        let pages = ((total.max(1) as usize - 1) / self.browse.page_size) + 1;
        ui.horizontal(|ui| {
            let start = if total == 0 { 0 } else { self.browse.page * self.browse.page_size + 1 };
            let end = (self.browse.page * self.browse.page_size + self.browse.rows.len()).min(total as usize);
            ui.label(RichText::new(format!("{start} – {end} of {total} rows")).small());
            if ui.add_enabled(self.browse.page > 0, egui::Button::new("⏮").small()).clicked() {
                self.browse.page = 0;
                self.browse.dirty = true;
            }
            if ui.add_enabled(self.browse.page > 0, egui::Button::new("◀").small()).clicked() {
                self.browse.page -= 1;
                self.browse.dirty = true;
            }
            ui.label(RichText::new(format!("page {} / {}", self.browse.page + 1, pages)).small());
            if ui.add_enabled(self.browse.page + 1 < pages, egui::Button::new("▶").small()).clicked() {
                self.browse.page += 1;
                self.browse.dirty = true;
            }
            if ui.add_enabled(self.browse.page + 1 < pages, egui::Button::new("⏭").small()).clicked() {
                self.browse.page = pages - 1;
                self.browse.dirty = true;
            }
            ui.label(RichText::new("rows/page").small().weak());
            let mut ps = self.browse.page_size;
            egui::ComboBox::from_id_salt("page_size").selected_text(ps.to_string()).width(70.0).show_ui(ui, |ui| {
                for n in [100, 500, 1000, 5000] {
                    ui.selectable_value(&mut ps, n, n.to_string());
                }
            });
            if ps != self.browse.page_size {
                self.browse.page_size = ps;
                self.browse.page = 0;
                self.browse.dirty = true;
            }
        });
        if let Some(e) = &self.browse.error {
            ui.colored_label(p.err, e);
        }

        // ---- cell editor dock
        egui::Panel::right("cell_editor").resizable(true).default_size(280.0).min_size(200.0).show(ui, |ui| {
            self.cell_editor(ui, read_only);
        });

        // ---- grid
        egui::CentralPanel::default_margins().show(ui, |ui| {
            let id = format!("browse_{}_{}", self.browse.table.clone().unwrap_or_default(), self.browse.columns.len());
            let first = self.browse.page * self.browse.page_size + 1;
            let columns = self.browse.columns.clone();
            let r = data_grid(
                ui,
                GridArgs {
                    id,
                    columns: &columns,
                    rows: &self.browse.rows,
                    first_row_number: first,
                    selected: self.browse.selected,
                    sort: self.browse.sort,
                    filters: Some(&mut self.browse.filters),
                },
            );
            if let Some(c) = r.header_clicked {
                self.browse.sort = match self.browse.sort {
                    Some((sc, true)) if sc == c => Some((c, false)),
                    Some((sc, false)) if sc == c => None,
                    _ => Some((c, true)),
                };
                self.browse.dirty = true;
            }
            if r.filters_changed {
                self.browse.page = 0;
                self.browse.dirty = true;
            }
            if let Some(cell) = r.clicked.or(r.double_clicked) {
                self.browse.selected = Some(cell);
            }
            if let Some(v) = r.copy_value {
                ui.ctx().copy_text(v);
            }
        });
    }

    fn cell_editor(&mut self, ui: &mut Ui, read_only: bool) {
        let p = palette(ui);
        ui.label(RichText::new("Edit Database Cell").strong());
        ui.separator();
        let Some((r, c)) = self.browse.selected else {
            ui.label(RichText::new("Select a cell to view or edit its value.").weak());
            return;
        };
        if r >= self.browse.rows.len() || c >= self.browse.columns.len() {
            return;
        }
        let value = self.browse.rows[r][c].clone();
        if self.browse.edit_for != Some((r, c)) {
            self.browse.edit_for = Some((r, c));
            self.browse.edit_text = match &value {
                Value::Blob(b) => crate::db::hex(b),
                v => as_text(v),
            };
            self.browse.edit_type = match value {
                Value::Null => CellType::Null,
                Value::Integer(_) => CellType::Integer,
                Value::Real(_) => CellType::Real,
                _ => CellType::Text,
            };
        }
        let col = self.browse.columns[c].clone();
        let decl = self
            .browse
            .details
            .as_ref()
            .and_then(|d| d.columns.iter().find(|x| x.name == col))
            .map(|x| x.decl_type.clone())
            .unwrap_or_default();
        ui.label(RichText::new(format!("{col}  ·  declared {}", if decl.is_empty() { "—" } else { &decl })).small());
        ui.label(RichText::new(format!("stored as {}", type_name(&value))).small().weak());

        let is_blob = matches!(value, Value::Blob(_));
        let editable = self.browse.editable() && !read_only && !is_blob;
        ui.horizontal(|ui| {
            ui.label("Mode:");
            ui.add_enabled_ui(editable, |ui| {
                ui.selectable_value(&mut self.browse.edit_type, CellType::Text, "Text");
                ui.selectable_value(&mut self.browse.edit_type, CellType::Integer, "Integer");
                ui.selectable_value(&mut self.browse.edit_type, CellType::Real, "Real");
                ui.selectable_value(&mut self.browse.edit_type, CellType::Null, "NULL");
            });
        });
        if is_blob {
            ui.label(RichText::new("Binary value shown as hex (read-only here).").small().color(p.warn));
        }
        let enabled = editable && self.browse.edit_type != CellType::Null;
        let avail = ui.available_height() - 40.0;
        egui::ScrollArea::vertical().max_height(avail.max(80.0)).show(ui, |ui| {
            ui.add_enabled(
                enabled,
                egui::TextEdit::multiline(&mut self.browse.edit_text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(10),
            );
        });
        ui.horizontal(|ui| {
            if ui.add_enabled(editable, egui::Button::new("Apply")).clicked() {
                let new = match self.browse.edit_type {
                    CellType::Null => Ok(Value::Null),
                    CellType::Text => Ok(Value::Text(self.browse.edit_text.clone())),
                    CellType::Integer => self
                        .browse
                        .edit_text
                        .trim()
                        .parse::<i64>()
                        .map(Value::Integer)
                        .map_err(|_| "Not a valid integer.".to_string()),
                    CellType::Real => self
                        .browse
                        .edit_text
                        .trim()
                        .parse::<f64>()
                        .map(Value::Real)
                        .map_err(|_| "Not a valid real number.".to_string()),
                };
                match new {
                    Ok(v) => self.update_cell(r, c, v),
                    Err(e) => self.error(e),
                }
            }
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(self.browse.edit_text.clone());
            }
        });
    }

    fn update_cell(&mut self, r: usize, c: usize, v: Value) {
        let table = self.browse.table.clone().unwrap_or_default();
        let (kw, mut params) = self.browse.key_where(r, 2);
        params.insert(0, v.clone());
        let sql = format!(
            "UPDATE {} SET {} = ?1 WHERE {};",
            quote_ident(&table),
            quote_ident(&self.browse.columns[c]),
            kw
        );
        let Some(db) = self.db.as_mut() else { return };
        match db.execute_edit(&sql, &params) {
            Ok(_) => {
                self.browse.rows[r][c] = v;
                // a PK/rowid change invalidates our key; just reload
                self.browse.dirty = true;
                self.browse.edit_for = None;
            }
            Err(e) => self.error(e),
        }
    }

    fn insert_record(&mut self) {
        let table = self.browse.table.clone().unwrap_or_default();
        let Some(db) = self.db.as_mut() else { return };
        match db.execute_edit(&format!("INSERT INTO {} DEFAULT VALUES;", quote_ident(&table)), &[]) {
            Ok(_) => {
                // jump to the end where the new row will be
                self.browse.sort = None;
                self.browse.filters.iter_mut().for_each(|f| f.clear());
                self.browse.total += 1;
                self.browse.page = (self.browse.total.max(1) as usize - 1) / self.browse.page_size;
                self.browse.dirty = true;
                self.load_browse();
                if !self.browse.rows.is_empty() {
                    self.browse.selected = Some((self.browse.rows.len() - 1, 0));
                }
            }
            Err(e) => self.error(format!("{e}\n\nTip: columns declared NOT NULL without a DEFAULT need a value; use Execute SQL for this table.")),
        }
    }

    fn delete_record(&mut self, r: usize) {
        if r >= self.browse.keys.len() {
            return;
        }
        let table = self.browse.table.clone().unwrap_or_default();
        let (kw, params) = self.browse.key_where(r, 1);
        let Some(db) = self.db.as_mut() else { return };
        match db.execute_edit(&format!("DELETE FROM {} WHERE {};", quote_ident(&table), kw), &params) {
            Ok(_) => {
                self.browse.selected = None;
                self.browse.dirty = true;
            }
            Err(e) => self.error(e),
        }
    }
}

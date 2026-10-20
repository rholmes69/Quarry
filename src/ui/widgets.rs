//! Shared widgets: SQL syntax highlighting and the data grid used by both
//! Browse Data and Execute SQL.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use rusqlite::types::Value;

use crate::db::display_value;

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

pub struct Palette {
    pub keyword: Color32,
    pub string: Color32,
    pub ident: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub null: Color32,
    pub blob: Color32,
    pub ok: Color32,
    pub err: Color32,
    pub warn: Color32,
    pub app: Color32,
    pub user: Color32,
}

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette {
            keyword: Color32::from_rgb(0x7a, 0xb8, 0xff),
            string: Color32::from_rgb(0xe6, 0xb4, 0x73),
            ident: Color32::from_rgb(0xc5, 0x9b, 0xf2),
            number: Color32::from_rgb(0x8f, 0xd4, 0x9a),
            comment: Color32::from_gray(0x80),
            null: Color32::from_gray(0x78),
            blob: Color32::from_rgb(0xd8, 0x8f, 0xb4),
            ok: Color32::from_rgb(0x6c, 0xc4, 0x7c),
            err: Color32::from_rgb(0xf0, 0x6a, 0x6a),
            warn: Color32::from_rgb(0xe8, 0xb3, 0x39),
            app: Color32::from_rgb(0x6a, 0x9f, 0xd8),
            user: Color32::from_rgb(0xd8, 0x9a, 0x4f),
        }
    } else {
        Palette {
            keyword: Color32::from_rgb(0x1f, 0x4f, 0xb8),
            string: Color32::from_rgb(0xa3, 0x4e, 0x00),
            ident: Color32::from_rgb(0x7a, 0x2e, 0xa8),
            number: Color32::from_rgb(0x1a, 0x7f, 0x37),
            comment: Color32::from_gray(0x8a),
            null: Color32::from_gray(0x99),
            blob: Color32::from_rgb(0xa8, 0x2e, 0x6c),
            ok: Color32::from_rgb(0x1a, 0x7f, 0x37),
            err: Color32::from_rgb(0xc0, 0x26, 0x26),
            warn: Color32::from_rgb(0xa8, 0x6a, 0x00),
            app: Color32::from_rgb(0x2b, 0x5c, 0x9e),
            user: Color32::from_rgb(0x9c, 0x5a, 0x10),
        }
    }
}

// ---------------------------------------------------------------------------
// SQL highlighting
// ---------------------------------------------------------------------------

const KEYWORDS: &[&str] = &[
    "ABORT", "ACTION", "ADD", "AFTER", "ALL", "ALTER", "ALWAYS", "ANALYZE", "AND", "ANY", "AS", "ASC", "ATTACH",
    "AUTOINCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BLOB", "BY", "CASCADE", "CASE", "CAST", "CHECK", "COLLATE",
    "COLUMN", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE", "CROSS", "CURRENT_DATE", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "DATABASE", "DEFAULT", "DEFERRABLE", "DEFERRED", "DELETE", "DESC", "DETACH", "DISTINCT",
    "DO", "DROP", "EACH", "ELSE", "END", "ESCAPE", "EXCEPT", "EXCLUSIVE", "EXISTS", "EXPLAIN", "FAIL", "FILTER",
    "FOR", "FOREIGN", "FROM", "FULL", "GENERATED", "GLOB", "GROUP", "HAVING", "IF", "IGNORE", "IMMEDIATE", "IN",
    "INDEX", "INDEXED", "INITIALLY", "INNER", "INSERT", "INSTEAD", "INTEGER", "INTERSECT", "INTO", "IS", "ISNULL",
    "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "MATCH", "MATERIALIZED", "NATURAL", "NO", "NOT", "NOTHING", "NOTNULL",
    "NULL", "NULLS", "NUMERIC", "OF", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PLAN",
    "PRAGMA", "PRIMARY", "QUERY", "RAISE", "REAL", "RECURSIVE", "REFERENCES", "REGEXP", "REINDEX", "RELEASE",
    "RENAME", "REPLACE", "RESTRICT", "RETURNING", "RIGHT", "ROLLBACK", "ROW", "ROWID", "ROWS", "SAVEPOINT",
    "SELECT", "SET", "STORED", "STRICT", "TABLE", "TEMP", "TEMPORARY", "TEXT", "THEN", "TO", "TRANSACTION",
    "TRIGGER", "UNION", "UNIQUE", "UPDATE", "USING", "VACUUM", "VALUES", "VIEW", "VIRTUAL", "WHEN", "WHERE",
    "WINDOW", "WITH", "WITHOUT",
];

pub fn highlight_sql(ui: &Ui, text: &str, font: FontId) -> LayoutJob {
    let p = palette(ui);
    let normal = ui.visuals().text_color();
    let mut job = LayoutJob::default();
    let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let color;
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            color = p.comment;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            color = p.comment;
        } else if c == b'\'' || c == b'"' || c == b'`' || c == b'[' {
            let close = if c == b'[' { b']' } else { c };
            i += 1;
            while i < b.len() && b[i] != close {
                i += 1;
            }
            i = (i + 1).min(b.len());
            color = if c == b'\'' { p.string } else { p.ident };
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            color = p.number;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = text[start..i].to_ascii_uppercase();
            color = if KEYWORDS.contains(&word.as_str()) { p.keyword } else { normal };
        } else {
            // advance one UTF-8 char
            i += text[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            color = normal;
        }
        job.append(&text[start..i], 0.0, fmt(color));
    }
    job
}

/// A multi-line SQL editor with highlighting. Returns the output so callers
/// can read the cursor/selection.
pub fn sql_editor(ui: &mut Ui, text: &mut String, rows: usize, interactive: bool) -> egui::text_edit::TextEditOutput {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| -> Arc<egui::Galley> {
        let mut job = highlight_sql(ui, buf.as_str(), font.clone());
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    egui::TextEdit::multiline(text)
        .code_editor()
        .desired_rows(rows)
        .desired_width(f32::INFINITY)
        .lock_focus(true)
        .interactive(interactive)
        .layouter(&mut layouter)
        .show(ui)
}

/// Read-only highlighted SQL block.
pub fn sql_view(ui: &mut Ui, sql: &str) {
    let mut s = sql.to_string();
    sql_editor(ui, &mut s, 1, true);
}

// ---------------------------------------------------------------------------
// Data grid
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct GridResponse {
    pub clicked: Option<(usize, usize)>,
    pub double_clicked: Option<(usize, usize)>,
    pub header_clicked: Option<usize>,
    pub filters_changed: bool,
    pub copy_value: Option<String>,
}

pub struct GridArgs<'a> {
    pub id: String,
    pub columns: &'a [String],
    pub rows: &'a [Vec<Value>],
    /// Row number shown for row 0 (for paging).
    pub first_row_number: usize,
    pub selected: Option<(usize, usize)>,
    pub sort: Option<(usize, bool)>,
    pub filters: Option<&'a mut Vec<String>>,
}

pub fn value_rich(ui: &Ui, v: &Value) -> RichText {
    let p = palette(ui);
    let t = display_value(v);
    match v {
        Value::Null => RichText::new(t).italics().color(p.null),
        Value::Blob(_) => RichText::new(t).italics().color(p.blob),
        Value::Integer(_) | Value::Real(_) => RichText::new(t).color(p.number),
        Value::Text(_) => RichText::new(t),
    }
}

pub fn data_grid(ui: &mut Ui, args: GridArgs<'_>) -> GridResponse {
    let mut resp = GridResponse::default();
    let GridArgs { id, columns, rows, first_row_number, selected, sort, mut filters } = args;
    if let Some(f) = filters.as_deref_mut() {
        f.resize(columns.len(), String::new());
    }
    let has_filters = filters.is_some();
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let header_h = if has_filters { row_h * 2.0 + 6.0 } else { row_h + 2.0 };
    let p = palette(ui);
    let digits = (first_row_number + rows.len()).max(1).to_string().len();

    let mut tb = TableBuilder::new(ui)
        .id_salt(&id)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(10.0 + 8.0 * digits as f32));
    for _ in columns {
        tb = tb.column(Column::initial(130.0).at_least(40.0).clip(true).resizable(true));
    }
    tb.header(header_h, |mut header| {
        header.col(|ui| {
            ui.label("");
        });
        for (ci, name) in columns.iter().enumerate() {
            header.col(|ui| {
                ui.vertical(|ui| {
                    let arrow = match sort {
                        Some((c, true)) if c == ci => " ▲",
                        Some((c, false)) if c == ci => " ▼",
                        _ => "",
                    };
                    let r = ui.add(
                        egui::Label::new(RichText::new(format!("{name}{arrow}")).strong())
                            .truncate()
                            .sense(Sense::click()),
                    );
                    if r.clicked() {
                        resp.header_clicked = Some(ci);
                    }
                    r.on_hover_text(format!("{name}\nClick to sort"));
                    if let Some(f) = filters.as_deref_mut() {
                        let te = ui.add(
                            egui::TextEdit::singleline(&mut f[ci])
                                .hint_text("Filter")
                                .desired_width(f32::INFINITY),
                        );
                        if te.changed() {
                            resp.filters_changed = true;
                        }
                        te.on_hover_text(
                            "Text → LIKE '%text%'\n>5, <=3, <>0, =abc → comparison\n=NULL / <>NULL → IS [NOT] NULL",
                        );
                    }
                });
            });
        }
    })
    .body(|body| {
        body.rows(row_h, rows.len(), |mut row| {
            let ri = row.index();
            let row_selected = selected.is_some_and(|(r, _)| r == ri);
            row.set_selected(row_selected);
            row.col(|ui| {
                ui.label(RichText::new((first_row_number + ri).to_string()).weak().small());
            });
            for (ci, v) in rows[ri].iter().enumerate() {
                let (_, cell) = row.col(|ui| {
                    let mut text = value_rich(ui, v);
                    if selected == Some((ri, ci)) {
                        text = text.background_color(ui.visuals().selection.bg_fill).color(p.keyword);
                    }
                    ui.add(egui::Label::new(text).truncate().selectable(false));
                });
                if cell.clicked() {
                    resp.clicked = Some((ri, ci));
                }
                if cell.double_clicked() {
                    resp.double_clicked = Some((ri, ci));
                }
                cell.context_menu(|ui| {
                    if ui.button("Copy value").clicked() {
                        resp.copy_value = Some(crate::db::as_text(v));
                        ui.close();
                    }
                    if ui.button("Copy as SQL literal").clicked() {
                        resp.copy_value = Some(crate::db::sql_literal(v));
                        ui.close();
                    }
                });
            }
        });
    });
    resp
}

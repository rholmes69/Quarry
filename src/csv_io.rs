//! CSV import / export.
//!
//! The import path is designed so a CSV never fails half-way because of a
//! type cast:
//!
//! * Type inference for new tables is conservative: a column only becomes
//!   INTEGER/REAL if *every* value round-trips. Zip codes ("02134"), phone
//!   numbers, 20-digit IDs, "1e400", "NaN" and friends stay TEXT so nothing
//!   is silently mangled.
//! * Values are bound with an explicit SQLite type chosen from the target
//!   column's *affinity* (the same rules SQLite uses), rather than relying on
//!   string casts.
//! * STRICT tables are validated value by value up front; bad cells are
//!   reported with row/column and you choose: abort, skip the row, or store
//!   NULL.
//! * The whole import runs inside its own savepoint, so an abort leaves the
//!   database untouched, and a success can still be reverted with
//!   "Revert Changes".

use std::path::Path;
use std::time::Instant;

use rusqlite::types::Value;

use crate::db::{format_real, hex, quote_ident, Database, LogSource, Result};

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub quote: u8,
    pub has_header: bool,
    pub trim: bool,
    pub empty_as_null: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions { delimiter: b',', quote: b'"', has_header: true, trim: false, empty_as_null: true }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CsvData {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Total data rows in the file (may exceed rows.len() for previews).
    pub total_rows: usize,
}

/// Guess the delimiter from the first line of the file.
pub fn sniff_delimiter(path: &Path) -> u8 {
    let Ok(text) = std::fs::read(path) else { return b',' };
    let first: Vec<u8> = text.iter().take(64 * 1024).copied().take_while(|&b| b != b'\n').collect();
    let mut best = (b',', 0usize);
    for d in [b',', b';', b'\t', b'|'] {
        let n = first.iter().filter(|&&b| b == d).count();
        if n > best.1 {
            best = (d, n);
        }
    }
    best.0
}

/// Read a CSV file. `limit` caps the number of data rows kept (for preview);
/// the total row count is still reported.
pub fn read_csv(path: &Path, opts: &CsvOptions, limit: Option<usize>) -> Result<CsvData> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(opts.delimiter)
        .quote(opts.quote)
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;

    let mut data = CsvData::default();
    let mut width = 0usize;
    let mut first = true;
    let mut rec = csv::ByteRecord::new();
    loop {
        match rdr.read_byte_record(&mut rec) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("CSV parse error: {e}")),
        }
        let mut fields: Vec<String> = rec
            .iter()
            .map(|f| {
                let s = String::from_utf8_lossy(f).to_string();
                if opts.trim { s.trim().to_string() } else { s }
            })
            .collect();
        if first {
            if let Some(f0) = fields.first_mut() {
                if let Some(stripped) = f0.strip_prefix('\u{feff}') {
                    *f0 = stripped.to_string();
                }
            }
        }
        // skip completely empty lines
        if fields.len() == 1 && fields[0].is_empty() {
            continue;
        }
        width = width.max(fields.len());
        if first && opts.has_header {
            data.headers = fields;
            first = false;
            continue;
        }
        first = false;
        data.total_rows += 1;
        if limit.is_none_or(|l| data.rows.len() < l) {
            data.rows.push(fields);
        }
    }
    // Normalise header and row widths.
    if data.headers.is_empty() || !opts.has_header {
        data.headers = (1..=width).map(|i| format!("field{i}")).collect();
    }
    let mut seen = std::collections::HashSet::new();
    for (i, h) in data.headers.iter_mut().enumerate() {
        if h.trim().is_empty() {
            *h = format!("field{}", i + 1);
        }
        let base = h.clone();
        let mut n = 2;
        while !seen.insert(h.to_lowercase()) {
            *h = format!("{base}_{n}");
            n += 1;
        }
    }
    while data.headers.len() < width {
        let n = data.headers.len() + 1;
        data.headers.push(format!("field{n}"));
    }
    for r in &mut data.rows {
        r.resize(width.max(data.headers.len()), String::new());
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Type inference & affinity
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Integer,
    Real,
    Text,
}

impl ColType {
    pub fn sql(self) -> &'static str {
        match self {
            ColType::Integer => "INTEGER",
            ColType::Real => "REAL",
            ColType::Text => "TEXT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Empty,
    Int,
    Real,
    Text,
}

/// Classify a raw CSV value *canonically*: only values that survive a
/// round-trip unchanged count as numbers.
fn classify(s: &str) -> Kind {
    if s.is_empty() {
        return Kind::Empty;
    }
    if let Ok(i) = s.parse::<i64>() {
        return if i.to_string() == s { Kind::Int } else { Kind::Text };
    }
    if is_plain_real(s) {
        return Kind::Real;
    }
    Kind::Text
}

fn is_plain_real(s: &str) -> bool {
    if !s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E')) {
        return false; // rejects "inf", "NaN", "1_000", "0x1F"
    }
    if s.starts_with('+') || !s.bytes().any(|b| b.is_ascii_digit()) {
        return false;
    }
    let unsigned = s.strip_prefix('-').unwrap_or(s);
    let int_part = unsigned.split(['.', 'e', 'E']).next().unwrap_or("");
    if int_part.len() > 1 && int_part.starts_with('0') {
        return false; // "007.5" keeps its zeros
    }
    if !s.contains(['.', 'e', 'E']) {
        return false; // an integer too big for i64 (e.g. 20-digit IDs) stays TEXT
    }
    let mantissa = unsigned.split(['e', 'E']).next().unwrap_or("");
    if mantissa.bytes().filter(|b| b.is_ascii_digit()).count() > 15 {
        return false; // more precision than a double can hold
    }
    matches!(s.parse::<f64>(), Ok(f) if f.is_finite())
}

/// Infer a column type for each CSV column over the given rows.
pub fn infer_types(data: &CsvData) -> Vec<ColType> {
    (0..data.headers.len())
        .map(|c| {
            let mut has_int = false;
            let mut has_real = false;
            for r in &data.rows {
                match classify(r.get(c).map(|s| s.as_str()).unwrap_or("")) {
                    Kind::Empty => {}
                    Kind::Int => has_int = true,
                    Kind::Real => has_real = true,
                    Kind::Text => return ColType::Text,
                }
            }
            match (has_int, has_real) {
                (_, true) => ColType::Real,
                (true, false) => ColType::Integer,
                _ => ColType::Text,
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// SQLite's column-affinity rules (datatype3.html §3.1).
pub fn affinity_of(decl: &str) -> Affinity {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        Affinity::Integer
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Affinity::Text
    } else if d.contains("BLOB") || d.trim().is_empty() || d.trim() == "ANY" {
        Affinity::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Convert one raw CSV value into a typed SQLite value for a target column.
/// Only STRICT tables can produce an error; everything else is lossless by
/// construction (unconvertible values are stored as TEXT, as SQLite would).
pub fn convert(raw: &str, affinity: Affinity, strict: bool, empty_as_null: bool) -> std::result::Result<Value, String> {
    if raw.is_empty() && empty_as_null {
        return Ok(Value::Null);
    }
    let t = raw.trim();
    match affinity {
        Affinity::Text => Ok(Value::Text(raw.to_string())),
        Affinity::Integer => {
            if let Ok(i) = t.parse::<i64>() {
                Ok(Value::Integer(i))
            } else if let Some(f) = parse_real(t) {
                if strict {
                    if f.fract() == 0.0 && f.abs() < 9.2e18 {
                        Ok(Value::Integer(f as i64))
                    } else {
                        Err(format!("'{raw}' is not an integer"))
                    }
                } else {
                    Ok(Value::Real(f))
                }
            } else if strict {
                Err(format!("'{raw}' is not an integer"))
            } else {
                Ok(Value::Text(raw.to_string()))
            }
        }
        Affinity::Real => {
            if let Some(f) = parse_real(t) {
                Ok(Value::Real(f))
            } else if strict {
                Err(format!("'{raw}' is not a real number"))
            } else {
                Ok(Value::Text(raw.to_string()))
            }
        }
        Affinity::Numeric => match classify(raw) {
            Kind::Int => Ok(Value::Integer(raw.parse().unwrap())),
            Kind::Real => Ok(Value::Real(raw.parse().unwrap())),
            _ => Ok(Value::Text(raw.to_string())),
        },
        // Typeless / ANY / BLOB columns: keep exactly what the file says,
        // but store canonical numbers as numbers so they sort correctly.
        Affinity::Blob => match classify(raw) {
            Kind::Int => Ok(Value::Integer(raw.parse().unwrap())),
            Kind::Real => Ok(Value::Real(raw.parse().unwrap())),
            Kind::Empty => Ok(Value::Text(String::new())),
            Kind::Text => Ok(Value::Text(raw.to_string())),
        },
    }
}

fn parse_real(t: &str) -> Option<f64> {
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E')) {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    Abort,
    SkipRow,
    UseNull,
}

#[derive(Clone, Debug)]
pub enum ImportTarget {
    /// Create a new table. (column name, declared type)
    NewTable { name: String, columns: Vec<(String, String)>, strict: bool },
    /// Append to an existing table.
    Existing { name: String },
}

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub inserted: usize,
    pub skipped: usize,
    pub nulled: usize,
    pub problems: Vec<String>,
    pub millis: f64,
}

/// Build the CREATE TABLE statement for a new-table import.
pub fn create_table_sql(name: &str, columns: &[(String, String)], strict: bool) -> String {
    let cols = columns
        .iter()
        .map(|(n, t)| if t.is_empty() { quote_ident(n) } else { format!("{} {}", quote_ident(n), t) })
        .collect::<Vec<_>>()
        .join(",\n\t");
    format!("CREATE TABLE {} (\n\t{}\n){};", quote_ident(name), cols, if strict { " STRICT" } else { "" })
}

pub fn import_csv(
    db: &mut Database,
    data: &CsvData,
    target: &ImportTarget,
    opts: &CsvOptions,
    on_error: OnError,
) -> Result<ImportReport> {
    if db.read_only {
        return Err("Database is opened read-only.".into());
    }
    let start = Instant::now();
    db.ensure_savepoint()?;
    db.execute(LogSource::App, "SAVEPOINT \"CSVIMPORT\";", &[])?;

    let result = (|| -> Result<ImportReport> {
        let table = match target {
            ImportTarget::NewTable { name, columns, strict } => {
                if name.trim().is_empty() {
                    return Err("Table name is empty.".into());
                }
                if columns.len() != data.headers.len() {
                    return Err("Column definitions don't match the CSV width.".into());
                }
                db.execute(LogSource::App, &create_table_sql(name, columns, *strict), &[])?;
                name.clone()
            }
            ImportTarget::Existing { name } => name.clone(),
        };

        let details = db.table_details(&table)?;
        if details.is_view {
            return Err(format!("'{table}' is a view."));
        }
        let writable: Vec<_> = details.columns.iter().filter(|c| c.hidden == 0).collect();

        // Map CSV columns -> table columns: by header name if every header
        // matches a column, otherwise by position.
        let by_name: Option<Vec<usize>> = if opts.has_header {
            data.headers
                .iter()
                .map(|h| writable.iter().position(|c| c.name.eq_ignore_ascii_case(h.trim())))
                .collect()
        } else {
            None
        };
        let mapping: Vec<usize> = match by_name {
            Some(m) => m,
            None => {
                if data.headers.len() > writable.len() {
                    return Err(format!(
                        "The CSV has {} columns but table '{}' only has {}.",
                        data.headers.len(),
                        table,
                        writable.len()
                    ));
                }
                (0..data.headers.len()).collect()
            }
        };

        let targets: Vec<(String, Affinity, bool)> = mapping
            .iter()
            .map(|&i| (writable[i].name.clone(), affinity_of(&writable[i].decl_type), writable[i].notnull))
            .collect();
        let col_list = targets.iter().map(|t| quote_ident(&t.0)).collect::<Vec<_>>().join(", ");
        let placeholders = (1..=targets.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
        let insert_sql = format!("INSERT INTO {} ({}) VALUES ({});", quote_ident(&table), col_list, placeholders);

        let mut report = ImportReport::default();
        let mut stmt = db.conn.prepare(&insert_sql).map_err(|e| e.to_string())?;
        'rows: for (ri, row) in data.rows.iter().enumerate() {
            let line = ri + 1 + usize::from(opts.has_header);
            let mut vals = Vec::with_capacity(targets.len());
            for (ci, (cname, aff, notnull)) in targets.iter().enumerate() {
                let raw = row.get(ci).map(|s| s.as_str()).unwrap_or("");
                // An empty cell in a NOT NULL column is stored as '' not NULL.
                let empty_null = opts.empty_as_null && !notnull;
                match convert(raw, *aff, details.strict, empty_null) {
                    Ok(v) => vals.push(v),
                    Err(msg) => {
                        let problem = format!("Line {line}, column '{cname}': {msg}");
                        match on_error {
                            OnError::Abort => return Err(format!("Import aborted. {problem}")),
                            OnError::SkipRow => {
                                push_problem(&mut report, problem);
                                report.skipped += 1;
                                continue 'rows;
                            }
                            OnError::UseNull => {
                                push_problem(&mut report, problem);
                                report.nulled += 1;
                                vals.push(Value::Null);
                            }
                        }
                    }
                }
            }
            for (i, v) in vals.iter().enumerate() {
                stmt.raw_bind_parameter(i + 1, v).map_err(|e| e.to_string())?;
            }
            if let Err(e) = stmt.raw_execute() {
                let problem = format!("Line {line}: {e}");
                match on_error {
                    OnError::Abort => return Err(format!("Import aborted. {problem}")),
                    _ => {
                        push_problem(&mut report, problem);
                        report.skipped += 1;
                        continue;
                    }
                }
            }
            report.inserted += 1;
        }
        drop(stmt);
        report.millis = start.elapsed().as_secs_f64() * 1000.0;
        db.push_log(
            LogSource::App,
            &format!("{insert_sql}  -- prepared once, executed {} time(s)", report.inserted),
            true,
            report.millis,
            format!("{} row(s) inserted, {} skipped", report.inserted, report.skipped),
        );
        Ok(report)
    })();

    match result {
        Ok(r) => {
            db.execute(LogSource::App, "RELEASE \"CSVIMPORT\";", &[])?;
            Ok(r)
        }
        Err(e) => {
            let _ = db.execute(LogSource::App, "ROLLBACK TO \"CSVIMPORT\";", &[]);
            let _ = db.execute(LogSource::App, "RELEASE \"CSVIMPORT\";", &[]);
            Err(e)
        }
    }
}

fn push_problem(r: &mut ImportReport, p: String) {
    if r.problems.len() < 200 {
        r.problems.push(p);
    }
}

/// Make a valid, readable table name from a file name.
pub fn table_name_from_path(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "imported".into());
    let s: String = stem.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if s.is_empty() { "imported".into() } else { s }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub delimiter: u8,
    pub quote: u8,
    pub header: bool,
    pub quote_all: bool,
    pub null_text: String,
    pub crlf: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { delimiter: b',', quote: b'"', header: true, quote_all: false, null_text: String::new(), crlf: false }
    }
}

pub fn export_value(v: &Value, null_text: &str) -> String {
    match v {
        Value::Null => null_text.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format_real(*f),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => format!("0x{}", hex(b)),
        },
    }
}

pub fn write_csv(path: &Path, columns: &[String], rows: &[Vec<Value>], opts: &ExportOptions) -> Result<usize> {
    let mut w = csv::WriterBuilder::new()
        .delimiter(opts.delimiter)
        .quote(opts.quote)
        .quote_style(if opts.quote_all { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
        .terminator(if opts.crlf { csv::Terminator::CRLF } else { csv::Terminator::Any(b'\n') })
        .from_path(path)
        .map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    if opts.header {
        w.write_record(columns).map_err(|e| e.to_string())?;
    }
    for r in rows {
        w.write_record(r.iter().map(|v| export_value(v, &opts.null_text))).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

pub fn export_table(db: &mut Database, table: &str, path: &Path, opts: &ExportOptions) -> Result<usize> {
    let r = db.query(LogSource::App, &format!("SELECT * FROM {};", quote_ident(table)), &[])?;
    write_csv(path, &r.columns, &r.rows, opts)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn csv_file(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::File::create(&p).unwrap().write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn inference_preserves_leading_zeros_and_big_ids() {
        let dir = tempfile::tempdir().unwrap();
        let p = csv_file(
            &dir,
            "people.csv",
            "\u{feff}id,zip,score,big_id,note,ratio\n1,02134,9.5,12345678901234567890,hi,0.25\n2,90210,10,2,\"a, b\",1e3\n3,,,,,\n",
        );
        let data = read_csv(&p, &CsvOptions::default(), None).unwrap();
        assert_eq!(data.headers[0], "id", "BOM stripped");
        let types = infer_types(&data);
        assert_eq!(
            types,
            vec![ColType::Integer, ColType::Text, ColType::Real, ColType::Text, ColType::Text, ColType::Real]
        );
    }

    #[test]
    fn round_trip_import_export_new_table() {
        let dir = tempfile::tempdir().unwrap();
        let p = csv_file(&dir, "t.csv", "a,b,c\n1,0007,x\n2,08,\"multi\nline\"\n3,,\n");
        let opts = CsvOptions::default();
        let data = read_csv(&p, &opts, None).unwrap();
        let types = infer_types(&data);
        let cols: Vec<(String, String)> =
            data.headers.iter().cloned().zip(types.iter().map(|t| t.sql().to_string())).collect();
        let mut db = Database::open_in_memory().unwrap();
        let target = ImportTarget::NewTable { name: "t".into(), columns: cols, strict: false };
        let rep = import_csv(&mut db, &data, &target, &opts, OnError::Abort).unwrap();
        assert_eq!(rep.inserted, 3);
        let r = db.query(LogSource::App, "SELECT b, typeof(a), c FROM t ORDER BY a", &[]).unwrap();
        assert_eq!(r.rows[0][0], Value::Text("0007".into()));
        assert_eq!(r.rows[0][1], Value::Text("integer".into()));
        assert_eq!(r.rows[1][2], Value::Text("multi\nline".into()));
        assert_eq!(r.rows[2][0], Value::Null);

        let out = dir.path().join("out.csv");
        export_table(&mut db, "t", &out, &ExportOptions::default()).unwrap();
        let back = read_csv(&out, &opts, None).unwrap();
        assert_eq!(back.rows, data.rows);
    }

    #[test]
    fn strict_table_reports_bad_cells_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let p = csv_file(&dir, "s.csv", "id,qty\n1,5\n2,five\n3,7.0\n");
        let opts = CsvOptions::default();
        let data = read_csv(&p, &opts, None).unwrap();
        let mut db = Database::open_in_memory().unwrap();
        db.execute(LogSource::App, "CREATE TABLE s(id INTEGER, qty INTEGER) STRICT;", &[]).unwrap();
        let target = ImportTarget::Existing { name: "s".into() };

        let err = import_csv(&mut db, &data, &target, &opts, OnError::Abort).unwrap_err();
        assert!(err.contains("Line 3") && err.contains("qty"), "{err}");
        assert_eq!(db.row_count("s", "", &[]).unwrap(), 0, "abort leaves table untouched");

        let rep = import_csv(&mut db, &data, &target, &opts, OnError::SkipRow).unwrap();
        assert_eq!((rep.inserted, rep.skipped), (2, 1));
        let rep = import_csv(&mut db, &data, &target, &opts, OnError::UseNull).unwrap();
        assert_eq!((rep.inserted, rep.nulled), (3, 1));
    }

    #[test]
    fn maps_existing_columns_by_header_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = csv_file(&dir, "m.csv", "Name,ID\nann,1\nbob,2\n");
        let opts = CsvOptions::default();
        let data = read_csv(&p, &opts, None).unwrap();
        let mut db = Database::open_in_memory().unwrap();
        db.execute(LogSource::App, "CREATE TABLE m(id INTEGER PRIMARY KEY, name TEXT, extra TEXT DEFAULT 'd');", &[])
            .unwrap();
        import_csv(&mut db, &data, &ImportTarget::Existing { name: "m".into() }, &opts, OnError::Abort).unwrap();
        let r = db.query(LogSource::App, "SELECT id, name, extra FROM m ORDER BY id", &[]).unwrap();
        assert_eq!(r.rows[1], vec![Value::Integer(2), Value::Text("bob".into()), Value::Text("d".into())]);
    }

    #[test]
    fn sniffs_semicolons() {
        let dir = tempfile::tempdir().unwrap();
        let p = csv_file(&dir, "eu.csv", "a;b;c\n1;2,5;3\n");
        assert_eq!(sniff_delimiter(&p), b';');
    }
}

//! Data Pipe: move tables and query results between SQLite and enterprise
//! servers (PostgreSQL, MySQL / MariaDB, Microsoft SQL Server).
//!
//! Design points:
//! * **Pull** (server → SQLite) streams the remote result into a new or
//!   existing SQLite table. Column types are inferred from the *values*
//!   (exact decimals that don't fit a double stay TEXT), and inserts run in
//!   a revertible savepoint like every other edit.
//! * **Push** (SQLite → server) infers the narrowest remote type that holds
//!   *every* value actually stored in each column. SQLite lets an INTEGER
//!   column contain text; we notice and widen to a text type instead of
//!   failing half-way through. Rows go in batched multi-row INSERTs inside a
//!   single remote transaction — all or nothing.
//! * All network work happens on a worker thread; the UI stays responsive
//!   and receives progress, log lines and the result over a channel.
//! * Every statement sent to the server is recorded in the SQL log with the
//!   "Remote" tag (bulk INSERT batches are abbreviated).
//! * Passwords are never written to disk.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use rusqlite::types::Value;

use crate::csv_io::{affinity_of, convert, Affinity};
use crate::db::{hex, quote_ident, Database, LogSource, Result};

#[cfg(feature = "mssql")]
mod mssql;
#[cfg(feature = "mysql-driver")]
mod mysql;
#[cfg(feature = "pg")]
mod pg;

// ---------------------------------------------------------------------------
// Connection settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Postgres,
    MySql,
    MsSql,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Postgres, Kind::MySql, Kind::MsSql];
    pub fn label(self) -> &'static str {
        match self {
            Kind::Postgres => "PostgreSQL",
            Kind::MySql => "MySQL / MariaDB",
            Kind::MsSql => "SQL Server / Azure SQL",
        }
    }
    pub fn default_port(self) -> u16 {
        match self {
            Kind::Postgres => 5432,
            Kind::MySql => 3306,
            Kind::MsSql => 1433,
        }
    }
    pub fn default_schema(self) -> &'static str {
        match self {
            Kind::Postgres => "public",
            Kind::MySql => "",
            Kind::MsSql => "dbo",
        }
    }
    pub fn compiled_in(self) -> bool {
        match self {
            Kind::Postgres => cfg!(feature = "pg"),
            Kind::MySql => cfg!(feature = "mysql-driver"),
            Kind::MsSql => cfg!(feature = "mssql"),
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::MySql => "mysql",
            Kind::MsSql => "mssql",
        }
    }
    pub fn from_key(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.key() == s)
    }

    /// Quote an identifier, splitting `schema.table`.
    pub fn quote(self, name: &str) -> String {
        name.split('.')
            .map(|part| match self {
                Kind::Postgres => format!("\"{}\"", part.replace('"', "\"\"")),
                Kind::MySql => format!("`{}`", part.replace('`', "``")),
                Kind::MsSql => format!("[{}]", part.replace(']', "]]")),
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// SQL literal for a value going into a column of class `class`.
    pub fn literal(self, v: &Value, class: ColClass) -> String {
        let text = |s: &str| match self {
            Kind::Postgres => format!("'{}'", s.replace('\'', "''")),
            // MySQL treats backslash as an escape character by default
            Kind::MySql => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            Kind::MsSql => format!("N'{}'", s.replace('\'', "''")),
        };
        match (v, class) {
            (Value::Null, _) => "NULL".into(),
            (Value::Blob(b), ColClass::Blob) => match self {
                Kind::Postgres => format!("'\\x{}'::bytea", hex(b)),
                Kind::MySql => if b.is_empty() { "''".into() } else { format!("X'{}'", hex(b)) },
                Kind::MsSql => if b.is_empty() { "0x".into() } else { format!("0x{}", hex(b)) },
            },
            (Value::Blob(b), _) => text(&String::from_utf8_lossy(b)),
            (Value::Integer(i), ColClass::Integer | ColClass::Real) => i.to_string(),
            (Value::Real(f), ColClass::Integer | ColClass::Real) if f.is_finite() => format!("{f:?}"),
            (Value::Real(f), _) if !f.is_finite() => "NULL".into(),
            (Value::Integer(i), _) => text(&i.to_string()),
            (Value::Real(f), _) => text(&crate::db::format_real(*f)),
            (Value::Text(t), _) => text(t),
        }
    }

    pub fn type_name(self, class: ColClass, keyed: bool) -> &'static str {
        match (self, class) {
            (Kind::Postgres, ColClass::Integer) => "BIGINT",
            (Kind::Postgres, ColClass::Real) => "DOUBLE PRECISION",
            (Kind::Postgres, ColClass::Text) => "TEXT",
            (Kind::Postgres, ColClass::Blob) => "BYTEA",
            (Kind::MySql, ColClass::Integer) => "BIGINT",
            (Kind::MySql, ColClass::Real) => "DOUBLE",
            (Kind::MySql, ColClass::Text) => if keyed { "VARCHAR(255)" } else { "LONGTEXT" },
            (Kind::MySql, ColClass::Blob) => if keyed { "VARBINARY(255)" } else { "LONGBLOB" },
            (Kind::MsSql, ColClass::Integer) => "BIGINT",
            (Kind::MsSql, ColClass::Real) => "FLOAT",
            (Kind::MsSql, ColClass::Text) => if keyed { "NVARCHAR(450)" } else { "NVARCHAR(MAX)" },
            (Kind::MsSql, ColClass::Blob) => if keyed { "VARBINARY(900)" } else { "VARBINARY(MAX)" },
        }
    }

    pub fn select_all(self, table: &str, limit: Option<usize>) -> String {
        let q = self.quote(table);
        match (self, limit) {
            (Kind::MsSql, Some(n)) => format!("SELECT TOP {n} * FROM {q}"),
            (_, Some(n)) => format!("SELECT * FROM {q} LIMIT {n}"),
            (_, None) => format!("SELECT * FROM {q}"),
        }
    }

    pub fn batch_rows(self) -> usize {
        // SQL Server caps a VALUES list at 1000 rows
        match self {
            Kind::MsSql => 500,
            _ => 1000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    Disable,
    Require,
}

#[derive(Clone, Debug)]
pub struct ConnSpec {
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub tls: TlsMode,
    /// Accept self-signed / untrusted server certificates.
    pub trust_cert: bool,
}

impl ConnSpec {
    pub fn new(kind: Kind) -> Self {
        ConnSpec {
            kind,
            host: "localhost".into(),
            port: kind.default_port(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tls: TlsMode::Disable,
            trust_cert: false,
        }
    }
    pub fn describe(&self) -> String {
        format!("{} {}@{}:{}/{}", self.kind.label(), self.user, self.host, self.port, self.database)
    }
}

// ---------------------------------------------------------------------------
// Saved connection profiles (no passwords)
// ---------------------------------------------------------------------------

pub fn profiles_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("quarry").join("connections.tsv"))
}

pub fn load_profiles() -> Vec<(String, ConnSpec)> {
    let Some(p) = profiles_path() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(p) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 8 {
                return None;
            }
            let kind = Kind::from_key(f[1])?;
            Some((
                f[0].to_string(),
                ConnSpec {
                    kind,
                    host: f[2].into(),
                    port: f[3].parse().unwrap_or(kind.default_port()),
                    database: f[4].into(),
                    user: f[5].into(),
                    password: String::new(),
                    tls: if f[6] == "require" { TlsMode::Require } else { TlsMode::Disable },
                    trust_cert: f[7] == "1",
                },
            ))
        })
        .collect()
}

pub fn save_profiles(list: &[(String, ConnSpec)]) -> Result<()> {
    let p = profiles_path().ok_or("No config directory")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let body: String = list
        .iter()
        .map(|(n, c)| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                clean(n),
                c.kind.key(),
                clean(&c.host),
                c.port,
                clean(&c.database),
                clean(&c.user),
                if c.tls == TlsMode::Require { "require" } else { "disable" },
                if c.trust_cert { "1" } else { "0" }
            )
        })
        .collect();
    std::fs::write(p, body).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Data & type inference
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColClass {
    Integer,
    Real,
    Text,
    Blob,
}

#[derive(Clone, Debug, Default)]
pub struct Tabular {
    pub columns: Vec<String>,
    /// Source type names (remote type for pulls, declared type for pushes).
    pub types: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// The narrowest class that can hold every value in column `c`.
pub fn infer_class(rows: &[Vec<Value>], c: usize) -> ColClass {
    let (mut int, mut real, mut text, mut blob) = (false, false, false, false);
    for r in rows {
        match r.get(c) {
            Some(Value::Integer(_)) => int = true,
            Some(Value::Real(_)) => real = true,
            Some(Value::Text(_)) => text = true,
            Some(Value::Blob(_)) => blob = true,
            _ => {}
        }
    }
    match (int, real, text, blob) {
        (_, _, false, true) if !int && !real => ColClass::Blob,
        (_, _, true, _) | (_, _, _, true) => ColClass::Text,
        (_, true, _, _) => ColClass::Real,
        (true, _, _, _) => ColClass::Integer,
        _ => ColClass::Text, // all NULL
    }
}

/// For pulls: exact-decimal text values (e.g. NUMERIC from Postgres) become
/// numbers only when that is lossless, same rules as CSV import.
pub fn infer_sqlite_type(rows: &[Vec<Value>], c: usize) -> &'static str {
    let mut texts = Vec::new();
    let (mut int, mut real, mut blob, mut other_text) = (false, false, false, false);
    for r in rows {
        match r.get(c) {
            Some(Value::Integer(_)) => int = true,
            Some(Value::Real(_)) => real = true,
            Some(Value::Blob(_)) => blob = true,
            Some(Value::Text(t)) => texts.push(t.clone()),
            _ => {}
        }
    }
    if !texts.is_empty() {
        let data = crate::csv_io::CsvData { headers: vec!["x".into()], rows: texts.into_iter().map(|t| vec![t]).collect(), total_rows: 0 };
        match crate::csv_io::infer_types(&data)[0] {
            crate::csv_io::ColType::Integer if !real => int = true,
            crate::csv_io::ColType::Integer | crate::csv_io::ColType::Real => real = true,
            crate::csv_io::ColType::Text => other_text = true,
        }
    }
    if other_text || (blob && (int || real)) {
        "TEXT"
    } else if blob {
        "BLOB"
    } else if real {
        "REAL"
    } else if int {
        "INTEGER"
    } else {
        "TEXT"
    }
}

// ---------------------------------------------------------------------------
// Remote driver abstraction
// ---------------------------------------------------------------------------

/// A line for the SQL log, produced on the worker thread.
#[derive(Clone, Debug)]
pub struct RemoteLog {
    pub sql: String,
    pub ok: bool,
    pub millis: f64,
    pub detail: String,
}

pub trait Remote {
    fn version(&mut self) -> Result<String>;
    fn query(&mut self, sql: &str) -> Result<Tabular>;
    fn execute(&mut self, sql: &str) -> Result<()>;
    fn list_tables(&mut self) -> Result<Vec<String>>;
    fn table_columns(&mut self, table: &str) -> Result<Option<Vec<String>>>;
}

pub fn connect(spec: &ConnSpec) -> Result<Box<dyn Remote>> {
    if !spec.kind.compiled_in() {
        return Err(format!("{} support isn't compiled into this build.", spec.kind.label()));
    }
    match spec.kind {
        #[cfg(feature = "pg")]
        Kind::Postgres => Ok(Box::new(pg::Pg::connect(spec)?)),
        #[cfg(feature = "mysql-driver")]
        Kind::MySql => Ok(Box::new(mysql::My::connect(spec)?)),
        #[cfg(feature = "mssql")]
        Kind::MsSql => Ok(Box::new(mssql::Ms::connect(spec)?)),
        #[allow(unreachable_patterns)]
        _ => Err("Driver not available".into()),
    }
}

// ---------------------------------------------------------------------------
// Jobs (run on a worker thread)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushMode {
    /// Create the table if missing, otherwise append.
    CreateOrAppend,
    /// Drop and recreate the remote table.
    Replace,
    /// Keep the remote table, delete its rows, then insert.
    Truncate,
}

pub enum Job {
    Test,
    ListTables,
    Pull { sql: String },
    Push { table: String, data: Tabular, pk: Vec<String>, mode: PushMode },
}

pub enum Output {
    Tested(String),
    Tables(Vec<String>),
    Pulled(Tabular),
    Pushed { rows: usize, created: bool, widened: Vec<String> },
}

pub enum Msg {
    Log(RemoteLog),
    Progress(String, f32),
    Done(Result<Output>),
}

pub fn spawn(spec: ConnSpec, job: Job) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = run_job(&spec, job, &tx);
        let _ = tx.send(Msg::Done(res));
    });
    rx
}

struct Logged<'a> {
    inner: Box<dyn Remote>,
    tx: &'a Sender<Msg>,
}

impl Logged<'_> {
    fn log(&self, sql: &str, ok: bool, t0: Instant, detail: String) {
        let _ = self.tx.send(Msg::Log(RemoteLog { sql: sql.to_string(), ok, millis: t0.elapsed().as_secs_f64() * 1000.0, detail }));
    }
    fn query(&mut self, sql: &str) -> Result<Tabular> {
        let t0 = Instant::now();
        let r = self.inner.query(sql);
        match &r {
            Ok(t) => self.log(sql, true, t0, format!("{} row(s) returned", t.rows.len())),
            Err(e) => self.log(sql, false, t0, e.clone()),
        }
        r
    }
    fn execute(&mut self, sql: &str, shown: Option<String>) -> Result<()> {
        let t0 = Instant::now();
        let r = self.inner.execute(sql);
        let shown = shown.unwrap_or_else(|| sql.to_string());
        match &r {
            Ok(()) => self.log(&shown, true, t0, "ok".into()),
            Err(e) => self.log(&shown, false, t0, e.clone()),
        }
        r
    }
}

fn run_job(spec: &ConnSpec, job: Job, tx: &Sender<Msg>) -> Result<Output> {
    let _ = tx.send(Msg::Progress(format!("Connecting to {}…", spec.host), 0.0));
    let t0 = Instant::now();
    let inner = connect(spec);
    let _ = tx.send(Msg::Log(RemoteLog {
        sql: format!("-- connect {}", spec.describe()),
        ok: inner.is_ok(),
        millis: t0.elapsed().as_secs_f64() * 1000.0,
        detail: inner.as_ref().err().cloned().unwrap_or_else(|| "connected".into()),
    }));
    let mut r = Logged { inner: inner?, tx };
    match job {
        Job::Test => {
            let t0 = Instant::now();
            let v = r.inner.version();
            r.log("-- server version", v.is_ok(), t0, v.clone().unwrap_or_else(|e| e));
            Ok(Output::Tested(v?))
        }
        Job::ListTables => {
            let t0 = Instant::now();
            let t = r.inner.list_tables();
            r.log("-- list tables (information_schema)", t.is_ok(), t0, match &t {
                Ok(t) => format!("{} table(s)", t.len()),
                Err(e) => e.clone(),
            });
            Ok(Output::Tables(t?))
        }
        Job::Pull { sql } => {
            let _ = tx.send(Msg::Progress("Fetching rows…".into(), 0.3));
            Ok(Output::Pulled(r.query(&sql)?))
        }
        Job::Push { table, data, pk, mode } => push(&mut r, spec.kind, &table, data, &pk, mode),
    }
}

fn push(r: &mut Logged<'_>, kind: Kind, table: &str, data: Tabular, pk: &[String], mode: PushMode) -> Result<Output> {
    let tx = r.tx;
    let classes: Vec<ColClass> = (0..data.columns.len()).map(|c| infer_class(&data.rows, c)).collect();

    // Columns whose declared SQLite type promised more than the data delivers.
    let widened: Vec<String> = data
        .columns
        .iter()
        .zip(&data.types)
        .zip(&classes)
        .filter(|((_, decl), class)| {
            matches!(affinity_of(decl), Affinity::Integer | Affinity::Real | Affinity::Numeric) && **class == ColClass::Text
        })
        .map(|((n, _), _)| n.clone())
        .collect();

    let existing = r.inner.table_columns(table)?;
    let q = kind.quote(table);
    let create_sql = || {
        let mut defs: Vec<String> = data
            .columns
            .iter()
            .zip(&classes)
            .map(|(n, c)| {
                let keyed = pk.iter().any(|p| p.eq_ignore_ascii_case(n));
                format!("{} {}{}", kind.quote(n), kind.type_name(*c, keyed), if keyed { " NOT NULL" } else { "" })
            })
            .collect();
        if !pk.is_empty() {
            defs.push(format!("PRIMARY KEY ({})", pk.iter().map(|p| kind.quote(p)).collect::<Vec<_>>().join(", ")));
        }
        format!("CREATE TABLE {q} (\n  {}\n)", defs.join(",\n  "))
    };

    let begin = match kind {
        Kind::MsSql => "BEGIN TRANSACTION",
        _ => "BEGIN",
    };
    r.execute(begin, None)?;
    let result = (|| -> Result<Output> {
        let mut created = false;
        // which source columns go where
        let targets: Vec<(usize, ColClass)> = match (&existing, mode) {
            (Some(_), PushMode::Replace) => {
                r.execute(&format!("DROP TABLE {q}"), None)?;
                r.execute(&create_sql(), None)?;
                created = true;
                classes.iter().copied().enumerate().collect()
            }
            (Some(cols), _) => {
                if mode == PushMode::Truncate {
                    r.execute(&format!("DELETE FROM {q}"), None)?;
                }
                let m: Vec<(usize, ColClass)> = data
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| cols.iter().any(|c| c.eq_ignore_ascii_case(n)))
                    .map(|(i, _)| (i, classes[i]))
                    .collect();
                if m.is_empty() {
                    return Err(format!("None of the columns match remote table {table} ({}).", cols.join(", ")));
                }
                m
            }
            (None, _) => {
                r.execute(&create_sql(), None)?;
                created = true;
                classes.iter().copied().enumerate().collect()
            }
        };
        let col_list = targets.iter().map(|(i, _)| kind.quote(&data.columns[*i])).collect::<Vec<_>>().join(", ");
        let total = data.rows.len();
        let batch = kind.batch_rows();
        let batches = total.div_ceil(batch).max(1);
        for (bi, chunk) in data.rows.chunks(batch).enumerate() {
            let values: Vec<String> = chunk
                .iter()
                .map(|row| format!("({})", targets.iter().map(|(i, c)| kind.literal(&row[*i], *c)).collect::<Vec<_>>().join(", ")))
                .collect();
            let sql = format!("INSERT INTO {q} ({col_list}) VALUES\n{}", values.join(",\n"));
            let shown = format!("INSERT INTO {q} ({col_list}) VALUES …  -- batch {}/{}, {} row(s)", bi + 1, batches, chunk.len());
            r.execute(&sql, Some(shown))?;
            let _ = tx.send(Msg::Progress(
                format!("Inserted {} of {} rows", (bi * batch + chunk.len()).min(total), total),
                (bi + 1) as f32 / batches as f32,
            ));
        }
        Ok(Output::Pushed { rows: total, created, widened: widened.clone() })
    })();
    match result {
        Ok(o) => {
            r.execute("COMMIT", None)?;
            Ok(o)
        }
        Err(e) => {
            let _ = r.execute(match kind { Kind::MsSql => "IF @@TRANCOUNT > 0 ROLLBACK", _ => "ROLLBACK" }, None);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite side
// ---------------------------------------------------------------------------

/// Read a SQLite table (or query) for pushing. Returns data + its PK columns.
pub fn read_local(db: &mut Database, table_or_sql: &str, is_query: bool) -> Result<(Tabular, Vec<String>)> {
    if is_query {
        let r = db.query(LogSource::App, table_or_sql, &[])?;
        let n = r.columns.len();
        return Ok((Tabular { columns: r.columns, types: vec![String::new(); n], rows: r.rows }, vec![]));
    }
    let d = db.table_details(table_or_sql)?;
    let cols: Vec<&crate::db::ColumnInfo> = d.columns.iter().filter(|c| c.hidden == 0).collect();
    let r = db.query(
        LogSource::App,
        &format!(
            "SELECT {} FROM {};",
            cols.iter().map(|c| quote_ident(&c.name)).collect::<Vec<_>>().join(", "),
            quote_ident(table_or_sql)
        ),
        &[],
    )?;
    Ok((
        Tabular { columns: r.columns, types: cols.iter().map(|c| c.decl_type.clone()).collect(), rows: r.rows },
        d.pk_columns(),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    Create,
    Replace,
    Append,
}

/// Write pulled rows into SQLite inside a savepoint. Returns rows inserted.
pub fn write_local(db: &mut Database, table: &str, data: &Tabular, mode: PullMode) -> Result<usize> {
    if db.read_only {
        return Err("Database is opened read-only.".into());
    }
    let exists = db.table_names(false)?.iter().any(|t| t.eq_ignore_ascii_case(table));
    db.ensure_savepoint()?;
    db.execute(LogSource::App, "SAVEPOINT \"DATAPIPE\";", &[])?;
    let res = (|| -> Result<usize> {
        if exists && mode == PullMode::Create {
            return Err(format!("Table '{table}' already exists — choose Replace or Append."));
        }
        if exists && mode == PullMode::Replace {
            db.execute(LogSource::App, &format!("DROP TABLE {};", quote_ident(table)), &[])?;
        }
        if !exists || mode == PullMode::Replace {
            let defs: Vec<String> = data
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} {}", quote_ident(c), infer_sqlite_type(&data.rows, i)))
                .collect();
            db.execute(LogSource::App, &format!("CREATE TABLE {} (\n\t{}\n);", quote_ident(table), defs.join(",\n\t")), &[])?;
        }
        let d = db.table_details(table)?;
        let map: Vec<(usize, Affinity, String)> = data
            .columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| d.columns.iter().find(|tc| tc.name.eq_ignore_ascii_case(c)).map(|tc| (i, affinity_of(&tc.decl_type), tc.name.clone())))
            .collect();
        if map.is_empty() {
            return Err(format!("No columns of the result match table '{table}'."));
        }
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({});",
            quote_ident(table),
            map.iter().map(|m| quote_ident(&m.2)).collect::<Vec<_>>().join(", "),
            (1..=map.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
        );
        let t0 = Instant::now();
        {
            let mut stmt = db.conn.prepare(&sql).map_err(|e| e.to_string())?;
            for (ri, row) in data.rows.iter().enumerate() {
                for (k, (i, aff, name)) in map.iter().enumerate() {
                    let v = match &row[*i] {
                        Value::Text(t) => convert(t, *aff, d.strict, false).map_err(|e| format!("Row {}, column {name}: {e}", ri + 1))?,
                        other => other.clone(),
                    };
                    stmt.raw_bind_parameter(k + 1, &v).map_err(|e| e.to_string())?;
                }
                stmt.raw_execute().map_err(|e| format!("Row {}: {e}", ri + 1))?;
            }
        }
        db.push_log(
            LogSource::App,
            &format!("{sql}  -- Data Pipe, prepared once, executed {} time(s)", data.rows.len()),
            true,
            t0.elapsed().as_secs_f64() * 1000.0,
            format!("{} row(s) inserted", data.rows.len()),
        );
        Ok(data.rows.len())
    })();
    match res {
        Ok(n) => {
            db.execute(LogSource::App, "RELEASE \"DATAPIPE\";", &[])?;
            Ok(n)
        }
        Err(e) => {
            let _ = db.execute(LogSource::App, "ROLLBACK TO \"DATAPIPE\";", &[]);
            let _ = db.execute(LogSource::App, "RELEASE \"DATAPIPE\";", &[]);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure); live-server tests are in tests_live below.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_escape_per_dialect() {
        let v = Value::Text("O'Brien \\ path".into());
        assert_eq!(Kind::Postgres.literal(&v, ColClass::Text), "'O''Brien \\ path'");
        assert_eq!(Kind::MySql.literal(&v, ColClass::Text), "'O''Brien \\\\ path'");
        assert_eq!(Kind::MsSql.literal(&v, ColClass::Text), "N'O''Brien \\ path'");
        assert_eq!(Kind::Postgres.literal(&Value::Blob(vec![0xde, 0xad]), ColClass::Blob), "'\\xDEAD'::bytea");
        assert_eq!(Kind::MsSql.literal(&Value::Integer(5), ColClass::Text), "N'5'");
        assert_eq!(Kind::MySql.quote("a`b"), "`a``b`");
        assert_eq!(Kind::MsSql.quote("dbo.t]x"), "[dbo].[t]]x]");
    }

    #[test]
    fn classes_widen_instead_of_failing() {
        let rows = vec![
            vec![Value::Integer(1), Value::Integer(1), Value::Blob(vec![1])],
            vec![Value::Real(2.5), Value::Text("n/a".into()), Value::Null],
        ];
        assert_eq!(infer_class(&rows, 0), ColClass::Real);
        assert_eq!(infer_class(&rows, 1), ColClass::Text);
        assert_eq!(infer_class(&rows, 2), ColClass::Blob);
    }

    #[test]
    fn pulled_decimals_stay_exact() {
        let rows = vec![vec![Value::Text("12.50".into()), Value::Text("123456789012345678901.5".into()), Value::Text("00123".into())]];
        assert_eq!(infer_sqlite_type(&rows, 0), "REAL");
        assert_eq!(infer_sqlite_type(&rows, 1), "TEXT");
        assert_eq!(infer_sqlite_type(&rows, 2), "TEXT");
    }
}

/// Integration tests against real servers. They run only when the matching
/// environment variable is set, e.g.
/// `QUARRY_TEST_PG=host:port:db:user:pass cargo test live`.
#[cfg(test)]
mod tests_live {
    use super::*;

    fn spec(var: &str, kind: Kind) -> Option<ConnSpec> {
        let v = std::env::var(var).ok()?;
        let f: Vec<&str> = v.split(':').collect();
        Some(ConnSpec {
            kind,
            host: f[0].into(),
            port: f[1].parse().ok()?,
            database: f[2].into(),
            user: f[3].into(),
            password: f[4].into(),
            tls: TlsMode::Disable,
            trust_cert: true,
        })
    }

    fn wait(rx: Receiver<Msg>) -> Result<Output> {
        loop {
            match rx.recv().map_err(|e| e.to_string())? {
                Msg::Done(r) => return r,
                _ => {}
            }
        }
    }

    fn round_trip(s: ConnSpec) {
        let mut db = Database::open_in_memory().unwrap();
        db.execute_script(
            "CREATE TABLE src(id INTEGER PRIMARY KEY, name TEXT, amount REAL, code INTEGER, raw BLOB, note TEXT);
             INSERT INTO src VALUES (1, 'O''Brien', 12.5, 7, X'00FF10', 'back\\slash'),
                                    (2, 'Zoë 中文', NULL, 'n/a', NULL, NULL),
                                    (3, NULL, -0.25, 9007199254740993, X'', '');",
            10,
        );
        db.write_changes().unwrap();
        let table = format!("quarry_rt_{}", std::process::id());
        let (data, pk) = read_local(&mut db, "src", false).unwrap();
        let out = wait(spawn(s.clone(), Job::Push { table: table.clone(), data, pk, mode: PushMode::Replace })).unwrap();
        match out {
            Output::Pushed { rows, widened, .. } => {
                assert_eq!(rows, 3);
                assert_eq!(widened, vec!["code".to_string()], "INTEGER column holding 'n/a' widened");
            }
            _ => panic!(),
        }
        // pushing a query result works too
        let (one, _) = read_local(&mut db, "SELECT * FROM src WHERE id = 1", true).unwrap();
        assert!(matches!(
            wait(spawn(s.clone(), Job::Push { table: format!("{table}_x"), data: one, pk: vec![], mode: PushMode::Replace })).unwrap(),
            Output::Pushed { rows: 1, .. }
        ));
        // pull back and compare
        let sql = s.kind.select_all(&table, None) + " ORDER BY " + &s.kind.quote("id");
        let pulled = match wait(spawn(s.clone(), Job::Pull { sql })).unwrap() {
            Output::Pulled(t) => t,
            _ => panic!(),
        };
        let n = write_local(&mut db, "back", &pulled, PullMode::Create).unwrap();
        assert_eq!(n, 3);
        let r = db.query(LogSource::App, "SELECT * FROM back ORDER BY id", &[]).unwrap();
        assert_eq!(r.rows[0][1], Value::Text("O'Brien".into()));
        assert_eq!(r.rows[0][2], Value::Real(12.5));
        assert_eq!(r.rows[0][4], Value::Blob(vec![0, 0xff, 0x10]));
        assert_eq!(r.rows[0][5], Value::Text("back\\slash".into()));
        assert_eq!(r.rows[1][1], Value::Text("Zoë 中文".into()));
        assert_eq!(r.rows[1][3], Value::Text("n/a".into()));
        assert_eq!(r.rows[2][3], Value::Text("9007199254740993".into()), "big ints survive (column widened to text)");
        // listing works and contains the table
        match wait(spawn(s.clone(), Job::ListTables)).unwrap() {
            Output::Tables(t) => assert!(t.iter().any(|x| x.ends_with(&table)), "{t:?}"),
            _ => panic!(),
        }
        // clean up
        let mut remote = connect(&s).unwrap();
        remote.execute(&format!("DROP TABLE {}", s.kind.quote(&table))).unwrap();
        remote.execute(&format!("DROP TABLE {}", s.kind.quote(&format!("{table}_x")))).unwrap();
    }

    #[test]
    fn live_postgres() {
        if let Some(s) = spec("QUARRY_TEST_PG", Kind::Postgres) {
            round_trip(s);
        }
    }
    #[test]
    fn live_mysql() {
        if let Some(s) = spec("QUARRY_TEST_MYSQL", Kind::MySql) {
            round_trip(s);
        }
    }
    #[test]
    fn live_mssql() {
        if let Some(s) = spec("QUARRY_TEST_MSSQL", Kind::MsSql) {
            round_trip(s);
        }
    }
}

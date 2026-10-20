//! PostgreSQL driver for Data Pipe.
//!
//! Values are read with the simple (text) protocol, so every type —
//! numeric, money, uuid, json, arrays, ranges, timestamps — arrives as its
//! canonical text and nothing is lost to a client-side cast. Column types
//! come from a separate `prepare`, and are used to turn integers, floats,
//! booleans and bytea back into the matching SQLite values.

use postgres::{Client, Config, NoTls, SimpleQueryMessage};
use rusqlite::types::Value;

use super::{ConnSpec, Remote, Tabular, TlsMode};
use crate::db::Result;

pub struct Pg {
    client: Client,
}

fn err(e: postgres::Error) -> String {
    // surface the server's message (e.g. "relation x does not exist")
    match e.as_db_error() {
        Some(db) => format!("{}: {}", db.severity(), db.message()),
        None => e.to_string(),
    }
}

impl Pg {
    pub fn connect(spec: &ConnSpec) -> Result<Self> {
        let mut cfg = Config::new();
        cfg.host(&spec.host)
            .port(spec.port)
            .dbname(&spec.database)
            .user(&spec.user)
            .password(&spec.password)
            .application_name("Quarry")
            .connect_timeout(std::time::Duration::from_secs(10));
        let client = match spec.tls {
            TlsMode::Disable => cfg.connect(NoTls).map_err(err)?,
            TlsMode::Require => {
                cfg.ssl_mode(postgres::config::SslMode::Require);
                let connector = native_tls::TlsConnector::builder()
                    .danger_accept_invalid_certs(spec.trust_cert)
                    .danger_accept_invalid_hostnames(spec.trust_cert)
                    .build()
                    .map_err(|e| e.to_string())?;
                cfg.connect(postgres_native_tls::MakeTlsConnector::new(connector)).map_err(err)?
            }
        };
        Ok(Pg { client })
    }
}

fn decode_bytea(s: &str) -> Option<Vec<u8>> {
    let h = s.strip_prefix("\\x")?;
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
        .collect()
}

fn to_value(raw: Option<&str>, ty: &str) -> Value {
    let Some(s) = raw else { return Value::Null };
    match ty {
        "int2" | "int4" | "int8" | "oid" => s.parse().map(Value::Integer).unwrap_or_else(|_| Value::Text(s.into())),
        "float4" | "float8" => match s.parse::<f64>() {
            Ok(f) if f.is_finite() => Value::Real(f),
            _ => Value::Text(s.into()), // NaN / Infinity stay readable
        },
        "bool" => Value::Integer((s == "t") as i64),
        "bytea" => decode_bytea(s).map(Value::Blob).unwrap_or_else(|| Value::Text(s.into())),
        _ => Value::Text(s.into()),
    }
}

impl Remote for Pg {
    fn version(&mut self) -> Result<String> {
        let rows = self.client.simple_query("SELECT version()").map_err(err)?;
        Ok(rows
            .iter()
            .find_map(|m| match m {
                SimpleQueryMessage::Row(r) => r.get(0).map(|s| s.split(" on ").next().unwrap_or(s).to_string()),
                _ => None,
            })
            .unwrap_or_default())
    }

    fn query(&mut self, sql: &str) -> Result<Tabular> {
        // types from the extended protocol; best effort (multi-statement → text)
        let types: Vec<String> = self
            .client
            .prepare(sql)
            .map(|st| st.columns().iter().map(|c| c.type_().name().to_string()).collect())
            .unwrap_or_default();
        let msgs = self.client.simple_query(sql).map_err(err)?;
        let mut out = Tabular::default();
        for m in msgs {
            match m {
                SimpleQueryMessage::RowDescription(cols) if out.columns.is_empty() => {
                    out.columns = cols.iter().map(|c| c.name().to_string()).collect();
                }
                SimpleQueryMessage::Row(r) => {
                    if out.columns.is_empty() {
                        out.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    let n = out.columns.len();
                    out.rows.push((0..n).map(|i| to_value(r.get(i), types.get(i).map(|s| s.as_str()).unwrap_or(""))).collect());
                }
                _ => {}
            }
        }
        out.types = if types.len() == out.columns.len() { types } else { vec![String::new(); out.columns.len()] };
        Ok(out)
    }

    fn execute(&mut self, sql: &str) -> Result<()> {
        self.client.batch_execute(sql).map_err(err)
    }

    fn list_tables(&mut self) -> Result<Vec<String>> {
        let t = self.query(
            "SELECT CASE WHEN table_schema = 'public' THEN table_name ELSE table_schema || '.' || table_name END \
             FROM information_schema.tables \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') AND table_type IN ('BASE TABLE', 'VIEW') \
             ORDER BY table_schema <> 'public', 1",
        )?;
        Ok(t.rows.into_iter().filter_map(|r| match r.into_iter().next() {
            Some(Value::Text(s)) => Some(s),
            _ => None,
        }).collect())
    }

    fn table_columns(&mut self, table: &str) -> Result<Option<Vec<String>>> {
        let (schema, name) = table.split_once('.').unwrap_or(("public", table));
        let esc = |s: &str| s.replace('\'', "''");
        let t = self.query(&format!(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = '{}' AND table_name = '{}' ORDER BY ordinal_position",
            esc(schema),
            esc(name)
        ))?;
        if t.rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(t.rows.into_iter().map(|r| crate::db::as_text(&r[0])).collect()))
    }
}

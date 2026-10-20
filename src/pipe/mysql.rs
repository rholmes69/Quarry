//! MySQL / MariaDB driver for Data Pipe (pure-Rust client, text protocol).
//!
//! DECIMAL arrives as exact text, BIGINT UNSIGNED values beyond i64 stay
//! text, BINARY/BLOB columns (charset 63) become SQLite BLOBs, and dates /
//! JSON / enums come through as the server formats them.

use mysql::consts::{ColumnFlags, ColumnType};
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder, SslOpts};
use rusqlite::types::Value;

use super::{ConnSpec, Remote, Tabular, TlsMode};
use crate::db::Result;

pub struct My {
    conn: Conn,
}

fn err(e: mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(m) => format!("MySQL error {}: {}", m.code, m.message),
        other => other.to_string(),
    }
}

impl My {
    pub fn connect(spec: &ConnSpec) -> Result<Self> {
        let mut opts = OptsBuilder::new()
            .ip_or_hostname(Some(spec.host.clone()))
            .tcp_port(spec.port)
            .user(Some(spec.user.clone()))
            .pass(Some(spec.password.clone()))
            .db_name((!spec.database.is_empty()).then(|| spec.database.clone()))
            .tcp_connect_timeout(Some(std::time::Duration::from_secs(10)));
        if spec.tls == TlsMode::Require {
            opts = opts.ssl_opts(SslOpts::default().with_danger_accept_invalid_certs(spec.trust_cert));
        }
        let mut conn = Conn::new(opts).map_err(err)?;
        // predictable text: UTF-8 everywhere, ANSI-ish behaviour for our literals
        conn.query_drop("SET NAMES utf8mb4").map_err(err)?;
        Ok(My { conn })
    }
}

struct ColMeta {
    name: String,
    ty: ColumnType,
    unsigned: bool,
    binary: bool,
}

fn to_value(v: mysql::Value, c: &ColMeta) -> Value {
    use ColumnType::*;
    let bytes = match v {
        mysql::Value::NULL => return Value::Null,
        mysql::Value::Int(i) => return Value::Integer(i),
        mysql::Value::UInt(u) => return i64::try_from(u).map(Value::Integer).unwrap_or(Value::Text(u.to_string())),
        mysql::Value::Float(f) => return Value::Real(f as f64),
        mysql::Value::Double(f) => return Value::Real(f),
        mysql::Value::Bytes(b) => b,
        other => return Value::Text(other.as_sql(true).trim_matches('\'').to_string()),
    };
    let text = || String::from_utf8_lossy(&bytes).to_string();
    match c.ty {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => {
            let s = text();
            s.parse::<i64>().map(Value::Integer).unwrap_or(Value::Text(s))
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text();
            s.parse::<f64>().ok().filter(|f| f.is_finite()).map(Value::Real).unwrap_or(Value::Text(s))
        }
        MYSQL_TYPE_BIT => Value::Integer(bytes.iter().fold(0i64, |acc, b| (acc << 8) | *b as i64)),
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_GEOMETRY
            if c.binary =>
        {
            Value::Blob(bytes)
        }
        _ => {
            let _ = c.unsigned;
            Value::Text(text())
        }
    }
}

impl Remote for My {
    fn version(&mut self) -> Result<String> {
        let v: Option<String> = self.conn.query_first("SELECT VERSION()").map_err(err)?;
        Ok(format!("MySQL-compatible server {}", v.unwrap_or_default()))
    }

    fn query(&mut self, sql: &str) -> Result<Tabular> {
        let mut res = self.conn.query_iter(sql).map_err(err)?;
        let meta: Vec<ColMeta> = res
            .columns()
            .as_ref()
            .iter()
            .map(|c| ColMeta {
                name: c.name_str().to_string(),
                ty: c.column_type(),
                unsigned: c.flags().contains(ColumnFlags::UNSIGNED_FLAG),
                // charset 63 = binary
                binary: c.character_set() == 63 && c.flags().contains(ColumnFlags::BINARY_FLAG),
            })
            .collect();
        let mut out = Tabular {
            columns: meta.iter().map(|m| m.name.clone()).collect(),
            types: meta.iter().map(|m| format!("{:?}", m.ty).trim_start_matches("MYSQL_TYPE_").to_lowercase()).collect(),
            rows: vec![],
        };
        if let Some(set) = res.iter() {
            for row in set {
                let row = row.map_err(err)?;
                out.rows.push(row.unwrap().into_iter().zip(&meta).map(|(v, m)| to_value(v, m)).collect());
            }
        }
        Ok(out)
    }

    fn execute(&mut self, sql: &str) -> Result<()> {
        self.conn.query_drop(sql).map_err(err)
    }

    fn list_tables(&mut self) -> Result<Vec<String>> {
        let names: Vec<String> = self
            .conn
            .query("SELECT CAST(table_name AS CHAR) FROM information_schema.tables WHERE table_schema = DATABASE() ORDER BY 1")
            .map_err(err)?;
        Ok(names)
    }

    fn table_columns(&mut self, table: &str) -> Result<Option<Vec<String>>> {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "''");
        let (schema, name) = match table.split_once('.') {
            Some((s, n)) => (format!("'{}'", esc(s)), n),
            None => ("DATABASE()".to_string(), table),
        };
        let cols: Vec<String> = self
            .conn
            .query(format!(
                "SELECT CAST(column_name AS CHAR) FROM information_schema.columns WHERE table_schema = {schema} AND table_name = '{}' ORDER BY ordinal_position",
                esc(name)
            ))
            .map_err(err)?;
        Ok((!cols.is_empty()).then_some(cols))
    }
}

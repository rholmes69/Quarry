//! Microsoft SQL Server / Azure SQL driver for Data Pipe (tiberius, TDS 7.3+).
//!
//! Everything is sent as plain SQL batches (not sp_executesql) so explicit
//! BEGIN TRANSACTION / COMMIT around bulk inserts behave as expected.
//! DECIMAL/NUMERIC/MONEY come back as exact text, UNIQUEIDENTIFIER as its
//! canonical string, date/time types as ISO-8601 text.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use rusqlite::types::Value;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{ConnSpec, Remote, Tabular, TlsMode};
use crate::db::Result;

pub struct Ms {
    rt: tokio::runtime::Runtime,
    client: Client<Compat<TcpStream>>,
}

fn err(e: tiberius::error::Error) -> String {
    match e {
        tiberius::error::Error::Server(t) => format!("SQL Server error {}: {}", t.code(), t.message()),
        other => other.to_string(),
    }
}

impl Ms {
    pub fn connect(spec: &ConnSpec) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build().map_err(|e| e.to_string())?;
        let mut cfg = Config::new();
        cfg.host(&spec.host);
        cfg.port(spec.port);
        if !spec.database.is_empty() {
            cfg.database(&spec.database);
        }
        cfg.application_name("Quarry");
        cfg.authentication(AuthMethod::sql_server(&spec.user, &spec.password));
        // "Disable" still encrypts the login packet (SQL Server's default);
        // "Require" encrypts the whole session.
        cfg.encryption(match spec.tls {
            TlsMode::Require => EncryptionLevel::Required,
            TlsMode::Disable => EncryptionLevel::Off,
        });
        if spec.trust_cert {
            cfg.trust_cert();
        }
        let client = rt.block_on(async {
            let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(cfg.get_addr()))
                .await
                .map_err(|_| format!("Timed out connecting to {}", cfg.get_addr()))?
                .map_err(|e| e.to_string())?;
            tcp.set_nodelay(true).map_err(|e| e.to_string())?;
            Client::connect(cfg, tcp.compat_write()).await.map_err(err)
        })?;
        Ok(Ms { rt, client })
    }
}

fn to_value(cd: ColumnData<'static>) -> Value {
    fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        v.map(f).unwrap_or(Value::Null)
    }
    let chrono_text = |cd: &ColumnData<'static>| -> Value {
        if let Ok(Some(dt)) = NaiveDateTime::from_sql(cd) {
            return Value::Text(dt.format("%Y-%m-%d %H:%M:%S%.f").to_string().trim_end_matches(".").to_string());
        }
        if let Ok(Some(d)) = NaiveDate::from_sql(cd) {
            return Value::Text(d.format("%Y-%m-%d").to_string());
        }
        if let Ok(Some(t)) = NaiveTime::from_sql(cd) {
            return Value::Text(t.format("%H:%M:%S%.f").to_string());
        }
        if let Ok(Some(t)) = DateTime::<FixedOffset>::from_sql(cd) {
            return Value::Text(t.to_rfc3339());
        }
        Value::Null
    };
    match cd {
        ColumnData::U8(v) => opt(v, |x| Value::Integer(x as i64)),
        ColumnData::I16(v) => opt(v, |x| Value::Integer(x as i64)),
        ColumnData::I32(v) => opt(v, |x| Value::Integer(x as i64)),
        ColumnData::I64(v) => opt(v, Value::Integer),
        ColumnData::F32(v) => opt(v, |x| Value::Real(x as f64)),
        ColumnData::F64(v) => opt(v, Value::Real),
        ColumnData::Bit(v) => opt(v, |x| Value::Integer(x as i64)),
        ColumnData::String(v) => opt(v, |s| Value::Text(s.into_owned())),
        ColumnData::Guid(v) => opt(v, |g| Value::Text(g.to_string().to_uppercase())),
        ColumnData::Binary(v) => opt(v, |b| Value::Blob(b.into_owned())),
        ColumnData::Numeric(v) => opt(v, |n| Value::Text(n.to_string())),
        ColumnData::Xml(v) => opt(v, |x| Value::Text(x.into_owned().into_string())),
        other => chrono_text(&other),
    }
}

impl Ms {
    fn run(&mut self, sql: &str) -> Result<Tabular> {
        let Ms { rt, client } = self;
        rt.block_on(async {
            let mut stream = client.simple_query(sql).await.map_err(err)?;
            let cols: Vec<(String, String)> = stream
                .columns()
                .await
                .map_err(err)?
                .map(|c| c.iter().map(|c| (c.name().to_string(), format!("{:?}", c.column_type()))).collect())
                .unwrap_or_default();
            let rows = stream.into_first_result().await.map_err(err)?;
            Ok(Tabular {
                columns: cols.iter().map(|c| c.0.clone()).collect(),
                types: cols.iter().map(|c| c.1.clone()).collect(),
                rows: rows.into_iter().map(|r| r.into_iter().map(to_value).collect()).collect(),
            })
        })
    }
}

impl Remote for Ms {
    fn version(&mut self) -> Result<String> {
        let t = self.run("SELECT @@VERSION")?;
        Ok(t.rows.first().map(|r| crate::db::as_text(&r[0])).unwrap_or_default().lines().next().unwrap_or("").to_string())
    }

    fn query(&mut self, sql: &str) -> Result<Tabular> {
        self.run(sql)
    }

    fn execute(&mut self, sql: &str) -> Result<()> {
        let Ms { rt, client } = self;
        rt.block_on(async {
            client.simple_query(sql).await.map_err(err)?.into_results().await.map_err(err)?;
            Ok(())
        })
    }

    fn list_tables(&mut self) -> Result<Vec<String>> {
        let t = self.run(
            "SELECT CASE WHEN TABLE_SCHEMA = 'dbo' THEN TABLE_NAME ELSE TABLE_SCHEMA + '.' + TABLE_NAME END \
             FROM INFORMATION_SCHEMA.TABLES ORDER BY CASE WHEN TABLE_SCHEMA = 'dbo' THEN 0 ELSE 1 END, 1",
        )?;
        Ok(t.rows.iter().map(|r| crate::db::as_text(&r[0])).collect())
    }

    fn table_columns(&mut self, table: &str) -> Result<Option<Vec<String>>> {
        let (schema, name) = table.split_once('.').unwrap_or(("dbo", table));
        let esc = |s: &str| s.replace('\'', "''");
        let t = self.run(&format!(
            "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = N'{}' AND TABLE_NAME = N'{}' ORDER BY ORDINAL_POSITION",
            esc(schema),
            esc(name)
        ))?;
        Ok((!t.rows.is_empty()).then(|| t.rows.iter().map(|r| crate::db::as_text(&r[0])).collect()))
    }
}

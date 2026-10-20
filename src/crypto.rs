//! Encrypted databases (SQLite3 Multiple Ciphers).
//!
//! Supported formats, all readable *and* writable:
//!
//! | Cipher            | Compatible with                                   |
//! |-------------------|---------------------------------------------------|
//! | `chacha20`        | sqleet; SQLite3MC default                         |
//! | `aes128cbc`       | wxSQLite3 AES-128 (the original wxSQLite3 codec)  |
//! | `aes256cbc`       | wxSQLite3 AES-256                                 |
//! | `sqlcipher` v1–4  | SQLCipher 1.x, 2.x, 3.x, 4.x (Zetetic)             |
//! | `rc4`             | System.Data.SQLite (legacy)                       |
//! | `ascon128`        | Ascon-128 (NIST lightweight standard)             |
//! | `aegis`           | AEGIS                                             |
//!
//! Passwords are passed to SQLite and dropped; Quarry never stores them and
//! the SQL log shows key pragmas with the secret masked.

use std::path::Path;

use crate::db::{Database, LogSource, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cipher {
    ChaCha20,
    Aes128,
    Aes256,
    /// SQLCipher, with its major version (1–4) for the KDF/HMAC/page layout.
    SqlCipher(u8),
    Rc4,
    Ascon128,
    Aegis,
}

impl Cipher {
    pub const ALL: [Cipher; 10] = [
        Cipher::ChaCha20,
        Cipher::SqlCipher(4),
        Cipher::SqlCipher(3),
        Cipher::SqlCipher(2),
        Cipher::SqlCipher(1),
        Cipher::Aes256,
        Cipher::Aes128,
        Cipher::Ascon128,
        Cipher::Aegis,
        Cipher::Rc4,
    ];

    pub fn pragma_name(self) -> &'static str {
        match self {
            Cipher::ChaCha20 => "chacha20",
            Cipher::Aes128 => "aes128cbc",
            Cipher::Aes256 => "aes256cbc",
            Cipher::SqlCipher(_) => "sqlcipher",
            Cipher::Rc4 => "rc4",
            Cipher::Ascon128 => "ascon128",
            Cipher::Aegis => "aegis",
        }
    }

    pub fn label(self) -> String {
        match self {
            Cipher::ChaCha20 => "ChaCha20-Poly1305 (sqleet) — recommended".into(),
            Cipher::Aes128 => "AES-128-CBC (wxSQLite3)".into(),
            Cipher::Aes256 => "AES-256-CBC (wxSQLite3)".into(),
            Cipher::SqlCipher(v) => format!("SQLCipher {v}.x (AES-256 + HMAC)"),
            Cipher::Rc4 => "RC4 (System.Data.SQLite, legacy — weak)".into(),
            Cipher::Ascon128 => "Ascon-128".into(),
            Cipher::Aegis => "AEGIS".into(),
        }
    }

    pub fn short(self) -> String {
        match self {
            Cipher::SqlCipher(v) => format!("SQLCipher v{v}"),
            Cipher::ChaCha20 => "ChaCha20".into(),
            Cipher::Aes128 => "AES-128 (wxSQLite3)".into(),
            Cipher::Aes256 => "AES-256 (wxSQLite3)".into(),
            Cipher::Rc4 => "RC4".into(),
            Cipher::Ascon128 => "Ascon-128".into(),
            Cipher::Aegis => "AEGIS".into(),
        }
    }

    /// SQLCipher 1–3 always used 1024-byte pages.
    pub fn required_page_size(self) -> Option<i64> {
        match self {
            Cipher::SqlCipher(v) if v <= 3 => Some(1024),
            _ => None,
        }
    }

    /// Statements that select this cipher for the *next* key/rekey.
    fn configure_sql(self) -> String {
        match self {
            Cipher::SqlCipher(v) => format!("PRAGMA cipher = 'sqlcipher'; PRAGMA legacy = {v};"),
            c => format!("PRAGMA cipher = '{}';", c.pragma_name()),
        }
    }
}

/// A password, or a raw key given as hex (SQLCipher "raw key" style).
#[derive(Clone)]
pub enum Secret {
    Password(String),
    Hex(String),
}

impl Secret {
    fn pragma(&self, rekey: bool) -> String {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        match (self, rekey) {
            (Secret::Password(p), false) => format!("PRAGMA key = {};", q(p)),
            (Secret::Password(p), true) => format!("PRAGMA rekey = {};", q(p)),
            (Secret::Hex(h), false) => format!("PRAGMA hexkey = {};", q(h)),
            (Secret::Hex(h), true) => format!("PRAGMA hexrekey = {};", q(h)),
        }
    }
}

/// Mask key material: string literals that follow a KEY / REKEY / HEXKEY /
/// HEXREKEY keyword (PRAGMA or ATTACH … KEY), and URI `key=` parameters.
pub fn redact(sql: &str) -> String {
    if !sql.to_ascii_uppercase().contains("KEY") {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        let mut lit = String::new();
        while let Some(ch) = chars.next() {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    lit.push('\'');
                    continue;
                }
                break;
            }
            lit.push(ch);
        }
        let before = out.trim_end().trim_end_matches(['=', '(']).trim_end().to_ascii_uppercase();
        let secret = before.ends_with("KEY") || lit.to_ascii_lowercase().contains("key=");
        if secret {
            out.push_str("'••••••'");
        } else {
            out.push('\'');
            out.push_str(&lit.replace('\'', "''"));
            out.push('\'');
        }
    }
    out
}

/// Plain SQLite files start with this header; anything else non-empty is
/// encrypted (or not a database at all).
pub fn looks_encrypted(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; 16];
    match f.read(&mut head) {
        Ok(0) => false,
        Ok(n) if n < 16 => true,
        Ok(_) => &head != b"SQLite format 3\0",
        Err(_) => false,
    }
}

impl Database {
    /// Open with a key. `cipher: None` tries every supported format.
    pub fn open_encrypted(path: &Path, read_only: bool, secret: &Secret, cipher: Option<Cipher>) -> Result<Self> {
        let candidates: Vec<Cipher> = match cipher {
            Some(c) => vec![c],
            None => Cipher::ALL.to_vec(),
        };
        let mut last_err = String::new();
        for c in candidates {
            let mut db = Database::open_raw(path, read_only)?;
            match db.apply_key(c, secret) {
                Ok(()) => {
                    db.encryption = Some(c);
                    return Ok(db);
                }
                Err(e) => last_err = e,
            }
        }
        Err(if cipher.is_none() {
            "Wrong password, or the file uses a cipher configuration Quarry couldn't detect. \
             Try picking the cipher explicitly (e.g. an older SQLCipher version)."
                .into()
        } else {
            format!("Could not open with {}: {last_err}", cipher.unwrap().short())
        })
    }

    fn apply_key(&mut self, c: Cipher, secret: &Secret) -> Result<()> {
        let t0 = std::time::Instant::now();
        let sql = format!("{} {}", c.configure_sql(), secret.pragma(false));
        let r = self.conn.execute_batch(&sql).map_err(|e| e.to_string());
        // a wrong key only shows up on first read
        let r = r.and_then(|_| {
            self.conn
                .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get::<_, i64>(0))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        self.push_log(LogSource::App, &sql, r.is_ok(), t0.elapsed().as_secs_f64() * 1000.0, match &r {
            Ok(()) => format!("unlocked with {}", c.short()),
            Err(e) => e.clone(),
        });
        r
    }

    /// Encrypt a plain database, change the password, or switch cipher.
    /// `secret = None` removes encryption.
    pub fn rekey(&mut self, cipher: Cipher, secret: Option<&Secret>) -> Result<()> {
        if self.read_only {
            return Err("Database is opened read-only.".into());
        }
        if self.has_pending_changes() {
            return Err("Write or revert pending changes first.".into());
        }
        if self.path.as_os_str() == ":memory:" {
            return Err("In-memory databases can't be encrypted.".into());
        }
        // SQLite3MC can't rekey in WAL mode: switch out, rekey, switch back.
        let mode = crate::db::as_text(&self.get_pragma("journal_mode")?).to_lowercase();
        let was_wal = mode == "wal";
        if was_wal {
            self.set_pragma("journal_mode", "DELETE")?;
        }
        if let (Some(ps), Some(_)) = (cipher.required_page_size(), secret) {
            let cur = crate::db::as_int(&self.get_pragma("page_size")?);
            if cur != ps {
                if self.encryption.is_some() {
                    if was_wal {
                        let _ = self.set_pragma("journal_mode", "WAL");
                    }
                    return Err(format!(
                        "{} needs {ps}-byte pages but this database uses {cur}. Remove encryption first, then encrypt with {}                          (Quarry won't silently write a plaintext copy).",
                        cipher.short(),
                        cipher.short()
                    ));
                }
                // still plaintext: rebuild with the page size the format requires
                self.set_pragma("page_size", &ps.to_string())?;
                self.execute(LogSource::App, "VACUUM;", &[])?;
            }
        }
        let t0 = std::time::Instant::now();
        let sql = match secret {
            Some(s) => format!("{} {}", cipher.configure_sql(), s.pragma(true)),
            None => "PRAGMA rekey = '';".to_string(),
        };
        let r = self.conn.execute_batch(&sql).map_err(|e| e.to_string());
        self.push_log(LogSource::App, &sql, r.is_ok(), t0.elapsed().as_secs_f64() * 1000.0, match &r {
            Ok(()) => match secret {
                Some(_) => format!("encrypted with {}", cipher.short()),
                None => "encryption removed".into(),
            },
            Err(e) => e.clone(),
        });
        if was_wal {
            let _ = self.set_pragma("journal_mode", "WAL");
        }
        r?;
        self.encryption = secret.map(|_| cipher);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    fn make(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut db = Database::open(&p, false).unwrap();
        db.execute_script("CREATE TABLE t(id INTEGER PRIMARY KEY, secret TEXT); INSERT INTO t(secret) VALUES ('top secret 42');", 10);
        db.write_changes().unwrap();
        p
    }

    fn contains_plaintext(p: &Path) -> bool {
        let bytes = std::fs::read(p).unwrap();
        bytes.windows(13).any(|w| w == b"top secret 42")
    }

    #[test]
    fn every_cipher_round_trips_and_autodetects() {
        let dir = tempfile::tempdir().unwrap();
        for (i, c) in Cipher::ALL.iter().enumerate() {
            let p = make(&dir, &format!("db{i}.db"));
            assert!(contains_plaintext(&p));
            let mut db = Database::open(&p, false).unwrap();
            db.rekey(*c, Some(&Secret::Password("pä$$ 'word".into()))).unwrap();
            drop(db);
            assert!(looks_encrypted(&p), "{c:?}");
            assert!(!contains_plaintext(&p), "{c:?} left plaintext on disk");
            assert!(Database::open(&p, false).is_err(), "opening without a key must fail");
            let mut db = Database::open_encrypted(&p, false, &Secret::Password("pä$$ 'word".into()), None).unwrap();
            assert_eq!(db.encryption, Some(*c), "auto-detect picked the right cipher");
            let r = db.query(LogSource::App, "SELECT secret FROM t", &[]).unwrap();
            assert_eq!(r.rows[0][0], Value::Text("top secret 42".into()));
            assert!(Database::open_encrypted(&p, false, &Secret::Password("wrong".into()), Some(*c)).is_err());
        }
    }

    #[test]
    fn change_password_switch_cipher_and_decrypt_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = make(&dir, "w.db");
        let mut db = Database::open(&p, false).unwrap();
        db.set_pragma("journal_mode", "WAL").unwrap();
        db.rekey(Cipher::Aes256, Some(&Secret::Password("one".into()))).unwrap();
        assert_eq!(crate::db::as_text(&db.get_pragma("journal_mode").unwrap()).to_lowercase(), "wal");
        db.rekey(Cipher::SqlCipher(4), Some(&Secret::Password("two".into()))).unwrap();
        drop(db);
        assert!(Database::open_encrypted(&p, false, &Secret::Password("one".into()), None).is_err());
        let mut db = Database::open_encrypted(&p, false, &Secret::Password("two".into()), None).unwrap();
        assert_eq!(db.encryption, Some(Cipher::SqlCipher(4)));
        db.rekey(Cipher::ChaCha20, None).unwrap();
        drop(db);
        assert!(!looks_encrypted(&p));
        assert!(contains_plaintext(&p) || Database::open(&p, true).is_ok());
    }

    #[test]
    fn brand_new_encrypted_database() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.db");
        let mut db = Database::open(&p, false).unwrap();
        db.rekey(Cipher::ChaCha20, Some(&Secret::Password("s3cret".into()))).unwrap();
        db.execute_script("CREATE TABLE notes(t TEXT); INSERT INTO notes VALUES ('top secret 42');", 10);
        db.write_changes().unwrap();
        drop(db);
        assert!(looks_encrypted(&p) && !contains_plaintext(&p));
        let err = Database::open(&p, false).err().unwrap();
        assert!(err.contains("encrypted"), "{err}");
        let mut db = Database::open_encrypted(&p, false, &Secret::Password("s3cret".into()), None).unwrap();
        assert_eq!(db.row_count("notes", "", &[]).unwrap(), 1);
    }

    #[test]
    fn hex_keys_work() {
        let dir = tempfile::tempdir().unwrap();
        let p = make(&dir, "h.db");
        let key = Secret::Hex("2DD29CA851E7B56E4697B0E1F08507293D761A05CE4D1B628663F411A8086D99".into());
        Database::open(&p, false).unwrap().rekey(Cipher::SqlCipher(4), Some(&key)).unwrap();
        assert!(Database::open_encrypted(&p, true, &key, None).is_ok());
    }

    #[test]
    fn secrets_never_reach_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = make(&dir, "l.db");
        let mut db = Database::open(&p, false).unwrap();
        db.rekey(Cipher::ChaCha20, Some(&Secret::Password("hunter2".into()))).unwrap();
        db.execute_script("PRAGMA rekey = 'hunter3';", 10);
        assert!(db.log.iter().all(|l| !l.sql.contains("hunter")), "{:#?}", db.log);
        assert!(db.log.iter().any(|l| l.sql.contains("PRAGMA rekey = '••••••'")));
        assert_eq!(redact("ATTACH 'x.db' AS x KEY 'it''s'"), "ATTACH 'x.db' AS x KEY '••••••'");
        assert_eq!(redact("ATTACH 'file:y.db?cipher=aes256cbc&key=abc' AS y"), "ATTACH '••••••' AS y");
        assert_eq!(redact("PRAGMA cipher = 'sqlcipher'; PRAGMA key='x'"), "PRAGMA cipher = 'sqlcipher'; PRAGMA key='••••••'");
        assert_eq!(redact("PRAGMA hexkey(\'00ff\')"), "PRAGMA hexkey('••••••')");
        assert_eq!(redact("SELECT 'key'"), "SELECT 'key'");
    }

    /// Interop with Zetetic's own SQLCipher, when its CLI is installed.
    #[test]
    fn interop_with_real_sqlcipher_cli() {
        let Ok(out) = std::process::Command::new("sqlcipher").arg("-version").output() else { return };
        if !out.status.success() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1. SQLCipher writes, Quarry reads (auto-detect)
        let p = dir.path().join("from_sqlcipher.db");
        let script = "PRAGMA key = 'zetetic';\nCREATE TABLE notes(body TEXT);\nINSERT INTO notes VALUES ('made by sqlcipher');\n";
        let mut child = std::process::Command::new("sqlcipher")
            .arg(&p)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
        let mut db = Database::open_encrypted(&p, true, &Secret::Password("zetetic".into()), None).unwrap();
        assert_eq!(db.encryption, Some(Cipher::SqlCipher(4)));
        let r = db.query(LogSource::App, "SELECT body FROM notes", &[]).unwrap();
        assert_eq!(r.rows[0][0], Value::Text("made by sqlcipher".into()));
        drop(db);

        // 2. Quarry writes SQLCipher v4, SQLCipher reads
        let q = make(&dir, "from_quarry.db");
        Database::open(&q, false).unwrap().rekey(Cipher::SqlCipher(4), Some(&Secret::Password("quarry".into()))).unwrap();
        let mut child = std::process::Command::new("sqlcipher")
            .arg(&q)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"PRAGMA key = 'quarry';\nSELECT secret FROM t;\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("top secret 42"), "{:?}", out);
    }
}

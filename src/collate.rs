//! Custom collations.
//!
//! Every connection Quarry opens gets:
//!
//! * **Built-ins** — `NATURAL`, `NATURAL_NOCASE` (file2 < file10),
//!   `UNICODE_NOCASE` (case-insensitive for all scripts, unlike SQLite's
//!   ASCII-only NOCASE), `UNICODE_CI_AI` (also ignores accents: é = e),
//!   `NUMERIC` (numbers by value, then text).
//! * **User collations** — defined in the Collations manager from a base
//!   behaviour plus an optional explicit ranking ("Low, Medium, High"),
//!   descending order and whitespace trimming. Saved with Quarry's settings
//!   (portable-aware) and registered on every database.
//! * **A safety net for unknown collations.** Databases created by other
//!   apps often use collations only that app defines (Android's `LOCALIZED`
//!   and `UNICODE`, app-specific ones…). Plain SQLite refuses to query any
//!   table or index that uses them ("no such collation sequence"). Quarry
//!   registers a stand-in on demand so the data stays readable, and lists
//!   every stand-in so you know which orderings are approximations.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock, RwLock};

use rusqlite::Connection;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

// ---------------------------------------------------------------------------
// Comparison primitives
// ---------------------------------------------------------------------------

fn fold_case(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

fn fold_accents(s: &str) -> String {
    s.nfd().filter(|c| !is_combining_mark(*c)).collect()
}

/// Natural ("human") order: digit runs compare by numeric value.
pub fn natural(a: &str, b: &str, nocase: bool) -> Ordering {
    let (a, b) = if nocase { (fold_case(a), fold_case(b)) } else { (a.to_string(), b.to_string()) };
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(ch) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(ch);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut x), take(&mut y));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| na.len().cmp(&nb.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

fn leading_number(s: &str) -> Option<f64> {
    let t = s.trim();
    let end = t
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || *c == '.' || ((*c == '-' || *c == '+') && *i == 0) || *c == 'e' || *c == 'E')
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    t[..end].parse().ok()
}

/// Numbers by value (anywhere a leading number parses), then text.
pub fn numeric(a: &str, b: &str) -> Ordering {
    match (leading_number(a), leading_number(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then_with(|| a.cmp(b)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

// ---------------------------------------------------------------------------
// User-defined collations
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Binary,
    NoCase,
    NoCaseNoAccent,
    Natural,
    NaturalNoCase,
    Numeric,
}

impl Base {
    pub const ALL: [Base; 6] = [Base::Binary, Base::NoCase, Base::NoCaseNoAccent, Base::Natural, Base::NaturalNoCase, Base::Numeric];
    pub fn label(self) -> &'static str {
        match self {
            Base::Binary => "Exact (binary)",
            Base::NoCase => "Ignore case (Unicode)",
            Base::NoCaseNoAccent => "Ignore case & accents",
            Base::Natural => "Natural (file2 < file10)",
            Base::NaturalNoCase => "Natural, ignore case",
            Base::Numeric => "Numeric value",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Base::Binary => "binary",
            Base::NoCase => "nocase",
            Base::NoCaseNoAccent => "ci_ai",
            Base::Natural => "natural",
            Base::NaturalNoCase => "natural_nocase",
            Base::Numeric => "numeric",
        }
    }
    fn from_key(s: &str) -> Base {
        Base::ALL.into_iter().find(|b| b.key() == s).unwrap_or(Base::Binary)
    }
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        match self {
            Base::Binary => a.cmp(b),
            Base::NoCase => fold_case(a).cmp(&fold_case(b)),
            Base::NoCaseNoAccent => fold_case(&fold_accents(a)).cmp(&fold_case(&fold_accents(b))),
            Base::Natural => natural(a, b, false),
            Base::NaturalNoCase => natural(a, b, true),
            Base::Numeric => numeric(a, b),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CollationDef {
    pub name: String,
    pub base: Base,
    /// Values ranked first, in this order (matched with the base rule).
    pub order: Vec<String>,
    pub descending: bool,
    pub trim: bool,
}

impl CollationDef {
    pub fn new(name: &str) -> Self {
        CollationDef { name: name.into(), base: Base::NoCase, order: vec![], descending: false, trim: false }
    }

    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        let (a, b) = if self.trim { (a.trim(), b.trim()) } else { (a, b) };
        let rank = |v: &str| self.order.iter().position(|o| self.base.compare(o, v) == Ordering::Equal);
        let ord = match (rank(a), rank(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.base.compare(a, b),
        };
        if self.descending { ord.reverse() } else { ord }
    }

    fn to_line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '|'], " ");
        format!(
            "{}\t{}\t{}\t{}\t{}\n",
            clean(&self.name),
            self.base.key(),
            self.order.iter().map(|s| clean(s)).collect::<Vec<_>>().join("|"),
            self.descending as u8,
            self.trim as u8
        )
    }

    fn from_line(l: &str) -> Option<Self> {
        let f: Vec<&str> = l.trim_end_matches(['\n', '\r']).split('\t').collect();
        if f.len() < 5 || f[0].trim().is_empty() {
            return None;
        }
        Some(CollationDef {
            name: f[0].to_string(),
            base: Base::from_key(f[1]),
            order: f[2].split('|').filter(|s| !s.is_empty()).map(String::from).collect(),
            descending: f[3] == "1",
            trim: f[4] == "1",
        })
    }
}

pub const BUILTINS: [(&str, &str); 5] = [
    ("NATURAL", "Digits compare by value: item2 < item10"),
    ("NATURAL_NOCASE", "Natural order, ignoring case"),
    ("UNICODE_NOCASE", "Case-insensitive for all scripts (SQLite's NOCASE is ASCII only)"),
    ("UNICODE_CI_AI", "Ignores case and accents: Émile = emile"),
    ("NUMERIC", "Leading numbers by value (2 < 10 < 10.5), then text"),
];

fn registry() -> &'static RwLock<Vec<CollationDef>> {
    static R: OnceLock<RwLock<Vec<CollationDef>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(load()))
}

fn fallbacks_used() -> &'static Mutex<BTreeSet<String>> {
    static F: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Comparator used for collations the database needs but nobody defined.
fn fallback_base() -> &'static RwLock<Base> {
    static B: OnceLock<RwLock<Base>> = OnceLock::new();
    B.get_or_init(|| {
        RwLock::new(crate::config::setting("collation_fallback").map(|k| Base::from_key(&k)).unwrap_or(Base::NoCase))
    })
}

pub fn fallback() -> Base {
    *fallback_base().read().unwrap()
}

pub fn set_fallback(b: Base) {
    *fallback_base().write().unwrap() = b;
    crate::config::set_setting("collation_fallback", b.key());
}

fn load() -> Vec<CollationDef> {
    crate::config::read("collations.tsv").unwrap_or_default().lines().filter_map(CollationDef::from_line).collect()
}

pub fn user_defs() -> Vec<CollationDef> {
    registry().read().unwrap().clone()
}

pub fn save_user_defs(defs: Vec<CollationDef>) -> Result<(), String> {
    let body: String = defs.iter().map(CollationDef::to_line).collect();
    *registry().write().unwrap() = defs;
    crate::config::write("collations.tsv", &body)
}

pub fn fallbacks() -> Vec<String> {
    fallbacks_used().lock().unwrap().iter().cloned().collect()
}

/// Register all collations on a connection.
pub fn install(conn: &Connection) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    conn.create_collation("NATURAL", |a: &str, b: &str| natural(a, b, false)).map_err(e)?;
    conn.create_collation("NATURAL_NOCASE", |a: &str, b: &str| natural(a, b, true)).map_err(e)?;
    conn.create_collation("UNICODE_NOCASE", |a: &str, b: &str| Base::NoCase.compare(a, b)).map_err(e)?;
    conn.create_collation("UNICODE_CI_AI", |a: &str, b: &str| Base::NoCaseNoAccent.compare(a, b)).map_err(e)?;
    conn.create_collation("NUMERIC", numeric).map_err(e)?;
    for d in user_defs() {
        let def = d.clone();
        conn.create_collation(d.name.as_str(), move |a: &str, b: &str| def.compare(a, b)).map_err(e)?;
    }
    conn.collation_needed(|c: &Connection, name: &str| {
        fallbacks_used().lock().unwrap().insert(name.to_string());
        let base = fallback();
        c.create_collation(name, move |a: &str, b: &str| base.compare(a, b))
    })
    .map_err(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, LogSource};
    use rusqlite::types::Value;

    fn col(db: &mut Database, sql: &str) -> Vec<String> {
        db.query(LogSource::App, sql, &[]).unwrap().rows.into_iter().map(|r| crate::db::as_text(&r[0])).collect()
    }

    #[test]
    fn comparators() {
        assert_eq!(natural("file2", "file10", false), Ordering::Less);
        assert_eq!(natural("v1.9", "v1.10", false), Ordering::Less);
        assert_eq!(natural("a01", "a1", false), Ordering::Greater, "leading zeros break ties deterministically");
        assert_eq!(Base::NoCase.compare("ÉCOLE", "école"), Ordering::Equal);
        assert_eq!(Base::NoCaseNoAccent.compare("Émile", "emile"), Ordering::Equal);
        assert_eq!(numeric("10 kg", "9 kg"), Ordering::Greater);
        assert_eq!(numeric("abc", "5"), Ordering::Greater);
    }

    #[test]
    fn builtins_work_in_sql_and_indexes() {
        let mut db = Database::open_in_memory().unwrap();
        db.execute_script(
            "CREATE TABLE f(n TEXT COLLATE NATURAL_NOCASE);
             INSERT INTO f VALUES ('Item10'),('item2'),('ITEM1');
             CREATE INDEX f_n ON f(n);",
            10,
        );
        assert_eq!(col(&mut db, "SELECT n FROM f ORDER BY n"), vec!["ITEM1", "item2", "Item10"]);
        assert_eq!(col(&mut db, "SELECT count(*) FROM f WHERE n = 'item10'"), vec!["1"]);
        let r = db.query(LogSource::App, "SELECT 'Ärger' = 'arger' COLLATE UNICODE_CI_AI", &[]).unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(1));
    }

    #[test]
    fn user_defined_ranking() {
        let d = CollationDef { name: "PRIORITY".into(), base: Base::NoCase, order: vec!["low".into(), "medium".into(), "high".into()], descending: true, trim: true };
        let back = CollationDef::from_line(&d.to_line()).unwrap();
        assert_eq!(back, d);
        let mut v = vec!["Low", "other", " HIGH ", "medium"];
        v.sort_by(|a, b| d.compare(a, b));
        assert_eq!(v, vec!["other", " HIGH ", "medium", "Low"]);
    }

    #[test]
    fn unknown_collations_get_a_stand_in() {
        // Build a file with a collation only "another app" knows.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("android.db");
        {
            let c = Connection::open(&p).unwrap();
            c.create_collation("LOCALIZED", |a: &str, b: &str| a.cmp(b)).unwrap();
            c.execute_batch("CREATE TABLE contacts(name TEXT COLLATE LOCALIZED); CREATE INDEX c_name ON contacts(name); INSERT INTO contacts VALUES ('bob'),('Alice');").unwrap();
        }
        // Plain SQLite (no stand-in) can't even sort it:
        let plain = Connection::open(&p).unwrap();
        assert!(plain.prepare("SELECT name FROM contacts ORDER BY name").is_err());
        // Quarry can:
        let mut db = Database::open(&p, false).unwrap();
        assert_eq!(col(&mut db, "SELECT name FROM contacts ORDER BY name"), vec!["Alice", "bob"]);
        assert!(fallbacks().contains(&"LOCALIZED".to_string()));
    }
}

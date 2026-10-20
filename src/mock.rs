//! Mock data generation.
//!
//! * A generator is picked per column from its name, declared type, default
//!   and constraints (e.g. `email` → Email, `*_id REFERENCES x` → sample
//!   existing keys from `x`, `INTEGER PRIMARY KEY` → let SQLite assign it).
//! * Foreign keys draw from rows that really exist in the parent table, and
//!   multi-table runs go parents-first so children always have keys to use.
//! * UNIQUE / PRIMARY KEY groups (including composite ones) are enforced,
//!   seeded with the values already in the table.
//! * Values come out typed for the column's affinity, so STRICT tables work.
//! * A seed makes every run reproducible.
//!
//! Emails use the reserved example.com/.org/.net domains, phone numbers the
//! fictional 555-01xx range and SSN-like patterns a 000 prefix, so mock data
//! can never collide with a real person.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use rusqlite::types::Value;

use crate::csv_io::{affinity_of, convert, Affinity};
use crate::db::{quote_ident, Database, LogSource, Result, TableDetails};

// ---------------------------------------------------------------------------
// RNG (SplitMix64 — tiny, fast, reproducible across platforms)
// ---------------------------------------------------------------------------

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi - lo) as u64 + 1;
        lo + (self.next_u64() % span) as i64
    }
    pub fn float(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() % items.len() as u64) as usize]
    }
    pub fn chance(&mut self, pct: u8) -> bool {
        pct > 0 && (self.next_u64() % 100) < pct as u64
    }
}

// ---------------------------------------------------------------------------
// Word lists
// ---------------------------------------------------------------------------

const FIRST: &[&str] = &[
    "James", "Mary", "Robert", "Patricia", "John", "Jennifer", "Michael", "Linda", "David", "Elizabeth", "William",
    "Barbara", "Richard", "Susan", "Joseph", "Jessica", "Thomas", "Sarah", "Charles", "Karen", "Daniel", "Lisa",
    "Matthew", "Nancy", "Anthony", "Betty", "Marcus", "Aisha", "Andre", "Keisha", "Luis", "Sofia", "Wei", "Mei",
    "Arjun", "Priya", "Kwame", "Amara", "Diego", "Valentina", "Omar", "Fatima", "Hiro", "Yuki", "Liam", "Emma",
];
const LAST: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Rodriguez", "Martinez",
    "Hernandez", "Lopez", "Gonzalez", "Wilson", "Anderson", "Thomas", "Taylor", "Moore", "Jackson", "Martin", "Lee",
    "Perez", "Thompson", "White", "Harris", "Clark", "Lewis", "Robinson", "Walker", "Young", "Allen", "King",
    "Wright", "Scott", "Nguyen", "Hill", "Green", "Adams", "Baker", "Nelson", "Carter", "Mitchell", "Okafor", "Patel",
];
const CITIES: &[&str] = &[
    "Springfield", "Riverside", "Fairview", "Madison", "Georgetown", "Clinton", "Salem", "Franklin", "Greenville",
    "Bristol", "Clarksville", "Oakland", "Ashland", "Burlington", "Manchester", "Milton", "Newport", "Dayton",
    "Lexington", "Marion", "Cleveland", "Jackson", "Oxford", "Winchester", "Arlington", "Dover", "Hudson", "Kingston",
];
const STATES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS", "KY", "LA", "ME",
    "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA",
    "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
];
const COUNTRIES: &[&str] = &[
    "United States", "Canada", "Mexico", "United Kingdom", "Germany", "France", "Japan", "Brazil", "India", "Nigeria",
    "Kenya", "Australia", "Spain", "Italy", "South Korea", "Netherlands", "Sweden", "Ghana", "Chile", "Vietnam",
];
const STREETS: &[&str] = &[
    "Main St", "Oak Ave", "Maple Dr", "Cedar Ln", "Pine St", "Elm St", "Washington Ave", "Lake Rd", "Hill St",
    "Park Ave", "Church St", "Mill Rd", "River Rd", "Highland Ave", "Sunset Blvd", "Delta Ave", "College St",
];
const COMPANY_A: &[&str] = &[
    "Delta", "Summit", "Pioneer", "Riverbend", "Blue Ridge", "Magnolia", "Northstar", "Cypress", "Harbor", "Keystone",
    "Evergreen", "Silverline", "Bright", "Prairie", "Lakeside", "Heritage", "Unity", "Crescent", "Golden", "Liberty",
];
const COMPANY_B: &[&str] = &[
    "Health", "Medical Group", "Partners", "Labs", "Systems", "Holdings", "Logistics", "Foods", "Analytics", "Clinic",
    "Supply", "Solutions", "Networks", "Insurance", "Care", "Industries", "Services", "Media", "Works", "Co.",
];
const LOREM: &[&str] = &[
    "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do", "eiusmod", "tempor",
    "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "enim", "ad", "minim", "veniam", "quis",
    "nostrud", "exercitation", "ullamco", "laboris", "nisi", "aliquip", "ex", "ea", "commodo", "consequat", "duis",
    "aute", "irure", "in", "reprehenderit", "voluptate", "velit", "esse", "cillum", "fugiat", "nulla", "pariatur",
];
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net"];
const PEOPLE_TABLES: &[&str] = &[
    "person", "people", "patient", "provider", "user", "customer", "employee", "member", "contact", "staff",
    "student", "author", "doctor", "physician", "client", "owner", "agent", "driver", "teacher", "nurse",
];

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Gen {
    /// Leave the column out of the INSERT (rowid alias / DEFAULT applies).
    Skip,
    Null,
    Sequence { start: i64, step: i64 },
    IntRange { min: i64, max: i64 },
    RealRange { min: f64, max: f64, decimals: u32 },
    Bool,
    Uuid,
    FirstName,
    LastName,
    FullName,
    Company,
    Email,
    Phone,
    Street,
    City,
    State,
    Zip,
    Country,
    Url,
    Date { from: String, to: String },
    DateTime { from: String, to: String },
    Words { min: i64, max: i64 },
    Sentence,
    Paragraph,
    /// Comma-separated list of values.
    Choice(String),
    /// `#` digit, `?` letter, `*` alphanumeric, anything else literal.
    Pattern(String),
    Constant(String),
    ForeignKey { table: String, column: String },
    Bytes(i64),
}

impl Gen {
    pub fn label(&self) -> &'static str {
        match self {
            Gen::Skip => "Skip (auto / default)",
            Gen::Null => "NULL",
            Gen::Sequence { .. } => "Sequence",
            Gen::IntRange { .. } => "Integer range",
            Gen::RealRange { .. } => "Decimal range",
            Gen::Bool => "Boolean 0/1",
            Gen::Uuid => "UUID",
            Gen::FirstName => "First name",
            Gen::LastName => "Last name",
            Gen::FullName => "Full name",
            Gen::Company => "Company",
            Gen::Email => "Email",
            Gen::Phone => "Phone",
            Gen::Street => "Street address",
            Gen::City => "City",
            Gen::State => "US state",
            Gen::Zip => "ZIP code",
            Gen::Country => "Country",
            Gen::Url => "URL",
            Gen::Date { .. } => "Date",
            Gen::DateTime { .. } => "Date & time",
            Gen::Words { .. } => "Words",
            Gen::Sentence => "Sentence",
            Gen::Paragraph => "Paragraph",
            Gen::Choice(_) => "One of…",
            Gen::Pattern(_) => "Pattern",
            Gen::Constant(_) => "Constant",
            Gen::ForeignKey { .. } => "Foreign key",
            Gen::Bytes(_) => "Random bytes",
        }
    }

    /// Fresh default of each kind, for the generator picker.
    pub fn catalog() -> Vec<Gen> {
        vec![
            Gen::Skip,
            Gen::Null,
            Gen::Sequence { start: 1, step: 1 },
            Gen::IntRange { min: 1, max: 1000 },
            Gen::RealRange { min: 0.0, max: 1000.0, decimals: 2 },
            Gen::Bool,
            Gen::Uuid,
            Gen::FirstName,
            Gen::LastName,
            Gen::FullName,
            Gen::Company,
            Gen::Email,
            Gen::Phone,
            Gen::Street,
            Gen::City,
            Gen::State,
            Gen::Zip,
            Gen::Country,
            Gen::Url,
            Gen::Date { from: "2023-01-01".into(), to: "2026-12-31".into() },
            Gen::DateTime { from: "2023-01-01".into(), to: "2026-12-31".into() },
            Gen::Words { min: 1, max: 3 },
            Gen::Sentence,
            Gen::Paragraph,
            Gen::Choice("alpha,beta,gamma".into()),
            Gen::Pattern("???-####".into()),
            Gen::Constant(String::new()),
            Gen::Bytes(16),
        ]
    }
}

#[derive(Clone, Debug)]
pub struct ColumnPlan {
    pub name: String,
    pub decl: String,
    pub generator: Gen,
    pub null_pct: u8,
    pub notnull: bool,
}

#[derive(Clone, Debug)]
pub struct TablePlan {
    pub table: String,
    pub columns: Vec<ColumnPlan>,
    /// Column-index groups that must be unique together (PK / UNIQUE indexes).
    pub unique_groups: Vec<Vec<usize>>,
    pub strict: bool,
}

fn has_any(n: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| n.contains(k))
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

/// Pick a sensible generator for a column.
pub fn auto_gen(table: &str, d: &TableDetails, col: &crate::db::ColumnInfo) -> (Gen, u8) {
    let n = col.name.to_lowercase();
    let t = table.to_lowercase();
    let aff = affinity_of(&col.decl_type);
    let dt = col.decl_type.to_lowercase();
    let nullable = !col.notnull && col.pk == 0;
    let null_pct = if nullable { 5 } else { 0 };

    // rowid alias: let SQLite assign it
    if col.pk > 0 && d.has_rowid() && d.pk_columns().len() == 1 && dt == "integer" {
        return (Gen::Skip, 0);
    }
    if col.hidden >= 2 {
        return (Gen::Skip, 0); // generated column
    }
    if let Some(fk) = d.foreign_keys.iter().find(|f| f.from.eq_ignore_ascii_case(&col.name)) {
        return (Gen::ForeignKey { table: fk.table.clone(), column: fk.to.clone() }, null_pct);
    }
    let default_now = col.default.as_deref().is_some_and(|d| d.to_uppercase().contains("CURRENT_"));

    let g = if has_any(&n, &["email", "e_mail"]) {
        Gen::Email
    } else if has_any(&n, &["first_name", "firstname", "fname", "given_name"]) {
        Gen::FirstName
    } else if has_any(&n, &["last_name", "lastname", "surname", "lname", "family_name"]) {
        Gen::LastName
    } else if n == "name" || n == "full_name" || n == "fullname" || n == "display_name" {
        if PEOPLE_TABLES.iter().any(|p| t.contains(p)) { Gen::FullName } else { Gen::Company }
    } else if has_any(&n, &["company", "organization", "organisation", "employer", "vendor", "payer_name"]) {
        Gen::Company
    } else if has_any(&n, &["phone", "mobile", "fax", "tel"]) {
        Gen::Phone
    } else if has_any(&n, &["city", "town", "county"]) {
        Gen::City
    } else if has_any(&n, &["state", "province"]) && !n.contains("status") && !n.contains("statement") {
        Gen::State
    } else if has_any(&n, &["zip", "postal", "postcode"]) {
        Gen::Zip
    } else if n.contains("country") {
        Gen::Country
    } else if has_any(&n, &["address", "street", "addr"]) {
        Gen::Street
    } else if has_any(&n, &["uuid", "guid"]) {
        Gen::Uuid
    } else if has_any(&n, &["url", "website", "homepage", "link"]) {
        Gen::Url
    } else if has_any(&n, &["dob", "birth"]) {
        Gen::Date { from: "1940-01-01".into(), to: "2008-12-31".into() }
    } else if default_now || has_any(&n, &["timestamp", "created", "updated", "modified", "_at"]) || n == "at" || dt.contains("time") {
        Gen::DateTime { from: "2023-01-01".into(), to: "2026-12-31".into() }
    } else if has_any(&n, &["date", "_on", "submitted", "day"]) || dt.contains("date") {
        Gen::Date { from: "2023-01-01".into(), to: "2026-12-31".into() }
    } else if has_any(&n, &["price", "amount", "charge", "cost", "paid", "total", "balance", "fee", "salary", "payment", "revenue"]) {
        Gen::RealRange { min: 10.0, max: 500.0, decimals: 2 }
    } else if has_any(&n, &["qty", "quantity", "units", "count", "num_"]) {
        Gen::IntRange { min: 1, max: 20 }
    } else if n == "age" {
        Gen::IntRange { min: 18, max: 90 }
    } else if n.contains("year") {
        Gen::IntRange { min: 1990, max: 2026 }
    } else if has_any(&n, &["rating", "stars", "score"]) {
        Gen::IntRange { min: 1, max: 5 }
    } else if has_any(&n, &["percent", "pct", "ratio"]) {
        Gen::RealRange { min: 0.0, max: 100.0, decimals: 1 }
    } else if has_any(&n, &["lat"]) && !n.contains("late") {
        Gen::RealRange { min: -90.0, max: 90.0, decimals: 6 }
    } else if n.starts_with("lon") || n.starts_with("lng") {
        Gen::RealRange { min: -180.0, max: 180.0, decimals: 6 }
    } else if n.starts_with("is_") || n.starts_with("has_") || has_any(&n, &["active", "enabled", "deleted", "flag", "verified"]) || dt.contains("bool") {
        Gen::Bool
    } else if n.contains("status") {
        Gen::Choice("active,pending,approved,denied,closed".into())
    } else if n.contains("priority") {
        Gen::Choice("low,medium,high".into())
    } else if has_any(&n, &["specialty", "speciality", "department", "category", "type", "kind"]) && aff == Affinity::Text {
        Gen::Words { min: 1, max: 2 }
    } else if has_any(&n, &["bio", "description", "body", "content", "paragraph"]) {
        Gen::Paragraph
    } else if has_any(&n, &["note", "comment", "summary", "message", "msg", "text", "remark"]) {
        Gen::Sentence
    } else if n.contains("npi") {
        Gen::Pattern("1#########".into())
    } else if n.contains("ssn") {
        Gen::Pattern("000-##-####".into())
    } else if n.contains("mrn") {
        Gen::Pattern("MRN######".into())
    } else if n.contains("sku") {
        Gen::Pattern("SKU-######".into())
    } else if has_any(&n, &["cpt", "code"]) {
        Gen::Pattern("#####".into())
    } else if has_any(&n, &["line_no", "line_num", "seq", "position", "sort_order"]) {
        Gen::IntRange { min: 1, max: 20 }
    } else {
        match aff {
            Affinity::Integer => Gen::IntRange { min: 1, max: 1000 },
            Affinity::Real | Affinity::Numeric => Gen::RealRange { min: 0.0, max: 1000.0, decimals: 2 },
            Affinity::Blob if dt.contains("blob") => Gen::Bytes(16),
            _ => Gen::Words { min: 1, max: 3 },
        }
    };
    (g, null_pct)
}

pub fn auto_plan(db: &mut Database, table: &str) -> Result<TablePlan> {
    let d = db.table_details(table)?;
    if d.is_view {
        return Err(format!("'{table}' is a view."));
    }
    let columns: Vec<ColumnPlan> = d
        .columns
        .iter()
        .filter(|c| c.hidden != 1)
        .map(|c| {
            let (generator, null_pct) = auto_gen(table, &d, c);
            ColumnPlan { name: c.name.clone(), decl: c.decl_type.clone(), generator, null_pct, notnull: c.notnull || c.pk > 0 }
        })
        .collect();
    let idx = |name: &str| columns.iter().position(|c| c.name.eq_ignore_ascii_case(name.trim_end_matches(" DESC")));
    let mut unique_groups: Vec<Vec<usize>> = Vec::new();
    for i in d.indexes.iter().filter(|i| i.unique && !i.partial) {
        if let Some(g) = i.columns.iter().map(|c| idx(c)).collect::<Option<Vec<_>>>() {
            unique_groups.push(g);
        }
    }
    let pk = d.pk_columns();
    if !pk.is_empty() {
        if let Some(g) = pk.iter().map(|c| idx(c)).collect::<Option<Vec<_>>>() {
            if !unique_groups.contains(&g) {
                unique_groups.push(g);
            }
        }
    }
    // skipped (auto-assigned) columns are unique by construction
    unique_groups.retain(|g| !g.iter().all(|&c| columns[c].generator == Gen::Skip));
    Ok(TablePlan { table: table.to_string(), columns, unique_groups, strict: d.strict })
}

/// Tables ordered so parents come before children (cycle-safe).
pub fn fk_order(db: &mut Database, tables: &[String]) -> Result<Vec<String>> {
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    for t in tables {
        let d = db.table_details(t)?;
        deps.insert(t.clone(), d.foreign_keys.iter().map(|f| f.table.clone()).filter(|p| p != t).collect());
    }
    let mut out = Vec::new();
    let mut done = HashSet::new();
    fn visit(t: &str, deps: &HashMap<String, Vec<String>>, done: &mut HashSet<String>, stack: &mut HashSet<String>, out: &mut Vec<String>) {
        if done.contains(t) || stack.contains(t) {
            return;
        }
        stack.insert(t.to_string());
        if let Some(ps) = deps.get(t) {
            for p in ps {
                if deps.contains_key(p) {
                    visit(p, deps, done, stack, out);
                }
            }
        }
        stack.remove(t);
        done.insert(t.to_string());
        out.push(t.to_string());
    }
    let mut stack = HashSet::new();
    for t in tables {
        visit(t, &deps, &mut done, &mut stack, &mut out);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Value generation
// ---------------------------------------------------------------------------

/// Pools of existing parent keys, keyed by (table, column).
pub type FkPools = HashMap<(String, String), Vec<Value>>;

pub fn load_pools(db: &mut Database, plan: &TablePlan) -> Result<FkPools> {
    let mut pools = FkPools::new();
    for c in &plan.columns {
        if let Gen::ForeignKey { table, column } = &c.generator {
            let key = (table.clone(), column.clone());
            if pools.contains_key(&key) {
                continue;
            }
            let col_expr = if column.is_empty() {
                // REFERENCES parent  → parent's primary key (or rowid)
                let pd = db.table_details(table)?;
                pd.pk_columns().first().map(|c| quote_ident(c)).unwrap_or_else(|| "rowid".into())
            } else {
                quote_ident(column)
            };
            let r = db.query(
                LogSource::App,
                &format!("SELECT DISTINCT {col_expr} FROM {} WHERE {col_expr} IS NOT NULL LIMIT 200000;", quote_ident(table)),
                &[],
            )?;
            pools.insert(key, r.rows.into_iter().map(|mut r| r.remove(0)).collect());
        }
    }
    Ok(pools)
}

fn pattern(rng: &mut Rng, p: &str) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    p.chars()
        .map(|c| match c {
            '#' => char::from(b'0' + rng.range(0, 9) as u8),
            '?' => char::from(b'A' + rng.range(0, 25) as u8),
            '*' => *rng.pick(ALNUM) as char,
            other => other,
        })
        .collect()
}

fn words(rng: &mut Rng, n: i64) -> String {
    (0..n.max(1)).map(|_| *rng.pick(LOREM)).collect::<Vec<_>>().join(" ")
}

fn sentence(rng: &mut Rng) -> String {
    let n = rng.range(6, 14);
    let mut s = words(rng, n);
    if let Some(f) = s.get_mut(0..1) {
        f.make_ascii_uppercase();
    }
    s.push('.');
    s
}

/// Generate one value. `seq` is the 0-based row number within this run.
pub fn gen_value(g: &Gen, rng: &mut Rng, seq: i64, pools: &FkPools) -> std::result::Result<Value, String> {
    Ok(match g {
        Gen::Skip | Gen::Null => Value::Null,
        Gen::Sequence { start, step } => Value::Integer(start + seq * step),
        Gen::IntRange { min, max } => Value::Integer(rng.range(*min, *max)),
        Gen::RealRange { min, max, decimals } => {
            let v = min + rng.float() * (max - min);
            let f = 10f64.powi(*decimals as i32);
            Value::Real((v * f).round() / f)
        }
        Gen::Bool => Value::Integer(rng.range(0, 1)),
        Gen::Uuid => {
            let a = rng.next_u64();
            let b = rng.next_u64();
            Value::Text(format!(
                "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
                a >> 32,
                (a >> 16) & 0xffff,
                a & 0x0fff,
                0x8000 | ((b >> 48) & 0x3fff),
                b & 0xffff_ffff_ffff
            ))
        }
        Gen::FirstName => Value::Text(rng.pick(FIRST).to_string()),
        Gen::LastName => Value::Text(rng.pick(LAST).to_string()),
        Gen::FullName => Value::Text(format!("{} {}", rng.pick(FIRST), rng.pick(LAST))),
        Gen::Company => Value::Text(format!("{} {}", rng.pick(COMPANY_A), rng.pick(COMPANY_B))),
        Gen::Email => {
            let f = rng.pick(FIRST).to_lowercase();
            let l = rng.pick(LAST).to_lowercase();
            let n = rng.range(1, 999);
            Value::Text(format!("{f}.{l}{n}@{}", rng.pick(DOMAINS)))
        }
        // 555-0100..0199 is reserved for fiction in the NANP.
        Gen::Phone => Value::Text(format!("({}) 555-01{:02}", rng.range(201, 989), rng.range(0, 99))),
        Gen::Street => Value::Text(format!("{} {}", rng.range(1, 9999), rng.pick(STREETS))),
        Gen::City => Value::Text(rng.pick(CITIES).to_string()),
        Gen::State => Value::Text(rng.pick(STATES).to_string()),
        Gen::Zip => Value::Text(format!("{:05}", rng.range(1001, 99950))),
        Gen::Country => Value::Text(rng.pick(COUNTRIES).to_string()),
        Gen::Url => Value::Text(format!("https://www.{}.{}", rng.pick(LOREM), rng.pick(DOMAINS))),
        Gen::Date { from, to } | Gen::DateTime { from, to } => {
            let a = parse_date(from).ok_or_else(|| format!("Bad date '{from}' (use YYYY-MM-DD)"))?;
            let b = parse_date(to).ok_or_else(|| format!("Bad date '{to}' (use YYYY-MM-DD)"))?;
            let days = (b - a).num_days().max(0);
            let d = a + Duration::days(rng.range(0, days));
            if matches!(g, Gen::DateTime { .. }) {
                let t: NaiveDateTime = d.and_hms_opt(0, 0, 0).unwrap() + Duration::seconds(rng.range(0, 86_399));
                Value::Text(t.format("%Y-%m-%d %H:%M:%S").to_string())
            } else {
                Value::Text(d.format("%Y-%m-%d").to_string())
            }
        }
        Gen::Words { min, max } => {
            let n = rng.range(*min, *max);
            Value::Text(words(rng, n))
        }
        Gen::Sentence => Value::Text(sentence(rng)),
        Gen::Paragraph => {
            let n = rng.range(2, 5);
            Value::Text((0..n).map(|_| sentence(rng)).collect::<Vec<_>>().join(" "))
        }
        Gen::Choice(list) => {
            let items: Vec<&str> = list.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if items.is_empty() {
                Value::Null
            } else {
                Value::Text(rng.pick(&items).to_string())
            }
        }
        Gen::Pattern(p) => Value::Text(pattern(rng, p)),
        Gen::Constant(c) => {
            if c.eq_ignore_ascii_case("null") { Value::Null } else { Value::Text(c.clone()) }
        }
        Gen::ForeignKey { table, column } => {
            let pool = pools
                .get(&(table.clone(), column.clone()))
                .ok_or_else(|| format!("Parent table '{table}' wasn't loaded"))?;
            if pool.is_empty() {
                return Err(format!("Parent table '{table}' has no rows yet — generate data for it first (or include it in this run)."));
            }
            rng.pick(pool).clone()
        }
        Gen::Bytes(n) => Value::Blob((0..(*n).clamp(0, 1 << 20)).map(|_| rng.next_u64() as u8).collect()),
    })
}

fn key_of(v: &Value) -> String {
    match v {
        Value::Null => "\u{0}N".into(),
        Value::Integer(i) => format!("i{i}"),
        Value::Real(f) => format!("r{f}"),
        Value::Text(t) => format!("t{t}"),
        Value::Blob(b) => format!("b{b:?}"),
    }
}

/// Generate `n` rows for the non-skipped columns of a plan.
/// `existing` seeds the uniqueness check with values already in the table.
pub fn generate(
    plan: &TablePlan,
    n: usize,
    rng: &mut Rng,
    pools: &FkPools,
    existing: &[HashSet<String>],
    seq_offset: i64,
) -> std::result::Result<Vec<Vec<Value>>, String> {
    let affs: Vec<Affinity> = plan.columns.iter().map(|c| affinity_of(&c.decl)).collect();
    let mut seen: Vec<HashSet<String>> = plan
        .unique_groups
        .iter()
        .enumerate()
        .map(|(i, _)| existing.get(i).cloned().unwrap_or_default())
        .collect();
    let mut out = Vec::with_capacity(n);
    for r in 0..n {
        let mut attempt = 0;
        loop {
            let mut row: Vec<Value> = Vec::with_capacity(plan.columns.len());
            for (ci, c) in plan.columns.iter().enumerate() {
                let mut v = if !c.notnull && rng.chance(c.null_pct) {
                    Value::Null
                } else {
                    gen_value(&c.generator, rng, seq_offset + r as i64, pools)?
                };
                // type the value for the column's affinity (STRICT-safe)
                if let Value::Text(t) = &v {
                    v = convert(t, affs[ci], plan.strict, false)
                        .map_err(|e| format!("Column '{}': {e} — pick a generator that matches its type.", c.name))?;
                }
                row.push(v);
            }
            let keys: Vec<Option<String>> = plan
                .unique_groups
                .iter()
                .map(|g| {
                    let vals: Vec<&Value> = g.iter().map(|&c| &row[c]).collect();
                    // NULLs never collide in UNIQUE constraints; skipped cols are auto-unique
                    if vals.iter().any(|v| matches!(v, Value::Null)) {
                        None
                    } else {
                        Some(vals.iter().map(|v| key_of(v)).collect::<Vec<_>>().join("\u{1}"))
                    }
                })
                .collect();
            let clash = keys.iter().enumerate().any(|(gi, k)| k.as_ref().is_some_and(|k| seen[gi].contains(k)));
            if !clash {
                for (gi, k) in keys.into_iter().enumerate() {
                    if let Some(k) = k {
                        seen[gi].insert(k);
                    }
                }
                out.push(row);
                break;
            }
            attempt += 1;
            if attempt > 500 {
                let cols: Vec<&str> = plan
                    .unique_groups
                    .iter()
                    .flat_map(|g| g.iter().map(|&c| plan.columns[c].name.as_str()))
                    .collect();
                return Err(format!(
                    "Ran out of unique values after {} row(s) for {} ({}). Widen the generator's range or generate fewer rows.",
                    out.len(),
                    plan.table,
                    cols.join(", ")
                ));
            }
        }
    }
    Ok(out)
}

/// Existing values of each unique group, so new rows don't collide.
pub fn existing_unique(db: &mut Database, plan: &TablePlan) -> Result<Vec<HashSet<String>>> {
    let mut out = Vec::new();
    for g in &plan.unique_groups {
        let cols: Vec<String> = g.iter().map(|&c| quote_ident(&plan.columns[c].name)).collect();
        let r = db.query(LogSource::App, &format!("SELECT {} FROM {};", cols.join(", "), quote_ident(&plan.table)), &[])?;
        out.push(
            r.rows
                .iter()
                .filter(|row| !row.iter().any(|v| matches!(v, Value::Null)))
                .map(|row| row.iter().map(key_of).collect::<Vec<_>>().join("\u{1}"))
                .collect(),
        );
    }
    Ok(out)
}

#[derive(Clone, Debug, Default)]
pub struct MockReport {
    pub per_table: Vec<(String, usize, usize)>,
    pub problems: Vec<String>,
    pub millis: f64,
}

/// Generate and insert rows for several tables (parents first), inside one
/// savepoint so the whole run can be reverted.
pub fn run(db: &mut Database, jobs: &[(TablePlan, usize)], seed: u64) -> Result<MockReport> {
    if db.read_only {
        return Err("Database is opened read-only.".into());
    }
    let start = Instant::now();
    let names: Vec<String> = jobs.iter().map(|j| j.0.table.clone()).collect();
    let order = fk_order(db, &names)?;
    let mut rng = Rng::new(seed);
    db.ensure_savepoint()?;
    db.execute(LogSource::App, "SAVEPOINT \"MOCKDATA\";", &[])?;
    let res = (|| -> Result<MockReport> {
        let mut report = MockReport::default();
        for t in &order {
            let (plan, n) = jobs.iter().find(|j| &j.0.table == t).unwrap();
            if *n == 0 {
                continue;
            }
            let pools = load_pools(db, plan)?;
            let existing = existing_unique(db, plan)?;
            let rows = generate(plan, *n, &mut rng, &pools, &existing, 0)?;
            let (ins, failed, problems) = insert(db, plan, &rows)?;
            report.per_table.push((t.clone(), ins, failed));
            report.problems.extend(problems.into_iter().take(50));
        }
        report.millis = start.elapsed().as_secs_f64() * 1000.0;
        Ok(report)
    })();
    match res {
        Ok(r) => {
            db.execute(LogSource::App, "RELEASE \"MOCKDATA\";", &[])?;
            Ok(r)
        }
        Err(e) => {
            let _ = db.execute(LogSource::App, "ROLLBACK TO \"MOCKDATA\";", &[]);
            let _ = db.execute(LogSource::App, "RELEASE \"MOCKDATA\";", &[]);
            Err(e)
        }
    }
}

fn insert(db: &mut Database, plan: &TablePlan, rows: &[Vec<Value>]) -> Result<(usize, usize, Vec<String>)> {
    let cols: Vec<usize> = (0..plan.columns.len()).filter(|&i| plan.columns[i].generator != Gen::Skip).collect();
    let sql = if cols.is_empty() {
        format!("INSERT INTO {} DEFAULT VALUES;", quote_ident(&plan.table))
    } else {
        format!(
            "INSERT INTO {} ({}) VALUES ({});",
            quote_ident(&plan.table),
            cols.iter().map(|&i| quote_ident(&plan.columns[i].name)).collect::<Vec<_>>().join(", "),
            (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
        )
    };
    let t0 = Instant::now();
    let mut inserted = 0;
    let mut problems = Vec::new();
    {
        let mut stmt = db.conn.prepare(&sql).map_err(|e| e.to_string())?;
        for (ri, row) in rows.iter().enumerate() {
            for (k, &ci) in cols.iter().enumerate() {
                stmt.raw_bind_parameter(k + 1, &row[ci]).map_err(|e| e.to_string())?;
            }
            match stmt.raw_execute() {
                Ok(_) => inserted += 1,
                // CHECK constraints and triggers can still reject a row: skip it
                Err(e) => {
                    if problems.len() < 50 {
                        problems.push(format!("{} row {}: {e}", plan.table, ri + 1));
                    }
                }
            }
        }
    }
    let failed = rows.len() - inserted;
    db.push_log(
        LogSource::App,
        &format!("{sql}  -- mock data, prepared once, executed {} time(s)", rows.len()),
        failed == 0,
        t0.elapsed().as_secs_f64() * 1000.0,
        format!("{inserted} row(s) inserted, {failed} rejected"),
    );
    Ok((inserted, failed, problems))
}

/// Render a value for previews.
pub fn preview_text(v: &Value) -> String {
    crate::db::display_value(v).chars().take(60).collect::<String>() + if matches!(v, Value::Text(t) if t.chars().count() > 60) { "…" } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.execute_script(
            "CREATE TABLE org(id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, created_at TEXT DEFAULT CURRENT_TIMESTAMP);
             CREATE TABLE patient(id INTEGER PRIMARY KEY, org_id INTEGER NOT NULL REFERENCES org(id), name TEXT, zip TEXT, dob TEXT, balance REAL, is_active INTEGER);
             CREATE TABLE pair(a INTEGER REFERENCES org(id), b INTEGER, PRIMARY KEY(a,b)) WITHOUT ROWID;
             CREATE TABLE s(id INTEGER PRIMARY KEY, qty INTEGER, code TEXT) STRICT;",
            10,
        );
        db.write_changes().unwrap();
        db
    }

    #[test]
    fn auto_detects_generators() {
        let mut db = db();
        let p = auto_plan(&mut db, "patient").unwrap();
        let g = |n: &str| p.columns.iter().find(|c| c.name == n).unwrap().generator.clone();
        assert_eq!(g("id"), Gen::Skip);
        assert!(matches!(g("org_id"), Gen::ForeignKey { .. }));
        assert_eq!(g("name"), Gen::FullName, "people table → person name");
        assert_eq!(g("zip"), Gen::Zip);
        assert!(matches!(g("dob"), Gen::Date { .. }));
        assert!(matches!(g("balance"), Gen::RealRange { .. }));
        assert_eq!(g("is_active"), Gen::Bool);
        let o = auto_plan(&mut db, "org").unwrap();
        assert_eq!(o.columns[1].generator, Gen::Company);
        assert!(matches!(o.columns[3].generator, Gen::DateTime { .. }));
    }

    #[test]
    fn multi_table_run_respects_fk_order_uniqueness_and_strict() {
        let mut db = db();
        let jobs: Vec<(TablePlan, usize)> = ["pair", "patient", "org", "s"]
            .iter()
            .map(|t| (auto_plan(&mut db, t).unwrap(), if *t == "pair" { 40 } else { 25 }))
            .collect();
        let rep = run(&mut db, &jobs, 7).unwrap();
        assert_eq!(rep.per_table[0].0, "org", "parents first: {:?}", rep.per_table);
        assert!(rep.per_table.iter().all(|(_, ins, fail)| *fail == 0 && *ins > 0), "{rep:?}");
        let orphans = db
            .query(LogSource::App, "SELECT COUNT(*) FROM patient WHERE org_id NOT IN (SELECT id FROM org)", &[])
            .unwrap();
        assert_eq!(orphans.rows[0][0], Value::Integer(0));
        let ty = db.query(LogSource::App, "SELECT DISTINCT typeof(qty) FROM s WHERE qty IS NOT NULL", &[]).unwrap();
        assert_eq!(ty.rows.len(), 1);
        // unique emails, and a second run doesn't collide with the first
        let jobs2 = vec![(auto_plan(&mut db, "org").unwrap(), 25)];
        run(&mut db, &jobs2, 7).unwrap();
        let dup = db
            .query(LogSource::App, "SELECT COUNT(*) - COUNT(DISTINCT email) FROM org WHERE email IS NOT NULL", &[])
            .unwrap();
        assert_eq!(dup.rows[0][0], Value::Integer(0));
    }

    #[test]
    fn seeded_runs_are_reproducible() {
        let mut db = db();
        let plan = auto_plan(&mut db, "org").unwrap();
        let a = generate(&plan, 20, &mut Rng::new(99), &FkPools::new(), &[], 0).unwrap();
        let b = generate(&plan, 20, &mut Rng::new(99), &FkPools::new(), &[], 0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn reports_exhausted_unique_space() {
        let mut db = db();
        let mut plan = auto_plan(&mut db, "org").unwrap();
        plan.columns[2].generator = Gen::Choice("a@example.com,b@example.com".into());
        plan.columns[2].null_pct = 0;
        let err = generate(&plan, 5, &mut Rng::new(1), &FkPools::new(), &[HashSet::new()], 0).unwrap_err();
        assert!(err.contains("unique"), "{err}");
    }

    #[test]
    fn empty_parent_is_a_clear_error() {
        let mut db = db();
        let jobs = vec![(auto_plan(&mut db, "patient").unwrap(), 5)];
        let err = run(&mut db, &jobs, 1).unwrap_err();
        assert!(err.contains("org") && err.contains("first"), "{err}");
        assert_eq!(db.row_count("patient", "", &[]).unwrap(), 0);
    }
}

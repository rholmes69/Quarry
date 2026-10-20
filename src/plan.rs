//! `EXPLAIN QUERY PLAN` analysis.
//!
//! Turns SQLite's plan rows into a tree of classified steps (index search,
//! covering scan, full scan, temp B-tree sort, automatic index, subquery…),
//! annotates each table step with its row count, and produces plain-language
//! findings with concrete `CREATE INDEX` suggestions where SQLite itself
//! hints at one (automatic indexes) or the query text makes it clear
//! (filtered full scans, sorts).

use std::collections::HashMap;

use crate::db::{as_int, as_text, quote_ident, strip_comments, Database, LogSource, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// SEARCH via INTEGER PRIMARY KEY / PRIMARY KEY — the best case.
    KeyLookup,
    /// SEARCH via an index (possibly covering).
    IndexSearch,
    /// SCAN of a covering index — reads only the index.
    CoveringScan,
    /// SCAN through an index (ordered), still visits every row.
    IndexScan,
    /// SCAN of the whole table.
    FullScan,
    /// SQLite built a throw-away index at run time.
    AutoIndex,
    /// Sorting / grouping / DISTINCT in a temporary B-tree.
    TempBTree,
    /// Subqueries, co-routines, compound queries, materialized views…
    Structure,
    Other,
}

impl NodeKind {
    pub fn label(self) -> &'static str {
        match self {
            NodeKind::KeyLookup => "KEY LOOKUP",
            NodeKind::IndexSearch => "INDEX SEARCH",
            NodeKind::CoveringScan => "COVERING SCAN",
            NodeKind::IndexScan => "INDEX SCAN",
            NodeKind::FullScan => "FULL SCAN",
            NodeKind::AutoIndex => "AUTO INDEX",
            NodeKind::TempBTree => "TEMP B-TREE",
            NodeKind::Structure => "STEP",
            NodeKind::Other => "STEP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Good,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub struct PlanNode {
    pub id: i64,
    pub parent: i64,
    pub depth: usize,
    pub detail: String,
    pub kind: NodeKind,
    pub severity: Severity,
    /// Alias or name as it appears in the plan.
    pub object: Option<String>,
    /// Resolved real table name.
    pub table: Option<String>,
    pub index: Option<String>,
    pub rows: Option<i64>,
    /// Is this a nested (inner) loop?
    pub inner_loop: bool,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
    pub suggest: Option<(String, Vec<String>)>,
}

impl Finding {
    pub fn index_sql(&self) -> Option<String> {
        let (t, cols) = self.suggest.as_ref()?;
        let name = format!("idx_{}_{}", t, cols.join("_")).replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
        Some(format!(
            "CREATE INDEX {} ON {} ({});",
            quote_ident(&name),
            quote_ident(t),
            cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ")
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlanAnalysis {
    pub sql: String,
    pub nodes: Vec<PlanNode>,
    pub findings: Vec<Finding>,
    pub millis: f64,
}

impl PlanAnalysis {
    pub fn worst(&self) -> Severity {
        self.nodes.iter().map(|n| n.severity).chain(self.findings.iter().map(|f| f.severity)).max().unwrap_or(Severity::Good)
    }
}

// ---------------------------------------------------------------------------
// SQL text helpers (best-effort, used only for hints)
// ---------------------------------------------------------------------------

fn unquote(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(t);
    let t = t.strip_prefix('`').and_then(|x| x.strip_suffix('`')).unwrap_or(t);
    let t = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(t);
    t.to_string()
}

fn tokens(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in strip_comments(sql).chars() {
        if let Some(q) = quote {
            cur.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '`' | '\'' => {
                cur.push(ch);
                quote = Some(ch);
            }
            '[' => {
                cur.push(ch);
                quote = Some(']');
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            ',' | '(' | ')' | ';' | '=' | '<' | '>' | '!' | '+' | '-' | '*' | '/' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const STOP: &[&str] = &[
    "ON", "WHERE", "JOIN", "LEFT", "RIGHT", "FULL", "INNER", "OUTER", "CROSS", "NATURAL", "GROUP", "ORDER", "LIMIT",
    "USING", "UNION", "EXCEPT", "INTERSECT", "HAVING", "WINDOW", "SET", "VALUES", "INDEXED", "NOT", "RETURNING",
];

/// alias (lowercase) → table name, from FROM / JOIN / UPDATE clauses.
fn aliases(toks: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let up: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let mut i = 0;
    while i < toks.len() {
        if matches!(up[i].as_str(), "FROM" | "JOIN" | "UPDATE") || (up[i] == "," && in_from(&up, i)) {
            if let Some(name) = toks.get(i + 1) {
                if name != "(" {
                    let table = unquote(name.rsplit('.').next().unwrap_or(name));
                    let mut j = i + 2;
                    if up.get(j).map(|s| s.as_str()) == Some("AS") {
                        j += 1;
                    }
                    map.insert(table.to_lowercase(), table.clone());
                    if let Some(a) = toks.get(j) {
                        if !STOP.contains(&up[j].as_str()) && a != "," && a != ")" && a != "(" && a != ";" {
                            map.insert(unquote(a).to_lowercase(), table.clone());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    map
}

/// Is the comma at `i` inside a FROM list (between FROM and the next clause)?
fn in_from(up: &[String], i: usize) -> bool {
    let mut depth = 0i32;
    for k in (0..i).rev() {
        match up[k].as_str() {
            ")" => depth += 1,
            "(" => {
                if depth == 0 {
                    return false;
                }
                depth -= 1
            }
            "FROM" if depth == 0 => return true,
            "SELECT" | "WHERE" | "GROUP" | "ORDER" | "SET" | "VALUES" | "HAVING" if depth == 0 => return false,
            _ => {}
        }
    }
    false
}

/// Columns of `table` (by alias) referenced in the clause starting at `kw`.
fn clause_columns(toks: &[String], kw: &[&str], alias: &str, table_cols: &[String], single_table: bool) -> Vec<String> {
    let up: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let Some(start) = (0..up.len()).find(|&i| kw.iter().enumerate().all(|(k, w)| up.get(i + k).map(|s| s.as_str()) == Some(*w))) else {
        return vec![];
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    for (k, (t, u)) in toks.iter().zip(&up).enumerate().skip(start + kw.len()) {
        if matches!(u.as_str(), "GROUP" | "ORDER" | "LIMIT" | "HAVING" | "UNION" | "WINDOW" | "RETURNING") && !kw.contains(&u.as_str()) {
            break;
        }
        let (qual, col) = match t.split_once('.') {
            Some((q, c)) => (Some(unquote(q).to_lowercase()), unquote(c)),
            None => (None, unquote(t)),
        };
        let belongs = match &qual {
            Some(q) => q == &alias.to_lowercase(),
            None => single_table,
        };
        if belongs {
            if let Some(real) = table_cols.iter().find(|c| c.eq_ignore_ascii_case(&col)) {
                // equality (col = x, col IN, col IS) belongs before range columns in an index
                let next = up.get(k + 1).map(|s| s.as_str()).unwrap_or("");
                let eq = matches!(next, "=" | "IN" | "IS") && up.get(k + 2).map(|s| s.as_str()) != Some("=");
                if !out.iter().any(|o| o.0.eq_ignore_ascii_case(real)) {
                    out.push((real.clone(), eq));
                }
            }
        }
    }
    out.sort_by_key(|(_, eq)| !*eq);
    out.into_iter().map(|(c, _)| c).collect()
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn after<'a>(detail: &'a str, word: &str) -> Option<&'a str> {
    detail.find(word).map(|i| detail[i + word.len()..].trim_start())
}

fn first_word(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_string()
}

fn paren_cols(detail: &str) -> Vec<String> {
    let Some(open) = detail.rfind('(') else { return vec![] };
    let inner = detail[open + 1..].trim_end_matches(')');
    inner
        .split(" AND ")
        .filter_map(|p| {
            let c = p.split(['=', '>', '<', ' ']).next()?.trim();
            (!c.is_empty() && c != "rowid").then(|| c.to_string())
        })
        .collect()
}

fn classify(detail: &str) -> (NodeKind, Option<String>, Option<String>) {
    let d = detail.trim();
    let obj = || {
        after(d, "SCAN ").or_else(|| after(d, "SEARCH ")).map(first_word).filter(|w| w != "CONSTANT" && w != "TABLE")
    };
    let index = || {
        after(d, "COVERING INDEX ")
            .or_else(|| after(d, "USING INDEX "))
            .map(first_word)
            .filter(|w| !w.starts_with('('))
    };
    if d.contains("AUTOMATIC") && d.contains("INDEX") {
        return (NodeKind::AutoIndex, obj(), None);
    }
    if d.starts_with("USE TEMP B-TREE") {
        return (NodeKind::TempBTree, None, None);
    }
    if d.starts_with("SEARCH") {
        if d.contains("INTEGER PRIMARY KEY") || d.contains("USING PRIMARY KEY") {
            return (NodeKind::KeyLookup, obj(), None);
        }
        return (NodeKind::IndexSearch, obj(), index());
    }
    if d.starts_with("SCAN") {
        if d.starts_with("SCAN CONSTANT ROW") {
            return (NodeKind::Other, None, None);
        }
        if d.contains("COVERING INDEX") {
            return (NodeKind::CoveringScan, obj(), index());
        }
        if d.contains("USING INDEX") || d.contains("USING PRIMARY KEY") {
            return (NodeKind::IndexScan, obj(), index());
        }
        // "SCAN x" where x is a CTE/subquery name is a scan of a temp result
        return (NodeKind::FullScan, obj(), None);
    }
    let structural = ["SUBQUERY", "CO-ROUTINE", "MATERIALIZE", "COMPOUND", "UNION", "MULTI-INDEX OR", "INDEX ", "RECURSIVE", "SETUP", "LEFT-MOST", "BLOOM FILTER", "RIGHT-JOIN"];
    if structural.iter().any(|s| d.contains(s)) {
        return (NodeKind::Structure, None, None);
    }
    (NodeKind::Other, None, None)
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn explain(db: &mut Database, sql: &str) -> Result<PlanAnalysis> {
    let body = strip_comments(sql).trim().trim_end_matches(';').trim().to_string();
    if body.is_empty() {
        return Err("Nothing to explain — write or select a statement first.".into());
    }
    if body.to_ascii_uppercase().starts_with("EXPLAIN") {
        return Err("Remove the EXPLAIN keyword; Quarry adds EXPLAIN QUERY PLAN itself.".into());
    }
    let t0 = std::time::Instant::now();
    let r = db.query(LogSource::App, &format!("EXPLAIN QUERY PLAN {body};"), &[])?;
    let millis = t0.elapsed().as_secs_f64() * 1000.0;

    let toks = tokens(&body);
    let alias_map = aliases(&toks);
    let real_tables: Vec<String> = db.table_names(false)?;
    let resolve = |name: &str| -> Option<String> {
        let n = name.to_lowercase();
        alias_map
            .get(&n)
            .cloned()
            .or_else(|| real_tables.iter().find(|t| t.to_lowercase() == n).cloned())
            .filter(|t| real_tables.iter().any(|r| r.eq_ignore_ascii_case(t)))
    };

    // build nodes in SQLite's order (already depth-first)
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut depth_of: HashMap<i64, usize> = HashMap::new();
    for row in &r.rows {
        let id = as_int(&row[0]);
        let parent = as_int(&row[1]);
        let detail = as_text(&row[3]);
        let depth = if parent == 0 { 0 } else { depth_of.get(&parent).map(|d| d + 1).unwrap_or(0) };
        depth_of.insert(id, depth);
        let (kind, object, index) = classify(&detail);
        let table = object.as_deref().and_then(resolve);
        nodes.push(PlanNode { id, parent, depth, detail, kind, severity: Severity::Info, object, table, index, rows: None, inner_loop: false });
    }

    // row counts (stat1 if analyzed, otherwise COUNT(*), capped for speed)
    let mut counts: HashMap<String, i64> = HashMap::new();
    for n in &nodes {
        if let Some(t) = &n.table {
            if !counts.contains_key(t) {
                let c = db
                    .query(LogSource::App, &format!("SELECT COUNT(*) FROM (SELECT 1 FROM {} LIMIT 10000000);", quote_ident(t)), &[])
                    .ok()
                    .and_then(|r| r.rows.first().map(|r| as_int(&r[0])));
                if let Some(c) = c {
                    counts.insert(t.clone(), c);
                }
            }
        }
    }

    // loop nesting: among siblings, table steps after the first are inner loops
    let mut seen_loop_under: HashMap<i64, bool> = HashMap::new();
    for n in nodes.iter_mut() {
        n.rows = n.table.as_ref().and_then(|t| counts.get(t).copied());
        let is_loop = matches!(n.kind, NodeKind::FullScan | NodeKind::IndexScan | NodeKind::CoveringScan | NodeKind::IndexSearch | NodeKind::KeyLookup | NodeKind::AutoIndex);
        if is_loop {
            let e = seen_loop_under.entry(n.parent).or_insert(false);
            n.inner_loop = *e;
            *e = true;
        }
        let big = n.rows.unwrap_or(0) >= 1000;
        n.severity = match n.kind {
            NodeKind::KeyLookup | NodeKind::IndexSearch | NodeKind::CoveringScan => Severity::Good,
            NodeKind::IndexScan => Severity::Info,
            NodeKind::FullScan if n.table.is_none() => Severity::Info, // scan of a subquery/CTE result
            NodeKind::FullScan if n.inner_loop && n.rows.unwrap_or(0) > 100 => Severity::Bad,
            NodeKind::FullScan if big => Severity::Warn,
            NodeKind::FullScan => Severity::Info,
            NodeKind::AutoIndex => Severity::Warn,
            NodeKind::TempBTree => Severity::Info,
            _ => Severity::Info,
        };
    }

    // findings
    let single_table = alias_map.values().collect::<std::collections::HashSet<_>>().len() <= 1;
    let mut findings = Vec::new();
    for ni in 0..nodes.len() {
        let n = nodes[ni].clone();
        let tname = n.table.clone();
        let cols_of = |db: &mut Database, t: &str| -> Vec<String> {
            db.table_details(t).map(|d| d.columns.into_iter().map(|c| c.name).collect()).unwrap_or_default()
        };
        match n.kind {
            NodeKind::AutoIndex => {
                let cols = paren_cols(&n.detail);
                let t = tname.clone().unwrap_or_else(|| n.object.clone().unwrap_or_default());
                findings.push(Finding {
                    severity: Severity::Warn,
                    text: format!(
                        "SQLite builds a temporary index on {t}{} every time this runs. A permanent index makes this a plain index search.",
                        if cols.is_empty() { String::new() } else { format!(" ({})", cols.join(", ")) }
                    ),
                    suggest: (!cols.is_empty() && tname.is_some()).then(|| (t.clone(), cols)),
                });
            }
            NodeKind::FullScan if tname.is_some() && n.severity >= Severity::Warn => {
                let t = tname.clone().unwrap();
                let alias = n.object.clone().unwrap_or(t.clone());
                let tcols = cols_of(db, &t);
                let mut wcols = clause_columns(&toks, &["WHERE"], &alias, &tcols, single_table);
                if n.inner_loop {
                    // join columns matter most for inner loops
                    for c in clause_columns(&toks, &["ON"], &alias, &tcols, single_table) {
                        if !wcols.contains(&c) {
                            wcols.insert(0, c);
                        }
                    }
                }
                let rows = n.rows.map(|r| format!("{r} rows")).unwrap_or_default();
                // an unfiltered outer scan is simply "read everything" — normal for reports
                let severity = if !n.inner_loop && wcols.is_empty() { Severity::Info } else { n.severity };
                nodes[ni].severity = severity;
                let text = if n.inner_loop {
                    format!("{t} ({rows}) is fully scanned once for every row of the outer loop — cost grows multiplicatively.")
                } else if wcols.is_empty() {
                    format!("{t} ({rows}) is read in full. Fine if you need every row; otherwise add a WHERE on an indexed column.")
                } else {
                    format!("{t} ({rows}) is read in full even though the query filters on {}.", wcols.join(", "))
                };
                findings.push(Finding { severity, text, suggest: (!wcols.is_empty()).then(|| (t, wcols.into_iter().take(3).collect())) });
            }
            NodeKind::TempBTree => {
                let what = n.detail.trim_start_matches("USE TEMP B-TREE FOR ").to_string();
                let mut suggest = None;
                if single_table && what.contains("ORDER BY") {
                    if let Some(t) = alias_map.values().next().cloned() {
                        let tcols = cols_of(db, &t);
                        let ocols = clause_columns(&toks, &["ORDER", "BY"], &t, &tcols, true);
                        if !ocols.is_empty() {
                            suggest = Some((t, ocols));
                        }
                    }
                }
                findings.push(Finding {
                    severity: Severity::Info,
                    text: format!("Rows are collected and sorted in a temporary B-tree for {what}. An index whose column order matches can return them pre-sorted."),
                    suggest,
                });
            }
            NodeKind::Structure if n.detail.contains("CORRELATED") => findings.push(Finding {
                severity: Severity::Warn,
                text: format!("{} runs once per outer row. Consider rewriting it as a JOIN.", n.detail.to_lowercase()),
                suggest: None,
            }),
            _ => {}
        }
    }
    if findings.is_empty() {
        findings.push(Finding { severity: Severity::Good, text: "No problems spotted — every table access uses a key or index, or the tables are small.".into(), suggest: None });
    }
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    Ok(PlanAnalysis { sql: body, nodes, findings, millis })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.execute_script(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT, city TEXT);
             CREATE TABLE v(id INTEGER PRIMARY KEY, p_id INTEGER, day TEXT, amt REAL);
             CREATE INDEX v_day ON v(day);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 3000)
               INSERT INTO p SELECT x, 'n' || x, 'c' || (x % 50) FROM n;
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 5000)
               INSERT INTO v SELECT x, x % 3000 + 1, '2026-01-' || (x % 28 + 1), x * 1.5 FROM n;",
            10,
        );
        db.write_changes().unwrap();
        db
    }

    #[test]
    fn classifies_key_lookups_and_index_searches() {
        let mut db = db();
        let a = explain(&mut db, "SELECT * FROM p WHERE id = 5").unwrap();
        assert_eq!(a.nodes[0].kind, NodeKind::KeyLookup);
        assert_eq!(a.worst(), Severity::Good);
        let a = explain(&mut db, "SELECT day FROM v WHERE day = '2026-01-02'").unwrap();
        assert_eq!(a.nodes[0].kind, NodeKind::IndexSearch);
        assert_eq!(a.nodes[0].index.as_deref(), Some("v_day"));
    }

    #[test]
    fn filtered_full_scan_suggests_index_on_where_columns() {
        let mut db = db();
        let a = explain(&mut db, "SELECT * FROM p AS pp WHERE pp.city = 'c3' ORDER BY name").unwrap();
        let scan = a.nodes.iter().find(|n| n.kind == NodeKind::FullScan).unwrap();
        assert_eq!(scan.table.as_deref(), Some("p"));
        assert_eq!(scan.rows, Some(3000));
        let f = a.findings.iter().find(|f| f.suggest.is_some()).unwrap();
        assert_eq!(f.suggest, Some(("p".to_string(), vec!["city".to_string()])));
        assert!(f.index_sql().unwrap().starts_with("CREATE INDEX \"idx_p_city\""));
        assert!(a.nodes.iter().any(|n| n.kind == NodeKind::TempBTree));
    }

    #[test]
    fn join_without_index_flags_automatic_index_or_inner_scan() {
        let mut db = db();
        let q = "SELECT v.amt, p.name FROM v JOIN p ON p.city = v.amt";
        let a = explain(&mut db, q).unwrap();
        assert!(a.worst() >= Severity::Warn, "{:#?}", a.nodes);
        let suggestion = a.findings.iter().find_map(|f| f.suggest.clone());
        assert_eq!(suggestion, Some(("v".to_string(), vec!["amt".to_string()])), "{:#?} {:#?}", a.findings, a.nodes);
        // after creating the suggested index, the problem goes away
        let sql = a.findings.iter().find_map(|f| f.index_sql()).unwrap();
        db.execute(LogSource::App, &sql, &[]).unwrap();
        let b = explain(&mut db, q).unwrap();
        assert!(b.nodes.iter().any(|n| n.kind == NodeKind::IndexSearch || n.kind == NodeKind::CoveringScan), "{:#?}", b.nodes);
        assert!(b.findings.iter().all(|f| f.severity < Severity::Warn), "{:#?}", b.findings);
    }

    #[test]
    fn equality_columns_lead_the_suggested_index() {
        let mut db = db();
        let a = explain(&mut db, "SELECT * FROM v WHERE v.amt > 10 AND v.p_id = 3").unwrap();
        let s = a.findings.iter().find_map(|f| f.suggest.clone()).unwrap();
        assert_eq!(s.1, vec!["p_id".to_string(), "amt".to_string()]);
    }

    #[test]
    fn alias_parsing() {
        let t = tokens("SELECT * FROM \"visits\" v JOIN providers AS p ON p.id = v.provider_id, sites s WHERE 1");
        let m = aliases(&t);
        assert_eq!(m.get("v").map(|s| s.as_str()), Some("visits"));
        assert_eq!(m.get("p").map(|s| s.as_str()), Some("providers"));
        assert_eq!(m.get("s").map(|s| s.as_str()), Some("sites"));
    }
}

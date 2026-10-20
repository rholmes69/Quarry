//! Entity-relationship model built from the foreign keys SQLite reports,
//! with an automatic layered layout and SVG / Mermaid / DOT exporters.
//!
//! Layout: parents (referenced tables) on the left, children on the right.
//! Each table's rank is the length of the longest FK chain above it; tables
//! inside a rank are ordered by the average position of their parents to
//! cut down on crossing lines. Unrelated tables are parked in a grid after
//! the connected ones.

use std::collections::{HashMap, HashSet};

use crate::db::{Database, ObjKind, Result};

pub const TABLE_W: f32 = 230.0;
pub const HEADER_H: f32 = 28.0;
pub const ROW_H: f32 = 20.0;
pub const PAD: f32 = 6.0;
const COL_GAP: f32 = 130.0;
const ROW_GAP: f32 = 36.0;
const MAX_COLUMN_H: f32 = 1500.0;

#[derive(Clone, Debug)]
pub struct ErColumn {
    pub name: String,
    pub ty: String,
    pub pk: bool,
    pub fk: bool,
    pub notnull: bool,
}

#[derive(Clone, Debug)]
pub struct ErTable {
    pub name: String,
    pub columns: Vec<ErColumn>,
    pub x: f32,
    pub y: f32,
    pub row_count: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct ErEdge {
    pub from: usize,
    pub to: usize,
    pub from_cols: Vec<String>,
    pub to_cols: Vec<String>,
    /// Child may exist without a parent (some FK column is nullable).
    pub optional: bool,
    /// FK columns are unique in the child → one-to-one.
    pub one_to_one: bool,
    /// Parent table isn't in the schema (dangling FK).
    pub dangling: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ErModel {
    pub tables: Vec<ErTable>,
    pub edges: Vec<ErEdge>,
}

impl ErTable {
    pub fn visible_columns(&self, keys_only: bool) -> Vec<&ErColumn> {
        self.columns.iter().filter(|c| !keys_only || c.pk || c.fk).collect()
    }
    pub fn height(&self, keys_only: bool) -> f32 {
        HEADER_H + self.visible_columns(keys_only).len().max(1) as f32 * ROW_H + PAD
    }
    /// Y (scene coords) of the middle of the row showing `col`, or the header.
    pub fn row_y(&self, col: &str, keys_only: bool) -> f32 {
        match self.visible_columns(keys_only).iter().position(|c| c.name.eq_ignore_ascii_case(col)) {
            Some(i) => self.y + HEADER_H + i as f32 * ROW_H + ROW_H / 2.0,
            None => self.y + HEADER_H / 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Building the model
// ---------------------------------------------------------------------------

pub fn build(db: &mut Database, with_counts: bool) -> Result<ErModel> {
    let names: Vec<String> = db
        .schema_objects()?
        .into_iter()
        .filter(|o| o.kind == ObjKind::Table && !o.name.starts_with("sqlite_"))
        .map(|o| o.name)
        .collect();
    let index: HashMap<String, usize> = names.iter().enumerate().map(|(i, n)| (n.to_lowercase(), i)).collect();

    let mut model = ErModel::default();
    let mut raw_fks = Vec::new();
    for name in &names {
        let d = db.table_details(name)?;
        let fk_cols: HashSet<String> = d.foreign_keys.iter().map(|f| f.from.to_lowercase()).collect();
        // single-column UNIQUE / PK sets, to detect 1:1 relationships
        let unique_sets: Vec<Vec<String>> = d
            .indexes
            .iter()
            .filter(|i| i.unique && !i.partial)
            .map(|i| i.columns.iter().map(|c| c.trim_end_matches(" DESC").to_lowercase()).collect())
            .chain(std::iter::once(d.pk_columns().iter().map(|c| c.to_lowercase()).collect()))
            .collect();
        let columns: Vec<ErColumn> = d
            .columns
            .iter()
            .filter(|c| c.hidden != 1)
            .map(|c| ErColumn {
                name: c.name.clone(),
                ty: c.decl_type.clone(),
                pk: c.pk > 0,
                fk: fk_cols.contains(&c.name.to_lowercase()),
                notnull: c.notnull || c.pk > 0,
            })
            .collect();

        // group composite FKs by id
        let mut by_id: Vec<(i64, String, Vec<String>, Vec<String>)> = Vec::new();
        for f in &d.foreign_keys {
            match by_id.iter_mut().find(|g| g.0 == f.id) {
                Some(g) => {
                    g.2.push(f.from.clone());
                    g.3.push(f.to.clone());
                }
                None => by_id.push((f.id, f.table.clone(), vec![f.from.clone()], vec![f.to.clone()])),
            }
        }
        for (_, parent, from_cols, to_cols) in by_id {
            let lc: Vec<String> = from_cols.iter().map(|c| c.to_lowercase()).collect();
            let optional = from_cols.iter().any(|fc| {
                columns.iter().find(|c| c.name.eq_ignore_ascii_case(fc)).is_some_and(|c| !c.notnull)
            });
            let one_to_one = unique_sets.iter().any(|u| !u.is_empty() && u.iter().all(|c| lc.contains(c)) && u.len() == lc.len());
            raw_fks.push((model.tables.len(), parent, from_cols, to_cols, optional, one_to_one));
        }

        let row_count = if with_counts { db.row_count(name, "", &[]).ok() } else { None };
        model.tables.push(ErTable { name: name.clone(), columns, x: 0.0, y: 0.0, row_count });
    }

    for (from, parent, from_cols, to_cols, optional, one_to_one) in raw_fks {
        let (to, dangling) = match index.get(&parent.to_lowercase()) {
            Some(&i) => (i, false),
            None => {
                // keep the relationship visible with a placeholder table
                let i = model.tables.len();
                model.tables.push(ErTable {
                    name: parent.clone(),
                    columns: vec![],
                    x: 0.0,
                    y: 0.0,
                    row_count: None,
                });
                (i, true)
            }
        };
        // empty "to" means "the parent's primary key"
        let to_cols = if to_cols.iter().all(|c| c.is_empty()) {
            let pk: Vec<String> = model.tables[to].columns.iter().filter(|c| c.pk).map(|c| c.name.clone()).collect();
            if pk.is_empty() { vec!["rowid".into()] } else { pk }
        } else {
            to_cols
        };
        model.edges.push(ErEdge { from, to, from_cols, to_cols, optional, one_to_one, dangling });
    }
    Ok(model)
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

pub fn auto_layout(model: &mut ErModel, keys_only: bool) {
    let n = model.tables.len();
    if n == 0 {
        return;
    }
    let mut parents: Vec<Vec<usize>> = vec![vec![]; n];
    let mut connected = vec![false; n];
    for e in &model.edges {
        if e.from != e.to {
            parents[e.from].push(e.to);
        }
        connected[e.from] = true;
        connected[e.to] = true;
    }

    // rank = longest path to a root (cycle-safe)
    fn rank_of(i: usize, parents: &[Vec<usize>], memo: &mut [Option<usize>], visiting: &mut [bool]) -> usize {
        if let Some(r) = memo[i] {
            return r;
        }
        if visiting[i] {
            return 0; // cycle: break it here
        }
        visiting[i] = true;
        let r = parents[i].iter().map(|&p| rank_of(p, parents, memo, visiting) + 1).max().unwrap_or(0);
        visiting[i] = false;
        memo[i] = Some(r);
        r
    }
    let mut memo = vec![None; n];
    let mut visiting = vec![false; n];
    let ranks: Vec<usize> = (0..n).map(|i| rank_of(i, &parents, &mut memo, &mut visiting)).collect();

    let max_rank = (0..n).filter(|&i| connected[i]).map(|i| ranks[i]).max().unwrap_or(0);
    let mut columns: Vec<Vec<usize>> = vec![vec![]; max_rank + 1];
    for i in (0..n).filter(|&i| connected[i]) {
        columns[ranks[i]].push(i);
    }
    // order: rank 0 alphabetically, later ranks by barycenter of parents
    let mut order_pos: HashMap<usize, f32> = HashMap::new();
    for (ci, col) in columns.iter_mut().enumerate() {
        if ci == 0 {
            col.sort_by_key(|&i| model.tables[i].name.to_lowercase());
        } else {
            col.sort_by(|&a, &b| {
                let bary = |i: usize| {
                    let ps: Vec<f32> = parents[i].iter().filter_map(|p| order_pos.get(p)).copied().collect();
                    if ps.is_empty() { f32::MAX } else { ps.iter().sum::<f32>() / ps.len() as f32 }
                };
                bary(a).partial_cmp(&bary(b)).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for (k, &i) in col.iter().enumerate() {
            order_pos.insert(i, k as f32);
        }
    }

    // place, wrapping very tall ranks into sub-columns
    let mut x = 0.0;
    for col in &columns {
        let mut y = 0.0;
        let mut col_x = x;
        let mut used = false;
        for &i in col {
            let h = model.tables[i].height(keys_only);
            if y > 0.0 && y + h > MAX_COLUMN_H {
                col_x += TABLE_W + COL_GAP * 0.5;
                y = 0.0;
            }
            model.tables[i].x = col_x;
            model.tables[i].y = y;
            y += h + ROW_GAP;
            used = true;
        }
        if used {
            x = col_x + TABLE_W + COL_GAP;
        }
    }

    // unrelated tables: grid below/after the graph
    let loose: Vec<usize> = {
        let mut v: Vec<usize> = (0..n).filter(|&i| !connected[i]).collect();
        v.sort_by_key(|&i| model.tables[i].name.to_lowercase());
        v
    };
    if !loose.is_empty() {
        let graph_bottom = (0..n)
            .filter(|&i| connected[i])
            .map(|i| model.tables[i].y + model.tables[i].height(keys_only))
            .fold(0.0f32, f32::max);
        let top = if graph_bottom > 0.0 { graph_bottom + ROW_GAP * 2.0 } else { 0.0 };
        let per_row = ((loose.len() as f32).sqrt().ceil() as usize).max(3);
        let mut row_h: f32 = 0.0;
        let mut y = top;
        for (k, &i) in loose.iter().enumerate() {
            if k > 0 && k % per_row == 0 {
                y += row_h + ROW_GAP;
                row_h = 0.0;
            }
            model.tables[i].x = (k % per_row) as f32 * (TABLE_W + COL_GAP * 0.5);
            model.tables[i].y = y;
            row_h = row_h.max(model.tables[i].height(keys_only));
        }
    }
}

pub fn bounds(model: &ErModel, keys_only: bool) -> (f32, f32, f32, f32) {
    let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for t in &model.tables {
        b.0 = b.0.min(t.x);
        b.1 = b.1.min(t.y);
        b.2 = b.2.max(t.x + TABLE_W);
        b.3 = b.3.max(t.y + t.height(keys_only));
    }
    if model.tables.is_empty() { (0.0, 0.0, 400.0, 300.0) } else { b }
}

/// Endpoints and control points of the curve for an edge (scene coords).
pub fn edge_geometry(model: &ErModel, e: &ErEdge, keys_only: bool) -> [(f32, f32); 4] {
    let a = &model.tables[e.from];
    let b = &model.tables[e.to];
    let ay = a.row_y(&e.from_cols[0], keys_only);
    let by = b.row_y(e.to_cols.first().map(|s| s.as_str()).unwrap_or(""), keys_only);
    if e.from == e.to {
        let x = a.x + TABLE_W;
        return [(x, ay), (x + 60.0, ay), (x + 60.0, by.max(ay) + 30.0), (x, by.max(ay) + 30.0)];
    }
    let a_center = a.x + TABLE_W / 2.0;
    let b_center = b.x + TABLE_W / 2.0;
    let (ax, bx, dir) = if a_center > b_center { (a.x, b.x + TABLE_W, -1.0) } else { (a.x + TABLE_W, b.x, 1.0) };
    let k = ((bx - ax).abs() * 0.45).max(50.0);
    [(ax, ay), (ax + dir * k, ay), (bx - dir * k, by), (bx, by)]
}

// ---------------------------------------------------------------------------
// Exporters
// ---------------------------------------------------------------------------

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn to_svg(model: &ErModel, keys_only: bool, title: &str) -> String {
    let (x0, y0, x1, y1) = bounds(model, keys_only);
    let m = 40.0;
    let (w, h) = (x1 - x0 + 2.0 * m, y1 - y0 + 2.0 * m + 30.0);
    let (ox, oy) = (m - x0, m - y0 + 30.0);
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"{h:.0}\" viewBox=\"0 0 {w:.0} {h:.0}\" \
         font-family=\"Segoe UI, Helvetica, Arial, sans-serif\" font-size=\"12\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n\
         <text x=\"{m}\" y=\"{}\" font-size=\"16\" font-weight=\"600\" fill=\"#1f2937\">{}</text>\n",
        m - 4.0,
        xml(title)
    );
    for e in &model.edges {
        let p = edge_geometry(model, e, keys_only);
        let dash = if e.optional { " stroke-dasharray=\"5 4\"" } else { "" };
        s.push_str(&format!(
            "<path d=\"M{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"none\" stroke=\"#64748b\" stroke-width=\"1.4\"{dash}/>\n",
            p[0].0 + ox, p[0].1 + oy, p[1].0 + ox, p[1].1 + oy, p[2].0 + ox, p[2].1 + oy, p[3].0 + ox, p[3].1 + oy
        ));
        // child end: crow's foot (many) unless 1:1; parent end: bar
        let cdir = if p[1].0 >= p[0].0 { 1.0 } else { -1.0 };
        let (cx, cy) = (p[0].0 + ox, p[0].1 + oy);
        if e.one_to_one {
            s.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#64748b\" stroke-width=\"1.4\"/>\n", cx + cdir * 10.0, cy - 6.0, cx + cdir * 10.0, cy + 6.0));
        } else {
            for dy in [-6.0, 0.0, 6.0] {
                s.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#64748b\" stroke-width=\"1.4\"/>\n", cx + cdir * 12.0, cy, cx, cy + dy));
            }
        }
        let pdir = if p[2].0 <= p[3].0 { -1.0 } else { 1.0 };
        let (px, py) = (p[3].0 + ox, p[3].1 + oy);
        s.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#64748b\" stroke-width=\"1.4\"/>\n", px + pdir * 8.0, py - 6.0, px + pdir * 8.0, py + 6.0));
    }
    for t in &model.tables {
        let (x, y) = (t.x + ox, t.y + oy);
        let th = t.height(keys_only);
        let header = if t.columns.is_empty() { "#9ca3af" } else { "#2563eb" };
        s.push_str(&format!(
            "<g><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{TABLE_W}\" height=\"{th:.1}\" rx=\"6\" fill=\"#f8fafc\" stroke=\"#94a3b8\"/>\n\
             <path d=\"M{x:.1},{:.1} v-{:.1} a6,6 0 0 1 6,-6 h{:.1} a6,6 0 0 1 6,6 v{:.1} z\" fill=\"{header}\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" fill=\"#ffffff\" font-weight=\"600\">{}</text>\n",
            y + HEADER_H, HEADER_H - 6.0, TABLE_W - 12.0, HEADER_H - 6.0,
            x + 10.0, y + 18.0, xml(&t.name)
        ));
        if let Some(n) = t.row_count {
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#dbeafe\" font-size=\"10\" text-anchor=\"end\">{n} rows</text>\n", x + TABLE_W - 8.0, y + 18.0));
        }
        for (i, c) in t.visible_columns(keys_only).iter().enumerate() {
            let ry = y + HEADER_H + i as f32 * ROW_H + 14.0;
            let key = if c.pk { "PK" } else if c.fk { "FK" } else { "" };
            let kc = if c.pk { "#b45309" } else { "#7c3aed" };
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"{kc}\" font-size=\"10\" font-weight=\"700\">{key}</text>\n", x + 8.0));
            let weight = if c.pk { " font-weight=\"600\"" } else { "" };
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"#111827\"{weight}>{}</text>\n", x + 30.0, xml(&c.name)));
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"#64748b\" font-size=\"11\" text-anchor=\"end\">{}{}</text>\n",
                x + TABLE_W - 8.0,
                xml(&c.ty.to_lowercase()),
                if c.notnull && !c.pk { " ·nn" } else { "" }
            ));
        }
        s.push_str("</g>\n");
    }
    s.push_str("</svg>\n");
    s
}

fn mermaid_ident(s: &str) -> String {
    let id: String = s.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if id.is_empty() { "_".into() } else { id }
}

/// Mermaid `erDiagram` — pastes straight into GitHub, GitLab, Notion, Confluence.
pub fn to_mermaid(model: &ErModel, keys_only: bool) -> String {
    let mut s = String::from("erDiagram\n");
    for t in &model.tables {
        s.push_str(&format!("    {} {{\n", mermaid_ident(&t.name)));
        for c in t.visible_columns(keys_only) {
            let ty = if c.ty.trim().is_empty() { "any".to_string() } else { mermaid_ident(&c.ty) };
            let key = match (c.pk, c.fk) {
                (true, true) => " PK, FK",
                (true, false) => " PK",
                (false, true) => " FK",
                _ => "",
            };
            s.push_str(&format!("        {} {}{}\n", ty, mermaid_ident(&c.name), key));
        }
        s.push_str("    }\n");
    }
    for e in &model.edges {
        let parent_side = if e.optional { "|o" } else { "||" };
        let child_side = if e.one_to_one { "o|" } else { "o{" };
        s.push_str(&format!(
            "    {} {}--{} {} : \"{}\"\n",
            mermaid_ident(&model.tables[e.to].name),
            parent_side,
            child_side,
            mermaid_ident(&model.tables[e.from].name),
            e.from_cols.join(", ")
        ));
    }
    s
}

/// Graphviz DOT, for people who want their own layout engine.
pub fn to_dot(model: &ErModel, keys_only: bool) -> String {
    let mut s = String::from("digraph schema {\n  rankdir=RL;\n  node [shape=plaintext fontname=\"Helvetica\"];\n");
    for (i, t) in model.tables.iter().enumerate() {
        let rows: String = t
            .visible_columns(keys_only)
            .iter()
            .map(|c| {
                format!(
                    "<tr><td align=\"left\" port=\"{}\">{}{}</td><td align=\"left\"><font color=\"#64748b\">{}</font></td></tr>",
                    xml(&c.name),
                    if c.pk { "🔑 " } else if c.fk { "↗ " } else { "" },
                    xml(&c.name),
                    xml(&c.ty)
                )
            })
            .collect();
        s.push_str(&format!(
            "  t{i} [label=<<table border=\"0\" cellborder=\"1\" cellspacing=\"0\"><tr><td colspan=\"2\" bgcolor=\"#2563eb\"><font color=\"white\"><b>{}</b></font></td></tr>{rows}</table>>];\n",
            xml(&t.name)
        ));
    }
    for e in &model.edges {
        s.push_str(&format!(
            "  t{} -> t{} [label=\"{}\"{}];\n",
            e.from,
            e.to,
            xml(&e.from_cols.join(",")),
            if e.optional { " style=dashed" } else { "" }
        ));
    }
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn sample() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.execute_script(
            "CREATE TABLE org(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE person(id INTEGER PRIMARY KEY, org_id INTEGER NOT NULL REFERENCES org(id), boss_id INTEGER REFERENCES person);
             CREATE TABLE badge(person_id INTEGER UNIQUE REFERENCES person(id), code TEXT);
             CREATE TABLE line(a INT, b INT, note TEXT, PRIMARY KEY(a,b));
             CREATE TABLE line_note(a INT, b INT, FOREIGN KEY(a,b) REFERENCES line(a,b));
             CREATE TABLE lonely(x);",
            10,
        );
        db
    }

    #[test]
    fn builds_edges_with_cardinality_and_composites() {
        let mut db = sample();
        let m = build(&mut db, true).unwrap();
        let name = |i: usize| m.tables[i].name.as_str();
        let find = |from: &str, to: &str| m.edges.iter().find(|e| name(e.from) == from && name(e.to) == to).unwrap();
        let po = find("person", "org");
        assert!(!po.optional && !po.one_to_one);
        assert_eq!(po.to_cols, vec!["id"]);
        let self_ref = find("person", "person");
        assert!(self_ref.optional);
        assert_eq!(self_ref.to_cols, vec!["id"], "empty REFERENCES target resolves to PK");
        assert!(find("badge", "person").one_to_one);
        assert_eq!(find("line_note", "line").from_cols, vec!["a", "b"]);
    }

    #[test]
    fn layout_puts_parents_left_and_no_overlaps() {
        let mut db = sample();
        let mut m = build(&mut db, false).unwrap();
        auto_layout(&mut m, false);
        let t = |n: &str| m.tables.iter().find(|t| t.name == n).unwrap().clone();
        assert!(t("org").x < t("person").x);
        assert!(t("person").x < t("badge").x);
        for (i, a) in m.tables.iter().enumerate() {
            for b in m.tables.iter().skip(i + 1) {
                let overlap = a.x < b.x + TABLE_W && b.x < a.x + TABLE_W && a.y < b.y + b.height(false) && b.y < a.y + a.height(false);
                assert!(!overlap, "{} overlaps {}", a.name, b.name);
            }
        }
    }

    #[test]
    fn exporters_produce_valid_text() {
        let mut db = sample();
        let mut m = build(&mut db, false).unwrap();
        auto_layout(&mut m, true);
        let svg = to_svg(&m, true, "test");
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert_eq!(svg.matches("<g>").count(), m.tables.len());
        let mm = to_mermaid(&m, false);
        assert!(mm.contains("org ||--o{ person : \"org_id\""), "{mm}");
        assert!(mm.contains("person |o--o| badge"), "{mm}");
        assert_eq!(to_dot(&m, false).matches(" -> t").count(), m.edges.len());
    }
}

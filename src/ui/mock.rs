//! Generate Mock Data dialog: pick tables + row counts, tune the generator
//! for each column, preview, then insert (parents first) in one revertible
//! savepoint.

use egui::{Context, RichText, Ui};

use crate::app::App;
use crate::mock::{self, Gen, MockReport, Rng, TablePlan};
use crate::ui::widgets::palette;

pub struct MockTable {
    name: String,
    include: bool,
    rows: usize,
    plan: Option<Result<TablePlan, String>>,
    auto: Vec<Gen>,
}

pub struct MockDialog {
    tables: Vec<MockTable>,
    selected: usize,
    seed: u64,
    preview: Option<Result<(Vec<String>, Vec<Vec<String>>), String>>,
    report: Option<Result<MockReport, String>>,
}

impl MockDialog {
    pub fn new(all_tables: Vec<String>, initial: Option<String>) -> Self {
        let selected = initial.as_ref().and_then(|t| all_tables.iter().position(|x| x == t)).unwrap_or(0);
        let tables = all_tables
            .into_iter()
            .enumerate()
            .map(|(i, name)| MockTable { name, include: i == selected, rows: 100, plan: None, auto: vec![] })
            .collect();
        MockDialog { tables, selected, seed: 42, preview: None, report: None }
    }
}

fn gen_params(ui: &mut Ui, g: &mut Gen) {
    let num = |ui: &mut Ui, v: &mut i64| {
        ui.add(egui::DragValue::new(v).speed(1.0));
    };
    match g {
        Gen::Sequence { start, step } => {
            ui.label("start");
            num(ui, start);
            ui.label("step");
            num(ui, step);
        }
        Gen::IntRange { min, max } | Gen::Words { min, max } => {
            num(ui, min);
            ui.label("–");
            num(ui, max);
        }
        Gen::RealRange { min, max, decimals } => {
            ui.add(egui::DragValue::new(min).speed(1.0));
            ui.label("–");
            ui.add(egui::DragValue::new(max).speed(1.0));
            ui.label("dp");
            ui.add(egui::DragValue::new(decimals).range(0..=10));
        }
        Gen::Date { from, to } | Gen::DateTime { from, to } => {
            ui.add_sized([86.0, 20.0], egui::TextEdit::singleline(from));
            ui.label("–");
            ui.add_sized([86.0, 20.0], egui::TextEdit::singleline(to));
        }
        Gen::Choice(s) => {
            ui.add_sized([200.0, 20.0], egui::TextEdit::singleline(s).hint_text("a, b, c"));
        }
        Gen::Pattern(s) => {
            ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(s))
                .on_hover_text("# digit · ? letter · * letter-or-digit · anything else literal");
        }
        Gen::Constant(s) => {
            ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(s).hint_text("value"));
        }
        Gen::ForeignKey { table, column } => {
            ui.label(RichText::new(format!("→ {table}.{}", if column.is_empty() { "<pk>" } else { column })).weak());
        }
        Gen::Bytes(n) => {
            ui.add(egui::DragValue::new(n).range(0..=65536));
            ui.label("bytes");
        }
        _ => {}
    }
}

impl App {
    pub fn mock_window(&mut self, ctx: &Context, d: &mut MockDialog) -> bool {
        let mut open = true;
        let mut close = false;

        // lazily build plans for the selected table and included ones
        if let Some(db) = self.db.as_mut() {
            for (i, t) in d.tables.iter_mut().enumerate() {
                if t.plan.is_none() && (t.include || i == d.selected) {
                    let p = mock::auto_plan(db, &t.name);
                    if let Ok(p) = &p {
                        t.auto = p.columns.iter().map(|c| c.generator.clone()).collect();
                    }
                    t.plan = Some(p);
                }
            }
        }

        egui::Window::new("Generate Mock Data")
            .open(&mut open)
            .default_size([980.0, 620.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let p = palette(ui);
                ui.horizontal(|ui| {
                    ui.label("Seed");
                    ui.add(egui::DragValue::new(&mut d.seed));
                    if ui.button("🎲").on_hover_text("New random seed").clicked() {
                        d.seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_nanos() as u64 % 1_000_000).unwrap_or(7);
                    }
                    ui.label(RichText::new("Same seed + same plan = same data").small().weak());
                    ui.separator();
                    if ui.button("Include empty parent tables").on_hover_text("Tick every table referenced by a ticked table that has no rows yet").clicked() {
                        self.include_parents(d);
                    }
                });
                ui.separator();

                egui::Panel::left("mock_tables").resizable(true).default_size(230.0).show(ui, |ui| {
                    ui.label(RichText::new("Tables  (inserted parents-first)").strong());
                    egui::ScrollArea::vertical().id_salt("mock_tbl_scroll").show(ui, |ui| {
                        for (i, t) in d.tables.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut t.include, "").changed() {
                                    d.report = None;
                                }
                                if ui.selectable_label(d.selected == i, &t.name).clicked() {
                                    d.selected = i;
                                    d.preview = None;
                                }
                                ui.add_enabled(t.include, egui::DragValue::new(&mut t.rows).range(0..=1_000_000).speed(10.0));
                            });
                        }
                    });
                });

                egui::CentralPanel::default_margins().show(ui, |ui| {
                    let Some(t) = d.tables.get_mut(d.selected) else { return };
                    ui.label(RichText::new(format!("Columns of {}", t.name)).strong());
                    match &mut t.plan {
                        None => {}
                        Some(Err(e)) => {
                            ui.colored_label(p.err, e.as_str());
                        }
                        Some(Ok(plan)) => {
                            let mut changed = false;
                            egui::ScrollArea::vertical().id_salt("mock_cols").max_height(260.0).show(ui, |ui| {
                                egui::Grid::new("mock_grid").striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                                    for h in ["Column", "Type", "Generator", "Options", "NULL %"] {
                                        ui.label(RichText::new(h).weak());
                                    }
                                    ui.end_row();
                                    for (ci, c) in plan.columns.iter_mut().enumerate() {
                                        let unique = plan.unique_groups.iter().any(|g| g.contains(&ci));
                                        ui.label(RichText::new(&c.name).monospace()).on_hover_text(if unique { "part of a UNIQUE / PRIMARY KEY" } else { "" });
                                        ui.label(RichText::new(format!("{}{}", c.decl, if unique { " ·u" } else { "" })).small().color(p.keyword));
                                        let auto = t.auto.get(ci).cloned();
                                        egui::ComboBox::from_id_salt(("mock_gen", ci))
                                            .selected_text(c.generator.label())
                                            .width(150.0)
                                            .show_ui(ui, |ui| {
                                                if let Some(a) = &auto {
                                                    if ui.selectable_label(false, format!("Auto: {}", a.label())).clicked() {
                                                        c.generator = a.clone();
                                                        changed = true;
                                                    }
                                                    ui.separator();
                                                }
                                                for g in Gen::catalog() {
                                                    if ui.selectable_label(c.generator.label() == g.label(), g.label()).clicked() {
                                                        c.generator = g;
                                                        changed = true;
                                                    }
                                                }
                                            });
                                        ui.horizontal(|ui| {
                                            let before = c.generator.clone();
                                            gen_params(ui, &mut c.generator);
                                            changed |= before != c.generator;
                                        });
                                        ui.add_enabled_ui(!c.notnull && c.generator != Gen::Skip, |ui| {
                                            changed |= ui.add(egui::DragValue::new(&mut c.null_pct).range(0..=100).suffix("%")).changed();
                                        });
                                        ui.end_row();
                                    }
                                });
                            });
                            if changed {
                                d.preview = None;
                            }
                        }
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Preview").strong());
                        if ui.button("⟳ Preview 8 rows").clicked() || d.preview.is_none() {
                            d.preview = Some(self.mock_preview(t, d.seed));
                        }
                    });
                    match &d.preview {
                        Some(Ok((cols, rows))) => {
                            egui::ScrollArea::both().id_salt("mock_preview").max_height(190.0).show(ui, |ui| {
                                egui::Grid::new("mock_prev").striped(true).spacing([12.0, 3.0]).show(ui, |ui| {
                                    for c in cols {
                                        ui.label(RichText::new(c).strong());
                                    }
                                    ui.end_row();
                                    for r in rows {
                                        for v in r {
                                            ui.label(RichText::new(v).small());
                                        }
                                        ui.end_row();
                                    }
                                });
                            });
                        }
                        Some(Err(e)) => {
                            ui.colored_label(p.warn, e);
                        }
                        None => {}
                    }

                    ui.separator();
                    if let Some(r) = &d.report {
                        match r {
                            Ok(rep) => {
                                let parts: Vec<String> = rep
                                    .per_table
                                    .iter()
                                    .map(|(t, ins, fail)| if *fail > 0 { format!("{t}: {ins} (+{fail} rejected)") } else { format!("{t}: {ins}") })
                                    .collect();
                                ui.colored_label(p.ok, format!("Inserted {} in {:.0} ms. Write Changes to keep, Revert to undo.", parts.join(" · "), rep.millis));
                                for pr in rep.problems.iter().take(5) {
                                    ui.label(RichText::new(pr).small().color(p.warn));
                                }
                            }
                            Err(e) => {
                                ui.colored_label(p.err, e);
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        let n: usize = d.tables.iter().filter(|t| t.include).map(|t| t.rows).sum();
                        let k = d.tables.iter().filter(|t| t.include && t.rows > 0).count();
                        if ui.add_enabled(n > 0, egui::Button::new(format!("Generate {n} rows in {k} table(s)"))).clicked() {
                            d.report = Some(self.mock_run(d));
                            d.preview = None;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            });
        open && !close
    }

    fn mock_preview(&mut self, t: &MockTable, seed: u64) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
        let Some(Ok(plan)) = &t.plan else { return Err("No plan".into()) };
        let db = self.db.as_mut().ok_or("No database")?;
        let pools = mock::load_pools(db, plan)?;
        let rows = mock::generate(plan, 8, &mut Rng::new(seed), &pools, &[], 0)?;
        let keep: Vec<usize> = (0..plan.columns.len()).filter(|&i| plan.columns[i].generator != Gen::Skip).collect();
        Ok((
            keep.iter().map(|&i| plan.columns[i].name.clone()).collect(),
            rows.iter().map(|r| keep.iter().map(|&i| mock::preview_text(&r[i])).collect()).collect(),
        ))
    }

    fn mock_run(&mut self, d: &mut MockDialog) -> Result<MockReport, String> {
        let jobs: Vec<(TablePlan, usize)> = d
            .tables
            .iter()
            .filter(|t| t.include && t.rows > 0)
            .map(|t| match &t.plan {
                Some(Ok(p)) => Ok((p.clone(), t.rows)),
                Some(Err(e)) => Err(format!("{}: {e}", t.name)),
                None => Err(format!("{}: plan not ready", t.name)),
            })
            .collect::<Result<_, _>>()?;
        let db = self.db.as_mut().ok_or("No database")?;
        let res = mock::run(db, &jobs, d.seed);
        self.invalidate();
        res
    }

    fn include_parents(&mut self, d: &mut MockDialog) {
        let Some(db) = self.db.as_mut() else { return };
        loop {
            let mut added = false;
            let included: Vec<String> = d.tables.iter().filter(|t| t.include).map(|t| t.name.clone()).collect();
            for name in included {
                let Ok(det) = db.table_details(&name) else { continue };
                for fk in det.foreign_keys {
                    if let Some(t) = d.tables.iter_mut().find(|t| t.name.eq_ignore_ascii_case(&fk.table)) {
                        if !t.include && db.row_count(&t.name, "", &[]).unwrap_or(0) == 0 {
                            t.include = true;
                            t.rows = 20;
                            added = true;
                        }
                    }
                }
            }
            if !added {
                break;
            }
        }
    }
}

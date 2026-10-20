//! Database Structure tab: tree of tables / views / indices / triggers with
//! columns, keys, index definitions, foreign keys and the CREATE statements.

use egui::{CollapsingHeader, RichText, Ui};
use egui_extras::{Column, TableBuilder};

use crate::app::{App, Confirm, Tab};
use crate::db::{ObjKind, SchemaObject, TableDetails};
use crate::ui::dialogs::{CreateIndexDialog, CreateTableDialog, ExportDialog, ExportSource};
use crate::ui::widgets::{palette, sql_view};

#[derive(Default)]
pub struct StructureState {
    pub selected: Option<(ObjKind, String)>,
    pub details: Option<(String, Result<TableDetails, String>)>,
    pub filter: String,
}

impl StructureState {
    /// Table that the current selection refers to (index/trigger -> its table).
    pub fn selected_table(&self, schema: &[SchemaObject]) -> Option<String> {
        let (kind, name) = self.selected.as_ref()?;
        match kind {
            ObjKind::Table => Some(name.clone()),
            _ => schema.iter().find(|o| &o.name == name).map(|o| o.tbl_name.clone()),
        }
    }
}

impl App {
    pub fn structure_tab(&mut self, ui: &mut Ui) {
        let read_only = self.db.as_ref().is_some_and(|d| d.read_only);
        ui.horizontal(|ui| {
            if ui.add_enabled(!read_only, egui::Button::new("➕ Create Table")).clicked() {
                self.create_table = Some(CreateTableDialog::default());
            }
            if ui.add_enabled(!read_only, egui::Button::new("➕ Create Index")).clicked() {
                self.create_index = Some(CreateIndexDialog::new(self.structure.selected_table(&self.schema)));
            }
            let sel = self.structure.selected.clone();
            let can_drop = !read_only && sel.as_ref().is_some_and(|(_, n)| !n.starts_with("sqlite_"));
            if ui.add_enabled(can_drop, egui::Button::new("🗑 Drop")).clicked() {
                if let Some((k, n)) = sel {
                    self.confirm = Some(Confirm::Drop(k, n));
                }
            }
            if ui.button("⟳ Refresh").clicked() {
                self.invalidate();
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.structure.filter).hint_text("Filter objects…").desired_width(180.0));
        });
        ui.add_space(4.0);

        egui::Panel::left("structure_tree")
            .resizable(true)
            .default_size(300.0)
            .min_size(180.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.schema_tree(ui));
            });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.object_details(ui));
        });
    }

    fn schema_tree(&mut self, ui: &mut Ui) {
        let filter = self.structure.filter.to_lowercase();
        let mut action: Option<TreeAction> = None;
        for kind in [ObjKind::Table, ObjKind::View, ObjKind::Index, ObjKind::Trigger] {
            let items: Vec<&SchemaObject> = self
                .schema
                .iter()
                .filter(|o| o.kind == kind && (filter.is_empty() || o.name.to_lowercase().contains(&filter)))
                .collect();
            CollapsingHeader::new(RichText::new(format!("{} ({})", kind.label(), items.len())).strong())
                .id_salt(kind.label())
                .default_open(kind == ObjKind::Table || kind == ObjKind::View)
                .show(ui, |ui| {
                    for o in items {
                        let selected = self.structure.selected.as_ref() == Some(&(kind, o.name.clone()));
                        let icon = match kind {
                            ObjKind::Table => "▦",
                            ObjKind::View => "◫",
                            ObjKind::Index => "⚡",
                            ObjKind::Trigger => "↯",
                        };
                        let label = if kind == ObjKind::Index || kind == ObjKind::Trigger {
                            format!("{icon} {}  ·  {}", o.name, o.tbl_name)
                        } else {
                            format!("{icon} {}", o.name)
                        };
                        let r = ui.selectable_label(selected, label);
                        if r.clicked() {
                            action = Some(TreeAction::Select(kind, o.name.clone()));
                        }
                        if r.double_clicked() && matches!(kind, ObjKind::Table | ObjKind::View) {
                            action = Some(TreeAction::Browse(o.name.clone()));
                        }
                        r.context_menu(|ui| {
                            if matches!(kind, ObjKind::Table | ObjKind::View) {
                                if ui.button("Browse Table").clicked() {
                                    action = Some(TreeAction::Browse(o.name.clone()));
                                    ui.close();
                                }
                                if ui.button("Export as CSV…").clicked() {
                                    action = Some(TreeAction::Export(o.name.clone()));
                                    ui.close();
                                }
                            }
                            if kind == ObjKind::Table && ui.button("Create Index…").clicked() {
                                action = Some(TreeAction::Index(o.name.clone()));
                                ui.close();
                            }
                            if let Some(sql) = &o.sql {
                                if ui.button("Copy CREATE statement").clicked() {
                                    ui.ctx().copy_text(format!("{sql};"));
                                    ui.close();
                                }
                            }
                            if !o.name.starts_with("sqlite_") && ui.button(format!("Drop {}", kind.sql_word().to_lowercase())).clicked() {
                                action = Some(TreeAction::Drop(kind, o.name.clone()));
                                ui.close();
                            }
                        });
                    }
                });
        }

        match action {
            Some(TreeAction::Select(k, n)) => {
                self.structure.selected = Some((k, n));
                self.structure.details = None;
            }
            Some(TreeAction::Browse(n)) => {
                self.browse.select_table(n);
                self.tab = Tab::Browse;
            }
            Some(TreeAction::Export(n)) => self.export = Some(ExportDialog::new(ExportSource::Table(n))),
            Some(TreeAction::Index(n)) => self.create_index = Some(CreateIndexDialog::new(Some(n))),
            Some(TreeAction::Drop(k, n)) => self.confirm = Some(Confirm::Drop(k, n)),
            None => {}
        }
    }

    fn object_details(&mut self, ui: &mut Ui) {
        let Some((kind, name)) = self.structure.selected.clone() else {
            let (t, v, i, tr) = self.schema.iter().fold((0, 0, 0, 0), |acc, o| match o.kind {
                ObjKind::Table => (acc.0 + 1, acc.1, acc.2, acc.3),
                ObjKind::View => (acc.0, acc.1 + 1, acc.2, acc.3),
                ObjKind::Index => (acc.0, acc.1, acc.2 + 1, acc.3),
                ObjKind::Trigger => (acc.0, acc.1, acc.2, acc.3 + 1),
            });
            ui.add_space(8.0);
            ui.heading(self.db.as_ref().map(|d| d.file_name()).unwrap_or_default());
            ui.label(format!("{t} tables · {v} views · {i} indices · {tr} triggers"));
            ui.add_space(6.0);
            ui.label(RichText::new("Select an object on the left to see its schema. Double-click a table to browse it.").weak());
            return;
        };
        let Some(obj) = self.schema.iter().find(|o| o.kind == kind && o.name == name).cloned() else {
            self.structure.selected = None;
            return;
        };
        let p = palette(ui);

        ui.horizontal(|ui| {
            ui.heading(&obj.name);
            ui.label(RichText::new(kind.sql_word()).small().weak());
            if kind != ObjKind::Table && kind != ObjKind::View {
                ui.label(RichText::new(format!("on {}", obj.tbl_name)).weak());
            }
        });

        if matches!(kind, ObjKind::Table | ObjKind::View) {
            if self.structure.details.as_ref().is_none_or(|(n, _)| n != &name) {
                let d = self.db.as_mut().unwrap().table_details(&name);
                self.structure.details = Some((name.clone(), d));
            }
            match &self.structure.details.as_ref().unwrap().1 {
                Err(e) => {
                    ui.colored_label(p.err, e);
                }
                Ok(d) => {
                    ui.horizontal(|ui| {
                        if d.strict {
                            ui.label(RichText::new("STRICT").small().strong().color(p.keyword));
                        }
                        if d.without_rowid {
                            ui.label(RichText::new("WITHOUT ROWID").small().strong().color(p.keyword));
                        }
                        if d.has_rowid() && !d.columns.iter().any(|c| c.pk > 0) {
                            ui.label(RichText::new("rowid table").small().weak());
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Columns").strong());
                    columns_table(ui, d, &name);
                    if !d.indexes.is_empty() {
                        ui.add_space(8.0);
                        ui.label(RichText::new("Indices").strong());
                        egui::Grid::new(("idx", &name)).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                            for h in ["Name", "Columns", "Unique", "Origin", "Partial"] {
                                ui.label(RichText::new(h).weak());
                            }
                            ui.end_row();
                            for i in &d.indexes {
                                ui.label(RichText::new(&i.name).monospace());
                                ui.label(i.columns.join(", "));
                                ui.label(if i.unique { "✔" } else { "" });
                                ui.label(match i.origin.as_str() {
                                    "pk" => "PRIMARY KEY",
                                    "u" => "UNIQUE constraint",
                                    _ => "CREATE INDEX",
                                });
                                ui.label(if i.partial { "✔" } else { "" });
                                ui.end_row();
                            }
                        });
                    }
                    if !d.foreign_keys.is_empty() {
                        ui.add_space(8.0);
                        ui.label(RichText::new("Foreign keys").strong());
                        egui::Grid::new(("fk", &name)).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
                            for h in ["Column", "References", "On update", "On delete"] {
                                ui.label(RichText::new(h).weak());
                            }
                            ui.end_row();
                            for f in &d.foreign_keys {
                                ui.label(&f.from);
                                ui.label(format!("{}({})", f.table, f.to));
                                ui.label(&f.on_update);
                                ui.label(&f.on_delete);
                                ui.end_row();
                            }
                        });
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Browse data").clicked() {
                    self.browse.select_table(name.clone());
                    self.tab = Tab::Browse;
                }
                if ui.button("Export CSV…").clicked() {
                    self.export = Some(ExportDialog::new(ExportSource::Table(name.clone())));
                }
            });
        }

        ui.add_space(8.0);
        ui.label(RichText::new("Schema").strong());
        match &obj.sql {
            Some(sql) => sql_view(ui, &format!("{sql};")),
            None => {
                ui.label(RichText::new("(internal object, no SQL)").weak());
            }
        }
    }
}

enum TreeAction {
    Select(ObjKind, String),
    Browse(String),
    Export(String),
    Index(String),
    Drop(ObjKind, String),
}

fn columns_table(ui: &mut Ui, d: &TableDetails, name: &str) {
    let p = palette(ui);
    let h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    ui.push_id(("cols", name), |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .vscroll(false)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(30.0))
            .column(Column::initial(170.0).resizable(true).clip(true))
            .column(Column::initial(110.0).resizable(true))
            .column(Column::exact(36.0))
            .column(Column::exact(36.0))
            .column(Column::remainder().clip(true))
            .header(h, |mut r| {
                for t in ["#", "Name", "Type", "PK", "NN", "Default"] {
                    r.col(|ui| {
                        ui.label(RichText::new(t).weak());
                    });
                }
            })
            .body(|mut b| {
                for c in &d.columns {
                    b.row(h, |mut r| {
                        r.col(|ui| {
                            ui.label(RichText::new(c.cid.to_string()).weak());
                        });
                        r.col(|ui| {
                            let mut t = RichText::new(&c.name).monospace();
                            if c.pk > 0 {
                                t = t.strong();
                            }
                            ui.label(t);
                        });
                        r.col(|ui| {
                            let ty = if c.decl_type.is_empty() { "—".to_string() } else { c.decl_type.clone() };
                            let extra = match c.hidden {
                                2 => " (generated)",
                                3 => " (stored)",
                                _ => "",
                            };
                            ui.label(RichText::new(format!("{ty}{extra}")).color(p.keyword));
                        });
                        r.col(|ui| {
                            if c.pk > 0 {
                                ui.label("🔑");
                            }
                        });
                        r.col(|ui| {
                            if c.notnull {
                                ui.label("✔");
                            }
                        });
                        r.col(|ui| {
                            if let Some(dv) = &c.default {
                                ui.label(RichText::new(dv).monospace());
                            }
                        });
                    });
                }
            });
    });
}

//! ER Diagram tab: pan (drag background), zoom (Ctrl+scroll / pinch), drag
//! tables, hover to highlight relationships, double-click to open a table.

use egui::epaint::CubicBezierShape;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::app::{App, Tab};
use crate::db::ObjKind;
use crate::er::{self, ErModel, HEADER_H, ROW_H, TABLE_W};
use crate::ui::widgets::palette;

pub struct ErState {
    pub model: Option<ErModel>,
    pub dirty: bool,
    pub scene_rect: Rect,
    pub keys_only: bool,
    pub show_counts: bool,
    pub selected: Option<usize>,
    pub fit_requested: bool,
}

impl Default for ErState {
    fn default() -> Self {
        ErState {
            model: None,
            dirty: true,
            scene_rect: Rect::ZERO,
            keys_only: false,
            show_counts: true,
            selected: None,
            fit_requested: true,
        }
    }
}

fn pos(p: (f32, f32)) -> Pos2 {
    Pos2::new(p.0, p.1)
}

impl App {
    fn rebuild_er(&mut self) {
        self.er.dirty = false;
        let Some(db) = self.db.as_mut() else { return };
        match er::build(db, self.er.show_counts) {
            Ok(mut m) => {
                // keep positions of tables the user already arranged
                let old = self.er.model.take();
                er::auto_layout(&mut m, self.er.keys_only);
                if let Some(old) = old {
                    for t in &mut m.tables {
                        if let Some(o) = old.tables.iter().find(|o| o.name == t.name) {
                            t.x = o.x;
                            t.y = o.y;
                        }
                    }
                } else {
                    self.er.fit_requested = true;
                }
                self.er.model = Some(m);
            }
            Err(e) => self.errors.push(e),
        }
    }

    pub fn er_tab(&mut self, ui: &mut Ui) {
        if self.er.dirty {
            self.rebuild_er();
        }
        let p = palette(ui);

        // ---- toolbar
        let mut export: Option<&str> = None;
        ui.horizontal(|ui| {
            if ui.button("⟲ Auto layout").clicked() {
                if let Some(m) = self.er.model.as_mut() {
                    er::auto_layout(m, self.er.keys_only);
                    self.er.fit_requested = true;
                }
            }
            if ui.button("⛶ Fit").clicked() {
                self.er.fit_requested = true;
            }
            if ui.button("⟳ Reload").clicked() {
                self.er.dirty = true;
            }
            ui.separator();
            if ui.checkbox(&mut self.er.keys_only, "Keys only").changed() {
                if let Some(m) = self.er.model.as_mut() {
                    er::auto_layout(m, self.er.keys_only);
                    self.er.fit_requested = true;
                }
            }
            if ui.checkbox(&mut self.er.show_counts, "Row counts").changed() {
                self.er.dirty = true;
            }
            ui.separator();
            if ui.button("Export SVG…").clicked() {
                export = Some("svg");
            }
            if ui.button("Copy Mermaid").on_hover_text("erDiagram text for GitHub / GitLab / Notion / Confluence").clicked() {
                export = Some("mermaid");
            }
            if ui.button("Copy DOT").on_hover_text("Graphviz source").clicked() {
                export = Some("dot");
            }
            if let Some(m) = &self.er.model {
                ui.label(RichText::new(format!("{} tables · {} relationships", m.tables.len(), m.edges.len())).small().weak());
            }
        });
        ui.label(
            RichText::new("Drag background to pan · Ctrl+scroll to zoom · drag tables to arrange · double-click a table to open it")
                .small()
                .weak(),
        );

        let Some(model) = self.er.model.as_mut() else { return };
        let keys_only = self.er.keys_only;
        if let Some(kind) = export {
            let title = self.db.as_ref().map(|d| d.file_name()).unwrap_or_default();
            match kind {
                "svg" => {
                    let svg = er::to_svg(model, keys_only, &title);
                    let default = format!("{}_er.svg", title.trim_end_matches(".db"));
                    if let Some(path) = rfd::FileDialog::new().set_file_name(&default).add_filter("SVG", &["svg"]).save_file() {
                        match std::fs::write(&path, svg) {
                            Ok(()) => self.status = Some((format!("Saved {}", path.display()), std::time::Instant::now())),
                            Err(e) => self.errors.push(e.to_string()),
                        }
                    } else {
                        // No file chooser available: save next to the database.
                        let path = self.db.as_ref().map(|d| d.path.with_file_name(&default)).unwrap();
                        if std::fs::write(&path, svg).is_ok() {
                            self.status = Some((format!("Saved {}", path.display()), std::time::Instant::now()));
                        }
                    }
                }
                "mermaid" => {
                    ui.ctx().copy_text(er::to_mermaid(model, keys_only));
                    self.status = Some(("Mermaid erDiagram copied to clipboard.".into(), std::time::Instant::now()));
                }
                _ => {
                    ui.ctx().copy_text(er::to_dot(model, keys_only));
                    self.status = Some(("Graphviz DOT copied to clipboard.".into(), std::time::Instant::now()));
                }
            }
        }

        if model.tables.is_empty() {
            ui.label(RichText::new("No tables yet.").weak());
            return;
        }

        if self.er.fit_requested {
            let (x0, y0, x1, y1) = er::bounds(model, keys_only);
            self.er.scene_rect = Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)).expand(40.0);
            self.er.fit_requested = false;
        }

        let mut open_table: Option<String> = None;
        let mut selected = self.er.selected;
        let dark = ui.visuals().dark_mode;
        let canvas_bg = if dark { Color32::from_gray(22) } else { Color32::from_gray(246) };
        let frame = egui::Frame::new().fill(canvas_bg).corner_radius(4.0);
        frame.show(ui, |ui| {
            egui::Scene::new().zoom_range(0.1..=3.0).show(ui, &mut self.er.scene_rect, |ui| {
                // reserve the whole diagram area so the scene knows its extent
                let (x0, y0, x1, y1) = er::bounds(model, keys_only);
                ui.allocate_rect(Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1)).expand(20.0), Sense::hover());

                // interactions first (so we know hover/drag before painting)
                let mut hovered: Option<usize> = None;
                for i in 0..model.tables.len() {
                    let t = &model.tables[i];
                    let rect = Rect::from_min_size(Pos2::new(t.x, t.y), Vec2::new(TABLE_W, t.height(keys_only)));
                    let r = ui.interact(rect, ui.scope_id().with(("er_table", i)), Sense::click_and_drag());
                    if r.dragged() {
                        let d = r.drag_delta();
                        model.tables[i].x += d.x;
                        model.tables[i].y += d.y;
                    }
                    if r.hovered() || r.dragged() {
                        hovered = Some(i);
                    }
                    if r.clicked() {
                        selected = Some(i);
                    }
                    if r.double_clicked() {
                        open_table = Some(model.tables[i].name.clone());
                    }
                }
                let focus = hovered.or(selected);
                let painter = ui.painter();

                // ---- edges
                let edge_col = if dark { Color32::from_gray(120) } else { Color32::from_gray(140) };
                for e in &model.edges {
                    let hot = focus.is_some_and(|f| f == e.from || f == e.to);
                    let color = if e.dangling { p.err } else if hot { p.keyword } else { edge_col };
                    let stroke = Stroke::new(if hot { 2.2 } else { 1.3 }, color);
                    let g = er::edge_geometry(model, e, keys_only);
                    let pts = [pos(g[0]), pos(g[1]), pos(g[2]), pos(g[3])];
                    if e.optional {
                        // dashed: sample the curve and draw every other segment
                        let bez = CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, stroke);
                        let samples: Vec<Pos2> = bez.flatten(Some(0.5));
                        for (k, w) in samples.windows(2).enumerate() {
                            if (k / 3) % 2 == 0 {
                                painter.line_segment([w[0], w[1]], stroke);
                            }
                        }
                    } else {
                        painter.add(CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, stroke));
                    }
                    // child end: crow's foot (many) or bar (one)
                    let cdir = if g[1].0 >= g[0].0 { 1.0 } else { -1.0 };
                    let c = pts[0];
                    if e.one_to_one {
                        painter.line_segment([c + Vec2::new(cdir * 10.0, -6.0), c + Vec2::new(cdir * 10.0, 6.0)], stroke);
                    } else {
                        for dy in [-6.0, 0.0, 6.0] {
                            painter.line_segment([c + Vec2::new(cdir * 12.0, 0.0), c + Vec2::new(0.0, dy)], stroke);
                        }
                    }
                    // parent end: bar (+ circle when optional)
                    let pdir = if g[2].0 <= g[3].0 { -1.0 } else { 1.0 };
                    let pp = pts[3];
                    painter.line_segment([pp + Vec2::new(pdir * 8.0, -6.0), pp + Vec2::new(pdir * 8.0, 6.0)], stroke);
                    if e.optional {
                        painter.circle_stroke(pp + Vec2::new(pdir * 16.0, 0.0), 3.5, stroke);
                    }
                    if hot {
                        let mid = pos(((g[1].0 + g[2].0) / 2.0, (g[1].1 + g[2].1) / 2.0));
                        painter.text(mid, Align2::CENTER_BOTTOM, e.from_cols.join(", "), FontId::proportional(10.0), color);
                    }
                }

                // ---- tables
                let body_bg = if dark { Color32::from_gray(38) } else { Color32::WHITE };
                let border = if dark { Color32::from_gray(80) } else { Color32::from_gray(190) };
                let text = ui.visuals().text_color();
                let weak = ui.visuals().weak_text_color();
                for (i, t) in model.tables.iter().enumerate() {
                    let h = t.height(keys_only);
                    let rect = Rect::from_min_size(Pos2::new(t.x, t.y), Vec2::new(TABLE_W, h));
                    let is_focus = focus == Some(i);
                    painter.rect_filled(rect.translate(Vec2::new(2.0, 3.0)), 6.0, Color32::from_black_alpha(if dark { 90 } else { 25 }));
                    painter.rect_filled(rect, 6.0, body_bg);
                    let head = Rect::from_min_size(rect.min, Vec2::new(TABLE_W, HEADER_H));
                    let head_col = if t.columns.is_empty() { Color32::from_gray(120) } else if is_focus { p.app } else { Color32::from_rgb(0x25, 0x63, 0xeb) };
                    painter.rect_filled(head, CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, head_col);
                    painter.text(head.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, &t.name, FontId::proportional(13.5), Color32::WHITE);
                    if let Some(n) = t.row_count {
                        painter.text(head.right_center() - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER, format!("{n} rows"), FontId::proportional(10.0), Color32::from_rgb(0xdb, 0xea, 0xfe));
                    }
                    if t.columns.is_empty() {
                        painter.text(rect.min + Vec2::new(10.0, HEADER_H + 10.0), Align2::LEFT_CENTER, "(missing table)", FontId::proportional(11.0), weak);
                    }
                    for (k, c) in t.visible_columns(keys_only).iter().enumerate() {
                        let y = t.y + HEADER_H + k as f32 * ROW_H + ROW_H / 2.0;
                        if k % 2 == 1 {
                            painter.rect_filled(
                                Rect::from_min_size(Pos2::new(t.x + 1.0, y - ROW_H / 2.0), Vec2::new(TABLE_W - 2.0, ROW_H)),
                                0.0,
                                ui.visuals().faint_bg_color,
                            );
                        }
                        let (badge, bc) = match (c.pk, c.fk) {
                            (true, true) => ("PF", p.warn),
                            (true, false) => ("PK", p.warn),
                            (false, true) => ("FK", p.ident),
                            _ => ("", weak),
                        };
                        painter.text(Pos2::new(t.x + 8.0, y), Align2::LEFT_CENTER, badge, FontId::monospace(10.0), bc);
                        painter.text(Pos2::new(t.x + 30.0, y), Align2::LEFT_CENTER, &c.name, FontId::proportional(12.5), text);
                        let ty = format!("{}{}", c.ty.to_lowercase(), if c.notnull && !c.pk { " ·nn" } else { "" });
                        painter.text(Pos2::new(t.x + TABLE_W - 8.0, y), Align2::RIGHT_CENTER, ty, FontId::proportional(11.0), weak);
                    }
                    let stroke = if is_focus { Stroke::new(2.0, p.keyword) } else { Stroke::new(1.0, border) };
                    painter.rect_stroke(rect, 6.0, stroke, StrokeKind::Inside);
                }
            });
        });
        self.er.selected = selected;

        if let Some(name) = open_table {
            self.structure.selected = Some((ObjKind::Table, name.clone()));
            self.structure.details = None;
            self.browse.select_table(name);
            self.tab = Tab::Structure;
        }
    }
}

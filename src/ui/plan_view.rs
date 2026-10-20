//! Visual query plan: verdict banner, painted plan tree with colour-coded
//! steps, and findings with one-click index suggestions.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};

use crate::plan::{NodeKind, PlanAnalysis, Severity};
use crate::ui::widgets::{palette, Palette};

pub enum PlanAction {
    CreateIndex(String, Vec<String>),
    Reexplain,
}

fn sev_color(p: &Palette, s: Severity, ui: &Ui) -> Color32 {
    match s {
        Severity::Good => p.ok,
        Severity::Info => if ui.visuals().dark_mode { Color32::from_gray(150) } else { Color32::from_gray(110) },
        Severity::Warn => p.warn,
        Severity::Bad => p.err,
    }
}

fn icon(k: NodeKind) -> &'static str {
    match k {
        NodeKind::KeyLookup => "🔑",
        NodeKind::IndexSearch => "⚡",
        NodeKind::CoveringScan => "📗",
        NodeKind::IndexScan => "📘",
        NodeKind::FullScan => "▦",
        NodeKind::AutoIndex => "⚙",
        NodeKind::TempBTree => "⇅",
        NodeKind::Structure => "◇",
        NodeKind::Other => "•",
    }
}

pub fn plan_view(ui: &mut Ui, a: &PlanAnalysis) -> Option<PlanAction> {
    let p = palette(ui);
    let mut action = None;

    // ---- verdict
    let worst = a.worst();
    let (verdict, color) = match worst {
        Severity::Good | Severity::Info => ("✔ Efficient plan", p.ok),
        Severity::Warn => ("⚠ Could be faster", p.warn),
        Severity::Bad => ("✖ Likely slow on large data", p.err),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(verdict).strong().size(15.0).color(color));
        let count = |k: NodeKind| a.nodes.iter().filter(|n| n.kind == k).count();
        let mut parts = vec![];
        for (k, word) in [
            (NodeKind::KeyLookup, "key lookup"),
            (NodeKind::IndexSearch, "index search"),
            (NodeKind::CoveringScan, "covering scan"),
            (NodeKind::IndexScan, "index scan"),
            (NodeKind::FullScan, "full scan"),
            (NodeKind::AutoIndex, "automatic index"),
            (NodeKind::TempBTree, "temp sort"),
        ] {
            let c = count(k);
            if c > 0 {
                parts.push(format!("{c} {word}{}", if c > 1 { "s" } else { "" }));
            }
        }
        ui.label(RichText::new(parts.join(" · ")).small().weak());
        ui.label(RichText::new(format!("planned in {:.1} ms", a.millis)).small().weak());
        if ui.small_button("⟳ Re-explain").clicked() {
            action = Some(PlanAction::Reexplain);
        }
    });

    egui::ScrollArea::vertical().id_salt("plan_scroll").auto_shrink([false, false]).show(ui, |ui| {
        // ---- painted tree
        let row_h = 38.0;
        let indent = 30.0;
        let width = ui.available_width().min(1100.0);
        let height = a.nodes.len().max(1) as f32 * row_h + 8.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter_at(rect);
        let text_col = ui.visuals().text_color();
        let weak = ui.visuals().weak_text_color();
        let box_bg = ui.visuals().faint_bg_color;
        let line = Stroke::new(1.2, ui.visuals().widgets.noninteractive.bg_stroke.color);
        let mut pos_of: std::collections::HashMap<i64, Pos2> = std::collections::HashMap::new();
        for (i, n) in a.nodes.iter().enumerate() {
            let x = rect.left() + 6.0 + n.depth as f32 * indent;
            let y = rect.top() + 4.0 + i as f32 * row_h;
            let b = Rect::from_min_size(Pos2::new(x, y), Vec2::new(rect.right() - x - 6.0, row_h - 8.0));
            // elbow connector from parent
            if let Some(pp) = pos_of.get(&n.parent) {
                let elbow_x = pp.x + 12.0;
                painter.line_segment([Pos2::new(elbow_x, pp.y), Pos2::new(elbow_x, b.center().y)], line);
                painter.line_segment([Pos2::new(elbow_x, b.center().y), Pos2::new(b.left(), b.center().y)], line);
            }
            pos_of.insert(n.id, Pos2::new(b.left(), b.bottom()));

            let c = sev_color(&p, n.severity, ui);
            painter.rect_filled(b, 5.0, box_bg);
            painter.rect_filled(Rect::from_min_size(b.min, Vec2::new(5.0, b.height())), CornerRadius { nw: 5, sw: 5, ne: 0, se: 0 }, c);
            // kind pill
            let pill_text = format!("{} {}", icon(n.kind), n.kind.label());
            let galley = painter.layout_no_wrap(pill_text, FontId::proportional(10.5), Color32::WHITE);
            let pill = Rect::from_min_size(Pos2::new(b.left() + 12.0, b.center().y - 9.0), Vec2::new(galley.size().x + 12.0, 18.0));
            painter.rect_filled(pill, 9.0, c.gamma_multiply(0.85));
            painter.galley(Pos2::new(pill.left() + 6.0, pill.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
            // detail
            painter.text(
                Pos2::new(pill.right() + 10.0, b.center().y),
                Align2::LEFT_CENTER,
                &n.detail,
                FontId::monospace(12.0),
                text_col,
            );
            // right-hand facts
            let mut facts = vec![];
            if let Some(r) = n.rows {
                facts.push(format!("{r} rows in table"));
            }
            if let Some(ix) = &n.index {
                facts.push(format!("via {ix}"));
            }
            if n.inner_loop {
                facts.push("inner loop".into());
            }
            if let Some(t) = &n.table {
                if n.object.as_deref() != Some(t.as_str()) {
                    facts.push(format!("= {t}"));
                }
            }
            if !facts.is_empty() {
                painter.text(Pos2::new(b.right() - 10.0, b.center().y), Align2::RIGHT_CENTER, facts.join(" · "), FontId::proportional(11.0), weak);
            }
        }

        // ---- legend
        ui.horizontal_wrapped(|ui| {
            for (s, t) in [(Severity::Good, "uses a key or index"), (Severity::Info, "neutral"), (Severity::Warn, "worth a look"), (Severity::Bad, "expensive")] {
                ui.label(RichText::new("■").color(sev_color(&p, s, ui)));
                ui.label(RichText::new(t).small().weak());
            }
        });
        ui.add_space(6.0);

        // ---- findings
        ui.label(RichText::new("Findings").strong());
        for f in &a.findings {
            ui.horizontal_wrapped(|ui| {
                let c = sev_color(&p, f.severity, ui);
                let mark = match f.severity {
                    Severity::Good => "✔",
                    Severity::Info => "ℹ",
                    Severity::Warn => "⚠",
                    Severity::Bad => "✖",
                };
                ui.label(RichText::new(mark).color(c));
                ui.label(&f.text);
            });
            if let (Some((t, cols)), Some(sql)) = (&f.suggest, f.index_sql()) {
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    ui.label(RichText::new(&sql).monospace().small().color(p.keyword));
                    if ui.small_button("Create index…").clicked() {
                        action = Some(PlanAction::CreateIndex(t.clone(), cols.clone()));
                    }
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(sql.clone());
                    }
                });
            }
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Suggestions are heuristics based on the plan and the query text — re-explain after creating an index to confirm SQLite uses it.")
                .small()
                .weak(),
        );
    });
    action
}

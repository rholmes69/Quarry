//! SQL Log dock: every raw statement executed behind the UI, tagged by who
//! submitted it (Application vs User), with timing and result.

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};

use crate::app::{App, Tab};
use crate::db::LogSource;
use crate::ui::widgets::palette;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogFilter {
    All,
    App,
    User,
    Remote,
}

pub struct LogUi {
    pub visible: bool,
    pub filter: LogFilter,
    pub errors_only: bool,
    pub search: String,
}

impl Default for LogUi {
    fn default() -> Self {
        LogUi { visible: true, filter: LogFilter::All, errors_only: false, search: String::new() }
    }
}

impl App {
    pub fn log_panel(&mut self, ui: &mut Ui) {
        let p = palette(ui);
        let mut clear = false;
        let mut send_to_editor: Option<String> = None;

        ui.horizontal(|ui| {
            ui.label(RichText::new("SQL Log").strong());
            ui.separator();
            ui.label("Submitted by:");
            egui::ComboBox::from_id_salt("log_filter")
                .selected_text(match self.log_ui.filter {
                    LogFilter::All => "All",
                    LogFilter::App => "Application",
                    LogFilter::User => "User",
                    LogFilter::Remote => "Remote server",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.log_ui.filter, LogFilter::All, "All");
                    ui.selectable_value(&mut self.log_ui.filter, LogFilter::App, "Application");
                    ui.selectable_value(&mut self.log_ui.filter, LogFilter::User, "User");
                    ui.selectable_value(&mut self.log_ui.filter, LogFilter::Remote, "Remote server");
                });
            ui.checkbox(&mut self.log_ui.errors_only, "Errors only");
            ui.add(egui::TextEdit::singleline(&mut self.log_ui.search).hint_text("Search…").desired_width(160.0));
            if ui.button("Copy all").clicked() {
                if let Some(db) = &self.db {
                    let text: String = db.log.iter().map(|l| format!("-- {} [{:?}]\n{}\n", l.time, l.source, l.sql)).collect();
                    ui.ctx().copy_text(text);
                    self.notify("SQL log copied to clipboard.");
                }
            }
            if ui.button("Save…").clicked() {
                if let Some(db) = &self.db {
                    if let Some(path) = rfd::FileDialog::new().set_file_name("sql_log.sql").save_file() {
                        let text: String = db
                            .log
                            .iter()
                            .map(|l| format!("-- {} {:?} {:.2} ms {}\n{}\n\n", l.time, l.source, l.millis, l.detail, l.sql))
                            .collect();
                        if let Err(e) = std::fs::write(&path, text) {
                            self.errors.push(e.to_string());
                        }
                    }
                }
            }
            if ui.button("Clear").clicked() {
                clear = true;
            }
        });

        let Some(db) = self.db.as_mut() else {
            ui.label(RichText::new("Open a database to see the statements it runs.").weak());
            return;
        };
        if clear {
            db.log.clear();
        }
        let search = self.log_ui.search.to_lowercase();
        let entries: Vec<usize> = db
            .log
            .iter()
            .enumerate()
            .filter(|(_, l)| match self.log_ui.filter {
                LogFilter::All => true,
                LogFilter::App => l.source == LogSource::App,
                LogFilter::User => l.source == LogSource::User,
                LogFilter::Remote => l.source == LogSource::Remote,
            })
            .filter(|(_, l)| !self.log_ui.errors_only || !l.ok)
            .filter(|(_, l)| search.is_empty() || l.sql.to_lowercase().contains(&search))
            .map(|(i, _)| i)
            .collect();

        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 6.0;
        TableBuilder::new(ui)
            .id_salt("sql_log_table")
            .striped(true)
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(92.0))
            .column(Column::exact(44.0))
            .column(Column::exact(64.0))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(200.0).clip(true))
            .header(row_h, |mut h| {
                for t in ["Time", "By", "ms", "Statement", "Result"] {
                    h.col(|ui| {
                        ui.label(RichText::new(t).strong());
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, entries.len(), |mut row| {
                    let l = &db.log[entries[row.index()]];
                    row.col(|ui| {
                        ui.label(RichText::new(&l.time).small().weak());
                    });
                    row.col(|ui| {
                        let (t, c) = match l.source {
                            LogSource::App => ("App", p.app),
                            LogSource::User => ("User", p.user),
                            LogSource::Remote => ("Remote", p.ok),
                        };
                        ui.label(RichText::new(t).small().strong().color(c));
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format!("{:.2}", l.millis)).small().weak());
                    });
                    row.col(|ui| {
                        let one_line: String = l.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                        let r = ui.add(
                            egui::Label::new(RichText::new(one_line).monospace())
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        let r = r.on_hover_text(RichText::new(&l.sql).monospace());
                        r.context_menu(|ui| {
                            if ui.button("Copy statement").clicked() {
                                ui.ctx().copy_text(l.sql.clone());
                                ui.close();
                            }
                            if ui.button("Open in Execute SQL").clicked() {
                                send_to_editor = Some(l.sql.clone());
                                ui.close();
                            }
                        });
                        if r.double_clicked() {
                            send_to_editor = Some(l.sql.clone());
                        }
                    });
                    row.col(|ui| {
                        let c = if l.ok { ui.visuals().weak_text_color() } else { p.err };
                        ui.label(RichText::new(&l.detail).small().color(c)).on_hover_text(&l.detail);
                    });
                });
            });

        if let Some(sql) = send_to_editor {
            if !self.exec.sql.trim().is_empty() && !self.exec.sql.ends_with('\n') {
                self.exec.sql.push('\n');
            }
            self.exec.sql.push_str(&sql);
            if !sql.trim_end().ends_with(';') {
                self.exec.sql.push(';');
            }
            self.exec.sql.push('\n');
            self.tab = Tab::Execute;
        }
    }
}

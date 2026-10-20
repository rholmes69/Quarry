//! Data Pipe tab: connect to PostgreSQL / MySQL / SQL Server and move data
//! in either direction. Network work runs on a worker thread.

use std::sync::mpsc::Receiver;

use egui::{RichText, Ui};

use crate::app::{App, Tab};
use crate::db::LogSource;
use crate::pipe::{self, ColClass, ConnSpec, Job, Kind, Msg, Output, PullMode, PushMode, TlsMode};
use crate::ui::widgets::{palette, sql_editor};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Pull,
    Push,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Running {
    Test,
    List,
    Pull,
    Push,
}

pub struct PipeState {
    profiles: Vec<(String, ConnSpec)>,
    profile_name: String,
    spec: ConnSpec,
    status: Option<(bool, String)>,
    remote_tables: Vec<String>,
    dir: Dir,
    // pull
    pull_query: bool,
    pull_table: String,
    pull_sql: String,
    pull_limit_on: bool,
    pull_limit: usize,
    pull_target: String,
    pull_mode: PullMode,
    // push
    push_query: bool,
    push_table: String,
    push_sql: String,
    push_target: String,
    push_mode: PushMode,
    mapping: Option<Vec<(String, String, String)>>,
    // job
    rx: Option<(Receiver<Msg>, Running)>,
    progress: (String, f32),
    result: Option<(bool, String)>,
}

impl Default for PipeState {
    fn default() -> Self {
        let profiles = pipe::load_profiles();
        let (profile_name, spec) = profiles.first().cloned().unwrap_or_else(|| (String::new(), ConnSpec::new(Kind::Postgres)));
        PipeState {
            profiles,
            profile_name,
            spec,
            status: None,
            remote_tables: vec![],
            dir: Dir::Pull,
            pull_query: false,
            pull_table: String::new(),
            pull_sql: "SELECT *\nFROM my_table\nWHERE updated_at >= '2026-01-01'".into(),
            pull_limit_on: false,
            pull_limit: 10_000,
            pull_target: String::new(),
            pull_mode: PullMode::Create,
            push_query: false,
            push_table: String::new(),
            push_sql: String::new(),
            push_target: String::new(),
            push_mode: PushMode::CreateOrAppend,
            mapping: None,
            rx: None,
            progress: (String::new(), 0.0),
            result: None,
        }
    }
}

fn local_name(remote: &str) -> String {
    remote.rsplit('.').next().unwrap_or(remote).chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

impl App {
    /// Called every frame (any tab) so background jobs finish even if the
    /// user switched away.
    pub fn pipe_poll(&mut self, ctx: &egui::Context) {
        let Some((rx, kind)) = self.pipe.rx.take() else { return };
        let mut done = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Log(l) => {
                    if let Some(db) = self.db.as_mut() {
                        db.push_log(LogSource::Remote, &l.sql, l.ok, l.millis, l.detail);
                    }
                }
                Msg::Progress(t, f) => self.pipe.progress = (t, f),
                Msg::Done(r) => done = Some(r),
            }
        }
        let Some(result) = done else {
            self.pipe.rx = Some((rx, kind));
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
            return;
        };
        match (kind, result) {
            (_, Err(e)) => {
                if matches!(kind, Running::Test | Running::List) {
                    self.pipe.status = Some((false, e));
                } else {
                    self.pipe.result = Some((false, e));
                }
            }
            (_, Ok(Output::Tested(v))) => {
                self.pipe.status = Some((true, v));
                self.pipe_start(Job::ListTables, Running::List);
            }
            (_, Ok(Output::Tables(t))) => {
                if self.pipe.pull_table.is_empty() {
                    if let Some(first) = t.first() {
                        self.pipe.pull_table = first.clone();
                        self.pipe.pull_target = local_name(first);
                    }
                }
                self.pipe.remote_tables = t;
            }
            (_, Ok(Output::Pulled(data))) => {
                let target = self.pipe.pull_target.trim().to_string();
                let res = match self.db.as_mut() {
                    Some(db) => pipe::write_local(db, &target, &data, self.pipe.pull_mode),
                    None => Err("No database open.".into()),
                };
                self.pipe.result = Some(match res {
                    Ok(n) => {
                        self.invalidate();
                        self.browse.select_table(target.clone());
                        (true, format!("Pulled {n} row(s) × {} column(s) into '{target}'. Write Changes to keep them.", data.columns.len()))
                    }
                    Err(e) => (false, e),
                });
            }
            (_, Ok(Output::Pushed { rows, created, widened })) => {
                let mut msg = format!(
                    "Pushed {rows} row(s) to {} on {}{}.",
                    self.pipe.push_target,
                    self.pipe.spec.host,
                    if created { " (table created)" } else { "" }
                );
                if !widened.is_empty() {
                    msg.push_str(&format!(
                        " Note: {} declared numeric but hold text values in SQLite, so they were sent as text instead of failing.",
                        widened.join(", ")
                    ));
                }
                self.pipe.result = Some((true, msg));
                self.pipe.remote_tables.clear();
                self.pipe_start(Job::ListTables, Running::List);
            }
        }
        self.pipe.progress = (String::new(), 0.0);
    }

    fn pipe_start(&mut self, job: Job, kind: Running) {
        if matches!(kind, Running::Pull | Running::Push) {
            self.pipe.result = None;
        }
        self.pipe.rx = Some((pipe::spawn(self.pipe.spec.clone(), job), kind));
    }

    pub fn pipe_tab(&mut self, ui: &mut Ui) {
        let p = palette(ui);
        let busy = self.pipe.rx.is_some();

        egui::Panel::left("pipe_conn").resizable(true).default_size(330.0).min_size(280.0).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("pipe_conn_scroll").show(ui, |ui| self.pipe_connection(ui, busy));
        });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            let before = self.pipe.dir;
            ui.horizontal(|ui| {
                let big = |t: &str| RichText::new(t).size(15.0).strong();
                ui.selectable_value(&mut self.pipe.dir, Dir::Pull, big("⬇  Pull: server → SQLite"));
                ui.selectable_value(&mut self.pipe.dir, Dir::Push, big("⬆  Push: SQLite → server"));
            });
            if before != self.pipe.dir {
                self.pipe.result = None;
            }
            ui.separator();
            egui::ScrollArea::vertical().id_salt("pipe_main_scroll").auto_shrink([false, false]).show(ui, |ui| {
            match self.pipe.dir {
                Dir::Pull => self.pipe_pull(ui, busy),
                Dir::Push => self.pipe_push(ui, busy),
            }
            ui.add_space(8.0);
            if busy {
                let (t, f) = self.pipe.progress.clone();
                ui.add(egui::ProgressBar::new(f.max(0.02)).text(if t.is_empty() { "Working…".to_string() } else { t }).animate(true));
            }
            if let Some((ok, m)) = &self.pipe.result {
                ui.colored_label(if *ok { p.ok } else { p.err }, m);
                if *ok && self.pipe.dir == Dir::Pull && ui.button("Browse pulled table").clicked() {
                    self.tab = Tab::Browse;
                }
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("Every statement sent to the server is in the SQL Log under \"Remote server\". Pulls land in a savepoint — Revert undoes them.")
                    .small()
                    .weak(),
            );
            });
        });
    }

    fn pipe_connection(&mut self, ui: &mut Ui, busy: bool) {
        let p = palette(ui);
        let s = &mut self.pipe;
        ui.label(RichText::new("Connection").strong());
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("pipe_profiles")
                .selected_text(if s.profile_name.is_empty() { "(unsaved)" } else { &s.profile_name })
                .width(170.0)
                .show_ui(ui, |ui| {
                    for (n, c) in &s.profiles {
                        if ui.selectable_label(&s.profile_name == n, n).clicked() {
                            s.profile_name = n.clone();
                            s.spec = c.clone();
                            s.status = None;
                            s.remote_tables.clear();
                        }
                    }
                });
            if ui.button("New").clicked() {
                s.profile_name.clear();
                s.spec = ConnSpec::new(s.spec.kind);
                s.status = None;
                s.remote_tables.clear();
            }
        });
        egui::Grid::new("pipe_conn_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Server");
            let prev = s.spec.kind;
            egui::ComboBox::from_id_salt("pipe_kind").selected_text(s.spec.kind.label()).width(190.0).show_ui(ui, |ui| {
                for k in Kind::ALL {
                    ui.add_enabled_ui(k.compiled_in(), |ui| {
                        ui.selectable_value(&mut s.spec.kind, k, k.label());
                    });
                }
            });
            if prev != s.spec.kind && s.spec.port == prev.default_port() {
                s.spec.port = s.spec.kind.default_port();
            }
            ui.end_row();
            ui.label("Host");
            ui.text_edit_singleline(&mut s.spec.host);
            ui.end_row();
            ui.label("Port");
            ui.add(egui::DragValue::new(&mut s.spec.port).range(1..=65535));
            ui.end_row();
            ui.label("Database");
            ui.text_edit_singleline(&mut s.spec.database);
            ui.end_row();
            ui.label("User");
            ui.text_edit_singleline(&mut s.spec.user);
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut s.spec.password).password(true));
            ui.end_row();
            ui.label("Encryption");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut s.spec.tls, TlsMode::Disable, "Off");
                ui.selectable_value(&mut s.spec.tls, TlsMode::Require, "Require TLS");
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut s.spec.trust_cert, "Trust server certificate").on_hover_text("Accept self-signed certificates (dev / internal servers)");
            ui.end_row();
        });
        if s.spec.kind == Kind::MsSql && s.spec.tls == TlsMode::Disable {
            ui.label(RichText::new("SQL Server always encrypts the login; \"Off\" leaves the rest of the session in clear text.").small().weak());
        }
        ui.add_space(4.0);
        let mut test = false;
        let mut save = false;
        let mut delete = false;
        ui.horizontal(|ui| {
            test = ui.add_enabled(!busy, egui::Button::new("🔌 Connect & list tables")).clicked();
            save = ui.button("💾 Save profile").on_hover_text("Saves host, port, database, user and TLS — never the password").clicked();
            delete = ui.add_enabled(!s.profile_name.is_empty(), egui::Button::new("🗑")).on_hover_text("Delete profile").clicked();
        });
        if save {
            let name = if s.profile_name.is_empty() {
                format!("{} {}", s.spec.database, s.spec.host).trim().to_string()
            } else {
                s.profile_name.clone()
            };
            s.profiles.retain(|(n, _)| n != &name);
            s.profiles.push((name.clone(), s.spec.clone()));
            s.profile_name = name;
            if let Err(e) = pipe::save_profiles(&s.profiles) {
                s.status = Some((false, e));
            }
        }
        if delete {
            let n = s.profile_name.clone();
            s.profiles.retain(|(x, _)| x != &n);
            s.profile_name.clear();
            let _ = pipe::save_profiles(&s.profiles);
        }
        if let Some((ok, msg)) = &s.status {
            ui.colored_label(if *ok { p.ok } else { p.err }, if *ok { format!("✔ {msg}") } else { format!("✖ {msg}") });
        }
        if test {
            s.status = None;
            self.pipe_start(Job::Test, Running::Test);
        }

        let s = &mut self.pipe;
        if !s.remote_tables.is_empty() {
            ui.add_space(8.0);
            let n = s.remote_tables.len();
            let refresh = ui
                .horizontal(|ui| {
                    ui.label(RichText::new(format!("Remote tables ({n})")).strong());
                    ui.small_button("⟳").clicked()
                })
                .inner;
            if refresh && !busy {
                self.pipe_start(Job::ListTables, Running::List);
            }
            let s = &mut self.pipe;
            egui::ScrollArea::vertical().id_salt("pipe_remote_tables").max_height(260.0).show(ui, |ui| {
                for t in s.remote_tables.clone() {
                    let sel = s.pull_table == t && !s.pull_query;
                    if ui.selectable_label(sel, format!("▦ {t}")).on_hover_text("Click to pull this table").clicked() {
                        s.dir = Dir::Pull;
                        s.pull_query = false;
                        s.pull_table = t.clone();
                        s.pull_target = local_name(&t);
                    }
                }
            });
        }
    }

    fn pipe_pull(&mut self, ui: &mut Ui, busy: bool) {
        let local_tables = self.tables(false);
        let s = &mut self.pipe;
        let connected = s.status.as_ref().is_some_and(|x| x.0);
        ui.label(RichText::new("Source on the server").strong());
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.pull_query, false, "Table");
            ui.add_enabled_ui(!s.pull_query, |ui| {
                egui::ComboBox::from_id_salt("pull_table")
                    .selected_text(if s.pull_table.is_empty() { "connect first…" } else { &s.pull_table })
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for t in &s.remote_tables {
                            if ui.selectable_label(&s.pull_table == t, t).clicked() {
                                s.pull_table = t.clone();
                                s.pull_target = local_name(t);
                            }
                        }
                    });
            });
            ui.radio_value(&mut s.pull_query, true, "Custom query");
        });
        if s.pull_query {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                sql_editor(ui, &mut s.pull_sql, 5, true);
            });
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.pull_limit_on, "Limit rows");
            ui.add_enabled(s.pull_limit_on, egui::DragValue::new(&mut s.pull_limit).range(1..=100_000_000).speed(100.0));
        });
        ui.add_space(6.0);
        ui.label(RichText::new("Destination in SQLite").strong());
        let exists = local_tables.iter().any(|t| t.eq_ignore_ascii_case(s.pull_target.trim()));
        ui.horizontal(|ui| {
            ui.label("Table");
            ui.add(egui::TextEdit::singleline(&mut s.pull_target).desired_width(220.0));
            if exists {
                ui.label(RichText::new("exists").small().weak());
            }
        });
        if exists {
            if s.pull_mode == PullMode::Create {
                s.pull_mode = PullMode::Append;
            }
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.pull_mode, PullMode::Append, "Append (match columns by name)");
                ui.radio_value(&mut s.pull_mode, PullMode::Replace, "Replace table");
            });
        } else {
            s.pull_mode = PullMode::Create;
            ui.label(RichText::new("A new table is created; column types are inferred from the values (exact decimals that don't fit a REAL stay TEXT).").small().weak());
        }
        ui.add_space(6.0);
        let ready = connected && !s.pull_target.trim().is_empty() && (s.pull_query || !s.pull_table.is_empty());
        if ui.add_enabled(ready && !busy, egui::Button::new(RichText::new("⬇ Pull into SQLite").strong())).clicked() {
            let limit = s.pull_limit_on.then_some(s.pull_limit);
            let sql = if s.pull_query {
                let base = s.pull_sql.trim().trim_end_matches(';').to_string();
                match (limit, s.spec.kind) {
                    (Some(n), Kind::MsSql) => format!("SELECT TOP {n} * FROM ({base}) AS q"),
                    (Some(n), _) => format!("SELECT * FROM ({base}) AS q LIMIT {n}"),
                    (None, _) => base,
                }
            } else {
                s.spec.kind.select_all(&s.pull_table, limit)
            };
            self.pipe_start(Job::Pull { sql }, Running::Pull);
        }
        if !connected {
            ui.label(RichText::new("Connect on the left first.").small().weak());
        }
    }

    fn pipe_push(&mut self, ui: &mut Ui, busy: bool) {
        let local_tables: Vec<String> = self.tables(true).into_iter().filter(|t| !t.starts_with("sqlite_")).collect();
        let s = &mut self.pipe;
        let connected = s.status.as_ref().is_some_and(|x| x.0);
        if s.push_table.is_empty() {
            if let Some(t) = local_tables.first() {
                s.push_table = t.clone();
                s.push_target = t.clone();
            }
        }
        ui.label(RichText::new("Source in SQLite").strong());
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui.radio_value(&mut s.push_query, false, "Table / view").changed();
            ui.add_enabled_ui(!s.push_query, |ui| {
                egui::ComboBox::from_id_salt("push_table").selected_text(&s.push_table).width(220.0).show_ui(ui, |ui| {
                    for t in &local_tables {
                        if ui.selectable_label(&s.push_table == t, t).clicked() {
                            s.push_table = t.clone();
                            s.push_target = t.clone();
                            changed = true;
                        }
                    }
                });
            });
            changed |= ui.radio_value(&mut s.push_query, true, "Query").changed();
        });
        if s.push_query {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                changed |= sql_editor(ui, &mut s.push_sql, 4, true).response.changed();
            });
        }
        if changed {
            s.mapping = None;
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Destination on {}", s.spec.kind.label())).strong());
        let remote_exists = s.remote_tables.iter().any(|t| t.eq_ignore_ascii_case(s.push_target.trim()));
        ui.horizontal(|ui| {
            ui.label("Table");
            ui.add(egui::TextEdit::singleline(&mut s.push_target).desired_width(220.0).hint_text(if s.spec.kind.default_schema().is_empty() { "table".to_string() } else { format!("table or {}.table", s.spec.kind.default_schema()) }));
            ui.label(RichText::new(if remote_exists { "exists on server" } else { "will be created" }).small().weak());
        });
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.push_mode, PushMode::CreateOrAppend, "Create or append");
            ui.radio_value(&mut s.push_mode, PushMode::Truncate, "Empty, then load");
            ui.radio_value(&mut s.push_mode, PushMode::Replace, "Drop & recreate");
        });
        let preview = ui.button("Preview type mapping").clicked();
        {
            if preview {
                let src = if s.push_query { s.push_sql.clone() } else { s.push_table.clone() };
                let is_q = s.push_query;
                let kind = s.spec.kind;
                let res = self.db.as_mut().map(|db| pipe::read_local(db, &src, is_q));
                let s = &mut self.pipe;
                s.mapping = match res {
                    Some(Ok((data, pk))) => Some(
                        data.columns
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                let class = pipe::infer_class(&data.rows, i);
                                let keyed = pk.iter().any(|p| p.eq_ignore_ascii_case(c));
                                let note = match (crate::csv_io::affinity_of(&data.types[i]), class) {
                                    (crate::csv_io::Affinity::Integer | crate::csv_io::Affinity::Real | crate::csv_io::Affinity::Numeric, ColClass::Text) if !data.types[i].is_empty() => " ⚠ holds text, widened".to_string(),
                                    _ => String::new(),
                                };
                                (
                                    c.clone(),
                                    if data.types[i].is_empty() { "—".into() } else { data.types[i].clone() },
                                    format!("{}{}{}", kind.type_name(class, keyed), if keyed { " PK" } else { "" }, note),
                                )
                            })
                            .collect(),
                    ),
                    Some(Err(e)) => {
                        s.result = Some((false, e));
                        None
                    }
                    None => None,
                };
            }
        }
        let s = &mut self.pipe;
        if let Some(m) = &s.mapping {
            egui::Grid::new("push_map").striped(true).spacing([18.0, 3.0]).show(ui, |ui| {
                for h in ["Column", "SQLite type", "Remote type (from data)"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (c, a, b) in m {
                    ui.label(RichText::new(c).monospace());
                    ui.label(a);
                    ui.label(RichText::new(b).monospace());
                    ui.end_row();
                }
            });
        }
        ui.add_space(6.0);
        let ready = connected && !s.push_target.trim().is_empty() && (s.push_query && !s.push_sql.trim().is_empty() || !s.push_query && !s.push_table.is_empty());
        if ui.add_enabled(ready && !busy, egui::Button::new(RichText::new("⬆ Push to server").strong())).clicked() {
            let (src, is_q, target, mode) = (if s.push_query { s.push_sql.clone() } else { s.push_table.clone() }, s.push_query, s.push_target.trim().to_string(), s.push_mode);
            let res = self.db.as_mut().map(|db| pipe::read_local(db, &src, is_q));
            match res {
                Some(Ok((data, pk))) => {
                    self.pipe.progress = (format!("Sending {} rows…", data.rows.len()), 0.0);
                    self.pipe_start(Job::Push { table: target, data, pk, mode }, Running::Push);
                }
                Some(Err(e)) => self.pipe.result = Some((false, e)),
                None => {}
            }
        }
        if !connected {
            ui.label(RichText::new("Connect on the left first.").small().weak());
        }
        ui.label(
            RichText::new("Rows are sent in batched INSERTs inside one remote transaction — if anything fails, nothing is committed.")
                .small()
                .weak(),
        );
    }
}

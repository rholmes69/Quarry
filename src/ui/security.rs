//! Windows for encryption (unlock / encrypt / rekey / decrypt), the
//! collations manager, and portable mode.

use std::path::PathBuf;

use egui::{Context, RichText, Ui};

use crate::app::App;
use crate::collate::{self, Base, CollationDef};
use crate::crypto::{Cipher, Secret};
use crate::db::Database;
use crate::ui::widgets::palette;

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

fn cipher_picker(ui: &mut Ui, id: &str, value: &mut Option<Cipher>, allow_auto: bool) {
    let text = match value {
        None => "Auto-detect".to_string(),
        Some(c) => c.label(),
    };
    egui::ComboBox::from_id_salt(id).selected_text(text).width(300.0).show_ui(ui, |ui| {
        if allow_auto {
            ui.selectable_value(value, None, "Auto-detect (tries every format)");
            ui.separator();
        }
        for c in Cipher::ALL {
            ui.selectable_value(value, Some(c), c.label());
        }
    });
}

fn secret_field(ui: &mut Ui, pw: &mut String, hex: &mut bool, show: &mut bool, hint: &str) -> egui::Response {
    let r = ui.add(
        egui::TextEdit::singleline(pw)
            .password(!*show)
            .hint_text(if *hex { "64 hex characters (raw key)" } else { hint })
            .desired_width(300.0),
    );
    ui.horizontal(|ui| {
        ui.checkbox(show, "Show");
        ui.checkbox(hex, "Raw hex key").on_hover_text("SQLCipher-style raw key: PRAGMA hexkey");
    });
    r
}

fn make_secret(pw: &str, hex: bool) -> Result<Secret, String> {
    if hex {
        let h = pw.trim();
        if h.is_empty() || h.len() % 2 != 0 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("A hex key needs an even number of hex digits (0-9, A-F).".into());
        }
        Ok(Secret::Hex(h.to_string()))
    } else {
        Ok(Secret::Password(pw.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Unlock an encrypted file
// ---------------------------------------------------------------------------

pub struct UnlockDialog {
    path: PathBuf,
    read_only: bool,
    password: String,
    hex: bool,
    show: bool,
    cipher: Option<Cipher>,
    error: Option<String>,
    focused: bool,
}

impl UnlockDialog {
    pub fn new(path: PathBuf, read_only: bool) -> Self {
        UnlockDialog { path, read_only, password: String::new(), hex: false, show: false, cipher: None, error: None, focused: false }
    }
}

// ---------------------------------------------------------------------------
// Encrypt / change password / decrypt
// ---------------------------------------------------------------------------

pub struct EncryptionDialog {
    cipher: Option<Cipher>,
    password: String,
    confirm: String,
    hex: bool,
    show: bool,
    error: Option<String>,
    done: Option<String>,
    confirm_remove: bool,
}

impl EncryptionDialog {
    pub fn new(current: Option<Cipher>) -> Self {
        EncryptionDialog {
            cipher: Some(current.unwrap_or(Cipher::ChaCha20)),
            password: String::new(),
            confirm: String::new(),
            hex: false,
            show: false,
            error: None,
            done: None,
            confirm_remove: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Collations manager
// ---------------------------------------------------------------------------

pub struct CollationsDialog {
    defs: Vec<CollationDef>,
    selected: Option<usize>,
    order_text: Vec<String>,
    sample: String,
    error: Option<String>,
    saved: bool,
}

impl CollationsDialog {
    pub fn new() -> Self {
        let defs = collate::user_defs();
        let order_text = defs.iter().map(|d| d.order.join(", ")).collect();
        CollationsDialog {
            selected: if defs.is_empty() { None } else { Some(0) },
            defs,
            order_text,
            sample: "file10, File2, file1, Émile, emile, zebra, Apple, 10 kg, 9 kg, high, Low, medium".into(),
            error: None,
            saved: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl App {
    pub fn security_windows(&mut self, ctx: &Context) {
        if let Some(mut d) = self.unlock.take() {
            if self.unlock_window(ctx, &mut d) {
                self.unlock = Some(d);
            }
        }
        if let Some(mut d) = self.encryption_dlg.take() {
            if self.encryption_window(ctx, &mut d) {
                self.encryption_dlg = Some(d);
            }
        }
        if let Some(mut d) = self.collations_dlg.take() {
            if self.collations_window(ctx, &mut d) {
                self.collations_dlg = Some(d);
            }
        }
        if self.portable_dlg {
            self.portable_window(ctx);
        }
    }

    fn unlock_window(&mut self, ctx: &Context, d: &mut UnlockDialog) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new("🔒 Encrypted Database")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -60.0])
            .show(ctx, |ui| {
                let p = palette(ui);
                ui.label(RichText::new(d.path.display().to_string()).monospace().small());
                ui.add_space(4.0);
                ui.label("Password");
                let r = secret_field(ui, &mut d.password, &mut d.hex, &mut d.show, "password");
                if !d.focused {
                    r.request_focus();
                    d.focused = true;
                }
                ui.add_space(4.0);
                ui.label("Encryption format");
                cipher_picker(ui, "unlock_cipher", &mut d.cipher, true);
                ui.label(
                    RichText::new("Reads SQLCipher 1–4, wxSQLite3 AES-128/256, sqleet ChaCha20, System.Data.SQLite RC4, Ascon, AEGIS.")
                        .small()
                        .weak(),
                );
                ui.checkbox(&mut d.read_only, "Open read-only");
                if let Some(e) = &d.error {
                    ui.colored_label(p.err, e);
                }
                ui.horizontal(|ui| {
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Open").clicked() || enter {
                        match make_secret(&d.password, d.hex)
                            .and_then(|s| Database::open_encrypted(&d.path, d.read_only, &s, d.cipher))
                        {
                            Ok(db) => {
                                d.password.clear();
                                self.install_db(db);
                                close = true;
                            }
                            Err(e) => d.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn encryption_window(&mut self, ctx: &Context, d: &mut EncryptionDialog) -> bool {
        let mut open = true;
        let mut close = false;
        let current = self.db.as_ref().and_then(|db| db.encryption);
        let pending = self.has_pending();
        egui::Window::new("🔒 Database Encryption")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                let p = palette(ui);
                match current {
                    Some(c) => ui.label(RichText::new(format!("Currently encrypted with {}", c.label())).color(p.ok)),
                    None => ui.label(RichText::new("Currently NOT encrypted — anyone with the file can read it.").color(p.warn)),
                };
                ui.separator();
                ui.label(RichText::new(if current.is_some() { "Change password or cipher" } else { "Encrypt this database" }).strong());
                cipher_picker(ui, "enc_cipher", &mut d.cipher, false);
                if let Some(Cipher::SqlCipher(v)) = d.cipher {
                    ui.label(RichText::new(format!("Files will open in SQLCipher {v}.x and in DB Browser for SQLite's SQLCipher build.")).small().weak());
                }
                if matches!(d.cipher, Some(Cipher::Aes128 | Cipher::Aes256)) {
                    ui.label(RichText::new("Compatible with wxSQLite3 and SQLite3 Multiple Ciphers.").small().weak());
                }
                if d.cipher == Some(Cipher::Rc4) {
                    ui.label(RichText::new("RC4 is cryptographically broken — use it only for legacy System.Data.SQLite apps.").small().color(p.warn));
                }
                ui.add_space(4.0);
                ui.label("New password");
                secret_field(ui, &mut d.password, &mut d.hex, &mut d.show, "new password");
                if !d.hex {
                    ui.add(egui::TextEdit::singleline(&mut d.confirm).password(!d.show).hint_text("repeat password").desired_width(300.0));
                }
                if pending {
                    ui.colored_label(p.warn, "Write or revert pending changes first.");
                }
                ui.label(
                    RichText::new("Quarry never stores passwords. If you lose this one, the data can't be recovered.")
                        .small()
                        .weak(),
                );
                if let Some(e) = &d.error {
                    ui.colored_label(p.err, e);
                }
                if let Some(m) = &d.done {
                    ui.colored_label(p.ok, m);
                }
                ui.horizontal(|ui| {
                    let label = if current.is_some() { "Change password" } else { "Encrypt" };
                    if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                        d.error = None;
                        d.done = None;
                        let res = (|| -> Result<String, String> {
                            if d.password.is_empty() {
                                return Err("Enter a password.".into());
                            }
                            if !d.hex && d.password != d.confirm {
                                return Err("The passwords don't match.".into());
                            }
                            let secret = make_secret(&d.password, d.hex)?;
                            let cipher = d.cipher.unwrap_or(Cipher::ChaCha20);
                            let db = self.db.as_mut().ok_or("No database open")?;
                            db.rekey(cipher, Some(&secret))?;
                            Ok(format!("Encrypted with {}.", cipher.short()))
                        })();
                        match res {
                            Ok(m) => {
                                d.password.clear();
                                d.confirm.clear();
                                d.done = Some(m);
                                self.pragmas.loaded = false;
                            }
                            Err(e) => d.error = Some(e),
                        }
                    }
                    if current.is_some() {
                        if !d.confirm_remove {
                            if ui.add_enabled(!pending, egui::Button::new("Remove encryption…")).clicked() {
                                d.confirm_remove = true;
                            }
                        } else if ui.add(egui::Button::new(RichText::new("Really decrypt to plaintext").color(p.err))).clicked() {
                            let res = self.db.as_mut().ok_or_else(|| "No database".to_string()).and_then(|db| db.rekey(Cipher::ChaCha20, None));
                            match res {
                                Ok(()) => d.done = Some("Encryption removed — the file is now plain SQLite.".into()),
                                Err(e) => d.error = Some(e),
                            }
                            d.confirm_remove = false;
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn collations_window(&mut self, ctx: &Context, d: &mut CollationsDialog) -> bool {
        let mut open = true;
        let mut close = false;
        egui::Window::new("Collations")
            .open(&mut open)
            .default_size([760.0, 560.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let p = palette(ui);
                let samples: Vec<String> = d.sample.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                let sorted = |f: &dyn Fn(&str, &str) -> std::cmp::Ordering| {
                    let mut v = samples.clone();
                    v.sort_by(|a, b| f(a, b));
                    v.join(" · ")
                };
                ui.horizontal(|ui| {
                    ui.label("Sample values");
                    ui.add(egui::TextEdit::singleline(&mut d.sample).desired_width(f32::INFINITY));
                });
                ui.add_space(4.0);
                let h = (ui.available_height() - 44.0).max(120.0);
                egui::ScrollArea::vertical().max_height(h).auto_shrink([false, false]).show(ui, |ui| {
                    ui.label(RichText::new("Built in — available in every database").strong());
                    egui::Grid::new("coll_builtin").striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                        for (name, desc) in collate::BUILTINS {
                            ui.label(RichText::new(name).monospace().strong());
                            ui.label(RichText::new(desc).small());
                            let f: Box<dyn Fn(&str, &str) -> std::cmp::Ordering> = match name {
                                "NATURAL" => Box::new(|a, b| collate::natural(a, b, false)),
                                "NATURAL_NOCASE" => Box::new(|a, b| collate::natural(a, b, true)),
                                "UNICODE_NOCASE" => Box::new(|a, b| Base::NoCase.compare(a, b)),
                                "UNICODE_CI_AI" => Box::new(|a, b| Base::NoCaseNoAccent.compare(a, b)),
                                _ => Box::new(collate::numeric),
                            };
                            ui.label(RichText::new(sorted(&*f)).small().weak());
                            ui.end_row();
                        }
                    });
                    ui.label(
                        RichText::new("Use them like any collation: … name TEXT COLLATE NATURAL_NOCASE, or ORDER BY name COLLATE UNICODE_CI_AI")
                            .small()
                            .weak(),
                    );

                    ui.add_space(8.0);
                    ui.label(RichText::new("Unknown collations").strong());
                    let fb = collate::fallbacks();
                    if fb.is_empty() {
                        ui.label(RichText::new("None needed so far. If a database uses a collation only its own app defines (e.g. Android's LOCALIZED), Quarry substitutes one so the data stays readable, and lists it here.").small().weak());
                    } else {
                        ui.colored_label(p.warn, format!("Stand-ins in use: {}", fb.join(", ")));
                        ui.label(RichText::new("Sorting with these is an approximation of the original app's rules. Indexes that use them may need REINDEX in that app after you edit data. Define a collation below with the same name to control the behaviour exactly.").small());
                    }
                    ui.horizontal(|ui| {
                        ui.label("Stand-in behaviour");
                        let mut fbase = collate::fallback();
                        egui::ComboBox::from_id_salt("coll_fallback").selected_text(fbase.label()).show_ui(ui, |ui| {
                            for b in Base::ALL {
                                ui.selectable_value(&mut fbase, b, b.label());
                            }
                        });
                        if fbase != collate::fallback() {
                            collate::set_fallback(fbase);
                        }
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Your collations").strong());
                        if ui.button("➕ New").clicked() {
                            let n = format!("MY_COLLATION_{}", d.defs.len() + 1);
                            d.defs.push(CollationDef::new(&n));
                            d.order_text.push(String::new());
                            d.selected = Some(d.defs.len() - 1);
                            d.saved = false;
                        }
                    });
                    ui.horizontal(|ui| {
                        for (i, def) in d.defs.iter().enumerate() {
                            if ui.selectable_label(d.selected == Some(i), &def.name).clicked() {
                                d.selected = Some(i);
                            }
                        }
                    });
                    if let Some(i) = d.selected.filter(|&i| i < d.defs.len()) {
                        let mut remove = false;
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            let def = &mut d.defs[i];
                            let before = def.clone();
                            egui::Grid::new("coll_edit").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                                ui.label("Name");
                                ui.text_edit_singleline(&mut def.name);
                                ui.end_row();
                                ui.label("Compare");
                                egui::ComboBox::from_id_salt("coll_base").selected_text(def.base.label()).show_ui(ui, |ui| {
                                    for b in Base::ALL {
                                        ui.selectable_value(&mut def.base, b, b.label());
                                    }
                                });
                                ui.end_row();
                                ui.label("Rank first");
                                ui.add(egui::TextEdit::singleline(&mut d.order_text[i]).hint_text("e.g. Low, Medium, High (optional)").desired_width(320.0));
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut def.descending, "Descending");
                                    ui.checkbox(&mut def.trim, "Ignore surrounding spaces");
                                });
                                ui.end_row();
                            });
                            def.order = d.order_text[i].split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                            if *def != before {
                                d.saved = false;
                            }
                            let def = def.clone();
                            ui.label(RichText::new(format!("Preview: {}", sorted(&|a, b| def.compare(a, b)))).small());
                            if ui.small_button("🗑 Delete").clicked() {
                                remove = true;
                            }
                        });
                        if remove {
                            d.defs.remove(i);
                            d.order_text.remove(i);
                            d.selected = None;
                            d.saved = false;
                        }
                    }
                    if let Some(e) = &d.error {
                        ui.colored_label(p.err, e);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(!d.saved, egui::Button::new("Save & apply")).clicked() {
                        let bad = d.defs.iter().find(|x| x.name.trim().is_empty() || !x.name.chars().all(|c| c.is_alphanumeric() || c == '_'));
                        if let Some(b) = bad {
                            d.error = Some(format!("'{}': collation names may only use letters, digits and _", b.name));
                        } else {
                            match collate::save_user_defs(d.defs.clone()) {
                                Ok(()) => {
                                    d.saved = true;
                                    d.error = self.db.as_ref().and_then(|db| collate::install(&db.conn).err());
                                }
                                Err(e) => d.error = Some(e),
                            }
                        }
                    }
                    if !d.saved {
                        ui.label(RichText::new("unsaved changes").small().color(p.warn));
                    }
                    if let Some(dir) = crate::config::data_dir() {
                        ui.label(RichText::new(format!("Stored in {}", dir.display())).small().weak());
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        open && !close
    }

    fn portable_window(&mut self, ctx: &Context) {
        let mut open = true;
        egui::Window::new("Portable Mode")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                let p = palette(ui);
                let portable = crate::config::is_portable();
                if portable {
                    ui.label(RichText::new("✔ Running in portable mode").strong().color(p.ok));
                } else {
                    ui.label(RichText::new("Installed mode").strong());
                }
                ui.label(format!(
                    "Settings, recent files, connection profiles and collations are stored in:\n{}",
                    crate::config::data_dir().map(|d| d.display().to_string()).unwrap_or_else(|| "(nowhere — no writable location)".into())
                ));
                if let Some(exe) = crate::config::exe_dir() {
                    ui.label(RichText::new(format!("Executable: {}", exe.display())).small().weak());
                }
                ui.add_space(6.0);
                ui.label(
                    "Quarry is a single self-contained executable: SQLite with all ciphers, the GUI, \
                     and the PostgreSQL / MySQL / SQL Server drivers are linked in. Nothing needs installing — \
                     no VC++ runtime, no OpenSSL, no ODBC/JDBC drivers. Copy it to a USB stick and run it.",
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "In portable mode nothing is written outside the QuarryData folder next to the executable, \
                         and recent files on the same drive are remembered relative to it, so drive-letter changes don't matter.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if !portable {
                        if ui.button("Enable portable mode").clicked() {
                            match crate::config::enable_portable() {
                                Ok(d) => self.notify(format!("Portable mode on — data in {}", d.display())),
                                Err(e) => self.errors.push(e),
                            }
                        }
                    } else if ui.button("Disable portable mode").clicked() {
                        match crate::config::disable_portable() {
                            Ok(()) => self.notify("Portable mode off — using the per-user settings folder."),
                            Err(e) => self.errors.push(e),
                        }
                    }
                });
            });
        if !open {
            self.portable_dlg = false;
        }
    }
}

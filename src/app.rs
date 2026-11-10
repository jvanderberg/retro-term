//! The application window: menus, dialogs, status bar and the terminal view.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use egui::{Key, KeyboardShortcut, Modifiers};
use xfer_common::Protocol;

use crate::config::{PhonebookEntry, Settings};
use crate::conn::{self, ConnSpec, FlowControl, Parity, SerialSettings};
use crate::session::{Session, SessionEvent};
use crate::term::{Emulator, Encoding, Scheme};
use crate::transfer::{human_bytes, Detected, Direction, TransferJob};
use crate::view::TermView;

/// Options that only matter for automated runs.
#[derive(Default, Clone)]
pub struct StartupOptions {
    pub connect: Option<ConnSpec>,
    /// Bytes to feed straight into the emulator at startup (e.g. an .ANS file).
    pub feed: Option<Vec<u8>>,
    /// Save a PNG of the window after startup, then exit.
    pub screenshot: Option<PathBuf>,
    pub encoding: Option<Encoding>,
    /// Override the terminal size (cols, rows) for screenshots/feeds.
    pub size: Option<(usize, usize)>,
    /// Open a dialog at startup (testing aid).
    pub ui_test: Option<String>,
    /// Save settings changes to disk.
    pub persist_settings: bool,
}

fn lock(m: &Mutex<Emulator>) -> MutexGuard<'_, Emulator> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn shortcut(key: Key) -> KeyboardShortcut {
    if cfg!(target_os = "macos") {
        KeyboardShortcut::new(Modifiers::MAC_CMD, key)
    } else {
        KeyboardShortcut::new(Modifiers::CTRL | Modifiers::SHIFT, key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnKind {
    Telnet,
    Raw,
    Serial,
    Shell,
}

struct ConnectDialog {
    kind: ConnKind,
    host: String,
    port: String,
    serial: SerialSettings,
    serial_ports: Vec<String>,
    command: String,
    encoding: Encoding,
    term_type: String,
    name: String,
    selected: Option<usize>,
}

impl ConnectDialog {
    fn new(settings: &Settings) -> ConnectDialog {
        ConnectDialog {
            kind: ConnKind::Telnet,
            host: String::new(),
            port: "23".into(),
            serial: SerialSettings::default(),
            serial_ports: conn::serial_ports(),
            command: String::new(),
            encoding: settings.encoding,
            term_type: String::new(),
            name: String::new(),
            selected: None,
        }
    }

    fn load(&mut self, e: &PhonebookEntry) {
        self.name = e.name.clone();
        self.encoding = e.encoding.unwrap_or(Encoding::Utf8);
        self.term_type = e.term_type.clone();
        match &e.spec {
            ConnSpec::Telnet { host, port } => {
                self.kind = ConnKind::Telnet;
                self.host = host.clone();
                self.port = port.to_string();
            }
            ConnSpec::Raw { host, port } => {
                self.kind = ConnKind::Raw;
                self.host = host.clone();
                self.port = port.to_string();
            }
            ConnSpec::Serial(s) => {
                self.kind = ConnKind::Serial;
                self.serial = s.clone();
            }
            ConnSpec::Shell { command } => {
                self.kind = ConnKind::Shell;
                self.command = command.clone().unwrap_or_default();
            }
        }
    }

    fn spec(&self) -> Result<ConnSpec, String> {
        let port = || self.port.trim().parse::<u16>().map_err(|_| format!("invalid port \"{}\"", self.port));
        let host = self.host.trim().to_string();
        Ok(match self.kind {
            ConnKind::Telnet | ConnKind::Raw if host.is_empty() => return Err("enter a host name".into()),
            ConnKind::Telnet => ConnSpec::Telnet { host, port: port()? },
            ConnKind::Raw => ConnSpec::Raw { host, port: port()? },
            ConnKind::Serial => ConnSpec::Serial(self.serial.clone()),
            ConnKind::Shell => {
                let c = self.command.trim();
                ConnSpec::Shell { command: (!c.is_empty()).then(|| c.to_string()) }
            }
        })
    }

    fn entry(&self) -> Result<PhonebookEntry, String> {
        let spec = self.spec()?;
        let name = if self.name.trim().is_empty() { spec.describe() } else { self.name.trim().to_string() };
        Ok(PhonebookEntry { name, spec, encoding: Some(self.encoding), term_type: self.term_type.trim().to_string() })
    }
}

/// A ZMODEM upload the remote asked for, waiting on the file picker.
struct PendingUpload;

pub struct RetroApp {
    settings: Settings,
    emu: Arc<Mutex<Emulator>>,
    session: Option<Session>,
    view: TermView,
    status: String,
    connect_dialog: Option<ConnectDialog>,
    connect_error: Option<String>,
    settings_open: bool,
    about_open: bool,
    job: Option<TransferJob>,
    job_window_open: bool,
    /// Settings changed in the Settings window but not yet saved.
    settings_dirty: bool,
    pending_upload: Option<PendingUpload>,
    last_title: String,
    startup: StartupOptions,
    started: Instant,
    frames: u64,
    screenshot_requested: bool,
    clipboard: Option<arboard::Clipboard>,
}

impl RetroApp {
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings, startup: StartupOptions) -> RetroApp {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (cols, rows) = startup.size.unwrap_or((80, 25));
        let mut emu = Emulator::new(cols, rows, settings.scheme);
        emu.encoding = startup.encoding.unwrap_or(settings.encoding);
        emu.term.scrollback_limit = settings.scrollback;
        emu.term.bbs_mode = emu.encoding == Encoding::Cp437;
        let emu = Arc::new(Mutex::new(emu));
        let mut app = RetroApp {
            settings,
            emu,
            session: None,
            view: TermView::default(),
            status: String::new(),
            connect_dialog: None,
            connect_error: None,
            settings_open: false,
            about_open: false,
            job: None,
            job_window_open: false,
            settings_dirty: false,
            pending_upload: None,
            last_title: String::new(),
            startup: startup.clone(),
            started: Instant::now(),
            frames: 0,
            screenshot_requested: false,
            clipboard: arboard::Clipboard::new().ok(),
        };
        if let Some(bytes) = &startup.feed {
            lock(&app.emu).feed(bytes);
        }
        match startup.ui_test.as_deref() {
            Some("connect") => app.connect_dialog = Some(ConnectDialog::new(&app.settings)),
            Some("settings") => app.settings_open = true,
            Some("about") => app.about_open = true,
            _ => {}
        }
        match startup.connect {
            Some(spec) => app.connect(spec, &cc.egui_ctx),
            None if startup.feed.is_none() => app.connect(ConnSpec::Shell { command: None }, &cc.egui_ctx),
            None => {}
        }
        app
    }

    fn connect(&mut self, spec: ConnSpec, ctx: &egui::Context) {
        self.connect_with(spec, None, ctx)
    }

    fn connect_with(&mut self, spec: ConnSpec, term_type: Option<String>, ctx: &egui::Context) {
        if let Some(old) = self.session.take() {
            // A transfer on the old connection can't continue on the new one.
            if let Some(job) = &self.job {
                job.cancel();
            }
            old.close();
        }
        let term_name = term_type.filter(|t| !t.is_empty()).unwrap_or_else(|| self.settings.term_type.clone());
        self.status = format!("Connecting to {}…", spec.describe());
        let session = Session::connect(spec, self.emu.clone(), term_name, ctx.clone());
        session.autodetect.store(self.settings.auto_transfer, std::sync::atomic::Ordering::Relaxed);
        self.session = Some(session);
    }

    fn dial(&mut self, entry: &PhonebookEntry, ctx: &egui::Context) {
        if let Some(enc) = entry.encoding {
            self.set_encoding(enc);
        }
        self.connect_with(entry.spec.clone(), Some(entry.term_type.clone()), ctx);
    }

    fn set_encoding(&mut self, enc: Encoding) {
        let mut e = lock(&self.emu);
        e.encoding = enc;
        e.term.bbs_mode = enc == Encoding::Cp437;
    }

    fn save_settings(&self) {
        if self.startup.persist_settings {
            self.settings.save();
        }
    }

    fn send(&self, data: Vec<u8>) {
        // User input during a transfer would corrupt the protocol stream.
        if data.is_empty() || self.transfer_busy() {
            return;
        }
        if self.settings.local_echo {
            feed_local(&self.emu, &data);
        }
        if let Some(s) = &self.session {
            s.send(data);
        }
    }

    fn paste_clipboard(&mut self) {
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else { return };
        let mut bytes = Vec::new();
        {
            let emu = lock(&self.emu);
            self.view.paste(&emu, &text, &mut bytes);
        }
        self.send(bytes);
    }

    fn copy_text(&mut self, ctx: &egui::Context, text: String) {
        if text.is_empty() {
            return;
        }
        ctx.copy_text(text);
    }

    // ----- transfers -------------------------------------------------------------

    fn transfer_busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.is_finished())
    }

    fn start_upload(&mut self, protocol: Protocol, files: Vec<PathBuf>) {
        let Some(session) = &self.session else { return };
        if files.is_empty() || self.transfer_busy() {
            return;
        }
        self.job = Some(TransferJob::start(session, protocol, Direction::Send(files)));
        self.job_window_open = true;
    }

    fn start_download(&mut self, protocol: Protocol, target: PathBuf) {
        let Some(session) = &self.session else { return };
        if self.transfer_busy() {
            return;
        }
        self.job = Some(TransferJob::start(session, protocol, Direction::Receive(target)));
        self.job_window_open = true;
    }

    fn pick_upload(&mut self, protocol: Protocol) {
        let dialog = rfd::FileDialog::new().set_title(format!("Upload with {}", protocol.name()));
        let files = if protocol.is_batch() {
            dialog.pick_files().unwrap_or_default()
        } else {
            dialog.pick_file().into_iter().collect()
        };
        self.start_upload(protocol, files);
    }

    fn pick_download(&mut self, protocol: Protocol) {
        if protocol.is_batch() {
            let dir = self.settings.download_dir.clone();
            self.start_download(protocol, dir);
        } else if let Some(path) = rfd::FileDialog::new()
            .set_title(format!("Save {} download as", protocol.name()))
            .set_directory(&self.settings.download_dir)
            .save_file()
        {
            self.start_download(protocol, path);
        }
    }

    fn poll_session(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(s) = &self.session {
            while let Ok(ev) = s.events.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                SessionEvent::Connected => {
                    if let Some(s) = &self.session {
                        self.status = format!("Connected: {}", s.spec.describe());
                        let e = lock(&self.emu);
                        let (c, r) = (e.term.cols() as u16, e.term.rows() as u16);
                        drop(e);
                        let cell = self.view.cell_size() * ctx.pixels_per_point();
                        s.resize(c, r, (f32::from(c) * cell.x) as u16, (f32::from(r) * cell.y) as u16);
                    }
                }
                SessionEvent::Disconnected(reason) => {
                    let desc = self.session.as_ref().map(|s| s.spec.describe()).unwrap_or_default();
                    self.status = format!("Disconnected from {desc}: {reason}");
                    if let Some(job) = &self.job {
                        job.cancel();
                    }
                    let msg = format!("\r\n\x1b[0;33m[{}]\x1b[0m\r\n", self.status);
                    lock(&self.emu).feed(msg.as_bytes());
                }
                SessionEvent::AutoDetect(what) => {
                    if self.transfer_busy() {
                        if let Some(s) = &self.session {
                            s.release_hold();
                        }
                        continue;
                    }
                    match what {
                        Detected::ZmodemDownload => {
                            let dir = self.settings.download_dir.clone();
                            self.start_download(Protocol::Zmodem, dir);
                        }
                        Detected::KermitDownload => {
                            let dir = self.settings.download_dir.clone();
                            self.start_download(Protocol::Kermit, dir);
                        }
                        Detected::ZmodemUpload => self.pending_upload = Some(PendingUpload),
                    }
                }
            }
        }

        if self.pending_upload.take().is_some() {
            let files = rfd::FileDialog::new()
                .set_title("Remote is waiting for a ZMODEM upload")
                .pick_files()
                .unwrap_or_default();
            if files.is_empty() {
                if let Some(s) = &self.session {
                    // Abort the remote receiver: 8 CANs then 8 backspaces.
                    let mut abort = vec![0x18; 8];
                    abort.extend([0x08; 8]);
                    s.send(abort);
                    s.release_hold();
                }
            } else {
                self.start_upload(Protocol::Zmodem, files);
            }
        }

        if let Some(job) = &mut self.job {
            // The transfer thread restores terminal routing itself.
            if job.poll() {
                ctx.request_repaint();
            }
            if !job.is_finished() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    // ----- UI ----------------------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_shortcut(&shortcut(k)));
        if pressed(Key::K) {
            self.connect_dialog = Some(ConnectDialog::new(&self.settings));
        }
        if pressed(Key::T) {
            self.connect(ConnSpec::Shell { command: None }, ctx);
        }
        if pressed(Key::Equals) || pressed(Key::Plus) {
            self.settings.font_size = (self.settings.font_size + 1.0).min(48.0);
        }
        if pressed(Key::Minus) {
            self.settings.font_size = (self.settings.font_size - 1.0).max(6.0);
        }
        if pressed(Key::Num0) {
            self.settings.font_size = Settings::default().font_size;
        }
        if pressed(Key::U) {
            self.pick_upload(Protocol::Zmodem);
        }
        if pressed(Key::D) {
            self.pick_download(Protocol::Zmodem);
        }
        if !cfg!(target_os = "macos") && pressed(Key::V) {
            self.paste_clipboard();
        }
        if cfg!(target_os = "macos") && pressed(Key::A) {
            let e = lock(&self.emu);
            self.view.select_all(&e.term);
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mods = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+Shift+" };
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Session", |ui| {
                if ui.button(format!("Connect…  ({mods}K)")).clicked() {
                    self.connect_dialog = Some(ConnectDialog::new(&self.settings));
                }
                if ui.button(format!("New local shell  ({mods}T)")).clicked() {
                    self.connect(ConnSpec::Shell { command: None }, &ctx);
                }
                let entries = self.settings.phonebook.clone();
                if !entries.is_empty() {
                    ui.menu_button("Phonebook", |ui| {
                        for e in &entries {
                            if ui.button(&e.name).clicked() {
                                self.dial(e, &ctx);
                            }
                        }
                    });
                }
                ui.separator();
                let connected = self.session.as_ref().is_some_and(|s| s.is_connected());
                if ui.add_enabled(self.session.is_some(), egui::Button::new("Reconnect")).clicked() {
                    if let Some(spec) = self.session.as_ref().map(|s| s.spec.clone()) {
                        self.connect(spec, &ctx);
                    }
                }
                if ui.add_enabled(connected, egui::Button::new("Disconnect")).clicked() {
                    if let Some(s) = &self.session {
                        s.close();
                    }
                }
                if ui.add_enabled(connected, egui::Button::new("Send break")).clicked() {
                    if let Some(s) = &self.session {
                        s.send_break();
                    }
                }
                if matches!(self.session.as_ref().map(|s| &s.spec), Some(ConnSpec::Serial(_))) {
                    ui.horizontal(|ui| {
                        ui.label("DTR");
                        if ui.button("On").clicked() {
                            if let Some(s) = &self.session {
                                s.set_dtr(true);
                            }
                        }
                        if ui.button("Off").clicked() {
                            if let Some(s) = &self.session {
                                s.set_dtr(false);
                            }
                        }
                    });
                }
                ui.separator();
                let capturing = self.session.as_ref().is_some_and(|s| s.capturing());
                if capturing {
                    if ui.button("Stop capture").clicked() {
                        if let Some(s) = &self.session {
                            s.set_capture(None);
                        }
                    }
                } else if ui.add_enabled(self.session.is_some(), egui::Button::new("Capture to file…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name("capture.log").save_file() {
                        match std::fs::File::create(&path) {
                            Ok(f) => {
                                if let Some(s) = &self.session {
                                    s.set_capture(Some(f));
                                }
                            }
                            Err(e) => self.status = format!("Could not create {}: {e}", path.display()),
                        }
                    }
                }
                if ui.button("View ANSI file…").clicked() {
                    if let Some(path) =
                        rfd::FileDialog::new().add_filter("ANSI art", &["ans", "asc", "txt", "diz", "nfo"]).pick_file()
                    {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                // SAUCE records and the EOF marker aren't part of the art.
                                let end = bytes.iter().position(|&b| b == 0x1A).unwrap_or(bytes.len());
                                feed_local(&self.emu, b"\x1b[0m\x1b[2J\x1b[H");
                                feed_local(&self.emu, &bytes[..end]);
                                feed_local(&self.emu, b"\x1b[0m");
                            }
                            Err(e) => self.status = format!("Could not read {}: {e}", path.display()),
                        }
                    }
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("Edit", |ui| {
                if ui.button(format!("Copy  ({mods}C)")).clicked() {
                    let text = self.view.selection_text(&lock(&self.emu).term);
                    if let Some(t) = text {
                        self.copy_text(&ctx, t);
                    }
                }
                if ui.button(format!("Paste  ({mods}V)")).clicked() {
                    self.paste_clipboard();
                }
                if ui.button("Select all").clicked() {
                    let e = lock(&self.emu);
                    self.view.select_all(&e.term);
                }
                ui.separator();
                if ui.button("Clear scrollback").clicked() {
                    lock(&self.emu).term.clear_scrollback();
                    self.view.scroll = 0;
                }
                if ui.button("Reset terminal").clicked() {
                    lock(&self.emu).term.reset();
                }
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.settings_open = true;
                }
            });

            ui.menu_button("Transfer", |ui| {
                let can = self.session.as_ref().is_some_and(|s| s.is_connected()) && !self.transfer_busy();
                ui.add_enabled_ui(can, |ui| {
                    ui.menu_button("Upload (send)", |ui| {
                        for p in Protocol::ALL {
                            if ui.button(p.name()).clicked() {
                                self.pick_upload(p);
                            }
                        }
                    });
                    ui.menu_button("Download (receive)", |ui| {
                        for p in Protocol::ALL {
                            if ui.button(p.name()).clicked() {
                                self.pick_download(p);
                            }
                        }
                    });
                });
                ui.separator();
                if ui.add_enabled(self.job.is_some(), egui::Button::new("Show transfer window")).clicked() {
                    self.job_window_open = true;
                }
                if ui.button("Download folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&self.settings.download_dir).pick_folder() {
                        self.settings.download_dir = dir;
                        self.save_settings();
                    }
                }
                ui.label(format!("Saving to {}", self.settings.download_dir.display()));
                let mut auto = self.settings.auto_transfer;
                if ui.checkbox(&mut auto, "Auto-start ZMODEM/Kermit").changed() {
                    self.settings.auto_transfer = auto;
                    if let Some(s) = &self.session {
                        s.autodetect.store(auto, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.save_settings();
                }
            });

            ui.menu_button("View", |ui| {
                ui.menu_button("Encoding", |ui| {
                    let current = lock(&self.emu).encoding;
                    for enc in Encoding::ALL {
                        if ui.radio(current == enc, enc.name()).clicked() {
                            self.set_encoding(enc);
                            self.settings.encoding = enc;
                            self.save_settings();
                        }
                    }
                });
                ui.menu_button("Color scheme", |ui| {
                    for s in Scheme::ALL {
                        if ui.radio(self.settings.scheme == s, s.name()).clicked() {
                            self.settings.scheme = s;
                            lock(&self.emu).term.set_scheme(s);
                            self.save_settings();
                        }
                    }
                });
                ui.separator();
                if ui.button(format!("Bigger text  ({mods}+)")).clicked() {
                    self.settings.font_size = (self.settings.font_size + 1.0).min(48.0);
                }
                if ui.button(format!("Smaller text  ({mods}-)")).clicked() {
                    self.settings.font_size = (self.settings.font_size - 1.0).max(6.0);
                }
                ui.separator();
                for (cols, rows) in [(80usize, 25usize), (80, 24), (132, 43)] {
                    if ui.button(format!("Fit window to {cols}×{rows}")).clicked() {
                        let cell = self.view.cell_size();
                        let size = egui::vec2(cols as f32 * cell.x, rows as f32 * cell.y + 52.0);
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                    }
                }
            });

            ui.menu_button("Help", |ui| {
                if ui.button("About retro-term").clicked() {
                    self.about_open = true;
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let e = lock(&self.emu);
                ui.label(format!("{}×{}", e.term.cols(), e.term.rows()));
                ui.separator();
                ui.label(e.encoding.name());
                drop(e);
                if self.session.as_ref().is_some_and(|s| s.capturing()) {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(0xFF, 0x55, 0x55), "● REC");
                }
                if let Some(job) = &self.job {
                    if !job.is_finished() {
                        ui.separator();
                        let pct = job.file_size.filter(|&s| s > 0).map(|s| job.bytes as f32 / s as f32);
                        let label = format!("{} {}", job.protocol.name(), human_bytes(job.bytes as f64));
                        match pct {
                            Some(p) => {
                                ui.add(egui::ProgressBar::new(p).desired_width(120.0).text(label));
                            }
                            None => {
                                ui.label(label);
                            }
                        }
                    }
                }
                ui.separator();
                // The status message takes whatever room is left.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let connected = self.session.as_ref().is_some_and(|s| s.is_connected());
                    let color = if connected {
                        egui::Color32::from_rgb(0x55, 0xFF, 0x55)
                    } else {
                        egui::Color32::from_rgb(0xAA, 0x55, 0x55)
                    };
                    ui.colored_label(color, "●");
                    ui.add(egui::Label::new(&self.status).truncate());
                });
            });
        });
    }

    fn transfer_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        if !self.job_window_open {
            return;
        }
        let mut open = true;
        let mut close = false;
        let title = format!("{} {}", job.protocol.name(), if job.sending { "upload" } else { "download" });
        egui::Window::new(title)
            .id(egui::Id::unique("retro-term-transfer-window"))
            .open(&mut open)
            .resizable(true)
            .default_width(420.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.label(if job.current_file.is_empty() {
                    "Waiting for the remote side…".to_string()
                } else {
                    format!("File: {}", job.current_file)
                });
                let pct = job.file_size.filter(|&s| s > 0).map(|s| (job.bytes as f32 / s as f32).min(1.0));
                let bytes_text = match job.file_size {
                    Some(s) => format!("{} / {}", human_bytes(job.bytes as f64), human_bytes(s as f64)),
                    None => human_bytes(job.bytes as f64),
                };
                ui.add(
                    egui::ProgressBar::new(pct.unwrap_or(0.0))
                        .text(bytes_text)
                        .animate(pct.is_none() && !job.is_finished()),
                );
                let rate = job.rate();
                let mut line =
                    format!("{}/s · {} file(s) done · {} retries", human_bytes(rate), job.files_done, job.errors);
                if let (Some(size), false) = (job.file_size, job.is_finished()) {
                    if rate > 1.0 && size > job.bytes {
                        line.push_str(&format!(" · ETA {:.0}s", (size - job.bytes) as f64 / rate));
                    }
                }
                ui.label(line);
                match &job.result {
                    Some(Ok(sum)) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(0x55, 0xFF, 0x55),
                            format!("Completed: {} file(s)", sum.files.len()),
                        );
                    }
                    Some(Err(e)) => {
                        ui.colored_label(egui::Color32::from_rgb(0xFF, 0x55, 0x55), format!("Failed: {e}"));
                    }
                    None => {}
                }
                egui::CollapsingHeader::new("Log").default_open(false).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).show(ui, |ui| {
                        for l in &job.log {
                            ui.label(l);
                        }
                    });
                });
                ui.horizontal(|ui| {
                    if job.is_finished() {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                        if !job.sending && ui.button("Open download folder").clicked() {
                            let _ = open_path(&self.settings.download_dir);
                        }
                    } else if ui.button("Cancel").clicked() {
                        job.cancel();
                    }
                });
            });
        if !open || close {
            self.job_window_open = false;
            if self.job.as_ref().is_some_and(|j| j.is_finished()) {
                self.job = None;
            }
        }
    }

    fn connect_window(&mut self, ctx: &egui::Context) {
        let Some(dlg) = &mut self.connect_dialog else { return };
        let mut open = true;
        let mut action: Option<PhonebookEntry> = None;
        let mut save = false;
        let mut delete: Option<usize> = None;
        let mut close = false;
        egui::Window::new("Connect").open(&mut open).resizable(true).default_width(560.0).show(ctx, |ui| {
            ui.columns(2, |cols| {
                let ui = &mut cols[0];
                ui.heading("Phonebook");
                egui::ScrollArea::vertical().id_salt("phonebook").max_height(260.0).show(ui, |ui| {
                    for (i, e) in self.settings.phonebook.iter().enumerate() {
                        let selected = dlg.selected == Some(i);
                        let resp = ui.selectable_label(selected, &e.name).on_hover_text(e.spec.describe());
                        if resp.clicked() {
                            dlg.selected = Some(i);
                            dlg.load(e);
                        }
                        if resp.double_clicked() {
                            action = Some(e.clone());
                        }
                    }
                });
                if let Some(i) = dlg.selected {
                    if ui.button("Delete entry").clicked() {
                        delete = Some(i);
                    }
                }

                let ui = &mut cols[1];
                ui.heading("Connection");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut dlg.kind, ConnKind::Telnet, "Telnet");
                    ui.selectable_value(&mut dlg.kind, ConnKind::Raw, "TCP");
                    ui.selectable_value(&mut dlg.kind, ConnKind::Serial, "Serial");
                    ui.selectable_value(&mut dlg.kind, ConnKind::Shell, "Shell");
                });
                egui::Grid::new("conn-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    match dlg.kind {
                        ConnKind::Telnet | ConnKind::Raw => {
                            ui.label("Host");
                            ui.text_edit_singleline(&mut dlg.host);
                            ui.end_row();
                            ui.label("Port");
                            ui.text_edit_singleline(&mut dlg.port);
                            ui.end_row();
                        }
                        ConnKind::Serial => {
                            ui.label("Port");
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("serial-port")
                                    .selected_text(if dlg.serial.port.is_empty() {
                                        "(choose)"
                                    } else {
                                        dlg.serial.port.as_str()
                                    })
                                    .show_ui(ui, |ui| {
                                        for p in &dlg.serial_ports {
                                            ui.selectable_value(&mut dlg.serial.port, p.clone(), p);
                                        }
                                    });
                                if ui.small_button("⟳").on_hover_text("Rescan ports").clicked() {
                                    dlg.serial_ports = conn::serial_ports();
                                }
                            });
                            ui.end_row();
                            ui.label("Device path");
                            ui.text_edit_singleline(&mut dlg.serial.port);
                            ui.end_row();
                            ui.label("Baud");
                            egui::ComboBox::from_id_salt("baud").selected_text(dlg.serial.baud.to_string()).show_ui(
                                ui,
                                |ui| {
                                    for b in [
                                        300u32, 1200, 2400, 4800, 9600, 19_200, 38_400, 57_600, 115_200, 230_400,
                                        460_800, 921_600, 1_000_000, 2_000_000,
                                    ] {
                                        ui.selectable_value(&mut dlg.serial.baud, b, b.to_string());
                                    }
                                },
                            );
                            ui.end_row();
                            ui.label("Data bits");
                            ui.horizontal(|ui| {
                                for b in [5u8, 6, 7, 8] {
                                    ui.selectable_value(&mut dlg.serial.data_bits, b, b.to_string());
                                }
                            });
                            ui.end_row();
                            ui.label("Parity");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut dlg.serial.parity, Parity::None, "None");
                                ui.selectable_value(&mut dlg.serial.parity, Parity::Odd, "Odd");
                                ui.selectable_value(&mut dlg.serial.parity, Parity::Even, "Even");
                            });
                            ui.end_row();
                            ui.label("Stop bits");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut dlg.serial.stop_bits, 1, "1");
                                ui.selectable_value(&mut dlg.serial.stop_bits, 2, "2");
                            });
                            ui.end_row();
                            ui.label("Flow control");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut dlg.serial.flow, FlowControl::None, "None");
                                ui.selectable_value(&mut dlg.serial.flow, FlowControl::Hardware, "RTS/CTS");
                                ui.selectable_value(&mut dlg.serial.flow, FlowControl::Software, "XON/XOFF");
                            });
                            ui.end_row();
                        }
                        ConnKind::Shell => {
                            ui.label("Command");
                            ui.add(egui::TextEdit::singleline(&mut dlg.command).hint_text("default login shell"));
                            ui.end_row();
                        }
                    }
                    ui.label("Encoding");
                    egui::ComboBox::from_id_salt("enc").selected_text(dlg.encoding.name()).show_ui(ui, |ui| {
                        for enc in Encoding::ALL {
                            ui.selectable_value(&mut dlg.encoding, enc, enc.name());
                        }
                    });
                    ui.end_row();
                    ui.label("Terminal type");
                    ui.add(egui::TextEdit::singleline(&mut dlg.term_type).hint_text("default (from settings)"));
                    ui.end_row();
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut dlg.name).hint_text("for the phonebook"));
                    ui.end_row();
                });
                if let Some(err) = &self.connect_error {
                    ui.colored_label(egui::Color32::from_rgb(0xFF, 0x55, 0x55), err);
                }
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        match dlg.entry() {
                            Ok(e) => action = Some(e),
                            Err(e) => self.connect_error = Some(e),
                        }
                    }
                    if ui.button("Save to phonebook").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        });
        if save {
            match dlg.entry() {
                Ok(e) => {
                    match self.settings.phonebook.iter_mut().find(|x| x.name == e.name) {
                        Some(existing) => *existing = e,
                        None => self.settings.phonebook.push(e),
                    }
                    self.save_settings();
                    self.connect_error = None;
                }
                Err(e) => self.connect_error = Some(e),
            }
        }
        if let Some(i) = delete {
            if i < self.settings.phonebook.len() {
                self.settings.phonebook.remove(i);
                self.save_settings();
            }
            if let Some(d) = &mut self.connect_dialog {
                d.selected = None;
            }
        }
        if let Some(e) = action {
            self.connect_error = None;
            self.connect_dialog = None;
            self.dial(&e, ctx);
            return;
        }
        if !open || close {
            self.connect_dialog = None;
            self.connect_error = None;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let before = self.settings.clone();
        egui::Window::new("Settings").open(&mut open).resizable(false).show(ctx, |ui| {
            let s = &mut self.settings;
            egui::Grid::new("settings-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Font size");
                ui.add(egui::Slider::new(&mut s.font_size, 6.0..=48.0).step_by(0.5));
                ui.end_row();
                ui.label("Scrollback lines");
                ui.add(egui::DragValue::new(&mut s.scrollback).range(0..=1_000_000).speed(100));
                ui.end_row();
                ui.label("Terminal type");
                ui.text_edit_singleline(&mut s.term_type);
                ui.end_row();
                ui.label("Download folder");
                ui.horizontal(|ui| {
                    ui.label(s.download_dir.display().to_string());
                    if ui.small_button("Change…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().set_directory(&s.download_dir).pick_folder() {
                            s.download_dir = dir;
                        }
                    }
                });
                ui.end_row();
            });
            ui.separator();
            ui.checkbox(&mut s.backspace_sends_del, "Backspace sends DEL (0x7F); off sends ^H");
            ui.checkbox(&mut s.bold_is_bright, "Bold text uses bright colors");
            ui.checkbox(&mut s.ice_colors, "iCE colors (blink = bright background)");
            ui.checkbox(&mut s.builtin_box_drawing, "Draw box/block characters geometrically");
            ui.checkbox(&mut s.local_echo, "Local echo");
            ui.checkbox(&mut s.option_as_meta, "Option/Alt key sends Meta (ESC prefix)");
            ui.checkbox(&mut s.auto_transfer, "Auto-start ZMODEM/Kermit downloads");
            ui.checkbox(&mut s.allow_osc52, "Let programs set the clipboard (OSC 52)");
            if let Some(p) = Settings::path() {
                ui.small(format!("Saved in {}", p.display()));
            }
        });
        if self.settings != before {
            let mut e = lock(&self.emu);
            e.term.scrollback_limit = self.settings.scrollback;
            drop(e);
            if let Some(s) = &self.session {
                s.autodetect.store(self.settings.auto_transfer, std::sync::atomic::Ordering::Relaxed);
            }
            self.settings_dirty = true;
        }
        // Save once a drag or edit has settled, not on every frame of it.
        if self.settings_dirty && (!open || !ctx.input(|i| i.pointer.any_down())) {
            self.settings_dirty = false;
            self.save_settings();
        }
        if !open {
            self.settings_open = false;
        }
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.about_open;
        egui::Window::new("About retro-term").open(&mut open).resizable(false).collapsible(false).show(ctx, |ui| {
            ui.heading(format!("retro-term {}", env!("CARGO_PKG_VERSION")));
            ui.label("A fast terminal emulator with full ANSI/VT/xterm emulation,");
            ui.label("telnet, serial and local shell connections, and");
            ui.label("XMODEM, YMODEM, ZMODEM and Kermit file transfers.");
        });
        self.about_open = open;
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let t = lock(&self.emu).term.title.clone();
        let desc = self.session.as_ref().map(|s| s.spec.describe()).unwrap_or_default();
        let title = if t.is_empty() { format!("retro-term — {desc}") } else { format!("{t} — retro-term") };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.startup.screenshot.clone() else { return };
        self.frames += 1;
        // Give the window a few frames (and any connection a moment) to settle.
        let ready = self.frames > 5 && self.started.elapsed() > Duration::from_millis(1500);
        if !ready {
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        if !self.screenshot_requested {
            self.screenshot_requested = true;
            let ctx2 = ctx.clone();
            ctx.request_screenshot(move |image| {
                if let Err(e) = save_png(&path, &image) {
                    eprintln!("screenshot failed: {e}");
                } else {
                    eprintln!("saved screenshot to {}", path.display());
                }
                ctx2.send_viewport_cmd(egui::ViewportCommand::Close);
            });
            ctx.request_repaint();
        }
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), image.size[0] as u32, image.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    let data: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
    w.write_image_data(&data)?;
    Ok(())
}

fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd).arg(path).spawn().map(|_| ())
}

impl eframe::App for RetroApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_session(&ctx);
        self.handle_shortcuts(&ctx);
        self.update_title(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let dialogs_open = self.connect_dialog.is_some() || self.settings_open;
        let bg = {
            let e = lock(&self.emu);
            let c = if e.term.modes.reverse_video { e.term.palette.fg } else { e.term.palette.bg };
            egui::Color32::from_rgb(c.0, c.1, c.2)
        };
        let out = egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(bg).inner_margin(egui::Margin::same(4)))
            .show(ui, |ui| {
                let mut emu = lock(&self.emu);
                self.view.show(ui, &mut emu, &self.settings, !dialogs_open)
            })
            .inner;

        if let Some((c, r, w, h)) = out.resized {
            if let Some(s) = &self.session {
                s.resize(c, r, w, h);
            }
        }
        if let Some(text) = out.copy {
            self.copy_text(&ctx, text);
        }
        if !out.send.is_empty() {
            if self.transfer_busy() {
                // Keystrokes during a transfer would corrupt it; Escape cancels.
                if out.send == [0x1b] {
                    if let Some(j) = &self.job {
                        j.cancel();
                    }
                }
            } else {
                self.send(out.send);
            }
        }
        // OSC 52 clipboard writes from the host, if allowed.
        let clip = lock(&self.emu).term.clipboard_request.take();
        if let Some(text) = clip.filter(|_| self.settings.allow_osc52) {
            self.copy_text(&ctx, text);
        }

        self.connect_window(&ctx);
        self.settings_window(&ctx);
        self.transfer_window(&ctx);
        self.about_window(&ctx);
        self.handle_screenshot(&ctx);
    }
}

/// Feed bytes that didn't come from the remote (local echo, a viewed ANSI
/// file). Any replies they provoke are discarded rather than sent to the host.
fn feed_local(emu: &Mutex<Emulator>, bytes: &[u8]) {
    let mut e = lock(emu);
    e.feed(bytes);
    e.term.responses.clear();
}

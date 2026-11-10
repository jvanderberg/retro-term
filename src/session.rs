//! A live connection: reader/writer threads, the emulator they drive, and the
//! routing of incoming bytes to either the terminal or a file transfer.

use std::fs::File;
use std::io::{ErrorKind, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use xfer_common::ChannelPort;

use crate::conn::{self, telnet::Telnet, ConnSpec};
use crate::term::Emulator;
use crate::transfer::{Detected, Detectors};

/// Messages for the writer thread.
enum Out {
    /// User/transfer data (escaped for telnet as needed).
    Data(Vec<u8>),
    /// Protocol bytes written verbatim (telnet negotiation replies).
    Raw(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
        px_w: u16,
        px_h: u16,
    },
    Break,
    SetDtr(bool),
    Close,
}

/// Where incoming bytes go.
enum Route {
    Terminal,
    /// Buffer bytes while the UI decides what to do (e.g. pick files for a
    /// ZMODEM upload the remote asked for).
    Hold(Vec<u8>),
    Transfer(Sender<Vec<u8>>),
}

/// Incoming-byte routing state, shared by the reader thread, the UI and
/// running transfers.
struct Router {
    route: Route,
    detectors: Detectors,
}

/// Most bytes kept while holding for the UI after an auto-detect.
const HOLD_LIMIT: usize = 4 << 20;

/// Everything needed to deliver incoming bytes.
#[derive(Clone)]
struct Shared {
    router: Arc<Mutex<Router>>,
    emu: Arc<Mutex<Emulator>>,
    out: Sender<Out>,
    events: Sender<SessionEvent>,
    autodetect: Arc<AtomicBool>,
    repaint: egui::Context,
}

impl Shared {
    fn deliver(&self, chunk: &[u8]) {
        let mut router = lock(&self.router);
        self.deliver_locked(&mut router, chunk);
    }

    fn deliver_locked(&self, router: &mut Router, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        match &mut router.route {
            Route::Terminal => self.to_terminal(router, chunk),
            Route::Hold(held) => {
                let room = HOLD_LIMIT.saturating_sub(held.len());
                held.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Route::Transfer(tx) => {
                if tx.send(chunk.to_vec()).is_err() {
                    // The transfer ended without handing the link back
                    // (it panicked); don't lose the bytes.
                    router.route = Route::Terminal;
                    self.to_terminal(router, chunk);
                }
            }
        }
    }

    fn to_terminal(&self, router: &mut Router, chunk: &[u8]) {
        let detect = if self.autodetect.load(Ordering::Relaxed) { router.detectors.scan(chunk) } else { None };
        let (to_term, rest) = match detect {
            Some((end, _)) => chunk.split_at(end),
            None => (chunk, &[][..]),
        };
        let responses = {
            let mut e = lock(&self.emu);
            e.feed(to_term);
            if detect.is_some() {
                // Erase the protocol start sequence ("**^XB00…" or a
                // Kermit packet) that was echoed onto the current line.
                e.feed(b"\r\x1b[2K");
            }
            std::mem::take(&mut e.term.responses)
        };
        if !responses.is_empty() {
            let _ = self.out.send(Out::Data(responses));
        }
        if let Some((_, what)) = detect {
            router.route = Route::Hold(rest.to_vec());
            router.detectors = Detectors::new();
            let _ = self.events.send(SessionEvent::AutoDetect(what));
        }
        self.repaint.request_repaint();
    }
}

/// The byte channels a file transfer talks through, plus the way back to
/// the terminal once it is done.
pub struct TransferLink {
    pub port: ChannelPort,
    pub ret: ReturnPath,
}

/// Restores terminal routing when a transfer ends. Dropping it without
/// calling [`ReturnPath::finish`] (e.g. on panic) still restores routing.
pub struct ReturnPath {
    shared: Shared,
}

impl ReturnPath {
    /// Route incoming bytes to the terminal again and show what the remote
    /// sent after the protocol finished (typically the shell prompt). Done
    /// under the routing lock so nothing arriving meanwhile is reordered.
    pub fn finish(self, port: ChannelPort) {
        let mut router = lock(&self.shared.router);
        let unread = port.into_unread();
        if matches!(router.route, Route::Transfer(_)) {
            router.route = Route::Terminal;
        }
        self.shared.deliver_locked(&mut router, &unread);
    }
}

impl Drop for ReturnPath {
    fn drop(&mut self) {
        let mut router = lock(&self.shared.router);
        if matches!(router.route, Route::Transfer(_)) {
            router.route = Route::Terminal;
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Connected,
    Disconnected(String),
    /// The remote side started a file transfer protocol.
    AutoDetect(Detected),
}

pub struct Session {
    pub spec: ConnSpec,
    out: Sender<Out>,
    shared: Shared,
    pub events: Receiver<SessionEvent>,
    connected: Arc<AtomicBool>,
    /// Set by [`Session::close`]; the reader stops even if the connection
    /// itself can't be interrupted (serial ports just time out).
    closing: Arc<AtomicBool>,
    capture: Arc<Mutex<Option<File>>>,
    pub autodetect: Arc<AtomicBool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Session {
    /// Start connecting in the background. The returned session is usable
    /// immediately; output is queued until the connection is up.
    pub fn connect(spec: ConnSpec, emu: Arc<Mutex<Emulator>>, term_name: String, repaint: egui::Context) -> Session {
        let (out_tx, out_rx) = mpsc::channel::<Out>();
        let (ev_tx, ev_rx) = mpsc::channel::<SessionEvent>();
        let connected = Arc::new(AtomicBool::new(false));
        let closing = Arc::new(AtomicBool::new(false));
        let capture: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
        let autodetect = Arc::new(AtomicBool::new(true));
        let shared = Shared {
            router: Arc::new(Mutex::new(Router { route: Route::Terminal, detectors: Detectors::new() })),
            emu: emu.clone(),
            out: out_tx.clone(),
            events: ev_tx.clone(),
            autodetect: autodetect.clone(),
            repaint: repaint.clone(),
        };

        let (cols, rows) = {
            let e = lock(&emu);
            (e.term.cols() as u16, e.term.rows() as u16)
        };

        let session = Session {
            spec: spec.clone(),
            out: out_tx.clone(),
            shared: shared.clone(),
            events: ev_rx,
            connected: connected.clone(),
            closing: closing.clone(),
            capture: capture.clone(),
            autodetect,
        };

        std::thread::Builder::new()
            .name("connect".into())
            .spawn(move || {
                let opened = match conn::open(&spec, cols, rows, &term_name) {
                    Ok(o) => o,
                    Err(e) => {
                        let _ = ev_tx.send(SessionEvent::Disconnected(format!("{e:#}")));
                        repaint.request_repaint();
                        return;
                    }
                };
                // Queue telnet negotiation before the UI hears we're up, so
                // our WILL NAWS precedes any size report.
                let telnet = opened.telnet.then(|| Arc::new(Mutex::new(Telnet::new(&term_name, cols, rows))));
                if let Some(t) = &telnet {
                    let init = lock(t).initial();
                    let _ = out_tx.send(Out::Raw(init));
                }
                connected.store(true, Ordering::SeqCst);
                let _ = ev_tx.send(SessionEvent::Connected);
                repaint.request_repaint();

                // Writer thread.
                let mut writer = opened.writer;
                let mut control = opened.control;
                let wtelnet = telnet.clone();
                std::thread::Builder::new()
                    .name("conn-writer".into())
                    .spawn(move || {
                        let mut buf = Vec::new();
                        for msg in out_rx {
                            let res = match msg {
                                Out::Data(d) => match &wtelnet {
                                    Some(t) => {
                                        buf.clear();
                                        lock(t).escape(&d, &mut buf);
                                        writer.write_all(&buf)
                                    }
                                    None => writer.write_all(&d),
                                },
                                Out::Raw(d) => writer.write_all(&d),
                                Out::Resize { cols, rows, px_w, px_h } => {
                                    control.resize(cols, rows, px_w, px_h);
                                    match &wtelnet {
                                        Some(t) => {
                                            // Build under the lock, write without it: the
                                            // reader needs the lock to parse input.
                                            let naws = {
                                                let mut t = lock(t);
                                                t.cols = cols;
                                                t.rows = rows;
                                                t.naws()
                                            };
                                            writer.write_all(&naws)
                                        }
                                        None => Ok(()),
                                    }
                                }
                                Out::Break => {
                                    if wtelnet.is_some() {
                                        // IAC BRK
                                        writer.write_all(&[conn::telnet::IAC, 243])
                                    } else {
                                        control.send_break();
                                        Ok(())
                                    }
                                }
                                Out::SetDtr(on) => {
                                    control.set_dtr(on);
                                    Ok(())
                                }
                                Out::Close => {
                                    control.close();
                                    break;
                                }
                            };
                            if res.and_then(|_| writer.flush()).is_err() {
                                break;
                            }
                        }
                        control.close();
                    })
                    .expect("spawn writer");

                // Reader loop (this thread).
                let mut reader = opened.reader;
                let mut buf = vec![0u8; 64 * 1024];
                let mut data = Vec::with_capacity(64 * 1024);
                let mut reply = Vec::new();
                let reason = loop {
                    if closing.load(Ordering::SeqCst) {
                        break "closed".to_string();
                    }
                    let n = match reader.read(&mut buf) {
                        Ok(0) => break "connection closed".to_string(),
                        Ok(n) => n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) if opened.timeouts_ok && e.kind() == ErrorKind::TimedOut => continue,
                        Err(e) => break format!("{e}"),
                    };
                    let chunk: &[u8] = match &telnet {
                        Some(t) => {
                            data.clear();
                            reply.clear();
                            lock(t).receive(&buf[..n], &mut data, &mut reply);
                            if !reply.is_empty() {
                                let _ = out_tx.send(Out::Raw(reply.clone()));
                            }
                            &data
                        }
                        None => &buf[..n],
                    };
                    if chunk.is_empty() {
                        continue;
                    }
                    if let Some(f) = lock(&capture).as_mut() {
                        let _ = f.write_all(chunk);
                    }
                    shared.deliver(chunk);
                };
                drop(reader);
                connected.store(false, Ordering::SeqCst);
                // Wake any transfer waiting on input.
                lock(&shared.router).route = Route::Terminal;
                let _ = ev_tx.send(SessionEvent::Disconnected(reason));
                repaint.request_repaint();
            })
            .expect("spawn connect thread");

        session
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn send(&self, data: Vec<u8>) {
        if !data.is_empty() {
            let _ = self.out.send(Out::Data(data));
        }
    }

    pub fn resize(&self, cols: u16, rows: u16, px_w: u16, px_h: u16) {
        let _ = self.out.send(Out::Resize { cols, rows, px_w, px_h });
    }

    pub fn send_break(&self) {
        let _ = self.out.send(Out::Break);
    }

    pub fn set_dtr(&self, on: bool) {
        let _ = self.out.send(Out::SetDtr(on));
    }

    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let _ = self.out.send(Out::Close);
    }

    pub fn set_capture(&self, file: Option<File>) {
        *lock(&self.capture) = file;
    }

    pub fn capturing(&self) -> bool {
        lock(&self.capture).is_some()
    }

    /// Divert incoming bytes into a channel for a file transfer. Any bytes
    /// held since an auto-detect are delivered first. Terminal routing comes
    /// back when the returned link's [`ReturnPath`] is finished or dropped.
    pub fn begin_transfer(&self) -> TransferLink {
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        {
            let mut router = lock(&self.shared.router);
            if let Route::Hold(held) = &mut router.route {
                if !held.is_empty() {
                    let _ = in_tx.send(std::mem::take(held));
                }
            }
            router.route = Route::Transfer(in_tx);
        }
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let out = self.out.clone();
        std::thread::Builder::new()
            .name("xfer-out".into())
            .spawn(move || {
                for chunk in out_rx {
                    if out.send(Out::Data(chunk)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn transfer forwarder");
        TransferLink { port: ChannelPort::new(in_rx, out_tx), ret: ReturnPath { shared: self.shared.clone() } }
    }

    /// Stop holding bytes after an auto-detect the user declined; what was
    /// held is shown on the terminal.
    pub fn release_hold(&self) {
        let mut router = lock(&self.shared.router);
        if let Route::Hold(held) = &mut router.route {
            let held = std::mem::take(held);
            router.route = Route::Terminal;
            self.shared.deliver_locked(&mut router, &held);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

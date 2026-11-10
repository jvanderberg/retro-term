//! Running file transfers on a background thread and tracking their progress.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use xfer_common::{ChannelControl, Event, Protocol, RecvOptions, Summary};
use xfer_xymodem::XVariant;

use crate::session::{Session, TransferLink};

/// A file transfer the remote side started on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// Remote ran `sz`: we should receive.
    ZmodemDownload,
    /// Remote ran `rz`: we should pick files to send.
    ZmodemUpload,
    /// Remote Kermit started sending.
    KermitDownload,
}

/// Watches terminal output for transfer start sequences.
pub struct Detectors {
    zmodem: xfer_zmodem::Detector,
    kermit: xfer_kermit::Detector,
}

impl Detectors {
    pub fn new() -> Detectors {
        Detectors { zmodem: xfer_zmodem::Detector::new(), kermit: xfer_kermit::Detector::new() }
    }

    /// Scan a chunk; on a hit returns the index just past the triggering
    /// byte and what was detected.
    pub fn scan(&mut self, chunk: &[u8]) -> Option<(usize, Detected)> {
        for (i, &b) in chunk.iter().enumerate() {
            if let Some(d) = self.zmodem.feed(b) {
                let what = match d {
                    xfer_zmodem::Detected::RemoteSending => Detected::ZmodemDownload,
                    xfer_zmodem::Detected::RemoteReceiving => Detected::ZmodemUpload,
                };
                return Some((i + 1, what));
            }
            if self.kermit.feed(b) {
                return Some((i + 1, Detected::KermitDownload));
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub enum Direction {
    Send(Vec<PathBuf>),
    /// Batch protocols: a directory. XMODEM: the exact file to write.
    Receive(PathBuf),
}

/// A transfer in progress, as seen by the UI.
pub struct TransferJob {
    pub protocol: Protocol,
    pub sending: bool,
    cancel: Arc<AtomicBool>,
    events: Receiver<Event>,
    done: Receiver<Result<Summary, String>>,
    pub started: Instant,
    pub file_started: Instant,
    pub current_file: String,
    pub file_size: Option<u64>,
    pub bytes: u64,
    /// A file is currently being transferred (`bytes` counts toward the total).
    pub in_progress: bool,
    /// Bytes from files already finished (for the overall rate).
    pub bytes_done: u64,
    pub files_done: usize,
    pub errors: u32,
    pub log: Vec<String>,
    pub result: Option<Result<Summary, String>>,
    pub finished_at: Option<Instant>,
}

impl TransferJob {
    pub fn start(session: &Session, protocol: Protocol, direction: Direction) -> TransferJob {
        let link = session.begin_transfer();
        let cancel = Arc::new(AtomicBool::new(false));
        let (ev_tx, ev_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut ctl = ChannelControl { events: ev_tx, cancel: cancel.clone() };
        let sending = matches!(direction, Direction::Send(_));
        std::thread::Builder::new()
            .name("transfer".into())
            .spawn(move || {
                let TransferLink { mut port, ret } = link;
                let p = &mut port;
                let c = &mut ctl;
                let result = match (&direction, protocol) {
                    (Direction::Send(files), Protocol::Zmodem) => xfer_zmodem::send(p, c, files),
                    (Direction::Receive(dir), Protocol::Zmodem) => xfer_zmodem::recv(p, c, &RecvOptions::new(dir)),
                    (Direction::Send(files), Protocol::Kermit) => {
                        xfer_kermit::send(p, c, files, &xfer_kermit::KermitOptions::default())
                    }
                    (Direction::Receive(dir), Protocol::Kermit) => {
                        xfer_kermit::recv(p, c, &RecvOptions::new(dir), &xfer_kermit::KermitOptions::default())
                    }
                    (Direction::Send(files), Protocol::Ymodem | Protocol::YmodemG) => {
                        xfer_xymodem::ymodem_send(p, c, files, protocol == Protocol::YmodemG)
                    }
                    (Direction::Receive(dir), Protocol::Ymodem | Protocol::YmodemG) => {
                        xfer_xymodem::ymodem_recv(p, c, &RecvOptions::new(dir), protocol == Protocol::YmodemG)
                    }
                    (dir, Protocol::Xmodem | Protocol::XmodemCrc | Protocol::Xmodem1k) => {
                        let v = match protocol {
                            Protocol::Xmodem => XVariant::Checksum,
                            Protocol::XmodemCrc => XVariant::Crc,
                            _ => XVariant::OneK,
                        };
                        match dir {
                            Direction::Send(files) => match files.first() {
                                Some(f) => xfer_xymodem::xmodem_send(p, c, f, v),
                                None => Ok(Summary::default()),
                            },
                            Direction::Receive(path) => xfer_xymodem::xmodem_recv(p, c, path, v),
                        }
                    }
                };
                // Back to the terminal (with whatever arrived after the
                // protocol finished) before the UI hears we're done.
                ret.finish(port);
                let _ = done_tx.send(result.map_err(|e| e.to_string()));
            })
            .expect("spawn transfer thread");
        let now = Instant::now();
        TransferJob {
            protocol,
            sending,
            cancel,
            events: ev_rx,
            done: done_rx,
            started: now,
            file_started: now,
            current_file: String::new(),
            file_size: None,
            bytes: 0,
            in_progress: false,
            bytes_done: 0,
            files_done: 0,
            errors: 0,
            log: Vec::new(),
            result: None,
            finished_at: None,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_finished(&self) -> bool {
        self.result.is_some()
    }

    /// Drain progress events. Returns true if the job just finished.
    pub fn poll(&mut self) -> bool {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                Event::FileStart { name, size } => {
                    self.current_file = name;
                    self.file_size = size;
                    self.bytes = 0;
                    self.in_progress = true;
                    self.file_started = Instant::now();
                }
                Event::Progress { bytes } => self.bytes = bytes,
                Event::FileDone { name, bytes, path } => {
                    self.files_done += 1;
                    self.bytes_done += bytes;
                    // Keep showing the finished file as complete.
                    self.bytes = bytes;
                    self.file_size = Some(self.file_size.unwrap_or(bytes).max(bytes));
                    self.in_progress = false;
                    self.log.push(format!("✔ {name} ({bytes} bytes) → {}", path.display()));
                }
                Event::FileSkipped { name, reason } => self.log.push(format!("⤼ skipped {name}: {reason}")),
                Event::Retry { reason } => {
                    self.errors += 1;
                    if self.log.len() < 500 {
                        self.log.push(format!("⟳ {reason}"));
                    }
                }
                Event::Message(m) => self.log.push(m),
            }
        }
        if self.result.is_none() {
            match self.done.try_recv() {
                Ok(r) => {
                    if let Err(e) = &r {
                        self.log.push(format!("✘ {e}"));
                    }
                    self.result = Some(r);
                    self.finished_at = Some(Instant::now());
                    return true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.result = Some(Err("transfer thread exited unexpectedly".into()));
                    self.finished_at = Some(Instant::now());
                    return true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        false
    }

    /// Average throughput in bytes/second over the whole job.
    pub fn rate(&self) -> f64 {
        let end = self.finished_at.unwrap_or_else(Instant::now);
        let secs = end.duration_since(self.started).as_secs_f64().max(0.001);
        let current = if self.in_progress { self.bytes } else { 0 };
        (self.bytes_done + current) as f64 / secs
    }
}

pub fn human_bytes(n: f64) -> String {
    if n >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", n / (1024.0 * 1024.0))
    } else if n >= 1024.0 {
        format!("{:.1} KiB", n / 1024.0)
    } else {
        format!("{n:.0} B")
    }
}

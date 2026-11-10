//! Shared plumbing for the file-transfer protocol crates.
//!
//! Every protocol is written as plain synchronous code that talks to a [`Port`]
//! (a byte pipe with read timeouts) and reports to a [`Control`] (progress
//! events + cancellation). The terminal runs each transfer on its own thread,
//! feeding it the bytes that arrive from the connection through a
//! [`ChannelPort`].

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod crc;

/// A bidirectional byte stream with timed reads.
pub trait Port: Send {
    /// Read up to `buf.len()` bytes, waiting at most `timeout` for the first
    /// byte. Returns `Ok(0)` on timeout. Returns an error with kind
    /// `UnexpectedEof` when the other side has gone away.
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;

    /// Write all of `data`.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;

    /// Flush buffered output, if any.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Discard any input that has already arrived but not been read.
    fn purge_input(&mut self);

    /// Read a single byte; `Ok(None)` on timeout.
    fn read_byte(&mut self, timeout: Duration) -> io::Result<Option<u8>> {
        let mut b = [0u8; 1];
        match self.read_timeout(&mut b, timeout)? {
            0 => Ok(None),
            _ => Ok(Some(b[0])),
        }
    }

    /// Fill `buf` completely, allowing at most `timeout` between bytes.
    /// Returns `Ok(false)` on timeout (buffer contents then unspecified).
    fn read_exact_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<bool> {
        let mut got = 0;
        while got < buf.len() {
            let n = self.read_timeout(&mut buf[got..], timeout)?;
            if n == 0 {
                return Ok(false);
            }
            got += n;
        }
        Ok(true)
    }
}

/// A [`Port`] built from a pair of channels. Incoming data arrives as chunks
/// on `rx`; outgoing data is sent as chunks on `tx`.
pub struct ChannelPort {
    rx: Receiver<Vec<u8>>,
    tx: Sender<Vec<u8>>,
    pending: VecDeque<u8>,
    closed: bool,
}

impl ChannelPort {
    pub fn new(rx: Receiver<Vec<u8>>, tx: Sender<Vec<u8>>) -> Self {
        Self { rx, tx, pending: VecDeque::new(), closed: false }
    }

    /// Two ports connected back to back (for loopback tests).
    pub fn pair() -> (ChannelPort, ChannelPort) {
        let (a_tx, b_rx) = mpsc::channel();
        let (b_tx, a_rx) = mpsc::channel();
        (ChannelPort::new(a_rx, a_tx), ChannelPort::new(b_rx, b_tx))
    }

    /// Spawn a child process and talk to it over its stdin/stdout.
    /// The child's stderr is inherited unless redirected by the caller.
    pub fn spawn(cmd: &mut std::process::Command) -> io::Result<(ChannelPort, std::process::Child)> {
        use std::process::Stdio;
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stdin = child.stdin.take().expect("stdin piped");
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if in_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        std::thread::spawn(move || {
            for chunk in out_rx {
                if stdin.write_all(&chunk).is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });
        Ok((ChannelPort::new(in_rx, out_tx), child))
    }

    /// Close the port and return everything received but not yet read: data
    /// the remote sent after the protocol finished (e.g. a shell prompt).
    pub fn into_unread(mut self) -> Vec<u8> {
        while let Ok(more) = self.rx.try_recv() {
            self.pending.extend(more);
        }
        self.pending.into()
    }

    fn take_pending(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.pending.len());
        for (slot, b) in buf.iter_mut().zip(self.pending.drain(..n)) {
            *slot = b;
        }
        n
    }
}

impl Port for ChannelPort {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.pending.is_empty() {
            return Ok(self.take_pending(buf));
        }
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(chunk) => {
                self.pending.extend(chunk);
                // Opportunistically grab anything else already queued.
                while let Ok(more) = self.rx.try_recv() {
                    self.pending.extend(more);
                }
                Ok(self.take_pending(buf))
            }
            Err(RecvTimeoutError::Timeout) => Ok(0),
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"))
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.tx.send(data.to_vec()).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }

    fn purge_input(&mut self) {
        self.pending.clear();
        while let Ok(_chunk) = self.rx.try_recv() {}
    }
}

/// Progress notifications emitted by a transfer.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A new file has started. `size` is known for senders and for receivers
    /// when the protocol carries it.
    FileStart { name: String, size: Option<u64> },
    /// Total bytes of the current file transferred so far.
    Progress { bytes: u64 },
    /// The current file finished successfully. `path` is where a received
    /// file was written (or the source path for a sent file).
    FileDone { name: String, bytes: u64, path: PathBuf },
    /// The current file was skipped (e.g. the receiver declined it).
    FileSkipped { name: String, reason: String },
    /// A recoverable error happened (bad CRC, timeout, retry...).
    Retry { reason: String },
    /// Free-form diagnostic message.
    Message(String),
}

/// Receives events from a transfer and tells it whether to stop.
pub trait Control {
    fn event(&mut self, ev: Event);
    fn cancelled(&self) -> bool;
}

/// A [`Control`] that forwards events over a channel and watches a shared flag.
pub struct ChannelControl {
    pub events: Sender<Event>,
    pub cancel: Arc<AtomicBool>,
}

impl Control for ChannelControl {
    fn event(&mut self, ev: Event) {
        let _ = self.events.send(ev);
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A [`Control`] that records events in memory (handy for tests).
#[derive(Default)]
pub struct RecordingControl {
    pub events: Vec<Event>,
    pub cancel: Arc<AtomicBool>,
}

impl Control for RecordingControl {
    fn event(&mut self, ev: Event) {
        self.events.push(ev);
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum XferError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("timed out waiting for the remote side")]
    Timeout,
    #[error("transfer cancelled")]
    Cancelled,
    #[error("remote side cancelled the transfer")]
    RemoteCancelled,
    #[error("too many errors: {0}")]
    TooManyErrors(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, XferError>;

/// What a finished transfer accomplished.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// (file name, bytes, local path) for every file completed.
    pub files: Vec<(String, u64, PathBuf)>,
}

/// Options common to all receivers.
#[derive(Debug, Clone)]
pub struct RecvOptions {
    /// Directory into which received files are written.
    pub dest_dir: PathBuf,
    /// Overwrite existing files instead of picking a unique name.
    pub overwrite: bool,
}

impl RecvOptions {
    pub fn new(dest_dir: impl Into<PathBuf>) -> Self {
        Self { dest_dir: dest_dir.into(), overwrite: false }
    }
}

/// Longest file name kept (bytes); most filesystems allow 255.
const MAX_NAME: usize = 200;

/// Turn a remote-supplied file name into a safe single path component:
/// strips directory parts, control characters and `:`, leading dots, Windows
/// device names (CON, NUL, COM1, ...), and overlong names.
pub fn safe_file_name(remote_name: &str) -> String {
    let base = remote_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        .collect::<String>();
    let mut base = base.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    if base.len() > MAX_NAME {
        // Keep the extension when shortening.
        let ext = base.rfind('.').map(|i| base[i..].to_string()).filter(|e| e.len() <= 16).unwrap_or_default();
        let mut cut = MAX_NAME - ext.len();
        while !base.is_char_boundary(cut) {
            cut -= 1;
        }
        base = format!("{}{ext}", &base[..cut]);
    }
    let stem = base.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        base.insert(0, '_');
    }
    if base.is_empty() {
        "received.bin".to_string()
    } else {
        base
    }
}

/// Turn a remote-supplied file name into a safe local path inside `dir`.
/// Strips any directory components (see [`safe_file_name`]) and (unless
/// `overwrite`) appends `.1`, `.2`, ... until the name is unused.
///
/// Prefer [`create_dest_file`], which also closes the race between
/// checking and creating.
pub fn safe_dest_path(dir: &Path, remote_name: &str, overwrite: bool) -> PathBuf {
    let base = safe_file_name(remote_name);
    let candidate = dir.join(&base);
    // symlink_metadata: a dangling symlink still counts as taken.
    if overwrite || candidate.symlink_metadata().is_err() {
        return candidate;
    }
    (1..).map(|i| dir.join(format!("{base}.{i}"))).find(|c| c.symlink_metadata().is_err()).expect("unbounded")
}

/// Create the local file for a received `remote_name` in `dir`, never
/// following a symlink planted there and (unless `overwrite`) never
/// replacing an existing file: `.1`, `.2`, ... are appended instead.
pub fn create_dest_file(dir: &Path, remote_name: &str, overwrite: bool) -> io::Result<(PathBuf, std::fs::File)> {
    let base = safe_file_name(remote_name);
    let first = dir.join(&base);
    let is_symlink = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
    if overwrite && !is_symlink(&first) {
        return std::fs::File::create(&first).map(|f| (first, f));
    }
    for i in 0..10_000u32 {
        let path = if i == 0 { first.clone() } else { dir.join(format!("{base}.{i}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("too many files named {base}")))
}

/// A remote-supplied modification time (seconds since 1970), or `None` if
/// it is zero or can't be represented.
pub fn mtime_from_unix(secs: u64) -> Option<std::time::SystemTime> {
    if secs == 0 {
        return None;
    }
    std::time::UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// The file name (last path component) to announce for a local file.
pub fn announce_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file.bin".to_string())
}

/// Rate-limits `Progress` events so the UI isn't flooded.
pub struct ProgressThrottle {
    last: Instant,
    interval: Duration,
}

impl Default for ProgressThrottle {
    fn default() -> Self {
        Self { last: Instant::now() - Duration::from_secs(1), interval: Duration::from_millis(50) }
    }
}

impl ProgressThrottle {
    /// Emit a progress event if enough time has passed (or `force`).
    pub fn report(&mut self, ctl: &mut dyn Control, bytes: u64, force: bool) {
        let now = Instant::now();
        if force || now.duration_since(self.last) >= self.interval {
            self.last = now;
            ctl.event(Event::Progress { bytes });
        }
    }
}

/// Which protocol to run (shared by the UI and CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Xmodem,
    XmodemCrc,
    Xmodem1k,
    Ymodem,
    YmodemG,
    Zmodem,
    Kermit,
}

impl Protocol {
    pub const ALL: [Protocol; 7] = [
        Protocol::Zmodem,
        Protocol::Ymodem,
        Protocol::YmodemG,
        Protocol::Xmodem1k,
        Protocol::XmodemCrc,
        Protocol::Xmodem,
        Protocol::Kermit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Protocol::Xmodem => "XMODEM (checksum)",
            Protocol::XmodemCrc => "XMODEM-CRC",
            Protocol::Xmodem1k => "XMODEM-1K",
            Protocol::Ymodem => "YMODEM",
            Protocol::YmodemG => "YMODEM-G",
            Protocol::Zmodem => "ZMODEM",
            Protocol::Kermit => "Kermit",
        }
    }

    /// Whether the protocol can carry several files with names.
    pub fn is_batch(self) -> bool {
        matches!(self, Protocol::Ymodem | Protocol::YmodemG | Protocol::Zmodem | Protocol::Kermit)
    }

    pub fn from_cli(s: &str) -> Option<Protocol> {
        Some(match s.to_ascii_lowercase().as_str() {
            "x" | "xmodem" | "xmodem-checksum" => Protocol::Xmodem,
            "xcrc" | "xmodem-crc" => Protocol::XmodemCrc,
            "x1k" | "xmodem-1k" | "xmodem1k" => Protocol::Xmodem1k,
            "y" | "ymodem" => Protocol::Ymodem,
            "yg" | "ymodem-g" | "ymodemg" => Protocol::YmodemG,
            "z" | "zmodem" => Protocol::Zmodem,
            "k" | "kermit" => Protocol::Kermit,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_pair_roundtrip() {
        let (mut a, mut b) = ChannelPort::pair();
        a.write_all(b"hello").unwrap();
        let mut buf = [0u8; 16];
        let n = b.read_timeout(&mut buf, Duration::from_millis(100)).unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(b.read_timeout(&mut buf, Duration::from_millis(10)).unwrap(), 0);
        drop(a);
        assert!(b.read_timeout(&mut buf, Duration::from_millis(10)).is_err());
    }

    #[test]
    fn safe_names() {
        let dir = std::env::temp_dir();
        assert_eq!(safe_dest_path(&dir, "../../etc/passwd", true), dir.join("passwd"));
        assert_eq!(safe_dest_path(&dir, "C:\\x\\y.txt", true), dir.join("y.txt"));
        assert_eq!(safe_dest_path(&dir, "..", true), dir.join("received.bin"));
        assert_eq!(safe_file_name("CON"), "_CON");
        assert_eq!(safe_file_name("com1.txt"), "_com1.txt");
        assert_eq!(safe_file_name("COMMAND.COM"), "COMMAND.COM");
        let long = format!("{}.zip", "é".repeat(300));
        let short = safe_file_name(&long);
        assert!(short.len() <= MAX_NAME && short.ends_with(".zip"), "{short}");
        assert_eq!(mtime_from_unix(u64::MAX), None);
        assert!(mtime_from_unix(1_700_000_000).is_some());
    }

    #[test]
    fn create_dest_file_never_clobbers() {
        let dir = tempfile_dir();
        std::fs::write(dir.join("a.txt"), b"keep").unwrap();
        let (p, _) = create_dest_file(&dir, "a.txt", false).unwrap();
        assert_eq!(p, dir.join("a.txt.1"));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"keep");
        #[cfg(unix)]
        {
            let target = dir.join("outside");
            std::os::unix::fs::symlink(&target, dir.join("link")).unwrap();
            let (p, _) = create_dest_file(&dir, "link", true).unwrap();
            assert_eq!(p, dir.join("link.1"));
            assert!(!target.exists());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("xfer-common-test-{}-{:?}", std::process::id(), Instant::now()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

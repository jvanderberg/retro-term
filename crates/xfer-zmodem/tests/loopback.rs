//! Our sender against our receiver over an in-memory channel pair.

mod common;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use xfer_common::{ChannelPort, Control, Event, Port, RecordingControl, RecvOptions, Summary, XferError};
use xfer_zmodem::{recv_with_options, send_with_options, Options};

type Outcome = (xfer_common::Result<Summary>, Vec<Event>);

fn transfer<P: Port + 'static>(
    files: Vec<PathBuf>,
    dest: PathBuf,
    sopts: Options,
    ropts: Options,
    wrap: impl FnOnce(ChannelPort) -> P,
) -> (Outcome, Outcome) {
    let (a, mut b) = ChannelPort::pair();
    let mut a = wrap(a);
    let sender = thread::spawn(move || {
        let mut ctl = RecordingControl::default();
        let r = send_with_options(&mut a, &mut ctl, &files, &sopts);
        (r, ctl.events)
    });
    let mut ctl = RecordingControl::default();
    let r = recv_with_options(&mut b, &mut ctl, &RecvOptions::new(dest), &ropts);
    let rx = (r, ctl.events);
    (sender.join().unwrap(), rx)
}

fn fast_opts() -> Options {
    Options { timeout: Duration::from_secs(2), ..Options::default() }
}

fn check_ok(files: &[PathBuf], dest: &std::path::Path, tx: &Outcome, rx: &Outcome) {
    if tx.0.is_err() || rx.0.is_err() {
        let tail = |ev: &Vec<Event>| {
            ev.iter().filter(|e| !matches!(e, Event::Progress { .. })).rev().take(12).cloned().collect::<Vec<_>>()
        };
        panic!("tx={:?}\nrx={:?}\ntx events={:#?}\nrx events={:#?}", tx.0, rx.0, tail(&tx.1), tail(&rx.1));
    }
    let s = tx.0.as_ref().expect("sender failed");
    let r = rx.0.as_ref().expect("receiver failed");
    assert_eq!(s.files.len(), files.len());
    assert_eq!(r.files.len(), files.len());
    assert_same(files, dest);
    for ((sn, sb, _), (rn, rb, _)) in s.files.iter().zip(&r.files) {
        assert_eq!(sn, rn);
        assert_eq!(sb, rb);
    }
}

#[test]
fn loopback_standard_files() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), fast_opts(), |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
    // Events make sense.
    let starts = rx.1.iter().filter(|e| matches!(e, Event::FileStart { .. })).count();
    let dones = rx.1.iter().filter(|e| matches!(e, Event::FileDone { .. })).count();
    assert_eq!((starts, dones), (files.len(), files.len()));
    assert!(rx.1.iter().any(|e| matches!(e, Event::FileStart { name, size: Some(0) } if name == "empty.bin")));
    assert!(tx.1.iter().any(|e| matches!(e, Event::Progress { .. })));
}

#[test]
fn loopback_preserves_mtime() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let f = write_file(src.path(), "dated.txt", b"old news");
    let when = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    std::fs::File::options().write(true).open(&f).unwrap().set_modified(when).unwrap();
    let (tx, rx) = transfer(vec![f.clone()], dst.path().into(), fast_opts(), fast_opts(), |p| p);
    check_ok(&[f], dst.path(), &tx, &rx);
    let got = std::fs::metadata(dst.path().join("dated.txt")).unwrap().modified().unwrap();
    assert_eq!(got, when);
}

#[test]
fn loopback_crc16_escctl_and_rx_buffer_window() {
    let src = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    // CRC-16 everywhere.
    let dst = tempfile::tempdir().unwrap();
    let r16 = Options { use_crc32: false, ..fast_opts() };
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), r16, |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
    // Escape all control characters (receiver requests it).
    let dst = tempfile::tempdir().unwrap();
    let rctl = Options { escape_ctl: true, ..fast_opts() };
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), rctl, |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
    // Escape all control characters (sender requests it via ZSINIT).
    let dst = tempfile::tempdir().unwrap();
    let sctl = Options { escape_ctl: true, ..fast_opts() };
    let (tx, rx) = transfer(files.clone(), dst.path().into(), sctl, fast_opts(), |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
    // Receiver with a 4 KiB buffer: sender must use ZCRCW windows.
    let dst = tempfile::tempdir().unwrap();
    let rbuf = Options { rx_buffer_size: 4096, ..fast_opts() };
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), rbuf, |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
    // Small blocks, no window.
    let dst = tempfile::tempdir().unwrap();
    let small = Options { max_block: 1024, window: 0, ..fast_opts() };
    let (tx, rx) = transfer(files.clone(), dst.path().into(), small, fast_opts(), |p| p);
    check_ok(&files, dst.path(), &tx, &rx);
}

/// Flips a byte in the outgoing stream every `interval` bytes.
struct Corrupting {
    inner: ChannelPort,
    sent: u64,
    next: u64,
    interval: u64,
    hits: Arc<AtomicUsize>,
}

impl Port for Corrupting {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.inner.read_timeout(buf, timeout)
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let end = self.sent + data.len() as u64;
        let r = if data.len() > 64 && end > self.next {
            let mut v = data.to_vec();
            let idx = (self.next.saturating_sub(self.sent) as usize).min(v.len() - 1);
            v[idx] ^= 0x5a;
            self.next += self.interval + (self.hits.load(Ordering::Relaxed) as u64 * 7919) % self.interval;
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.inner.write_all(&v)
        } else {
            self.inner.write_all(data)
        };
        self.sent = end;
        r
    }
    fn purge_input(&mut self) {
        self.inner.purge_input();
    }
}

#[test]
fn loopback_recovers_from_corruption() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), fast_opts(), move |p| Corrupting {
        inner: p,
        sent: 0,
        next: 50_000,
        interval: 150_000,
        hits: h2,
    });
    check_ok(&files, dst.path(), &tx, &rx);
    let n = hits.load(Ordering::Relaxed);
    for (side, ev) in [("tx", &tx.1), ("rx", &rx.1)] {
        let mut counts = std::collections::BTreeMap::new();
        for e in ev.iter() {
            if let Event::Retry { reason } = e {
                let key: String = reason.chars().take_while(|c| !c.is_ascii_digit()).collect();
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        eprintln!("{side} retries: {counts:?}");
    }
    assert!(n >= 10, "only {n} corruptions injected");
    let rx_retries = rx.1.iter().filter(|e| matches!(e, Event::Retry { .. })).count();
    let tx_retries = tx.1.iter().filter(|e| matches!(e, Event::Retry { .. })).count();
    assert!(rx_retries >= 5 && tx_retries >= 5, "retries rx={rx_retries} tx={tx_retries} hits={n}");
}

/// Cancels once a given number of progress events have been seen.
struct CancelAfter {
    inner: RecordingControl,
    after: usize,
    seen: usize,
    flag: AtomicBool,
}

impl CancelAfter {
    fn new(after: usize) -> Self {
        Self { inner: RecordingControl::default(), after, seen: 0, flag: AtomicBool::new(false) }
    }
}

impl Control for CancelAfter {
    fn event(&mut self, ev: Event) {
        if matches!(ev, Event::Progress { .. }) {
            self.seen += 1;
            if self.seen >= self.after {
                self.flag.store(true, Ordering::Relaxed);
            }
        }
        self.inner.event(ev);
    }
    fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// A port that throttles writes so a transfer lasts long enough to cancel.
struct Slow(ChannelPort);

impl Port for Slow {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.0.read_timeout(buf, timeout)
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if data.len() > 1000 {
            thread::sleep(Duration::from_millis(2));
        }
        self.0.write_all(data)
    }
    fn purge_input(&mut self) {
        self.0.purge_input();
    }
}

#[test]
fn sender_cancel_is_seen_by_receiver() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = vec![write_file(src.path(), "big.bin", &random_bytes(4 << 20, 3))];
    let (a, mut b) = ChannelPort::pair();
    let opts = Options { max_block: 1024, ..fast_opts() };
    let sender = thread::spawn(move || {
        let mut a = Slow(a);
        let mut ctl = CancelAfter::new(3);
        send_with_options(&mut a, &mut ctl, &files, &opts)
    });
    let mut ctl = RecordingControl::default();
    let t0 = Instant::now();
    let r = recv_with_options(&mut b, &mut ctl, &RecvOptions::new(dst.path()), &fast_opts());
    assert!(matches!(r, Err(XferError::RemoteCancelled)), "receiver got {r:?}");
    assert!(matches!(sender.join().unwrap(), Err(XferError::Cancelled)));
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn receiver_cancel_is_seen_by_sender() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = vec![write_file(src.path(), "big.bin", &random_bytes(4 << 20, 4))];
    let (a, b) = ChannelPort::pair();
    let opts = Options { max_block: 1024, ..fast_opts() };
    let sender = thread::spawn(move || {
        let mut a = Slow(a);
        let mut ctl = RecordingControl::default();
        send_with_options(&mut a, &mut ctl, &files, &opts)
    });
    let mut b = b;
    let mut ctl = CancelAfter::new(3);
    let r = recv_with_options(&mut b, &mut ctl, &RecvOptions::new(dst.path()), &fast_opts());
    assert!(matches!(r, Err(XferError::Cancelled)), "receiver got {r:?}");
    let s = sender.join().unwrap();
    assert!(matches!(s, Err(XferError::RemoteCancelled)), "sender got {s:?}");
}

#[test]
fn receiver_times_out_without_sender() {
    let (_a, mut b) = ChannelPort::pair();
    let dst = tempfile::tempdir().unwrap();
    let mut ctl = RecordingControl::default();
    let opts = Options { timeout: Duration::from_millis(100), max_retries: 2, ..Options::default() };
    let r = recv_with_options(&mut b, &mut ctl, &RecvOptions::new(dst.path()), &opts);
    assert!(matches!(r, Err(XferError::Timeout)), "{r:?}");
}

#[test]
fn twenty_megabytes_fast() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = vec![write_file(src.path(), "twenty.bin", &random_bytes(20 << 20, 11))];
    let t0 = Instant::now();
    let (tx, rx) = transfer(files.clone(), dst.path().into(), fast_opts(), fast_opts(), |p| p);
    let dt = t0.elapsed();
    check_ok(&files, dst.path(), &tx, &rx);
    eprintln!("20 MiB loopback transfer took {dt:?}");
    if !cfg!(debug_assertions) {
        assert!(dt < Duration::from_secs(1), "too slow: {dt:?}");
    }
}

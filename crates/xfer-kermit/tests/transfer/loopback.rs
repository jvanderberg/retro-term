//! Our sender against our receiver over an in-memory link.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use xfer_common::{ChannelPort, Control, Event, RecordingControl, RecvOptions, Result, Summary, XferError};
use xfer_kermit::{EightBitPrefix, KermitOptions};

use super::common::{assert_same_files, make_files, FaultyPort};

#[derive(Clone, Copy, Default)]
struct Faults {
    drop: u64,
    corrupt: u64,
    strip: bool,
    seed: u64,
}

struct Outcome {
    sent: Result<Summary>,
    received: Result<Summary>,
    send_events: Vec<Event>,
    recv_events: Vec<Event>,
}

fn run_pair(
    sopts: &KermitOptions,
    ropts: &KermitOptions,
    files: &[PathBuf],
    dest: &Path,
    (sf, rf): (Faults, Faults),
    (mut sctl, mut rctl): (Rec, Rec),
) -> (Result<Summary>, Result<Summary>, Rec, Rec) {
    let (a, b) = ChannelPort::pair();
    let mut sp = FaultyPort::new(a, sf.seed + 1);
    sp.drop_per_mille = sf.drop;
    sp.corrupt_per_mille = sf.corrupt;
    sp.strip_8th_bit = sf.strip;
    let mut rp = FaultyPort::new(b, rf.seed + 2);
    rp.drop_per_mille = rf.drop;
    rp.corrupt_per_mille = rf.corrupt;
    rp.strip_8th_bit = rf.strip;
    let ropts = ropts.clone();
    let recv_opts = RecvOptions::new(dest);
    let rx = thread::spawn(move || {
        let r = xfer_kermit::recv(&mut rp, &mut rctl, &recv_opts, &ropts);
        (r, rctl)
    });
    let sent = xfer_kermit::send(&mut sp, &mut sctl, files, sopts);
    // Keep our end open until the receiver is done (like a real line).
    let (received, rctl) = rx.join().unwrap();
    drop(sp);
    (sent, received, sctl, rctl)
}

/// Records events; optionally cancels at the first progress report for the
/// named file.
struct Rec {
    inner: RecordingControl,
    cancel_in: Option<&'static str>,
    armed: bool,
}

impl Control for Rec {
    fn event(&mut self, ev: Event) {
        match &ev {
            Event::FileStart { name, .. } => self.armed = self.cancel_in == Some(name.as_str()),
            Event::Progress { bytes } if self.armed && *bytes > 0 => self.inner.cancel.store(true, Ordering::Relaxed),
            _ => {}
        }
        self.inner.event(ev);
    }
    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }
}

fn rec(cancel_in: Option<&'static str>) -> Rec {
    Rec { inner: RecordingControl::default(), cancel_in, armed: false }
}

fn transfer(sopts: &KermitOptions, ropts: &KermitOptions, sf: Faults, rf: Faults, big: usize) -> Outcome {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), big);
    let start = Instant::now();
    let (sent, received, sc, rc) = run_pair(sopts, ropts, &files, dst.path(), (sf, rf), (rec(None), rec(None)));
    eprintln!("transfer took {:?}", start.elapsed());
    let out = Outcome { sent, received, send_events: sc.inner.events, recv_events: rc.inner.events };
    if let (Ok(s), Ok(r)) = (&out.sent, &out.received) {
        assert_same_files(&files, dst.path());
        assert_eq!(s.files.len(), files.len());
        assert_eq!(r.files.len(), files.len());
        for ((sn, sb, _), (rn, rb, _)) in s.files.iter().zip(&r.files) {
            assert_eq!((sn, sb), (rn, rb));
        }
    }
    out
}

fn ok(out: &Outcome) {
    if let Err(e) = &out.sent {
        panic!("send failed: {e}; events {:?}", tail(&out.send_events));
    }
    if let Err(e) = &out.received {
        panic!("recv failed: {e}; events {:?}", tail(&out.recv_events));
    }
}

fn tail(ev: &[Event]) -> &[Event] {
    &ev[ev.len().saturating_sub(10)..]
}

fn rlist(ev: &[Event]) -> Vec<&Event> {
    ev.iter().filter(|e| matches!(e, Event::Retry { .. })).collect()
}

fn retries(ev: &[Event]) -> usize {
    ev.iter().filter(|e| matches!(e, Event::Retry { .. })).count()
}

fn opts(f: impl FnOnce(&mut KermitOptions)) -> KermitOptions {
    let mut o = KermitOptions { timeout: Duration::from_millis(300), ..KermitOptions::default() };
    f(&mut o);
    o
}

const MB: usize = 1024 * 1024;

#[test]
fn defaults_long_packets_windows_crc() {
    let o = opts(|_| {});
    let out = transfer(&o, &o, Faults::default(), Faults::default(), 3 * MB);
    ok(&out);
    assert_eq!(
        retries(&out.send_events) + retries(&out.recv_events),
        0,
        "{:?} {:?}",
        rlist(&out.send_events),
        rlist(&out.recv_events)
    );
    let msg = out.send_events.iter().find_map(|e| match e {
        Event::Message(m) => Some(m.clone()),
        _ => None,
    });
    let msg = msg.unwrap();
    assert!(msg.contains("block check 3") && msg.contains("window 30") && msg.contains("4000"), "{msg}");
    // FileStart carries the size on both sides (sizes come from A packets).
    let size_of = |ev: &[Event], n: &str| {
        ev.iter().find_map(|e| match e {
            Event::FileStart { name, size } if name == n => Some(*size),
            _ => None,
        })
    };
    assert_eq!(size_of(&out.recv_events, "all-bytes.bin"), Some(Some(1024)));
    assert_eq!(size_of(&out.send_events, "empty.dat"), Some(Some(0)));
}

#[test]
fn block_check_1_short_packets_stop_and_wait() {
    let o = opts(|o| {
        o.block_check = 1;
        o.long_packets = false;
        o.max_packet_len = 94;
        o.window = 1;
    });
    ok(&transfer(&o, &o, Faults::default(), Faults::default(), 200_000));
}

#[test]
fn block_check_2_medium_packets_small_window() {
    let o = opts(|o| {
        o.block_check = 2;
        o.max_packet_len = 1000;
        o.window = 4;
    });
    ok(&transfer(&o, &o, Faults::default(), Faults::default(), MB));
}

#[test]
fn receiver_adopts_senders_block_check() {
    // Different preferences: the receiver agrees to what the sender asks for.
    let s = opts(|o| o.block_check = 2);
    let r = opts(|o| o.block_check = 3);
    let out = transfer(&s, &r, Faults::default(), Faults::default(), 100_000);
    ok(&out);
}

#[test]
fn forced_8th_bit_prefixing_over_7_bit_link() {
    let strip = Faults { strip: true, ..Faults::default() };
    for (s, r) in [
        (EightBitPrefix::Always, EightBitPrefix::Always),
        (EightBitPrefix::Always, EightBitPrefix::IfRequested),
        (EightBitPrefix::IfRequested, EightBitPrefix::Always),
    ] {
        let so = opts(|o| {
            o.eight_bit = s;
            o.repeat_counts = false;
        });
        let ro = opts(|o| o.eight_bit = r);
        let out = transfer(&so, &ro, strip, strip, 300_000);
        ok(&out);
        assert_eq!(
            retries(&out.send_events) + retries(&out.recv_events),
            0,
            "{:?} {:?}",
            tail(&out.send_events),
            tail(&out.recv_events)
        );
    }
}

#[test]
fn eight_bit_data_without_prefixing_is_corrupted_on_7_bit_link() {
    // Sanity check of the test harness: without QBIN a 7-bit link breaks.
    let strip = Faults { strip: true, ..Faults::default() };
    let o = opts(|o| {
        o.retry_limit = 3;
        o.timeout = Duration::from_millis(100);
    });
    let out = transfer(&o, &o, strip, strip, 10_000);
    eprintln!("{:?} / {:?}", out.sent, out.received);
    assert!(out.sent.is_err() && out.received.is_err());
}

#[test]
fn lossy_link_sliding_window() {
    let o = opts(|o| {
        o.retry_limit = 20;
        o.timeout = Duration::from_millis(200);
    });
    let sf = Faults { drop: 20, corrupt: 20, seed: 11, ..Faults::default() };
    let rf = Faults { drop: 30, corrupt: 30, seed: 12, ..Faults::default() };
    let out = transfer(&o, &o, sf, rf, MB);
    ok(&out);
    assert!(retries(&out.send_events) + retries(&out.recv_events) > 0, "faults should have caused retries");
}

#[test]
fn lossy_link_stop_and_wait_short_packets() {
    let o = opts(|o| {
        o.retry_limit = 20;
        o.timeout = Duration::from_millis(100);
        o.window = 1;
        o.long_packets = false;
        o.block_check = 1;
    });
    let sf = Faults { drop: 30, corrupt: 30, seed: 21, ..Faults::default() };
    let rf = Faults { drop: 30, corrupt: 30, seed: 22, ..Faults::default() };
    ok(&transfer(&o, &o, sf, rf, 25_000));
}

#[test]
fn lossy_link_many_seeds() {
    for seed in 100..112 {
        let o = opts(|o| {
            o.retry_limit = 25;
            o.timeout = Duration::from_millis(100);
            o.window = (seed % 5 * 7 + 1) as u8;
            o.max_packet_len = [200, 1000, 4000][seed as usize % 3];
            o.block_check = (seed % 3 + 1) as u8;
        });
        let sf = Faults { drop: 50, corrupt: 40, seed, ..Faults::default() };
        let rf = Faults { drop: 50, corrupt: 40, seed: seed * 3, ..Faults::default() };
        ok(&transfer(&o, &o, sf, rf, 80_000));
    }
}

#[test]
fn sender_cancel_sends_error_and_discards_partial() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), 4 * MB);
    let o = KermitOptions { max_packet_len: 500, window: 2, ..opts(|_| {}) };
    let (sent, received, _sc, _rc) =
        run_pair(&o, &o, &files, dst.path(), (Faults::default(), Faults::default()), (rec(Some("big.bin")), rec(None)));
    assert!(matches!(sent, Err(XferError::Cancelled)), "{sent:?}");
    match received {
        Err(XferError::Protocol(m)) => assert!(m.contains("Cancelled by user"), "{m}"),
        other => panic!("receiver should see the E packet: {other:?}"),
    }
    assert!(!dst.path().join("big.bin").exists(), "partial file must be removed");
    assert!(dst.path().join("hello.txt").exists(), "completed files stay");
}

#[test]
fn receiver_cancel_sends_error() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), 4 * MB);
    let o = KermitOptions { max_packet_len: 500, window: 2, ..opts(|_| {}) };
    let (sent, received, _sc, _rc) =
        run_pair(&o, &o, &files, dst.path(), (Faults::default(), Faults::default()), (rec(None), rec(Some("big.bin"))));
    assert!(matches!(received, Err(XferError::Cancelled)), "{received:?}");
    match sent {
        Err(XferError::Protocol(m)) => assert!(m.contains("Cancelled by user"), "{m}"),
        other => panic!("sender should see the E packet: {other:?}"),
    }
    assert!(!dst.path().join("big.bin").exists());
}

#[test]
fn receiver_times_out_without_sender() {
    let (a, mut b) = ChannelPort::pair();
    let dst = tempfile::tempdir().unwrap();
    let o = KermitOptions { timeout: Duration::from_millis(50), retry_limit: 3, ..KermitOptions::default() };
    let mut ctl = RecordingControl::default();
    let r = xfer_kermit::recv(&mut b, &mut ctl, &RecvOptions::new(dst.path()), &o);
    assert!(matches!(r, Err(XferError::Timeout)), "{r:?}");
    drop(a);
}

#[test]
fn missing_file_is_skipped() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let mut files = make_files(src.path(), 1000);
    files.insert(1, src.path().join("does-not-exist"));
    let o = opts(|_| {});
    let (sent, received, sc, _rc) =
        run_pair(&o, &o, &files, dst.path(), (Faults::default(), Faults::default()), (rec(None), rec(None)));
    assert_eq!(sent.unwrap().files.len(), files.len() - 1);
    assert_eq!(received.unwrap().files.len(), files.len() - 1);
    assert!(sc.inner.events.iter().any(|e| matches!(e, Event::FileSkipped { name, .. } if name == "does-not-exist")));
    files.remove(1);
    assert_same_files(&files, dst.path());
}

//! Our sender against our receiver over an in-memory channel pair.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use xfer_common::{ChannelPort, Control, Event, Port, RecordingControl, RecvOptions, Result, Summary, XferError};
use xfer_xymodem::{xmodem_recv, xmodem_send, ymodem_recv, ymodem_send, XVariant};

use crate::common::{count_retries, random_bytes, test_files, write_files, Faulty};

/// A control that asks to cancel once `after` progress events have been seen.
struct CancelAfter {
    events: Vec<Event>,
    after: usize,
    progress: usize,
}

impl CancelAfter {
    fn new(after: usize) -> Self {
        Self { events: Vec::new(), after, progress: 0 }
    }
}

impl Control for CancelAfter {
    fn event(&mut self, ev: Event) {
        if matches!(ev, Event::Progress { .. }) {
            self.progress += 1;
        }
        self.events.push(ev);
    }
    fn cancelled(&self) -> bool {
        self.progress >= self.after
    }
}

type Side<C> = (Result<Summary>, C);

/// Run `send` and `recv` on two threads joined by ports `ps` and `pr`.
fn run_pair<PS, PR, CS, CR, S, R>(ps: PS, cs: CS, send: S, pr: PR, cr: CR, recv: R) -> (Side<CS>, Side<CR>)
where
    PS: Port + 'static,
    PR: Port + 'static,
    CS: Control + Send + 'static,
    CR: Control + Send + 'static,
    S: FnOnce(&mut dyn Port, &mut dyn Control) -> Result<Summary> + Send + 'static,
    R: FnOnce(&mut dyn Port, &mut dyn Control) -> Result<Summary> + Send + 'static,
{
    let st = thread::spawn(move || {
        let (mut p, mut c) = (ps, cs);
        let r = send(&mut p, &mut c);
        (r, c)
    });
    let rt = thread::spawn(move || {
        let (mut p, mut c) = (pr, cr);
        let r = recv(&mut p, &mut c);
        (r, c)
    });
    (st.join().unwrap(), rt.join().unwrap())
}

fn x_roundtrip(
    src: &Path,
    dest: &Path,
    sv: XVariant,
    rv: XVariant,
) -> (Side<RecordingControl>, Side<RecordingControl>) {
    let (a, b) = ChannelPort::pair();
    let (src, dest) = (src.to_path_buf(), dest.to_path_buf());
    run_pair(
        a,
        RecordingControl::default(),
        move |p, c| xmodem_send(p, c, &src, sv),
        b,
        RecordingControl::default(),
        move |p, c| xmodem_recv(p, c, &dest, rv),
    )
}

#[test]
fn xmodem_all_variants_all_files() {
    let dir = tempfile::tempdir().unwrap();
    let files = test_files();
    let paths = write_files(dir.path(), &files);
    for v in [XVariant::Checksum, XVariant::Crc, XVariant::OneK] {
        for ((name, data), path) in files.iter().zip(&paths) {
            let dest = dir.path().join(format!("out-{v:?}-{name}"));
            let ((sr, sc), (rr, rc)) = x_roundtrip(path, &dest, v, v);
            let ss = sr.unwrap_or_else(|e| panic!("{v:?} {name} send: {e}"));
            let rs = rr.unwrap_or_else(|e| panic!("{v:?} {name} recv: {e}"));
            assert_eq!(ss.files[0].1, data.len() as u64);
            assert_eq!(rs.files[0].1, data.len() as u64);
            assert_eq!(&std::fs::read(&dest).unwrap(), data, "{v:?} {name}");
            assert_eq!(count_retries(&sc.events), 0, "{v:?} {name}: {:?}", sc.events);
            assert_eq!(count_retries(&rc.events), 0, "{v:?} {name}: {:?}", rc.events);
            assert!(matches!(rc.events.last(), Some(Event::FileDone { .. })));
        }
    }
}

#[test]
fn xmodem_mixed_variants() {
    let dir = tempfile::tempdir().unwrap();
    let data = random_bytes(5000, 9);
    let src = dir.path().join("src.bin");
    std::fs::write(&src, &data).unwrap();
    // The receiver decides the check; 1K falls back to 128/checksum on NAK.
    for (sv, rv) in [
        (XVariant::OneK, XVariant::Checksum),
        (XVariant::Checksum, XVariant::Crc),
        (XVariant::Crc, XVariant::Checksum),
        (XVariant::Checksum, XVariant::OneK),
        (XVariant::OneK, XVariant::Crc),
    ] {
        let dest = dir.path().join(format!("out-{sv:?}-{rv:?}"));
        let ((sr, _), (rr, _)) = x_roundtrip(&src, &dest, sv, rv);
        sr.unwrap();
        rr.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data, "{sv:?} -> {rv:?}");
    }
}

fn y_roundtrip(
    ps: impl Port + 'static,
    pr: impl Port + 'static,
    files: Vec<PathBuf>,
    out: &Path,
    g: bool,
) -> (Side<RecordingControl>, Side<RecordingControl>) {
    let opts = RecvOptions::new(out);
    run_pair(
        ps,
        RecordingControl::default(),
        move |p, c| ymodem_send(p, c, &files, g),
        pr,
        RecordingControl::default(),
        move |p, c| ymodem_recv(p, c, &opts, g),
    )
}

fn check_batch(out: &Path, files: &[(String, Vec<u8>)], summary: &Summary) {
    assert_eq!(summary.files.len(), files.len());
    for ((name, data), (got_name, bytes, path)) in files.iter().zip(&summary.files) {
        assert_eq!(got_name, name);
        assert_eq!(*bytes, data.len() as u64, "{name}");
        assert_eq!(path, &out.join(name));
        assert_eq!(&std::fs::read(path).unwrap(), data, "{name}");
    }
}

#[test]
fn ymodem_and_ymodem_g_batch() {
    for g in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let mut files = test_files();
        // Unlike XMODEM, YMODEM keeps trailing SUB bytes (exact size).
        files.push(("subtail.bin".into(), vec![1, 2, 3, 0x1A, 0x1A]));
        let paths = write_files(&src, &files);
        let out = dir.path().join("out");
        let (a, b) = ChannelPort::pair();
        let ((sr, sc), (rr, rc)) = y_roundtrip(a, b, paths.clone(), &out, g);
        let ss = sr.unwrap_or_else(|e| panic!("g={g} send: {e}"));
        let rs = rr.unwrap_or_else(|e| panic!("g={g} recv: {e}"));
        assert_eq!(ss.files.len(), files.len());
        check_batch(&out, &files, &rs);
        assert_eq!(count_retries(&sc.events), 0, "{:?}", sc.events);
        assert_eq!(count_retries(&rc.events), 0, "{:?}", rc.events);
        // Sizes are announced and the mtime is carried over.
        assert!(rc.events.contains(&Event::FileStart { name: "big.bin".into(), size: Some(1_500_000) }));
        let m_src = std::fs::metadata(&paths[0]).unwrap().modified().unwrap();
        let m_dst = std::fs::metadata(out.join(&files[0].0)).unwrap().modified().unwrap();
        let delta = m_src.duration_since(m_dst).unwrap_or_else(|e| e.duration());
        assert!(delta < Duration::from_secs(1), "mtime not preserved: {delta:?}");
    }
}

#[test]
fn xmodem_recovers_from_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let data = random_bytes(40_000, 11);
    let src = dir.path().join("src.bin");
    std::fs::write(&src, &data).unwrap();
    for v in [XVariant::Checksum, XVariant::Crc, XVariant::OneK] {
        let dest = dir.path().join(format!("out-{v:?}"));
        let (a, b) = ChannelPort::pair();
        let mut fa = Faulty::new(a);
        fa.corrupt_every = Some(13);
        let mut fb = Faulty::new(b);
        fb.ack_to_nak = vec![3, 10, 25];
        let (s2, d2) = (src.clone(), dest.clone());
        let ((sr, sc), (rr, rc)) = run_pair(
            fa,
            RecordingControl::default(),
            move |p, c| xmodem_send(p, c, &s2, v),
            fb,
            RecordingControl::default(),
            move |p, c| xmodem_recv(p, c, &d2, v),
        );
        sr.unwrap_or_else(|e| panic!("{v:?} send: {e}"));
        rr.unwrap_or_else(|e| panic!("{v:?} recv: {e}"));
        assert_eq!(std::fs::read(&dest).unwrap(), data, "{v:?}");
        assert!(count_retries(&sc.events) >= 3, "sender saw NAKs");
        assert!(count_retries(&rc.events) >= 3, "receiver saw bad blocks");
    }
}

#[test]
fn ymodem_recovers_from_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let files = vec![
        ("a.bin".to_string(), random_bytes(50_000, 21)),
        ("b.bin".to_string(), random_bytes(3000, 22)),
        ("c.bin".to_string(), Vec::new()),
    ];
    let paths = write_files(&src, &files);
    let out = dir.path().join("out");
    let (a, b) = ChannelPort::pair();
    let mut fa = Faulty::new(a);
    fa.corrupt_every = Some(5);
    let mut fb = Faulty::new(b);
    // ACK #1 is block 0's: exercises the repeated-header path.
    fb.ack_to_nak = vec![1, 6, 30];
    let ((sr, sc), (rr, rc)) = y_roundtrip(fa, fb, paths, &out, false);
    sr.unwrap();
    let rs = rr.unwrap();
    check_batch(&out, &files, &rs);
    assert!(count_retries(&sc.events) >= 3);
    assert!(count_retries(&rc.events) >= 3);
}

#[test]
fn ymodem_g_aborts_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.bin");
    std::fs::write(&src, random_bytes(50_000, 31)).unwrap();
    let (a, b) = ChannelPort::pair();
    let mut fa = Faulty::new(a);
    fa.corrupt_every = Some(10);
    let ((sr, _), (rr, _)) = y_roundtrip(fa, b, vec![src], &dir.path().join("out"), true);
    assert!(matches!(rr, Err(XferError::Protocol(_))), "{rr:?}");
    assert!(matches!(sr, Err(XferError::RemoteCancelled)), "{sr:?}");
}

fn big_file(dir: &Path) -> PathBuf {
    let p = dir.join("big.bin");
    std::fs::write(&p, random_bytes(300_000, 41)).unwrap();
    p
}

#[test]
fn sender_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let src = big_file(dir.path());
    // XMODEM.
    let (a, b) = ChannelPort::pair();
    let (s2, dest) = (src.clone(), dir.path().join("x.out"));
    let ((sr, _), (rr, _)) = run_pair(
        a,
        CancelAfter::new(1),
        move |p, c| xmodem_send(p, c, &s2, XVariant::Crc),
        b,
        RecordingControl::default(),
        move |p, c| xmodem_recv(p, c, &dest, XVariant::Crc),
    );
    assert!(matches!(sr, Err(XferError::Cancelled)), "{sr:?}");
    assert!(matches!(rr, Err(XferError::RemoteCancelled)), "{rr:?}");
    // YMODEM and YMODEM-G.
    for g in [false, true] {
        let (a, b) = ChannelPort::pair();
        let files = vec![src.clone()];
        let opts = RecvOptions::new(dir.path().join(format!("y{g}")));
        let ((sr, _), (rr, _)) = run_pair(
            a,
            CancelAfter::new(1),
            move |p, c| ymodem_send(p, c, &files, g),
            b,
            RecordingControl::default(),
            move |p, c| ymodem_recv(p, c, &opts, g),
        );
        assert!(matches!(sr, Err(XferError::Cancelled)), "g={g} {sr:?}");
        assert!(matches!(rr, Err(XferError::RemoteCancelled)), "g={g} {rr:?}");
    }
}

#[test]
fn receiver_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let src = big_file(dir.path());
    let (a, b) = ChannelPort::pair();
    let (s2, dest) = (src.clone(), dir.path().join("x.out"));
    let ((sr, _), (rr, _)) = run_pair(
        a,
        RecordingControl::default(),
        move |p, c| xmodem_send(p, c, &s2, XVariant::OneK),
        b,
        CancelAfter::new(1),
        move |p, c| xmodem_recv(p, c, &dest, XVariant::OneK),
    );
    assert!(matches!(rr, Err(XferError::Cancelled)), "{rr:?}");
    assert!(matches!(sr, Err(XferError::RemoteCancelled)), "{sr:?}");
    for g in [false, true] {
        let (a, b) = ChannelPort::pair();
        let files = vec![src.clone()];
        let opts = RecvOptions::new(dir.path().join(format!("y{g}")));
        let ((sr, _), (rr, _)) = run_pair(
            a,
            RecordingControl::default(),
            move |p, c| ymodem_send(p, c, &files, g),
            b,
            CancelAfter::new(1),
            move |p, c| ymodem_recv(p, c, &opts, g),
        );
        assert!(matches!(rr, Err(XferError::Cancelled)), "g={g} {rr:?}");
        assert!(matches!(sr, Err(XferError::RemoteCancelled)), "g={g} {sr:?}");
    }
}

#[test]
fn cancel_before_start() {
    // Cancelling while still waiting for the other side returns promptly.
    let dir = tempfile::tempdir().unwrap();
    let src = big_file(dir.path());
    let (mut a, _b) = ChannelPort::pair();
    let mut ctl = RecordingControl::default();
    ctl.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::Instant::now();
    let r = xmodem_send(&mut a, &mut ctl, &src, XVariant::Crc);
    assert!(matches!(r, Err(XferError::Cancelled)), "{r:?}");
    let r = ymodem_recv(&mut a, &mut ctl, &RecvOptions::new(dir.path().join("o")), false);
    assert!(matches!(r, Err(XferError::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn ymodem_sender_starts_late() {
    // The receiver's repeated 'C's queue up; the sender must not mistake the
    // extras for replies.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let files =
        vec![("late.bin".to_string(), random_bytes(20_000, 51)), ("two.bin".to_string(), random_bytes(700, 52))];
    let paths = write_files(&src, &files);
    let out = dir.path().join("out");
    let opts = RecvOptions::new(&out);
    let (a, b) = ChannelPort::pair();
    let ((sr, _), (rr, _)) = run_pair(
        a,
        RecordingControl::default(),
        move |p, c| {
            thread::sleep(Duration::from_millis(6500));
            ymodem_send(p, c, &paths, false)
        },
        b,
        RecordingControl::default(),
        move |p, c| ymodem_recv(p, c, &opts, false),
    );
    sr.unwrap();
    check_batch(&out, &files, &rr.unwrap());
}

//! Interoperability with C-Kermit, run on the slave side of a PTY.
//! Skipped (with a message) when `kermit` is not installed.

use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};

use std::time::{Duration, Instant};

use xfer_common::{Event, Port, RecordingControl, RecvOptions, Result};
use xfer_kermit::{Detector, EightBitPrefix, KermitOptions};

use super::common::{assert_same_files, make_files, random_bytes, FaultyPort};
use super::pty::{spawn, PtyPort};

fn kermit() -> Option<String> {
    let candidates = ["/opt/homebrew/bin/kermit", "/usr/local/bin/kermit", "/usr/bin/kermit"];
    if let Some(c) = candidates.iter().find(|c| Path::new(c).exists()) {
        return Some((*c).to_string());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join("kermit"))
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

macro_rules! need_kermit {
    () => {
        match kermit() {
            Some(k) => k,
            None => {
                eprintln!("C-Kermit not installed; skipping interop test");
                return;
            }
        }
    };
}

/// Common C-Kermit flags: no init file, literal file names, binary mode, quiet.
const BASE: [&str; 4] = ["-Y", "-P", "-i", "-q"];

/// Build C-Kermit's argument list. Entries of `extra` that start with `set `
/// are written to a command file in `cwd` (C-Kermit runs a command file given
/// as the first argument before acting on the options).
fn args(cwd: &Path, extra: &[&str], action: &[&str]) -> Vec<String> {
    let (cmds, flags): (Vec<&str>, Vec<&str>) = extra.iter().partition(|a| a.starts_with("set "));
    let mut v = Vec::new();
    if !cmds.is_empty() {
        let script = cwd.join("setup.ksc");
        std::fs::write(&script, cmds.join("\n") + "\n").unwrap();
        v.push(script.to_string_lossy().into_owned());
    }
    v.extend(BASE.iter().chain(&flags).chain(action).map(|s| s.to_string()));
    v
}

/// Wait for the child to exit, draining the PTY meanwhile.
fn finish(port: &mut PtyPort, child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = [0u8; 4096];
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("kermit did not exit");
        }
        if port.read_timeout(&mut buf, Duration::from_millis(50)).is_err() {
            return child.wait().unwrap();
        }
    }
}

fn opts() -> KermitOptions {
    KermitOptions { timeout: Duration::from_secs(2), ..KermitOptions::default() }
}

fn ok_or_dump<T>(r: Result<T>, ctl: &RecordingControl) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("transfer failed: {e}; last events: {:?}", &ctl.events[ctl.events.len().saturating_sub(15)..]),
    }
}

fn negotiated(ctl: &RecordingControl) -> String {
    ctl.events
        .iter()
        .find_map(|e| match e {
            Event::Message(m) => Some(m.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Copies C-Kermit's debug log out of the temp dir when dropped.
struct DebugCopy(Option<PathBuf>, PathBuf);

impl Drop for DebugCopy {
    fn drop(&mut self) {
        if let Some(to) = &self.0 {
            let _ = std::fs::copy(&self.1, to);
        }
    }
}

/// Our sender -> `kermit -r`.
fn send_to_kermit(extra: &[&str], o: &KermitOptions, big: usize) -> String {
    send_to_kermit_faulty(extra, o, big, (0, 0))
}

/// Our sender -> `kermit -r`, dropping / corrupting `faults` per mille of our packets.
fn send_to_kermit_faulty(extra: &[&str], o: &KermitOptions, big: usize, faults: (u64, u64)) -> String {
    let k = kermit().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), big);
    // C-Kermit 9.0.302's zchko() (ckufio.c, NOUUCP builds such as Homebrew's)
    // rejects new files with "Write access denied": when open(O_WRONLY) of a
    // not-yet-existing file fails it sets x = -1 and then indexes s[-1]. The
    // check passes when the file already exists, so pre-create placeholders
    // and let it overwrite them (-w).
    for f in &files {
        std::fs::write(dst.path().join(f.file_name().unwrap()), b"").unwrap();
    }
    let mut extra = extra.to_vec();
    extra.push("-w");
    let dbg = std::env::var_os("KDEBUG").map(PathBuf::from);
    if dbg.is_some() {
        extra.push("-d");
    }
    let (port, mut child) = spawn(&k, &args(dst.path(), &extra, &["-r"]), dst.path()).unwrap();
    let _keep = DebugCopy(dbg, dst.path().join("debug.log"));
    let mut port = FaultyPort::new(port, 99);
    (port.drop_per_mille, port.corrupt_per_mille) = faults;
    let mut ctl = RecordingControl::default();
    let start = Instant::now();
    let res = xfer_kermit::send(&mut port, &mut ctl, &files, o);
    let elapsed = start.elapsed();
    let summary = ok_or_dump(res, &ctl);
    let status = finish(&mut port.inner, &mut child);
    assert!(status.success(), "kermit -r exited with {status}");
    assert_eq!(summary.files.len(), files.len());
    assert_same_files(&files, dst.path());
    let n = negotiated(&ctl);
    let retries = ctl.events.iter().filter(|e| matches!(e, Event::Retry { .. })).count();
    eprintln!(
        "send -> kermit -r {extra:?}: {n}; {elapsed:?}; {} dropped, {} corrupted, {retries} retries",
        port.dropped, port.corrupted
    );
    if faults != (0, 0) {
        assert!(port.dropped + port.corrupted > 0 && retries > 0);
    }
    n
}

/// How the receiving side gets started.
#[derive(Clone, Copy, Debug)]
enum Start {
    /// Call `recv` straight away (it NAKs to prompt the Send-Init).
    Direct,
    /// Let the detector consume the Send-Init, then call `recv` (NAK path).
    DetectThenRecv,
    /// Let the detector consume the Send-Init and hand it to `recv_with_init`.
    DetectWithInit,
}

/// `kermit -s files` -> our receiver.
fn recv_from_kermit(extra: &[&str], o: &KermitOptions, big: usize, start: Start) -> String {
    recv_from_kermit_faulty(extra, o, big, start, (0, 0))
}

fn recv_from_kermit_faulty(extra: &[&str], o: &KermitOptions, big: usize, start: Start, faults: (u64, u64)) -> String {
    let k = kermit().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), big);
    let names: Vec<String> = files.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
    let mut action: Vec<&str> = vec!["-s"];
    action.extend(names.iter().map(String::as_str));
    let (port, mut child) = spawn(&k, &args(src.path(), extra, &action), src.path()).unwrap();
    let mut port = FaultyPort::new(port, 77);
    (port.drop_per_mille, port.corrupt_per_mille) = faults;
    let mut ctl = RecordingControl::default();
    let ropts = RecvOptions::new(dst.path());
    let t0 = Instant::now();
    let res = match start {
        Start::Direct => xfer_kermit::recv(&mut port, &mut ctl, &ropts, o),
        Start::DetectThenRecv | Start::DetectWithInit => {
            let init = detect(&mut port.inner);
            if matches!(start, Start::DetectWithInit) {
                xfer_kermit::recv_with_init(&mut port, &mut ctl, &ropts, o, &init)
            } else {
                xfer_kermit::recv(&mut port, &mut ctl, &ropts, o)
            }
        }
    };
    let elapsed = t0.elapsed();
    let summary = ok_or_dump(res, &ctl);
    let status = finish(&mut port.inner, &mut child);
    assert!(status.success(), "kermit -s exited with {status}");
    assert_eq!(summary.files.len(), files.len());
    assert_same_files(&files, dst.path());
    let n = negotiated(&ctl);
    eprintln!(
        "kermit -s {extra:?} -> recv ({start:?}): {n}; {elapsed:?}; {} dropped, {} corrupted",
        port.dropped, port.corrupted
    );
    if faults != (0, 0) {
        assert!(port.dropped + port.corrupted > 0);
    }
    n
}

/// Feed terminal output through the detector until a Send-Init shows up.
fn detect(port: &mut PtyPort) -> Vec<u8> {
    let mut d = Detector::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let n = port.read_timeout(&mut buf, Duration::from_millis(100)).unwrap();
        for &b in &buf[..n] {
            if d.feed(b) {
                return d.packet().to_vec();
            }
        }
    }
    panic!("no Send-Init detected");
}

#[test]
fn send_to_ckermit_defaults() {
    let _k = need_kermit!();
    let n = send_to_kermit(&[], &opts(), 2 * 1024 * 1024);
    assert!(n.contains("block check 3"), "{n}");
}

#[test]
fn recv_from_ckermit_direct() {
    let _k = need_kermit!();
    recv_from_kermit(&[], &opts(), 2 * 1024 * 1024, Start::Direct);
}

#[test]
fn recv_from_ckermit_after_detection() {
    let _k = need_kermit!();
    recv_from_kermit(&[], &opts(), 300_000, Start::DetectThenRecv);
    recv_from_kermit(&[], &opts(), 300_000, Start::DetectWithInit);
}

#[test]
fn send_to_ckermit_block_check_1_short_packets_stop_and_wait() {
    let _k = need_kermit!();
    let o = KermitOptions { block_check: 1, long_packets: false, max_packet_len: 94, window: 1, ..opts() };
    let n = send_to_kermit(&[], &o, 100_000);
    assert!(n.contains("block check 1") && n.contains("window 1,") && n.contains("length 94"), "{n}");
}

#[test]
fn send_to_ckermit_block_check_2_eight_bit_prefixing() {
    let _k = need_kermit!();
    let o = KermitOptions {
        block_check: 2,
        eight_bit: EightBitPrefix::Always,
        repeat_counts: false,
        max_packet_len: 1000,
        window: 8,
        ..opts()
    };
    let n = send_to_kermit(&[], &o, 300_000);
    assert!(n.contains("block check 2") && n.contains("window 8") && n.contains("8th-bit"), "{n}");
    assert!(!n.contains("repeat"), "{n}");
}

#[test]
fn send_to_ckermit_with_its_limits() {
    let _k = need_kermit!();
    // C-Kermit asks for a 4-slot window and 500-byte packets.
    let n = send_to_kermit(&["-v", "4", "-e", "500"], &opts(), 500_000);
    // (C-Kermit announces one less than the -e value.)
    assert!(n.contains("window 4") && (n.contains("length 499") || n.contains("length 500")), "{n}");
}

#[test]
fn recv_from_ckermit_block_check_and_eight_bit() {
    let _k = need_kermit!();
    let o = KermitOptions { eight_bit: EightBitPrefix::Always, max_packet_len: 700, window: 5, ..opts() };
    let n = recv_from_kermit(&["set block-check 2"], &o, 300_000, Start::Direct);
    assert!(n.contains("block check 2") && n.contains("window 5") && n.contains("8th-bit"), "{n}");
    let o = KermitOptions { long_packets: false, max_packet_len: 90, window: 1, ..opts() };
    let n = recv_from_kermit(&["set block-check 1"], &o, 100_000, Start::Direct);
    assert!(n.contains("block check 1") && n.contains("window 1"), "{n}");
}

#[test]
fn recv_from_ckermit_window_limited_by_ckermit() {
    let _k = need_kermit!();
    let n = recv_from_kermit(&["-v", "3"], &opts(), 500_000, Start::DetectWithInit);
    assert!(n.contains("window 3"), "{n}");
}

#[test]
fn recv_from_ckermit_many_files_in_one_batch() {
    let _k = need_kermit!();
    let k = kermit().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for i in 0..12 {
        let p = src.path().join(format!("f{i:02}.bin"));
        std::fs::write(&p, random_bytes(i, (i as usize) * 7919)).unwrap();
        files.push(p);
    }
    let (mut port, mut child) = spawn(&k, &args(src.path(), &[], &["-s", "f*.bin"]), src.path()).unwrap();
    let mut ctl = RecordingControl::default();
    let res = xfer_kermit::recv(&mut port, &mut ctl, &RecvOptions::new(dst.path()), &opts());
    let summary = ok_or_dump(res, &ctl);
    assert!(finish(&mut port, &mut child).success());
    assert_eq!(summary.files.len(), files.len());
    assert_same_files(&files, dst.path());
    // Sizes come through C-Kermit's attribute packets.
    let sized = ctl.events.iter().filter(|e| matches!(e, Event::FileStart { size: Some(_), .. })).count();
    assert_eq!(sized, files.len());
}

#[test]
fn ckermit_error_packet_is_reported() {
    let _k = need_kermit!();
    let k = kermit().unwrap();
    let src = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), 1000);
    // Without placeholders, C-Kermit 9.0.302 refuses every new file (see
    // `send_to_kermit`): a handy way to get a genuine E packet.
    let dst = tempfile::tempdir().unwrap();
    let (mut port, mut child) = spawn(&k, &args(dst.path(), &[], &["-r"]), dst.path()).unwrap();
    let mut ctl = RecordingControl::default();
    let res = xfer_kermit::send(&mut port, &mut ctl, &files, &opts());
    let _ = finish(&mut port, &mut child);
    match res {
        Err(xfer_common::XferError::Protocol(m)) => assert!(!m.is_empty()),
        other => {
            // A C-Kermit build without the bug simply succeeds.
            let summary = other.expect("transfer");
            assert_eq!(summary.files.len(), files.len());
        }
    }
}

#[test]
fn send_to_ckermit_over_lossy_link() {
    let _k = need_kermit!();
    let o = KermitOptions { retry_limit: 20, max_packet_len: 1000, ..opts() };
    send_to_kermit_faulty(&[], &o, 400_000, (20, 20));
    let o = KermitOptions { window: 1, timeout: Duration::from_secs(1), ..o };
    send_to_kermit_faulty(&[], &o, 40_000, (20, 20));
}

#[test]
fn recv_from_ckermit_over_lossy_link() {
    let _k = need_kermit!();
    let o = KermitOptions { retry_limit: 20, max_packet_len: 1000, ..opts() };
    recv_from_kermit_faulty(&[], &o, 150_000, Start::Direct, (20, 20));
}

#[test]
fn ckermit_refusing_files_skips_them() {
    let _k = need_kermit!();
    let k = kermit().unwrap();
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = make_files(src.path(), 50_000);
    for f in &files {
        std::fs::write(dst.path().join(f.file_name().unwrap()), b"old").unwrap();
    }
    let argv = args(dst.path(), &["set file collision discard"], &["-r"]);
    let (mut port, mut child) = spawn(&k, &argv, dst.path()).unwrap();
    let mut ctl = RecordingControl::default();
    let res = xfer_kermit::send(&mut port, &mut ctl, &files, &opts());
    let summary = ok_or_dump(res, &ctl);
    let _ = finish(&mut port, &mut child);
    assert!(summary.files.is_empty(), "{summary:?}");
    let skipped = ctl.events.iter().filter(|e| matches!(e, Event::FileSkipped { .. })).count();
    assert_eq!(skipped, files.len());
    for f in &files {
        assert_eq!(std::fs::read(dst.path().join(f.file_name().unwrap())).unwrap(), b"old");
    }
}

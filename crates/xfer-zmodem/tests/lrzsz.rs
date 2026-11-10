//! Interoperability with lrzsz `sz`/`rz` over pipes. Skipped (with a note)
//! when lrzsz is not installed.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use xfer_common::{ChannelPort, Event, RecordingControl, RecvOptions};
use xfer_zmodem::{recv_with_options, send_with_options, Options};

fn find_tool(names: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    for name in names {
        for d in &dirs {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn tool(names: &[&str]) -> Option<PathBuf> {
    let t = find_tool(names);
    if t.is_none() {
        eprintln!("NOTE: lrzsz ({}) not found; skipping interop test", names.join("/"));
    }
    t
}

fn wait_child(child: &mut Child, log: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "lrzsz exited with {status}; log:\n{}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("lrzsz did not exit; log:\n{}", std::fs::read_to_string(log).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn opts() -> Options {
    Options { timeout: Duration::from_secs(5), ..Options::default() }
}

/// Our sender → lrzsz `rz <args>` running in a fresh directory.
fn send_to_rz(args: &[&str], sopts: Options) {
    let Some(rz) = tool(&["rz", "lrz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    let log = src.path().join("rz.log");
    let mut cmd = Command::new(rz);
    cmd.args(args).current_dir(dst.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    let mut ctl = RecordingControl::default();
    let r = send_with_options(&mut port, &mut ctl, &files, &sopts);
    let summary = r.unwrap_or_else(|e| {
        let _ = child.kill();
        panic!("send failed: {e}; rz log:\n{}", std::fs::read_to_string(&log).unwrap_or_default())
    });
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), files.len());
    assert_same(&files, dst.path());
    let retries = ctl.events.iter().filter(|e| matches!(e, Event::Retry { .. })).count();
    eprintln!("send_to_rz {args:?}: ok, {retries} retries");
}

/// lrzsz `sz <args> files` → our receiver.
fn recv_from_sz(args: &[&str], ropts: Options) {
    let Some(sz) = tool(&["sz", "lsz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    let log = src.path().join("sz.log");
    let mut cmd = Command::new(sz);
    cmd.args(args).args(&files).current_dir(src.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    let mut ctl = RecordingControl::default();
    let r = recv_with_options(&mut port, &mut ctl, &RecvOptions::new(dst.path()), &ropts);
    let summary = r.unwrap_or_else(|e| {
        let _ = child.kill();
        panic!("recv failed: {e}; sz log:\n{}", std::fs::read_to_string(&log).unwrap_or_default())
    });
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), files.len());
    assert_same(&files, dst.path());
    // lrzsz sends the modification time; check it survived.
    for f in &files {
        let want = std::fs::metadata(f).unwrap().modified().unwrap();
        let got = std::fs::metadata(dst.path().join(f.file_name().unwrap())).unwrap().modified().unwrap();
        let diff = want.duration_since(got).unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_secs(2), "mtime of {f:?} not preserved");
    }
    eprintln!("recv_from_sz {args:?}: ok");
}

#[test]
fn send_to_rz_default() {
    send_to_rz(&["-b", "-y"], opts());
}

#[test]
fn send_to_rz_crc16_small_blocks() {
    send_to_rz(&["-b", "-y"], Options { use_crc32: false, max_block: 1024, ..opts() });
}

#[test]
fn send_to_rz_escape_ctl() {
    // Receiver asks for control-character escaping.
    send_to_rz(&["-b", "-y", "-e"], opts());
    // Sender asks for it via ZSINIT.
    send_to_rz(&["-b", "-y"], Options { escape_ctl: true, ..opts() });
}

#[test]
fn send_to_rz_with_injected_errors() {
    // rz deliberately fakes a CRC error every N bytes, forcing ZRPOS recovery.
    send_to_rz(&["-b", "-y", "--errors", "300000"], opts());
}

#[test]
fn send_to_rz_with_windowsize() {
    send_to_rz(&["-b", "-y", "-w", "16384"], opts());
}

#[test]
fn recv_from_sz_default() {
    recv_from_sz(&["-b"], opts());
}

#[test]
fn recv_from_sz_8k_blocks() {
    recv_from_sz(&["-b", "-8"], opts());
}

#[test]
fn recv_from_sz_crc16() {
    recv_from_sz(&["-b", "-o"], opts());
}

#[test]
fn recv_from_sz_escape_ctl() {
    recv_from_sz(&["-b", "-e"], opts());
}

#[test]
fn recv_from_sz_we_request_escape_and_crc16() {
    recv_from_sz(&["-b"], Options { escape_ctl: true, use_crc32: false, ..opts() });
}

#[test]
fn recv_from_sz_windowed() {
    recv_from_sz(&["-b", "-w", "8192"], opts());
    recv_from_sz(&["-b", "-8"], Options { rx_buffer_size: 16384, ..opts() });
}

/// Wraps a port and flips a byte in the *incoming* stream every `interval`
/// bytes, so our receiver must recover from errors in lrzsz's data.
struct CorruptReads {
    inner: ChannelPort,
    seen: u64,
    next: u64,
    interval: u64,
    hits: usize,
}

impl xfer_common::Port for CorruptReads {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        let n = self.inner.read_timeout(buf, timeout)?;
        let end = self.seen + n as u64;
        if n > 0 && end > self.next {
            buf[(self.next - self.seen) as usize] ^= 0x21;
            self.next += self.interval;
            self.hits += 1;
        }
        self.seen = end;
        Ok(n)
    }
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(data)
    }
    fn purge_input(&mut self) {
        self.inner.purge_input();
    }
}

#[test]
fn recv_from_sz_with_line_noise() {
    let Some(sz) = tool(&["sz", "lsz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = standard_files(src.path());
    let log = src.path().join("sz.log");
    let mut cmd = Command::new(sz);
    cmd.arg("-b").args(&files).current_dir(src.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    let mut port = CorruptReads { inner: port, seen: 0, next: 20_000, interval: 250_000, hits: 0 };
    let mut ctl = RecordingControl::default();
    let r = recv_with_options(&mut port, &mut ctl, &RecvOptions::new(dst.path()), &opts());
    let summary = r.unwrap_or_else(|e| {
        let _ = child.kill();
        panic!("recv failed: {e}; sz log:\n{}", std::fs::read_to_string(&log).unwrap_or_default())
    });
    let hits = port.hits;
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), files.len());
    assert_same(&files, dst.path());
    let retries = ctl.events.iter().filter(|e| matches!(e, Event::Retry { .. })).count();
    assert!(hits >= 5 && retries >= 5, "hits={hits} retries={retries}");
    eprintln!("recv_from_sz with noise: ok, {hits} corruptions, {retries} retries");
}

/// Feed the remote's output one byte at a time to the detector, the way the
/// terminal would, then hand the same stream to the transfer.
fn detect_on(port: &mut ChannelPort) -> xfer_zmodem::Detected {
    use xfer_common::Port;
    let mut det = xfer_zmodem::Detector::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut b = [0u8; 1];
    while Instant::now() < deadline {
        if port.read_timeout(&mut b, Duration::from_millis(100)).unwrap() == 1 {
            if let Some(d) = det.feed(b[0]) {
                return d;
            }
        }
    }
    panic!("no ZMODEM header detected");
}

#[test]
fn autodetect_sz_then_recv() {
    let Some(sz) = tool(&["sz", "lsz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = vec![write_file(src.path(), "auto.bin", &random_bytes(200_000, 5))];
    let log = src.path().join("sz.log");
    let mut cmd = Command::new(sz);
    cmd.args(&files).current_dir(src.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    assert_eq!(detect_on(&mut port), xfer_zmodem::Detected::RemoteSending);
    let mut ctl = RecordingControl::default();
    let summary = xfer_zmodem::recv(&mut port, &mut ctl, &RecvOptions::new(dst.path())).unwrap();
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), 1);
    assert_same(&files, dst.path());
}

#[test]
fn autodetect_rz_then_send() {
    let Some(rz) = tool(&["rz", "lrz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = vec![write_file(src.path(), "auto.bin", &random_bytes(200_000, 6))];
    let log = src.path().join("rz.log");
    let mut cmd = Command::new(rz);
    cmd.current_dir(dst.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    assert_eq!(detect_on(&mut port), xfer_zmodem::Detected::RemoteReceiving);
    let mut ctl = RecordingControl::default();
    let summary = xfer_zmodem::send(&mut port, &mut ctl, &files).unwrap();
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), 1);
    assert_same(&files, dst.path());
}

#[test]
fn sz_remote_command_is_refused() {
    let Some(sz) = tool(&["sz", "lsz"]) else {
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let marker = work.path().join("PWNED");
    let mut cmd = Command::new(sz);
    cmd.arg("-c").arg(format!("touch {}", marker.display())).current_dir(work.path()).stderr(Stdio::null());
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    let mut ctl = RecordingControl::default();
    let summary = recv_with_options(&mut port, &mut ctl, &RecvOptions::new(dst.path()), &opts()).unwrap();
    drop(port);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    assert!(summary.files.is_empty());
    assert!(!marker.exists(), "remote command was executed!");
    assert!(ctl.events.iter().any(|e| matches!(e, Event::Message(m) if m.contains("refused"))));
}

#[test]
fn send_to_rz_resume() {
    let Some(rz) = tool(&["rz", "lrz"]) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = random_bytes(2_000_000, 21);
    let files = vec![write_file(src.path(), "resume.bin", &data)];
    // A partial copy already exists on the receiving side.
    write_file(dst.path(), "resume.bin", &data[..700_000]);
    let log = src.path().join("rz.log");
    let mut cmd = Command::new(rz);
    cmd.args(["-b", "-r"]).current_dir(dst.path()).stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
    let (mut port, mut child) = ChannelPort::spawn(&mut cmd).unwrap();
    let mut ctl = RecordingControl::default();
    let summary = send_with_options(&mut port, &mut ctl, &files, &opts()).unwrap();
    drop(port);
    wait_child(&mut child, &log);
    assert_eq!(summary.files.len(), 1);
    assert_same(&files, dst.path());
    let first = ctl.events.iter().find_map(|e| match e {
        Event::Progress { bytes } => Some(*bytes),
        _ => None,
    });
    eprintln!("resume: first progress report at {first:?}");
    assert!(first.is_some_and(|b| b > 700_000), "transfer did not resume: {first:?}");
}

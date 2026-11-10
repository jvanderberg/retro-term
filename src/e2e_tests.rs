//! End-to-end tests: a real `Session` running lrzsz / C-Kermit in a PTY,
//! exercising auto-detection, byte routing and the protocol crates together.
//! Tests skip themselves when the external programs aren't installed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xfer_common::Protocol;

use crate::conn::ConnSpec;
use crate::session::{Session, SessionEvent};
use crate::term::{Emulator, Scheme};
use crate::transfer::{Detected, Direction, TransferJob};

fn which(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(prog)).find(|p| p.is_file())
}

fn test_data(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

fn start(command: String) -> Session {
    start_with(command, Arc::new(Mutex::new(Emulator::new(80, 24, Scheme::Vga))))
}

fn start_with(command: String, emu: Arc<Mutex<Emulator>>) -> Session {
    let s = Session::connect(
        ConnSpec::Shell { command: Some(command) },
        emu,
        "xterm-256color".into(),
        egui::Context::default(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match s.events.recv_timeout(Duration::from_millis(100)) {
            Ok(SessionEvent::Connected) => return s,
            Ok(SessionEvent::Disconnected(r)) => panic!("could not start: {r}"),
            _ if Instant::now() > deadline => panic!("timed out starting session"),
            _ => {}
        }
    }
}

fn wait_detect(s: &Session) -> Detected {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match s.events.recv_timeout(Duration::from_millis(100)) {
            Ok(SessionEvent::AutoDetect(d)) => return d,
            Ok(SessionEvent::Disconnected(r)) => panic!("disconnected before transfer: {r}"),
            _ if Instant::now() > deadline => panic!("no transfer detected"),
            _ => {}
        }
    }
}

fn run_job(s: &Session, protocol: Protocol, dir: Direction) {
    let mut job = TransferJob::start(s, protocol, dir);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !job.poll() {
        assert!(Instant::now() < deadline, "transfer timed out; log: {:?}", job.log);
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(Err(e)) = &job.result {
        panic!("{} failed: {e}; log: {:?}", protocol.name(), job.log);
    }
}

/// Wait until `path` exists with `len` bytes (the remote program may still be
/// closing it when the protocol finishes).
fn wait_file(path: &Path, len: usize) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(data) = std::fs::read(path) {
            if data.len() == len {
                return data;
            }
        }
        assert!(Instant::now() < deadline, "{} never reached {len} bytes", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn zmodem_download_autodetected_from_sz() {
    let Some(sz) = which("sz") else {
        eprintln!("skipping: sz not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = test_data(300_000, 1);
    let file = src.path().join("payload.bin");
    std::fs::write(&file, &data).unwrap();

    let s = start(format!("{} -b {}", sz.display(), file.display()));
    assert_eq!(wait_detect(&s), Detected::ZmodemDownload);
    run_job(&s, Protocol::Zmodem, Direction::Receive(dst.path().to_path_buf()));
    assert_eq!(wait_file(&dst.path().join("payload.bin"), data.len()), data);
}

/// Output the remote prints right after a transfer (the shell prompt) must
/// reach the terminal, not vanish into the finished transfer's channel.
#[test]
fn output_after_transfer_reaches_terminal() {
    let Some(sz) = which("sz") else {
        eprintln!("skipping: sz not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("small.bin");
    std::fs::write(&file, test_data(20_000, 7)).unwrap();
    let script = src.path().join("run.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n{} -b {}\necho AFTER-MARKER-1\nsleep 30\n", sz.display(), file.display()),
    )
    .unwrap();
    make_executable(&script);
    let emu = Arc::new(Mutex::new(Emulator::new(80, 24, Scheme::Vga)));
    let s = start_with(script.display().to_string(), emu.clone());
    assert_eq!(wait_detect(&s), Detected::ZmodemDownload);
    run_job(&s, Protocol::Zmodem, Direction::Receive(dst.path().to_path_buf()));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen: String = {
            let e = emu.lock().unwrap();
            (0..e.term.rows()).flat_map(|y| e.term.screen_row(y).cells.iter().map(|c| c.ch)).collect()
        };
        if screen.contains("AFTER-MARKER-1") {
            break;
        }
        assert!(Instant::now() < deadline, "marker never shown: {:?}", screen.trim());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn zmodem_upload_autodetected_from_rz() {
    let Some(rz) = which("rz") else {
        eprintln!("skipping: rz not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = test_data(250_000, 2);
    let file = src.path().join("upload.bin");
    std::fs::write(&file, &data).unwrap();

    // rz writes into its working directory; run it through sh to cd there.
    let script = dst.path().join("run.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncd {} && exec {} -b\n", dst.path().display(), rz.display())).unwrap();
    make_executable(&script);
    let s = start(script.display().to_string());
    assert_eq!(wait_detect(&s), Detected::ZmodemUpload);
    run_job(&s, Protocol::Zmodem, Direction::Send(vec![file]));
    assert_eq!(wait_file(&dst.path().join("upload.bin"), data.len()), data);
}

#[test]
fn ymodem_upload_to_rb() {
    let Some(rz) = which("rz") else {
        eprintln!("skipping: rz not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = test_data(70_000, 3);
    let file = src.path().join("y.bin");
    std::fs::write(&file, &data).unwrap();
    // lrzsz leaves IEXTEN on, and macOS then eats ^V/^O even in raw mode.
    // YMODEM can't escape them, so clear IEXTEN first.
    let script = dst.path().join("run.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nstty -iexten\ncd {} && exec {} --ymodem -b\n", dst.path().display(), rz.display()),
    )
    .unwrap();
    make_executable(&script);
    let s = start(script.display().to_string());
    run_job(&s, Protocol::Ymodem, Direction::Send(vec![file]));
    assert_eq!(wait_file(&dst.path().join("y.bin"), data.len()), data);
}

#[test]
fn xmodem_download_from_sx() {
    let Some(sz) = which("sz") else {
        eprintln!("skipping: sz not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // A multiple of 128 so XMODEM's padding doesn't change the length.
    let data = test_data(128 * 300, 4);
    let file = src.path().join("x.bin");
    std::fs::write(&file, &data).unwrap();
    let s = start(format!("{} --xmodem -b {}", sz.display(), file.display()));
    let out = dst.path().join("x.bin");
    run_job(&s, Protocol::XmodemCrc, Direction::Receive(out.clone()));
    assert_eq!(wait_file(&out, data.len()), data);
}

#[test]
fn kermit_download_autodetected() {
    let Some(kermit) = which("kermit") else {
        eprintln!("skipping: kermit not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = test_data(120_000, 5);
    let file = src.path().join("k.bin");
    std::fs::write(&file, &data).unwrap();
    let s = start(format!("{} -Y -i -q -s {}", kermit.display(), file.display()));
    assert_eq!(wait_detect(&s), Detected::KermitDownload);
    run_job(&s, Protocol::Kermit, Direction::Receive(dst.path().to_path_buf()));
    assert_eq!(wait_file(&dst.path().join("k.bin"), data.len()), data);
}

#[test]
fn kermit_upload() {
    let Some(kermit) = which("kermit") else {
        eprintln!("skipping: kermit not installed");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let data = test_data(90_000, 6);
    let file = src.path().join("ku.bin");
    std::fs::write(&file, &data).unwrap();
    // C-Kermit 9.0.302 refuses to create files that don't exist yet ("Write
    // access denied"); pre-create the target and let it overwrite (-w).
    std::fs::write(dst.path().join("ku.bin"), b"").unwrap();
    let script = dst.path().join("run.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncd {} && exec {} -Y -i -q -w -r\n", dst.path().display(), kermit.display()),
    )
    .unwrap();
    make_executable(&script);
    let s = start(script.display().to_string());
    run_job(&s, Protocol::Kermit, Direction::Send(vec![file]));
    assert_eq!(wait_file(&dst.path().join("ku.bin"), data.len()), data);
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(path).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(path, p).unwrap();
}

#[test]
fn telnet_negotiation_and_iac_escaping() {
    use crate::conn::telnet::{DO, IAC, OPT_ECHO, OPT_NAWS, OPT_TTYPE, SB, SE, WILL};
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        sock.write_all(&[IAC, DO, OPT_TTYPE, IAC, DO, OPT_NAWS, IAC, WILL, OPT_ECHO]).unwrap();
        sock.write_all(&[IAC, SB, OPT_TTYPE, 1, IAC, SE]).unwrap();
        sock.write_all(b"hello \xff\xff").unwrap();
        // Collect what the client says until we see its escaped 0xFF payload.
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while !got.windows(4).any(|w| w == [b'x', IAC, IAC, b'y']) {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        got
    });

    let mut e = Emulator::new(80, 24, Scheme::Vga);
    e.encoding = crate::term::Encoding::Latin1;
    let emu = Arc::new(Mutex::new(e));
    let s = Session::connect(
        ConnSpec::Telnet { host: "127.0.0.1".into(), port },
        emu.clone(),
        "ANSI".into(),
        egui::Context::default(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !s.is_connected() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    s.resize(100, 40, 0, 0);
    // Wait for the greeting, then send a byte that needs IAC doubling.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text: String = emu.lock().unwrap().term.screen_row(0).cells.iter().map(|c| c.ch).collect();
        if text.starts_with("hello ÿ") {
            break;
        }
        assert!(Instant::now() < deadline, "greeting not shown: {text:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    s.send(vec![b'x', 0xFF, b'y']);
    let got = server.join().unwrap();
    let has = |needle: &[u8]| got.windows(needle.len()).any(|w| w == needle);
    assert!(has(&[IAC, WILL, OPT_NAWS]), "{got:?}");
    assert!(has(&[IAC, SB, OPT_NAWS, 0, 100, 0, 40, IAC, SE]), "{got:?}");
    assert!(has(&[IAC, SB, OPT_TTYPE, 0, b'A', b'N', b'S', b'I', IAC, SE]), "{got:?}");
    assert!(has(&[IAC, DO, OPT_ECHO]), "{got:?}");
    assert!(has(&[b'x', IAC, IAC, b'y']), "{got:?}");
}

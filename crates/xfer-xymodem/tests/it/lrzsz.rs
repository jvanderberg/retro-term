//! Interoperability with lrzsz (`lsx`/`lrx`/`lsb`/`lrb`, or `sz`/`rz` with
//! `--xmodem`/`--ymodem`). Each test is skipped (with a note on stderr) when
//! no suitable binary is installed.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use xfer_common::{ChannelPort, RecordingControl, RecvOptions};
use xfer_xymodem::{xmodem_recv, xmodem_send, ymodem_recv, ymodem_send, XVariant};

use crate::common::{assert_padded_eq, random_bytes, test_files, write_files};

const SEARCH_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    SEARCH_DIRS.iter().map(PathBuf::from).chain(path_dirs).map(|d| d.join(name)).find(|p| p.is_file())
}

/// The first available program from `choices` (binary, fixed arguments).
fn find_tool(choices: &[(&str, &[&str])]) -> Option<(PathBuf, Vec<String>)> {
    choices
        .iter()
        .find_map(|(name, args)| find_in_path(name).map(|p| (p, args.iter().map(|s| s.to_string()).collect())))
}

fn x_sender() -> Option<(PathBuf, Vec<String>)> {
    find_tool(&[("lsx", &[]), ("sx", &[]), ("lsz", &["--xmodem"]), ("sz", &["--xmodem"])])
}
fn x_receiver() -> Option<(PathBuf, Vec<String>)> {
    find_tool(&[("lrx", &[]), ("rx", &[]), ("lrz", &["--xmodem"]), ("rz", &["--xmodem"])])
}
fn y_sender() -> Option<(PathBuf, Vec<String>)> {
    find_tool(&[("lsb", &[]), ("sb", &[]), ("lsz", &["--ymodem"]), ("sz", &["--ymodem"])])
}
fn y_receiver() -> Option<(PathBuf, Vec<String>)> {
    find_tool(&[("lrb", &[]), ("rb", &[]), ("lrz", &["--ymodem"]), ("rz", &["--ymodem"])])
}

macro_rules! require {
    ($tool:expr, $what:literal) => {
        match $tool {
            Some(t) => t,
            None => {
                eprintln!("skipping: no lrzsz {} found", $what);
                return;
            }
        }
    };
}

struct Proc {
    child: Child,
    log: PathBuf,
}

/// Start an lrzsz program in `cwd`, with its stderr captured to a log file.
fn spawn(tool: &(PathBuf, Vec<String>), extra: &[&str], cwd: &Path) -> (ChannelPort, Proc) {
    let log = cwd.with_extension("log");
    let mut cmd = Command::new(&tool.0);
    cmd.args(&tool.1).args(extra).current_dir(cwd).stderr(File::create(&log).unwrap());
    let (port, child) = ChannelPort::spawn(&mut cmd).expect("spawn lrzsz");
    (port, Proc { child, log })
}

impl Proc {
    /// Wait (bounded) for the program to exit successfully.
    fn finish(mut self, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break Some(s);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        assert!(status.is_some_and(|s| s.success()), "{what}: lrzsz exit {status:?}\n{log}");
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// Files that exercise block boundaries, kept modest for 128-byte XMODEM.
fn x_files() -> Vec<(String, Vec<u8>)> {
    test_files().into_iter().filter(|(n, _)| n != "empty.bin").collect()
}

#[test]
fn our_xmodem_send_to_lrzsz() {
    let rx = require!(x_receiver(), "XMODEM receiver");
    for v in [XVariant::Checksum, XVariant::Crc, XVariant::OneK] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let files = x_files();
        let paths = write_files(&src, &files);
        for ((name, data), path) in files.iter().zip(&paths) {
            let cwd = dir.path().join(format!("rx-{name}"));
            std::fs::create_dir(&cwd).unwrap();
            // lrzsz's XMODEM receiver uses checksum unless given -c.
            let mut args = vec![];
            if v != XVariant::Checksum {
                args.push("-c");
            }
            args.push("out.bin");
            let (mut port, proc_) = spawn(&rx, &args, &cwd);
            let mut ctl = RecordingControl::default();
            let r = xmodem_send(&mut port, &mut ctl, path, v);
            drop(port);
            let log = proc_.log();
            r.unwrap_or_else(|e| panic!("{v:?} {name}: {e}\n{log}"));
            proc_.finish(&format!("{v:?} {name}"));
            let got = std::fs::read(cwd.join("out.bin")).unwrap();
            let block = if v == XVariant::OneK { 1024 } else { 128 };
            assert_padded_eq(&got, data, block, &format!("{v:?} {name}"));
        }
    }
}

#[test]
fn lrzsz_xmodem_send_to_us() {
    let sx = require!(x_sender(), "XMODEM sender");
    for v in [XVariant::Checksum, XVariant::Crc, XVariant::OneK] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let files = x_files();
        let paths = write_files(&src, &files);
        for ((name, data), path) in files.iter().zip(&paths) {
            let mut args = vec![];
            if v == XVariant::OneK {
                args.push("-k");
            }
            let p = path.to_str().unwrap();
            args.push(p);
            let (mut port, proc_) = spawn(&sx, &args, dir.path());
            let dest = dir.path().join(format!("out-{name}"));
            let mut ctl = RecordingControl::default();
            let r = xmodem_recv(&mut port, &mut ctl, &dest, v);
            drop(port);
            let log = proc_.log();
            r.unwrap_or_else(|e| panic!("{v:?} {name}: {e}\n{log}"));
            proc_.finish(&format!("{v:?} {name}"));
            assert_eq!(&std::fs::read(&dest).unwrap(), data, "{v:?} {name}");
        }
    }
}

/// Lower-case names: lrzsz may change the case of all-caps names.
fn y_files() -> Vec<(String, Vec<u8>)> {
    let mut f = test_files();
    f.push(("subtail.bin".into(), vec![9, 8, 7, 0x1A, 0x1A, 0x1A]));
    f.push(("medium.dat".into(), random_bytes(70_001, 77)));
    f
}

#[test]
fn our_ymodem_send_to_lrzsz() {
    let rb = require!(y_receiver(), "YMODEM receiver");
    // `g` = true still works: lrzsz asks for 'C', so we fall back to YMODEM.
    for g in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let files = y_files();
        let paths = write_files(&src, &files);
        let cwd = dir.path().join("rb");
        std::fs::create_dir(&cwd).unwrap();
        let (mut port, proc_) = spawn(&rb, &[], &cwd);
        let mut ctl = RecordingControl::default();
        let r = ymodem_send(&mut port, &mut ctl, &paths, g);
        drop(port);
        let log = proc_.log();
        let s = r.unwrap_or_else(|e| panic!("g={g}: {e}\n{log}"));
        proc_.finish(&format!("ymodem g={g}"));
        assert_eq!(s.files.len(), files.len());
        for (name, data) in &files {
            let got = std::fs::read(cwd.join(name)).unwrap_or_else(|e| panic!("{name}: {e}\n{log}"));
            assert_eq!(&got, data, "{name}");
        }
    }
}

#[test]
fn lrzsz_ymodem_send_to_us() {
    let sb = require!(y_sender(), "YMODEM sender");
    // g = true: lrzsz's sender honours a 'G' request (YMODEM-G).
    for g in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let files = y_files();
        let paths = write_files(&src, &files);
        let args: Vec<&str> = paths.iter().map(|p| p.to_str().unwrap()).collect();
        let (mut port, proc_) = spawn(&sb, &args, dir.path());
        let out = dir.path().join("out");
        let mut ctl = RecordingControl::default();
        let r = ymodem_recv(&mut port, &mut ctl, &RecvOptions::new(&out), g);
        drop(port);
        let log = proc_.log();
        let s = r.unwrap_or_else(|e| panic!("g={g}: {e}\n{log}"));
        proc_.finish(&format!("ymodem g={g}"));
        assert_eq!(s.files.len(), files.len(), "{log}");
        for ((name, data), (got_name, bytes, path)) in files.iter().zip(&s.files) {
            assert_eq!(got_name, name);
            assert_eq!(*bytes, data.len() as u64);
            assert_eq!(&std::fs::read(path).unwrap(), data, "{name}");
        }
    }
}

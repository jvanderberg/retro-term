//! Minimal PTY wrapper: run a program on the slave side of a pseudo-terminal
//! (as its controlling terminal) and talk to it through the master as a
//! [`Port`].

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use xfer_common::Port;

pub struct PtyPort {
    master: OwnedFd,
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "pty closed")
}

impl Port for PtyPort {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let fd = self.master.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let mut ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        if ms == 0 && !timeout.is_zero() {
            ms = 1;
        }
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            return if errno() == libc::EINTR { Ok(0) } else { Err(io::Error::last_os_error()) };
        }
        if r == 0 {
            return Ok(0);
        }
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return match errno() {
                libc::EINTR | libc::EAGAIN => Ok(0),
                libc::EIO => Err(closed()),
                _ => Err(io::Error::last_os_error()),
            };
        }
        if n == 0 {
            return Err(closed());
        }
        Ok(n as usize)
    }

    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        let fd = self.master.as_raw_fd();
        while !data.is_empty() {
            // SAFETY: `data` is valid for reads of `data.len()` bytes.
            let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
            if n < 0 {
                match errno() {
                    libc::EINTR | libc::EAGAIN => continue,
                    libc::EIO => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pty closed")),
                    _ => return Err(io::Error::last_os_error()),
                }
            }
            data = &data[n as usize..];
        }
        Ok(())
    }

    fn purge_input(&mut self) {
        let mut buf = [0u8; 4096];
        while matches!(self.read_timeout(&mut buf, Duration::ZERO), Ok(n) if n > 0) {}
    }
}

/// Spawn `program args...` in `cwd` with a fresh PTY as its controlling
/// terminal (set to raw mode before the program starts).
pub fn spawn(program: &str, args: &[String], cwd: &Path) -> io::Result<(PtyPort, Child)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: out-pointers are valid; name/termios/winsize may be null.
    let r = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty just returned these descriptors and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // SAFETY: plain fcntl/termios calls on descriptors we own.
    unsafe {
        libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t);
        }
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe calls (setsid, ioctl) run in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(slave);
    Ok((PtyPort { master }, child))
}

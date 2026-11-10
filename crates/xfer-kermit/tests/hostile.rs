use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use xfer_common::{ChannelPort, Port, RecordingControl, RecvOptions};
use xfer_kermit::KermitOptions;

fn tochar(x: u8) -> u8 {
    x + 32
}
fn check1(b: &[u8]) -> u8 {
    let s: u32 = b.iter().map(|&x| x as u32).sum();
    tochar(((s + ((s & 0xC0) >> 6)) & 0x3F) as u8)
}
fn pkt(seq: u8, typ: u8, data: &[u8]) -> Vec<u8> {
    let mut body = vec![tochar((3 + data.len()) as u8), tochar(seq), typ];
    body.extend_from_slice(data);
    let c = check1(&body);
    let mut v = vec![1];
    v.extend(body);
    v.push(c);
    v.push(b'\r');
    v
}
fn drain(p: &mut ChannelPort, t: Duration) -> Vec<u8> {
    let mut acc = Vec::new();
    let mut b = [0u8; 4096];
    let dl = Instant::now() + t;
    while Instant::now() < dl {
        let n = p.read_timeout(&mut b, Duration::from_millis(20)).unwrap_or(0);
        acc.extend_from_slice(&b[..n]);
    }
    acc
}

#[test]
fn attr_size_in_k_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let (mut a, mut b) = ChannelPort::pair();
    let h = std::thread::spawn(move || {
        let mut ctl = RecordingControl::default();
        let o = KermitOptions { initial_nak: false, ..KermitOptions::default() };
        xfer_kermit::recv(&mut b, &mut ctl, &RecvOptions::new(path), &o)
    });
    a.write_all(&pkt(0, b'S', b"~* @-#N1 (")).unwrap();
    drain(&mut a, Duration::from_millis(200));
    a.write_all(&pkt(1, b'F', b"a.txt")).unwrap();
    drain(&mut a, Duration::from_millis(200));
    let v = b"18446744073709551";
    let mut attr = vec![b'!', tochar(v.len() as u8)];
    attr.extend_from_slice(v);
    a.write_all(&pkt(2, b'A', &attr)).unwrap();
    drain(&mut a, Duration::from_millis(300));
    drop(a);
    assert!(h.join().is_ok(), "KERMIT RECEIVER PANICKED on attribute size overflow");
}

#[test]
fn cancel_ignored_during_no_soh_flood() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let (mut a, mut b) = ChannelPort::pair();
    let ctl = RecordingControl::default();
    let cancel = ctl.cancel.clone();
    let h = std::thread::spawn(move || {
        let mut ctl = ctl;
        let o = KermitOptions { timeout: Duration::from_secs(1), ..KermitOptions::default() };
        let r = xfer_kermit::recv(&mut b, &mut ctl, &RecvOptions::new(path), &o);
        (r.map(|_| ()).map_err(|e| e.to_string()), Instant::now())
    });
    // Flood plain text (e.g. a remote shell printing a log) for 6 seconds.
    let flood_end = Instant::now() + Duration::from_secs(6);
    let mut cancelled_at = None;
    let start = Instant::now();
    while Instant::now() < flood_end {
        let _ = a.write_all(&[b'x'; 64]);
        std::thread::sleep(Duration::from_micros(20));
        let _ = a.write_all(&[b'y'; 4096]);
        if cancelled_at.is_none() && start.elapsed() > Duration::from_millis(500) {
            cancel.store(true, Ordering::SeqCst);
            cancelled_at = Some(Instant::now());
        }
    }
    let stopped = Instant::now();
    let (r, ended) = h.join().unwrap();
    let lag = ended.saturating_duration_since(cancelled_at.unwrap());
    println!(
        "result {r:?}; returned {:?} after cancel; flood stopped {:?} after cancel",
        lag,
        stopped - cancelled_at.unwrap()
    );
    assert!(lag < Duration::from_secs(3), "cancel took {lag:?} to take effect");
}

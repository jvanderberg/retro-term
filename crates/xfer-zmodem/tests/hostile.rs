use std::time::{Duration, Instant};
use xfer_common::crc::{crc16, crc16_update};
use xfer_common::{ChannelPort, Port, RecordingControl, RecvOptions};

fn hex_hdr(kind: u8, d: [u8; 4]) -> Vec<u8> {
    let raw = [kind, d[0], d[1], d[2], d[3]];
    let crc = crc16(&raw).to_be_bytes();
    let mut v = b"**\x18B".to_vec();
    for b in raw.iter().chain(crc.iter()) {
        v.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    v.extend_from_slice(b"\r\n");
    v
}

fn esc(data: &[u8], out: &mut Vec<u8>) {
    for &b in data {
        if matches!(b, 0x18 | 0x11 | 0x13 | 0x91 | 0x93 | 0x10 | 0x90 | 0x0d | 0x8d) {
            out.push(0x18);
            out.push(b ^ 0x40);
        } else {
            out.push(b);
        }
    }
}

fn sub16(data: &[u8], end: u8) -> Vec<u8> {
    let mut v = Vec::new();
    esc(data, &mut v);
    v.push(0x18);
    v.push(end);
    let crc = crc16_update(crc16(data), &[end]).to_be_bytes();
    esc(&crc, &mut v);
    v
}

fn wait_for(port: &mut ChannelPort, kind: u8, t: Duration) -> bool {
    // crude: look for the hex header type digits after "**\x18B"
    let deadline = Instant::now() + t;
    let mut acc = Vec::new();
    let want = format!("{kind:02x}");
    while Instant::now() < deadline {
        let mut b = [0u8; 4096];
        let n = port.read_timeout(&mut b, Duration::from_millis(50)).unwrap_or(0);
        acc.extend_from_slice(&b[..n]);
        if let Some(i) = acc.windows(4).position(|w| w == b"**\x18B") {
            if acc.len() >= i + 6 {
                if &acc[i + 4..i + 6] == want.as_bytes() {
                    return true;
                }
                acc.drain(..i + 4);
            }
        }
    }
    false
}

fn run_recv(info: &[u8], data: &[u8]) -> std::thread::Result<xfer_common::Result<xfer_common::Summary>> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let (mut a, mut b) = ChannelPort::pair();
    let h = std::thread::spawn(move || {
        let mut ctl = RecordingControl::default();
        xfer_zmodem::recv(&mut b, &mut ctl, &RecvOptions::new(path))
    });
    assert!(wait_for(&mut a, 1, Duration::from_secs(3)), "ZRINIT");
    let mut f = hex_hdr(4, [0, 0, 0, 1]);
    f.extend(sub16(info, b'k'));
    a.write_all(&f).unwrap();
    assert!(wait_for(&mut a, 9, Duration::from_secs(3)), "ZRPOS");
    let mut f = hex_hdr(10, [0, 0, 0, 0]);
    f.extend(sub16(data, b'h'));
    f.extend(hex_hdr(11, (data.len() as u32).to_le_bytes()));
    a.write_all(&f).unwrap();
    let _ = wait_for(&mut a, 1, Duration::from_secs(2));
    a.write_all(&hex_hdr(8, [0; 4])).unwrap();
    a.write_all(b"OO").unwrap();
    let r = h.join();
    drop(dir);
    r
}

#[test]
fn huge_mtime_panics_receiver() {
    let r = run_recv(b"evil.txt\x005 1777777777777777777777 100644\x00", b"hello");
    match r {
        Err(_) => panic!("RECEIVER THREAD PANICKED on huge mtime"),
        Ok(res) => println!("receiver result: {res:?}"),
    }
}

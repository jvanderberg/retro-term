use std::time::Duration;
use xfer_common::crc::crc16;
use xfer_common::{ChannelPort, Port, RecordingControl, RecvOptions};

fn block(num: u8, data: &[u8], pad: u8) -> Vec<u8> {
    let mut d = data.to_vec();
    d.resize(128, pad);
    let mut v = vec![0x01, num, !num];
    v.extend_from_slice(&d);
    v.extend_from_slice(&crc16(&d).to_be_bytes());
    v
}

fn expect(port: &mut ChannelPort, want: u8) {
    for _ in 0..100 {
        if let Some(b) = port.read_byte(Duration::from_millis(100)).unwrap() {
            if b == want {
                return;
            }
        }
    }
    panic!("never got {want:#x}");
}

#[test]
fn ymodem_huge_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let (mut a, mut b) = ChannelPort::pair();
    let h = std::thread::spawn(move || {
        let mut ctl = RecordingControl::default();
        xfer_xymodem::ymodem_recv(&mut b, &mut ctl, &RecvOptions::new(path), false)
    });
    expect(&mut a, b'C');
    a.write_all(&block(0, b"a.txt\x005 1777777777777777777777 0", 0)).unwrap();
    expect(&mut a, 0x06);
    expect(&mut a, b'C');
    a.write_all(&block(1, b"hello", 0x1a)).unwrap();
    expect(&mut a, 0x06);
    a.write_all(&[4]).unwrap();
    expect(&mut a, 0x15);
    a.write_all(&[4]).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(a);
    let r = h.join();
    assert!(r.is_ok(), "YMODEM RECEIVER PANICKED on huge mtime");
}

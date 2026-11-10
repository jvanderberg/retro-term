//! YMODEM and YMODEM-G batch transfers.
//!
//! Each file is preceded by block 0 carrying `name NUL size mtime mode`
//! (size decimal, mtime and mode octal); an empty block 0 ends the batch.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use xfer_common::{
    announce_name, create_dest_file, mtime_from_unix, Control, Event, Port, ProgressThrottle, RecvOptions, Result,
    Summary, XferError,
};

use crate::block::{Check, InBlock, Inbound, OutBlock, ACK, LONG_BLOCK, NAK, SHORT_BLOCK, WANT_CRC, WANT_G};
use crate::link::Link;
use crate::receiver::{recv_data, Handshake, RecvMode};
use crate::sender::{send_block, send_data, send_eot, send_final_block, wait_start, SendMode};
use crate::{BLOCK_TIMEOUT, FILE_BUF, INTER_BYTE_TIMEOUT, MAX_ERRORS, PROMPT_INTERVAL, RECV_HANDSHAKE_TRIES};

/// The contents of a YMODEM block 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub size: Option<u64>,
    pub mtime: Option<u64>,
    pub mode: Option<u32>,
}

/// Build block 0's payload.
pub fn encode_header(name: &str, size: u64, mtime: u64, mode: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(name.len() + 40);
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    v.extend_from_slice(format!("{size} {mtime:o} {mode:o}").as_bytes());
    v
}

/// Parse block 0's payload; `None` for the empty end-of-batch block.
pub fn parse_header(data: &[u8]) -> Option<Header> {
    let nul = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    if nul == 0 {
        return None;
    }
    let name = String::from_utf8_lossy(&data[..nul]).into_owned();
    let rest = data.get(nul + 1..).unwrap_or(&[]);
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let info = String::from_utf8_lossy(&rest[..end]);
    let mut fields = info.split_ascii_whitespace();
    let size = fields.next().and_then(|s| s.parse::<u64>().ok());
    let mtime = fields.next().and_then(|s| u64::from_str_radix(s, 8).ok());
    let mode = fields.next().and_then(|s| u32::from_str_radix(s, 8).ok());
    Some(Header { name, size, mtime, mode })
}

fn check_for(start: u8) -> Check {
    if start == NAK {
        Check::Sum
    } else {
        Check::Crc
    }
}

pub fn send(port: &mut dyn Port, ctl: &mut dyn Control, files: &[PathBuf], g: bool) -> Result<Summary> {
    let mut link = Link::new(port, ctl);
    send_inner(&mut link, files, g).map_err(|e| link.abort(e))
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &fs::Metadata) -> u32 {
    0
}

fn send_inner(link: &mut Link<'_>, files: &[PathBuf], g: bool) -> Result<Summary> {
    let mut summary = Summary::default();
    let mut blk = OutBlock::new();
    let mut noted_no_g = false;

    for path in files {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        let name = announce_name(path);
        link.event(Event::FileStart { name: name.clone(), size: Some(size) });

        // Block 0.
        let start = wait_start(link, true)?;
        if g && start != WANT_G && !noted_no_g {
            noted_no_g = true;
            link.event(Event::Message("receiver did not request YMODEM-G; using YMODEM".into()));
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let hdr = encode_header(&name, size, mtime, file_mode(&meta));
        if hdr.len() > LONG_BLOCK {
            return Err(XferError::Protocol(format!("file name too long: {name}")));
        }
        blk.data_mut()[..hdr.len()].copy_from_slice(&hdr);
        let hdr_len = if hdr.len() <= SHORT_BLOCK { SHORT_BLOCK } else { LONG_BLOCK };
        blk.seal(0, hdr.len(), hdr_len, check_for(start), 0);
        send_block(link, &blk, 0, start == WANT_G)?;

        // Data: the receiver asks again once it has opened the file.
        let start = wait_start(link, true)?;
        let mode = SendMode { check: check_for(start), block_len: LONG_BLOCK, streaming: start == WANT_G };
        let mut throttle = ProgressThrottle::default();
        let mut reader = BufReader::with_capacity(FILE_BUF, file);
        let sent = send_data(link, &mut blk, &mut reader, mode, &mut throttle)?;
        send_eot(link)?;
        throttle.report(&mut *link.ctl, sent, true);
        link.event(Event::FileDone { name: name.clone(), bytes: sent, path: path.clone() });
        summary.files.push((name, sent, path.clone()));
    }

    // End of batch: an empty block 0.
    let start = wait_start(link, true)?;
    blk.seal(0, 0, SHORT_BLOCK, check_for(start), 0);
    send_final_block(link, &blk, start == WANT_G)?;
    Ok(summary)
}

pub fn recv(port: &mut dyn Port, ctl: &mut dyn Control, opts: &RecvOptions, g: bool) -> Result<Summary> {
    let mut link = Link::new(port, ctl);
    recv_inner(&mut link, opts, g).map_err(|e| link.abort(e))
}

fn recv_inner(link: &mut Link<'_>, opts: &RecvOptions, g: bool) -> Result<Summary> {
    fs::create_dir_all(&opts.dest_dir)?;
    let prompt = if g { WANT_G } else { WANT_CRC };
    let mut summary = Summary::default();
    let mut inb = InBlock::new();

    while let Some(hdr) = recv_header(link, &mut inb, prompt, g)? {
        link.event(Event::FileStart { name: hdr.name.clone(), size: hdr.size });
        let (path, file) = create_dest_file(&opts.dest_dir, &hdr.name, opts.overwrite)?;
        let mut out = BufWriter::with_capacity(FILE_BUF, file);
        let mode = RecvMode { check: Check::Crc, streaming: g, double_eot: !g, size: hdr.size };
        let hs = Handshake { prompt, interval: BLOCK_TIMEOUT, tries: MAX_ERRORS, fallback_after: None };
        let mut throttle = ProgressThrottle::default();
        let bytes = recv_data(link, &mut inb, &mut out, mode, hs, &mut throttle)?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        if let Some(t) = hdr.mtime.and_then(mtime_from_unix) {
            // Best effort: not every filesystem allows it.
            let _ = file.set_modified(t);
        }
        drop(file);
        link.event(Event::FileDone { name: hdr.name.clone(), bytes, path: path.clone() });
        summary.files.push((hdr.name, bytes, path));
    }
    Ok(summary)
}

/// Solicit and read block 0. `None` means the batch is over.
fn recv_header(link: &mut Link<'_>, inb: &mut InBlock, prompt: u8, streaming: bool) -> Result<Option<Header>> {
    let mut reply = prompt;
    let mut waits = 0u32;
    let mut errors = 0u32;
    loop {
        link.write(&[reply])?;
        match inb.read(link, Check::Crc, PROMPT_INTERVAL, INTER_BYTE_TIMEOUT)? {
            Inbound::Block { num: 0 } => {
                let hdr = parse_header(inb.data());
                // YMODEM-G never acknowledges blocks; plain YMODEM ACKs block
                // 0 and then asks for the data with another prompt.
                if !streaming {
                    // The sender may exit right after the end-of-batch block.
                    let res = link.write(&[ACK]);
                    if hdr.is_some() {
                        res?;
                    }
                }
                return Ok(hdr);
            }
            Inbound::Block { num } => {
                if streaming {
                    return Err(XferError::Protocol(format!("expected block 0, got {num}")));
                }
                errors += 1;
                link.retry(format!("expected block 0, got block {num}"));
                link.purge_line()?;
                reply = NAK;
            }
            Inbound::Eot => {
                // The sender repeated EOT: our ACK of the last file was lost.
                link.write(&[ACK])?;
                reply = prompt;
            }
            Inbound::Bad(reason) => {
                if streaming {
                    return Err(XferError::Protocol(format!("YMODEM-G: {reason}")));
                }
                errors += 1;
                link.retry(reason);
                link.purge_line()?;
                reply = NAK;
            }
            Inbound::Timeout => {
                waits += 1;
                if waits >= RECV_HANDSHAKE_TRIES {
                    return Err(XferError::Timeout);
                }
                link.retry("waiting for file header");
                reply = prompt;
            }
            Inbound::RemoteCancel => return Err(XferError::RemoteCancelled),
        }
        if errors >= MAX_ERRORS {
            return Err(XferError::TooManyErrors("file header kept failing".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = encode_header("foo.txt", 1234, 0o14_000_000_000, 0o100_644);
        assert_eq!(
            parse_header(&h),
            Some(Header {
                name: "foo.txt".into(),
                size: Some(1234),
                mtime: Some(0o14_000_000_000),
                mode: Some(0o100_644)
            })
        );
        let mut padded = h.clone();
        padded.resize(128, 0);
        assert_eq!(parse_header(&padded).unwrap().size, Some(1234));
        assert_eq!(parse_header(&[0u8; 128]), None);
        // lrzsz appends a serial number and remaining-file counts.
        let lrz = b"a.bin\x0042 14567 100644 0 3 999\x00\x00";
        let h = parse_header(lrz).unwrap();
        assert_eq!((h.size, h.mtime, h.mode), (Some(42), Some(0o14567), Some(0o100644)));
        // Name only.
        assert_eq!(parse_header(b"x\0").unwrap().size, None);
    }
}

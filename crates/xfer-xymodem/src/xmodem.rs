//! XMODEM (checksum, CRC and 1K): a single unnamed file.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use xfer_common::{announce_name, Control, Event, Port, ProgressThrottle, Result, Summary};

use crate::block::{Check, InBlock, OutBlock, LONG_BLOCK, NAK, SHORT_BLOCK, WANT_CRC};
use crate::link::Link;
use crate::receiver::{recv_data, Handshake, RecvMode};
use crate::sender::{send_data, send_eot, wait_start, SendMode};
use crate::{XVariant, CRC_PROBES, FILE_BUF, PROMPT_INTERVAL, RECV_HANDSHAKE_TRIES};

pub fn send(port: &mut dyn Port, ctl: &mut dyn Control, file: &Path, variant: XVariant) -> Result<Summary> {
    let mut link = Link::new(port, ctl);
    send_inner(&mut link, file, variant).map_err(|e| link.abort(e))
}

fn send_inner(link: &mut Link<'_>, path: &Path, variant: XVariant) -> Result<Summary> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let name = announce_name(path);
    link.event(Event::FileStart { name: name.clone(), size: Some(size) });

    let start = wait_start(link, false)?;
    let mode = match (start, variant) {
        // The receiver only speaks checksum: classic 128-byte blocks.
        (NAK, _) => SendMode { check: Check::Sum, block_len: SHORT_BLOCK, streaming: false },
        (_, XVariant::OneK) => SendMode { check: Check::Crc, block_len: LONG_BLOCK, streaming: false },
        _ => SendMode { check: Check::Crc, block_len: SHORT_BLOCK, streaming: false },
    };
    if start == NAK && variant != XVariant::Checksum {
        link.event(Event::Message("receiver requested checksum mode".into()));
    }

    let mut throttle = ProgressThrottle::default();
    let mut blk = OutBlock::new();
    let mut reader = BufReader::with_capacity(FILE_BUF, file);
    let sent = send_data(link, &mut blk, &mut reader, mode, &mut throttle)?;
    send_eot(link)?;
    throttle.report(&mut *link.ctl, sent, true);
    link.event(Event::FileDone { name: name.clone(), bytes: sent, path: path.to_path_buf() });
    Ok(Summary { files: vec![(name, sent, path.to_path_buf())] })
}

pub fn recv(port: &mut dyn Port, ctl: &mut dyn Control, dest: &Path, variant: XVariant) -> Result<Summary> {
    let mut link = Link::new(port, ctl);
    recv_inner(&mut link, dest, variant).map_err(|e| link.abort(e))
}

fn recv_inner(link: &mut Link<'_>, dest: &Path, variant: XVariant) -> Result<Summary> {
    let name = announce_name(dest);
    let mut out = BufWriter::with_capacity(FILE_BUF, File::create(dest)?);
    link.event(Event::FileStart { name: name.clone(), size: None });

    let (check, hs) = match variant {
        XVariant::Checksum => (
            Check::Sum,
            Handshake { prompt: NAK, interval: PROMPT_INTERVAL, tries: RECV_HANDSHAKE_TRIES, fallback_after: None },
        ),
        XVariant::Crc | XVariant::OneK => (
            Check::Crc,
            Handshake {
                prompt: WANT_CRC,
                interval: PROMPT_INTERVAL,
                tries: RECV_HANDSHAKE_TRIES,
                fallback_after: Some(CRC_PROBES),
            },
        ),
    };
    let mode = RecvMode { check, streaming: false, double_eot: false, size: None };
    let mut throttle = ProgressThrottle::default();
    let mut inb = InBlock::new();
    let bytes = recv_data(link, &mut inb, &mut out, mode, hs, &mut throttle)?;
    drop(out);
    link.event(Event::FileDone { name: name.clone(), bytes, path: dest.to_path_buf() });
    Ok(Summary { files: vec![(name, bytes, dest.to_path_buf())] })
}

# File-transfer protocols

Each protocol lives in its own crate under `crates/` and has no GUI code. The
crates can be used on their own, for example in a command-line tool, a test
harness or another terminal.

## The common interface (`xfer-common`)

Every protocol function is plain synchronous code with the same shape:

```rust
fn transfer(port: &mut dyn Port, ctl: &mut dyn Control, /* files or options */) -> Result<Summary>
```

- **`Port`** is a byte pipe with read timeouts: `read_timeout`, `read_byte`,
  `read_exact_timeout`, `write_all`, `flush` and `purge_input`. Implement it
  for whatever carries the bytes.
- **`ChannelPort`** implements `Port` over a pair of `mpsc` channels:
  - `ChannelPort::pair()` gives two ports connected back to back, for
    loopback tests.
  - `ChannelPort::spawn(cmd)` talks to a child process over its stdin and
    stdout.
  - `into_unread()` returns bytes that arrived but were never read.
- **`Control`** receives progress `Event`s and says whether the transfer was
  cancelled:
  - `ChannelControl` forwards events over a channel and reads an
    `AtomicBool` cancel flag.
  - `RecordingControl` keeps the events, for tests.
- **`Event`**: `FileStart`, `Progress`, `FileDone`, `FileSkipped`, `Retry` and
  `Message`.
- **`Summary`** lists the files transferred as (name, bytes, local path).
- **`XferError`**: `Io`, `Timeout`, `Cancelled`, `RemoteCancelled`,
  `TooManyErrors` and `Protocol`.
- **`RecvOptions`** gives the destination directory and whether existing
  files may be overwritten.

Run transfers on a worker thread. Each protocol checks `Control::cancelled` at
least every 100 ms and, on cancel, sends its protocol's abort sequence before
returning `XferError::Cancelled`.

### Example: receive with ZMODEM from a child process

```rust
use std::process::Command;
use xfer_common::{ChannelPort, RecordingControl, RecvOptions};

let (mut port, _child) = ChannelPort::spawn(Command::new("sz").arg("file.bin"))?;
let mut ctl = RecordingControl::default();
let summary = xfer_zmodem::recv(&mut port, &mut ctl, &RecvOptions::new("downloads"))?;
for (name, bytes, path) in summary.files {
    println!("{name}: {bytes} bytes -> {}", path.display());
}
```

## Received file names

A remote picks the names of the files it sends, so every receiver passes them
through `create_dest_file`. It:

- keeps only the last path component (so `../`, absolute paths and `C:\` are
  stripped);
- removes control characters and characters Windows forbids;
- removes leading dots;
- renames Windows device names (`CON`, `NUL`, `COM1`, …) and shortens
  overlong names;
- creates the file with `create_new`, so an existing file is never replaced
  unless `overwrite` is set. Instead, `.1`, `.2`, … is appended.

A symlink already in the download folder is never followed, even with
`overwrite`.

Remote modification times go through `mtime_from_unix`, which ignores values
that can't be represented instead of panicking.

## XMODEM, YMODEM and YMODEM-G (`xfer-xymodem`)

| Function | Protocol |
|---|---|
| `xmodem_send(port, ctl, file, variant)` | XMODEM. `XVariant::Checksum`, `Crc` or `OneK` |
| `xmodem_recv(port, ctl, dest_file, variant)` | XMODEM, written to an exact path |
| `ymodem_send(port, ctl, files, g)` | YMODEM batch, or YMODEM-G streaming when `g` is true |
| `ymodem_recv(port, ctl, opts, g)` | YMODEM batch receive into `opts.dest_dir` |

- **Sender:** follows what the receiver asks for. NAK means 8-bit checksum, `C`
  means CRC-16 and `G` means streaming.
- **Final block:** a final chunk of 128 bytes or less goes in a 128-byte
  block, as lrzsz does.
- **XMODEM length:** XMODEM has no length field, so the receiver strips SUB
  (0x1A) padding from the last block.
- **YMODEM length:** YMODEM truncates the file to the size given in block 0.
- **Duplicates:** a repeated block is acknowledged and discarded; a wrong
  sequence number is fatal.
- **Cancel:** `CAN CAN` from the remote cancels.

## ZMODEM (`xfer-zmodem`)

| Function | Purpose |
|---|---|
| `send(port, ctl, files)` / `send_with_options` | Batch send |
| `recv(port, ctl, opts)` / `recv_with_options` | Batch receive |
| `Detector` | Spots `sz` (ZRQINIT) or `rz` (ZRINIT) starting up in a terminal byte stream |

Supported:

- **Headers:** hex headers and binary headers with CRC-16 or CRC-32.
- **Data subpackets:** ZCRCE, ZCRCG, ZCRCQ and ZCRCW.
- **Streaming:** a sliding window acknowledged with ZCRCQ, or
  receiver-buffer windows with ZCRCW.
- **Error recovery:** ZRPOS with resume.
- **Other frames:** ZSKIP, ZCRC, ZSINIT/ESCCTL and ZFREECNT.

ZCOMMAND is always refused.

The sender always escapes ZDLE, XON/XOFF and DLE (including their high-bit
forms), and also `^O` and `^V`. lrzsz leaves `IEXTEN` set on its tty, and the
macOS PTY then swallows those two bytes even in raw mode.

Receive buffers are bounded: subpackets are limited to 16 KiB and garbage
between headers to 4 MiB.

## Kermit (`xfer-kermit`)

| Function | Purpose |
|---|---|
| `send(port, ctl, files, opts)` | Send files |
| `recv(port, ctl, recv_opts, opts)` | Receive files |
| `recv_with_init(port, ctl, recv_opts, opts, packet)` | Receive, starting from a Send-Init packet already read by the detector |
| `Detector` | Spots a Send-Init packet in a terminal byte stream |

Supported:

- **Packets:** normal and extended-length (long) packets, up to 9024 bytes.
- **Block checks:** types 1, 2 and 3 (CRC-16).
- **Encoding:** control prefixing, 8th-bit prefixing and repeat-count
  compression.
- **Attributes:** attribute packets carrying the file size.
- **Sliding windows:** selective repeat, up to 31 slots.

`KermitOptions` controls packet length, window size, block check, prefixing,
attributes, timeout and the retry limit. The defaults match a fast,
error-free link to C-Kermit.

A packet read waits until its deadline even when bytes keep arriving that
never form a packet. It returns `XferError::Cancelled` within 100 ms of
cancellation.

## Tests

Each crate has:

- unit tests (framing, CRCs, encoders);
- loopback tests, in which our sender talks to our receiver over
  `ChannelPort::pair()`;
- interop tests against the reference implementations:
  - `sz`, `rz`, `sb`, `rb`, `sx` and `rx` from lrzsz;
  - `kermit` from C-Kermit;
- hostile-input tests (`tests/hostile.rs`): malformed frames, absurd
  timestamps and sizes, junk floods.

Interop tests skip themselves when the external programs aren't installed. On
macOS, install them with `brew install lrzsz c-kermit`.

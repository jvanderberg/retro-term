<img src="assets/icon.svg" width="64" height="64" alt="">

# retro-term

A fast terminal emulator in Rust with full ANSI/VT/xterm emulation and the classic
file-transfer protocols: **XMODEM** (checksum, CRC, 1K), **YMODEM**, **YMODEM-G**,
**ZMODEM** and **Kermit**. It's built for dialing BBSes over telnet, talking to
hardware over serial ports, and everyday use as a local shell.

## Features

**Emulation**
- VT100/VT220/xterm escape sequences, parsed by `vte` (SIMD-scanned plain text).
  Covers cursor movement, scroll regions, insert/delete, tab stops, origin mode,
  alternate screen (47/1047/1049), DEC line drawing, charset designation (G0–G3, SS2/SS3),
  REP, DECALN, soft and hard reset, and DSR/DA/DECRQM reports.
- SGR: bold, dim, italic, underline (single and double), blink, inverse, hidden,
  strike, overline. 16, 256 and 24-bit color, in both `;` and `:` sub-parameter forms.
- xterm mouse reporting (X10, 1000, 1002, 1003, with UTF-8 and SGR encodings),
  bracketed paste, focus events, cursor shapes (DECSCUSR), OSC title, OSC 4/10/11/12
  palette set and query, and OSC 52 clipboard (write only, and off unless enabled
  in Settings).
- Wide (CJK and emoji) characters, a 10,000-line scrollback (configurable), and
  word/line selection.
- **CP437** mode for BBSes and ANSI art (ANSI.SYS clear-screen semantics, iCE colors).
  UTF-8 and ISO-8859-1 are also available.
- Box-drawing and block characters (U+2500–U+259F) are drawn geometrically, so ANSI art
  and TUI borders join seamlessly at any font size.
- Color schemes: VGA (classic BBS), xterm, green phosphor and amber phosphor.

**Connections**
- Local shell or any program in a PTY.
- Telnet with option negotiation (BINARY, SGA, ECHO, TTYPE, NAWS) and IAC escaping.
  Binary mode is requested both ways, so 8-bit transfers survive non-compliant servers.
- Raw TCP.
- Serial ports: any baud rate, 5–8 data bits, parity, stop bits, RTS/CTS or XON/XOFF
  flow control, break, and DTR.
- A phonebook (dialing directory), session capture to a file, and an ANSI file viewer.

**File transfer**
- ZMODEM: CRC-16 and CRC-32, streaming with error recovery (ZRPOS), and resume/skip.
  Auto-start fires when the remote runs `sz` (download) or `rz` (upload).
- Kermit: long packets, sliding windows, all three block-check types, repeat
  compression, 8th-bit prefixing and attribute packets. Auto-start fires on a remote `kermit -s`.
- XMODEM (checksum, CRC, 1K), YMODEM batch and YMODEM-G.
- A progress window with rate, ETA, retries and a log. Esc or the Cancel button aborts.
- Each protocol lives in its own crate (`crates/xfer-*`) behind a small `Port` trait, so
  they can be reused without the GUI.

## Building

```sh
cargo build --release
./target/release/retro-term
```

## Usage

```sh
retro-term                                  # local login shell
retro-term bbs.example.org                  # telnet (port 23)
retro-term telnet://bbs.example.org:2323 --encoding cp437
retro-term tcp://192.168.1.10:5000          # raw TCP
retro-term /dev/cu.usbserial-1410 --baud 9600 --framing 8N1
retro-term --shell "ssh myhost"
retro-term --view art.ans                   # display an ANSI art file
```

The window resizes the grid to fit. **View → Fit window to 80×25** gives the classic size.

### Shortcuts

| macOS | Linux/Windows | Action |
|---|---|---|
| ⌘K | Ctrl+Shift+K | Connect / phonebook |
| ⌘T | Ctrl+Shift+T | New local shell |
| ⌘C / ⌘V | Ctrl+Shift+C / V | Copy / paste |
| ⌘U / ⌘D | Ctrl+Shift+U / D | ZMODEM upload / download |
| ⌘+ / ⌘- / ⌘0 | Ctrl+Shift+= / - / 0 | Font size |
| Shift+PgUp/PgDn/Home/End | same | Scroll back |

Shift+drag selects text even when an application has turned on mouse reporting.
Double-click selects a word and triple-click selects a line.

### File transfers

**Transfer → Upload / Download** offers every protocol. ZMODEM and Kermit downloads start
automatically when the remote side begins sending. When the remote runs `rz`, a file
picker opens. Files are saved to the download folder (Transfer → Download folder…).

Settings and the phonebook are stored in `settings.toml` under the platform config
directory. The path is shown in Edit → Settings. Saves are atomic, and a file that
can't be read is moved aside to `settings.toml.bad` instead of being overwritten.

### Safety with untrusted hosts

BBSes and remote hosts send arbitrary bytes, so retro-term treats their output as hostile:

- **Received file names** are reduced to a plain name inside the download folder.
  Existing files are never overwritten, and symlinks are never followed.
- **No title report:** the window title is never echoed back (CSI 21 t is ignored).
- **Bracketed paste** strips control characters, so pasted text can't break out
  of the bracket.
- **OSC 52** (setting the clipboard from the host) is off by default.
- **Memory and CPU limits:** OSC strings, the Hold buffer and REP are all bounded.
- **Malformed protocol frames** and absurd sizes or timestamps are rejected
  without crashing. Each protocol crate has hostile-input tests.

## Testing

```sh
cargo test --workspace
```

Besides unit tests for the emulator, telnet layer, input encoding and each protocol, the
test suite includes:

- **Interop tests** against `lrzsz` (`sz`/`rz`) and C-Kermit, in both directions.
- **End-to-end tests** (`src/e2e_tests.rs`). These run the real session plumbing with `sz`,
  `rz`, `kermit` and a mock telnet server, covering PTY I/O, auto-detection, byte routing
  and the protocol crates together.

Tests that need external programs skip themselves when the programs aren't installed.
On macOS: `brew install lrzsz c-kermit`.

Known quirks of the reference tools, worked around in the tests:
- lrzsz leaves `IEXTEN` set on its tty. On macOS the PTY then swallows `^V` and `^O` even
  in raw mode, so our ZMODEM sender always escapes those bytes. YMODEM/XMODEM have no
  escaping, so uploads to `rb`/`rx` on macOS need `stty -iexten` first.
- The Homebrew C-Kermit 9.0.302 build refuses to receive files that don't already exist
  ("Write access denied"). Pre-create them and use `kermit -w -r`.

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how bytes flow from the connection
  to the screen, the threads and locks, and how transfers take over the stream.
- [docs/PROTOCOLS.md](docs/PROTOCOLS.md): the protocol crates, their APIs and
  how to use them without the GUI.
- `cargo doc --workspace --no-deps --open` gives the API reference.

## Layout

```
src/term/        screen model, SGR/CSI/OSC handling, charsets, palette
src/view.rs      egui widget: rendering, selection, keyboard/mouse input
src/boxdraw.rs   geometric box-drawing/block glyphs
src/session.rs   reader/writer threads, routing to terminal or transfer
src/conn/        PTY, telnet, TCP, serial
src/transfer.rs  transfer jobs and auto-detection
crates/xfer-common   Port/Control traits, CRCs, ChannelPort
crates/xfer-xymodem  XMODEM, YMODEM, YMODEM-G
crates/xfer-zmodem   ZMODEM
crates/xfer-kermit   Kermit
```

Created in [T3 Code](https://t3.codes).

# Architecture

retro-term is a pipeline. Bytes come in from a connection, background threads
route them either to the terminal emulator or to a running file transfer, and
the GUI draws the emulator's screen every frame.

```
            ┌───────────── connection (src/conn/) ─────────────┐
            │  PTY shell │ Telnet │ raw TCP │ serial port       │
            └──────┬──────────────────────────────▲────────────┘
                   │ bytes in                     │ bytes out
            ┌──────▼──────┐                ┌──────┴──────┐
            │ reader      │                │ writer      │
            │ thread      │                │ thread      │
            └──────┬──────┘                └──────▲──────┘
     telnet decode │                              │ keys, paste, resize,
     auto-detect   │                              │ break, DTR, transfer data
          ┌────────┴────────┐                     │
          ▼                 ▼                     │
   ┌────────────┐   ┌───────────────┐             │
   │ Emulator   │   │ TransferJob   │─────────────┤
   │ (screen    │   │ (own thread:  │             │
   │  model)    │   │  ZMODEM etc.) │             │
   └─────┬──────┘   └───────┬───────┘             │
         │ Arc<Mutex>       │ progress events     │
   ┌─────▼──────────────────▼───────┐             │
   │ GUI thread (egui): TermView,   │─────────────┘
   │ menus, transfer window         │
   └────────────────────────────────┘
```

## Source layout

| Path | Role |
|---|---|
| `src/main.rs` | Command-line parsing (`clap`), target parsing, window setup |
| `src/app.rs` | The egui application: menus, dialogs, status bar, transfer window |
| `src/config.rs` | `Settings` and the phonebook, stored as TOML |
| `src/conn/` | Opening PTY, telnet, raw TCP and serial connections; telnet negotiation |
| `src/session.rs` | Reader and writer threads, routing of incoming bytes |
| `src/transfer.rs` | `TransferJob` (a transfer on its own thread) and auto-detection |
| `src/term/` | The screen model and escape-sequence handling |
| `src/view.rs` | The terminal widget: drawing, selection, keyboard and mouse |
| `src/boxdraw.rs` | Geometric drawing of box and block characters |
| `src/input.rs` | Key and mouse event encoding |
| `crates/xfer-*` | The file-transfer protocols, usable without the GUI |

## Connections (`src/conn/`)

`conn::open` turns every `ConnSpec` into the same `Opened` bundle: a reader, a
writer, and a `ConnControl` for out-of-band actions (resize, break, DTR,
close).

- **Shell:** a pseudo-terminal from `portable-pty`, with `TERM` set to the
  configured terminal type.
- **Telnet:** a TCP socket plus the `Telnet` negotiator (`telnet.rs`).
  - It filters IAC commands out of the data stream.
  - It answers option negotiation and offers NAWS (window size), TTYPE
    (terminal type) and BINARY, and asks the server for SGA and BINARY.
  - Options we asked for are tracked separately, so the server's answer is
    never answered again (no negotiation loops).
  - Binary mode matters for file transfers. Without it, CR goes out as CR NUL
    and a NUL after CR is dropped on input, which corrupts X/Y/ZMODEM data on
    servers that don't follow the telnet spec strictly.
  - A literal 0xFF in user or transfer data is sent as IAC IAC.
- **Raw TCP:** a plain socket.
- **Serial:** `serialport` with a 50 ms read timeout. The reader treats those
  timeouts as normal (`timeouts_ok`) and checks the session's close flag
  between reads, because a serial read can't be interrupted from outside.

## Sessions and routing (`src/session.rs`)

A `Session` owns two threads per connection.

The **writer thread** works through a queue of `Out` messages: data
(telnet-escaped when needed), raw negotiation bytes, resizes (including the
telnet NAWS message), break, DTR and close.

The **reader thread** reads, strips telnet commands, writes the capture file if
one is open, and hands each chunk to the `Router`. The router sends bytes to
one of three places:

| Route | When | What happens to the bytes |
|---|---|---|
| `Terminal` | Normal use | Fed to the emulator; replies it queues (cursor reports and so on) go back to the writer |
| `Hold` | A transfer was auto-detected and the UI hasn't started it yet | Buffered, up to 4 MiB |
| `Transfer` | A transfer is running | Sent to the protocol thread over a channel |

The router and its auto-detectors sit behind one mutex. The reader, the UI and
a finishing transfer all take that lock to deliver bytes, so delivery order is
always preserved.

### Auto-detection (`src/transfer.rs`)

While routing to the terminal, each chunk is also scanned by `Detectors`:

| Seen from the remote | Meaning | Action |
|---|---|---|
| ZMODEM ZRQINIT header (`sz` started) | The remote wants to send | Start a ZMODEM download |
| ZMODEM ZRINIT header (`rz` started) | The remote is ready to receive | Open a file picker |
| Kermit Send-Init packet (`kermit -s`) | The remote is sending | Start a Kermit download |

Detection checks the header CRC or packet checksum, so ordinary output doesn't
trigger it. On a hit, the bytes up to the trigger go to the terminal, followed
by `\r ESC[2K` to erase the header that was echoed onto the line. The route
switches to `Hold`, and the UI is notified.

If the user declines (for example by cancelling the upload file picker),
`release_hold` shows the held bytes on the terminal again.

### Transfer lifecycle

1. `Session::begin_transfer` switches the route to `Transfer` and returns a
   `TransferLink`. Any bytes held since the auto-detect are delivered first.
2. `TransferJob::start` runs the protocol on its own thread, talking through
   the link's `ChannelPort`.
3. When the protocol returns, the thread calls `ReturnPath::finish`. Under the
   router lock it:
   - takes every byte that arrived but wasn't read (usually the remote shell
     prompt);
   - switches the route back to `Terminal`;
   - delivers those bytes, running auto-detection on them too.
4. If the thread panics, dropping the `ReturnPath` still restores terminal
   routing.

The transfer thread restores routing itself, not the UI, for two reasons:

- Output that arrives between the end of the protocol and the UI's next poll
  isn't lost.
- A finished job can never reset the routing of a newer session.

## Terminal emulation (`src/term/`)

`Emulator` combines a `vte` parser with the `Term` screen model.

- **Decoding:** bytes are first decoded according to the selected encoding:
  - UTF-8 is passed through unchanged.
  - CP437 and ISO-8859-1 are translated to UTF-8 (`charset.rs`).
- **Parsing:** `vte` splits the stream into printable runs and escape
  sequences. Its OSC buffer has a fixed size (`OSC_MAX`, 256 KiB), so a hostile
  stream can't grow memory without bound.
- **The screen (`Term`):** a grid of `Cell`s, each holding a character,
  foreground and background colors and attribute flags (`cell.rs`). It also
  keeps:
  - the cursor and the cursor saved by DECSC;
  - the scroll region and tab stops;
  - the primary and alternate screens;
  - the scrollback buffer;
  - the active modes.
- **Escape sequences:**
  - CSI: cursor movement, erase, insert and delete, scrolling, modes.
  - SGR: attributes and colors (16, 256 and 24-bit).
  - OSC: window title, palette set and query, clipboard write.
  - ESC: charsets, save and restore cursor, reset.
- **Replies:** sequences that need an answer (DA, DSR, DECRQM, palette
  queries, CSI 18 t) append it to `Term::responses`, and the reader sends it to
  the host.
- **BBS mode (CP437):** copies ANSI.SYS behavior. ED 2 also homes the cursor,
  and with iCE colors enabled, the blink attribute selects a bright background.

### Resizing

When the window shrinks, rows below the cursor are dropped first. Rows are
pushed into scrollback only when the cursor line would otherwise be hidden.
When it grows, lines come back from scrollback.

The primary screen is resized this way even while the alternate screen is
shown, anchored on its saved cursor. Resizing the window inside vim or less
therefore doesn't lose the shell's screen.

`lines_scrolled` counts every line that leaves the top of the screen. It is
reduced again when lines are pulled back from scrollback, so absolute line
numbers, which selections are anchored to, stay stable.

## Rendering (`src/view.rs`, `src/boxdraw.rs`)

Each frame, `TermView` locks the emulator briefly and draws the visible rows:

- **Text:** one egui layout job per row, with runs of the same style batched.
  Letter spacing is set to the cell width minus the font's glyph advance, so
  every glyph lands exactly on the cell grid.
- **Box drawing and blocks:** U+2500–U+259F are drawn as rectangles from a
  table (`boxdraw.rs`), so ANSI art joins without gaps at any font size. Light,
  heavy and double lines meet correctly.
- **Separate glyphs:** wide (CJK, emoji) characters and characters missing
  from the font are drawn on their own.
- **Cursor and effects:** the cursor (block, underline or bar), selection
  highlighting, blink and the visual bell.

Input:

- **Keys:** encoded by `input::key_bytes`, including application cursor and
  keypad modes and Option-as-Meta.
- **Mouse:** produces xterm mouse reports when the application has asked for
  them (X10, normal, button and any-motion tracking, with the default, UTF-8
  or SGR encoding). Otherwise it selects text. Shift forces selection.
- **Bracketed paste:** the pasted text has its control characters removed
  first, so it can't end the bracket early and inject keystrokes.
- **Resize:** the grid is refitted to the window, and the new size is sent to
  the PTY or as telnet NAWS.

## File-transfer crates (`crates/`)

| Crate | Contents |
|---|---|
| `xfer-common` | `Port` and `Control` traits, `ChannelPort`, CRCs, safe file naming, progress throttling |
| `xfer-xymodem` | XMODEM (checksum, CRC, 1K), YMODEM batch, YMODEM-G |
| `xfer-zmodem` | ZMODEM sender, receiver and start-sequence detector |
| `xfer-kermit` | Kermit sender, receiver (sliding windows, long packets) and detector |

See [PROTOCOLS.md](PROTOCOLS.md) for how the protocols are implemented and how
to use the crates on their own.

## Threads and locks

| Thread | Holds | Never does |
|---|---|---|
| GUI | Emulator lock while drawing a frame | Session calls while holding the emulator lock |
| Reader | Router lock, then emulator lock, while delivering | Block on I/O while holding either lock |
| Writer | Telnet lock only while escaping or building NAWS | Hold the telnet lock while writing to the socket |
| Transfer | Router lock in `ReturnPath::finish` | — |

Locks are always taken in the same order (router, then emulator), so they
can't deadlock.

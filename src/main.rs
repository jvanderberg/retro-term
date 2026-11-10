//! retro-term: a fast terminal emulator with full ANSI support and
//! XMODEM/YMODEM/ZMODEM/Kermit file transfers.

mod app;
mod boxdraw;
mod config;
mod conn;
#[cfg(all(test, unix))]
mod e2e_tests;
mod input;
mod session;
mod term;
mod transfer;
mod view;

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use crate::config::Settings;
use crate::conn::{ConnSpec, SerialSettings};
use crate::term::Encoding;

#[derive(Parser, Debug)]
#[command(name = "retro-term", version, about)]
struct Cli {
    /// Where to connect: `telnet://host[:port]`, `tcp://host:port`, `host[:port]`
    /// (telnet), or a serial device path such as /dev/cu.usbserial-1410.
    target: Option<String>,

    /// Run this command in a local pseudo-terminal instead of your login shell.
    #[arg(long, conflicts_with = "target")]
    shell: Option<String>,

    /// Baud rate for serial connections.
    #[arg(long, default_value_t = 115_200)]
    baud: u32,

    /// Serial framing, e.g. 8N1, 7E1.
    #[arg(long, default_value = "8N1")]
    framing: String,

    /// Character encoding: utf8, cp437 (BBS/ANSI art) or latin1.
    #[arg(long)]
    encoding: Option<String>,

    /// Display a file (e.g. .ANS art) instead of connecting.
    #[arg(long)]
    view: Option<PathBuf>,

    /// Terminal size as COLSxROWS for the initial window.
    #[arg(long)]
    size: Option<String>,

    /// Font size in points.
    #[arg(long)]
    font_size: Option<f32>,

    /// Directory for received files (overrides the saved setting for this run).
    #[arg(long)]
    download_dir: Option<PathBuf>,

    /// Open a dialog at startup (for testing): connect, settings or about.
    #[arg(long, hide = true)]
    ui_test: Option<String>,

    /// Save a PNG screenshot of the window after startup and exit (for testing).
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
}

fn parse_target(t: &str, baud: u32, framing: &str) -> Result<ConnSpec> {
    let host_port = |s: &str, default: u16| -> Result<(String, u16)> {
        match s.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') || h.starts_with('[') => {
                Ok((h.trim_matches(['[', ']']).to_string(), p.parse().with_context(|| format!("bad port in {s}"))?))
            }
            _ => Ok((s.to_string(), default)),
        }
    };
    if let Some(rest) = t.strip_prefix("telnet://") {
        let (host, port) = host_port(rest.trim_end_matches('/'), 23)?;
        return Ok(ConnSpec::Telnet { host, port });
    }
    if let Some(rest) = t.strip_prefix("tcp://") {
        let (host, port) = host_port(rest.trim_end_matches('/'), 23)?;
        return Ok(ConnSpec::Raw { host, port });
    }
    let is_com_port = t.len() > 3 && t[..3].eq_ignore_ascii_case("com") && t[3..].bytes().all(|b| b.is_ascii_digit());
    if t.starts_with('/') || is_com_port {
        let f = framing.as_bytes();
        let ok = f.len() == 3 && (b'5'..=b'8').contains(&f[0]) && (b'1'..=b'2').contains(&f[2]);
        if !ok {
            return Err(anyhow!("framing must look like 8N1 (5-8 data bits, N/O/E parity, 1-2 stop bits)"));
        }
        let parity = match f[1].to_ascii_uppercase() {
            b'N' => conn::Parity::None,
            b'O' => conn::Parity::Odd,
            b'E' => conn::Parity::Even,
            _ => return Err(anyhow!("unknown parity in {framing}")),
        };
        return Ok(ConnSpec::Serial(SerialSettings {
            port: t.to_string(),
            baud,
            data_bits: f[0] - b'0',
            parity,
            stop_bits: f[2] - b'0',
            flow: conn::FlowControl::None,
        }));
    }
    let (host, port) = host_port(t, 23)?;
    Ok(ConnSpec::Telnet { host, port })
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let mut settings = Settings::load();
    if let Some(fs) = cli.font_size {
        settings.font_size = fs;
    }
    if let Some(dir) = &cli.download_dir {
        settings.download_dir = dir.clone();
    }
    // Command-line overrides are for this run only; don't persist them.
    let persist = cli.download_dir.is_none() && cli.font_size.is_none() && cli.screenshot.is_none();

    let mut startup = app::StartupOptions {
        screenshot: cli.screenshot.clone(),
        ui_test: cli.ui_test.clone(),
        persist_settings: persist,
        ..Default::default()
    };
    startup.encoding = match cli.encoding.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => None,
        Some("utf8" | "utf-8") => Some(Encoding::Utf8),
        Some("cp437" | "ibm" | "ansi") => Some(Encoding::Cp437),
        Some("latin1" | "iso-8859-1") => Some(Encoding::Latin1),
        Some(other) => return Err(anyhow!("unknown encoding {other}")),
    };
    if let Some(size) = &cli.size {
        let (c, r) = size.split_once(['x', 'X']).ok_or_else(|| anyhow!("--size must look like 80x25"))?;
        let (c, r): (usize, usize) = (c.parse()?, r.parse()?);
        if !(2..=1000).contains(&c) || !(1..=500).contains(&r) {
            return Err(anyhow!("--size must be between 2x1 and 1000x500"));
        }
        startup.size = Some((c, r));
    }
    if let Some(path) = &cli.view {
        let mut bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        if let Some(end) = bytes.iter().position(|&b| b == 0x1A) {
            bytes.truncate(end);
        }
        startup.feed = Some(bytes);
        if startup.encoding.is_none() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ans")) {
            startup.encoding = Some(Encoding::Cp437);
        }
    } else if let Some(cmd) = &cli.shell {
        startup.connect = Some(ConnSpec::Shell { command: Some(cmd.clone()) });
    } else if let Some(t) = &cli.target {
        startup.connect = Some(parse_target(t, cli.baud, &cli.framing)?);
    }

    // Size the window to the requested grid (approximate; the view refits).
    let (cols, rows) = startup.size.unwrap_or((100, 30));
    let cell_w = settings.font_size * 0.602;
    let cell_h = settings.font_size * 1.17;
    let inner = egui::vec2(cols as f32 * cell_w + 8.0, rows as f32 * cell_h + 8.0 + 52.0);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(inner)
            .with_min_inner_size([320.0, 200.0])
            .with_title("retro-term"),
        ..Default::default()
    };
    eframe::run_native(
        "retro-term",
        options,
        Box::new(move |cc| Ok(Box::new(app::RetroApp::new(cc, settings, startup)))),
    )
    .map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("bbs.example.com", 0, "8N1").unwrap(),
            ConnSpec::Telnet { host: "bbs.example.com".into(), port: 23 }
        );
        assert_eq!(
            parse_target("telnet://h:2323", 0, "8N1").unwrap(),
            ConnSpec::Telnet { host: "h".into(), port: 2323 }
        );
        assert_eq!(parse_target("tcp://h:80", 0, "8N1").unwrap(), ConnSpec::Raw { host: "h".into(), port: 80 });
        match parse_target("/dev/cu.usb", 9600, "7E1").unwrap() {
            ConnSpec::Serial(s) => {
                assert_eq!((s.baud, s.data_bits, s.parity, s.stop_bits), (9600, 7, conn::Parity::Even, 1));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_target("COM3", 9600, "8N1").unwrap(), ConnSpec::Serial(_)));
        assert_eq!(
            parse_target("commodore.example.org", 0, "8N1").unwrap(),
            ConnSpec::Telnet { host: "commodore.example.org".into(), port: 23 }
        );
        assert!(parse_target("/dev/cu.usb", 9600, " N1").is_err());
        assert!(parse_target("/dev/cu.usb", 9600, "9N1").is_err());
    }
}

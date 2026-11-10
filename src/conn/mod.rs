//! Connections: local shell (PTY), telnet, raw TCP and serial ports.

pub mod telnet;

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FlowControl {
    #[default]
    None,
    Hardware,
    Software,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialSettings {
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub flow: FlowControl,
}

impl Default for SerialSettings {
    fn default() -> Self {
        SerialSettings {
            port: String::new(),
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow: FlowControl::None,
        }
    }
}

/// What to connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnSpec {
    /// A local shell (or other program) in a pseudo-terminal.
    Shell {
        command: Option<String>,
    },
    Telnet {
        host: String,
        port: u16,
    },
    /// Plain TCP with no telnet processing.
    Raw {
        host: String,
        port: u16,
    },
    Serial(SerialSettings),
}

impl ConnSpec {
    pub fn describe(&self) -> String {
        match self {
            ConnSpec::Shell { command: Some(c) } => c.clone(),
            ConnSpec::Shell { command: None } => "local shell".into(),
            ConnSpec::Telnet { host, port } => format!("telnet://{host}:{port}"),
            ConnSpec::Raw { host, port } => format!("tcp://{host}:{port}"),
            ConnSpec::Serial(s) => {
                let p = match s.parity {
                    Parity::None => 'N',
                    Parity::Odd => 'O',
                    Parity::Even => 'E',
                };
                format!("{} {} {}{}{}", s.port, s.baud, s.data_bits, p, s.stop_bits)
            }
        }
    }
}

/// Out-of-band operations on a connection.
pub trait ConnControl: Send {
    fn resize(&mut self, _cols: u16, _rows: u16, _px_w: u16, _px_h: u16) {}
    fn send_break(&mut self) {}
    fn set_dtr(&mut self, _on: bool) {}
    fn close(&mut self) {}
}

pub struct Opened {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub control: Box<dyn ConnControl>,
    /// Apply telnet protocol processing.
    pub telnet: bool,
    /// Reads may time out (serial ports); timeouts are not errors.
    pub timeouts_ok: bool,
}

pub fn open(spec: &ConnSpec, cols: u16, rows: u16, term_name: &str) -> Result<Opened> {
    match spec {
        ConnSpec::Shell { command } => open_pty(command.as_deref(), cols, rows, term_name),
        ConnSpec::Telnet { host, port } => open_tcp(host, *port, true),
        ConnSpec::Raw { host, port } => open_tcp(host, *port, false),
        ConnSpec::Serial(s) => open_serial(s),
    }
}

// ----- PTY ---------------------------------------------------------------------

struct PtyControl {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl ConnControl for PtyControl {
    fn resize(&mut self, cols: u16, rows: u16, px_w: u16, px_h: u16) {
        let _ = self.master.resize(portable_pty::PtySize { rows, cols, pixel_width: px_w, pixel_height: px_h });
    }
    fn close(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for PtyControl {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open_pty(command: Option<&str>, cols: u16, rows: u16, term_name: &str) -> Result<Opened> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .context("could not open a pseudo-terminal")?;
    let mut cmd = match command {
        Some(c) if !c.trim().is_empty() => {
            let mut parts = c.split_whitespace();
            let mut b = CommandBuilder::new(parts.next().unwrap_or_default());
            b.args(parts);
            b
        }
        _ => CommandBuilder::new_default_prog(),
    };
    cmd.env("TERM", term_name);
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "retro-term");
    if let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
        cmd.cwd(home);
    }
    let child = pair.slave.spawn_command(cmd).context("could not start the shell")?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    Ok(Opened {
        reader,
        writer,
        control: Box::new(PtyControl { master: pair.master, child }),
        telnet: false,
        timeouts_ok: false,
    })
}

// ----- TCP ---------------------------------------------------------------------

struct TcpControl(TcpStream);

impl ConnControl for TcpControl {
    fn close(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

fn open_tcp(host: &str, port: u16, telnet: bool) -> Result<Opened> {
    let addrs: Vec<_> = (host, port).to_socket_addrs().with_context(|| format!("could not resolve {host}"))?.collect();
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                let reader = stream.try_clone()?;
                let control = stream.try_clone()?;
                return Ok(Opened {
                    reader: Box::new(reader),
                    writer: Box::new(stream),
                    control: Box::new(TcpControl(control)),
                    telnet,
                    timeouts_ok: false,
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(match last_err {
        Some(e) => anyhow!("could not connect to {host}:{port}: {e}"),
        None => anyhow!("{host} did not resolve to any address"),
    })
}

// ----- Serial ------------------------------------------------------------------

struct SerialControl(Box<dyn serialport::SerialPort>);

impl ConnControl for SerialControl {
    fn send_break(&mut self) {
        if self.0.set_break().is_ok() {
            std::thread::sleep(Duration::from_millis(250));
            let _ = self.0.clear_break();
        }
    }
    fn set_dtr(&mut self, on: bool) {
        let _ = self.0.write_data_terminal_ready(on);
    }
}

fn open_serial(s: &SerialSettings) -> Result<Opened> {
    use serialport::{DataBits, StopBits};
    if s.port.is_empty() {
        return Err(anyhow!("no serial port selected"));
    }
    let data_bits = match s.data_bits {
        5 => DataBits::Five,
        6 => DataBits::Six,
        7 => DataBits::Seven,
        _ => DataBits::Eight,
    };
    let parity = match s.parity {
        Parity::None => serialport::Parity::None,
        Parity::Odd => serialport::Parity::Odd,
        Parity::Even => serialport::Parity::Even,
    };
    let stop_bits = if s.stop_bits == 2 { StopBits::Two } else { StopBits::One };
    let flow = match s.flow {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
        FlowControl::Software => serialport::FlowControl::Software,
    };
    let port = serialport::new(&s.port, s.baud)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .flow_control(flow)
        .timeout(Duration::from_millis(50))
        .open()
        .with_context(|| format!("could not open {}", s.port))?;
    let reader = port.try_clone()?;
    let control = port.try_clone()?;
    Ok(Opened {
        reader: Box::new(reader),
        writer: Box::new(port),
        control: Box::new(SerialControl(control)),
        telnet: false,
        timeouts_ok: true,
    })
}

/// Serial ports present on this machine.
pub fn serial_ports() -> Vec<String> {
    let mut v: Vec<String> =
        serialport::available_ports().unwrap_or_default().into_iter().map(|p| p.port_name).collect();
    // On macOS every device shows up twice; the /dev/cu.* node is the one to dial out on.
    if v.iter().any(|p| p.starts_with("/dev/cu.")) {
        v.retain(|p| !p.starts_with("/dev/tty."));
    }
    v.sort();
    v
}

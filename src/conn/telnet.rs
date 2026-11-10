//! A small telnet (RFC 854) client-side option negotiator and stream filter.

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;

pub const OPT_BINARY: u8 = 0;
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_TTYPE: u8 = 24;
pub const OPT_NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    Iac,
    Negotiate(u8),
    Sub,
    SubIac,
    /// Saw CR in NVT (non-binary) mode; a following NUL is dropped.
    Cr,
}

/// Telnet protocol state shared by the reader (which parses) and the writer
/// (which needs to know whether to escape CR and how big the window is).
pub struct Telnet {
    state: State,
    sub: Vec<u8>,
    /// Options we have agreed to perform (WILL).
    local: [bool; 256],
    /// Options the server has agreed to perform (DO).
    remote: [bool; 256],
    /// We sent WILL and await DO/DONT (so the answer needs no reply).
    asked_local: [bool; 256],
    /// We sent DO and await WILL/WONT.
    asked_remote: [bool; 256],
    pub term_type: String,
    pub cols: u16,
    pub rows: u16,
}

impl Telnet {
    pub fn new(term_type: &str, cols: u16, rows: u16) -> Telnet {
        Telnet {
            state: State::Data,
            sub: Vec::new(),
            local: [false; 256],
            remote: [false; 256],
            asked_local: [false; 256],
            asked_remote: [false; 256],
            term_type: term_type.to_string(),
            cols,
            rows,
        }
    }

    /// Negotiation we start ourselves right after connecting.
    ///
    /// Binary mode is requested both ways: without it, CR is sent as CR NUL
    /// and NUL after CR is dropped on input, which corrupts X/Y/ZMODEM data
    /// on servers that don't strictly follow the telnet spec.
    pub fn initial(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for opt in [OPT_NAWS, OPT_TTYPE, OPT_BINARY] {
            self.asked_local[opt as usize] = true;
            out.extend([IAC, WILL, opt]);
        }
        for opt in [OPT_SGA, OPT_BINARY] {
            self.asked_remote[opt as usize] = true;
            out.extend([IAC, DO, opt]);
        }
        // The window size itself is sent once the server answers DO NAWS.
        out
    }

    /// Whether our outbound stream is in binary mode.
    pub fn binary_out(&self) -> bool {
        self.local[OPT_BINARY as usize]
    }

    /// The NAWS subnegotiation for the current size (empty unless enabled).
    pub fn naws(&self) -> Vec<u8> {
        if !self.local[OPT_NAWS as usize] {
            return Vec::new();
        }
        let mut out = vec![IAC, SB, OPT_NAWS];
        for v in [self.cols, self.rows] {
            for b in v.to_be_bytes() {
                out.push(b);
                if b == IAC {
                    out.push(IAC);
                }
            }
        }
        out.extend([IAC, SE]);
        out
    }

    /// Escape outgoing user data.
    pub fn escape(&self, data: &[u8], out: &mut Vec<u8>) {
        let binary = self.binary_out();
        for &b in data {
            match b {
                IAC => out.extend([IAC, IAC]),
                b'\r' if !binary => out.extend([b'\r', 0]),
                _ => out.push(b),
            }
        }
    }

    /// Filter incoming bytes: data goes to `data`, protocol replies to `reply`.
    pub fn receive(&mut self, input: &[u8], data: &mut Vec<u8>, reply: &mut Vec<u8>) {
        for &b in input {
            self.state = match self.state {
                State::Data => match b {
                    IAC => State::Iac,
                    b'\r' if !self.remote[OPT_BINARY as usize] => {
                        data.push(b);
                        State::Cr
                    }
                    _ => {
                        data.push(b);
                        State::Data
                    }
                },
                State::Cr => match b {
                    0 => State::Data,
                    IAC => State::Iac,
                    _ => {
                        data.push(b);
                        if b == b'\r' {
                            State::Cr
                        } else {
                            State::Data
                        }
                    }
                },
                State::Iac => match b {
                    IAC => {
                        data.push(IAC);
                        State::Data
                    }
                    DO | DONT | WILL | WONT => State::Negotiate(b),
                    SB => {
                        self.sub.clear();
                        State::Sub
                    }
                    _ => State::Data, // NOP, GA, etc.
                },
                State::Negotiate(cmd) => {
                    self.negotiate(cmd, b, reply);
                    State::Data
                }
                State::Sub => match b {
                    IAC => State::SubIac,
                    _ => {
                        if self.sub.len() < 1024 {
                            self.sub.push(b);
                        }
                        State::Sub
                    }
                },
                State::SubIac => match b {
                    SE => {
                        self.subnegotiation(reply);
                        State::Data
                    }
                    IAC => {
                        self.sub.push(IAC);
                        State::Sub
                    }
                    _ => State::Sub,
                },
            };
        }
    }

    fn negotiate(&mut self, cmd: u8, opt: u8, reply: &mut Vec<u8>) {
        let i = opt as usize;
        match cmd {
            DO => {
                let supported = matches!(opt, OPT_BINARY | OPT_SGA | OPT_TTYPE | OPT_NAWS);
                if supported {
                    if std::mem::take(&mut self.asked_local[i]) {
                        self.local[i] = true;
                    } else if !self.local[i] {
                        self.local[i] = true;
                        reply.extend([IAC, WILL, opt]);
                    }
                    if opt == OPT_NAWS {
                        reply.extend(self.naws());
                    }
                } else {
                    reply.extend([IAC, WONT, opt]);
                }
            }
            DONT => {
                if std::mem::take(&mut self.asked_local[i]) {
                    // Our WILL was refused; nothing to answer.
                } else if self.local[i] {
                    self.local[i] = false;
                    reply.extend([IAC, WONT, opt]);
                }
            }
            WILL => {
                let supported = matches!(opt, OPT_BINARY | OPT_ECHO | OPT_SGA);
                if supported {
                    if std::mem::take(&mut self.asked_remote[i]) {
                        self.remote[i] = true;
                    } else if !self.remote[i] {
                        self.remote[i] = true;
                        reply.extend([IAC, DO, opt]);
                    }
                } else if !self.remote[i] {
                    reply.extend([IAC, DONT, opt]);
                }
            }
            WONT => {
                if std::mem::take(&mut self.asked_remote[i]) {
                    // Our DO was refused.
                } else if self.remote[i] {
                    self.remote[i] = false;
                    reply.extend([IAC, DONT, opt]);
                }
            }
            _ => {}
        }
    }

    fn subnegotiation(&mut self, reply: &mut Vec<u8>) {
        if self.sub.first() == Some(&OPT_TTYPE) && self.sub.get(1) == Some(&TTYPE_SEND) {
            reply.extend([IAC, SB, OPT_TTYPE, TTYPE_IS]);
            reply.extend(self.term_type.as_bytes());
            reply.extend([IAC, SE]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_negotiates() {
        let mut t = Telnet::new("ANSI", 80, 25);
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        t.receive(&[b'h', IAC, WILL, OPT_ECHO, b'i', IAC, IAC, IAC, DO, 99, b'\r', 0, b'x'], &mut data, &mut reply);
        assert_eq!(data, vec![b'h', b'i', IAC, b'\r', b'x']);
        assert_eq!(reply, vec![IAC, DO, OPT_ECHO, IAC, WONT, 99]);
        assert!(t.remote[OPT_ECHO as usize]);
    }

    #[test]
    fn terminal_type_and_naws() {
        let mut t = Telnet::new("ANSI", 80, 25);
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        t.receive(&[IAC, DO, OPT_NAWS, IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE], &mut data, &mut reply);
        let mut expect = vec![IAC, WILL, OPT_NAWS, IAC, SB, OPT_NAWS, 0, 80, 0, 25, IAC, SE];
        expect.extend([IAC, SB, OPT_TTYPE, TTYPE_IS, b'A', b'N', b'S', b'I', IAC, SE]);
        assert_eq!(reply, expect);
        assert!(data.is_empty());
    }

    #[test]
    fn requested_options_are_not_answered_twice() {
        let mut t = Telnet::new("ANSI", 80, 25);
        let init = t.initial();
        assert!(init.windows(3).any(|w| w == [IAC, WILL, OPT_BINARY]));
        assert!(init.windows(3).any(|w| w == [IAC, DO, OPT_BINARY]));
        let (mut data, mut reply) = (Vec::new(), Vec::new());
        t.receive(&[IAC, DO, OPT_BINARY, IAC, WILL, OPT_BINARY, IAC, DONT, OPT_TTYPE], &mut data, &mut reply);
        assert!(reply.is_empty(), "{reply:?}");
        assert!(t.binary_out());
        // NAWS isn't sent until the server agrees to it.
        assert!(t.naws().is_empty());
        t.receive(&[IAC, DO, OPT_NAWS], &mut data, &mut reply);
        assert_eq!(reply, vec![IAC, SB, OPT_NAWS, 0, 80, 0, 25, IAC, SE]);
    }

    #[test]
    fn escapes_output() {
        let t = Telnet::new("ANSI", 80, 25);
        let mut out = Vec::new();
        t.escape(&[1, IAC, b'\r'], &mut out);
        assert_eq!(out, vec![1, IAC, IAC, b'\r', 0]);
    }
}

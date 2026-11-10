//! 256-color palette and theme handling.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn from_hex(s: &str) -> Option<Rgb> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }

    /// Parse an X11 color spec as used by OSC 4/10/11: `rgb:RR/GG/BB` (1-4
    /// hex digits per channel) or `#RRGGBB`.
    pub fn from_x11(spec: &str) -> Option<Rgb> {
        if let Some(rest) = spec.strip_prefix("rgb:") {
            let mut it = rest.split('/');
            let mut chan = || -> Option<u8> {
                let p = it.next()?;
                if p.is_empty() || p.len() > 4 {
                    return None;
                }
                let v = u32::from_str_radix(p, 16).ok()?;
                let max = (1u32 << (4 * p.len())) - 1;
                Some((v * 255 / max) as u8)
            };
            let (r, g, b) = (chan()?, chan()?, chan()?);
            return Some(Rgb(r, g, b));
        }
        if spec.starts_with('#') {
            return Rgb::from_hex(spec);
        }
        None
    }

    pub fn to_x11(self) -> String {
        format!("rgb:{:04x}/{:04x}/{:04x}", u16::from(self.0) * 257, u16::from(self.1) * 257, u16::from(self.2) * 257)
    }
}

/// Named color schemes for the 16 base colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Scheme {
    /// Classic IBM CGA/VGA text mode colors (the BBS look).
    #[default]
    Vga,
    /// xterm's default colors.
    Xterm,
    /// Green phosphor monochrome.
    GreenPhosphor,
    /// Amber phosphor monochrome.
    AmberPhosphor,
}

impl Scheme {
    pub const ALL: [Scheme; 4] = [Scheme::Vga, Scheme::Xterm, Scheme::GreenPhosphor, Scheme::AmberPhosphor];

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Vga => "VGA (classic BBS)",
            Scheme::Xterm => "xterm",
            Scheme::GreenPhosphor => "Green phosphor",
            Scheme::AmberPhosphor => "Amber phosphor",
        }
    }

    fn base16(self) -> [Rgb; 16] {
        match self {
            Scheme::Vga => [
                Rgb(0x00, 0x00, 0x00),
                Rgb(0xAA, 0x00, 0x00),
                Rgb(0x00, 0xAA, 0x00),
                Rgb(0xAA, 0x55, 0x00),
                Rgb(0x00, 0x00, 0xAA),
                Rgb(0xAA, 0x00, 0xAA),
                Rgb(0x00, 0xAA, 0xAA),
                Rgb(0xAA, 0xAA, 0xAA),
                Rgb(0x55, 0x55, 0x55),
                Rgb(0xFF, 0x55, 0x55),
                Rgb(0x55, 0xFF, 0x55),
                Rgb(0xFF, 0xFF, 0x55),
                Rgb(0x55, 0x55, 0xFF),
                Rgb(0xFF, 0x55, 0xFF),
                Rgb(0x55, 0xFF, 0xFF),
                Rgb(0xFF, 0xFF, 0xFF),
            ],
            Scheme::Xterm => [
                Rgb(0x00, 0x00, 0x00),
                Rgb(0xCD, 0x00, 0x00),
                Rgb(0x00, 0xCD, 0x00),
                Rgb(0xCD, 0xCD, 0x00),
                Rgb(0x00, 0x00, 0xEE),
                Rgb(0xCD, 0x00, 0xCD),
                Rgb(0x00, 0xCD, 0xCD),
                Rgb(0xE5, 0xE5, 0xE5),
                Rgb(0x7F, 0x7F, 0x7F),
                Rgb(0xFF, 0x00, 0x00),
                Rgb(0x00, 0xFF, 0x00),
                Rgb(0xFF, 0xFF, 0x00),
                Rgb(0x5C, 0x5C, 0xFF),
                Rgb(0xFF, 0x00, 0xFF),
                Rgb(0x00, 0xFF, 0xFF),
                Rgb(0xFF, 0xFF, 0xFF),
            ],
            Scheme::GreenPhosphor => Self::mono(Rgb(0x33, 0xFF, 0x66)),
            Scheme::AmberPhosphor => Self::mono(Rgb(0xFF, 0xB0, 0x00)),
        }
    }

    /// A monochrome ramp: dark colors dim, bright colors full intensity.
    fn mono(c: Rgb) -> [Rgb; 16] {
        let scale = |pct: u16| {
            Rgb(
                (u16::from(c.0) * pct / 100) as u8,
                (u16::from(c.1) * pct / 100) as u8,
                (u16::from(c.2) * pct / 100) as u8,
            )
        };
        let mut out = [Rgb(0, 0, 0); 16];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = match i {
                0 => Rgb(0, 0, 0),
                7 => scale(75),
                8 => scale(40),
                15 => scale(100),
                1..=6 => scale(60),
                _ => scale(90),
            };
        }
        out
    }
}

/// The full 256-entry palette plus default foreground/background/cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub colors: [Rgb; 256],
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
}

impl Palette {
    pub fn new(scheme: Scheme) -> Palette {
        let mut colors = [Rgb(0, 0, 0); 256];
        colors[..16].copy_from_slice(&scheme.base16());
        let levels = [0u8, 95, 135, 175, 215, 255];
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    colors[16 + r * 36 + g * 6 + b] = Rgb(levels[r], levels[g], levels[b]);
                }
            }
        }
        for i in 0..24 {
            let v = 8 + 10 * i as u8;
            colors[232 + i] = Rgb(v, v, v);
        }
        let (fg, bg, cursor) = match scheme {
            Scheme::Vga => (colors[7], colors[0], Rgb(0xAA, 0xAA, 0xAA)),
            Scheme::Xterm => (colors[7], colors[0], Rgb(0xE5, 0xE5, 0xE5)),
            Scheme::GreenPhosphor => (colors[7], Rgb(0x02, 0x0A, 0x04), colors[15]),
            Scheme::AmberPhosphor => (colors[7], Rgb(0x0A, 0x06, 0x00), colors[15]),
        };
        Palette { colors, fg, bg, cursor }
    }
}

//! Character set translation: CP437 (IBM PC / BBS) and DEC special graphics.

/// IBM code page 437, bytes 0x80..=0xFF.
pub const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Translate a CP437 byte stream into UTF-8 so the (UTF-8) parser can consume
/// it. Control characters and ASCII pass through unchanged.
pub fn cp437_to_utf8(input: &[u8], out: &mut Vec<u8>) {
    out.reserve(input.len());
    let mut buf = [0u8; 4];
    for &b in input {
        if b < 0x80 {
            out.push(b);
        } else {
            out.extend_from_slice(CP437_HIGH[(b - 0x80) as usize].encode_utf8(&mut buf).as_bytes());
        }
    }
}

/// Encode a char typed by the user into CP437 (for sending). Returns None for
/// characters that have no CP437 representation.
pub fn char_to_cp437(c: char) -> Option<u8> {
    if (c as u32) < 0x80 {
        return Some(c as u8);
    }
    CP437_HIGH.iter().position(|&x| x == c).map(|i| 0x80 + i as u8)
}

/// Translate ISO-8859-1 bytes into UTF-8.
pub fn latin1_to_utf8(input: &[u8], out: &mut Vec<u8>) {
    out.reserve(input.len());
    let mut buf = [0u8; 4];
    for &b in input {
        if b < 0x80 {
            out.push(b);
        } else {
            // 0x80..=0x9F become UTF-8 encoded C1 controls, which the parser
            // still treats as controls.
            out.extend_from_slice(char::from(b).encode_utf8(&mut buf).as_bytes());
        }
    }
}

/// G0..G3 designations we understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecial,
    Uk,
}

impl Charset {
    #[inline]
    pub fn map(self, c: char) -> char {
        match self {
            Charset::Ascii => c,
            Charset::Uk => {
                if c == '#' {
                    '£'
                } else {
                    c
                }
            }
            Charset::DecSpecial => match c {
                '_' => ' ',
                '`' => '◆',
                'a' => '▒',
                'b' => '␉',
                'c' => '␌',
                'd' => '␍',
                'e' => '␊',
                'f' => '°',
                'g' => '±',
                'h' => '␤',
                'i' => '␋',
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                _ => c,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp437_roundtrip() {
        for b in 0x80u8..=0xFF {
            let c = CP437_HIGH[(b - 0x80) as usize];
            assert_eq!(char_to_cp437(c), Some(b));
        }
        let mut out = Vec::new();
        cp437_to_utf8(&[b'A', 0xDB, 0xB0], &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "A█░");
    }
}

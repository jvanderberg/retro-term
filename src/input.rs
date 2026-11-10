//! Keyboard and mouse → byte sequences for the host.

use egui::{Key, Modifiers};

use crate::term::{Modes, MouseMode};

pub struct KeyOptions {
    pub backspace_sends_del: bool,
    pub option_as_meta: bool,
}

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
fn modifier_param(m: &Modifiers) -> u8 {
    1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl)
}

fn letter(key: Key) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_none() && c.is_ascii_alphanumeric() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// Translate a key press into bytes. Returns `None` when the key should be
/// handled as text (via `Event::Text`) or not at all.
pub fn key_bytes(key: Key, mods: &Modifiers, modes: &Modes, opts: &KeyOptions) -> Option<Vec<u8>> {
    // On macOS the Command key is reserved for application shortcuts.
    if mods.mac_cmd {
        return None;
    }
    let mp = modifier_param(mods);
    let has_mods = mp > 1;
    let csi_letter = |c: char, ss3_when_app: bool| -> Vec<u8> {
        if has_mods {
            format!("\x1b[1;{mp}{c}").into_bytes()
        } else if ss3_when_app {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if has_mods {
            format!("\x1b[{n};{mp}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let app = modes.app_cursor;
    let bytes = match key {
        Key::ArrowUp => csi_letter('A', app),
        Key::ArrowDown => csi_letter('B', app),
        Key::ArrowRight => csi_letter('C', app),
        Key::ArrowLeft => csi_letter('D', app),
        Key::Home => csi_letter('H', app),
        Key::End => csi_letter('F', app),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => csi_letter('P', true),
        Key::F2 => csi_letter('Q', true),
        Key::F3 => csi_letter('R', true),
        Key::F4 => csi_letter('S', true),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        Key::Enter => {
            let mut v = if mods.alt { vec![0x1b] } else { Vec::new() };
            if modes.newline {
                v.extend_from_slice(b"\r\n");
            } else {
                v.push(b'\r');
            }
            v
        }
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::Backspace => {
            let b = if opts.backspace_sends_del != mods.ctrl { 0x7f } else { 0x08 };
            if mods.alt {
                vec![0x1b, b]
            } else {
                vec![b]
            }
        }
        _ => {
            if mods.ctrl && !mods.alt {
                ctrl_key(key, mods.shift)?
            } else if mods.alt && opts.option_as_meta {
                let c = letter(key)?;
                let c = if mods.shift { c.to_ascii_uppercase() } else { c };
                if mods.ctrl {
                    vec![0x1b, (c as u8) & 0x1f]
                } else {
                    vec![0x1b, c as u8]
                }
            } else {
                return None;
            }
        }
    };
    Some(bytes)
}

fn ctrl_key(key: Key, shift: bool) -> Option<Vec<u8>> {
    let b = match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Slash => 0x1f,
        Key::Num3 => 0x1b,
        Key::Num4 => 0x1c,
        Key::Num5 => 0x1d,
        Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => {
            let c = letter(key)?;
            if !c.is_ascii_lowercase() {
                return None;
            }
            // Ctrl+Shift+letter is reserved for the app (copy/paste) off macOS.
            if shift && !cfg!(target_os = "macos") {
                return None;
            }
            (c as u8) & 0x1f
        }
    };
    Some(vec![b])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press(u8),
    Release(u8),
    /// Motion with the given button held (3 = none).
    Motion(u8),
    WheelUp,
    WheelDown,
}

/// Encode a mouse event for the host according to the active mouse mode.
/// `col`/`row` are 0-based cell coordinates.
pub fn mouse_report(modes: &Modes, action: MouseAction, col: usize, row: usize, mods: &Modifiers) -> Option<Vec<u8>> {
    let mode = modes.mouse;
    if mode == MouseMode::Off {
        return None;
    }
    let (mut code, release) = match action {
        MouseAction::Press(b) => (b, false),
        MouseAction::Release(b) => (if modes.mouse_sgr { b } else { 3 }, true),
        MouseAction::Motion(b) => {
            let allowed = match mode {
                MouseMode::AnyMotion => true,
                MouseMode::ButtonMotion => b != 3,
                _ => false,
            };
            if !allowed {
                return None;
            }
            (b + 32, false)
        }
        MouseAction::WheelUp => (64, false),
        MouseAction::WheelDown => (65, false),
    };
    if mode == MouseMode::X10 {
        if !matches!(action, MouseAction::Press(_)) {
            return None;
        }
    } else {
        code += 4 * u8::from(mods.shift) + 8 * u8::from(mods.alt) + 16 * u8::from(mods.ctrl);
    }
    let (x, y) = (col + 1, row + 1);
    if modes.mouse_sgr {
        let fin = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes());
    }
    let enc = |v: usize, out: &mut Vec<u8>| {
        let v = v + 32;
        if modes.mouse_utf8 && v >= 128 {
            let c = char::from_u32(v.min(2047) as u32).unwrap_or(' ');
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            out.push(v.min(255) as u8);
        }
    };
    let mut out = b"\x1b[M".to_vec();
    out.push(code + 32);
    enc(x, &mut out);
    enc(y, &mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> KeyOptions {
        KeyOptions { backspace_sends_del: true, option_as_meta: false }
    }

    #[test]
    fn arrows_respect_app_mode() {
        let mut modes = Modes::default();
        let none = Modifiers::NONE;
        assert_eq!(key_bytes(Key::ArrowUp, &none, &modes, &opts()).unwrap(), b"\x1b[A");
        modes.app_cursor = true;
        assert_eq!(key_bytes(Key::ArrowUp, &none, &modes, &opts()).unwrap(), b"\x1bOA");
        let shift = Modifiers { shift: true, ..Modifiers::NONE };
        assert_eq!(key_bytes(Key::ArrowUp, &shift, &modes, &opts()).unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn control_letters() {
        let ctrl = Modifiers { ctrl: true, ..Modifiers::NONE };
        let modes = Modes::default();
        assert_eq!(key_bytes(Key::C, &ctrl, &modes, &opts()).unwrap(), vec![3]);
        assert_eq!(key_bytes(Key::X, &ctrl, &modes, &opts()).unwrap(), vec![0x18]);
        assert_eq!(key_bytes(Key::OpenBracket, &ctrl, &modes, &opts()).unwrap(), vec![0x1b]);
        assert!(key_bytes(Key::C, &Modifiers::NONE, &modes, &opts()).is_none());
    }

    #[test]
    fn sgr_mouse() {
        let modes = Modes { mouse: MouseMode::Normal, mouse_sgr: true, ..Modes::default() };
        let m = Modifiers::NONE;
        assert_eq!(mouse_report(&modes, MouseAction::Press(0), 4, 9, &m).unwrap(), b"\x1b[<0;5;10M");
        assert_eq!(mouse_report(&modes, MouseAction::Release(0), 4, 9, &m).unwrap(), b"\x1b[<0;5;10m");
        assert!(mouse_report(&modes, MouseAction::Motion(3), 4, 9, &m).is_none());
    }
}

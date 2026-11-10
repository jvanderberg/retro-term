//! Persistent settings and the phonebook (dialing directory).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::conn::ConnSpec;
use crate::term::{Encoding, Scheme};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhonebookEntry {
    pub name: String,
    pub spec: ConnSpec,
    /// Character encoding to switch to when dialing this entry.
    #[serde(default)]
    pub encoding: Option<Encoding>,
    /// Terminal type to announce (telnet TTYPE / $TERM); empty = default.
    #[serde(default)]
    pub term_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    pub scheme: Scheme,
    pub encoding: Encoding,
    pub scrollback: usize,
    pub download_dir: PathBuf,
    /// Backspace sends DEL (0x7f) rather than BS (0x08).
    pub backspace_sends_del: bool,
    /// Bold text uses the bright variant of the 8 basic colors.
    pub bold_is_bright: bool,
    /// Blink attribute selects a bright background (iCE colors), as on BBSes.
    pub ice_colors: bool,
    pub local_echo: bool,
    /// Treat Option/Alt as Meta (ESC prefix).
    pub option_as_meta: bool,
    /// $TERM / telnet terminal type.
    pub term_type: String,
    /// Draw box-drawing and block characters geometrically for seamless art.
    pub builtin_box_drawing: bool,
    /// Automatically start ZMODEM/Kermit when the remote starts one.
    pub auto_transfer: bool,
    /// Let the remote set the clipboard with OSC 52. Off by default: a
    /// hostile host could plant a command for the user to paste.
    pub allow_osc52: bool,
    pub phonebook: Vec<PhonebookEntry>,
}

impl Default for Settings {
    fn default() -> Self {
        let download_dir = directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        Settings {
            font_size: 15.0,
            scheme: Scheme::Vga,
            encoding: Encoding::Utf8,
            scrollback: 10_000,
            download_dir,
            backspace_sends_del: true,
            bold_is_bright: true,
            ice_colors: false,
            local_echo: false,
            option_as_meta: false,
            term_type: "xterm-256color".into(),
            builtin_box_drawing: true,
            auto_transfer: true,
            allow_osc52: false,
            phonebook: vec![PhonebookEntry {
                name: "Example BBS (telnet)".into(),
                spec: ConnSpec::Telnet { host: "bbs.example.org".into(), port: 23 },
                encoding: Some(Encoding::Cp437),
                term_type: "ANSI".into(),
            }],
        }
    }
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "retro-term").map(|d| d.config_dir().join("settings.toml"))
    }

    pub fn load() -> Settings {
        let Some(path) = Self::path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse_or_backup(&text, &path),
            Err(_) => Settings::default(),
        }
    }

    /// Parse settings; if they can't be read, move the file aside so the
    /// next save (which writes defaults) doesn't destroy the phonebook.
    fn parse_or_backup(text: &str, path: &Path) -> Settings {
        match toml::from_str(text) {
            Ok(s) => s,
            Err(e) => {
                let backup = path.with_extension("toml.bad");
                log::warn!("could not parse {}: {e}; moved it to {}", path.display(), backup.display());
                let _ = std::fs::rename(path, &backup);
                Settings::default()
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Err(e) = self.save_to(&path) {
            log::warn!("could not save settings to {}: {e}", path.display());
        }
    }

    /// Write atomically: a crash mid-write leaves the old file intact.
    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn unreadable_settings_are_backed_up_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut s = Settings::default();
        s.phonebook[0].name = "Mine".into();
        s.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap().replace("scheme = \"Vga\"", "scheme = \"Future\"");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(Settings::parse_or_backup(&text, &path), Settings::default());
        assert!(!path.exists());
        assert!(std::fs::read_to_string(dir.path().join("settings.toml.bad")).unwrap().contains("Mine"));
    }
}

//! User settings (hidden devices, hide-last-known), stored as a small text file:
//! `hidden=<device key>` lines and `hide_last_known=1`. Port of the registry values in main.cpp.

use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub hidden: BTreeSet<String>,
    pub hide_last_known: bool,
    /// Windows: show one strip on the taskbar (default) instead of one tray icon per device.
    pub taskbar_widget: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { hidden: BTreeSet::new(), hide_last_known: false, taskbar_widget: true }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("hidden", k)) if !k.is_empty() => {
                    s.hidden.insert(k.to_string());
                }
                Some(("hide_last_known", v)) => s.hide_last_known = v.trim() == "1",
                Some(("taskbar_widget", v)) => s.taskbar_widget = v.trim() != "0",
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for k in &self.hidden {
            out.push_str(&format!("hidden={k}\n"));
        }
        out.push_str(&format!("hide_last_known={}\n", u8::from(self.hide_last_known)));
        out.push_str(&format!("taskbar_widget={}\n", u8::from(self.taskbar_widget)));
        out
    }

    /// Flips a device between hidden and shown.
    pub fn toggle_hidden(&mut self, key: &str) {
        if !self.hidden.remove(key) {
            self.hidden.insert(key.to_string());
        }
    }

    pub fn load() -> Self {
        path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Self::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, self.to_text());
        }
    }
}

/// Per-user folder for settings and the instance lock.
pub fn dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(base.join("WirelessBattery"))
}

fn path() -> Option<PathBuf> {
    Some(dir()?.join("settings.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut s = Settings::default();
        s.hidden.insert("glorious:2022".into());
        s.hidden.insert("xbox:1".into());
        s.hide_last_known = true;
        s.taskbar_widget = false;
        assert_eq!(Settings::parse(&s.to_text()), s);
    }

    #[test]
    fn ignores_garbage_and_toggles() {
        let mut s = Settings::parse("nonsense\nhidden=\nfoo=bar\nhidden=a\n");
        assert_eq!(s.hidden.len(), 1);
        assert!(!s.hide_last_known);
        assert!(s.taskbar_widget, "widget is on unless switched off");
        s.toggle_hidden("a");
        assert!(s.hidden.is_empty());
        s.toggle_hidden("b");
        assert!(s.hidden.contains("b"));
    }
}

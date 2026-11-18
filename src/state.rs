//! Tracks devices across polls (port of the TrayEntry logic in main.cpp): remembers the last known
//! percentage, keeps sleeping devices around for a grace period and decides when to warn.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::device::{DeviceKind, DeviceStatus};
use crate::settings::Settings;

pub const LOW_BATTERY: u8 = 15;

pub struct Entry {
    pub st: DeviceStatus,
    pub last_pct: Option<u8>,
    last_seen: Instant,
    low_warned: bool,
    /// A higher reading seen once while discharging; only believed if it repeats.
    rise: Option<u8>,
}

impl Entry {
    /// Percentage to display: the live value, else the last known one.
    pub fn shown(&self) -> Option<u8> {
        self.st.percent.or(self.last_pct)
    }

    pub fn is_low(&self) -> bool {
        !self.st.charging && self.st.percent.is_some_and(|p| p <= LOW_BATTERY)
    }
}

pub struct Notice {
    pub title: String,
    pub body: String,
}

#[derive(Default)]
pub struct Tracker {
    pub entries: BTreeMap<String, Entry>,
}

impl Tracker {
    /// Merges one poll. Returns the "low battery" notices to show.
    pub fn on_results(&mut self, results: Vec<DeviceStatus>, now: Instant) -> Vec<Notice> {
        let mut notices = Vec::new();
        let mut seen = Vec::new();
        for s in results {
            seen.push(s.key.clone());
            let e = self.entries.entry(s.key.clone()).or_insert_with(|| Entry {
                st: s.clone(),
                last_pct: None,
                last_seen: now,
                low_warned: false,
                rise: None,
            });
            let s = steady(e, s);
            e.st = s;
            if e.st.percent.is_some() {
                e.last_pct = e.st.percent;
            }
            e.last_seen = now;
            notices.extend(low_notice(e));
        }
        self.entries.retain(|k, e| {
            if seen.contains(k) {
                return true;
            }
            // Some receivers drop sleeping devices from the link; keep the last value for a while.
            let in_grace = e.st.grace_sec > 0
                && e.st.connected
                && now.duration_since(e.last_seen) < Duration::from_secs(e.st.grace_sec as u64);
            if in_grace {
                e.st.percent = None;
                e.st.asleep = true;
            }
            in_grace
        });
        notices
    }

    /// Connected, not hidden devices: headsets first, then mice, keyboards, gamepads, others.
    pub fn visible(&self, settings: &Settings) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.entries.values().filter(|e| e.st.connected && !is_hidden(e, settings)).collect();
        v.sort_by(|a, b| {
            kind_order(a.st.kind).cmp(&kind_order(b.st.kind)).then_with(|| a.st.name.cmp(&b.st.name))
        });
        v
    }
}

/// Batteries do not charge themselves: while neither the old nor the new reading is charging, a
/// jump up is usually sensor noise under load (70 / 80 / 70). Keep the old value unless the
/// higher one is read twice in a row.
fn steady(e: &mut Entry, mut s: DeviceStatus) -> DeviceStatus {
    match (e.st.percent, s.percent) {
        (Some(old), Some(new)) if new > old && !e.st.charging && !s.charging => {
            if e.rise == Some(new) {
                e.rise = None;
            } else {
                e.rise = Some(new);
                s.percent = Some(old);
            }
        }
        _ => e.rise = None,
    }
    s
}

fn low_notice(e: &mut Entry) -> Option<Notice> {
    let shown_high = e.shown().is_some_and(|p| p > LOW_BATTERY + 10);
    if e.st.charging || shown_high {
        e.low_warned = false;
    }
    if e.is_low() && !e.low_warned {
        e.low_warned = true;
        return Some(Notice {
            title: "Low battery".into(),
            body: format!("{} is at {}%", e.st.name, e.st.percent.unwrap_or(0)),
        });
    }
    None
}

pub fn kind_order(k: DeviceKind) -> u8 {
    match k {
        DeviceKind::Headset => 0,
        DeviceKind::Mouse => 1,
        DeviceKind::Keyboard => 2,
        DeviceKind::Gamepad => 3,
        DeviceKind::Other => 4,
    }
}

pub fn is_hidden(e: &Entry, s: &Settings) -> bool {
    s.hidden.contains(&e.st.key)
        || (s.hide_last_known && (e.st.asleep || (e.st.percent.is_none() && e.last_pct.is_some())))
}

pub fn status_text(e: &Entry) -> String {
    let s = &e.st;
    if !s.connected {
        return "off / not connected".into();
    }
    let mut t = match (s.percent, e.last_pct) {
        (Some(_), _) if !s.level_text.is_empty() => s.level_text.clone(),
        (Some(p), _) => format!("{p}%"),
        (None, Some(l)) => format!("{l}% (last known)"),
        (None, None) => "unknown".into(),
    };
    if s.charging {
        t += ", charging";
    }
    if s.asleep {
        t += ", asleep";
    }
    t
}

/// One-line summary for the macOS menu bar, e.g. "🎧 30%  🖱 93%⚡".
pub fn summary(entries: &[&Entry]) -> String {
    if entries.is_empty() {
        return "🔋 –".into();
    }
    entries
        .iter()
        .map(|e| {
            let glyph = match e.st.kind {
                DeviceKind::Headset => "🎧",
                DeviceKind::Mouse => "🖱",
                DeviceKind::Keyboard => "⌨",
                DeviceKind::Gamepad => "🎮",
                DeviceKind::Other => "🔋",
            };
            let pct = match e.shown() {
                Some(p) if e.st.percent.is_none() => format!("~{p}%"), // last known
                Some(p) => format!("{p}%"),
                None => "?".into(),
            };
            let tail = if e.st.charging { "⚡" } else if e.is_low() { "⚠" } else { "" };
            format!("{glyph} {pct}{tail}")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

pub fn tooltip(entries: &[&Entry]) -> String {
    if entries.is_empty() {
        return "Wireless Battery: no devices with battery info found".into();
    }
    entries.iter().map(|e| format!("{}: {}", e.st.name, status_text(e))).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(key: &str, pct: Option<u8>) -> DeviceStatus {
        DeviceStatus { key: key.into(), name: key.to_uppercase(), percent: pct, ..Default::default() }
    }

    #[test]
    fn remembers_last_percent_when_device_sleeps() {
        let mut t = Tracker::default();
        let now = Instant::now();
        t.on_results(vec![dev("a", Some(70))], now);
        let mut asleep = dev("a", None);
        asleep.asleep = true;
        t.on_results(vec![asleep], now);
        let e = &t.entries["a"];
        assert_eq!(e.shown(), Some(70));
        assert_eq!(status_text(e), "70% (last known), asleep");
    }

    #[test]
    fn flicker_upwards_is_ignored_unless_repeated() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        for (p, want) in [(70, 70), (80, 70), (70, 70), (80, 70), (80, 80), (60, 60)] {
            t.on_results(vec![dev("a", Some(p))], t0);
            assert_eq!(t.entries["a"].st.percent, Some(want), "reading {p}");
        }
        // charging may rise freely
        let mut c = dev("a", Some(70));
        c.charging = true;
        t.on_results(vec![c], t0);
        let mut c = dev("a", Some(90));
        c.charging = true;
        t.on_results(vec![c], t0);
        assert_eq!(t.entries["a"].st.percent, Some(90));
    }

    #[test]
    fn low_battery_warns_once_until_recharged() {
        let mut t = Tracker::default();
        let now = Instant::now();
        assert_eq!(t.on_results(vec![dev("a", Some(15))], now).len(), 1);
        assert_eq!(t.on_results(vec![dev("a", Some(14))], now).len(), 0);
        // Recovers above LOW + 10 (a new battery: believed on the second reading), so the next dip warns again.
        t.on_results(vec![dev("a", Some(60))], now);
        t.on_results(vec![dev("a", Some(60))], now);
        assert_eq!(t.on_results(vec![dev("a", Some(10))], now).len(), 1);
    }

    #[test]
    fn charging_never_warns_and_rearms() {
        let mut t = Tracker::default();
        let now = Instant::now();
        t.on_results(vec![dev("a", Some(10))], now);
        let mut c = dev("a", Some(10));
        c.charging = true;
        assert_eq!(t.on_results(vec![c], now).len(), 0);
        assert_eq!(t.on_results(vec![dev("a", Some(9))], now).len(), 1);
    }

    #[test]
    fn missing_device_is_dropped_unless_in_grace() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut g = dev("g", Some(50));
        g.grace_sec = 60;
        t.on_results(vec![g, dev("x", Some(50))], t0);
        t.on_results(vec![], t0 + Duration::from_secs(10));
        assert!(t.entries.contains_key("g") && !t.entries.contains_key("x"));
        let e = &t.entries["g"];
        assert!(e.st.asleep && e.st.percent.is_none() && e.shown() == Some(50));
        t.on_results(vec![], t0 + Duration::from_secs(120));
        assert!(t.entries.is_empty());
    }

    #[test]
    fn visible_sorted_and_filtered() {
        let mut t = Tracker::default();
        let mut mouse = dev("m", Some(50));
        mouse.kind = DeviceKind::Mouse;
        let mut head = dev("h", Some(50));
        head.kind = DeviceKind::Headset;
        let mut off = dev("o", Some(50));
        off.connected = false;
        t.on_results(vec![mouse, head, off, dev("z", Some(5))], Instant::now());
        let mut s = Settings::default();
        let keys: Vec<_> = t.visible(&s).iter().map(|e| e.st.key.clone()).collect();
        assert_eq!(keys, ["h", "m", "z"]);
        s.hidden.insert("m".into());
        assert_eq!(t.visible(&s).len(), 2);
    }

    #[test]
    fn hide_last_known_hides_asleep_devices() {
        let mut t = Tracker::default();
        let mut a = dev("a", Some(50));
        a.asleep = true;
        t.on_results(vec![a], Instant::now());
        let mut s = Settings::default();
        assert_eq!(t.visible(&s).len(), 1);
        s.hide_last_known = true;
        assert_eq!(t.visible(&s).len(), 0);
    }

    #[test]
    fn summary_marks_charging_low_and_last_known() {
        let mut t = Tracker::default();
        let mut m = dev("m", Some(93));
        m.kind = DeviceKind::Mouse;
        m.charging = true;
        let mut h = dev("h", Some(10));
        h.kind = DeviceKind::Headset;
        t.on_results(vec![m, h], Instant::now());
        let v = t.visible(&Settings::default());
        assert_eq!(summary(&v), "🎧 10%⚠  🖱 93%⚡");
        assert_eq!(summary(&[]), "🔋 –");
    }
}

//! Sony DualShock 4 / DualSense / DualSense Edge (VID 054C), USB or Bluetooth.
//! The battery is part of the regular input report:
//!   DualShock 4: USB report 01 byte 30, Bluetooth report 11 byte 32: bits 0-3 level, bit 4 cable
//!   DualSense:   USB report 01 byte 53, Bluetooth report 31 byte 54: bits 0-3 level (0-10), bits 4-7 state
//! Over Bluetooth both controllers send a reduced report until a feature report is read, so we read
//! the calibration report (05) once, which is what games do too.
//! Layout reference: Linux drivers/hid/hid-playstation.c.
//!
//! Cross-platform note: Bluetooth is detected like the original, by an input report length above 64
//! (from the report descriptor). If the length is unknown (0) the controller is treated as USB.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4, is_bluetooth, HidDev, HidIo};

fn is_ds4(pid: u16) -> bool {
    pid == 0x05C4 || pid == 0x09CC || pid == 0x0BA0
}

fn is_dualsense(pid: u16) -> bool {
    pid == 0x0CE6 || pid == 0x0DF2
}

#[derive(Debug, PartialEq, Eq)]
enum Decoded {
    /// Not a usable battery report: keep reading.
    Skip,
    /// DS4 wireless adapter without a controller: give up on this collection.
    NoController,
    Found { percent: u8, charging: bool },
}

/// Decodes one input report (`r[0]` = report id) for the given product.
fn decode(pid: u16, bt: bool, r: &[u8]) -> Decoded {
    if r.is_empty() {
        return Decoded::Skip;
    }
    if is_ds4(pid) {
        let at = match r[0] {
            0x01 => 30,
            0x11 => 32,
            _ => 0,
        };
        if at == 0 || r.len() <= at || (r[0] == 0x01 && bt) {
            return Decoded::Skip;
        }
        let level = (r[at] & 0x0F) as i32;
        let cable = r[at] & 0x10 != 0;
        if pid == 0x0BA0 && r[at] == 0 {
            return Decoded::NoController;
        }
        let percent = if cable {
            if level >= 11 { 100 } else { clamp100(level * 10) }
        } else {
            clamp100(level * 10 + 5)
        };
        Decoded::Found { percent, charging: cable && level < 11 }
    } else {
        let at = match r[0] {
            0x01 => 53,
            0x31 => 54,
            _ => 0,
        };
        if at == 0 || r.len() <= at || (r[0] == 0x01 && bt) {
            return Decoded::Skip;
        }
        let level = (r[at] & 0x0F) as i32;
        let state = r[at] >> 4;
        let mut percent = clamp100(level * 10 + 5);
        let mut charging = state == 0x1;
        if state == 0x2 {
            percent = 100;
            charging = true;
        }
        Decoded::Found { percent, charging }
    }
}

/// Waits up to `total_ms` for a battery report. None = none arrived (or no controller behind the adapter).
fn read_battery(io: &mut impl HidIo, pid: u16, bt: bool, total_ms: u64) -> Option<(u8, bool)> {
    let end = Instant::now() + Duration::from_millis(total_ms);
    while !end.saturating_duration_since(Instant::now()).is_zero() {
        let Some(r) = io.read(100) else { continue };
        if r.len() < 10 {
            continue;
        }
        match decode(pid, bt, &r) {
            Decoded::Skip => continue,
            Decoded::NoController => return None,
            Decoded::Found { percent, charging } => return Some((percent, charging)),
        }
    }
    None
}

/// FNV-1a of the device path: stable identity per physical controller.
fn path_hash(path: &[u8]) -> u32 {
    let mut h: u32 = 2166136261;
    for &b in path {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

fn model_name(pid: u16) -> &'static str {
    if is_ds4(pid) {
        "DualShock 4"
    } else if pid == 0x0DF2 {
        "DualSense Edge"
    } else {
        "DualSense"
    }
}

fn build_status(pid: u16, bt: bool, path: &[u8], percent: u8, charging: bool) -> DeviceStatus {
    DeviceStatus {
        key: format!("ps:{}:{}", hex4(pid), path_hash(path)),
        name: model_name(pid).into(),
        kind: DeviceKind::Gamepad,
        via: if bt {
            "Bluetooth"
        } else if pid == 0x0BA0 {
            "Wireless adapter"
        } else {
            "USB cable"
        }
        .into(),
        percent: Some(percent),
        charging,
        ..Default::default()
    }
}

pub fn poll(ctx: &mut PollContext) {
    let colls = ctx.colls.clone();
    for c in &colls {
        if c.vid != 0x054C || !(is_ds4(c.pid) || is_dualsense(c.pid)) {
            continue;
        }
        if c.usage_page != 0x01 || c.usage != 0x05 {
            continue; // the gamepad collection
        }
        let Some(mut d) = HidDev::open(ctx.api, c) else { continue };
        let bt = detect_bluetooth(c.in_len, is_bluetooth(c));
        if bt {
            let _ = d.get_feature_default(0x05); // switches the controller to full reports
        }
        let Some((pct, charging)) = read_battery(&mut d, c.pid, bt, 500) else { continue };
        let s = build_status(c.pid, bt, c.path.as_bytes(), pct, charging);
        ctx.out.push(s);
        ctx.claim(c.vid, c.pid);
    }
}

/// Bluetooth reports are longer than 64 bytes (the original's rule). When the descriptor could not
/// be read (`in_len == 0`, as on macOS without Input Monitoring), fall back to the bus hidapi reports.
fn detect_bluetooth(in_len: u16, bus_is_bluetooth: bool) -> bool {
    if in_len == 0 {
        bus_is_bluetooth
    } else {
        in_len > 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;

    struct Mock {
        replies: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        flushed: u32,
        accept_write: bool,
    }

    impl Mock {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Self { replies, sent: vec![], flushed: 0, accept_write: true }
        }
    }

    impl FeatureIo for Mock {
        fn set_feature(&mut self, _d: &[u8]) -> bool {
            false
        }
        fn get_feature(&mut self, _id: u8, _len: usize) -> Option<Vec<u8>> {
            None
        }
    }

    impl HidIo for Mock {
        fn write(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            self.accept_write
        }
        fn read(&mut self, _t: i32) -> Option<Vec<u8>> {
            if self.replies.is_empty() { None } else { Some(self.replies.remove(0)) }
        }
        fn flush(&mut self) {
            self.flushed += 1;
        }
    }

    fn report(id: u8, at: usize, byte: u8) -> Vec<u8> {
        let mut r = vec![0u8; 80];
        r[0] = id;
        r[at] = byte;
        r
    }

    #[test]
    fn ds4_usb_battery_levels() {
        // on battery: level*10+5
        assert_eq!(decode(0x05C4, false, &report(0x01, 30, 0x07)), Decoded::Found { percent: 75, charging: false });
        assert_eq!(decode(0x05C4, false, &report(0x01, 30, 0x0A)), Decoded::Found { percent: 100, charging: false });
        // cable: level*10, charging until level 11 (full)
        assert_eq!(decode(0x09CC, false, &report(0x01, 30, 0x15)), Decoded::Found { percent: 50, charging: true });
        assert_eq!(decode(0x09CC, false, &report(0x01, 30, 0x1B)), Decoded::Found { percent: 100, charging: false });
    }

    #[test]
    fn ds4_bluetooth_uses_report_11_and_ignores_reduced_01() {
        assert_eq!(decode(0x05C4, true, &report(0x11, 32, 0x03)), Decoded::Found { percent: 35, charging: false });
        assert_eq!(decode(0x05C4, true, &report(0x01, 30, 0x03)), Decoded::Skip);
        assert_eq!(decode(0x05C4, false, &report(0x31, 54, 0x03)), Decoded::Skip);
    }

    #[test]
    fn ds4_adapter_without_controller_gives_up() {
        assert_eq!(decode(0x0BA0, false, &report(0x01, 30, 0x00)), Decoded::NoController);
        assert_eq!(decode(0x05C4, false, &report(0x01, 30, 0x00)), Decoded::Found { percent: 5, charging: false });
    }

    #[test]
    fn dualsense_levels_and_states() {
        assert_eq!(decode(0x0CE6, false, &report(0x01, 53, 0x05)), Decoded::Found { percent: 55, charging: false });
        assert_eq!(decode(0x0CE6, false, &report(0x01, 53, 0x14)), Decoded::Found { percent: 45, charging: true });
        assert_eq!(decode(0x0DF2, false, &report(0x01, 53, 0x20)), Decoded::Found { percent: 100, charging: true });
        assert_eq!(decode(0x0CE6, false, &report(0x01, 53, 0x2A)), Decoded::Found { percent: 100, charging: true });
        assert_eq!(decode(0x0CE6, false, &report(0x01, 53, 0x0F)), Decoded::Found { percent: 100, charging: false });
    }

    #[test]
    fn dualsense_bluetooth_uses_report_31() {
        assert_eq!(decode(0x0CE6, true, &report(0x31, 54, 0x08)), Decoded::Found { percent: 85, charging: false });
        assert_eq!(decode(0x0CE6, true, &report(0x01, 53, 0x08)), Decoded::Skip);
    }

    #[test]
    fn short_or_unknown_reports_are_skipped() {
        assert_eq!(decode(0x0CE6, false, &[0x01; 40]), Decoded::Skip); // too short for byte 53
        assert_eq!(decode(0x05C4, false, &[0x01; 30]), Decoded::Skip); // too short for byte 30
        assert_eq!(decode(0x0CE6, false, &report(0x07, 53, 0x05)), Decoded::Skip);
        assert_eq!(decode(0x0CE6, false, &[]), Decoded::Skip);
    }

    #[test]
    fn read_battery_skips_junk_until_a_battery_report() {
        let mut m = Mock::new(vec![vec![1, 2, 3], report(0x07, 0, 0), report(0x01, 53, 0x05)]);
        assert_eq!(read_battery(&mut m, 0x0CE6, false, 500), Some((55, false)));
        let mut m = Mock::new(vec![report(0x01, 30, 0x00)]);
        assert_eq!(read_battery(&mut m, 0x0BA0, false, 500), None); // no controller
        assert_eq!(read_battery(&mut Mock::new(vec![]), 0x0CE6, false, 0), None); // nothing arrives
    }

    #[test]
    fn status_fields_and_identity() {
        let s = build_status(0x0DF2, false, b"path-a", 80, true);
        assert_eq!((s.name.as_str(), s.via.as_str(), s.kind), ("DualSense Edge", "USB cable", DeviceKind::Gamepad));
        assert_eq!(s.percent, Some(80));
        assert!(s.charging);
        assert_eq!(build_status(0x0CE6, true, b"p", 1, false).via, "Bluetooth");
        assert_eq!(build_status(0x0BA0, false, b"p", 1, false).via, "Wireless adapter");
        assert_eq!(build_status(0x05C4, false, b"p", 1, false).name, "DualShock 4");
        assert_eq!(build_status(0x0CE6, false, b"p", 1, false).name, "DualSense");
        // same device -> same key, different path -> different key
        assert_eq!(build_status(0x0CE6, false, b"a", 1, false).key, build_status(0x0CE6, false, b"a", 9, true).key);
        assert_ne!(build_status(0x0CE6, false, b"a", 1, false).key, build_status(0x0CE6, false, b"b", 1, false).key);
        // FNV-1a reference value for "a"
        assert_eq!(path_hash(b"a"), 0xE40C292C);
    }

    #[test]
    fn bluetooth_detection_prefers_report_length_then_bus() {
        assert!(detect_bluetooth(78, false)); // DualSense / DS4 Bluetooth report length
        assert!(!detect_bluetooth(64, true)); // a readable length wins over the bus
        assert!(detect_bluetooth(0, true)); // unreadable length: trust the bus
        assert!(!detect_bluetooth(0, false)); // unknown: assume USB, as before
    }
}

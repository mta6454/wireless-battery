//! Xbox controllers.
//!  1. Windows: Windows.Gaming.Input (WinRT) knows every Xbox controller (Xbox Wireless adapter,
//!     USB, Bluetooth) and gives a real battery report (remaining / full mWh -> %).
//!  2. Windows: XInput fallback with coarse levels (empty / low / medium / full) shown as text; it
//!     also covers headsets plugged into a controller.
//!  3. macOS: a Bluetooth/USB Xbox controller (vendor 045E) that macOS exposes as an IOHIDDevice
//!     with a `BatteryPercent` property, read through `ioreg`. Controllers the Bluetooth driver
//!     already listed (same name) are skipped. Nothing is reported when the OS exposes no such
//!     property; there is no public non-Objective-C API for the XInput-style levels there.
//! All decision logic is pure and compiled on every platform; only the OS calls are cfg-gated.

// The Windows and macOS halves are each unused on the other platform.
#![allow(dead_code)]

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::hex4;

const MS_VID: u16 = 0x045E;

/// One controller as Windows.Gaming.Input sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WgiPad {
    pub vid: u16,
    pub pid: u16,
    pub display_name: String,
    pub remaining_mwh: i32,
    pub full_mwh: i32,
    pub status: BatStatus,
}

/// Windows.System.Power.BatteryStatus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BatStatus {
    #[default]
    NotPresent,
    Discharging,
    Idle,
    Charging,
}

/// XInput battery query answer (`XINPUT_BATTERY_INFORMATION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XBattery {
    pub battery_type: u8,
    pub level: u8,
}

pub const DEVTYPE_GAMEPAD: u8 = 0;
pub const DEVTYPE_HEADSET: u8 = 1;
const TYPE_DISCONNECTED: u8 = 0;
const TYPE_WIRED: u8 = 1;
const TYPE_UNKNOWN: u8 = 0xFF;

/// OS access, mockable.
pub trait XboxSource {
    /// Controllers from Windows.Gaming.Input that expose a battery report (None report: skipped
    /// by the caller, so only controllers with a report are listed, index = list position).
    fn wgi_pads(&mut self) -> Vec<(usize, WgiPad)>;
    /// True when an XInput controller is connected in this slot (0..4).
    fn xinput_connected(&mut self, user: u32) -> bool;
    fn xinput_battery(&mut self, user: u32, devtype: u8) -> Option<XBattery>;
}

fn clamp100(v: i64) -> u8 {
    v.clamp(0, 100) as u8
}

/// Real-percent status for a WGI pad, None when the report is unusable.
pub fn wgi_status(index: usize, p: &WgiPad) -> Option<DeviceStatus> {
    if p.vid != MS_VID || p.full_mwh <= 0 || p.status == BatStatus::NotPresent {
        return None;
    }
    let name = if p.display_name.is_empty() { "Xbox controller".to_string() } else { p.display_name.clone() };
    Some(DeviceStatus {
        key: format!("xbox:{}:{}", hex4(p.pid), index),
        name,
        kind: DeviceKind::Gamepad,
        via: "Xbox Wireless / Bluetooth".into(),
        percent: Some(clamp100(p.remaining_mwh as i64 * 100 / p.full_mwh as i64)),
        charging: p.status == BatStatus::Charging,
        ..Default::default()
    })
}

/// Coarse XInput level -> (percent, text).
pub fn level_info(level: u8) -> (u8, &'static str) {
    match level {
        0 => (5, "Empty"),
        1 => (25, "Low"),
        2 => (60, "Medium"),
        _ => (100, "Full"),
    }
}

/// Status for one XInput battery answer; None for disconnected/wired and for the phantom
/// "unknown" headset a controller without a headset can still answer with.
pub fn xinput_status(user: u32, devtype: u8, b: XBattery) -> Option<DeviceStatus> {
    if b.battery_type == TYPE_DISCONNECTED || b.battery_type == TYPE_WIRED {
        return None;
    }
    if devtype == DEVTYPE_HEADSET && b.battery_type == TYPE_UNKNOWN {
        return None;
    }
    let pad = devtype == DEVTYPE_GAMEPAD;
    let (percent, text) = level_info(b.level);
    Some(DeviceStatus {
        key: format!("xinput:{user}:{devtype}"),
        name: if pad { format!("Xbox controller {}", user + 1) } else { "Xbox headset".into() },
        kind: if pad { DeviceKind::Gamepad } else { DeviceKind::Headset },
        via: "Xbox Wireless".into(),
        percent: Some(percent),
        level_text: text.into(),
        ..Default::default()
    })
}

/// An Xbox controller on Bluetooth may already be listed (with a real percentage).
fn has_bt_xbox(existing: &[DeviceStatus]) -> bool {
    existing.iter().any(|s| s.via == "Bluetooth" && s.name.contains("Xbox"))
}

/// Full Windows logic: returns new statuses and the (vid, pid) pairs to claim.
pub fn collect(src: &mut impl XboxSource, existing: &[DeviceStatus]) -> (Vec<DeviceStatus>, Vec<(u16, u16)>) {
    let mut out = Vec::new();
    let mut claims = Vec::new();
    let bt_xbox = has_bt_xbox(existing);
    let mut precise = 0;
    if !bt_xbox {
        for (i, p) in src.wgi_pads() {
            if let Some(s) = wgi_status(i, &p) {
                out.push(s);
                claims.push((p.vid, p.pid));
                precise += 1;
            }
        }
    }
    for user in 0..4u32 {
        // Empty slots can still answer battery queries (one controller showed up as four
        // "Medium" headsets), so only look at slots with a connected controller.
        if !src.xinput_connected(user) {
            continue;
        }
        for devtype in [DEVTYPE_GAMEPAD, DEVTYPE_HEADSET] {
            if devtype == DEVTYPE_GAMEPAD && (bt_xbox || precise > 0) {
                continue; // already shown precisely
            }
            let Some(b) = src.xinput_battery(user, devtype) else { continue };
            if let Some(s) = xinput_status(user, devtype, b) {
                out.push(s);
            }
        }
    }
    (out, claims)
}

// ---------------------------------------------------------------------------------------------
// macOS: ioreg parsing (pure).

/// An Xbox HID device with a battery property found in `ioreg -r -c IOHIDDevice -l -w0`.
#[derive(Clone, Debug, PartialEq)]
pub struct MacPad {
    pub name: String,
    pub pid: u16,
    pub percent: u8,
    pub bluetooth: bool,
}

fn prop_int(line: &str, key: &str) -> Option<i64> {
    let rest = line.trim_start_matches([' ', '|']).trim_end().strip_prefix(&format!("\"{key}\" = "))?;
    rest.trim().parse().ok()
}

fn prop_str(line: &str, key: &str) -> Option<String> {
    let rest = line.trim_start_matches([' ', '|']).trim_end().strip_prefix(&format!("\"{key}\" = \""))?;
    Some(rest.strip_suffix('"')?.to_string())
}

/// Parses ioreg output: one block per `+-o` node, properties as `"Key" = value` lines.
pub fn parse_ioreg(text: &str) -> Vec<MacPad> {
    #[derive(Default)]
    struct Cur {
        vid: Option<i64>,
        pid: Option<i64>,
        name: Option<String>,
        pct: Option<i64>,
        transport: String,
    }
    fn finish(c: &Cur, out: &mut Vec<MacPad>) {
        if c.vid != Some(MS_VID as i64) {
            return;
        }
        let (Some(pct), Some(pid)) = (c.pct, c.pid) else { return };
        if !(0..=100).contains(&pct) {
            return;
        }
        let name = c.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Xbox controller".into());
        out.push(MacPad { name, pid: pid as u16, percent: pct as u8, bluetooth: c.transport.contains("Bluetooth") });
    }
    let mut out = Vec::new();
    let mut cur = Cur::default();
    for line in text.lines() {
        if line.contains("+-o ") {
            finish(&cur, &mut out);
            cur = Cur::default();
            continue;
        }
        if let Some(v) = prop_int(line, "VendorID") {
            cur.vid = Some(v);
        } else if let Some(v) = prop_int(line, "ProductID") {
            cur.pid = Some(v);
        } else if let Some(v) = prop_int(line, "BatteryPercent") {
            cur.pct = Some(v);
        } else if let Some(v) = prop_str(line, "Product") {
            cur.name = Some(v);
        } else if let Some(v) = prop_str(line, "Transport") {
            cur.transport = v;
        }
    }
    finish(&cur, &mut out);
    // Several HID collections of one controller repeat the same properties.
    let mut seen = Vec::new();
    out.retain(|p: &MacPad| {
        let k = (p.pid, p.name.clone(), p.percent);
        if seen.contains(&k) {
            false
        } else {
            seen.push(k);
            true
        }
    });
    out
}

/// Statuses for parsed pads, skipping names already reported by earlier drivers.
pub fn mac_statuses(pads: &[MacPad], existing: &[DeviceStatus]) -> Vec<(DeviceStatus, u16)> {
    let mut out: Vec<(DeviceStatus, u16)> = Vec::new();
    for (i, p) in pads.iter().enumerate() {
        if existing.iter().any(|s| s.name == p.name) {
            continue;
        }
        out.push((
            DeviceStatus {
                key: format!("xbox:{}:{}", hex4(p.pid), i),
                name: p.name.clone(),
                kind: DeviceKind::Gamepad,
                via: if p.bluetooth { "Bluetooth" } else { "USB cable" }.into(),
                percent: Some(p.percent),
                ..Default::default()
            },
            p.pid,
        ));
    }
    out
}

#[cfg(target_os = "macos")]
fn run_ioreg() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new("/usr/sbin/ioreg")
        .args(["-r", "-c", "IOHIDDevice", "-l", "-w0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    Some(String::from_utf8_lossy(&reader.join().ok()?).into_owned())
}

// ---------------------------------------------------------------------------------------------
// Windows OS access.

#[cfg(windows)]
mod win {
    use super::*;
    use windows::Gaming::Input::RawGameController;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    use windows::Win32::UI::Input::XboxController::{
        XInputGetBatteryInformation, XInputGetState, BATTERY_DEVTYPE, XINPUT_BATTERY_INFORMATION, XINPUT_STATE,
    };

    pub struct WinSource {
        first: bool,
    }

    impl WinSource {
        pub fn new() -> Self {
            Self { first: true }
        }
    }

    fn pad_at(c: &RawGameController) -> Option<WgiPad> {
        let vid = c.HardwareVendorId().ok()?;
        let pid = c.HardwareProductId().ok()?;
        if vid != MS_VID {
            return None;
        }
        let report = c.TryGetBatteryReport().ok()?;
        let rem = report.RemainingCapacityInMilliwattHours().ok()?.Value().ok()?;
        let full = report.FullChargeCapacityInMilliwattHours().ok()?.Value().ok()?;
        let status = match report.Status().ok()? {
            windows::System::Power::BatteryStatus::Discharging => BatStatus::Discharging,
            windows::System::Power::BatteryStatus::Idle => BatStatus::Idle,
            windows::System::Power::BatteryStatus::Charging => BatStatus::Charging,
            _ => BatStatus::NotPresent,
        };
        let display_name = c.DisplayName().map(|h| h.to_string()).unwrap_or_default();
        Some(WgiPad { vid, pid, display_name, remaining_mwh: rem, full_mwh: full, status })
    }

    impl XboxSource for WinSource {
        fn wgi_pads(&mut self) -> Vec<(usize, WgiPad)> {
            if self.first {
                self.first = false;
                // Our poll thread; harmless if already initialised.
                unsafe {
                    let _ = RoInitialize(RO_INIT_MULTITHREADED);
                }
                // Touch the list, then give the asynchronously filled list time to populate.
                let _ = RawGameController::RawGameControllers();
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            let Ok(list) = RawGameController::RawGameControllers() else { return Vec::new() };
            let mut out = Vec::new();
            for (i, c) in list.into_iter().enumerate() {
                if let Some(p) = pad_at(&c) {
                    out.push((i, p));
                }
            }
            out
        }

        fn xinput_connected(&mut self, user: u32) -> bool {
            let mut st = XINPUT_STATE::default();
            unsafe { XInputGetState(user, &mut st) == 0 }
        }

        fn xinput_battery(&mut self, user: u32, devtype: u8) -> Option<XBattery> {
            let mut bi = XINPUT_BATTERY_INFORMATION::default();
            let r = unsafe { XInputGetBatteryInformation(user, BATTERY_DEVTYPE(devtype), &mut bi) };
            (r == 0).then_some(XBattery { battery_type: bi.BatteryType.0, level: bi.BatteryLevel.0 })
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    #[cfg(windows)]
    {
        let mut src = win::WinSource::new();
        let (found, claims) = collect(&mut src, &ctx.out);
        ctx.out.extend(found);
        for (v, p) in claims {
            ctx.claim(v, p);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let Some(text) = run_ioreg() else { return };
        let pads = parse_ioreg(&text);
        for (s, pid) in mac_statuses(&pads, &ctx.out) {
            ctx.add(s, MS_VID, pid);
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = ctx;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        pads: Vec<(usize, WgiPad)>,
        connected: [bool; 4],
        bat: Vec<(u32, u8, XBattery)>,
    }

    impl XboxSource for Mock {
        fn wgi_pads(&mut self) -> Vec<(usize, WgiPad)> {
            self.pads.clone()
        }
        fn xinput_connected(&mut self, u: u32) -> bool {
            self.connected[u as usize]
        }
        fn xinput_battery(&mut self, u: u32, t: u8) -> Option<XBattery> {
            self.bat.iter().find(|(a, b, _)| *a == u && *b == t).map(|x| x.2)
        }
    }

    fn pad(rem: i32, full: i32, st: BatStatus) -> WgiPad {
        WgiPad { vid: MS_VID, pid: 0x0B13, display_name: "Xbox Wireless Controller".into(), remaining_mwh: rem, full_mwh: full, status: st }
    }

    fn xb(t: u8, l: u8) -> XBattery {
        XBattery { battery_type: t, level: l }
    }

    #[test]
    fn wgi_percent_and_charging() {
        let s = wgi_status(2, &pad(1500, 2000, BatStatus::Charging)).unwrap();
        assert_eq!((s.percent, s.charging, s.key.as_str()), (Some(75), true, "xbox:0B13:2"));
        assert_eq!(s.via, "Xbox Wireless / Bluetooth");
        assert_eq!(wgi_status(0, &pad(3000, 2000, BatStatus::Idle)).unwrap().percent, Some(100));
        assert!(wgi_status(0, &pad(10, 0, BatStatus::Idle)).is_none());
        assert!(wgi_status(0, &pad(10, 20, BatStatus::NotPresent)).is_none());
        let mut other = pad(10, 20, BatStatus::Idle);
        other.vid = 0x1234;
        assert!(wgi_status(0, &other).is_none());
        let mut noname = pad(10, 20, BatStatus::Idle);
        noname.display_name.clear();
        assert_eq!(wgi_status(0, &noname).unwrap().name, "Xbox controller");
    }

    #[test]
    fn level_mapping() {
        assert_eq!(level_info(0), (5, "Empty"));
        assert_eq!(level_info(1), (25, "Low"));
        assert_eq!(level_info(2), (60, "Medium"));
        assert_eq!(level_info(3), (100, "Full"));
    }

    #[test]
    fn xinput_filters() {
        assert!(xinput_status(0, DEVTYPE_GAMEPAD, xb(TYPE_DISCONNECTED, 3)).is_none());
        assert!(xinput_status(0, DEVTYPE_GAMEPAD, xb(TYPE_WIRED, 3)).is_none());
        assert!(xinput_status(0, DEVTYPE_HEADSET, xb(TYPE_UNKNOWN, 2)).is_none());
        let s = xinput_status(1, DEVTYPE_GAMEPAD, xb(0xFF, 1)).unwrap();
        assert_eq!((s.name.as_str(), s.key.as_str(), s.level_text.as_str()), ("Xbox controller 2", "xinput:1:0", "Low"));
        let h = xinput_status(0, DEVTYPE_HEADSET, xb(3, 2)).unwrap();
        assert_eq!((h.name.as_str(), h.kind, h.percent), ("Xbox headset", DeviceKind::Headset, Some(60)));
    }

    #[test]
    fn collect_prefers_precise_and_claims() {
        let mut m = Mock { pads: vec![(0, pad(1000, 2000, BatStatus::Discharging))], ..Default::default() };
        m.connected[0] = true;
        m.bat = vec![(0, 0, xb(3, 2)), (0, 1, xb(3, 3))];
        let (out, claims) = collect(&mut m, &[]);
        // precise pad + headset only (gamepad XInput entry suppressed)
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].percent, Some(50));
        assert_eq!(out[1].kind, DeviceKind::Headset);
        assert_eq!(claims, vec![(MS_VID, 0x0B13)]);
    }

    #[test]
    fn collect_falls_back_to_xinput() {
        let mut m = Mock::default();
        m.connected[1] = true;
        m.bat = vec![(1, 0, xb(2, 0)), (1, 1, xb(0xFF, 2)), (3, 0, xb(2, 3))]; // slot 3 not connected
        let (out, claims) = collect(&mut m, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].name.as_str(), out[0].percent), ("Xbox controller 2", Some(5)));
        assert!(claims.is_empty());
    }

    #[test]
    fn collect_skips_pad_when_bluetooth_listed() {
        let bt = DeviceStatus { name: "Xbox Wireless Controller".into(), via: "Bluetooth".into(), ..Default::default() };
        let mut m = Mock { pads: vec![(0, pad(1000, 2000, BatStatus::Idle))], ..Default::default() };
        m.connected[0] = true;
        m.bat = vec![(0, 0, xb(2, 1)), (0, 1, xb(3, 1))];
        let (out, _) = collect(&mut m, &[bt]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DeviceKind::Headset);
    }

    const IOREG: &str = r#"+-o AppleUserHIDDevice  <class IOHIDDevice, id 0x1000, registered, matched, active, busy 0 (0 ms), retain 8>
  | {
  |   "Transport" = "Bluetooth"
  |   "VendorID" = 1118
  |   "ProductID" = 2835
  |   "Product" = "Xbox Wireless Controller"
  |   "BatteryPercent" = 80
  | }
  |
+-o AppleUserHIDDevice  <class IOHIDDevice, id 0x1001, registered, matched, active, busy 0 (0 ms), retain 8>
  | {
  |   "Transport" = "Bluetooth"
  |   "VendorID" = 1118
  |   "ProductID" = 2835
  |   "Product" = "Xbox Wireless Controller"
  |   "BatteryPercent" = 80
  | }
+-o AppleUserHIDDevice  <class IOHIDDevice, id 0x1002, registered, matched, active, busy 0 (0 ms), retain 8>
  | {
  |   "Transport" = "USB"
  |   "VendorID" = 1118
  |   "ProductID" = 2834
  |   "Product" = "Controller"
  | }
+-o AppleUserHIDDevice  <class IOHIDDevice, id 0x1003, registered, matched, active, busy 0 (0 ms), retain 8>
  | {
  |   "Transport" = "Bluetooth"
  |   "VendorID" = 1356
  |   "ProductID" = 3302
  |   "Product" = "Other Pad"
  |   "BatteryPercent" = 55
  | }
"#;

    #[test]
    fn ioreg_parse() {
        let pads = parse_ioreg(IOREG);
        assert_eq!(pads, vec![MacPad { name: "Xbox Wireless Controller".into(), pid: 2835, percent: 80, bluetooth: true }]);
        assert!(parse_ioreg("").is_empty());
        assert!(parse_ioreg("garbage\n\"BatteryPercent\" = 5").is_empty());
    }

    #[test]
    fn mac_dedup_by_name() {
        let pads = parse_ioreg(IOREG);
        let r = mac_statuses(&pads, &[]);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].0.percent, r[0].0.via.as_str(), r[0].1), (Some(80), "Bluetooth", 2835));
        let bt = DeviceStatus { name: "Xbox Wireless Controller".into(), ..Default::default() };
        assert!(mac_statuses(&pads, &[bt]).is_empty());
    }
}

//! ASUS ROG wireless keyboards and mice (VID 0B05), including the ROG Omni Receiver (0B05:1ACE).
//! Query: [report id][12][sub]   sub 01 = keyboard battery, sub 07 = mouse battery.
//! Reply: [report id][12][sub]...  keyboard: [6] % (Falchion: [11]), [9] == 1 charging
//!                                 mouse:    [5] level (x25 on older 0-4 gauges), [10] charging
//! [1..2] == FF AA means the device behind the receiver did not answer (asleep / off).
//! Offsets count the report id byte. Only query commands are sent.
//! Protocol and model table from github.com/SteveWang92/peripheral-battery-tray (rog-battery,
//! MIT, itself based on G-Helper) and github.com/1gcat/BatteryTool.
//!
//! Cross-platform notes: the Windows `colNN` path tag only exists on Windows; where a path has no
//! such tag (macOS/Linux) the tag requirement is treated as "unknown, accept". Likewise an unknown
//! USB interface number (-1) is accepted.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidColl, HidDev, HidIo};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Keyboard,
    Mouse,
}

struct Model {
    pid: u16,
    name: &'static str,
    kind: Kind,
    /// 0 = unnumbered
    report_id: u8,
    /// USB interface (-1 = any vendor-page collection)
    iface: i32,
    /// required collection tag in the path ("col02"), if any
    col: Option<&'static str>,
    /// battery byte
    offset: usize,
    /// battery multiplier
    scale: i32,
}

use Kind::{Keyboard as KB, Mouse as MS};

const MODELS: &[Model] = &[
    // Keyboards (Omni = col02 report 02, receiver/wired = interface 1 unnumbered)
    Model { pid: 0x1A85, name: "ROG Azoth", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1A85, name: "ROG Azoth", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1A83, name: "ROG Azoth", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1B42, name: "ROG Azoth Extreme", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1B3F, name: "ROG Azoth Extreme", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1CF1, name: "ROG Azoth Extreme SE", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1CEF, name: "ROG Azoth Extreme SE", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1C25, name: "ROG Azoth X", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1C24, name: "ROG Azoth X", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x193E, name: "ROG Falchion", kind: KB, report_id: 0, iface: 1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x193C, name: "ROG Falchion", kind: KB, report_id: 0, iface: 1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x1B04, name: "ROG Falchion RX", kind: KB, report_id: 0, iface: -1, col: None, offset: 11, scale: 1 },
    Model { pid: 0x1B06, name: "ROG Falchion RX Low Profile", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 11, scale: 1 },
    Model { pid: 0x1AB0, name: "ROG Strix Scope II 96", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1AAE, name: "ROG Strix Scope II 96", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1B7A, name: "ROG Strix Scope II 96 RX", kind: KB, report_id: 2, iface: 2, col: Some("col02"), offset: 6, scale: 1 },
    Model { pid: 0x1B78, name: "ROG Strix Scope II 96 RX", kind: KB, report_id: 0, iface: 1, col: None, offset: 6, scale: 1 },
    Model { pid: 0x1A07, name: "ROG Strix Scope RX TKL", kind: KB, report_id: 0, iface: -1, col: None, offset: 6, scale: 1 },
    // Mice (Omni = col03 report 03, receiver/wired = interface 0 unnumbered)
    Model { pid: 0x18E5, name: "ROG Chakram", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x18E3, name: "ROG Chakram", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A1A, name: "ROG Chakram X", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A18, name: "ROG Chakram X", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x18A0, name: "ROG Gladius II Wireless", kind: MS, report_id: 0, iface: 2, col: None, offset: 5, scale: 25 },
    Model { pid: 0x197F, name: "ROG Gladius III Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x197D, name: "ROG Gladius III Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A72, name: "ROG Gladius III Aimpoint", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A72, name: "ROG Gladius III Aimpoint", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A70, name: "ROG Gladius III Aimpoint", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B0C, name: "ROG Gladius III Aimpoint EVA-02", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B0A, name: "ROG Gladius III Aimpoint EVA-02", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A94, name: "ROG Harpe Ace Aim Lab Edition", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A94, name: "ROG Harpe Ace Aim Lab Edition", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A92, name: "ROG Harpe Ace Aim Lab Edition", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B68, name: "ROG Harpe Ace Extreme", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B69, name: "ROG Harpe Ace Extreme", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B67, name: "ROG Harpe Ace Extreme", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B65, name: "ROG Harpe Ace Mini", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B63, name: "ROG Harpe Ace Mini", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1AD0, name: "ROG Harpe II Ace", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1C69, name: "ROG Harpe II Ace", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1960, name: "ROG Keris Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x195E, name: "ROG Keris Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A59, name: "ROG Keris Wireless EVA Edition", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A57, name: "ROG Keris Wireless EVA Edition", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1A68, name: "ROG Keris Wireless Aimpoint", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A6A, name: "ROG Keris Wireless Aimpoint", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1A68, name: "ROG Keris Wireless Aimpoint", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1A66, name: "ROG Keris Wireless Aimpoint", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1B1A, name: "ROG Keris II Ace", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B18, name: "ROG Keris II Ace", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1B16, name: "ROG Keris II Ace", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1C0E, name: "ROG Keris II Origin", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1C0C, name: "ROG Keris II Origin", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1D4E, name: "ROG Keris II Origin KJP", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
    Model { pid: 0x1D4C, name: "ROG Keris II Origin KJP", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1908, name: "ROG Pugio II", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1906, name: "ROG Pugio II", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1979, name: "ROG Spatha X", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x1977, name: "ROG Spatha X", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 1 },
    Model { pid: 0x18B4, name: "ROG Strix Carry", kind: MS, report_id: 0, iface: 1, col: None, offset: 7, scale: 25 },
    Model { pid: 0x1949, name: "ROG Strix Impact II Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1947, name: "ROG Strix Impact II Wireless", kind: MS, report_id: 0, iface: 0, col: None, offset: 5, scale: 25 },
    Model { pid: 0x1AD7, name: "ROG Strix Impact III Wireless", kind: MS, report_id: 3, iface: 2, col: Some("col03"), offset: 5, scale: 1 },
];

/// True when the path carries the Windows collection tag (`colNN`) at all.
fn has_col_tags(path: &str) -> bool {
    let p = path.to_lowercase();
    (0..10).any(|i| p.contains(&format!("col0{i}")))
}

/// Windows-only `colNN` path filter; paths without any tag cannot be told apart, so accept them.
fn path_has(path: &str, tag: Option<&str>) -> bool {
    match tag {
        None => true,
        Some(t) => !has_col_tags(path) || path.to_lowercase().contains(t),
    }
}

fn coll_path(c: &HidColl) -> String {
    c.path.to_string_lossy().into_owned()
}

/// USB interface matches, or is unknown (-1, e.g. macOS).
fn iface_ok(c: &HidColl, want: i32) -> bool {
    want < 0 || c.iface < 0 || c.iface == want
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Ok(Vec<u8>),
    Asleep,
    None,
}

fn query(io: &mut impl HidIo, rid: u8, cmd: u8, sub: u8) -> Reply {
    io.flush();
    if !io.write(&[rid, cmd, sub]) {
        return Reply::None;
    }
    let end = Instant::now() + Duration::from_millis(500);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Reply::None;
        }
        let Some(mut r) = io.read(left.as_millis() as i32) else { return Reply::None };
        if r.len() < 11 {
            return Reply::None;
        }
        if rid == 0 {
            r.insert(0, 0x00); // restore the report id slot so offsets match
        }
        if r.len() < 12 {
            continue;
        }
        if r[1] == 0xFF && r[2] == 0xAA {
            return Reply::Asleep;
        }
        if r[1] == cmd && r[2] == sub {
            return Reply::Ok(r);
        }
    }
}

/// Decodes a battery reply for one model (`r` includes the report id slot).
fn decode(m: &Model, r: &[u8], asleep_reply: bool) -> DeviceStatus {
    let mut s = DeviceStatus {
        key: format!("rog:{}{}", hex4(m.pid), if m.kind == KB { ":kb" } else { ":ms" }),
        name: m.name.to_string(),
        kind: if m.kind == KB { DeviceKind::Keyboard } else { DeviceKind::Mouse },
        via: if m.report_id != 0 { "ROG Omni Receiver" } else { "2.4 GHz receiver / USB" }.to_string(),
        ..Default::default()
    };
    if asleep_reply {
        s.asleep = true;
    } else if m.kind == KB {
        if r[m.offset] <= 100 {
            s.percent = Some(r[m.offset]);
        }
        s.charging = r[9] == 1;
    } else {
        let v = r[m.offset] as i32 * m.scale;
        if v > 0 && v <= 100 {
            s.percent = Some(v as u8);
        } else {
            s.asleep = true; // 0 means unavailable, not empty
        }
        s.charging = r[10] != 0;
    }
    s
}

/// Reads one model's battery through an open collection. None if it did not answer.
fn read_model(io: &mut impl HidIo, m: &Model) -> Option<DeviceStatus> {
    match query(io, m.report_id, 0x12, if m.kind == KB { 0x01 } else { 0x07 }) {
        Reply::None => None,
        Reply::Asleep => Some(decode(m, &[], true)),
        Reply::Ok(r) => Some(decode(m, &r, false)),
    }
}

/// One entry per device name (wired + receiver both present while charging).
fn add_once(ctx: &mut PollContext, s: DeviceStatus) {
    if ctx.out.iter().any(|o| o.name == s.name) {
        return;
    }
    ctx.out.push(s);
}

/// Product ids listed by the Omni receiver's pairing reply (4-byte slots from byte 5).
fn omni_pids(r: &[u8]) -> Vec<u16> {
    let mut v = Vec::new();
    let mut at = 5;
    while at + 1 < r.len() {
        let pid = r[at] as u16 | ((r[at + 1] as u16) << 8);
        if pid == 0 {
            break;
        }
        v.push(pid);
        at += 4;
    }
    v
}

// ROG Omni Receiver (0B05:1ACE): ask it which devices are paired ([01 A0 00] on col01 lists their
// product ids in 4-byte slots from byte 5), then read each one on its own channel of the receiver.
fn poll_omni(ctx: &mut PollContext) {
    if ctx.is_claimed(0x0B05, 0x1ACE) {
        return;
    }
    let list = ctx.find(0x0B05, 0x1ACE, |c| {
        iface_ok(c, 2) && (c.iface >= 0 || c.is_vendor_page()) && c.out_len >= 12 && path_has(&coll_path(c), Some("col01"))
    });
    let Some(first) = list.first() else { return };
    let Some(mut d) = HidDev::open(ctx.api, first) else { return };
    let Reply::Ok(r) = query(&mut d, 0x01, 0xA0, 0x00) else { return };
    ctx.claim(0x0B05, 0x1ACE);
    for pid in omni_pids(&r) {
        // only the Omni-channel entry of that model
        let Some(m) = MODELS.iter().find(|m| m.pid == pid && m.report_id != 0) else { continue };
        let colls = ctx.find(0x0B05, 0x1ACE, |c| {
            iface_ok(c, 2) && (c.iface >= 0 || c.is_vendor_page()) && c.out_len >= 12 && path_has(&coll_path(c), m.col)
        });
        for c in colls {
            let Some(mut dev) = HidDev::open(ctx.api, &c) else { continue };
            if let Some(s) = read_model(&mut dev, m) {
                add_once(ctx, s);
                break;
            }
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    poll_omni(ctx);
    for m in MODELS {
        if ctx.is_claimed(0x0B05, m.pid) {
            continue;
        }
        let colls = ctx.find(0x0B05, m.pid, |c| {
            if c.out_len < 12 || !c.is_vendor_page() {
                return false;
            }
            iface_ok(c, m.iface) && path_has(&coll_path(c), m.col)
        });
        for c in colls {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some(s) = read_model(&mut d, m) else { continue };
            add_once(ctx, s);
            ctx.claim(0x0B05, m.pid);
            break;
        }
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

    fn model(pid: u16, rid: u8) -> &'static Model {
        MODELS.iter().find(|m| m.pid == pid && m.report_id == rid).unwrap()
    }

    /// Numbered reply (report id kept): 20 bytes.
    fn numbered(rid: u8, cmd: u8, sub: u8) -> Vec<u8> {
        let mut r = vec![0u8; 20];
        r[0] = rid;
        r[1] = cmd;
        r[2] = sub;
        r
    }

    /// Unnumbered reply as hidapi delivers it (report id stripped): 19 bytes, `[0]` = cmd.
    fn unnumbered(cmd: u8, sub: u8) -> Vec<u8> {
        let mut r = vec![0u8; 19];
        r[0] = cmd;
        r[1] = sub;
        r
    }

    #[test]
    fn query_layout_flush_and_unnumbered_offset_restore() {
        let mut m = Mock::new(vec![unnumbered(0x12, 0x01)]);
        let Reply::Ok(r) = query(&mut m, 0, 0x12, 0x01) else { panic!() };
        assert_eq!(m.sent, vec![vec![0, 0x12, 0x01]]);
        assert_eq!(m.flushed, 1);
        assert_eq!((r[0], r[1], r[2]), (0, 0x12, 0x01)); // id slot re-inserted
        assert_eq!(r.len(), 20);
    }

    #[test]
    fn keyboard_battery_and_charging() {
        let mut raw = unnumbered(0x12, 0x01);
        raw[5] = 77; // [6] after the id slot
        raw[8] = 1; // [9]
        let mut m = Mock::new(vec![raw]);
        let s = read_model(&mut m, model(0x1A83, 0)).unwrap();
        assert_eq!(s.key, "rog:1A83:kb");
        assert_eq!(s.name, "ROG Azoth");
        assert_eq!(s.kind, DeviceKind::Keyboard);
        assert_eq!(s.via, "2.4 GHz receiver / USB");
        assert_eq!(s.percent, Some(77));
        assert!(s.charging && !s.asleep);
    }

    #[test]
    fn falchion_uses_offset_11_and_ignores_over_100() {
        let mut raw = unnumbered(0x12, 0x01);
        raw[10] = 55; // [11]
        let s = read_model(&mut Mock::new(vec![raw.clone()]), model(0x193E, 0)).unwrap();
        assert_eq!(s.percent, Some(55));
        raw[10] = 200;
        let s = read_model(&mut Mock::new(vec![raw]), model(0x193E, 0)).unwrap();
        assert_eq!(s.percent, None);
        assert!(!s.asleep);
    }

    #[test]
    fn omni_keyboard_uses_numbered_report_and_via() {
        let mut r = numbered(2, 0x12, 0x01);
        r[6] = 40;
        let mut m = Mock::new(vec![r]);
        let s = read_model(&mut m, model(0x1A85, 2)).unwrap();
        assert_eq!(m.sent[0], vec![2, 0x12, 0x01]);
        assert_eq!(s.via, "ROG Omni Receiver");
        assert_eq!(s.percent, Some(40));
    }

    #[test]
    fn mouse_level_scale_and_charging() {
        // Chakram: older 0-4 gauge, x25
        let mut raw = unnumbered(0x12, 0x07);
        raw[4] = 3; // [5]
        raw[9] = 1; // [10]
        let mut m = Mock::new(vec![raw]);
        let s = read_model(&mut m, model(0x18E5, 0)).unwrap();
        assert_eq!(m.sent[0], vec![0, 0x12, 0x07]);
        assert_eq!(s.key, "rog:18E5:ms");
        assert_eq!(s.kind, DeviceKind::Mouse);
        assert_eq!(s.percent, Some(75));
        assert!(s.charging);
        // Gladius III Wireless: direct percent
        let mut raw = unnumbered(0x12, 0x07);
        raw[4] = 93;
        let s = read_model(&mut Mock::new(vec![raw]), model(0x197F, 0)).unwrap();
        assert_eq!(s.percent, Some(93));
        assert!(!s.charging);
    }

    #[test]
    fn mouse_zero_or_out_of_range_means_unavailable() {
        let raw = unnumbered(0x12, 0x07);
        let s = read_model(&mut Mock::new(vec![raw.clone()]), model(0x197F, 0)).unwrap();
        assert!(s.asleep && s.percent.is_none());
        let mut raw = raw;
        raw[4] = 101;
        let s = read_model(&mut Mock::new(vec![raw]), model(0x197F, 0)).unwrap();
        assert!(s.asleep && s.percent.is_none());
    }

    #[test]
    fn ff_aa_reply_means_asleep() {
        let mut raw = unnumbered(0xFF, 0xAA);
        raw[4] = 50;
        let s = read_model(&mut Mock::new(vec![raw]), model(0x1A83, 0)).unwrap();
        assert!(s.asleep);
        assert_eq!(s.percent, None);
        assert_eq!(s.name, "ROG Azoth");
    }

    #[test]
    fn unrelated_replies_are_skipped_until_the_matching_one() {
        let mut good = unnumbered(0x12, 0x07);
        good[4] = 60;
        let mut m = Mock::new(vec![unnumbered(0x55, 0x01), unnumbered(0x12, 0x01), good]);
        let s = read_model(&mut m, model(0x197F, 0)).unwrap();
        assert_eq!(s.percent, Some(60));
    }

    #[test]
    fn no_answer_short_reply_or_failed_write_give_none() {
        assert!(read_model(&mut Mock::new(vec![]), model(0x197F, 0)).is_none());
        assert!(read_model(&mut Mock::new(vec![vec![0x12; 5]]), model(0x197F, 0)).is_none());
        let mut m = Mock::new(vec![unnumbered(0x12, 0x07)]);
        m.accept_write = false;
        assert!(read_model(&mut m, model(0x197F, 0)).is_none());
    }

    #[test]
    fn omni_pairing_list_parses_four_byte_slots() {
        let mut r = vec![0u8; 20];
        r[5] = 0x68;
        r[6] = 0x1A; // 1A68
        r[9] = 0x85;
        r[10] = 0x1A; // 1A85
        assert_eq!(omni_pids(&r), vec![0x1A68, 0x1A85]);
        assert!(omni_pids(&[0u8; 20]).is_empty());
    }

    #[test]
    fn omni_pairing_query_layout() {
        let mut m = Mock::new(vec![numbered(1, 0xA0, 0x00)]);
        assert!(matches!(query(&mut m, 0x01, 0xA0, 0x00), Reply::Ok(_)));
        assert_eq!(m.sent[0], vec![1, 0xA0, 0x00]);
    }

    #[test]
    fn path_tag_and_interface_filters() {
        assert!(path_has(r"\\?\hid#vid_0b05&pid_1a85&mi_02&col02#7&abc", Some("col02")));
        assert!(!path_has(r"\\?\hid#vid_0b05&pid_1a85&mi_02&col01#7&abc", Some("col02")));
        assert!(path_has("DevSrvsID:4295000000", Some("col02"))); // macOS: no tag, accept
        assert!(path_has("anything", None));
    }

    #[test]
    fn model_table_sanity() {
        assert_eq!(MODELS.len(), 63);
        for m in MODELS {
            let top = if m.kind == KB { 11 } else { 7 };
            assert!(m.offset <= top);
            assert_eq!(m.report_id != 0, m.col.is_some(), "{:04X}", m.pid);
        }
        assert_eq!(model(0x18B4, 0).offset, 7);
        assert_eq!(model(0x1AD7, 3).col, Some("col03"));
    }
}

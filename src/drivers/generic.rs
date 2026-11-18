//! Fallback that works for any vendor: HID devices exposing a standard battery usage (Generic
//! Device Controls "Battery Strength" 06:20, Battery System "Absolute State Of Charge" 85:65) in a
//! feature or input report.
//!
//! The C++ original asks Windows' hidp for the value caps; here the report descriptor is walked
//! privately (see [`parse_battery_fields`]) to find where those usages sit in which report.
//!
//! TODO(port): Bluetooth. The original's second fallback (PollBluetooth) reads the level Windows
//! keeps in a Bluetooth device property (SetupAPI / DEVPKEY). That is Windows-only and is not
//! ported here.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use hidapi::HidDevice;

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, guess_kind, hex4, kind_from_collections};

fn is_battery_usage(page: u16, usage: u16) -> bool {
    (page == 0x06 && usage == 0x20) || (page == 0x85 && usage == 0x65)
}

/// A battery value found in a report descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatteryField {
    /// true = feature report, false = input report.
    pub feature: bool,
    pub report_id: u8,
    /// First bit of the value within the report data (after the report-id byte).
    pub bit_offset: u32,
    pub bit_size: u32,
    pub logical_min: i64,
    pub logical_max: i64,
}

/// Battery fields of one top-level collection.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TopBattery {
    pub usage_page: u16,
    pub usage: u16,
    pub fields: Vec<BatteryField>,
}

fn sign_extend(v: u32, size: usize) -> i64 {
    match size {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        _ => v as i32 as i64,
    }
}

/// Walks a report descriptor and lists, per top-level collection, the variable Input/Feature
/// fields whose usage is a battery usage. Like HIDP value caps, a usage range counts once (at its
/// minimum) and array items are ignored. Never panics on malformed input.
pub fn parse_battery_fields(desc: &[u8]) -> Vec<TopBattery> {
    #[derive(Clone, Default)]
    struct Globals {
        page: u32,
        lmin: i64,
        lmax: i64,
        lmax_raw: i64,
        size: u32,
        count: u32,
        id: u8,
    }
    #[derive(Default)]
    struct Locals {
        usages: Vec<(Option<u16>, u16)>, // (page override from a 32-bit usage, usage)
        min: Option<(Option<u16>, u16)>,
        has_range: bool,
    }
    let mut out = Vec::new();
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut l = Locals::default();
    let mut depth = 0u32;
    let mut cur: Option<TopBattery> = None;
    // bits used so far per (report id, is_feature) within the current top-level collection
    let mut bits: HashMap<(u8, bool), u32> = HashMap::new();
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xFE {
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        let size = match b & 3 {
            3 => 4,
            n => n as usize,
        };
        let (kind, tag) = ((b >> 2) & 3, b >> 4);
        let mut v: u32 = 0;
        for k in 0..size {
            v |= (desc.get(i + 1 + k).copied().unwrap_or(0) as u32) << (8 * k);
        }
        i += 1 + size;
        let ext = |v: u32| if size == 4 { (Some((v >> 16) as u16), v as u16) } else { (None, v as u16) };
        match (kind, tag) {
            (1, 0) => g.page = v,
            (1, 1) => {
                g.lmin = sign_extend(v, size);
            }
            (1, 2) => {
                g.lmax = sign_extend(v, size);
                g.lmax_raw = v as i64;
            }
            (1, 7) => g.size = v,
            (1, 8) => g.id = v as u8,
            (1, 9) => g.count = v,
            (1, 10) => stack.push(g.clone()),
            (1, 11) => g = stack.pop().unwrap_or_default(),
            (2, 0) => l.usages.push(ext(v)),
            (2, 1) => {
                l.min = Some(ext(v));
                l.has_range = true;
            }
            (0, 0xA) => {
                if depth == 0 {
                    let (pg, u) = l.usages.first().copied().unwrap_or((None, 0));
                    cur = Some(TopBattery { usage_page: pg.unwrap_or(g.page as u16), usage: u, fields: Vec::new() });
                    bits.clear();
                }
                depth += 1;
                l = Locals::default();
            }
            (0, 0xC) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(t) = cur.take() {
                        out.push(t);
                    }
                }
                l = Locals::default();
            }
            (0, t @ (8 | 9 | 0xB)) => {
                if t != 9 {
                    let feature = t == 0xB;
                    let used = bits.entry((g.id, feature)).or_insert(0);
                    let start = *used;
                    *used = used.saturating_add(g.size.saturating_mul(g.count));
                    let constant = v & 1 != 0;
                    let variable = v & 2 != 0;
                    if let (Some(top), false, true, true) = (cur.as_mut(), constant, variable, g.size > 0 && g.size <= 32) {
                        // lmax is unsigned when lmin is non-negative (0..255 in one item byte)
                        let lmax = if g.lmin >= 0 { g.lmax_raw } else { g.lmax };
                        let mut seen: Vec<(u16, u16)> = Vec::new();
                        let mut push = |page: u16, usage: u16, idx: u32, top: &mut TopBattery| {
                            if is_battery_usage(page, usage) && !seen.contains(&(page, usage)) {
                                seen.push((page, usage));
                                top.fields.push(BatteryField {
                                    feature,
                                    report_id: g.id,
                                    bit_offset: start.saturating_add(idx.saturating_mul(g.size)),
                                    bit_size: g.size,
                                    logical_min: g.lmin,
                                    logical_max: lmax,
                                });
                            }
                        };
                        if l.has_range {
                            if let Some((pg, u)) = l.min {
                                push(pg.unwrap_or(g.page as u16), u, 0, top);
                            }
                        } else if !l.usages.is_empty() {
                            for idx in 0..g.count.min(1024) {
                                let (pg, u) = l.usages[(idx as usize).min(l.usages.len() - 1)];
                                push(pg.unwrap_or(g.page as u16), u, idx, top);
                            }
                        }
                    }
                }
                l = Locals::default();
            }
            (0, _) => l = Locals::default(),
            _ => {}
        }
    }
    out
}

/// Value of `size` bits starting at bit `pos` (little-endian bit order) in `buf`.
fn extract_bits(buf: &[u8], pos: u32, size: u32) -> Option<u32> {
    if size == 0 || size > 32 || (pos as u64 + size as u64).div_ceil(8) > buf.len() as u64 {
        return None;
    }
    let mut v: u64 = 0;
    for k in 0..size {
        let p = (pos + k) as usize;
        v |= (((buf[p / 8] >> (p % 8)) & 1) as u64) << k;
    }
    Some(v as u32)
}

/// Raw field value -> percent. Apple devices (Magic Trackpad / Mouse / Keyboard) report a plain
/// percentage while declaring a 0-255 range (Linux: HID_BATTERY_QUIRK_PERCENT).
pub fn scale_percent(f: &BatteryField, val: u32, vid: u16) -> Option<i32> {
    if vid == 0x05AC && val <= 100 {
        return Some(val as i32);
    }
    if f.logical_max > f.logical_min {
        let p = (val as i32 as i64 - f.logical_min) * 100 / (f.logical_max - f.logical_min);
        return Some(clamp100(p.clamp(i32::MIN as i64, i32::MAX as i64) as i32) as i32);
    }
    if val <= 100 {
        return Some(val as i32);
    }
    None
}

/// Report access, abstracted for tests. Returns the report with `[0]` = report id.
trait ReportSource {
    fn get(&mut self, feature: bool, id: u8, len: usize) -> Option<Vec<u8>>;
}

struct Raw(HidDevice, #[cfg_attr(not(windows), allow(dead_code))] CString);

impl ReportSource for Raw {
    fn get(&mut self, feature: bool, id: u8, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len.max(1)];
        buf[0] = id;
        let n = if feature { self.0.get_feature_report(&mut buf).ok()? } else { self.input(id, &mut buf)? };
        buf.truncate(n.max(1));
        Some(buf)
    }
}

impl Raw {
    /// HidD_GetInputReport equivalent.
    #[cfg(not(windows))]
    fn input(&self, _id: u8, buf: &mut [u8]) -> Option<usize> {
        self.0.get_input_report(buf).ok()
    }

    /// hidapi's pure-Rust Windows backend has no get_input_report, so open the path again and
    /// call HidD_GetInputReport like the original.
    #[cfg(windows)]
    fn input(&self, _id: u8, buf: &mut [u8]) -> Option<usize> {
        use windows_sys::Win32::Devices::HumanInterfaceDevice::HidD_GetInputReport;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        let wide: Vec<u16> = self.1.to_string_lossy().encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated; the handle is closed before returning.
        unsafe {
            let h = CreateFileW(wide.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let ok = HidD_GetInputReport(h, buf.as_mut_ptr().cast(), buf.len() as u32);
            CloseHandle(h);
            ok.then_some(buf.len())
        }
    }
}

/// Feature reports first, then input reports; the first battery field that reads wins.
fn read_percent(src: &mut impl ReportSource, fields: &[BatteryField], feat_len: u16, in_len: u16, vid: u16) -> Option<i32> {
    for feature in [true, false] {
        let len = if feature { feat_len } else { in_len } as usize;
        if len == 0 {
            continue;
        }
        for f in fields.iter().filter(|f| f.feature == feature) {
            let Some(mut buf) = src.get(feature, f.report_id, len) else { continue };
            buf.resize(len, 0); // the OS returns a full-length buffer
            let Some(val) = extract_bits(&buf, 8 + f.bit_offset, f.bit_size) else { continue };
            if let Some(p) = scale_percent(f, val, vid) {
                return Some(p);
            }
        }
    }
    None
}

fn cache() -> &'static Mutex<HashMap<CString, Vec<TopBattery>>> {
    static C: OnceLock<Mutex<HashMap<CString, Vec<TopBattery>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Picks the top-level collection of `c` (macOS returns the whole interface descriptor).
fn pick<'a>(tops: &'a [TopBattery], page: u16, usage: u16) -> Option<&'a TopBattery> {
    tops.iter().find(|t| t.usage_page == page && t.usage == usage).or(if tops.len() == 1 { tops.first() } else { None })
}

pub fn poll(ctx: &mut PollContext) {
    for c in ctx.colls.clone() {
        if ctx.is_claimed(c.vid, c.pid) {
            continue;
        }
        if c.usage_page == 0x84 || c.usage_page == 0x85 {
            continue; // UPS / power supplies
        }
        if c.feat_len == 0 && c.in_len == 0 {
            continue;
        }
        let cached = cache().lock().ok().and_then(|m| m.get(&c.path).cloned());
        let mut dev = None;
        let tops = match cached {
            Some(t) => t,
            None => {
                let Ok(d) = ctx.api.open_path(&c.path) else { continue };
                let mut buf = vec![0u8; 4096];
                let tops = d.get_report_descriptor(&mut buf).map(|n| parse_battery_fields(&buf[..n])).unwrap_or_default();
                if let Ok(mut m) = cache().lock() {
                    m.insert(c.path.clone(), tops.clone());
                }
                dev = Some(d);
                tops
            }
        };
        let Some(top) = pick(&tops, c.usage_page, c.usage) else { continue };
        if top.fields.is_empty() {
            continue;
        }
        let d = match dev {
            Some(d) => d,
            None => match ctx.api.open_path(&c.path) {
                Ok(d) => d,
                Err(_) => continue,
            },
        };
        let Some(pct) = read_percent(&mut Raw(d, c.path.clone()), &top.fields, c.feat_len, c.in_len, c.vid) else { continue };
        let mut s = DeviceStatus {
            name: if c.product.is_empty() { "Wireless device".into() } else { c.product.clone() },
            key: format!("hid:{}:{}", hex4(c.vid), hex4(c.pid)),
            via: "HID".into(),
            kind: kind_from_collections(&ctx.colls, c.vid, c.pid),
            percent: Some(pct as u8),
            ..Default::default()
        };
        if s.kind == DeviceKind::Other {
            s.kind = guess_kind(&s.name);
        }
        ctx.add(s, c.vid, c.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(feature: bool, id: u8, off: u32, size: u32, lmin: i64, lmax: i64) -> BatteryField {
        BatteryField { feature, report_id: id, bit_offset: off, bit_size: size, logical_min: lmin, logical_max: lmax }
    }

    #[test]
    fn feature_battery_after_other_fields() {
        let d = [
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, // mouse application
            0x85, 0x02, // report id 2
            0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, // 3 buttons
            0x95, 0x01, 0x75, 0x05, 0x81, 0x03, // 5 bits padding
            0x85, 0x05, // report id 5
            0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x01, 0xB1, 0x02, // battery feature
            0xC0,
        ];
        let t = parse_battery_fields(&d);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].usage_page, t[0].usage), (0x01, 0x02));
        assert_eq!(t[0].fields, vec![field(true, 5, 0, 8, 0, 100)]);
    }

    #[test]
    fn input_battery_with_offset_and_wide_range() {
        let d = [
            0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, // keyboard
            0x75, 0x08, 0x95, 0x01, 0x81, 0x03, // 8 bit constant
            0x05, 0x85, 0x09, 0x65, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // 85:65 input
            0xC0,
        ];
        let t = parse_battery_fields(&d);
        assert_eq!(t[0].fields, vec![field(false, 0, 8, 8, 0, 255)]);
    }

    #[test]
    fn arrays_ranges_and_other_usages_are_ignored_or_counted_once() {
        let d = [
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01,
            0x05, 0x06, 0x09, 0x20, 0x75, 0x08, 0x95, 0x01, 0x15, 0x00, 0x25, 0x64, 0x81, 0x00, // array: ignored, 8 bits
            0x09, 0x21, 0x81, 0x02, // other usage, 8 bits
            0x19, 0x20, 0x29, 0x22, 0x95, 0x03, 0x81, 0x02, // range from 20: one cap at offset 16
            0xC0,
        ];
        let t = parse_battery_fields(&d);
        assert_eq!(t[0].fields, vec![field(false, 0, 16, 8, 0, 100)]);
    }

    #[test]
    fn usage_list_maps_fields_and_extended_usage() {
        // two fields: usage 0x21 then 0x20 (battery) at offset 8; then 32-bit extended usage 0x00060020
        let d = [
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01,
            0x05, 0x06, 0x09, 0x21, 0x09, 0x20, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x02, 0xB1, 0x02,
            0x05, 0x01, 0x0B, 0x20, 0x00, 0x06, 0x00, 0x95, 0x01, 0x81, 0x02,
            0xC0,
        ];
        let t = parse_battery_fields(&d);
        assert_eq!(t[0].fields, vec![field(true, 0, 8, 8, 0, 100), field(false, 0, 0, 8, 0, 100)]);
    }

    #[test]
    fn collections_are_separate_and_offsets_reset() {
        let d = [
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0xC0, // 32 bits, no battery
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xC0,
        ];
        let t = parse_battery_fields(&d);
        assert_eq!(t.len(), 2);
        assert!(t[0].fields.is_empty());
        assert_eq!((t[1].usage_page, t[1].usage), (0xFF00, 1));
        assert_eq!(t[1].fields, vec![field(false, 0, 0, 8, 0, 100)]);
    }

    #[test]
    fn push_pop_and_garbage_do_not_panic() {
        assert!(parse_battery_fields(&[]).is_empty());
        let _ = parse_battery_fields(&[0xFE, 0x05, 0x01]);
        let _ = parse_battery_fields(&[0xA1, 0x01, 0xB4, 0xB4, 0xC0, 0xC0, 0x81, 0xB1, 0x75]);
        let d = [0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xA4, 0x05, 0x06, 0x09, 0x20, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xB4, 0xC0];
        let t = parse_battery_fields(&d);
        assert_eq!(t[0].fields.len(), 1);
    }

    #[test]
    fn bit_extraction() {
        let buf = [0x00, 0b1010_0101, 0xFF];
        assert_eq!(extract_bits(&buf, 8, 8), Some(0xA5));
        assert_eq!(extract_bits(&buf, 12, 8), Some(0xFA));
        assert_eq!(extract_bits(&buf, 8, 16), Some(0xFFA5));
        assert_eq!(extract_bits(&buf, 16, 9), None);
        assert_eq!(extract_bits(&buf, 0, 0), None);
    }

    #[test]
    fn percent_scaling() {
        let f = field(true, 0, 0, 8, 0, 255);
        assert_eq!(scale_percent(&f, 255, 0x1234), Some(100));
        assert_eq!(scale_percent(&f, 51, 0x1234), Some(20));
        assert_eq!(scale_percent(&f, 51, 0x05AC), Some(51)); // Apple quirk: plain percentage
        assert_eq!(scale_percent(&f, 200, 0x05AC), Some(78));
        let p = field(true, 0, 0, 8, 0, 100);
        assert_eq!(scale_percent(&p, 150, 0x1234), Some(100));
        let flat = field(true, 0, 0, 8, 0, 0);
        assert_eq!(scale_percent(&flat, 80, 1), Some(80));
        assert_eq!(scale_percent(&flat, 180, 1), None);
        let neg = field(true, 0, 0, 8, -50, 50);
        assert_eq!(scale_percent(&neg, 0, 1), Some(50));
    }

    struct Mock {
        feature: Option<Vec<u8>>,
        input: Option<Vec<u8>>,
        asked: Vec<(bool, u8, usize)>,
    }

    impl ReportSource for Mock {
        fn get(&mut self, feature: bool, id: u8, len: usize) -> Option<Vec<u8>> {
            self.asked.push((feature, id, len));
            if feature { self.feature.clone() } else { self.input.clone() }
        }
    }

    #[test]
    fn feature_wins_then_input_fallback() {
        let fields = [field(false, 0, 8, 8, 0, 100), field(true, 3, 0, 8, 0, 100)];
        let mut m = Mock { feature: Some(vec![3, 64]), input: Some(vec![0, 0, 10]), asked: vec![] };
        assert_eq!(read_percent(&mut m, &fields, 2, 3, 1), Some(64));
        assert_eq!(m.asked, vec![(true, 3, 2)]);
        let mut m = Mock { feature: None, input: Some(vec![0, 0, 10]), asked: vec![] };
        assert_eq!(read_percent(&mut m, &fields, 2, 3, 1), Some(10));
        assert_eq!(m.asked, vec![(true, 3, 2), (false, 0, 3)]);
    }

    #[test]
    fn zero_lengths_short_buffers_and_failures() {
        let fields = [field(true, 3, 0, 8, 0, 100)];
        let mut m = Mock { feature: Some(vec![3, 50]), input: None, asked: vec![] };
        assert_eq!(read_percent(&mut m, &fields, 0, 0, 1), None);
        assert!(m.asked.is_empty());
        // short reply is zero-padded to the declared length
        let mut m = Mock { feature: Some(vec![3]), input: None, asked: vec![] };
        assert_eq!(read_percent(&mut m, &fields, 2, 0, 1), Some(0));
        // value beyond declared length fails
        let far = [field(true, 3, 40, 8, 0, 100)];
        let mut m = Mock { feature: Some(vec![3, 50]), input: None, asked: vec![] };
        assert_eq!(read_percent(&mut m, &far, 2, 0, 1), None);
    }

    #[test]
    fn picks_collection_by_usage_or_single() {
        let a = TopBattery { usage_page: 1, usage: 2, fields: vec![] };
        let b = TopBattery { usage_page: 1, usage: 6, fields: vec![field(true, 0, 0, 8, 0, 100)] };
        let two = [a.clone(), b.clone()];
        assert_eq!(pick(&two, 1, 6), Some(&b));
        assert_eq!(pick(&two, 0xFF00, 1), None);
        assert_eq!(pick(&[a.clone()], 0xFF00, 1), Some(&a));
    }
}

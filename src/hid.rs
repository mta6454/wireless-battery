//! Thin HID layer shared by all drivers, built on hidapi (port of hid.h / hid.cpp).
//!
//! Report buffers follow hidapi conventions: `data[0]` is the report ID (0 when the device does
//! not number its reports) for writes and feature reports.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use hidapi::{HidApi, HidDevice};

use crate::device::DeviceKind;

/// One top-level HID collection of a device.
#[derive(Clone, Debug)]
pub struct HidColl {
    pub path: CString,
    pub vid: u16,
    pub pid: u16,
    /// USB interface number, -1 when unknown.
    pub iface: i32,
    pub usage_page: u16,
    pub usage: u16,
    pub product: String,
    /// Report lengths in bytes *including* the report-id byte, like Windows' HIDP_CAPS
    /// (InputReportByteLength etc.): a 64-byte unnumbered report is 65. 0 = no such report, or
    /// unknown because the descriptor could not be read.
    pub in_len: u16,
    pub out_len: u16,
    pub feat_len: u16,
}

impl HidColl {
    /// Vendor-defined usage pages (0xFF00..=0xFFFF) carry the proprietary battery protocols.
    pub fn is_vendor_page(&self) -> bool {
        self.usage_page >= 0xFF00
    }
}

type Lens = Vec<TopColl>;

fn bluetooth_paths() -> &'static Mutex<std::collections::HashSet<CString>> {
    static B: OnceLock<Mutex<std::collections::HashSet<CString>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// True when hidapi reported this collection on the Bluetooth bus during the last enumeration.
/// Lets drivers tell Bluetooth from USB when the report lengths are unreadable (macOS).
pub fn is_bluetooth(c: &HidColl) -> bool {
    bluetooth_paths().lock().map(|s| s.contains(&c.path)).unwrap_or(false)
}

fn descriptor_cache() -> &'static Mutex<HashMap<CString, Lens>> {
    static C: OnceLock<Mutex<HashMap<CString, Lens>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Report descriptor of the device at `path`, parsed per top-level collection (cached: opening
/// every device on every poll would be wasteful).
fn collections_of(api: &HidApi, path: &CString) -> Lens {
    if let Some(l) = descriptor_cache().lock().ok().and_then(|c| c.get(path).cloned()) {
        return l;
    }
    let mut lens = Vec::new();
    if let Ok(dev) = api.open_path(path) {
        let mut buf = vec![0u8; 4096];
        if let Ok(n) = dev.get_report_descriptor(&mut buf) {
            lens = parse_descriptor(&buf[..n]);
        }
    }
    if let Ok(mut c) = descriptor_cache().lock() {
        c.insert(path.clone(), lens.clone());
    }
    lens
}

pub fn enum_hid(api: &HidApi) -> Vec<HidColl> {
    if let Ok(mut set) = bluetooth_paths().lock() {
        set.clear();
        set.extend(api.device_list().filter(|d| matches!(d.bus_type(), hidapi::BusType::Bluetooth)).map(|d| d.path().to_owned()));
    }
    api.device_list()
        .map(|d| {
            let path = d.path().to_owned();
            let tops = collections_of(api, &path);
            // macOS reports the whole interface descriptor for every collection; pick ours.
            let mine = tops
                .iter()
                .find(|t| t.usage_page == d.usage_page() && t.usage == d.usage())
                .or(if tops.len() == 1 { tops.first() } else { None });
            let (in_len, out_len, feat_len) = mine.map(|t| (t.in_len, t.out_len, t.feat_len)).unwrap_or((0, 0, 0));
            HidColl {
                path,
                vid: d.vendor_id(),
                pid: d.product_id(),
                iface: d.interface_number(),
                usage_page: d.usage_page(),
                usage: d.usage(),
                product: d.product_string().unwrap_or("").trim().to_string(),
                in_len,
                out_len,
                feat_len,
            }
        })
        .collect()
}

/// Top-level collection found in a report descriptor, with its report lengths.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TopColl {
    pub usage_page: u16,
    pub usage: u16,
    pub in_len: u16,
    pub out_len: u16,
    pub feat_len: u16,
}

/// Minimal HID report-descriptor parser: per top-level collection, the largest Input / Output /
/// Feature report in bytes, +1 for the report-id byte (Windows convention, see [`HidColl`]).
pub fn parse_descriptor(desc: &[u8]) -> Vec<TopColl> {
    #[derive(Clone, Default)]
    struct Globals {
        page: u32,
        size: u32,
        count: u32,
        id: u8,
    }
    #[derive(Default)]
    struct Open {
        top: TopColl,
        // bits per report id: [input, output, feature]
        bits: BTreeMap<u8, [u32; 3]>,
    }
    let mut out = Vec::new();
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut first_usage: Option<u32> = None;
    let mut depth = 0u32;
    let mut cur: Option<Open> = None;
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xFE {
            // long item: 0xFE, length, tag, data...
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
        match (kind, tag) {
            (1, 0) => g.page = v,
            (1, 7) => g.size = v,
            (1, 8) => g.id = v as u8,
            (1, 9) => g.count = v,
            (1, 10) => stack.push(g.clone()),
            (1, 11) => g = stack.pop().unwrap_or_default(),
            (2, 0) => {
                first_usage.get_or_insert(v);
            }
            (0, 0xA) => {
                if depth == 0 {
                    cur = Some(Open {
                        top: TopColl {
                            usage_page: g.page as u16,
                            usage: first_usage.unwrap_or(0) as u16,
                            ..Default::default()
                        },
                        bits: BTreeMap::new(),
                    });
                }
                depth += 1;
                first_usage = None;
            }
            (0, 0xC) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(mut o) = cur.take() {
                        let len = |n: usize| o.bits.values().map(|b| b[n].div_ceil(8)).max().filter(|m| *m > 0).map_or(0, |m| m as u16 + 1);
                        o.top.in_len = len(0);
                        o.top.out_len = len(1);
                        o.top.feat_len = len(2);
                        out.push(o.top);
                    }
                }
                first_usage = None;
            }
            (0, t @ (8 | 9 | 0xB)) => {
                if let Some(o) = cur.as_mut() {
                    let slot = match t {
                        8 => 0,
                        9 => 1,
                        _ => 2,
                    };
                    o.bits.entry(g.id).or_default()[slot] += g.size * g.count;
                }
                first_usage = None;
            }
            (0, _) => first_usage = None,
            _ => {}
        }
    }
    out
}

/// Feature-report access, abstracted so drivers can be tested without hardware.
pub trait FeatureIo {
    /// `data[0]` = report ID. Returns true on success.
    fn set_feature(&mut self, data: &[u8]) -> bool;
    /// Reads feature report `id` into a buffer of `len` bytes; `out[0]` stays the report ID.
    fn get_feature(&mut self, id: u8, len: usize) -> Option<Vec<u8>>;
}

/// Everything a driver needs from an open device; mockable in tests.
/// Buffers follow hidapi conventions: `data[0]` is the report ID (0 when the device does not
/// number its reports). `read` strips a leading 0 report ID for unnumbered reports.
pub trait HidIo: FeatureIo {
    /// Zero-pads to the collection's output report length, then writes.
    fn write(&mut self, data: &[u8]) -> bool;
    /// One input report, or None on timeout/error.
    fn read(&mut self, timeout_ms: i32) -> Option<Vec<u8>>;
    /// Discards queued input reports.
    fn flush(&mut self);
}

pub struct HidDev {
    dev: HidDevice,
    coll: HidColl,
}

impl HidDev {
    /// Opens a collection for reading and writing (shared access).
    pub fn open(api: &HidApi, c: &HidColl) -> Option<Self> {
        api.open_path(&c.path).ok().map(|dev| Self { dev, coll: c.clone() })
    }

    /// Same as [`open`]; kept as a separate name so ported call sites read like the original.
    pub fn open_feature_only(api: &HidApi, c: &HidColl) -> Option<Self> {
        Self::open(api, c)
    }

    /// Feature report `id` using the collection's feature report length.
    pub fn get_feature_default(&mut self, id: u8) -> Option<Vec<u8>> {
        let len = self.coll.feat_len as usize;
        if len == 0 {
            return None;
        }
        FeatureIo::get_feature(self, id, len)
    }
}

impl FeatureIo for HidDev {
    /// Zero-pads to the collection's feature report length (when known), then sends.
    fn set_feature(&mut self, data: &[u8]) -> bool {
        let n = (self.coll.feat_len as usize).max(data.len());
        let mut buf = vec![0u8; n];
        buf[..data.len()].copy_from_slice(data);
        self.dev.send_feature_report(&buf).is_ok()
    }

    fn get_feature(&mut self, id: u8, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; len.max(1)];
        buf[0] = id;
        let n = self.dev.get_feature_report(&mut buf).ok()?;
        buf.truncate(n.max(1));
        Some(buf)
    }
}

impl HidIo for HidDev {
    fn write(&mut self, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        let n = (self.coll.out_len as usize).max(data.len());
        let mut buf = vec![0u8; n];
        buf[..data.len()].copy_from_slice(data);
        self.dev.write(&buf).is_ok()
    }

    fn read(&mut self, timeout_ms: i32) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; (self.coll.in_len as usize).max(64) + 1];
        match self.dev.read_timeout(&mut buf, timeout_ms) {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                Some(buf)
            }
            _ => None,
        }
    }

    fn flush(&mut self) {
        let mut buf = vec![0u8; (self.coll.in_len as usize).max(64) + 1];
        while matches!(self.dev.read_timeout(&mut buf, 0), Ok(n) if n > 0) {}
    }
}

/// Sleep helper so ported `Sleep(ms)` calls stay one-liners.
pub fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

pub fn hex4(v: u16) -> String {
    format!("{v:04X}")
}

pub fn clamp100(v: i32) -> u8 {
    v.clamp(0, 100) as u8
}

/// Linear map lo..hi -> 0..100, clamped.
pub fn map_to_100(v: i32, lo: i32, hi: i32) -> u8 {
    if hi <= lo {
        return clamp100(v);
    }
    clamp100((v - lo) * 100 / (hi - lo))
}

/// Piecewise-linear interpolation through (mv[i], pct[i]); tables sorted by descending voltage.
pub fn interpolate(mv: i32, mv_table: &[i32], pct_table: &[i32]) -> i32 {
    let n = mv_table.len();
    if mv >= mv_table[0] {
        return pct_table[0];
    }
    for i in 1..n {
        if mv >= mv_table[i] {
            let span = mv_table[i - 1] - mv_table[i];
            let span = if span == 0 { 1 } else { span };
            return pct_table[i] + (pct_table[i - 1] - pct_table[i]) * (mv - mv_table[i]) / span;
        }
    }
    pct_table[n - 1]
}

pub fn guess_kind(name: &str) -> DeviceKind {
    let n = name.to_lowercase();
    let has = |s: &str| n.contains(s);
    if has("mouse") || has("mice") || has("trackball") {
        return DeviceKind::Mouse;
    }
    if has("keyboard") || has("keys") {
        return DeviceKind::Keyboard;
    }
    if has("controller") || has("gamepad") || has("xbox") || has("dualsense") || has("dualshock") || has("joy-con") {
        return DeviceKind::Gamepad;
    }
    if has("head") || has("buds") || has("pods") || has("ear") || has("wh-") || has("wf-") || has("audio")
        || has("speaker") || has("arctis") || has("cloud") || has("void") || has("virtuoso")
    {
        return DeviceKind::Headset;
    }
    DeviceKind::Other
}

/// Device type from the top-level collections a device exposes (mouse > gamepad > keyboard).
pub fn kind_from_collections(all: &[HidColl], vid: u16, pid: u16) -> DeviceKind {
    let (mut mouse, mut pad, mut kbd) = (false, false, false);
    for c in all.iter().filter(|c| c.vid == vid && c.pid == pid && c.usage_page == 0x01) {
        mouse |= c.usage == 0x02;
        pad |= c.usage == 0x04 || c.usage == 0x05;
        kbd |= c.usage == 0x06;
    }
    if mouse {
        DeviceKind::Mouse
    } else if pad {
        DeviceKind::Gamepad
    } else if kbd {
        DeviceKind::Keyboard
    } else {
        DeviceKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vendor collection FF00:01, one 64-byte unnumbered input, output and feature report.
    const VENDOR_64: [u8; 31] = [
        0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40,
        0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0x91, 0x02, 0x09, 0x01, 0xB1, 0x02, 0xC0, 0x00, 0x00,
    ];

    #[test]
    fn descriptor_lengths_include_report_id_byte() {
        let t = parse_descriptor(&VENDOR_64[..29]);
        assert_eq!(t, vec![TopColl { usage_page: 0xFF00, usage: 1, in_len: 65, out_len: 65, feat_len: 65 }]);
    }

    #[test]
    fn numbered_reports_take_the_largest_and_split_collections() {
        // Logitech-style: short (id 0x10, 6 bytes out) and long (id 0x11, 19 bytes out) in one
        // top-level collection, plus a second top-level collection with a 4-byte input.
        let d = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, // FF00:01 application
            0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x09, 0x01, 0x91, 0x00, // id 0x10, output 6 bytes
            0x85, 0x11, 0x95, 0x13, 0x09, 0x02, 0x91, 0x00, // id 0x11, output 19 bytes
            0xC0, //
            0x06, 0x43, 0xFF, 0x09, 0x02, 0xA1, 0x01, // FF43:02 application
            0x75, 0x08, 0x95, 0x04, 0x09, 0x01, 0x81, 0x00, // input 4 bytes
            0xC0,
        ];
        let t = parse_descriptor(&d);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].usage_page, t[0].usage, t[0].out_len, t[0].in_len), (0xFF00, 1, 20, 0));
        assert_eq!((t[1].usage_page, t[1].usage, t[1].in_len, t[1].out_len), (0xFF43, 2, 5, 0));
    }

    #[test]
    fn push_pop_and_junk_do_not_panic() {
        assert!(parse_descriptor(&[]).is_empty());
        assert!(parse_descriptor(&[0xFE, 0x05, 0x01]).is_empty());
        let _ = parse_descriptor(&[0xA1, 0x01, 0xB4, 0xB4, 0xC0, 0xC0, 0x81]); // unbalanced
    }

    #[test]
    fn map_and_clamp() {
        assert_eq!(map_to_100(3300, 3000, 4200), 25);
        assert_eq!(map_to_100(2000, 3000, 4200), 0);
        assert_eq!(map_to_100(5000, 3000, 4200), 100);
        assert_eq!(clamp100(250), 100);
    }

    #[test]
    fn interpolation_matches_cpp_semantics() {
        let mv = [4200, 3800, 3500];
        let pct = [100, 50, 0];
        assert_eq!(interpolate(4300, &mv, &pct), 100);
        assert_eq!(interpolate(4000, &mv, &pct), 75);
        assert_eq!(interpolate(3000, &mv, &pct), 0);
    }

    #[test]
    fn guesses_kind_from_name() {
        assert_eq!(guess_kind("Logitech Wireless Mouse"), DeviceKind::Mouse);
        assert_eq!(guess_kind("Xbox Controller"), DeviceKind::Gamepad);
        assert_eq!(guess_kind("ARCTIS 7"), DeviceKind::Headset);
        assert_eq!(guess_kind("Widget"), DeviceKind::Other);
    }
}

//! Logitech HID++ (VID 046D): Unifying / Bolt / Lightspeed / Nano receivers, wired or Bluetooth
//! devices, and G headsets. Features are discovered through the root feature, so devices are
//! supported without a per-model table.
//!
//! Message: [report][device index][feature index][function << 4 | sw id][params...]
//!   report 0x10 = short (7 bytes), 0x11 = long (20 bytes)
//!   HID++ 2.0 error: [.., 0xFF, feature, fn|sw, code]   HID++ 1.0 error: [.., 0x8F, sub id, address, code]
//! Battery features: 0x1004 unified battery, 0x1000 battery level, 0x1001 battery voltage,
//! 0x1F20 ADC measurement (headsets). HID++ 1.0 devices: registers 0x0D and 0x07.
//! References: Logitech HID++ 2.0 specification drafts, github.com/pwr-Solaar/Solaar,
//! github.com/Sapd/HeadsetControl (headset voltage calibration).

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, guess_kind, hex4, interpolate, HidDev, HidIo};

/// Our software id; replies to other apps (G HUB, Options+) carry theirs.
const SW_ID: u8 = 0x0D;

/// Outcome of one HID++ request.
#[derive(Debug, PartialEq)]
enum Reply {
    /// Parameters of the reply (bytes after the 4-byte header).
    Ok(Vec<u8>),
    /// Error reply with its code.
    Err(u8),
    /// No answer in time (or nothing to send on).
    None,
}

struct Hidpp<D: HidIo> {
    short: Option<D>,
    long: Option<D>,
    /// Upper bound for any request timeout; tests lower it so silent devices do not stall them.
    timeout_cap_ms: u64,
}

impl<D: HidIo> Hidpp<D> {
    fn new(short: Option<D>, long: Option<D>) -> Self {
        Self { short, long, timeout_cap_ms: u64::MAX }
    }

    /// Sends a request and waits for its reply.
    fn request(&mut self, idx: u8, b2: u8, b3: u8, params: &[u8], force_short: bool, timeout_ms: u64) -> Reply {
        let use_long = self.long.is_some() && !force_short;
        if !use_long && self.short.is_none() {
            return Reply::None;
        }
        let mut msg = vec![0u8; if use_long { 20 } else { 7 }];
        msg[0] = if use_long { 0x11 } else { 0x10 };
        msg[1] = idx;
        msg[2] = b2;
        msg[3] = b3;
        for (i, v) in params.iter().take(msg.len() - 4).enumerate() {
            msg[4 + i] = *v;
        }
        let sender = if use_long { self.long.as_mut() } else { self.short.as_mut() };
        if !sender.is_some_and(|d| d.write(&msg)) {
            return Reply::None;
        }

        let end = Instant::now() + Duration::from_millis(timeout_ms.min(self.timeout_cap_ms));
        while Instant::now() < end {
            for d in [self.long.as_mut(), self.short.as_mut()].into_iter().flatten() {
                let Some(r) = d.read(15) else { continue };
                if r.len() < 7 || (r[0] != 0x10 && r[0] != 0x11) || r[1] != idx {
                    continue;
                }
                if r[2] == b2 && r[3] == b3 {
                    return Reply::Ok(r[4..].to_vec());
                }
                if (r[2] == 0xFF || r[2] == 0x8F) && r[3] == b2 && r[4] == b3 {
                    return Reply::Err(r[5]);
                }
            }
        }
        Reply::None
    }

    /// HID++ 2.0 call: feature index + function number.
    fn call(&mut self, idx: u8, feature: u8, func: u8, params: &[u8]) -> Option<Vec<u8>> {
        match self.request(idx, feature, (func << 4) | SW_ID, params, false, 500) {
            Reply::Ok(p) => Some(p),
            _ => None,
        }
    }

    /// Index of `feature_id` in the device's feature table, 0 when unsupported.
    fn feature_index(&mut self, idx: u8, feature_id: u16) -> u8 {
        match self.call(idx, 0x00, 0, &[(feature_id >> 8) as u8, feature_id as u8]) {
            Some(p) if !p.is_empty() => p[0],
            _ => 0,
        }
    }

    /// HID++ 2.0 ping (root feature, function 1); `Some(protocol major)` when it answers.
    fn ping(&mut self, idx: u8) -> Reply {
        self.request(idx, 0x00, 0x10 | SW_ID, &[0, 0, 0x5A], false, 400)
    }
}

/// Voltage -> % for a typical Li-ion cell (mice/keyboards).
fn li_ion_percent(mv: i32) -> i32 {
    const V: [i32; 13] = [4186, 4067, 3989, 3922, 3859, 3811, 3778, 3751, 3717, 3671, 3646, 3579, 3500];
    const P: [i32; 13] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 5, 2, 0];
    interpolate(mv, &V, &P)
}

/// Voltage -> % for G headsets (calibration from HeadsetControl).
fn headset_percent(pid: u16, mv: i32) -> i32 {
    const G533V: [i32; 6] = [4200, 3850, 3790, 3750, 3680, 3330];
    const G535V: [i32; 6] = [4175, 3817, 3766, 3730, 3664, 3310];
    const G633V: [i32; 8] = [4100, 3950, 3850, 3750, 3650, 3500, 3300, 3150];
    const G633P: [i32; 8] = [100, 80, 60, 40, 20, 10, 5, 0];
    const GPROV: [i32; 6] = [4150, 3830, 3780, 3740, 3670, 3320];
    const DEFV: [i32; 6] = [4100, 3850, 3780, 3730, 3650, 3300];
    const P6: [i32; 6] = [100, 50, 30, 20, 5, 0]; // shared by every 6-point table
    match pid {
        0x0A66 => interpolate(mv, &G533V, &P6),
        0x0AC4 => interpolate(mv, &G535V, &P6),
        0x0A5C | 0x0A89 | 0x0A5B | 0x0A87 | 0x0AB5 | 0x0AFE => interpolate(mv, &G633V, &G633P),
        0x0AA7 | 0x0AAA | 0x0ABA | 0x0AFB => interpolate(mv, &GPROV, &P6),
        _ => interpolate(mv, &DEFV, &P6),
    }
}

fn kind_from_type(t: u8) -> DeviceKind {
    match t {
        0x00 | 0x02 => DeviceKind::Keyboard,
        0x03..=0x05 => DeviceKind::Mouse,
        0x08 => DeviceKind::Headset,
        0x0B | 0x0C => DeviceKind::Gamepad,
        _ => DeviceKind::Other,
    }
}

/// Fills battery fields from whichever battery feature the device has. Returns false if none.
fn read_battery20<D: HidIo>(h: &mut Hidpp<D>, idx: u8, pid: u16, s: &mut DeviceStatus) -> bool {
    let f = h.feature_index(idx, 0x1004); // UNIFIED_BATTERY: get_status (fn 1)
    if f != 0 {
        if let Some(p) = h.call(idx, f, 1, &[]).filter(|p| p.len() >= 3) {
            let (soc, level, st) = (p[0], p[1], p[2]);
            if soc > 0 && soc <= 100 {
                s.percent = Some(soc);
            } else if level != 0 {
                s.percent = Some(if level & 8 != 0 { 100 } else if level & 4 != 0 { 60 } else if level & 2 != 0 { 20 } else { 5 });
                s.level_text = if level & 8 != 0 {
                    "Full"
                } else if level & 4 != 0 {
                    "Good"
                } else if level & 2 != 0 {
                    "Low"
                } else {
                    "Critical"
                }
                .into();
            }
            s.charging = st == 1 || st == 2 || st == 3;
            return true;
        }
    }
    let f = h.feature_index(idx, 0x1000); // BATTERY_STATUS: GetBatteryLevelStatus (fn 0)
    if f != 0 {
        if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| p.len() >= 3) {
            let st = p[2];
            s.charging = (1..=4).contains(&st);
            s.percent = (p[0] <= 100).then_some(p[0]);
            if s.percent == Some(0) && st == 3 {
                s.percent = Some(100);
            }
            return true;
        }
    }
    let f = h.feature_index(idx, 0x1001); // BATTERY_VOLTAGE: GetBatteryInfo (fn 0)
    if f != 0 {
        if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| p.len() >= 3) {
            s.millivolts = ((p[0] as u32) << 8) | p[1] as u32;
            s.charging = p[2] & 0x80 != 0;
            if s.millivolts != 0 {
                s.percent = Some(clamp100(li_ion_percent(s.millivolts as i32)));
            }
            return true;
        }
    }
    let f = h.feature_index(idx, 0x1F20); // ADC_MEASUREMENT (headsets)
    if f != 0 {
        if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| p.len() >= 3) {
            s.millivolts = ((p[0] as u32) << 8) | p[1] as u32;
            s.charging = p[2] == 0x03;
            if s.millivolts != 0 {
                s.percent = Some(clamp100(headset_percent(pid, s.millivolts as i32)));
            }
            return true;
        }
    }
    false
}

fn read_name_and_type20<D: HidIo>(h: &mut Hidpp<D>, idx: u8, s: &mut DeviceStatus) {
    let f = h.feature_index(idx, 0x0005); // DEVICE_NAME
    if f == 0 {
        return;
    }
    if let Some(p) = h.call(idx, f, 0, &[]).filter(|p| !p.is_empty()) {
        let len = p[0] as usize;
        let mut name = String::new();
        while name.len() < len {
            let Some(p) = h.call(idx, f, 1, &[name.len() as u8]).filter(|p| !p.is_empty()) else { break };
            let before = name.len();
            for ch in p {
                if name.len() >= len || ch == 0 {
                    break;
                }
                name.push(ch as char);
            }
            if name.len() == before {
                break;
            }
        }
        if !name.is_empty() {
            s.name = if name.starts_with("Logitech") || name.starts_with("G ") || name.starts_with("MX") {
                name
            } else {
                format!("Logitech {name}")
            };
        }
    }
    if let Some(p) = h.call(idx, f, 2, &[]).filter(|p| !p.is_empty()) {
        s.kind = kind_from_type(p[0]);
    }
}

/// HID++ 1.0 devices: register 0x0D (battery %) or 0x07 (battery level 1..7).
fn read_battery10<D: HidIo>(h: &mut Hidpp<D>, idx: u8, s: &mut DeviceStatus) -> bool {
    if let Reply::Ok(p) = h.request(idx, 0x81, 0x0D, &[], true, 500) {
        if p.len() >= 3 && p[0] != 0 && p[0] <= 100 {
            s.percent = Some(p[0]);
            s.charging = (0x21..=0x26).contains(&p[2]);
            return true;
        }
    }
    if let Reply::Ok(p) = h.request(idx, 0x81, 0x07, &[], true, 500) {
        if p.len() >= 2 && p[0] != 0 {
            const PCT: [u8; 8] = [0, 5, 10, 20, 35, 50, 75, 100];
            s.percent = Some(PCT[p[0].min(7) as usize]);
            s.level_text = if p[0] >= 7 {
                "Full"
            } else if p[0] >= 5 {
                "Good"
            } else if p[0] >= 3 {
                "Low"
            } else {
                "Critical"
            }
            .into();
            s.charging = (0x21..=0x26).contains(&p[1]);
            return true;
        }
    }
    false
}

/// Probes every candidate device index behind one HID++ interface pair and returns what it found.
/// `long_is_ff43` is true when the long collection is on vendor page FF43 (G headsets).
fn poll_product<D: HidIo>(h: &mut Hidpp<D>, pid: u16, product: &str, long_is_ff43: bool) -> Vec<DeviceStatus> {
    let is_receiver = product.contains("Receiver") || product.contains("USB Receiver");

    // A device answering a HID++ 2.0 ping at index 0xFF is connected directly (USB cable,
    // Bluetooth, or a receiver-less headset); otherwise it is a receiver with paired devices.
    let direct = !is_receiver && matches!(h.ping(0xFF), Reply::Ok(p) if p.len() >= 3 && p[0] >= 2);
    let indices: Vec<u8> = if direct { vec![0xFF] } else { vec![1, 2, 3, 4, 5, 6] };

    let mut out = Vec::new();
    for idx in indices {
        let mut s = DeviceStatus { key: format!("logi:{}:{}", hex4(pid), idx), ..Default::default() };
        s.name = if idx == 0xFF && !product.is_empty() { product.to_string() } else { "Logitech wireless device".into() };
        s.via = if idx == 0xFF { "USB / Bluetooth" } else { "Logitech receiver" }.into();
        s.grace_sec = if idx == 0xFF { 0 } else { 1800 }; // receiver devices drop off the link while sleeping
        s.kind = guess_kind(&s.name);

        let r = h.ping(idx);
        let found = match r {
            Reply::Ok(ref p) if p.len() >= 3 && p[0] >= 2 => {
                read_name_and_type20(h, idx, &mut s);
                let found = read_battery20(h, idx, pid, &mut s);
                if found && s.kind == DeviceKind::Other && long_is_ff43 {
                    s.kind = DeviceKind::Headset;
                }
                found
            }
            Reply::Err(0x01) => read_battery10(h, idx, &mut s), // "invalid sub id": a HID++ 1.0 device
            _ => false,
        };
        if found {
            if s.kind == DeviceKind::Other {
                s.kind = guess_kind(&s.name);
            }
            out.push(s);
        }
    }
    out
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x046D) {
        let shorts = ctx.find(vid, pid, |c| c.usage_page == 0xFF00 && c.usage == 0x0001 && c.out_len == 7);
        let longs = ctx.find(vid, pid, |c| {
            ((c.usage_page == 0xFF00 && c.usage == 0x0002) || (c.usage_page == 0xFF43 && c.usage == 0x0202)) && c.out_len == 20
        });
        if shorts.is_empty() && longs.is_empty() {
            continue;
        }
        let sd = shorts.first().and_then(|c| HidDev::open(ctx.api, c));
        let ld = longs.first().and_then(|c| HidDev::open(ctx.api, c));
        if sd.is_none() && ld.is_none() {
            continue;
        }
        let mut h = Hidpp::new(sd, ld);
        let product = longs.first().or(shorts.first()).map(|c| c.product.clone()).unwrap_or_default();
        let long_is_ff43 = longs.first().is_some_and(|c| c.usage_page == 0xFF43);

        let found = poll_product(&mut h, pid, &product, long_is_ff43);
        let any = !found.is_empty();
        ctx.out.extend(found);
        if any {
            ctx.claim(vid, pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;
    use std::collections::{HashMap, VecDeque};

    /// What a simulated device index answers.
    #[derive(Clone, Default)]
    struct Dev {
        /// HID++ 2.0: feature id -> feature index
        features: HashMap<u16, u8>,
        /// (feature index, function) -> reply params
        funcs: HashMap<(u8, u8), Vec<u8>>,
        /// HID++ 1.0: register -> reply params (device answers ping with error 0x01)
        regs: HashMap<u8, Vec<u8>>,
        hidpp10: bool,
        /// feature index -> data served in 16-byte slices by function 1 (DEVICE_NAME get-char)
        chunked: HashMap<u8, Vec<u8>>,
    }

    /// Simulated HID++ endpoint: reacts to each write by queueing the protocol-correct reply.
    struct Mock {
        devs: HashMap<u8, Dev>,
        queue: VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
    }

    impl Mock {
        fn new(devs: Vec<(u8, Dev)>) -> Self {
            Self { devs: devs.into_iter().collect(), queue: VecDeque::new(), writes: vec![] }
        }
        fn reply(&mut self, mut r: Vec<u8>) {
            r.resize(20, 0);
            self.queue.push_back(r);
        }
    }

    impl FeatureIo for Mock {
        fn set_feature(&mut self, _: &[u8]) -> bool {
            false
        }
        fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> {
            None
        }
    }

    impl HidIo for Mock {
        fn write(&mut self, m: &[u8]) -> bool {
            self.writes.push(m.to_vec());
            let (idx, b2, b3) = (m[1], m[2], m[3]);
            let Some(dev) = self.devs.get(&idx).cloned() else { return true }; // nobody home
            if dev.hidpp10 {
                if b2 == 0x00 {
                    // HID++ 1.0 device: "invalid sub id" for any 2.0 root call
                    self.reply(vec![0x10, idx, 0x8F, b2, b3, 0x01]);
                } else if let Some(p) = dev.regs.get(&b3) {
                    let mut r = vec![0x10, idx, b2, b3];
                    r.extend(p);
                    self.reply(r);
                } else {
                    self.reply(vec![0x10, idx, 0x8F, b2, b3, 0x02]);
                }
                return true;
            }
            let mut r = vec![0x11, idx, b2, b3];
            if b2 == 0x00 && b3 >> 4 == 1 {
                r.extend([4, 2, 0x5A]); // ping: protocol 4.2, echoes the ping data
            } else if b2 == 0x00 && b3 >> 4 == 0 {
                let id = ((m[4] as u16) << 8) | m[5] as u16;
                r.push(*dev.features.get(&id).unwrap_or(&0));
            } else if let (Some(data), 1) = (dev.chunked.get(&b2), b3 >> 4) {
                let off = (m[4] as usize).min(data.len());
                r.extend(&data[off..data.len().min(off + 16)]);
            } else if let Some(p) = dev.funcs.get(&(b2, b3 >> 4)) {
                r.extend(p);
            } else {
                r = vec![0x11, idx, 0xFF, b2, b3, 0x02]; // not supported
            }
            self.reply(r);
            true
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            self.queue.pop_front()
        }
        fn flush(&mut self) {
            self.queue.clear();
        }
    }

    fn hp(devs: Vec<(u8, Dev)>) -> Hidpp<Mock> {
        let mut h = Hidpp::new(Some(Mock::new(devs.clone())), Some(Mock::new(devs)));
        h.timeout_cap_ms = 30;
        h
    }

    fn dev20(features: &[(u16, u8)], funcs: &[((u8, u8), Vec<u8>)]) -> Dev {
        Dev { features: features.iter().cloned().collect(), funcs: funcs.iter().cloned().collect(), ..Default::default() }
    }

    fn run(devs: Vec<(u8, Dev)>, pid: u16, product: &str) -> Vec<DeviceStatus> {
        poll_product(&mut hp(devs), pid, product, false)
    }

    #[test]
    fn request_layout_short_and_long() {
        let mut h = hp(vec![]);
        h.request(3, 0x12, 0x34, &[9, 8, 7], false, 1);
        assert_eq!(h.long.as_ref().unwrap().writes[0].len(), 20);
        assert_eq!(&h.long.as_ref().unwrap().writes[0][..7], &[0x11, 3, 0x12, 0x34, 9, 8, 7]);
        h.request(3, 0x12, 0x34, &[9], true, 1);
        assert_eq!(h.short.as_ref().unwrap().writes[0], vec![0x10, 3, 0x12, 0x34, 9, 0, 0]);
        // params beyond the message are dropped, not a panic
        h.request(1, 0, 0, &[1, 2, 3, 4, 5], true, 1);
        assert_eq!(h.short.as_ref().unwrap().writes[1].len(), 7);
    }

    #[test]
    fn short_only_and_long_only_endpoints() {
        let mut h = Hidpp::new(Some(Mock::new(vec![])), None);
        h.timeout_cap_ms = 1;
        h.request(1, 0, 0x1D, &[], false, 1); // long requested but absent: falls back to short
        assert_eq!(h.short.as_ref().unwrap().writes[0][0], 0x10);
        let mut h = Hidpp::new(None, Some(Mock::new(vec![])));
        h.timeout_cap_ms = 1;
        assert_eq!(h.request(1, 0x81, 0x0D, &[], true, 1), Reply::None); // forced short with no short
        assert!(h.long.as_ref().unwrap().writes.is_empty());
    }

    #[test]
    fn ignores_other_apps_replies_and_indices() {
        let mut h = hp(vec![]);
        let m = h.long.as_mut().unwrap();
        m.queue.push_back(vec![0x11, 2, 0x00, 0x1D, 9, 9, 9]); // other device index
        m.queue.push_back(vec![0x11, 1, 0x00, 0x1E, 8, 8, 8]); // other software id
        m.queue.push_back(vec![0x02, 1, 0x00, 0x1D, 7, 7, 7]); // not a HID++ report
        m.queue.push_back(vec![0x11, 1, 0x00]); // too short
        m.queue.push_back(vec![0x11, 1, 0x00, 0x1D, 1, 2, 3]);
        let r = h.request(1, 0x00, 0x1D, &[], false, 2000);
        assert_eq!(&match r { Reply::Ok(p) => p, _ => vec![] }[..3], &[1, 2, 3]);
    }

    #[test]
    fn error_replies_are_decoded_for_both_hidpp_versions() {
        let mut h = hp(vec![]);
        h.long.as_mut().unwrap().queue.push_back(vec![0x11, 1, 0xFF, 0x00, 0x1D, 0x05, 0]);
        assert_eq!(h.request(1, 0x00, 0x1D, &[], false, 2000), Reply::Err(5));
        h.short.as_mut().unwrap().queue.push_back(vec![0x10, 1, 0x8F, 0x81, 0x0D, 0x09, 0]);
        assert_eq!(h.request(1, 0x81, 0x0D, &[], true, 2000), Reply::Err(9));
        assert_eq!(h.request(1, 0x00, 0x1D, &[], false, 5), Reply::None);
    }

    #[test]
    fn unified_battery_percent_and_charging() {
        // feature table: 0x1004 -> index 6
        let d = dev20(&[(0x1004, 6)], &[((6, 1), vec![73, 0, 1])]);
        let r = run(vec![(0xFF, d)], 0xC52B, "MX Master 3");
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].percent, r[0].charging, r[0].key.as_str()), (Some(73), true, "logi:C52B:255"));
        assert_eq!((r[0].name.as_str(), r[0].via.as_str(), r[0].grace_sec), ("MX Master 3", "USB / Bluetooth", 0));
    }

    #[test]
    fn unified_battery_level_bits_when_soc_unknown() {
        for (level, pct, text) in [(8, 100, "Full"), (4, 60, "Good"), (2, 20, "Low"), (1, 5, "Critical")] {
            let d = dev20(&[(0x1004, 2)], &[((2, 1), vec![0, level, 0])]);
            let r = run(vec![(0xFF, d)], 0xC52B, "Dev");
            assert_eq!((r[0].percent, r[0].level_text.as_str(), r[0].charging), (Some(pct), text, false));
        }
        // statuses 1, 2, 3 charge; 0 and 4+ do not
        for (st, ch) in [(0, false), (1, true), (2, true), (3, true), (4, false)] {
            let d = dev20(&[(0x1004, 2)], &[((2, 1), vec![50, 0, st])]);
            assert_eq!(run(vec![(0xFF, d)], 1, "Dev")[0].charging, ch);
        }
    }

    #[test]
    fn battery_status_feature_1000() {
        let d = dev20(&[(0x1000, 4)], &[((4, 0), vec![55, 40, 2])]);
        let r = &run(vec![(0xFF, d)], 1, "Dev")[0];
        assert_eq!((r.percent, r.charging), (Some(55), true));
        // 0 % while status 3 (charge complete) is shown as full
        let d = dev20(&[(0x1000, 4)], &[((4, 0), vec![0, 0, 3])]);
        assert_eq!(run(vec![(0xFF, d)], 1, "Dev")[0].percent, Some(100));
        // out-of-range level: unknown percent but still a hit
        let d = dev20(&[(0x1000, 4)], &[((4, 0), vec![200, 0, 0])]);
        let r = run(vec![(0xFF, d)], 1, "Dev");
        assert_eq!((r.len(), r[0].percent), (1, None));
    }

    #[test]
    fn battery_voltage_feature_1001_uses_liion_table() {
        let d = dev20(&[(0x1001, 3)], &[((3, 0), vec![0x0F, 0xBA, 0x80])]); // 4026 mV, charging flag
        let r = &run(vec![(0xFF, d)], 1, "Dev")[0];
        assert_eq!((r.millivolts, r.charging), (4026, true));
        assert_eq!(r.percent, Some(84)); // between 4067 (90) and 3989 (80)
        assert_eq!(li_ion_percent(4300), 100);
        assert_eq!(li_ion_percent(3500), 0);
        assert_eq!(li_ion_percent(3000), 0);
        // 0 mV: no percentage
        let d = dev20(&[(0x1001, 3)], &[((3, 0), vec![0, 0, 0])]);
        assert_eq!(run(vec![(0xFF, d)], 1, "Dev")[0].percent, None);
    }

    #[test]
    fn adc_measurement_headset_tables() {
        assert_eq!(headset_percent(0x0A66, 4200), 100);
        assert_eq!(headset_percent(0x0A66, 3330), 0);
        assert_eq!(headset_percent(0x0AC4, 3817), 50);
        assert_eq!(headset_percent(0x0A5C, 3850), 60);
        assert_eq!(headset_percent(0x0AB5, 3150), 0);
        assert_eq!(headset_percent(0x0AA7, 3830), 50);
        assert_eq!(headset_percent(0x1234, 3850), 50); // default table
        assert_eq!(headset_percent(0x1234, 3500), 2); // interpolated between 3300 (0) and 3650 (5)
        let d = dev20(&[(0x1F20, 5)], &[((5, 0), vec![0x0F, 0x0A, 0x03])]); // 3850 mV, charging
        let r = &run(vec![(0xFF, d)], 0x1234, "G Headset")[0];
        assert_eq!((r.percent, r.charging, r.millivolts), (Some(50), true, 3850));
    }

    #[test]
    fn feature_priority_prefers_unified_battery() {
        let d = dev20(&[(0x1004, 6), (0x1000, 4)], &[((6, 1), vec![10, 0, 0]), ((4, 0), vec![90, 0, 0])]);
        assert_eq!(run(vec![(0xFF, d)], 1, "Dev")[0].percent, Some(10));
        // 0x1004 present but not answering usable data: falls through to 0x1000
        let d = dev20(&[(0x1004, 6), (0x1000, 4)], &[((4, 0), vec![90, 0, 0])]); // no reply for 0x1004's get_status
        assert_eq!(run(vec![(0xFF, d)], 1, "Dev")[0].percent, Some(90));
    }

    #[test]
    fn device_name_and_type_are_read_in_chunks() {
        let mut d = dev20(&[(0x0005, 1), (0x1004, 2)], &[((1, 0), vec![21]), ((1, 2), vec![3]), ((2, 1), vec![50, 0, 0])]);
        d.chunked.insert(1, b"Anywhere Mouse 3S XX".to_vec()); // 20 bytes served, 21 announced
        let r = &run(vec![(0xFF, d)], 1, "Raw product")[0];
        // reads 16 + 4 bytes, then the next chunk is empty -> stops without looping forever
        assert_eq!(r.name, "Logitech Anywhere Mouse 3S XX");
        assert_eq!(r.kind, DeviceKind::Mouse);
    }

    #[test]
    fn name_prefix_rules_and_type_table() {
        let mk = |n: &[u8]| {
            let mut d = dev20(&[(0x0005, 1), (0x1004, 2)], &[((1, 0), vec![n.len() as u8]), ((2, 1), vec![50, 0, 0])]);
            d.chunked.insert(1, n.to_vec());
            run(vec![(0xFF, d)], 1, "x").remove(0).name
        };
        assert_eq!(mk(b"MX Keys"), "MX Keys");
        assert_eq!(mk(b"G Pro X"), "G Pro X");
        assert_eq!(mk(b"Logitech Zone"), "Logitech Zone");
        assert_eq!(mk(b"Craft"), "Logitech Craft");
        for (t, k) in [(0, DeviceKind::Keyboard), (2, DeviceKind::Keyboard), (3, DeviceKind::Mouse), (5, DeviceKind::Mouse),
                       (8, DeviceKind::Headset), (0x0B, DeviceKind::Gamepad), (0x0C, DeviceKind::Gamepad), (1, DeviceKind::Other)] {
            assert_eq!(kind_from_type(t), k);
        }
    }

    #[test]
    fn receiver_enumerates_indices_one_to_six() {
        let d1 = dev20(&[(0x1004, 6)], &[((6, 1), vec![80, 0, 0])]);
        let d4 = dev20(&[(0x1000, 2)], &[((2, 0), vec![30, 0, 0])]);
        let r = run(vec![(1, d1), (4, d4)], 0xC52B, "USB Receiver");
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].key.as_str(), r[0].percent, r[0].grace_sec, r[0].via.as_str()), ("logi:C52B:1", Some(80), 1800, "Logitech receiver"));
        assert_eq!((r[1].key.as_str(), r[1].percent), ("logi:C52B:4", Some(30)));
        assert_eq!(r[0].name, "Logitech wireless device");
    }

    #[test]
    fn receiver_product_skips_direct_ping() {
        // even though index 0xFF would answer, a "Receiver" product is enumerated by index
        let d = dev20(&[(0x1004, 6)], &[((6, 1), vec![80, 0, 0])]);
        let r = run(vec![(0xFF, d.clone()), (2, d)], 1, "Logitech Bolt Receiver");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "logi:0001:2");
    }

    #[test]
    fn non_answering_non_receiver_falls_back_to_indices() {
        let d = dev20(&[(0x1004, 6)], &[((6, 1), vec![44, 0, 0])]);
        let r = run(vec![(3, d)], 1, "Some Device");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].key, "logi:0001:3");
    }

    #[test]
    fn hidpp10_battery_register_0d() {
        let dev = Dev { hidpp10: true, regs: [(0x0D, vec![65, 0, 0x22])].into_iter().collect(), ..Default::default() };
        let r = run(vec![(2, dev)], 0xC52B, "USB Receiver");
        assert_eq!((r.len(), r[0].percent, r[0].charging), (1, Some(65), true));
        let dev = Dev { hidpp10: true, regs: [(0x0D, vec![65, 0, 0x30])].into_iter().collect(), ..Default::default() };
        assert!(!run(vec![(2, dev)], 1, "USB Receiver")[0].charging);
    }

    #[test]
    fn hidpp10_level_register_07_fallback() {
        // register 0x0D unusable (0 %): falls back to the level register
        let regs = [(0x0D, vec![0, 0, 0]), (0x07, vec![5, 0x21])].into_iter().collect();
        let r = run(vec![(1, Dev { hidpp10: true, regs, ..Default::default() })], 1, "USB Receiver");
        assert_eq!((r[0].percent, r[0].level_text.as_str(), r[0].charging), (Some(50), "Good", true));
        for (lvl, pct, text) in [(1, 5, "Critical"), (3, 20, "Low"), (7, 100, "Full"), (9, 100, "Full")] {
            let regs = [(0x07, vec![lvl, 0])].into_iter().collect();
            let r = run(vec![(1, Dev { hidpp10: true, regs, ..Default::default() })], 1, "USB Receiver");
            assert_eq!((r[0].percent, r[0].level_text.as_str()), (Some(pct), text));
        }
        // level 0 -> nothing usable
        let regs = [(0x07, vec![0, 0])].into_iter().collect();
        assert!(run(vec![(1, Dev { hidpp10: true, regs, ..Default::default() })], 1, "USB Receiver").is_empty());
    }

    #[test]
    fn hidpp10_requests_use_short_reports() {
        let regs = [(0x0D, vec![65, 0, 0])].into_iter().collect();
        let mut h = hp(vec![(1, Dev { hidpp10: true, regs, ..Default::default() })]);
        let mut s = DeviceStatus::default();
        assert!(read_battery10(&mut h, 1, &mut s));
        let w = &h.short.as_ref().unwrap().writes;
        assert_eq!(w.last().unwrap(), &vec![0x10, 1, 0x81, 0x0D, 0, 0, 0]);
    }

    #[test]
    fn device_without_battery_feature_is_dropped() {
        let d = dev20(&[(0x0005, 1)], &[((1, 0), vec![3])]);
        assert!(run(vec![(0xFF, d)], 1, "Dev").is_empty());
        assert!(run(vec![], 1, "Dev").is_empty());
    }

    #[test]
    fn unknown_kind_falls_back_to_name_then_ff43_headset() {
        let d = dev20(&[(0x1004, 2)], &[((2, 1), vec![50, 0, 0])]);
        assert_eq!(run(vec![(0xFF, d.clone())], 1, "Logitech G PRO X Headset")[0].kind, DeviceKind::Headset);
        assert_eq!(run(vec![(0xFF, d.clone())], 1, "Widget")[0].kind, DeviceKind::Other);
        let r = poll_product(&mut hp(vec![(0xFF, d)]), 1, "Widget", true);
        assert_eq!(r[0].kind, DeviceKind::Headset);
    }
}

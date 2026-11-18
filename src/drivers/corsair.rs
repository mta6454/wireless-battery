//! Corsair wireless headsets, mice and keyboards (VID 1B1C).
//!  * Void / Void Pro / Void Elite / HS70 / HS80 family: output C9 64 -> [2] % (bit 7 = mic up), [4] status
//!    (0 offline, 1-2 ok, 4-5 charging). From github.com/Sapd/HeadsetControl.
//!  * "Bragi" devices (Virtuoso, Void V2, HS80 MAX, Slipstream mice/keyboards): wire bytes
//!    02 <target> 02 <property> = GET; reply [1] source, [2] 02, [3] status (0 ok), [4..7] value LE.
//!    Property 0F = battery (0..1000), 10 = charge state (1 charging, 3 full). Target 08 is the device
//!    or receiver itself, 09+ are devices paired to a receiver. GET is read-only; we never switch the
//!    device into software mode.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, guess_kind, hex4, kind_from_collections, HidColl, HidDev, HidIo};

const VOID_PIDS: [u16; 18] = [
    0x1B1C, 0x1B27, 0x0A14, 0x0A16, 0x0A17, 0x0A1D, 0x0A1A, 0x1B2A, 0x1B23, 0x1B29, 0x0A55, 0x0A51, 0x0A52, 0x0A38, 0x0A4F,
    0x0A2B, 0x0A75, 0x0A56,
];
const BRAGI_PIDS: [u16; 7] = [0x2A08, 0x2A02, 0x0A97, 0x0A64, 0x0A62, 0x0A3E, 0x0A3D];

/// Void family: one request, the first reply (at least 5 bytes) is the answer.
pub fn poll_void(d: &mut impl HidIo, pid: u16, product: &str) -> Option<DeviceStatus> {
    d.flush();
    if !d.write(&[0xC9, 0x64]) {
        return None;
    }
    let r = d.read(1000).filter(|r| r.len() >= 5)?;
    let st = r[4];
    let mut s = DeviceStatus {
        key: format!("corsair:{}", hex4(pid)),
        name: if product.is_empty() { "Corsair wireless headset".into() } else { product.to_string() },
        kind: DeviceKind::Headset,
        via: "2.4 GHz receiver".into(),
        connected: st != 0,
        charging: st == 4 || st == 5,
        ..Default::default()
    };
    if st == 1 || st == 2 || s.charging {
        s.percent = Some(clamp100((r[2] & 0x7F) as i32));
    }
    Some(s)
}

pub struct Bragi<'a, I: HidIo> {
    d: &'a mut I,
    numbered: bool,
}

impl<'a, I: HidIo> Bragi<'a, I> {
    /// `out_len` is the collection's output report length: 64 means numbered reports (the leading
    /// byte of the request is then the report id 02), otherwise a 00 report id is prepended.
    pub fn new(d: &'a mut I, out_len: u16) -> Self {
        Self { d, numbered: out_len == 64 }
    }

    /// GET `prop` of `target`: the 32-bit LE value, or None on timeout / error status.
    pub fn get(&mut self, target: u8, prop: u8, timeout_ms: u64) -> Option<u32> {
        let req: Vec<u8> = if self.numbered { vec![0x02, target, 0x02, prop] } else { vec![0x00, 0x02, target, 0x02, prop] };
        self.d.flush();
        if !self.d.write(&req) {
            return None;
        }
        let source = if target == 0x08 { 0x00 } else { 0x01 };
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
            if left <= 0 {
                return None;
            }
            let r = self.d.read(left).filter(|r| r.len() >= 8)?;
            if r[2] != 0x02 || r[1] != source {
                continue; // notification or another app's reply
            }
            if r[3] != 0x00 {
                return None;
            }
            return Some(u32::from_le_bytes([r[4], r[5], r[6], r[7]]));
        }
    }
}

/// One battery found by [`scan_bragi`].
#[derive(Debug, PartialEq, Eq)]
pub struct BragiBattery {
    pub target: u8,
    /// Raw level 0..=1000.
    pub level: u32,
    /// Raw charge state when the device answered the property.
    pub charge: Option<u32>,
}

/// Paired devices behind a receiver first, then the device itself (wired / Bluetooth).
pub fn scan_bragi(b: &mut Bragi<'_, impl HidIo>) -> Vec<BragiBattery> {
    let mut found = Vec::new();
    for target in [0x09u8, 0x0A, 0x0B, 0x08] {
        if target == 0x08 && !found.is_empty() {
            break;
        }
        let mut level = b.get(target, 0x0F, 300);
        let mut retry = 0;
        while matches!(level, Some(l) if l == 0 || l > 1000) && retry < 2 {
            level = b.get(target, 0x0F, 300);
            retry += 1;
        }
        let Some(level) = level.filter(|l| *l <= 1000) else { continue };
        found.push(BragiBattery { target, level, charge: b.get(target, 0x10, 300) });
    }
    found
}

pub fn bragi_status(pid: u16, product: &str, kind: DeviceKind, bat: &BragiBattery) -> DeviceStatus {
    let mut s = DeviceStatus {
        key: format!("corsair:{}:{}", hex4(pid), bat.target),
        name: if product.is_empty() { "Corsair wireless device".into() } else { product.to_string() },
        kind,
        via: if bat.target == 0x08 { "USB / Bluetooth" } else { "Corsair receiver" }.into(),
        percent: Some(clamp100(((bat.level + 5) / 10) as i32)),
        ..Default::default()
    };
    if let Some(c) = bat.charge {
        s.charging = c == 1 || c == 3;
    }
    s
}

fn void_collection(c: &HidColl) -> bool {
    (c.usage_page == 0xFFC5 || c.usage_page == 0xFF00) && c.out_len > 0
}

fn bragi_collection(c: &HidColl, known: bool) -> bool {
    let sizes = (c.out_len == 64 || c.out_len == 65) && (c.in_len == 64 || c.in_len == 65);
    // macOS never reports the interface number (-1): treat it as unknown.
    let iface4 = c.iface == 4 || c.iface < 0;
    sizes && (c.usage_page == 0xFF42 || (known && c.usage_page >= 0xFF00) || (known && iface4))
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x1B1C) {
        if VOID_PIDS.contains(&pid) {
            for c in ctx.find(vid, pid, void_collection) {
                let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
                if let Some(s) = poll_void(&mut d, c.pid, &c.product) {
                    ctx.add(s, vid, pid);
                    break;
                }
            }
            continue;
        }
        let known = BRAGI_PIDS.contains(&pid);
        for c in ctx.find(vid, pid, |c| bragi_collection(c, known)) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let found = scan_bragi(&mut Bragi::new(&mut d, c.out_len));
            if found.is_empty() {
                continue;
            }
            let mut kind = guess_kind(if c.product.is_empty() { "Corsair wireless device" } else { &c.product });
            if kind == DeviceKind::Other {
                kind = kind_from_collections(&ctx.colls, vid, pid);
            }
            for bat in &found {
                ctx.out.push(bragi_status(pid, &c.product, kind, bat));
            }
            ctx.claim(vid, pid);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;

    /// Answers each request through a closure: returns the reports to queue for that request.
    struct Mock<F: FnMut(&[u8]) -> Vec<Vec<u8>>> {
        respond: F,
        queue: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    fn mock<F: FnMut(&[u8]) -> Vec<Vec<u8>>>(respond: F) -> Mock<F> {
        Mock { respond, queue: vec![], sent: vec![] }
    }

    impl<F: FnMut(&[u8]) -> Vec<Vec<u8>>> FeatureIo for Mock<F> {
        fn set_feature(&mut self, _: &[u8]) -> bool {
            false
        }
        fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> {
            None
        }
    }

    impl<F: FnMut(&[u8]) -> Vec<Vec<u8>>> HidIo for Mock<F> {
        fn write(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            self.queue = (self.respond)(data);
            true
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            if self.queue.is_empty() { None } else { Some(self.queue.remove(0)) }
        }
        fn flush(&mut self) {
            self.queue.clear();
        }
    }

    fn void_reply(level: u8, status: u8) -> Vec<u8> {
        vec![0xC9, 0x64, level, 0, status, 0, 0, 0]
    }

    fn bragi_reply(source: u8, status: u8, value: u32) -> Vec<u8> {
        let v = value.to_le_bytes();
        vec![0, source, 0x02, status, v[0], v[1], v[2], v[3]]
    }

    #[test]
    fn void_states() {
        let mut m = mock(|_| vec![void_reply(0x80 | 77, 1)]); // mic-up bit is masked off
        let s = poll_void(&mut m, 0x0A14, "").unwrap();
        assert_eq!(m.sent[0], vec![0xC9, 0x64]);
        assert_eq!((s.percent, s.connected, s.charging, s.name.as_str()), (Some(77), true, false, "Corsair wireless headset"));
        assert_eq!(s.key, "corsair:0A14");
        let s = poll_void(&mut mock(|_| vec![void_reply(50, 5)]), 0x0A14, "HS70").unwrap();
        assert_eq!((s.percent, s.charging, s.name.as_str()), (Some(50), true, "HS70"));
        let s = poll_void(&mut mock(|_| vec![void_reply(50, 0)]), 0x0A14, "x").unwrap();
        assert!(!s.connected && s.percent.is_none());
        let s = poll_void(&mut mock(|_| vec![void_reply(50, 3)]), 0x0A14, "x").unwrap();
        assert!(s.connected && !s.charging && s.percent.is_none()); // unknown status: no level
        assert!(poll_void(&mut mock(|_| vec![vec![0xC9, 0x64, 1, 2]]), 0x0A14, "x").is_none());
        assert!(poll_void(&mut mock(|_| vec![]), 0x0A14, "x").is_none());
    }

    #[test]
    fn bragi_request_layout_depends_on_numbering() {
        let mut m = mock(|_| vec![bragi_reply(0, 0, 500)]);
        assert_eq!(Bragi::new(&mut m, 64).get(0x08, 0x0F, 300), Some(500));
        assert_eq!(m.sent[0], vec![0x02, 0x08, 0x02, 0x0F]);
        let mut m = mock(|_| vec![bragi_reply(0, 0, 500)]);
        assert_eq!(Bragi::new(&mut m, 65).get(0x08, 0x0F, 300), Some(500));
        assert_eq!(m.sent[0], vec![0x00, 0x02, 0x08, 0x02, 0x0F]);
    }

    #[test]
    fn bragi_get_filters_replies_and_decodes_le() {
        // notification (wrong [2]) and a reply from the wrong source are skipped
        let mut m = mock(|_| {
            vec![
                vec![0, 0, 0x01, 0, 0, 0, 0, 0],
                bragi_reply(0x00, 0, 111), // receiver itself, but we asked a paired device
                bragi_reply(0x01, 0, 0x0102_0304),
            ]
        });
        assert_eq!(Bragi::new(&mut m, 64).get(0x09, 0x0F, 300), Some(0x0102_0304));
        // non-zero status = failure, short reply = failure
        assert_eq!(Bragi::new(&mut mock(|_| vec![bragi_reply(0, 5, 1)]), 64).get(0x08, 0x0F, 300), None);
        assert_eq!(Bragi::new(&mut mock(|_| vec![vec![0, 0, 2, 0]]), 64).get(0x08, 0x0F, 300), None);
    }

    #[test]
    fn scan_prefers_paired_devices_and_skips_own_target() {
        let mut m = mock(|req| {
            // numbered layout: [02, target, 02, prop]
            match (req[1], req[3]) {
                (0x09, 0x0F) => vec![bragi_reply(0x01, 0, 850)],
                (0x09, 0x10) => vec![bragi_reply(0x01, 0, 1)],
                (0x0A, 0x0F) => vec![bragi_reply(0x01, 5, 0)], // no such device
                (0x0B, 0x0F) => vec![bragi_reply(0x01, 0, 304)],
                (0x0B, 0x10) => vec![], // no charge answer
                _ => vec![],
            }
        });
        let found = scan_bragi(&mut Bragi::new(&mut m, 64));
        assert_eq!(
            found,
            vec![
                BragiBattery { target: 0x09, level: 850, charge: Some(1) },
                BragiBattery { target: 0x0B, level: 304, charge: None },
            ]
        );
        assert!(m.sent.iter().all(|r| r[1] != 0x08)); // device itself never asked
    }

    #[test]
    fn scan_falls_back_to_device_itself() {
        let mut m = mock(|req| if req[1] == 0x08 && req[3] == 0x0F { vec![bragi_reply(0, 0, 1000)] } else { vec![] });
        let found = scan_bragi(&mut Bragi::new(&mut m, 64));
        assert_eq!(found, vec![BragiBattery { target: 0x08, level: 1000, charge: None }]);
    }

    #[test]
    fn scan_retries_zero_and_out_of_range_levels_twice() {
        let mut n = 0;
        let mut m = mock(|req| {
            if req[1] == 0x08 && req[3] == 0x0F {
                n += 1;
                return vec![bragi_reply(0, 0, if n == 1 { 0 } else { 640 })];
            }
            vec![]
        });
        let found = scan_bragi(&mut Bragi::new(&mut m, 64));
        assert_eq!(found[0].level, 640);
        // a level stuck at 0 is asked three times in total, then reported as 0 (like the original)
        let mut m = mock(|req| if req[1] == 0x08 && req[3] == 0x0F { vec![bragi_reply(0, 0, 0)] } else { vec![] });
        assert_eq!(scan_bragi(&mut Bragi::new(&mut m, 64))[0].level, 0);
        assert_eq!(m.sent.iter().filter(|r| r[1] == 0x08 && r[3] == 0x0F).count(), 3);
        // above 1000 is also invalid
        let mut m = mock(|req| if req[1] == 0x08 && req[3] == 0x0F { vec![bragi_reply(0, 0, 2000)] } else { vec![] });
        assert!(scan_bragi(&mut Bragi::new(&mut m, 64)).is_empty());
    }

    #[test]
    fn bragi_status_fields() {
        let s = bragi_status(0x0A62, "", DeviceKind::Headset, &BragiBattery { target: 0x09, level: 855, charge: Some(3) });
        assert_eq!(s.key, "corsair:0A62:9");
        assert_eq!((s.name.as_str(), s.via.as_str(), s.percent, s.charging), ("Corsair wireless device", "Corsair receiver", Some(86), true));
        let s = bragi_status(0x0A62, "Virtuoso", DeviceKind::Headset, &BragiBattery { target: 0x08, level: 1000, charge: Some(2) });
        assert_eq!((s.via.as_str(), s.percent, s.charging), ("USB / Bluetooth", Some(100), false));
        let s = bragi_status(0x0A62, "Virtuoso", DeviceKind::Headset, &BragiBattery { target: 0x08, level: 4, charge: None });
        assert_eq!((s.percent, s.charging), (Some(0), false));
    }

    #[test]
    fn collection_filters() {
        let mk = |page, iface, in_len, out_len| HidColl {
            path: Default::default(),
            vid: 0x1B1C,
            pid: 1,
            iface,
            usage_page: page,
            usage: 1,
            product: String::new(),
            in_len,
            out_len,
            feat_len: 0,
        };
        assert!(void_collection(&mk(0xFFC5, 3, 0, 33)) && void_collection(&mk(0xFF00, 3, 0, 33)));
        assert!(!void_collection(&mk(0xFF00, 3, 0, 0)) && !void_collection(&mk(0xFF42, 3, 0, 33)));
        assert!(bragi_collection(&mk(0xFF42, 1, 64, 64), false));
        assert!(!bragi_collection(&mk(0xFF42, 1, 64, 32), false)); // wrong size
        assert!(bragi_collection(&mk(0xFF01, 1, 65, 65), true));
        assert!(!bragi_collection(&mk(0xFF01, 1, 65, 65), false));
        assert!(bragi_collection(&mk(0x0001, 4, 64, 64), true));
        assert!(!bragi_collection(&mk(0x0001, 1, 64, 64), true));
    }

    #[test]
    fn pid_tables_do_not_overlap() {
        assert!(BRAGI_PIDS.iter().all(|p| !VOID_PIDS.contains(p)));
    }
}

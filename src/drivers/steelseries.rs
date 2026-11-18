//! SteelSeries wireless headsets and mice (VID 1038).
//! Headset protocols follow github.com/Sapd/HeadsetControl, mouse protocols github.com/flozz/rivalcfg.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4, map_to_100, HidColl, HidDev, HidIo};

fn write_read(d: &mut impl HidIo, req: &[u8], timeout_ms: i32) -> Option<Vec<u8>> {
    d.flush();
    if !d.write(req) {
        return None;
    }
    d.read(timeout_ms)
}

fn headset(pid: u16, name: &str) -> DeviceStatus {
    DeviceStatus {
        key: format!("steelseries:{}", hex4(pid)),
        name: name.to_string(),
        kind: DeviceKind::Headset,
        via: "2.4 GHz receiver".into(),
        ..Default::default()
    }
}

// ---- headsets ----------------------------------------------------------------------------------

/// Arctis 1 / 1 Xbox / 7X: 06 12 -> [2]==01 offline, [3] %.
pub fn arctis1(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x06, 0x12], 1000).filter(|r| r.len() >= 4)?;
    let mut s = headset(pid, "SteelSeries Arctis 1 Wireless");
    s.connected = r[2] != 0x01;
    if s.connected {
        s.percent = Some(clamp100(r[3] as i32));
    }
    Some(s)
}

/// Arctis 7 / 7 2019 / Pro 2019: 06 18 -> [2] %.
pub fn arctis7(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x06, 0x18], 1000).filter(|r| r.len() >= 3)?;
    let mut s = headset(pid, if pid == 0x1252 { "SteelSeries Arctis Pro" } else { "SteelSeries Arctis 7" });
    s.percent = Some(clamp100(r[2] as i32));
    s.connected = r[2] != 0;
    Some(s)
}

/// Arctis 9: 00 20 -> [3] raw (0x64..0x9A), [4]==01 charging.
pub fn arctis9(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x00, 0x20], 1000).filter(|r| r.len() >= 5)?;
    let mut s = headset(pid, "SteelSeries Arctis 9");
    s.percent = Some(map_to_100(r[3] as i32, 0x64, 0x9A));
    s.charging = r[4] == 0x01;
    Some(s)
}

/// Arctis Pro Wireless: 41 AA -> [0]==02 offline; 40 AA -> [0] level 0..4.
pub fn arctis_pro_wireless(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x41, 0xAA], 1000).filter(|r| !r.is_empty())?;
    let mut s = headset(pid, "SteelSeries Arctis Pro Wireless");
    s.connected = r[0] != 0x02;
    if s.connected {
        if let Some(r) = write_read(d, &[0x40, 0xAA], 1000).filter(|r| !r.is_empty()) {
            s.percent = Some(map_to_100(r[0] as i32, 0, 4));
        }
    }
    Some(s)
}

/// Arctis Nova Pro Wireless: 06 B0 -> [15] status (01 offline, 02 charging), [6] level 0..8.
pub fn nova_pro_wireless(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x06, 0xB0], 1000).filter(|r| r.len() >= 16)?;
    let mut s = headset(pid, "SteelSeries Arctis Nova Pro Wireless");
    s.connected = r[15] != 0x01;
    s.charging = r[15] == 0x02;
    if s.connected {
        s.percent = Some(map_to_100(r[6] as i32, 0, 8));
    }
    Some(s)
}

/// Nova family: 00 B0 -> status block; layout depends on the model.
pub fn nova_family(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x00, 0xB0], 1000).filter(|r| r.len() >= 7)?;
    let mut s;
    if matches!(pid, 0x220E | 0x2212 | 0x2216 | 0x2236) {
        // Arctis 7+
        s = headset(pid, "SteelSeries Arctis 7+");
        s.connected = r[1] != 0x01;
        s.percent = Some(map_to_100(r[2] as i32, 0, 4));
        s.charging = r[3] == 0x01;
    } else if matches!(pid, 0x2269 | 0x226D | 0x2232 | 0x2253) {
        // Nova 3P Wireless, Nova 5
        let name = if matches!(pid, 0x2232 | 0x2253) { "SteelSeries Arctis Nova 5" } else { "SteelSeries Arctis Nova 3P Wireless" };
        s = headset(pid, name);
        s.connected = r[1] != 0x02;
        s.percent = Some(clamp100(r[3] as i32));
        s.charging = r[4] == 0x01;
    } else if pid == 0x230A {
        // Arctis GameBuds: lowest of the buds that are out of the case
        s = headset(pid, "SteelSeries Arctis GameBuds");
        let mut lvl: i32 = -1;
        if r[3] == 0x03 {
            lvl = r[5] as i32;
        }
        if r[4] == 0x03 {
            lvl = if lvl < 0 { r[6] as i32 } else { lvl.min(r[6] as i32) };
        }
        s.connected = lvl >= 0;
        s.percent = if lvl >= 0 { Some(clamp100(lvl)) } else { None };
    } else {
        // Arctis Nova 7 / 7X / 7P and variants
        let discrete = matches!(pid, 0x2202 | 0x2206 | 0x220A | 0x223A | 0x227A | 0x22A4);
        s = headset(pid, "SteelSeries Arctis Nova 7");
        s.connected = r[3] != 0x00;
        s.charging = r[3] == 0x01 || r[3] == 0x02;
        s.percent = Some(if discrete { map_to_100(r[2] as i32, 0, 4) } else { clamp100(r[2] as i32) });
    }
    if !s.connected {
        s.percent = None;
    }
    Some(s)
}

// ---- mice --------------------------------------------------------------------------------------

fn mouse(pid: u16, name: &str, wired: bool) -> DeviceStatus {
    DeviceStatus {
        key: format!("steelseries:{}", hex4(pid)),
        name: name.to_string(),
        kind: DeviceKind::Mouse,
        via: if wired { "USB cable" } else { "2.4 GHz receiver" }.into(),
        ..Default::default()
    }
}

/// Aerox 3/5/9 Wireless, Prime Wireless: 92 (wired) / D2 (wireless) -> [1] = (level/5 + 1) | charging<<7.
pub fn aerox(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let wired = matches!(pid, 0x183A | 0x1854 | 0x185A | 0x1842);
    let r = write_read(d, &[0x00, if wired { 0x92 } else { 0xD2 }], 300).filter(|r| r.len() >= 2)?;
    let name = match pid {
        0x1838 | 0x183A => "SteelSeries Aerox 3 Wireless",
        0x1852 | 0x1854 => "SteelSeries Aerox 5 Wireless",
        0x1858 | 0x185A => "SteelSeries Aerox 9 Wireless",
        _ => "SteelSeries Prime Wireless",
    };
    let mut s = mouse(pid, name, wired);
    let raw = (r[1] & 0x7F) as i32;
    s.charging = (r[1] & 0x80) != 0;
    if raw == 0 {
        s.connected = false; // mouse off / out of range
    } else {
        s.percent = Some(clamp100((raw - 1) * 5));
    }
    Some(s)
}

/// Rival 3 Wireless (+ Gen 2), Rival 650: AA 01 -> [0] %, [2] charging.
pub fn rival(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = write_read(d, &[0x00, 0xAA, 0x01], 300).filter(|r| r.len() >= 3)?;
    let name = match pid {
        0x172B => "SteelSeries Rival 650 Wireless",
        0x1872 => "SteelSeries Rival 3 Wireless Gen 2",
        _ => "SteelSeries Rival 3 Wireless",
    };
    let mut s = mouse(pid, name, pid == 0x172B);
    s.percent = Some(clamp100(r[0] as i32));
    s.charging = r[2] != 0;
    Some(s)
}

/// Rival Pro (UWB, hot-swappable batteries): 92 -> 92 00 <mouse %> <flag> <spare battery %>.
/// From the Rival Pro support pull request on rivalcfg (github.com/flozz/rivalcfg/pull/294).
pub fn rival_pro(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    d.flush();
    if !d.write(&[0x00, 0x92]) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(500);
    let r = loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = d.read(left).filter(|r| r.len() >= 3)?;
        if r[0] == 0x92 && r[1] == 0x00 {
            break r;
        }
    };
    if r[2] > 100 {
        return None;
    }
    let mut s = mouse(pid, "SteelSeries Rival Pro", false);
    s.via = "UWB base station".into();
    s.percent = Some(r[2]);
    Some(s)
}

// ---- model table -------------------------------------------------------------------------------

type PollFn = fn(&mut HidDev, u16) -> Option<DeviceStatus>;

struct Model {
    pids: &'static [u16],
    accept: fn(&HidColl) -> bool,
    poll: PollFn,
}

fn page_ff43(c: &HidColl) -> bool {
    c.usage_page == 0xFF43 && c.usage == 0x0202
}
fn page_ffc0(c: &HidColl) -> bool {
    c.usage_page == 0xFFC0 && c.usage == 0x0001
}
/// macOS never reports the interface number (-1): accept it as "unknown".
fn iface_n(c: &HidColl, n: i32) -> bool {
    (c.iface == n || c.iface < 0) && c.out_len > 0
}
fn iface0(c: &HidColl) -> bool {
    iface_n(c, 0)
}
fn iface2(c: &HidColl) -> bool {
    iface_n(c, 2)
}
fn iface3(c: &HidColl) -> bool {
    iface_n(c, 3)
}
fn iface4(c: &HidColl) -> bool {
    iface_n(c, 4)
}

const MODELS: [Model; 10] = [
    Model { pids: &[0x12B3, 0x12B6, 0x12D7], accept: page_ff43, poll: arctis1 },
    Model { pids: &[0x1260, 0x12AD, 0x1252], accept: page_ff43, poll: arctis7 },
    Model { pids: &[0x12C2], accept: iface0, poll: arctis9 },
    Model { pids: &[0x1290], accept: iface0, poll: arctis_pro_wireless },
    Model { pids: &[0x12E0], accept: iface4, poll: nova_pro_wireless },
    Model {
        pids: &[
            0x220E, 0x2212, 0x2216, 0x2236, 0x2269, 0x226D, 0x2232, 0x2253, 0x230A, 0x2202, 0x22A1, 0x227E, 0x2206, 0x2258,
            0x229E, 0x22AD, 0x223A, 0x22A9, 0x227A, 0x22A4, 0x22A5, 0x220A, 0x22A7,
        ],
        accept: page_ffc0,
        poll: nova_family,
    },
    Model {
        pids: &[0x1838, 0x183A, 0x1852, 0x1854, 0x1858, 0x185A, 0x1840, 0x1842],
        accept: iface3,
        poll: aerox,
    },
    Model { pids: &[0x1830, 0x1872], accept: iface3, poll: rival },
    Model { pids: &[0x172B], accept: iface0, poll: rival },
    Model { pids: &[0x1880], accept: iface2, poll: rival_pro },
];

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x1038) {
        let Some(m) = MODELS.iter().find(|m| m.pids.contains(&pid)) else { continue };
        for c in ctx.find(vid, pid, m.accept) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            if let Some(s) = (m.poll)(&mut d, pid) {
                ctx.add(s, vid, pid);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;

    /// Scripted device: every write queues the next scripted reply (or none), reads pop the queue.
    struct Mock {
        script: Vec<Option<Vec<u8>>>,
        queue: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl Mock {
        fn new(script: Vec<Option<Vec<u8>>>) -> Self {
            Self { script, queue: vec![], sent: vec![] }
        }
        fn ok(replies: Vec<Vec<u8>>) -> Self {
            Self::new(replies.into_iter().map(Some).collect())
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
        fn write(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            if !self.script.is_empty() {
                if let Some(r) = self.script.remove(0) {
                    self.queue.push(r);
                }
            }
            true
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            if self.queue.is_empty() { None } else { Some(self.queue.remove(0)) }
        }
        fn flush(&mut self) {
            self.queue.clear();
        }
    }

    fn buf(n: usize, set: &[(usize, u8)]) -> Vec<u8> {
        let mut v = vec![0u8; n];
        for &(i, b) in set {
            v[i] = b;
        }
        v
    }

    #[test]
    fn arctis1_online_offline_and_short_reply() {
        let mut m = Mock::ok(vec![buf(8, &[(2, 0), (3, 70)])]);
        let s = arctis1(&mut m, 0x12B3).unwrap();
        assert_eq!(m.sent[0], vec![0x06, 0x12]);
        assert_eq!((s.percent, s.connected), (Some(70), true));
        assert_eq!(s.key, "steelseries:12B3");
        assert_eq!(s.via, "2.4 GHz receiver");
        let s = arctis1(&mut Mock::ok(vec![buf(8, &[(2, 1), (3, 70)])]), 0x12B3).unwrap();
        assert!(!s.connected && s.percent.is_none());
        assert!(arctis1(&mut Mock::ok(vec![vec![0; 3]]), 0x12B3).is_none());
        assert!(arctis1(&mut Mock::new(vec![None]), 0x12B3).is_none());
    }

    #[test]
    fn arctis7_names_and_zero_is_disconnected() {
        let s = arctis7(&mut Mock::ok(vec![buf(8, &[(2, 55)])]), 0x1252).unwrap();
        assert_eq!((s.name.as_str(), s.percent, s.connected), ("SteelSeries Arctis Pro", Some(55), true));
        let s = arctis7(&mut Mock::ok(vec![buf(8, &[(2, 0)])]), 0x1260).unwrap();
        assert_eq!((s.name.as_str(), s.connected), ("SteelSeries Arctis 7", false));
    }

    #[test]
    fn arctis9_scales_raw_range_and_charging() {
        let mut m = Mock::ok(vec![buf(8, &[(3, 0x9A), (4, 1)])]);
        let s = arctis9(&mut m, 0x12C2).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0x20]);
        assert_eq!((s.percent, s.charging), (Some(100), true));
        let s = arctis9(&mut Mock::ok(vec![buf(8, &[(3, 0x64)])]), 0x12C2).unwrap();
        assert_eq!((s.percent, s.charging), (Some(0), false));
    }

    #[test]
    fn arctis_pro_wireless_two_step() {
        let mut m = Mock::ok(vec![buf(2, &[(0, 0)]), buf(2, &[(0, 2)])]);
        let s = arctis_pro_wireless(&mut m, 0x1290).unwrap();
        assert_eq!(m.sent, vec![vec![0x41, 0xAA], vec![0x40, 0xAA]]);
        assert_eq!(s.percent, Some(50));
        let mut m = Mock::ok(vec![buf(2, &[(0, 2)])]);
        let s = arctis_pro_wireless(&mut m, 0x1290).unwrap();
        assert!(!s.connected && s.percent.is_none() && m.sent.len() == 1);
        // second query unanswered: still reported, without a level
        let s = arctis_pro_wireless(&mut Mock::new(vec![Some(buf(2, &[])), None]), 0x1290).unwrap();
        assert!(s.connected && s.percent.is_none());
    }

    #[test]
    fn nova_pro_wireless_status_byte() {
        let s = nova_pro_wireless(&mut Mock::ok(vec![buf(16, &[(15, 2), (6, 4)])]), 0x12E0).unwrap();
        assert_eq!((s.connected, s.charging, s.percent), (true, true, Some(50)));
        let s = nova_pro_wireless(&mut Mock::ok(vec![buf(16, &[(15, 1), (6, 4)])]), 0x12E0).unwrap();
        assert!(!s.connected && s.percent.is_none());
        assert!(nova_pro_wireless(&mut Mock::ok(vec![buf(15, &[])]), 0x12E0).is_none());
    }

    #[test]
    fn nova_family_layouts() {
        // Arctis 7+: discrete level, charging at [3]
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(2, 3), (3, 1)])]), 0x220E).unwrap();
        assert_eq!((s.name.as_str(), s.percent, s.charging, s.connected), ("SteelSeries Arctis 7+", Some(75), true, true));
        // Nova 3P: [1]==2 offline; Nova 5 name
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(1, 0), (3, 88), (4, 1)])]), 0x2269).unwrap();
        assert_eq!((s.name.as_str(), s.percent, s.charging), ("SteelSeries Arctis Nova 3P Wireless", Some(88), true));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(1, 2), (3, 88)])]), 0x2232).unwrap();
        assert_eq!((s.name.as_str(), s.connected, s.percent), ("SteelSeries Arctis Nova 5", false, None));
        // Nova 7 continuous vs discrete, charging states 1/2, offline 0
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(2, 66), (3, 2)])]), 0x2200).unwrap();
        assert_eq!((s.name.as_str(), s.percent, s.charging), ("SteelSeries Arctis Nova 7", Some(66), true));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(2, 2), (3, 3)])]), 0x2202).unwrap();
        assert_eq!((s.percent, s.charging), (Some(50), false));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(2, 2), (3, 0)])]), 0x2202).unwrap();
        assert!(!s.connected && s.percent.is_none());
        assert!(nova_family(&mut Mock::ok(vec![buf(6, &[])]), 0x2202).is_none());
    }

    #[test]
    fn gamebuds_uses_lowest_bud_outside_case() {
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(3, 3), (4, 3), (5, 80), (6, 40)])]), 0x230A).unwrap();
        assert_eq!((s.connected, s.percent), (true, Some(40)));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(3, 0), (4, 3), (5, 80), (6, 60)])]), 0x230A).unwrap();
        assert_eq!(s.percent, Some(60));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(3, 3), (4, 0), (5, 77)])]), 0x230A).unwrap();
        assert_eq!(s.percent, Some(77));
        let s = nova_family(&mut Mock::ok(vec![buf(8, &[(5, 80), (6, 40)])]), 0x230A).unwrap();
        assert!(!s.connected && s.percent.is_none());
    }

    #[test]
    fn aerox_wired_and_wireless_requests() {
        let mut m = Mock::ok(vec![buf(4, &[(1, 0x80 | 5)])]);
        let s = aerox(&mut m, 0x183A).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0x92]);
        assert_eq!((s.percent, s.charging, s.via.as_str(), s.name.as_str()), (Some(20), true, "USB cable", "SteelSeries Aerox 3 Wireless"));
        let mut m = Mock::ok(vec![buf(4, &[(1, 21)])]);
        let s = aerox(&mut m, 0x1858).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0xD2]);
        assert_eq!((s.percent, s.charging, s.via.as_str(), s.name.as_str()), (Some(100), false, "2.4 GHz receiver", "SteelSeries Aerox 9 Wireless"));
        let s = aerox(&mut Mock::ok(vec![buf(4, &[(1, 0)])]), 0x1840).unwrap();
        assert!(!s.connected && s.percent.is_none());
        assert_eq!(s.name, "SteelSeries Prime Wireless");
    }

    #[test]
    fn rival_variants() {
        let mut m = Mock::ok(vec![buf(4, &[(0, 61), (2, 1)])]);
        let s = rival(&mut m, 0x172B).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0xAA, 0x01]);
        assert_eq!((s.name.as_str(), s.via.as_str(), s.percent, s.charging), ("SteelSeries Rival 650 Wireless", "USB cable", Some(61), true));
        let s = rival(&mut Mock::ok(vec![buf(4, &[(0, 150)])]), 0x1872).unwrap();
        assert_eq!((s.name.as_str(), s.via.as_str(), s.percent), ("SteelSeries Rival 3 Wireless Gen 2", "2.4 GHz receiver", Some(100)));
        assert_eq!(rival(&mut Mock::ok(vec![buf(4, &[])]), 0x1830).unwrap().name, "SteelSeries Rival 3 Wireless");
    }

    #[test]
    fn rival_pro_reply_and_rejects() {
        struct Burst(Vec<Vec<u8>>, Vec<Vec<u8>>);
        impl FeatureIo for Burst {
            fn set_feature(&mut self, _: &[u8]) -> bool { false }
            fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> { None }
        }
        impl HidIo for Burst {
            fn write(&mut self, _: &[u8]) -> bool {
                self.1 = self.0.clone();
                true
            }
            fn read(&mut self, _: i32) -> Option<Vec<u8>> {
                if self.1.is_empty() { None } else { Some(self.1.remove(0)) }
            }
            fn flush(&mut self) {
                self.1.clear();
            }
        }
        let mut b = Burst(vec![buf(8, &[(0, 0x55)]), buf(8, &[(0, 0x92), (1, 0), (2, 73)])], vec![]);
        let s = rival_pro(&mut b, 0x1880).unwrap();
        assert_eq!((s.percent, s.via.as_str(), s.name.as_str()), (Some(73), "UWB base station", "SteelSeries Rival Pro"));
        let mut b = Burst(vec![buf(8, &[(0, 0x92), (1, 0), (2, 101)])], vec![]);
        assert!(rival_pro(&mut b, 0x1880).is_none());
        let mut b = Burst(vec![buf(2, &[(0, 0x92)])], vec![]); // too short
        assert!(rival_pro(&mut b, 0x1880).is_none());
        assert!(rival_pro(&mut Burst(vec![], vec![]), 0x1880).is_none());
    }

    #[test]
    fn model_table_is_unambiguous() {
        let mut seen = std::collections::HashSet::new();
        for m in &MODELS {
            for p in m.pids {
                assert!(seen.insert(*p), "duplicate pid {p:04X}");
            }
        }
        assert_eq!(seen.len(), 3 + 3 + 1 + 1 + 1 + 23 + 8 + 2 + 1 + 1);
    }

    #[test]
    fn interface_filter_accepts_unknown_iface_and_requires_output() {
        let mk = |iface, out_len| HidColl {
            path: Default::default(),
            vid: 0x1038,
            pid: 0x1872,
            iface,
            usage_page: 0xFF00,
            usage: 1,
            product: String::new(),
            in_len: 0,
            out_len,
            feat_len: 0,
        };
        assert!(iface3(&mk(3, 65)) && iface3(&mk(-1, 65)));
        assert!(!iface3(&mk(2, 65)) && !iface3(&mk(3, 0)));
    }
}

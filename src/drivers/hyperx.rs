//! HyperX wireless headsets (HP VID 03F0, older Kingston VID 0951).
//! Protocols: Cloud III Wireless verified on hardware (03F0:0C9D) and github.com/Medprj/HyperXWM;
//! the others follow github.com/Sapd/HeadsetControl.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4, HidColl, HidDev, HidIo};

/// Writes `req` and returns the first input report accepted by `matches` within `timeout_ms`.
fn query(d: &mut impl HidIo, req: &[u8], timeout_ms: u64, matches: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
    if !d.write(req) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = d.read(left)?;
        if matches(&r) {
            return Some(r);
        }
    }
}

/// How long a headset stays listed (as "last known") after the dongle stops answering.
const GRACE_SEC: u32 = 180;

fn base(pid: u16, name: &str) -> DeviceStatus {
    DeviceStatus {
        key: format!("hyperx:{}", hex4(pid)),
        name: name.to_string(),
        kind: DeviceKind::Headset,
        via: "2.4 GHz receiver".into(),
        // A busy dongle (audio streaming) sometimes misses a reply: keep the last reading for a while
        // instead of dropping the headset from the list for one failed poll.
        grace_sec: GRACE_SEC,
        ..Default::default()
    }
}

/// Cloud III Wireless: report 66 <cmd>; 82 -> [2] connected, 8A -> [2] charging, 89 -> [2..3] mV, [4] %.
pub fn cloud3(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8| query(d, &[0x66, cmd], 800, |r| r.len() >= 5 && r[0] == 0x66 && r[1] == cmd);
    let r = q(0x82)?;
    let name = if pid == 0x069F || pid == 0x0995 { "HyperX Cloud II Core Wireless" } else { "HyperX Cloud III Wireless" };
    let mut s = base(pid, name);
    s.connected = r[2] != 0;
    if s.connected {
        if let Some(r) = q(0x8A) {
            s.charging = r[2] == 1;
        }
        if let Some(r) = q(0x89) {
            if r[4] <= 100 {
                s.percent = Some(r[4]);
                s.millivolts = ((r[2] as u32) << 8) | r[3] as u32;
            }
        }
    }
    Some(s)
}

/// Cloud II Wireless (HP): report 06 FF BB <cmd>; 02 -> [7] %, 03 -> [4] charging.
pub fn cloud2(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8| {
        query(d, &[0x06, 0xFF, 0xBB, cmd], 1000, |r| r.len() >= 8 && r[0] == 0x06 && r[1] == 0xFF && r[2] == 0xBB && r[3] == cmd)
    };
    let r = q(0x02).filter(|r| r[7] <= 100)?;
    let mut s = base(pid, "HyperX Cloud II Wireless");
    s.percent = Some(r[7]);
    s.millivolts = ((r[5] as u32) << 8) | r[6] as u32;
    if let Some(r) = q(0x03) {
        s.charging = r[4] == 1;
    }
    Some(s)
}

/// Cloud II Wireless (Kingston 0951:1718): fixed 62-byte frame, reply 0B 00 BB <cmd>.
pub fn cloud2_kingston(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8| {
        let req = [0x06, 0x00, 0x02, 0x00, 0x9A, 0x00, 0x00, 0x68, 0x4A, 0x8E, 0x0A, 0x00, 0x00, 0x00, 0xBB, cmd, 0x00];
        query(d, &req, 1000, |r| r.len() >= 8 && r[0] == 0x0B && r[2] == 0xBB && r[3] == cmd)
    };
    let r = q(0x02).filter(|r| r[7] <= 100)?;
    let mut s = base(pid, "HyperX Cloud II Wireless");
    s.percent = Some(r[7]);
    if let Some(r) = q(0x03) {
        s.charging = r[4] == 1;
    }
    Some(s)
}

/// Cloud Alpha Wireless: 21 BB <cmd>; 03 -> [3]==1 offline, 0C -> [3]==1 charging, 0B -> [3] %.
pub fn cloud_alpha(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8| query(d, &[0x21, 0xBB, cmd], 1000, |r| r.len() >= 4 && r[0] == 0x21 && r[2] == cmd);
    let r = q(0x03)?;
    let mut s = base(pid, "HyperX Cloud Alpha Wireless");
    s.connected = r[3] != 0x01;
    if !s.connected {
        return Some(s);
    }
    if let Some(r) = q(0x0C) {
        s.charging = r[3] == 0x01;
    }
    if let Some(r) = q(0x0B) {
        if r[3] <= 100 {
            s.percent = Some(r[3]);
        }
    }
    Some(s)
}

/// Cloud Alpha 2 Wireless: [cmd][sub]; reply [cmd+1][sub]...; 52 01 -> [2] connected, 50 02 -> [2] %, [3] charging.
pub fn cloud_alpha2(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8, sub: u8| {
        query(d, &[cmd, sub], 1000, |r| r.len() >= 8 && r[0] as u16 == cmd as u16 + 1 && r[1] == sub)
    };
    let r = q(0x52, 0x01)?;
    let mut s = base(pid, "HyperX Cloud Alpha 2 Wireless");
    s.connected = r[2] != 0;
    if s.connected {
        if let Some(r) = q(0x50, 0x02) {
            if r[2] <= 100 {
                s.percent = Some(r[2]);
                s.charging = r[3] == 0x01;
                s.millivolts = r[6] as u32 | ((r[7] as u32) << 8);
            }
        }
    }
    Some(s)
}

/// Battery percent from the Cloud Flight's millivolt reading (None when above 0x100B = charging).
pub fn flight_percent(mv: i32) -> Option<u8> {
    if mv > 0x100B {
        None
    } else if mv <= 3648 {
        Some(clamp100((0.00125 * mv as f64) as i32))
    } else if mv > 3975 {
        Some(100)
    } else {
        let v = mv as f64;
        Some(clamp100(
            (0.00000002547505 * v.powi(4) - 0.0003900299 * v.powi(3) + 2.238321 * v * v - 5706.256 * v + 5452299.0).round() as i32,
        ))
    }
}

/// Cloud Flight: 21 FF 05 -> [3..4] mV (above 0x100B means charging).
pub fn cloud_flight(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let r = query(d, &[0x21, 0xFF, 0x05], 1000, |r| r.len() >= 5)?;
    let mv = ((r[3] as i32) << 8) | r[4] as i32;
    let mut s = base(pid, "HyperX Cloud Flight");
    s.millivolts = mv as u32;
    if mv > 0x100B {
        s.charging = true;
    }
    s.percent = flight_percent(mv);
    Some(s)
}

/// Cloud III S Wireless: report 0C [02 03 01 00 <cmd>]; reply [5] = cmd, [6] = value (FF = invalid).
///   02 -> [6] == 2 connected, 06 -> [6] %, 48 -> [6] 0 discharging / 1 charging / 2 full.
/// From github.com/LennardKittner/HyperHeadset (cloud_iii_s_wireless.rs).
pub fn cloud3s(d: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    let mut q = |cmd: u8| {
        query(d, &[0x0C, 0x02, 0x03, 0x01, 0x00, cmd], 1000, |r| r.len() >= 7 && r[0] == 0x0C && r[5] == cmd && r[6] != 0xFF)
    };
    let r = q(0x02)?;
    let mut s = base(pid, "HyperX Cloud III S Wireless");
    s.connected = r[6] == 2;
    if !s.connected {
        return Some(s);
    }
    if let Some(r) = q(0x06) {
        if r[6] <= 100 {
            s.percent = Some(r[6]);
        }
    }
    if let Some(r) = q(0x48) {
        s.charging = r[6] == 1 || r[6] == 2;
    }
    Some(s)
}

type PollFn = fn(&mut HidDev, u16) -> Option<DeviceStatus>;

struct Model {
    vid: u16,
    pids: &'static [u16],
    /// Which collection carries the protocol.
    accept: fn(&HidColl) -> bool,
    poll: PollFn,
}

const MODELS: [Model; 9] = [
    Model { vid: 0x03F0, pids: &[0x05B7, 0x0C9D], accept: |c| c.out_len == 62 && c.in_len == 62, poll: cloud3 },
    // Cloud II Core Wireless: same 66 <cmd> protocol as Cloud III Wireless, 64-byte reports (HyperHeadset).
    Model { vid: 0x03F0, pids: &[0x069F, 0x0995], accept: |c| c.usage_page >= 0xFF00 && c.out_len >= 62 && c.in_len >= 62, poll: cloud3 },
    Model { vid: 0x03F0, pids: &[0x06BE, 0x02CC], accept: |c| c.usage_page >= 0xFF00 && c.out_len >= 7, poll: cloud3s },
    Model { vid: 0x03F0, pids: &[0x0696, 0x018B, 0x0D93], accept: |c| c.usage_page == 0xFF90 && c.usage == 0x0303, poll: cloud2 },
    Model { vid: 0x0951, pids: &[0x1718], accept: |c| c.out_len == 62, poll: cloud2_kingston },
    Model { vid: 0x03F0, pids: &[0x098D], accept: |c| c.usage_page >= 0xFF00 && c.out_len >= 3, poll: cloud_alpha },
    Model { vid: 0x03F0, pids: &[0x08BE], accept: |c| c.usage_page == 0xFF13 && c.usage == 0xFF00, poll: cloud_alpha2 },
    Model { vid: 0x0951, pids: &[0x16C4, 0x1723, 0x1749], accept: |c| c.usage_page >= 0xFF00 && c.out_len >= 3, poll: cloud_flight },
    Model { vid: 0x03F0, pids: &[0x0E90], accept: |c| c.usage_page >= 0xFF00 && c.out_len >= 3, poll: cloud_flight }, // Cloud Flight (HP)
];

pub fn poll(ctx: &mut PollContext) {
    for m in &MODELS {
        for &pid in m.pids {
            if ctx.is_claimed(m.vid, pid) {
                continue;
            }
            for c in ctx.find(m.vid, pid, m.accept) {
                let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
                // One retry: the dongle can miss a reply while it is busy streaming audio.
                if let Some(s) = (m.poll)(&mut d, pid).or_else(|| (m.poll)(&mut d, pid)) {
                    ctx.add(s, m.vid, pid);
                    break;
                }
            }
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

    fn buf(n: usize, set: &[(usize, u8)]) -> Vec<u8> {
        let mut v = vec![0u8; n];
        for &(i, b) in set {
            v[i] = b;
        }
        v
    }

    #[test]
    fn cloud3_full_exchange() {
        let mut m = mock(|req| match req[1] {
            0x82 => vec![buf(62, &[(0, 0x66), (1, 0x82), (2, 1)])],
            0x8A => vec![buf(62, &[(0, 0x66), (1, 0x8A), (2, 1)])],
            0x89 => vec![buf(62, &[(0, 0x66), (1, 0x89), (2, 0x0F), (3, 0xA0), (4, 85)])],
            _ => vec![],
        });
        let s = cloud3(&mut m, 0x0C9D).unwrap();
        assert_eq!(m.sent, vec![vec![0x66, 0x82], vec![0x66, 0x8A], vec![0x66, 0x89]]);
        assert_eq!((s.name.as_str(), s.connected, s.charging, s.percent, s.millivolts), ("HyperX Cloud III Wireless", true, true, Some(85), 0x0FA0));
        assert_eq!(s.key, "hyperx:0C9D");
    }

    #[test]
    fn cloud3_disconnected_core_name_and_stale_reports() {
        let mut m = mock(|req| vec![buf(62, &[(0, 0x66), (1, req[1]), (2, 0)])]);
        let s = cloud3(&mut m, 0x069F).unwrap();
        assert_eq!((s.name.as_str(), s.connected, s.percent), ("HyperX Cloud II Core Wireless", false, None));
        assert_eq!(m.sent.len(), 1);
        // a report for another command is skipped; short ones don't match; no answer fails
        let mut m = mock(|req| vec![buf(62, &[(0, 0x66), (1, 0x11)]), buf(3, &[]), buf(62, &[(0, 0x66), (1, req[1]), (2, 1)])]);
        assert!(cloud3(&mut m, 0x0C9D).is_some());
        assert!(cloud3(&mut mock(|_| vec![]), 0x0C9D).is_none());
        // percent above 100 is ignored
        let mut m = mock(|req| match req[1] {
            0x89 => vec![buf(62, &[(0, 0x66), (1, 0x89), (4, 101)])],
            _ => vec![buf(62, &[(0, 0x66), (1, req[1]), (2, 1)])],
        });
        let s = cloud3(&mut m, 0x0C9D).unwrap();
        assert_eq!((s.percent, s.millivolts), (None, 0));
    }

    #[test]
    fn headsets_get_a_grace_period() {
        assert_eq!(base(0x0696, "x").grace_sec, GRACE_SEC);
    }

    #[test]
    fn cloud2_hp() {
        let mut m = mock(|req| match req[3] {
            0x02 => vec![buf(20, &[(0, 6), (1, 0xFF), (2, 0xBB), (3, 2), (5, 0x10), (6, 0x01), (7, 64)])],
            0x03 => vec![buf(20, &[(0, 6), (1, 0xFF), (2, 0xBB), (3, 3), (4, 1)])],
            _ => vec![],
        });
        let s = cloud2(&mut m, 0x0696).unwrap();
        assert_eq!(m.sent[0], vec![0x06, 0xFF, 0xBB, 0x02]);
        assert_eq!((s.percent, s.charging, s.millivolts), (Some(64), true, 0x1001));
        // percent > 100 rejects the device
        let mut m = mock(|_| vec![buf(20, &[(0, 6), (1, 0xFF), (2, 0xBB), (3, 2), (7, 200)])]);
        assert!(cloud2(&mut m, 0x0696).is_none());
        // no charging answer: still reported
        let mut m = mock(|req| if req[3] == 2 { vec![buf(20, &[(0, 6), (1, 0xFF), (2, 0xBB), (3, 2), (7, 10)])] } else { vec![] });
        let s = cloud2(&mut m, 0x0696).unwrap();
        assert_eq!((s.percent, s.charging), (Some(10), false));
    }

    #[test]
    fn cloud2_kingston_frame_and_reply() {
        let mut m = mock(|req| match req[15] {
            0x02 => vec![buf(62, &[(0, 0x0B), (2, 0xBB), (3, 2), (7, 55)])],
            0x03 => vec![buf(62, &[(0, 0x0B), (2, 0xBB), (3, 3), (4, 1)])],
            _ => vec![],
        });
        let s = cloud2_kingston(&mut m, 0x1718).unwrap();
        assert_eq!(m.sent[0].len(), 17);
        assert_eq!(&m.sent[0][..8], &[0x06, 0x00, 0x02, 0x00, 0x9A, 0x00, 0x00, 0x68]);
        assert_eq!(&m.sent[0][8..], &[0x4A, 0x8E, 0x0A, 0x00, 0x00, 0x00, 0xBB, 0x02, 0x00]);
        assert_eq!((s.percent, s.charging, s.millivolts), (Some(55), true, 0));
        assert!(cloud2_kingston(&mut mock(|_| vec![]), 0x1718).is_none());
    }

    #[test]
    fn cloud_alpha_offline_and_online() {
        let mut m = mock(|req| match req[2] {
            0x03 => vec![buf(8, &[(0, 0x21), (2, 3), (3, 1)])],
            _ => vec![],
        });
        let s = cloud_alpha(&mut m, 0x098D).unwrap();
        assert!(!s.connected && s.percent.is_none());
        assert_eq!(m.sent.len(), 1);
        let mut m = mock(|req| match req[2] {
            0x03 => vec![buf(8, &[(0, 0x21), (2, 3), (3, 0)])],
            0x0C => vec![buf(8, &[(0, 0x21), (2, 0x0C), (3, 1)])],
            0x0B => vec![buf(8, &[(0, 0x21), (2, 0x0B), (3, 73)])],
            _ => vec![],
        });
        let s = cloud_alpha(&mut m, 0x098D).unwrap();
        assert_eq!(m.sent[0], vec![0x21, 0xBB, 0x03]);
        assert_eq!((s.connected, s.charging, s.percent), (true, true, Some(73)));
    }

    #[test]
    fn cloud_alpha2_reply_byte_is_cmd_plus_one() {
        let mut m = mock(|req| match (req[0], req[1]) {
            (0x52, 0x01) => vec![buf(12, &[(0, 0x53), (1, 1), (2, 1)])],
            (0x50, 0x02) => vec![buf(12, &[(0, 0x51), (1, 2), (2, 40), (3, 1), (6, 0x34), (7, 0x0E)])],
            _ => vec![],
        });
        let s = cloud_alpha2(&mut m, 0x08BE).unwrap();
        assert_eq!(m.sent, vec![vec![0x52, 0x01], vec![0x50, 0x02]]);
        assert_eq!((s.connected, s.percent, s.charging, s.millivolts), (true, Some(40), true, 0x0E34));
        // device off
        let mut m = mock(|req| if req[0] == 0x52 { vec![buf(12, &[(0, 0x53), (1, 1), (2, 0)])] } else { vec![] });
        let s = cloud_alpha2(&mut m, 0x08BE).unwrap();
        assert!(!s.connected && s.percent.is_none());
        // reply echoing the request id (not +1) does not match
        assert!(cloud_alpha2(&mut mock(|req| vec![buf(12, &[(0, req[0]), (1, req[1]), (2, 1)])]), 0x08BE).is_none());
    }

    #[test]
    fn flight_percent_curve() {
        assert_eq!(flight_percent(0x100C), None);
        assert_eq!(flight_percent(3000), Some(3)); // truncated linear branch
        assert_eq!(flight_percent(3648), Some(4));
        assert_eq!(flight_percent(3976), Some(100));
        let mid = |mv| flight_percent(mv).unwrap();
        assert!(mid(3649) <= mid(3800) && mid(3800) <= mid(3975));
        assert!(mid(3975) <= 100);
    }

    #[test]
    fn cloud_flight_charging_and_levels() {
        let mut m = mock(|_| vec![buf(8, &[(3, 0x10), (4, 0x20)])]);
        let s = cloud_flight(&mut m, 0x16C4).unwrap();
        assert_eq!(m.sent[0], vec![0x21, 0xFF, 0x05]);
        assert_eq!((s.charging, s.percent, s.millivolts), (true, None, 0x1020));
        let s = cloud_flight(&mut mock(|_| vec![buf(8, &[(3, 0x0F), (4, 0xA0)])]), 0x16C4).unwrap();
        assert_eq!((s.charging, s.percent, s.millivolts), (false, Some(100), 0x0FA0));
        assert!(cloud_flight(&mut mock(|_| vec![buf(4, &[])]), 0x16C4).is_none());
    }

    #[test]
    fn cloud3s_values_and_invalid_ff() {
        let reply = |cmd: u8, v: u8| buf(16, &[(0, 0x0C), (5, cmd), (6, v)]);
        let mut m = mock(|req| match req[5] {
            0x02 => vec![reply(0x02, 2)],
            0x06 => vec![reply(0x06, 0xFF), reply(0x06, 61)], // FF = invalid, keep waiting
            0x48 => vec![reply(0x48, 2)],
            _ => vec![],
        });
        let s = cloud3s(&mut m, 0x06BE).unwrap();
        assert_eq!(m.sent[0], vec![0x0C, 0x02, 0x03, 0x01, 0x00, 0x02]);
        assert_eq!((s.connected, s.percent, s.charging), (true, Some(61), true));
        let s = cloud3s(&mut mock(|req| vec![reply(req[5], if req[5] == 2 { 1 } else { 0 })]), 0x06BE).unwrap();
        assert!(!s.connected && s.percent.is_none());
        // discharging
        let mut m = mock(|req| match req[5] {
            0x02 => vec![reply(0x02, 2)],
            0x06 => vec![reply(0x06, 50)],
            _ => vec![reply(0x48, 0)],
        });
        assert!(!cloud3s(&mut m, 0x06BE).unwrap().charging);
        assert!(cloud3s(&mut mock(|req| vec![reply(req[5], 0xFF)]), 0x06BE).is_none());
    }

    #[test]
    fn model_table_filters_and_pids() {
        let mk = |page, usage, in_len, out_len| HidColl {
            path: Default::default(),
            vid: 0x03F0,
            pid: 0,
            iface: -1,
            usage_page: page,
            usage,
            product: String::new(),
            in_len,
            out_len,
            feat_len: 0,
        };
        assert!((MODELS[0].accept)(&mk(0xFF00, 1, 62, 62)) && !(MODELS[0].accept)(&mk(0xFF00, 1, 64, 64)));
        assert!((MODELS[1].accept)(&mk(0xFF00, 1, 64, 64)) && !(MODELS[1].accept)(&mk(0x0001, 1, 64, 64)));
        assert!((MODELS[3].accept)(&mk(0xFF90, 0x0303, 0, 0)) && !(MODELS[3].accept)(&mk(0xFF90, 1, 0, 0)));
        assert!((MODELS[6].accept)(&mk(0xFF13, 0xFF00, 0, 0)));
        assert!((MODELS[5].accept)(&mk(0xFF00, 1, 0, 3)) && !(MODELS[5].accept)(&mk(0xFF00, 1, 0, 2)));
        assert_eq!(MODELS.iter().map(|m| m.pids.len()).sum::<usize>(), 2 + 2 + 2 + 3 + 1 + 1 + 1 + 3 + 1);
    }
}

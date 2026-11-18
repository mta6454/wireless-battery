//! Sony INZONE H9 II (054C:0FA8) and H5 (054C:0EBF) headsets: a Sony command layer tunnelled through
//! HID (report 02, 64 bytes) on the vendor collection with usage page FF04.
//!   command: [02][len][01][00 FC][8+n][96 C3][addr][event][type][tid lo][tid hi][payload][sum]
//!   event:   [02][len][04][FF][..][00][96 C3][addr][event][type][tid lo][tid hi][payload][sum]
//! Battery = event 04, type 01 (GET) to the headset (addr 0x41); reply payload [charging][percent],
//! percent FF = headset off. Both directions carry a checksum and a transaction id.
//! Protocol from github.com/Sapd/HeadsetControl (sony_inzone_protocol.hpp).

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidDev, HidIo};

#[derive(Debug, PartialEq, Eq)]
struct Event {
    id: u8,
    kind: u8,
    tid: u16,
    payload: Vec<u8>,
}

fn parse_event(b: &[u8]) -> Option<Event> {
    if b.len() < 15 || b[0] != 0x02 {
        return None;
    }
    let len = b[1] as usize;
    if !(12..=62).contains(&len) || b.len() < len + 2 {
        return None;
    }
    if b[2] != 0x04 || b[3] != 0xFF || b[5] != 0x00 || b[6] != 0x96 || b[7] != 0xC3 {
        return None;
    }
    if (b[8] >> 4) != 0x1 {
        return None; // must be addressed to the PC
    }
    let sum = b[5..=len].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    if sum != b[len + 1] {
        return None;
    }
    Some(Event { id: b[9], kind: b[10], tid: b[11] as u16 | ((b[12] as u16) << 8), payload: b[13..=len].to_vec() })
}

/// Battery GET request for transaction id `tid` (64 bytes, report id 02).
fn build_request(tid: u16) -> Vec<u8> {
    let mut req = vec![0u8; 64];
    req[0] = 0x02;
    req[1] = 12; // HCI byte count (no payload)
    req[2] = 0x01; // HCI command
    req[3] = 0x00;
    req[4] = 0xFC; // vendor opcode 0xFC00
    req[5] = 8;
    req[6] = 0x96;
    req[7] = 0xC3;
    req[8] = 0x41; // PC -> headset
    req[9] = 0x04; // battery info
    req[10] = 0x01; // GET
    req[11] = tid as u8;
    req[12] = (tid >> 8) as u8;
    req[13] = req[6..=12].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    req
}

/// Next transaction id; 1 is used by the dongle's own notifications so it is skipped.
fn next_tid() -> u16 {
    static TID: AtomicU16 = AtomicU16::new(1);
    let mut t = TID.load(Ordering::Relaxed).wrapping_add(1);
    if t <= 1 {
        t = 2;
    }
    TID.store(t, Ordering::Relaxed);
    t
}

struct Battery {
    /// `None` = unknown / headset off
    percent: Option<u8>,
    charging: bool,
    offline: bool,
}

fn get_battery(io: &mut impl HidIo, tid: u16) -> Option<Battery> {
    io.flush();
    if !io.write(&build_request(tid)) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(1500);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let r = io.read(left.as_millis() as i32)?;
        if r.len() < 15 {
            return None;
        }
        let Some(e) = parse_event(&r) else { continue };
        if e.id != 0x04 || e.tid != tid || e.payload.len() < 2 {
            continue;
        }
        if e.kind != 0x10 && e.kind != 0xA0 {
            continue; // RET or active notification
        }
        let offline = e.payload[1] == 0xFF;
        let percent = if !offline && e.payload[1] <= 100 { Some(e.payload[1]) } else { None };
        return Some(Battery { percent, charging: e.payload[0] != 0, offline });
    }
}

fn status(pid: u16, name: &str, b: &Battery) -> DeviceStatus {
    DeviceStatus {
        key: format!("inzone:{}", hex4(pid)),
        name: name.into(),
        kind: DeviceKind::Headset,
        via: "2.4 GHz receiver".into(),
        connected: !b.offline,
        percent: b.percent,
        charging: b.charging,
        ..Default::default()
    }
}

/// (pid, model name). The original also carried a usage per model but never used it.
const MODELS: [(u16, &str); 2] = [(0x0FA8, "Sony INZONE H9 II"), (0x0EBF, "Sony INZONE H5")];

pub fn poll(ctx: &mut PollContext) {
    for (pid, name) in MODELS {
        if ctx.is_claimed(0x054C, pid) {
            continue;
        }
        for c in ctx.find(0x054C, pid, |c| c.usage_page == 0xFF04 && c.out_len >= 64) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some(b) = get_battery(&mut d, next_tid()) else { continue };
            ctx.add(status(pid, name, &b), 0x054C, pid);
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

    /// Builds a device->PC event frame with a valid checksum.
    fn event(id: u8, kind: u8, tid: u16, payload: &[u8]) -> Vec<u8> {
        let len = 12 + payload.len();
        let mut b = vec![0u8; 64];
        b[0] = 0x02;
        b[1] = len as u8;
        b[2] = 0x04;
        b[3] = 0xFF;
        b[6] = 0x96;
        b[7] = 0xC3;
        b[8] = 0x14; // headset -> PC
        b[9] = id;
        b[10] = kind;
        b[11] = tid as u8;
        b[12] = (tid >> 8) as u8;
        b[13..13 + payload.len()].copy_from_slice(payload);
        b[len + 1] = b[5..=len].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        b
    }

    #[test]
    fn request_layout_and_checksum() {
        let r = build_request(0x0102);
        assert_eq!(r.len(), 64);
        assert_eq!(&r[..13], &[0x02, 12, 0x01, 0x00, 0xFC, 8, 0x96, 0xC3, 0x41, 0x04, 0x01, 0x02, 0x01]);
        let sum: u32 = [0x96u32, 0xC3, 0x41, 0x04, 0x01, 0x02, 0x01].iter().sum();
        assert_eq!(r[13], sum as u8);
        assert!(r[14..].iter().all(|&b| b == 0));
    }

    #[test]
    fn tids_skip_zero_and_one() {
        let a = next_tid();
        let b = next_tid();
        assert!(a >= 2 && b >= 2 && a != b);
    }

    #[test]
    fn battery_reply_decoding() {
        let mut m = Mock::new(vec![event(0x04, 0x10, 7, &[1, 64])]);
        let b = get_battery(&mut m, 7).unwrap();
        assert_eq!(m.sent[0], build_request(7));
        assert_eq!(m.flushed, 1);
        assert_eq!((b.percent, b.charging, b.offline), (Some(64), true, false));
        // active notification (type A0), not charging
        let b = get_battery(&mut Mock::new(vec![event(0x04, 0xA0, 9, &[0, 100])]), 9).unwrap();
        assert_eq!((b.percent, b.charging), (Some(100), false));
    }

    #[test]
    fn headset_off_is_offline_and_hidden() {
        let b = get_battery(&mut Mock::new(vec![event(0x04, 0x10, 3, &[0, 0xFF])]), 3).unwrap();
        assert!(b.offline && b.percent.is_none());
        let s = status(0x0FA8, "Sony INZONE H9 II", &b);
        assert!(!s.connected);
        assert_eq!(s.key, "inzone:0FA8");
        assert_eq!(s.kind, DeviceKind::Headset);
        assert_eq!(s.via, "2.4 GHz receiver");
    }

    #[test]
    fn percent_over_100_is_unknown() {
        let b = get_battery(&mut Mock::new(vec![event(0x04, 0x10, 3, &[0, 101])]), 3).unwrap();
        assert_eq!(b.percent, None);
        assert!(!b.offline);
    }

    #[test]
    fn unrelated_events_are_skipped() {
        let mut m = Mock::new(vec![
            event(0x04, 0x10, 99, &[0, 10]),      // other transaction
            event(0x05, 0x10, 3, &[0, 10]),       // other event id
            event(0x04, 0x11, 3, &[0, 10]),       // wrong type
            event(0x04, 0x10, 3, &[0]),           // payload too short
            vec![0x02; 20],                        // garbage
            event(0x04, 0x10, 3, &[0, 42]),
        ]);
        assert_eq!(get_battery(&mut m, 3).unwrap().percent, Some(42));
    }

    #[test]
    fn parse_event_validates_frame() {
        let good = event(0x04, 0x10, 0x0102, &[1, 55]);
        let e = parse_event(&good).unwrap();
        assert_eq!(e, Event { id: 4, kind: 0x10, tid: 0x0102, payload: vec![1, 55] });
        let mut bad = good.clone();
        bad[14] ^= 1; // checksum
        assert!(parse_event(&bad).is_none());
        let mut bad = good.clone();
        bad[8] = 0x41; // addressed to headset, not PC
        assert!(parse_event(&bad).is_none());
        let mut bad = good.clone();
        bad[0] = 0x03;
        assert!(parse_event(&bad).is_none());
        let mut bad = good.clone();
        bad[1] = 11; // length out of range
        assert!(parse_event(&bad).is_none());
        let mut bad = good.clone();
        bad[7] = 0;
        assert!(parse_event(&bad).is_none());
        assert!(parse_event(&good[..14]).is_none());
    }

    #[test]
    fn failed_write_short_read_and_silence_give_none() {
        let mut m = Mock::new(vec![event(0x04, 0x10, 3, &[0, 42])]);
        m.accept_write = false;
        assert!(get_battery(&mut m, 3).is_none());
        assert!(get_battery(&mut Mock::new(vec![vec![0x02; 10]]), 3).is_none());
        assert!(get_battery(&mut Mock::new(vec![]), 3).is_none());
    }
}

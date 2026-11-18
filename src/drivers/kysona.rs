//! Kysona, Pulsar and ATK / VXE wireless mice (VID 3554, ATK dongles 373B), G-Wolves models on this protocol
//! (VID 33E4, e.g. Fenrir Asym) and the original Hitscan
//! Hyperlight (VID 3770, verified on the 1K receiver 3770:0200), Pulsar's newer XS-1 generation
//! (VID 3710, verified on the 8K Dongle 3710:5406) and Redragon's Compx-based mice (M916 Pro 1K,
//! M913 tri-mode, G49: receiver 3554:F55D / F5D5, cable F55E; verified on an M916 Pro 1K through
//! the F55D receiver). They share one protocol:
//! 17-byte output/input report id 08: [08][cmd][0 x14][checksum], checksum = 0x55 - sum(bytes 0..15).
//!   cmd 03 = online status -> reply [6] = 1 when the mouse is connected to the dongle
//!   cmd 04 = battery       -> reply [6] = percent, [7] = charging, [8..9] = millivolts
//! Reference: Linux drivers/hid/hid-kysona.c (Lode Willems) and the hid-pulsar patch discussion;
//! Redragon ids from github.com/EliottV17/redragon-battery-omarchy and github.com/pingpingjinping/g49-web-control.

use std::time::{Duration, Instant};

use super::glorious::{gwolves_is_cable, gwolves_model_name};
use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, kind_from_collections, HidColl, HidDev, HidIo};

/// Redragon uses Compx's own ids, and its receiver only calls itself "2.4G Wireless Receiver".
fn is_redragon(vid: u16, pid: u16) -> bool {
    vid == 0x3554 && matches!(pid, 0xF55D | 0xF5D5 | 0xF55E)
}

fn is_cable(vid: u16, pid: u16) -> bool {
    (vid == 0x33E4 && gwolves_is_cable(pid)) || (vid == 0x3554 && pid == 0xF55E)
}

/// `0x55 - sum(b[0..16])`, modulo 256.
fn checksum(b: &[u8]) -> u8 {
    let sum: u32 = b[..16].iter().map(|&x| x as u32).sum();
    0x55u32.wrapping_sub(sum) as u8
}

fn build_request(cmd: u8) -> [u8; 17] {
    let mut req = [0u8; 17];
    req[0] = 0x08;
    req[1] = cmd;
    req[16] = checksum(&req);
    req
}

/// Sends `cmd` and waits (500 ms) for its checksummed reply.
fn query(io: &mut impl HidIo, cmd: u8) -> Option<Vec<u8>> {
    let req = build_request(cmd);
    io.flush();
    if !io.write(&req) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(500);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = io.read(left)?;
        if r.len() < 17 {
            return None;
        }
        if r[0] == 0x08 && r[1] == cmd && r[16] == checksum(&r) {
            return Some(r);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Reading {
    connected: bool,
    percent: Option<u8>,
    charging: bool,
    millivolts: u32,
}

/// Online-status then battery queries; `None` when the device answered neither.
fn read_status(io: &mut impl HidIo) -> Option<Reading> {
    let mut s = Reading { connected: true, percent: None, charging: false, millivolts: 0 };
    let mut answered = false;
    if let Some(r) = query(io, 0x03) {
        answered = true;
        s.connected = r[6] != 0;
    }
    if !answered || s.connected {
        if let Some(r) = query(io, 0x04) {
            answered = true;
            s.connected = true;
            if r[6] <= 100 {
                s.percent = Some(r[6]);
            }
            s.charging = r[7] != 0;
            s.millivolts = ((r[8] as u32) << 8) | r[9] as u32;
        }
    }
    answered.then_some(s)
}

fn clean_name(product: &str, vid: u16, pid: u16) -> String {
    if vid == 0x33E4 {
        return gwolves_model_name(pid).to_string();
    }
    if is_redragon(vid, pid) {
        return "Redragon wireless mouse".into(); // shared by several models
    }
    let mut n = product.to_string();
    for suffix in [" Dongle", " Receiver", " 2.4G", " Wireless Receiver"] {
        if n.len() > suffix.len() && n.ends_with(suffix) {
            n.truncate(n.len() - suffix.len());
        }
    }
    if vid == 0x3770 {
        return "Hitscan Hyperlight".into(); // the receiver only calls itself "Hitscan USB Receiver"
    }
    if n.is_empty() {
        n = if vid == 0x373B { "ATK wireless mouse" } else { "Wireless mouse" }.into();
    }
    n
}

/// Kysona exposes a dedicated 17-byte collection; ATK / VXE receivers put report 08 in a vendor
/// collection that also carries larger reports. The checksummed reply filters out anything that
/// does not speak this protocol.
fn collection_matches(c: &HidColl) -> bool {
    (c.out_len == 17 && c.in_len == 17) || (c.usage_page >= 0xFF00 && c.out_len >= 17 && c.in_len >= 17)
}

pub fn poll(ctx: &mut PollContext) {
    let mut seen_names: Vec<String> = Vec::new();
    for vendor in [0x3554u16, 0x373B, 0x3770, 0x33E4, 0x3710] {
        for (vid, pid) in ctx.products(vendor) {
            for c in ctx.find(vid, pid, collection_matches) {
                let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
                let Some(r) = read_status(&mut d) else { continue };
                let mut s = DeviceStatus {
                    key: format!("kysona:{}:{}", hex4(vid), hex4(pid)),
                    name: clean_name(&c.product, vid, pid),
                    kind: kind_from_collections(&ctx.colls, vid, pid),
                    connected: r.connected,
                    percent: r.percent,
                    charging: r.charging,
                    millivolts: r.millivolts,
                    ..Default::default()
                };
                if s.kind == DeviceKind::Other {
                    s.kind = DeviceKind::Mouse;
                }
                s.via = if is_cable(vid, pid) { "USB cable" } else { "2.4 GHz receiver" }.into();
                // A mouse plugged in for charging can show up twice (cable + dongle); keep one.
                if seen_names.contains(&s.name) {
                    ctx.claim(vid, pid);
                    if s.charging {
                        for o in ctx.out.iter_mut().filter(|o| o.name == s.name) {
                            o.charging = true;
                        }
                    }
                } else {
                    seen_names.push(s.name.clone());
                    ctx.add(s, vid, pid);
                }
                break;
            }
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
            true
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            if self.replies.is_empty() { None } else { Some(self.replies.remove(0)) }
        }
        fn flush(&mut self) {}
    }

    fn frame(cmd: u8, set: impl Fn(&mut [u8])) -> Vec<u8> {
        let mut r = vec![0u8; 17];
        r[0] = 0x08;
        r[1] = cmd;
        set(&mut r);
        r[16] = checksum(&r);
        r
    }

    fn mock(replies: Vec<Vec<u8>>) -> Mock {
        Mock { replies, sent: vec![] }
    }

    #[test]
    fn checksum_and_request_layout() {
        let r = build_request(0x04);
        assert_eq!(&r[..3], &[0x08, 0x04, 0x00]);
        assert_eq!(r[16], 0x55 - 0x08 - 0x04);
        assert_eq!(build_request(0x03)[16], 0x55 - 0x08 - 0x03);
        // wraps modulo 256
        let mut b = [0xFFu8; 17];
        assert_eq!(checksum(&b), 0x55u8.wrapping_sub(0xFFu8.wrapping_mul(16)));
        b[0] = 0;
        assert_eq!(checksum(&b), 0x55u8.wrapping_sub(0xFFu8.wrapping_mul(15)));
    }

    #[test]
    fn battery_reply_decoding() {
        let on = frame(0x03, |r| r[6] = 1);
        let bat = frame(0x04, |r| {
            r[6] = 73;
            r[7] = 1;
            r[8] = 0x0F;
            r[9] = 0xA0;
        });
        let mut m = mock(vec![on, bat]);
        let s = read_status(&mut m).unwrap();
        assert_eq!(s, Reading { connected: true, percent: Some(73), charging: true, millivolts: 4000 });
        assert_eq!(m.sent.len(), 2);
        assert_eq!(m.sent[0][1], 0x03);
        assert_eq!(m.sent[1][1], 0x04);
    }

    #[test]
    fn offline_mouse_skips_battery_query() {
        let mut m = mock(vec![frame(0x03, |r| r[6] = 0)]);
        let s = read_status(&mut m).unwrap();
        assert!(!s.connected);
        assert_eq!(s.percent, None);
        assert_eq!(m.sent.len(), 1);
    }

    #[test]
    fn percent_over_100_is_unknown_but_connected() {
        let mut m = mock(vec![frame(0x03, |r| r[6] = 1), frame(0x04, |r| r[6] = 150)]);
        let s = read_status(&mut m).unwrap();
        assert!(s.connected);
        assert_eq!(s.percent, None);
    }

    #[test]
    fn unrelated_and_corrupt_frames_are_skipped() {
        let mut bad_sum = frame(0x04, |r| r[6] = 10);
        bad_sum[16] ^= 0x01;
        let wrong_cmd = frame(0x03, |r| r[6] = 1);
        let good = frame(0x04, |r| r[6] = 55);
        let mut m = mock(vec![bad_sum, wrong_cmd, good]);
        assert_eq!(query(&mut m, 0x04).unwrap()[6], 55);
    }

    #[test]
    fn short_read_or_silence_fails() {
        assert!(query(&mut mock(vec![vec![0x08; 10]]), 0x04).is_none());
        assert!(query(&mut mock(vec![]), 0x04).is_none());
        assert!(read_status(&mut mock(vec![])).is_none());
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("Anything", 0x33E4, 0x3508), "G-Wolves Fenrir");
        assert_eq!(clean_name("2.4G Wireless Receiver", 0x3554, 0xF55D), "Redragon wireless mouse");
        assert_eq!(clean_name("Kysona M600 Dongle", 0x3554, 0xF58A), "Kysona M600");
        assert_eq!(clean_name("Pulsar X2 Receiver", 0x3710, 0x5406), "Pulsar X2");
        assert_eq!(clean_name("Hitscan USB Receiver", 0x3770, 0x0200), "Hitscan Hyperlight");
        assert_eq!(clean_name("", 0x373B, 0x1000), "ATK wireless mouse");
        assert_eq!(clean_name("", 0x3554, 0x1000), "Wireless mouse");
        assert_eq!(clean_name(" Dongle", 0x3554, 0x1000), " Dongle"); // not longer than the suffix
    }

    #[test]
    fn cable_and_collection_filters() {
        assert!(is_cable(0x3554, 0xF55E) && !is_cable(0x3554, 0xF55D));
        assert!(is_cable(0x33E4, 0x3508) && !is_cable(0x33E4, 0x3517));
        let mut c = HidColl {
            path: Default::default(),
            vid: 0x3554,
            pid: 1,
            iface: -1,
            usage_page: 0x01,
            usage: 2,
            product: String::new(),
            in_len: 17,
            out_len: 17,
            feat_len: 0,
        };
        assert!(collection_matches(&c));
        c.in_len = 65;
        c.out_len = 65;
        assert!(!collection_matches(&c));
        c.usage_page = 0xFF00;
        assert!(collection_matches(&c));
    }
}

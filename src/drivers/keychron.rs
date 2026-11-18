//! Keychron wireless mice (VID 3434, e.g. M4), from Keychron's own web Launcher (launcher.keychron.com).
//! 64-byte output report without report id: [01][00][81][01][0...][checksum], checksum (byte 63)
//! = 0xA1 - sum(bytes 0..62). The reply [01][..][..][01][state][rate][vid lo][vid hi][pid lo][pid hi]
//! [charging][battery %]... carries the mouse's own vendor id, which is checked so that only a
//! genuine reply is accepted. Keychron keyboards do not report their battery this way (only over
//! Bluetooth, which the Bluetooth fallback already covers).
//! The 8K mice (M5 8K, G6 HE 8K ... on the TurboLink 8K receiver, Launcher protocol "8k_nordic") use
//! the same request, but on the receiver bit 6 of the first byte is set (41 00 81 01 ...). Their reply
//! [41][..][..][01][..][max rate][..]..[state][battery %][profile] has no vendor id; state 1 =
//! charging, 3 is treated as 0.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidDev, HidIo};

/// Builds the 64-byte payload (without report id) with its checksum in byte 63.
fn build_payload(cmd: u8) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[0] = cmd;
    p[2] = 0x81;
    p[3] = 0x01;
    let sum: u32 = p[..63].iter().map(|&x| x as u32).sum();
    p[63] = 0xA1u32.wrapping_sub(sum) as u8;
    p
}

fn clean_name(n: &str) -> String {
    if n.is_empty() || n.contains("Link") || n.contains("Receiver") {
        return "Keychron mouse".into();
    }
    n.to_string()
}

/// Sends [cmd][00][81][01] and waits up to 600 ms for a reply starting with cmd whose byte 3 is 01.
fn query(io: &mut impl HidIo, cmd: u8) -> Option<Vec<u8>> {
    let mut req = vec![0x00u8]; // report id 0 (unnumbered)
    req.extend_from_slice(&build_payload(cmd));
    io.flush();
    if !io.write(&req) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(600);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = io.read(left)?;
        if r.len() < 13 {
            return None;
        }
        if r[0] == cmd && r[3] == 0x01 {
            return Some(r);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Reading {
    percent: u8,
    charging: bool,
    wireless: bool,
}

fn read_battery(io: &mut impl HidIo) -> Option<Reading> {
    let r1 = query(io, 0x01);
    if let Some(r1) = r1.as_ref().filter(|r| (r[6] as u16 | (r[7] as u16) << 8) == 0x3434 && r[11] <= 100) {
        // M4 and other mice on the classic protocol: the reply carries the vendor id
        return Some(Reading { percent: r1[11], charging: r1[10] != 0, wireless: true });
    }
    if let Some(r) = query(io, 0x41).filter(|r| r[10] <= 3 && r[11] <= 100) {
        // 8K mouse on its receiver
        return Some(Reading { percent: r[11], charging: r[10] == 1, wireless: true });
    }
    if let Some(r1) = r1.as_ref().filter(|r| r[10] <= 3 && (1..=100).contains(&r[11]) && (1..=8).contains(&r[5])) {
        // 8K mouse on the USB cable (no bit 6); byte 5 = highest polling rate level
        return Some(Reading { percent: r1[11], charging: r1[10] == 1, wireless: false });
    }
    None
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x3434) {
        for c in ctx.find(vid, pid, |c| c.usage_page >= 0xFF00 && c.out_len == 65 && c.in_len == 65) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some(r) = read_battery(&mut d) else { continue };
            let s = DeviceStatus {
                key: format!("keychron:{}", hex4(pid)),
                name: clean_name(&c.product),
                kind: DeviceKind::Mouse,
                via: if r.wireless { "2.4 GHz receiver" } else { "USB cable" }.into(),
                percent: Some(r.percent),
                charging: r.charging,
                ..Default::default()
            };
            ctx.add(s, vid, pid);
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

    fn mock(replies: Vec<Vec<u8>>) -> Mock {
        Mock { replies, sent: vec![] }
    }

    fn frame(first: u8, set: impl Fn(&mut [u8])) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = first;
        r[3] = 0x01;
        set(&mut r);
        r
    }

    #[test]
    fn request_layout_and_checksum() {
        let p = build_payload(0x01);
        assert_eq!(&p[..4], &[0x01, 0x00, 0x81, 0x01]);
        assert_eq!(p[63], 0xA1 - 0x01 - 0x81 - 0x01);
        let p41 = build_payload(0x41);
        assert_eq!(p41[63], 0xA1u8.wrapping_sub(0x41 + 0x81 + 0x01));
        // on the wire: report id 0 + 64-byte payload
        let mut m = mock(vec![frame(0x01, |_| {})]);
        query(&mut m, 0x01).unwrap();
        assert_eq!(m.sent[0].len(), 65);
        assert_eq!(m.sent[0][0], 0x00);
        assert_eq!(m.sent[0][1], 0x01);
    }

    #[test]
    fn classic_mouse_checks_vendor_id() {
        let ok = frame(0x01, |r| {
            r[6] = 0x34;
            r[7] = 0x34;
            r[10] = 1;
            r[11] = 66;
        });
        let got = read_battery(&mut mock(vec![ok])).unwrap();
        assert_eq!(got, Reading { percent: 66, charging: true, wireless: true });
    }

    #[test]
    fn eightk_receiver_fallback() {
        // classic query answered but no vendor id and not a cable-shaped reply, then 0x41 answers
        let classic = frame(0x01, |r| {
            r[10] = 9;
            r[11] = 50;
        });
        let recv = frame(0x41, |r| {
            r[10] = 1;
            r[11] = 80;
        });
        let got = read_battery(&mut mock(vec![classic, recv])).unwrap();
        assert_eq!(got, Reading { percent: 80, charging: true, wireless: true });
        // state 3 is not "charging"
        let recv3 = frame(0x41, |r| {
            r[10] = 3;
            r[11] = 40;
        });
        let got = read_battery(&mut mock(vec![frame(0x01, |_| {}), recv3])).unwrap();
        assert!(!got.charging && got.percent == 40);
    }

    #[test]
    fn eightk_cable_uses_first_reply() {
        let cable = frame(0x01, |r| {
            r[5] = 4;
            r[10] = 1;
            r[11] = 90;
        });
        // 0x41 query gets no answer
        let got = read_battery(&mut mock(vec![cable])).unwrap();
        assert_eq!(got, Reading { percent: 90, charging: true, wireless: false });
        // percent 0 or polling-rate byte out of range is rejected on the cable path
        let zero = frame(0x01, |r| {
            r[5] = 4;
            r[11] = 0;
        });
        assert!(read_battery(&mut mock(vec![zero])).is_none());
        let bad_rate = frame(0x01, |r| {
            r[5] = 0;
            r[11] = 50;
        });
        assert!(read_battery(&mut mock(vec![bad_rate])).is_none());
    }

    #[test]
    fn silence_and_foreign_frames() {
        assert!(read_battery(&mut mock(vec![])).is_none());
        // wrong byte 3 is skipped, then the right one is accepted
        let skip = frame(0x01, |r| r[3] = 0x02);
        let good = frame(0x01, |_| {});
        assert!(query(&mut mock(vec![skip, good]), 0x01).is_some());
        assert!(query(&mut mock(vec![vec![0x01; 8]]), 0x01).is_none());
    }

    #[test]
    fn names() {
        assert_eq!(clean_name(""), "Keychron mouse");
        assert_eq!(clean_name("Keychron Link"), "Keychron mouse");
        assert_eq!(clean_name("Keychron Receiver"), "Keychron mouse");
        assert_eq!(clean_name("Keychron M4"), "Keychron M4");
    }
}

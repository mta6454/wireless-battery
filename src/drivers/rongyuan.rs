//! Rongyuan 2.4 GHz receivers, used by Attack Shark, Ajazz and many other mice and keyboards (and
//! some MSI devices). One feature report returns both the keyboard and the mouse slot:
//!   request [00 F7 00 ...] on interface 2, vendor collection FFFF:0002
//!   reply   [00][ok][keyboard %][mouse %][keyboard offline][mouse offline]...
//! Receiver list and protocol from github.com/qu1ncky/BatteryTray (RongyuanDongleProvider, MIT),
//! itself taken from the official ATTACK SHARK web driver.

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, sleep_ms, FeatureIo, HidColl, HidDev};

/// (vid << 16) | pid
const RECEIVERS: &[u32] = &[
    0x0DB0197E, 0x0DB08E9A, 0x145F0376, 0x145F038D, 0x14A5C045, 0x25AA2008, 0x25AA4008, 0x25AA4018, 0x31514011,
    0x31514014, 0x31514017, 0x31514021, 0x31514027, 0x3151402D, 0x31514038, 0x31515006, 0x31515007, 0x31515008,
    0x31515020, 0x31515026, 0x3151502B, 0x31515031, 0x31515037, 0x31515038, 0x3151503D, 0x3151503F, 0x31515040,
    0x31515042, 0x31515045, 0x3151504B, 0x3151504E, 0x31515050, 0x31515052, 0x31515055, 0x3151505A, 0x342DE3E0,
    0x342DE3E1, 0x342DE3E2, 0x342DE3E6, 0x342DE3E7, 0x342DE3E8, 0x342DE3F5, 0x342DE3F6, 0x342DE3F7, 0x342DE3FB,
    0x342DE3FC, 0x342DE3FD, 0x347AA302, 0x347AA305, 0x374AA205, 0x374AA217, 0x374AA220, 0x374AA223, 0x374AA226,
    0x374AA229, 0x374AA234, 0x374AA237, 0x374AA251, 0x374AA263, 0x374AA268, 0x374AA311, 0x374AA314, 0x374AA319,
    0x374AA322, 0x374AA329, 0x374AA355, 0x379A1804, 0x379A1807, 0x38A90002, 0x38EE0000, 0x38EE0002, 0x38EE0039,
    0x391D1A04, 0x391D1A08, 0x391D1A0E, 0x391D1A13, 0x391D1A25, 0x391D1A2B, 0x391D1A34, 0x391D2017, 0x391D2A05,
];

fn is_receiver(vid: u16, pid: u16) -> bool {
    RECEIVERS.contains(&(((vid as u32) << 16) | pid as u32))
}

/// Interface 2, vendor collection FFFF:0002 with a feature report of at least 7 bytes. macOS does
/// not report interface numbers (iface < 0), so there the usage page / usage / length decide.
fn collection_matches(c: &HidColl) -> bool {
    (c.iface == 2 || c.iface < 0) && c.usage_page == 0xFFFF && c.usage == 0x0002 && c.feat_len >= 7
}

/// Up to three attempts (100 ms apart) to get a feature reply with some non-zero slot data.
fn query_status(io: &mut impl FeatureIo, feat_len: usize, retry_ms: u64, reply_ms: u64) -> Option<Vec<u8>> {
    for attempt in 0..3 {
        if attempt > 0 {
            sleep_ms(retry_ms);
        }
        let mut cmd = vec![0u8; feat_len];
        cmd[1] = 0xF7;
        if !io.set_feature(&cmd) {
            return None;
        }
        sleep_ms(reply_ms);
        if let Some(r) = io.get_feature(0, feat_len) {
            if r.len() >= 7 && (r[2] | r[3] | r[4] | r[5] | r[6]) != 0 {
                return Some(r);
            }
        }
    }
    None
}

/// Builds the device entries for the keyboard slot and the mouse slot of a reply.
fn slots(r: &[u8], vid: u16, pid: u16, product: &str) -> Vec<DeviceStatus> {
    let generic = product.is_empty() || product.contains("Receiver") || product.contains("2.4G");
    let mut out = Vec::new();
    // Slot layout: [2] keyboard %, [3] mouse %, [4] keyboard offline, [5] mouse offline.
    for slot in 0..2 {
        let keyboard = slot == 0;
        if r[if keyboard { 4 } else { 5 }] != 0 {
            continue; // nothing connected in this slot
        }
        let level = r[if keyboard { 2 } else { 3 }];
        let name = if vid == 0x3151 && pid == 0x402D && !keyboard {
            "Attack Shark R3".to_string()
        } else if generic {
            if keyboard { "Wireless keyboard" } else { "Wireless mouse" }.to_string()
        } else {
            product.to_string()
        };
        out.push(DeviceStatus {
            key: format!("rongyuan:{}:{}{}", hex4(vid), hex4(pid), if keyboard { ":kb" } else { ":ms" }),
            name,
            kind: if keyboard { DeviceKind::Keyboard } else { DeviceKind::Mouse },
            via: "2.4 GHz receiver".into(),
            percent: (1..=100).contains(&level).then_some(level),
            ..Default::default()
        });
    }
    out
}

pub fn poll(ctx: &mut PollContext) {
    let mut products: Vec<(u16, u16)> = Vec::new();
    for c in &ctx.colls {
        if is_receiver(c.vid, c.pid) && !ctx.is_claimed(c.vid, c.pid) && !products.contains(&(c.vid, c.pid)) {
            products.push((c.vid, c.pid));
        }
    }
    for (vid, pid) in products {
        for c in ctx.find(vid, pid, collection_matches) {
            let Some(mut d) = HidDev::open_feature_only(ctx.api, &c) else { continue };
            let Some(r) = query_status(&mut d, c.feat_len as usize, 100, 20) else { continue };
            ctx.out.extend(slots(&r, vid, pid, &c.product));
            ctx.claim(vid, pid);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock {
        replies: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        accept: bool,
    }
    impl FeatureIo for Mock {
        fn set_feature(&mut self, d: &[u8]) -> bool {
            self.sent.push(d.to_vec());
            self.accept
        }
        fn get_feature(&mut self, id: u8, _: usize) -> Option<Vec<u8>> {
            assert_eq!(id, 0);
            if self.replies.is_empty() { None } else { Some(self.replies.remove(0)) }
        }
    }
    fn mock(replies: Vec<Vec<u8>>) -> Mock {
        Mock { replies, sent: vec![], accept: true }
    }
    /// [00][ok][kb %][ms %][kb offline][ms offline]
    fn reply(kb: u8, ms: u8, kb_off: u8, ms_off: u8) -> Vec<u8> {
        let mut r = vec![0u8; 65];
        r[1] = 1;
        r[2] = kb;
        r[3] = ms;
        r[4] = kb_off;
        r[5] = ms_off;
        r
    }

    #[test]
    fn request_layout() {
        let mut m = mock(vec![reply(10, 20, 0, 0)]);
        query_status(&mut m, 65, 0, 0).unwrap();
        assert_eq!(m.sent[0].len(), 65);
        assert_eq!(m.sent[0][1], 0xF7);
        assert_eq!(m.sent[0].iter().filter(|&&b| b != 0).count(), 1);
    }

    #[test]
    fn retries_up_to_three_times_on_empty_data() {
        let mut m = mock(vec![reply(0, 0, 0, 0), reply(0, 0, 0, 0), reply(0, 55, 1, 0)]);
        let r = query_status(&mut m, 65, 0, 0).unwrap();
        assert_eq!(m.sent.len(), 3);
        assert_eq!(r[3], 55);
        let mut m = mock(vec![reply(0, 0, 0, 0); 5]);
        assert!(query_status(&mut m, 65, 0, 0).is_none());
        assert_eq!(m.sent.len(), 3);
    }

    #[test]
    fn rejected_request_gives_up_at_once() {
        let mut m = mock(vec![reply(1, 1, 0, 0)]);
        m.accept = false;
        assert!(query_status(&mut m, 65, 0, 0).is_none());
        assert_eq!(m.sent.len(), 1);
        // too-short replies never count
        assert!(query_status(&mut mock(vec![vec![0, 1, 5, 5, 0, 0]; 3]), 65, 0, 0).is_none());
    }

    #[test]
    fn both_slots_connected() {
        let v = slots(&reply(80, 45, 0, 0), 0x3151, 0x5006, "");
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].name.as_str(), v[0].kind, v[0].percent), ("Wireless keyboard", DeviceKind::Keyboard, Some(80)));
        assert_eq!((v[1].name.as_str(), v[1].kind, v[1].percent), ("Wireless mouse", DeviceKind::Mouse, Some(45)));
        assert_eq!(v[0].key, "rongyuan:3151:5006:kb");
        assert_eq!(v[1].key, "rongyuan:3151:5006:ms");
        assert_eq!(v[1].via, "2.4 GHz receiver");
    }

    #[test]
    fn offline_slot_is_skipped_and_levels_validated() {
        let v = slots(&reply(80, 0, 1, 0), 0x3151, 0x5006, "");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, DeviceKind::Mouse);
        assert_eq!(v[0].percent, None); // 0% = unknown
        let v = slots(&reply(101, 100, 0, 0), 0x3151, 0x5006, "");
        assert_eq!((v[0].percent, v[1].percent), (None, Some(100)));
        assert!(slots(&reply(5, 5, 1, 1), 0x3151, 0x5006, "").is_empty());
    }

    #[test]
    fn naming() {
        let v = slots(&reply(10, 10, 0, 0), 0x3151, 0x402D, "2.4G Wireless Receiver");
        assert_eq!(v[0].name, "Wireless keyboard");
        assert_eq!(v[1].name, "Attack Shark R3");
        let v = slots(&reply(10, 10, 0, 0), 0x3151, 0x5006, "Ajazz Dongle");
        assert_eq!((v[0].name.as_str(), v[1].name.as_str()), ("Ajazz Dongle", "Ajazz Dongle"));
        let v = slots(&reply(10, 10, 0, 0), 0x3151, 0x5006, "USB Receiver");
        assert_eq!(v[1].name, "Wireless mouse");
    }

    #[test]
    fn receiver_table_and_collection_filter() {
        assert!(is_receiver(0x3151, 0x402D) && is_receiver(0x391D, 0x2A05) && is_receiver(0x0DB0, 0x197E));
        assert!(!is_receiver(0x3151, 0x0000) && !is_receiver(0x1234, 0x402D));
        assert_eq!(RECEIVERS.len(), 81);
        let mut c = HidColl {
            path: Default::default(),
            vid: 0x3151,
            pid: 0x5006,
            iface: 2,
            usage_page: 0xFFFF,
            usage: 2,
            product: String::new(),
            in_len: 0,
            out_len: 0,
            feat_len: 65,
        };
        assert!(collection_matches(&c));
        c.iface = -1; // macOS
        assert!(collection_matches(&c));
        c.iface = 1;
        assert!(!collection_matches(&c));
        c.iface = 2;
        c.feat_len = 6;
        assert!(!collection_matches(&c));
    }
}

//! Razer wireless mice, keyboards and dongles (VID 1532).
//! 90-byte feature report (report id 0):
//!   [0] status  [1] transaction id  [2..3] remaining packets  [4] protocol type  [5] data size
//!   [6] command class  [7] command id  [8..87] arguments  [88] crc (xor of 2..87)  [89] reserved
//! Battery: class 07 id 80 -> args[1] = 0..255. Charging: class 07 id 84 -> args[1] = 1.
//! Reply status: 01 busy, 02 ok, 03 failed, 04 timeout (device off), 05 not supported.
//! Different models expect different transaction ids, so the known ones are tried in turn.
//! Reference: github.com/openrazer/openrazer (protocol documentation in its driver sources).

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, guess_kind, hex4, kind_from_collections, sleep_ms, FeatureIo, HidColl, HidDev};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rz {
    Ok,
    Offline,
    Unsupported,
    NoReply,
}

/// Builds the 91-byte request (`[0]` = report id 0, then the 90-byte report).
fn build_request(tid: u8, cls: u8, id: u8, size: u8) -> Vec<u8> {
    let mut rep = vec![0u8; 91];
    {
        let r = &mut rep[1..];
        r[1] = tid;
        r[5] = size;
        r[6] = cls;
        r[7] = id;
        r[88] = r[2..88].iter().fold(0, |c, b| c ^ b);
    }
    rep
}

/// Sends one command and polls for its reply. `delay_scale` lets tests skip the real waits.
fn command(d: &mut impl FeatureIo, tid: u8, cls: u8, id: u8, size: u8, wait: bool) -> (Rz, Vec<u8>) {
    if !d.set_feature(&build_request(tid, cls, id, size)) {
        return (Rz::NoReply, vec![]);
    }
    for attempt in 0..8 {
        if wait {
            sleep_ms(if attempt == 0 { 15 } else { 40 });
        }
        let Some(resp) = d.get_feature(0, 91) else { return (Rz::NoReply, vec![]) };
        if resp.len() < 91 {
            return (Rz::NoReply, vec![]);
        }
        let a = &resp[1..];
        if a[0] == 0x01 {
            continue; // busy
        }
        if a[6] != cls || a[7] != id {
            return (Rz::NoReply, vec![]);
        }
        return match a[0] {
            0x02 => (Rz::Ok, a[8..88].to_vec()),
            0x04 => (Rz::Offline, vec![]),
            0x05 | 0x03 => (Rz::Unsupported, vec![]),
            _ => (Rz::NoReply, vec![]),
        };
    }
    (Rz::NoReply, vec![])
}

fn clean_name(product: &str) -> String {
    let mut n = product.to_string();
    for suffix in [" Dongle", " (Wired)", " (Wireless)", " Wireless Receiver", " Receiver", " HyperSpeed Dongle"] {
        if n.len() > suffix.len() && n.ends_with(suffix) {
            n.truncate(n.len() - suffix.len());
        }
    }
    if n.is_empty() {
        n = "Razer device".into();
    }
    n
}

// Exact keyboard/keypad/laptop and mouse/receiver IDs from OpenRazer (no PID-range assumptions).
// https://github.com/openrazer/openrazer/blob/cd5c4243bfafb08df7e2d2963367c3c150626eac/driver/razerkbd_driver.h
// https://github.com/openrazer/openrazer/blob/cd5c4243bfafb08df7e2d2963367c3c150626eac/driver/razermouse_driver.h
const KEYBOARD_PIDS: [u16; 125] = [
    0x010D, 0x010E, 0x010F, 0x0111, 0x0113, 0x0118, 0x011A, 0x011B, 0x011C, 0x0201,
    0x0202, 0x0203, 0x0204, 0x0205, 0x0207, 0x0208, 0x0209, 0x020F, 0x0210, 0x0211,
    0x0214, 0x0216, 0x0217, 0x021A, 0x021E, 0x021F, 0x0220, 0x0221, 0x0224, 0x0225,
    0x0226, 0x0227, 0x0228, 0x022A, 0x022B, 0x022C, 0x022D, 0x022F, 0x0232, 0x0233,
    0x0234, 0x0235, 0x0237, 0x0239, 0x023A, 0x023B, 0x023F, 0x0240, 0x0241, 0x0243,
    0x0244, 0x0245, 0x0246, 0x024A, 0x024B, 0x024C, 0x024D, 0x024E, 0x0252, 0x0253,
    0x0255, 0x0256, 0x0257, 0x0258, 0x0259, 0x025A, 0x025C, 0x025D, 0x025E, 0x0266,
    0x0268, 0x0269, 0x026A, 0x026B, 0x026C, 0x026D, 0x026E, 0x026F, 0x0270, 0x0271,
    0x0276, 0x0279, 0x027A, 0x0282, 0x0287, 0x028A, 0x028B, 0x028C, 0x028D, 0x028F,
    0x0290, 0x0292, 0x0293, 0x0294, 0x0295, 0x0296, 0x0298, 0x029D, 0x029E, 0x029F,
    0x02A0, 0x02A1, 0x02A2, 0x02A3, 0x02A5, 0x02A6, 0x02A7, 0x02B0, 0x02B6, 0x02B8,
    0x02B9, 0x02BA, 0x02C5, 0x02C6, 0x02C7, 0x02CD, 0x02CF, 0x02D2, 0x02D4, 0x02D5,
    0x02D7, 0x02E0, 0x02E1, 0x02EA, 0x0A24,
];

const MOUSE_PIDS: [u16; 121] = [
    0x0013, 0x0015, 0x0016, 0x001F, 0x0020, 0x0024, 0x0025, 0x0029, 0x002E, 0x002F,
    0x0032, 0x0034, 0x0036, 0x0037, 0x0038, 0x0039, 0x003E, 0x003F, 0x0040, 0x0041,
    0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0048, 0x004C, 0x004F, 0x0050, 0x0053,
    0x0054, 0x0059, 0x005A, 0x005B, 0x005C, 0x005E, 0x005F, 0x0060, 0x0062, 0x0064,
    0x0065, 0x0067, 0x006A, 0x006B, 0x006C, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072,
    0x0073, 0x0077, 0x0078, 0x007A, 0x007B, 0x007C, 0x007D, 0x0080, 0x0083, 0x0084,
    0x0085, 0x0086, 0x0088, 0x008A, 0x008C, 0x008D, 0x008F, 0x0090, 0x0091, 0x0094,
    0x0095, 0x0096, 0x0098, 0x0099, 0x009A, 0x009C, 0x009E, 0x009F, 0x00A1, 0x00A3,
    0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00AA, 0x00AB, 0x00AF, 0x00B0, 0x00B2, 0x00B3,
    0x00B4, 0x00B6, 0x00B7, 0x00B8, 0x00B9, 0x00BE, 0x00BF, 0x00C0, 0x00C1, 0x00C2,
    0x00C3, 0x00C4, 0x00C5, 0x00C7, 0x00C8, 0x00CB, 0x00CC, 0x00CD, 0x00D0, 0x00D1,
    0x00D3, 0x00D4, 0x00D6, 0x00D7, 0x00DA, 0x00DB, 0x00DE, 0x00DF, 0x00E0, 0x00E5,
    0x00E6,
];

fn kind_of(colls: &[HidColl], vid: u16, pid: u16) -> DeviceKind {
    // Both keyboards and mice can expose keyboard AND mouse collections for macros/buttons.
    if vid == 0x1532 {
        if KEYBOARD_PIDS.contains(&pid) {
            return DeviceKind::Keyboard;
        }
        if MOUSE_PIDS.contains(&pid) {
            return DeviceKind::Mouse;
        }
    }
    kind_from_collections(colls, vid, pid)
}

/// Result of probing one feature-report collection.
#[derive(Debug, PartialEq)]
enum Probe {
    Offline,
    Online { percent: u8, charging: Option<bool> },
}

/// Tries the known transaction ids until the device answers the battery query.
fn probe(d: &mut impl FeatureIo, wait: bool) -> Option<Probe> {
    let mut res = Rz::NoReply;
    let mut tid = 0;
    let mut args = vec![];
    let mut saw_offline = false;
    for t in [0x1F, 0x3F, 0xFF, 0x08, 0x9F] {
        let (r, a) = command(d, t, 0x07, 0x80, 0x02, wait);
        if r == Rz::Ok {
            res = r;
            tid = t;
            args = a;
            break;
        }
        saw_offline |= r == Rz::Offline; // may also mean "wrong transaction id": keep trying
    }
    if res != Rz::Ok && saw_offline {
        res = Rz::Offline;
    }
    match res {
        Rz::Ok => {
            let percent = clamp100((args[1] as i32 * 100 + 127) / 255);
            let (r, ch) = command(d, tid, 0x07, 0x84, 0x02, wait);
            Some(Probe::Online { percent, charging: (r == Rz::Ok).then(|| ch[1] == 1) })
        }
        Rz::Offline => Some(Probe::Offline),
        _ => None,
    }
}

pub fn poll(ctx: &mut PollContext) {
    let mut seen_names: Vec<String> = Vec::new();
    for (vid, pid) in ctx.products(0x1532) {
        // On macOS the descriptor of a mouse/keyboard collection may be unreadable (length 0, no
        // Input Monitoring permission); the 91-byte feature report is still usable there.
        let colls = ctx.find(vid, pid, |c| c.feat_len == 91 || (cfg!(target_os = "macos") && c.feat_len == 0));
        for c in colls {
            let Some(mut d) = HidDev::open_feature_only(ctx.api, &c) else { continue };
            let Some(p) = probe(&mut d, true) else { continue };

            let mut s = DeviceStatus { key: format!("razer:{}", hex4(pid)), name: clean_name(&c.product), ..Default::default() };
            s.kind = kind_of(&ctx.colls, vid, pid);
            if s.kind == DeviceKind::Other {
                s.kind = guess_kind(&s.name);
            }
            s.via = "Razer receiver".into();
            match p {
                Probe::Offline => s.connected = false,
                Probe::Online { percent, charging } => {
                    s.percent = Some(percent);
                    if let Some(ch) = charging {
                        s.charging = ch;
                    }
                }
            }
            // A wireless mouse plugged in for charging shows up twice (cable + dongle); keep one.
            if !seen_names.contains(&s.name) {
                seen_names.push(s.name.clone());
                ctx.add(s, vid, pid);
            } else {
                ctx.claim(vid, pid);
                if s.charging {
                    for o in ctx.out.iter_mut().filter(|o| o.name == s.name) {
                        o.charging = true;
                    }
                }
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted Razer device: answers each request via `answer(tid, cls, id)` -> (status, args).
    struct Mock {
        sent: Vec<Vec<u8>>,
        pending: Option<Vec<u8>>,
        busy_first: u32,
        answer: Box<dyn FnMut(u8, u8, u8) -> Option<(u8, Vec<u8>)>>,
        accept_set: bool,
    }

    impl Mock {
        fn new(answer: impl FnMut(u8, u8, u8) -> Option<(u8, Vec<u8>)> + 'static) -> Self {
            Self { sent: vec![], pending: None, busy_first: 0, answer: Box::new(answer), accept_set: true }
        }
    }

    impl FeatureIo for Mock {
        fn set_feature(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            let (tid, cls, id) = (data[2], data[7], data[8]);
            self.pending = (self.answer)(tid, cls, id).map(|(status, args)| {
                let mut r = vec![0u8; 91];
                r[1] = status;
                r[2] = tid;
                r[7] = cls;
                r[8] = id;
                for (i, a) in args.iter().enumerate() {
                    r[9 + i] = *a;
                }
                r
            });
            self.accept_set
        }
        fn get_feature(&mut self, _id: u8, _len: usize) -> Option<Vec<u8>> {
            if self.busy_first > 0 {
                self.busy_first -= 1;
                return Some(vec![0u8; 91].into_iter().enumerate().map(|(i, _)| if i == 1 { 0x01 } else { 0 }).collect());
            }
            self.pending.clone()
        }
    }

    /// Device that only accepts transaction id `tid`, with `raw` battery (0..255) and charging flag.
    fn device(tid_ok: u8, raw: u8, charging: u8) -> Mock {
        Mock::new(move |tid, cls, id| {
            if tid != tid_ok {
                return Some((0x03, vec![])); // failed: wrong transaction id
            }
            match (cls, id) {
                (0x07, 0x80) => Some((0x02, vec![0, raw])),
                (0x07, 0x84) => Some((0x02, vec![0, charging])),
                _ => None,
            }
        })
    }

    #[test]
    fn request_layout_and_crc() {
        let r = build_request(0x3F, 0x07, 0x80, 0x02);
        assert_eq!(r.len(), 91);
        assert_eq!((r[0], r[2], r[6], r[7], r[8]), (0, 0x3F, 0x02, 0x07, 0x80));
        // crc = xor of report bytes 2..87 (excludes status and transaction id)
        assert_eq!(r[89], 0x02 ^ 0x07 ^ 0x80);
        assert_eq!(r[89], r[3..89].iter().fold(0, |c, b| c ^ b));
    }

    #[test]
    fn crc_covers_arguments_region_only() {
        let mut r = build_request(0x1F, 0x07, 0x84, 0x02);
        // bytes 0 (status) and 1 (tid) of the report are excluded: tid at r[2] must not be in crc
        let expect = 0x02 ^ 0x07 ^ 0x84;
        assert_eq!(r[89], expect);
        r[2] = 0;
        assert_eq!(r[89], expect);
    }

    #[test]
    fn battery_scaled_from_0_255() {
        let mut m = device(0x1F, 255, 0);
        assert_eq!(probe(&mut m, false), Some(Probe::Online { percent: 100, charging: Some(false) }));
        let mut m = device(0x1F, 128, 1);
        assert_eq!(probe(&mut m, false), Some(Probe::Online { percent: 50, charging: Some(true) }));
        let mut m = device(0x1F, 0, 0);
        assert_eq!(probe(&mut m, false), Some(Probe::Online { percent: 0, charging: Some(false) }));
    }

    #[test]
    fn tries_transaction_ids_in_turn() {
        let mut m = device(0x08, 200, 0);
        assert!(matches!(probe(&mut m, false), Some(Probe::Online { .. })));
        let tids: Vec<u8> = m.sent.iter().map(|s| s[2]).collect();
        assert_eq!(&tids[..4], &[0x1F, 0x3F, 0xFF, 0x08]); // stops at the one that works
        // charging query reuses the working id
        assert_eq!(*tids.last().unwrap(), 0x08);
        assert_eq!(m.sent.last().unwrap()[8], 0x84);
    }

    #[test]
    fn offline_status_means_disconnected() {
        let mut m = Mock::new(|_, _, _| Some((0x04, vec![])));
        assert_eq!(probe(&mut m, false), Some(Probe::Offline));
        // offline only for some tids still ends up offline when no tid succeeds
        let mut m = Mock::new(|tid, _, _| Some((if tid == 0xFF { 0x04 } else { 0x03 }, vec![])));
        assert_eq!(probe(&mut m, false), Some(Probe::Offline));
    }

    #[test]
    fn online_beats_earlier_offline() {
        let mut m = Mock::new(|tid, cls, id| match (tid, cls, id) {
            (0x1F, _, _) => Some((0x04, vec![])),
            (_, 0x07, 0x80) => Some((0x02, vec![0, 51])),
            _ => Some((0x02, vec![0, 0])),
        });
        assert_eq!(probe(&mut m, false), Some(Probe::Online { percent: 20, charging: Some(false) }));
    }

    #[test]
    fn unsupported_or_silent_device_is_skipped() {
        assert_eq!(probe(&mut Mock::new(|_, _, _| Some((0x05, vec![]))), false), None);
        assert_eq!(probe(&mut Mock::new(|_, _, _| None), false), None);
        let mut m = device(0x1F, 10, 0);
        m.accept_set = false;
        assert_eq!(probe(&mut m, false), None);
    }

    #[test]
    fn mismatched_command_echo_is_no_reply() {
        struct Wrong;
        impl FeatureIo for Wrong {
            fn set_feature(&mut self, _: &[u8]) -> bool {
                true
            }
            fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> {
                let mut r = vec![0u8; 91];
                r[1] = 0x02;
                r[7] = 0x01; // replies to some other command
                r[8] = 0x01;
                Some(r)
            }
        }
        assert_eq!(command(&mut Wrong, 0x1F, 0x07, 0x80, 2, false).0, Rz::NoReply);
    }

    #[test]
    fn busy_replies_are_retried() {
        let mut m = device(0x1F, 255, 0);
        m.busy_first = 3;
        let (r, args) = command(&mut m, 0x1F, 0x07, 0x80, 2, false);
        assert_eq!(r, Rz::Ok);
        assert_eq!(args[1], 255);
        assert_eq!(args.len(), 80);
        // always busy: gives up after 8 attempts
        let mut m = device(0x1F, 255, 0);
        m.busy_first = 100;
        assert_eq!(command(&mut m, 0x1F, 0x07, 0x80, 2, false).0, Rz::NoReply);
        assert_eq!(m.busy_first, 92);
    }

    #[test]
    fn short_reply_is_no_reply() {
        struct Short;
        impl FeatureIo for Short {
            fn set_feature(&mut self, _: &[u8]) -> bool {
                true
            }
            fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> {
                Some(vec![0u8; 20])
            }
        }
        assert_eq!(command(&mut Short, 1, 7, 0x80, 2, false).0, Rz::NoReply);
    }

    #[test]
    fn name_cleanup() {
        assert_eq!(clean_name("Razer Viper Ultimate Dongle"), "Razer Viper Ultimate");
        assert_eq!(clean_name("Razer DeathAdder V3 Pro (Wired)"), "Razer DeathAdder V3 Pro");
        assert_eq!(clean_name("Razer HyperSpeed Dongle"), "Razer HyperSpeed"); // " Dongle" matches first
        assert_eq!(clean_name("Razer Basilisk Wireless Receiver"), "Razer Basilisk");
        assert_eq!(clean_name(""), "Razer device");
        assert_eq!(clean_name(" Receiver"), " Receiver"); // never strips down to nothing
    }

    #[test]
    fn model_tables_decide_kind() {
        assert_eq!(kind_of(&[], 0x1532, 0x0221), DeviceKind::Keyboard);
        assert_eq!(kind_of(&[], 0x1532, 0x0A24), DeviceKind::Keyboard);
        assert_eq!(kind_of(&[], 0x1532, 0x007A), DeviceKind::Mouse);
        assert_eq!(kind_of(&[], 0x1532, 0x9999), DeviceKind::Other);
        assert_eq!((KEYBOARD_PIDS.len(), MOUSE_PIDS.len()), (125, 121));
        assert!(KEYBOARD_PIDS.iter().all(|p| !MOUSE_PIDS.contains(p)));
    }
}

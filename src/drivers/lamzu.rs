//! LAMZU wireless mice on their own vendor ids 373E and 37B0 (Maya X, Maya, Tachi, Inca, Paro, Thorn,
//! Atlantis Mini/OG Champion, Orcus ...). The Atlantis on Compx 3554 uses the Kysona driver instead.
//! Protocol from LAMZU's Aurora web driver (lamzu.net, "IsNewProtocol" models):
//!   SET_FEATURE report 0, 64 bytes: [00][00][02][02][00][83][00 ...]
//!   ~100 ms later GET_FEATURE report 0: [A1][..][..][02][..][83][charging][percent] after the report
//!   id byte (the web driver accepts the reply with or without that byte, so we do too).
//!   charging 1 = on the cable; percent 0 means the mouse is not linked to the dongle.
//! Model ids from lamzu.net/Config/env-models.json. Several newer models share one dongle id, so those
//! dongles get a generic name.

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, sleep_ms, FeatureIo, HidDev};

struct Model {
    vid: u16,
    pids: [u16; 3],
    name: &'static str,
}

const fn m(vid: u16, pids: [u16; 3], name: &'static str) -> Model {
    Model { vid, pids, name }
}

const MODELS: &[Model] = &[
    m(0x373E, [0x001C, 0x001D, 0x001E], "LAMZU Maya X"),
    m(0x373E, [0x0016, 0, 0], "LAMZU Maya X V2"),
    m(0x37B0, [0x001F, 0, 0], "LAMZU Maya X V2"),
    m(0x37B0, [0x0005, 0x000B, 0x000C], "LAMZU Tachi"),
    m(0x37B0, [0x001C, 0x001E, 0], "LAMZU Tachi Lite"),
    m(0x37B0, [0x0009, 0x000F, 0x0010], "LAMZU Inca"),
    m(0x37B0, [0x0011, 0x0013, 0x0015], "LAMZU Maya"),
    m(0x37B0, [0x0007, 0x000D, 0x000E], "LAMZU Paro"),
    m(0x37B0, [0x0017, 0x0019, 0x001B], "LAMZU Thorn"),
    m(0x37B0, [0x0021, 0x0003, 0x0023], "LAMZU Thorn V2"),
    m(0x37B0, [0x0030, 0x0032, 0x0040], "LAMZU Thorn V2"),
    m(0x37B0, [0x0025, 0x0027, 0], "LAMZU Atlantis OG Champion"),
    m(0x37B0, [0x0028, 0x002B, 0x002A], "LAMZU Atlantis Mini"),
    m(0x37B0, [0x003E, 0x003F, 0], "LAMZU Atlantis Mini"),
    m(0x37B0, [0x002C, 0x0052, 0], "LAMZU Maya M"),
    m(0x37B0, [0x003A, 0x003B, 0x0044], "LAMZU Maya X"),
    m(0x37B0, [0x003C, 0x0050, 0], "LAMZU Maya"),
    m(0x37B0, [0x0042, 0x0056, 0], "LAMZU Orcus"),
    m(0x37B0, [0x0046, 0x0054, 0], "LAMZU Mini"),
];

fn model_name(vid: u16, pid: u16) -> Option<&'static str> {
    MODELS
        .iter()
        .filter(|m| m.vid == vid)
        .find(|m| m.pids.iter().any(|&p| p != 0 && p == pid))
        .map(|m| m.name)
}

/// Wired product ids: the mouse itself on the USB cable.
fn is_cable(vid: u16, pid: u16) -> bool {
    if vid == 0x373E {
        return pid == 0x001C;
    }
    matches!(
        pid,
        0x0005 | 0x0007 | 0x0009 | 0x0011 | 0x0017 | 0x001C | 0x001F | 0x0021 | 0x0025 | 0x0028 | 0x002C | 0x0030
            | 0x0034 | 0x0038 | 0x003A | 0x003C | 0x003E | 0x0040 | 0x0042 | 0x0044 | 0x0046 | 0x0050 | 0x0052
            | 0x0056
    )
}

fn request() -> [u8; 65] {
    let mut req = [0u8; 65]; // [0] = report id 0
    req[3] = 0x02;
    req[4] = 0x02;
    req[6] = 0x83;
    req
}

/// Decodes a reply into (charging byte, percent), accepting it with or without the report id byte.
fn decode(r: &[u8]) -> Option<(u8, u8)> {
    if r.len() < 10 {
        return None;
    }
    if r[1] == 0xA1 && r[4] == 0x02 && r[6] == 0x83 {
        Some((r[7], r[8]))
    } else if r[0] == 0xA1 && r[3] == 0x02 && r[5] == 0x83 {
        Some((r[6], r[7]))
    } else {
        None
    }
}

/// One request/reply exchange; `None` when the device does not speak this protocol or the
/// percent is out of range.
fn query(io: &mut impl FeatureIo, wait_ms: u64) -> Option<(u8, u8)> {
    if !io.set_feature(&request()) {
        return None;
    }
    sleep_ms(wait_ms);
    let r = io.get_feature(0x00, 65)?;
    let (chg, pct) = decode(&r)?;
    (pct <= 100).then_some((chg, pct))
}

fn build_status(vid: u16, pid: u16, chg: u8, pct: u8) -> DeviceStatus {
    let cable = is_cable(vid, pid);
    DeviceStatus {
        key: format!("lamzu:{}:{}", hex4(vid), hex4(pid)),
        name: model_name(vid, pid).unwrap_or("LAMZU wireless mouse").to_string(),
        kind: DeviceKind::Mouse,
        via: if cable { "USB cable" } else { "2.4 GHz receiver" }.into(),
        connected: pct > 0 || cable, // 0 from a dongle = mouse off or out of range
        percent: (pct > 0).then_some(pct),
        charging: chg == 1,
        ..Default::default()
    }
}

pub fn poll(ctx: &mut PollContext) {
    for vendor in [0x373Eu16, 0x37B0] {
        for (vid, pid) in ctx.products(vendor) {
            for c in ctx.find(vid, pid, |c| c.feat_len == 65) {
                let Some(mut d) = HidDev::open_feature_only(ctx.api, &c) else { continue };
                let Some((chg, pct)) = query(&mut d, 100) else { continue };
                ctx.add(build_status(vid, pid, chg, pct), vid, pid);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock {
        reply: Option<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        accept: bool,
    }
    impl FeatureIo for Mock {
        fn set_feature(&mut self, d: &[u8]) -> bool {
            self.sent.push(d.to_vec());
            self.accept
        }
        fn get_feature(&mut self, _: u8, _: usize) -> Option<Vec<u8>> {
            self.reply.clone()
        }
    }
    fn mock(reply: Option<Vec<u8>>) -> Mock {
        Mock { reply, sent: vec![], accept: true }
    }
    /// Reply with the report id byte present: [00][A1][..][..][02][..][83][chg][pct]
    fn with_id(chg: u8, pct: u8) -> Vec<u8> {
        let mut r = vec![0u8; 65];
        r[1] = 0xA1;
        r[4] = 0x02;
        r[6] = 0x83;
        r[7] = chg;
        r[8] = pct;
        r
    }
    /// Reply without it: [A1][..][..][02][..][83][chg][pct]
    fn without_id(chg: u8, pct: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0xA1;
        r[3] = 0x02;
        r[5] = 0x83;
        r[6] = chg;
        r[7] = pct;
        r
    }

    #[test]
    fn request_layout() {
        let mut m = mock(Some(with_id(0, 50)));
        query(&mut m, 0).unwrap();
        let w = &m.sent[0];
        assert_eq!(w.len(), 65);
        assert_eq!(&w[..7], &[0, 0, 0, 0x02, 0x02, 0, 0x83]); // report id + [00][00][02][02][00][83]
        assert!(w[7..].iter().all(|&b| b == 0) && w[0] == 0);
        assert_eq!((w[3], w[4], w[6]), (0x02, 0x02, 0x83));
    }

    #[test]
    fn both_reply_layouts_decode() {
        assert_eq!(query(&mut mock(Some(with_id(1, 88))), 0), Some((1, 88)));
        assert_eq!(query(&mut mock(Some(without_id(0, 12))), 0), Some((0, 12)));
    }

    #[test]
    fn rejects_foreign_short_or_out_of_range() {
        assert_eq!(query(&mut mock(Some(with_id(0, 101))), 0), None);
        assert_eq!(query(&mut mock(Some(vec![0u8; 65])), 0), None);
        assert_eq!(query(&mut mock(Some(vec![0xA1; 9])), 0), None);
        assert_eq!(query(&mut mock(None), 0), None);
        let mut m = mock(Some(with_id(0, 50)));
        m.accept = false;
        assert_eq!(query(&mut m, 0), None);
    }

    #[test]
    fn dongle_with_mouse_off_is_disconnected() {
        let s = build_status(0x37B0, 0x000B, 0, 0); // Tachi dongle
        assert!(!s.connected);
        assert_eq!(s.percent, None);
        assert_eq!(s.via, "2.4 GHz receiver");
        assert_eq!(s.key, "lamzu:37B0:000B");
    }

    #[test]
    fn cable_with_zero_percent_stays_connected() {
        let s = build_status(0x37B0, 0x0005, 1, 0);
        assert!(s.connected && s.charging);
        assert_eq!(s.via, "USB cable");
        assert_eq!(s.percent, None);
    }

    #[test]
    fn percent_and_charging() {
        let s = build_status(0x373E, 0x001D, 1, 77);
        assert_eq!((s.percent, s.charging), (Some(77), true));
        let s = build_status(0x373E, 0x001D, 2, 77);
        assert!(!s.charging); // only 1 means charging
    }

    #[test]
    fn model_and_cable_tables() {
        assert_eq!(model_name(0x373E, 0x001C), Some("LAMZU Maya X"));
        assert_eq!(model_name(0x37B0, 0x001C), Some("LAMZU Tachi Lite"));
        assert_eq!(model_name(0x373E, 0x0016), Some("LAMZU Maya X V2"));
        assert_eq!(model_name(0x37B0, 0x0003), Some("LAMZU Thorn V2"));
        assert_eq!(model_name(0x37B0, 0x0046), Some("LAMZU Mini"));
        assert_eq!(model_name(0x37B0, 0), None); // zero padding is not a product id
        assert_eq!(model_name(0x37B0, 0x9999), None);
        assert_eq!(build_status(0x37B0, 0x9999, 0, 10).name, "LAMZU wireless mouse");
        assert!(is_cable(0x373E, 0x001C) && !is_cable(0x373E, 0x001D) && !is_cable(0x373E, 0x0005));
        assert!(is_cable(0x37B0, 0x0056) && !is_cable(0x37B0, 0x000B));
    }
}

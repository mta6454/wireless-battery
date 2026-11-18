//! Cooler Master wireless mice (VID 2516): MM712 (cable 0169) and MM731 (cable 0167). Both pair with
//! the same "Cooler Master Mice Dongle" (01B7). Vendor interface 3, usage page FF0A, no report id;
//! 32-byte reports on the dongle, 64-byte reports over the cable.
//!   Request: [46][81][00] -> status report [47][81][model] ... [10] percent, [11] charging (0 = no),
//!            [12] link (0 cable, 1 dongle). The mouse also sends it on its own when the level changes.
//!   [2] tells the models apart on the shared dongle: 50 = MM712; 70 seen in an MM731 status report.
//!   A sleeping or switched-off mouse does not answer; the last value is kept (graceSec).
//! MM712 verified on hardware (2516:01B7). MM731: same protocol family per MM731-Controller; its battery
//! bytes and model byte are not verified.
//! Protocol: github.com/syntactic-spice/coolermaster-usb-protocol (docs/PROTOCOL.md) and
//! github.com/riVFerd/MM731-Controller.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidColl, HidDev, HidIo};

const DONGLE: u16 = 0x01B7;
const MM712_CABLE: u16 = 0x0169;
const MM731_CABLE: u16 = 0x0167;

pub fn model_name(pid: u16, model: u8) -> &'static str {
    if pid == MM712_CABLE || (pid == DONGLE && model == 0x50) {
        return "Cooler Master MM712";
    }
    if pid == MM731_CABLE || (pid == DONGLE && model == 0x70) {
        return "Cooler Master MM731";
    }
    "Cooler Master wireless mouse"
}

/// Sends the status request and waits for the 47 81 reply.
pub fn query_status(d: &mut impl HidIo) -> Option<Vec<u8>> {
    d.flush();
    if !d.write(&[0x00, 0x46, 0x81, 0x00]) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(500);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = d.read(left).filter(|r| r.len() >= 13)?;
        if r[0] == 0x47 && r[1] == 0x81 {
            return Some(r);
        }
    }
}

/// Builds the status from a 47 81 report (at least 13 bytes).
pub fn status_from_report(pid: u16, r: &[u8]) -> DeviceStatus {
    let mut s = DeviceStatus {
        key: format!("coolermaster:{}", hex4(pid)),
        name: model_name(pid, r[2]).to_string(),
        kind: DeviceKind::Mouse,
        via: if r[12] == 0 { "USB cable" } else { "2.4 GHz receiver" }.into(),
        charging: r[11] != 0,
        grace_sec: 1800,
        ..Default::default()
    };
    if (1..=100).contains(&r[10]) {
        s.percent = Some(r[10]);
    }
    s
}

/// With the cable and the dongle both plugged in the mouse shows up twice; keep one. Returns true
/// when `s` was merged into an already reported entry (charging is OR-ed in).
pub fn merge_duplicate(out: &mut [DeviceStatus], s: &DeviceStatus) -> bool {
    let mut dup = false;
    for o in out.iter_mut().filter(|o| o.name == s.name) {
        dup = true;
        o.charging |= s.charging;
    }
    dup
}

/// Vendor interface 3 (unknown on macOS, where the interface number is not reported).
fn status_collection(c: &HidColl) -> bool {
    (c.iface == 3 || c.iface < 0) && c.usage_page == 0xFF0A
}

pub fn poll(ctx: &mut PollContext) {
    // sorted: the cable ids come before the dongle
    for (vid, pid) in ctx.products(0x2516) {
        if pid != DONGLE && pid != MM712_CABLE && pid != MM731_CABLE {
            continue;
        }
        for c in ctx.find(vid, pid, status_collection) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some(r) = query_status(&mut d) else { continue };
            let s = status_from_report(pid, &r);
            if merge_duplicate(&mut ctx.out, &s) {
                ctx.claim(vid, pid);
            } else {
                ctx.add(s, vid, pid);
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;

    struct Mock {
        /// Reports returned after the (single) request.
        replies: Vec<Vec<u8>>,
        queue: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        accept_write: bool,
    }

    impl Mock {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Self { replies, queue: vec![], sent: vec![], accept_write: true }
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
            self.queue = self.replies.clone();
            self.accept_write
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            if self.queue.is_empty() { None } else { Some(self.queue.remove(0)) }
        }
        fn flush(&mut self) {
            self.queue.clear();
        }
    }

    fn report(model: u8, pct: u8, charging: u8, link: u8) -> Vec<u8> {
        let mut r = vec![0u8; 32];
        r[0] = 0x47;
        r[1] = 0x81;
        r[2] = model;
        r[10] = pct;
        r[11] = charging;
        r[12] = link;
        r
    }

    #[test]
    fn request_and_reply_decoding() {
        let mut m = Mock::new(vec![report(0x50, 87, 0, 1)]);
        let r = query_status(&mut m).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0x46, 0x81, 0x00]);
        let s = status_from_report(DONGLE, &r);
        assert_eq!(s.key, "coolermaster:01B7");
        assert_eq!((s.name.as_str(), s.via.as_str(), s.percent, s.charging), ("Cooler Master MM712", "2.4 GHz receiver", Some(87), false));
        assert_eq!((s.kind, s.grace_sec, s.connected), (DeviceKind::Mouse, 1800, true));
    }

    #[test]
    fn cable_and_charging() {
        let s = status_from_report(MM731_CABLE, &report(0x00, 40, 1, 0));
        assert_eq!((s.name.as_str(), s.via.as_str(), s.charging), ("Cooler Master MM731", "USB cable", true));
    }

    #[test]
    fn percent_out_of_range_is_unknown() {
        assert_eq!(status_from_report(DONGLE, &report(0x50, 0, 0, 1)).percent, None);
        assert_eq!(status_from_report(DONGLE, &report(0x50, 101, 0, 1)).percent, None);
        assert_eq!(status_from_report(DONGLE, &report(0x50, 100, 0, 1)).percent, Some(100));
        assert_eq!(status_from_report(DONGLE, &report(0x50, 1, 0, 1)).percent, Some(1));
    }

    #[test]
    fn model_names() {
        assert_eq!(model_name(DONGLE, 0x50), "Cooler Master MM712");
        assert_eq!(model_name(DONGLE, 0x70), "Cooler Master MM731");
        assert_eq!(model_name(DONGLE, 0x00), "Cooler Master wireless mouse");
        assert_eq!(model_name(MM712_CABLE, 0x70), "Cooler Master MM712"); // cable id wins
        assert_eq!(model_name(MM731_CABLE, 0x50), "Cooler Master MM731");
    }

    #[test]
    fn query_skips_other_reports_and_rejects_short_ones() {
        let mut other = report(0x50, 10, 0, 1);
        other[0] = 0x48; // not a 47 81 status
        let mut m = Mock::new(vec![other, report(0x50, 66, 0, 1)]);
        assert_eq!(query_status(&mut m).unwrap()[10], 66);
        // a short report aborts the query
        assert!(query_status(&mut Mock::new(vec![vec![0x47, 0x81, 0x50]])).is_none());
        // sleeping mouse: no answer
        assert!(query_status(&mut Mock::new(vec![])).is_none());
        let mut m = Mock::new(vec![report(0x50, 10, 0, 1)]);
        m.accept_write = false;
        assert!(query_status(&mut m).is_none());
    }

    #[test]
    fn cable_and_dongle_duplicates_are_merged() {
        let mut out = vec![status_from_report(MM712_CABLE, &report(0x50, 90, 0, 0))];
        let dongle = status_from_report(DONGLE, &report(0x50, 90, 1, 1));
        assert!(merge_duplicate(&mut out, &dongle));
        assert_eq!(out.len(), 1);
        assert!(out[0].charging);
        // a different model is not a duplicate
        let other = status_from_report(DONGLE, &report(0x70, 50, 0, 1));
        assert!(!merge_duplicate(&mut out, &other));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn collection_filter() {
        let mk = |iface, page| HidColl {
            path: Default::default(),
            vid: 0x2516,
            pid: DONGLE,
            iface,
            usage_page: page,
            usage: 1,
            product: String::new(),
            in_len: 0,
            out_len: 0,
            feat_len: 0,
        };
        assert!(status_collection(&mk(3, 0xFF0A)) && status_collection(&mk(-1, 0xFF0A)));
        assert!(!status_collection(&mk(2, 0xFF0A)) && !status_collection(&mk(3, 0xFF00)));
    }
}

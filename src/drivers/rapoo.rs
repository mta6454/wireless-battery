//! Rapoo VT gaming mice. Battery is pushed periodically; no commands or handshake are needed.
//! Sources:
//!  * Official Rapoo Hub (https://hub.rapoo.cn/), VT_nrf54L input-report decoder
//!    and hardware manifests: report 07 on FF00:0002, state at byte 7, percent at byte 8,
//!    optional mouse PID at 13..14. Connection is the low nibble of byte 1 (0 RF / 1 BT / 2 USB).
//!  * https://github.com/PyFlat/Device-Battery-Info (RapooProtocol.cs): older 7-byte BB B0
//!    reports on FF00:0002, state at byte 5, percent at byte 6; charging can report 7F (unknown).
//! Legacy BB semantics are restricted to VT3 Pro 1215/4415; other Telink firmware differs.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidColl, HidDev, HidIo};

/// Passive Rapoo status reports (port of rapoo_protocol.h). Frames include the report ID
/// (hidapi convention); independent of any device I/O so captured frames can be checked directly.
mod protocol {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum Connection {
        #[default]
        Unknown,
        Receiver,
        Bluetooth,
        Cable,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub enum Profile {
        #[default]
        Nordic54L,
        LegacyVT3Pro,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct Battery {
        /// `None` = unknown (never invented as 100 %)
        pub percent: Option<u8>,
        pub charging: bool,
        pub connection: Connection,
        pub mouse_pid: u16,
    }

    pub fn decode_battery(data: &[u8], profile: Profile) -> Option<Battery> {
        if data.is_empty() {
            return None;
        }
        let size = data.len();
        let mut result = Battery::default();
        let (state, percent);
        if profile == Profile::Nordic54L && data[0] == 0x07 {
            // Nordic 54L: mode, DPI gear, X/Y DPI, battery state, percentage.
            if size < 9 || (data[1] & 0x0F) > 2 {
                return None;
            }
            result.connection = match data[1] & 0x0F {
                0 => Connection::Receiver,
                1 => Connection::Bluetooth,
                _ => Connection::Cable,
            };
            state = data[7];
            percent = data[8];
            if size >= 15 {
                result.mouse_pid = data[13] as u16 | ((data[14] as u16) << 8);
            }
        } else if profile == Profile::LegacyVT3Pro && data[0] == 0xBB {
            // VT3 Pro firmware: BB B0 mode DPI-lo DPI-hi battery-state percentage.
            if size < 7 || data[1] != 0xB0 {
                return None;
            }
            state = data[5];
            percent = data[6];
        } else {
            return None;
        }
        // 0 = invalid/transition. Some older firmware uses 7F instead of a level while charging.
        if state != 1 && state != 2 {
            return None;
        }
        if percent > 100 && !(profile == Profile::LegacyVT3Pro && state == 2 && percent == 0x7F) {
            return None;
        }
        result.charging = state == 2;
        if percent <= 100 {
            result.percent = Some(percent);
        }
        Some(result)
    }
}

use protocol::{Battery, Connection, Profile};

// Selected names from Rapoo's manifests. Support is determined by the collection and valid
// status frame, so other models on the same protocol work without extending this table.
struct Model {
    cable: u16,
    receiver: u16,
    name: &'static str,
}

const MODELS: [Model; 16] = [
    Model { cable: 0x4610, receiver: 0x1410, name: "Rapoo VT0" },
    Model { cable: 0x4620, receiver: 0x1420, name: "Rapoo VT0 Max" },
    Model { cable: 0x4615, receiver: 0x1415, name: "Rapoo VT1" },
    Model { cable: 0x4644, receiver: 0x1444, name: "Rapoo VT1 Max" },
    Model { cable: 0x4622, receiver: 0x1422, name: "Rapoo VT2" },
    Model { cable: 0x4623, receiver: 0x1423, name: "Rapoo VT2 Max" },
    Model { cable: 0x4612, receiver: 0x1412, name: "Rapoo VT3" },
    Model { cable: 0x4617, receiver: 0x1417, name: "Rapoo VT3 Max (Gen 2)" },
    Model { cable: 0x4B06, receiver: 0x1B06, name: "Rapoo VT3 Max V2" },
    Model { cable: 0x4611, receiver: 0x1411, name: "Rapoo VT3S" },
    Model { cable: 0x4621, receiver: 0x1421, name: "Rapoo VT3S Max" },
    Model { cable: 0x4613, receiver: 0x1413, name: "Rapoo VT7" },
    Model { cable: 0x4616, receiver: 0x1416, name: "Rapoo VT7 Max" },
    Model { cable: 0x4614, receiver: 0x1414, name: "Rapoo VT9" },
    Model { cable: 0x4618, receiver: 0x1418, name: "Rapoo VT9 Max" },
    Model { cable: 0x4415, receiver: 0x1215, name: "Rapoo VT3 Pro" },
];

fn find_model(pid: u16) -> Option<&'static Model> {
    MODELS.iter().find(|m| pid == m.cable || pid == m.receiver)
}

fn is_legacy_vt3_pro(pid: u16) -> bool {
    pid == 0x1215 || pid == 0x4415
}

const VID: u16 = 0x24AE;

fn read_battery(io: &mut (impl HidIo + ?Sized), profile: Profile) -> Option<Battery> {
    // A passive legacy report can take ~3.2 seconds. One deadline includes unrelated reports;
    // do not flush, since a queued status report is already useful.
    let end = Instant::now() + Duration::from_millis(4000);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let r = io.read(left.as_millis() as i32)?;
        if let Some(b) = protocol::decode_battery(&r, profile) {
            return Some(b);
        }
        if Instant::now() >= end {
            return None;
        }
    }
}

/// Collections that carry the battery reports; the other vendor collections are for
/// configuration/firmware.
fn is_battery_coll(c: &HidColl, pid: u16) -> bool {
    c.usage_page == 0xFF00 && c.usage == 0x0002 && (c.in_len == 19 || (c.in_len == 7 && is_legacy_vt3_pro(pid)))
}

/// Core of the poll, independent of hidapi so it can be driven by a simulated transport.
/// Appends to `out` and records every (vid, pid) that must be claimed in `claim`.
fn poll_with<'a>(
    colls: &[HidColl],
    claimed: &BTreeSet<(u16, u16)>,
    out: &mut Vec<DeviceStatus>,
    claim: &mut Vec<(u16, u16)>,
    mut open: impl FnMut(&HidColl) -> Option<Box<dyn HidIo + 'a>>,
) {
    let products: BTreeSet<(u16, u16)> =
        colls.iter().filter(|c| c.vid == VID && !claimed.contains(&(c.vid, c.pid))).map(|c| (c.vid, c.pid)).collect();
    for (vid, pid) in products {
        for c in colls.iter().filter(|c| c.vid == vid && c.pid == pid && is_battery_coll(c, pid)) {
            let Some(mut d) = open(c) else { continue };
            let profile = if c.in_len == 7 { Profile::LegacyVT3Pro } else { Profile::Nordic54L };
            let Some(battery) = read_battery(&mut *d, profile) else { continue };

            let model = find_model(if battery.mouse_pid != 0 { battery.mouse_pid } else { pid });
            let identity = match model {
                Some(m) => m.cable,
                None if battery.mouse_pid != 0 => battery.mouse_pid,
                None => pid,
            };
            let mut s = DeviceStatus { key: format!("rapoo:{}", hex4(identity)), ..Default::default() };
            s.name = model.map_or_else(|| c.product.clone(), |m| m.name.to_string());
            if s.name.is_empty() || s.name == "Rapoo Gaming Device" {
                s.name = "Rapoo wireless mouse".into();
            }
            s.kind = DeviceKind::Mouse;
            s.via = if battery.connection == Connection::Cable
                || (battery.connection == Connection::Unknown && model.is_some_and(|m| pid == m.cable))
            {
                "USB cable"
            } else if battery.connection == Connection::Bluetooth {
                "Bluetooth"
            } else {
                "2.4 GHz receiver"
            }
            .into();
            s.percent = battery.percent;
            s.charging = battery.charging;
            s.grace_sec = 1800; // existing UI keeps the last reading, dimmed, while the mouse sleeps

            // A charging mouse can report through both cable and receiver. Keep a single entry,
            // preferring the cable's state but keeping any available percentage. Claim both
            // products before generic fallbacks.
            match out.iter_mut().find(|o| o.key == s.key) {
                None => {
                    out.push(s);
                    claim.push((vid, pid));
                }
                Some(existing) => {
                    if s.via == "USB cable" {
                        let receiver_percent = existing.percent;
                        *existing = s;
                        if existing.percent.is_none() {
                            existing.percent = receiver_percent;
                        }
                    } else if existing.percent.is_none() && s.percent.is_some() {
                        existing.percent = s.percent;
                    }
                    claim.push((vid, pid));
                }
            }
            break;
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    let claimed: BTreeSet<(u16, u16)> =
        ctx.colls.iter().map(|c| (c.vid, c.pid)).filter(|&(v, p)| ctx.is_claimed(v, p)).collect();
    let api = ctx.api;
    let mut claim = Vec::new();
    poll_with(&ctx.colls, &claimed, &mut ctx.out, &mut claim, |c| {
        HidDev::open(api, c).map(|d| Box::new(d) as Box<dyn HidIo>)
    });
    for (v, p) in claim {
        ctx.claim(v, p);
    }
}

#[cfg(test)]
mod tests {
    use super::protocol::*;
    use super::*;
    use crate::hid::FeatureIo;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::rc::Rc;

    // ---- protocol tests (port of rapoo_protocol_test.cpp) ----

    fn nordic(d: &[u8]) -> Option<Battery> {
        decode_battery(d, Profile::Nordic54L)
    }
    fn legacy(d: &[u8]) -> Option<Battery> {
        decode_battery(d, Profile::LegacyVT3Pro)
    }

    // Captured on the user's VT3 Max Gen 2, receiver 24AE:1417, FF00:0002.
    const CAPTURED: [u8; 19] =
        [0x07, 0x20, 0x03, 0x20, 0x03, 0x20, 0x03, 0x01, 0x19, 0x00, 0x04, 0x84, 0x00, 0x17, 0x46, 0x00, 0x02, 0x03, 0xBC];
    // Also captured live after connecting the cable (before its percentage increased).
    const CAPTURED_CABLE: [u8; 19] =
        [0x07, 0x22, 0x03, 0x20, 0x03, 0x20, 0x03, 0x02, 0x19, 0x00, 0x04, 0x84, 0x00, 0x17, 0x46, 0x00, 0x02, 0x03, 0x00];

    #[test]
    fn protocol_captured_frames() {
        let b = nordic(&CAPTURED).unwrap();
        assert_eq!((b.percent, b.charging, b.mouse_pid), (Some(25), false, 0x4617));
        assert_eq!(b.connection, Connection::Receiver);
        let b = nordic(&CAPTURED_CABLE).unwrap();
        assert_eq!((b.percent, b.charging, b.mouse_pid), (Some(25), true, 0x4617));
        assert_eq!(b.connection, Connection::Cable);
    }

    #[test]
    fn protocol_modern_variations() {
        let mut m = CAPTURED;
        m[1] = 0x22; // sensor bits must not affect the connection type
        m[7] = 2;
        m[8] = 100;
        let b = nordic(&m).unwrap();
        assert!(b.charging && b.percent == Some(100) && b.connection == Connection::Cable);
        m[1] = 0x21;
        m[7] = 1;
        m[8] = 0;
        let b = nordic(&m).unwrap();
        assert!(b.percent == Some(0) && !b.charging && b.connection == Connection::Bluetooth);
        m[1] = 0x23;
        assert!(nordic(&m).is_none());
    }

    #[test]
    fn protocol_legacy_profile() {
        let mut l = [0xBB, 0xB0, 0x81, 0x20, 0x03, 1, 37];
        assert!(nordic(&l).is_none());
        assert!(legacy(&CAPTURED).is_none());
        let b = legacy(&l).unwrap();
        assert!(b.percent == Some(37) && !b.charging && b.mouse_pid == 0);
        assert_eq!(b.connection, Connection::Unknown);
        l[5] = 2;
        l[6] = 0x7F;
        let b = legacy(&l).unwrap();
        assert!(b.charging && b.percent.is_none()); // unknown must never become 100%
        l[6] = 100;
        let b = legacy(&l).unwrap();
        assert!(b.percent == Some(100) && b.charging);
        l[5] = 1;
        assert!(!legacy(&l).unwrap().charging); // a previous charging reading must not leak into this one
    }

    #[test]
    fn protocol_bounds_and_malformed() {
        for size in 0..9 {
            assert!(nordic(&CAPTURED[..size]).is_none());
        }
        let l = [0xBB, 0xB0, 0x81, 0x20, 0x03, 1, 37];
        for size in 0..7 {
            assert!(legacy(&l[..size]).is_none());
        }
        assert!(nordic(&[]).is_none());
        // Short modern reports have no optional model ID.
        assert_eq!(nordic(&CAPTURED[..9]).unwrap().mouse_pid, 0);
        let mut l = [0xBC, 0xB0, 0x81, 0x20, 0x03, 1, 50, 0, 0, 0];
        assert!(legacy(&l).is_none());
        l[0] = 0xBB;
        l[1] = 0;
        assert!(legacy(&l).is_none());
        let mut m = CAPTURED_CABLE;
        m[8] = 0x7F;
        assert!(nordic(&m).is_none()); // sentinel is legacy-only
        let mut m = CAPTURED;
        m[0] = 0x08; // CH584 has a separate protocol; do not claim it as Nordic support
        assert!(nordic(&m).is_none());
    }

    #[test]
    fn protocol_invalid_states_and_percentages() {
        let mut l = [0xBB, 0xB0, 0x81, 0x20, 0x03, 1, 37];
        for state in [0u8, 3, 255] {
            let mut m = CAPTURED;
            m[7] = state;
            assert!(nordic(&m).is_none());
            l[5] = state;
            assert!(legacy(&l).is_none());
        }
        for percent in [101u8, 126, 127, 128, 255] {
            let mut m = CAPTURED;
            m[8] = percent;
            assert!(nordic(&m).is_none());
            l[5] = 1;
            l[6] = percent;
            assert!(legacy(&l).is_none());
        }
    }

    // ---- driver tests (port of rapoo_driver_test.cpp) with a simulated transport ----

    type Queues = Rc<RefCell<HashMap<Vec<u8>, Vec<Vec<u8>>>>>;

    struct SimDev {
        path: Vec<u8>,
        queues: Queues,
    }

    impl FeatureIo for SimDev {
        fn set_feature(&mut self, _d: &[u8]) -> bool {
            panic!("rapoo must never write");
        }
        fn get_feature(&mut self, _id: u8, _len: usize) -> Option<Vec<u8>> {
            panic!("rapoo must never query");
        }
    }

    impl HidIo for SimDev {
        fn write(&mut self, _d: &[u8]) -> bool {
            panic!("rapoo must never write");
        }
        fn read(&mut self, timeout_ms: i32) -> Option<Vec<u8>> {
            assert!(timeout_ms > 0 && timeout_ms <= 4000);
            let mut q = self.queues.borrow_mut();
            let queue = q.get_mut(&self.path)?;
            if queue.is_empty() { None } else { Some(queue.remove(0)) }
        }
        fn flush(&mut self) {
            panic!("rapoo must not flush");
        }
    }

    fn collection(pid: u16, path: &str, length: u16) -> HidColl {
        HidColl {
            path: CString::new(path).unwrap(),
            vid: VID,
            pid,
            iface: -1,
            usage_page: 0xFF00,
            usage: 2,
            product: String::new(),
            in_len: length,
            out_len: 0,
            feat_len: 0,
        }
    }

    fn status(cable: bool, percent: u8) -> Vec<u8> {
        vec![
            7, if cable { 0x22 } else { 0x20 }, 3, 0x20, 3, 0x20, 3, if cable { 2 } else { 1 }, percent, 0, 4, 0x84, 0, 0x17,
            0x46, 0, 2, 3, 0xBC,
        ]
    }

    struct Run {
        out: Vec<DeviceStatus>,
        claim: Vec<(u16, u16)>,
        opens: u32,
    }

    /// Runs the driver over `colls` with the given per-path report queues and prior state.
    fn run(colls: &[HidColl], reports: &[(&str, Vec<Vec<u8>>)], claimed: &[(u16, u16)], out: Vec<DeviceStatus>) -> Run {
        let queues: Queues =
            Rc::new(RefCell::new(reports.iter().map(|(p, q)| (p.as_bytes().to_vec(), q.clone())).collect()));
        let claimed: BTreeSet<_> = claimed.iter().copied().collect();
        let mut r = Run { out, claim: vec![], opens: 0 };
        let opens = Rc::new(RefCell::new(0u32));
        let o2 = opens.clone();
        poll_with(colls, &claimed, &mut r.out, &mut r.claim, |c| {
            *o2.borrow_mut() += 1;
            Some(Box::new(SimDev { path: c.path.as_bytes().to_vec(), queues: queues.clone() }) as Box<dyn HidIo>)
        });
        r.opens = *opens.borrow();
        r
    }

    #[test]
    fn driver_cable_wins_over_receiver_and_ignores_unrelated_packet() {
        let receiver = collection(0x1417, "receiver", 19);
        let cable = collection(0x4617, "cable", 19);
        // Ignore an unrelated packet and accept the next status; cable wins over receiver.
        let r = run(
            &[receiver, cable],
            &[("receiver", vec![vec![0xBC, 0], status(false, 25)]), ("cable", vec![status(true, 27)])],
            &[],
            vec![],
        );
        assert_eq!(r.out.len(), 1);
        let s = &r.out[0];
        assert_eq!(s.key, "rapoo:4617");
        assert_eq!((s.percent, s.charging), (Some(27), true));
        assert_eq!((s.via.as_str(), s.kind, s.grace_sec), ("USB cable", DeviceKind::Mouse, 1800));
        assert_eq!(s.name, "Rapoo VT3 Max (Gen 2)");
        assert!(r.claim.contains(&(VID, 0x1417)) && r.claim.contains(&(VID, 0x4617)));
    }

    #[test]
    fn driver_receiver_only_uses_same_stable_key() {
        let r = run(&[collection(0x1417, "receiver", 19)], &[("receiver", vec![status(false, 25)])], &[], vec![]);
        assert_eq!(r.out.len(), 1);
        assert_eq!(r.out[0].key, "rapoo:4617");
        assert!(!r.out[0].charging && r.out[0].percent == Some(25));
        assert_eq!(r.out[0].via, "2.4 GHz receiver");
    }

    #[test]
    fn driver_timeout_does_not_claim_or_invent_a_level() {
        let r = run(&[collection(0x1417, "receiver", 19)], &[], &[], vec![]);
        assert!(r.out.is_empty() && r.claim.is_empty());
    }

    #[test]
    fn driver_legacy_vt3_pro_cable_wins_with_receiver_percent() {
        let r = run(
            &[collection(0x1215, "legacy", 7), collection(0x4415, "legacy-cable", 7)],
            &[
                ("legacy", vec![vec![0xBB, 0xB0, 0x81, 0x20, 3, 1, 37]]),
                ("legacy-cable", vec![vec![0xBB, 0xB0, 0x81, 0x20, 3, 2, 0x7F]]),
            ],
            &[],
            vec![],
        );
        assert_eq!(r.out.len(), 1);
        assert_eq!(r.out[0].key, "rapoo:4415");
        assert!(r.out[0].charging && r.out[0].percent == Some(37));
        assert_eq!(r.out[0].via, "USB cable");
    }

    #[test]
    fn driver_unknown_cable_level_is_filled_by_later_receiver_reading() {
        // A cable-only unknown level stays unknown...
        let r = run(
            &[collection(0x4415, "legacy-cable", 7)],
            &[("legacy-cable", vec![vec![0xBB, 0xB0, 0x81, 0x20, 3, 2, 0x7F]])],
            &[],
            vec![],
        );
        assert!(r.out.len() == 1 && r.out[0].charging && r.out[0].percent.is_none());
        // ...a later receiver reading fills it without clearing the cable's charging state.
        let r = run(
            &[collection(0x1215, "legacy", 7)],
            &[("legacy", vec![vec![0xBB, 0xB0, 0x81, 0x20, 3, 1, 37]])],
            &[],
            r.out,
        );
        assert!(r.out.len() == 1 && r.out[0].charging && r.out[0].percent == Some(37));
        assert_eq!(r.out[0].via, "USB cable");
        assert_eq!(r.claim, vec![(VID, 0x1215)]);
    }

    #[test]
    fn driver_legacy_marker_does_not_apply_to_modern_collection() {
        let r = run(
            &[collection(0x1417, "receiver", 19)],
            &[("receiver", vec![vec![0xBB, 0xB0, 0x81, 0x20, 3, 2, 50]])],
            &[],
            vec![],
        );
        assert!(r.out.is_empty() && r.claim.is_empty());
    }

    #[test]
    fn driver_unlisted_pid_works_without_invented_model_name() {
        let mut frame = status(false, 62);
        frame[13] = 0;
        frame[14] = 0;
        let r = run(&[collection(0x7ABC, "new-model", 19)], &[("new-model", vec![frame])], &[], vec![]);
        assert_eq!(r.out.len(), 1);
        assert_eq!(r.out[0].name, "Rapoo wireless mouse");
        assert_eq!((r.out[0].key.as_str(), r.out[0].percent), ("rapoo:7ABC", Some(62)));
    }

    #[test]
    fn driver_filters_collections_and_claimed_products() {
        let receiver = collection(0x1417, "receiver", 19);
        let mut other_vendor = receiver.clone();
        other_vendor.vid = 0x1234;
        let mut config = receiver.clone();
        config.usage = 14;
        let mut wrong_page = receiver.clone();
        wrong_page.usage_page = 1;
        let colls = [
            other_vendor,
            config,
            wrong_page,
            collection(0x1215, "silent-sibling", 10),
            collection(0x1C00, "unrecognised-telink", 7),
        ];
        let r = run(&colls, &[], &[], vec![]);
        assert!(r.opens == 0 && r.out.is_empty());
        let r = run(&[receiver.clone()], &[("receiver", vec![status(false, 25)])], &[(VID, 0x1417)], vec![]);
        assert_eq!(r.opens, 0);
    }

    #[test]
    fn driver_bluetooth_connection_and_model_pid_identity() {
        // Bluetooth mouse reported through the receiver of another model: identity comes from the frame's mouse PID.
        let mut frame = status(false, 80);
        frame[1] = 0x21;
        let r = run(&[collection(0x1417, "r", 19)], &[("r", vec![frame])], &[], vec![]);
        assert_eq!(r.out[0].via, "Bluetooth");
        assert_eq!(r.out[0].key, "rapoo:4617");
    }

    #[test]
    fn model_table_lookup() {
        assert_eq!(find_model(0x1215).unwrap().name, "Rapoo VT3 Pro");
        assert_eq!(find_model(0x4B06).unwrap().receiver, 0x1B06);
        assert!(find_model(0x7ABC).is_none());
    }
}

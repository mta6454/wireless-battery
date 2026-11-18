//! MCHOSE G3 A wireless mouse through its 2.4 GHz dongle (VID A8A5, PID 2255; A8A4 when the mouse is
//! on its cable). Protocol read from the mouse page of MCHOSE's own web configurator (M HUB web),
//! which talks to the vendor collection FF01:0010 with 64-byte unnumbered reports. Only the two
//! read-only queries the configurator sends when it connects are used:
//!  * online status: out `55 ED`, reply `AA ED ... [8]`; 1 = the mouse is not linked to the dongle
//!  * battery:       out `55 30 A5 0B 2E 01 01 01 00 00`, reply `AA 30 ... [8]=percent [9]=charging`

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidDev, HidIo};

const PID: u16 = 0x2255;
const VIDS: [u16; 2] = [0xA8A5, 0xA8A4];
const USAGE_PAGE: u16 = 0xFF01;
const USAGE: u16 = 0x0010;

const STATUS_CMD: [u8; 3] = [0x00, 0x55, 0xED];
const BATTERY_CMD: [u8; 11] = [0x00, 0x55, 0x30, 0xA5, 0x0B, 0x2E, 0x01, 0x01, 0x01, 0x00, 0x00];

/// Sends `cmd` and waits for the reply whose second byte is `tag` (the dongle also sends unrelated
/// reports such as DPI changes).
fn ask(io: &mut impl HidIo, cmd: &[u8], tag: u8) -> Option<Vec<u8>> {
    io.flush();
    if !io.write(cmd) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(1000);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let r = io.read(left.as_millis() as i32)?;
        if r.len() >= 10 && r[0] == 0xAA && r[1] == tag {
            return Some(r);
        }
    }
}

/// Reads one dongle/cable. `Some(status)` when the collection speaks this protocol.
fn query(io: &mut impl HidIo, vid: u16) -> Option<DeviceStatus> {
    let cable = vid == 0xA8A4;
    let mut s = DeviceStatus {
        key: format!("mchose:{}", hex4(vid)),
        name: "MCHOSE G3 A".into(), // the configurator calls the "MCHOSE 2.4G Wireless" dongle a G3 A
        kind: DeviceKind::Mouse,
        via: if cable { "USB cable" } else { "2.4 GHz receiver" }.into(),
        charging: cable,
        ..Default::default()
    };
    let status = ask(io, &STATUS_CMD, 0xED)?;
    if !cable && status[8] == 1 {
        s.connected = false; // dongle present, mouse switched off or out of range
        return Some(s);
    }
    let r = ask(io, &BATTERY_CMD, 0x30)?;
    match r[8] {
        1..=100 => s.percent = Some(r[8]),
        _ => s.asleep = true, // 0 = no value yet
    }
    s.charging |= r[9] != 0;
    Some(s)
}

pub fn poll(ctx: &mut PollContext) {
    for vid in VIDS {
        if ctx.is_claimed(vid, PID) {
            continue;
        }
        for c in ctx.find(vid, PID, |c| c.usage_page == USAGE_PAGE && c.usage == USAGE && c.out_len == 65) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            if let Some(s) = query(&mut d, vid) {
                ctx.add(s, vid, PID);
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
            true
        }
        fn read(&mut self, _t: i32) -> Option<Vec<u8>> {
            if self.replies.is_empty() { None } else { Some(self.replies.remove(0)) }
        }
        fn flush(&mut self) {}
    }

    fn reply(tag: u8, b8: u8, b9: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0xAA;
        r[1] = tag;
        r[8] = b8;
        r[9] = b9;
        r
    }

    fn mock(replies: Vec<Vec<u8>>) -> Mock {
        Mock { replies, sent: vec![] }
    }

    #[test]
    fn reads_percent_and_charging_and_skips_unrelated_reports() {
        let mut m = mock(vec![reply(0xED, 0, 0), reply(0xFA, 5, 5), reply(0x30, 70, 1)]);
        let s = query(&mut m, 0xA8A5).unwrap();
        assert_eq!(s.percent, Some(70));
        assert!(s.charging && s.connected);
        assert_eq!(s.via, "2.4 GHz receiver");
        assert_eq!(m.sent[0], STATUS_CMD);
        assert_eq!(m.sent[1], BATTERY_CMD);
    }

    #[test]
    fn mouse_not_linked_is_hidden() {
        let mut m = mock(vec![reply(0xED, 1, 0)]);
        let s = query(&mut m, 0xA8A5).unwrap();
        assert!(!s.connected);
        assert_eq!(m.sent.len(), 1, "no battery query when the mouse is not linked");
    }

    #[test]
    fn cable_ignores_link_flag_and_is_charging() {
        let mut m = mock(vec![reply(0xED, 2, 0), reply(0x30, 55, 0)]);
        let s = query(&mut m, 0xA8A4).unwrap();
        assert_eq!(s.percent, Some(55));
        assert!(s.charging);
        assert_eq!(s.via, "USB cable");
    }

    #[test]
    fn zero_percent_is_asleep_and_silence_is_not_ours() {
        let mut m = mock(vec![reply(0xED, 0, 0), reply(0x30, 0, 0)]);
        let s = query(&mut m, 0xA8A5).unwrap();
        assert!(s.asleep && s.percent.is_none());
        assert!(query(&mut mock(vec![]), 0xA8A5).is_none());
        assert!(query(&mut mock(vec![vec![0xAA, 0xED, 0, 0]]), 0xA8A5).is_none(), "short reply");
    }
}

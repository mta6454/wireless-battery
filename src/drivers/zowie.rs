//! BenQ ZOWIE wireless mice (VID 04A5). Zowie ships no software; these vendor commands were
//! reverse-engineered by the community. Only the read-only battery commands below are ever sent
//! (other EC-CW commands reboot the receiver and unpair the mouse).
//!  * EC-CW series (EC1/EC2/EC3-CW): receiver 8006, cable 8005. Unnumbered 64-byte output report
//!    [A5 03]; reply [mV lo][mV hi][percent]... mV = 0 when the mouse is not linked.
//!    From github.com/yleshchuk/zowie-battery.
//!  * DW series (U2-DW, S2-DW ...): receiver 800A. Output report 08 [B4 15] on usage page FF03,
//!    reply input report 09 on usage page FF04: mV = ([1] * 256 + [2]) * 10, [4] == C8.
//!    From github.com/IgnacioBarb/zowie-battery-tray (MIT).

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, interpolate, HidDev, HidIo};

const VID: u16 = 0x04A5;

/// Voltage -> % anchored on the mouse's own LED thresholds (10 / 25 / 50 %).
fn zowie_percent(mv: i32) -> u8 {
    const V: [i32; 6] = [4200, 4000, 3890, 3760, 3680, 3500];
    const P: [i32; 6] = [100, 75, 50, 25, 10, 0];
    interpolate(mv, &V, &P).clamp(0, 100) as u8
}

/// Decodes an EC-CW reply (`r[0]` = first payload byte, unnumbered report). None = not a battery reply.
fn decode_cw(r: &[u8], pid: u16) -> Option<DeviceStatus> {
    if r.len() < 3 {
        return None;
    }
    let mv = r[0] as i32 | ((r[1] as i32) << 8);
    if mv != 0 && !(3000..=4500).contains(&mv) {
        return None; // not a battery reply
    }
    let cable = pid == 0x8005;
    let mut s = DeviceStatus {
        key: format!("zowie:{}", hex4(pid)),
        name: "ZOWIE EC-CW".into(),
        kind: DeviceKind::Mouse,
        via: if cable { "USB cable" } else { "2.4 GHz receiver" }.into(),
        charging: cable,
        ..Default::default()
    };
    if mv == 0 {
        s.connected = false;
    } else {
        s.millivolts = mv as u32;
        s.percent = Some(if (1..=100).contains(&r[2]) { r[2] } else { zowie_percent(mv) });
    }
    Some(s)
}

/// EC-CW battery query on one collection.
fn query_cw(io: &mut impl HidIo, pid: u16) -> Option<DeviceStatus> {
    io.flush();
    if !io.write(&[0x00, 0xA5, 0x03]) {
        return None;
    }
    decode_cw(&io.read(1000)?, pid)
}

/// DW reply (input report 09) -> status. `name` is the product string of the command collection.
fn decode_dw(r: &[u8], pid: u16, name: &str) -> Option<DeviceStatus> {
    if r.len() < 5 || r[1] == 0xE6 || r[4] != 0xC8 {
        return None;
    }
    let mv = (r[1] as i32 * 256 + r[2] as i32) * 10;
    let cable = pid != 0x800A;
    let mut s = DeviceStatus {
        key: format!("zowie:{}", hex4(pid)),
        name: if name.is_empty() || name.contains("Receiver") { "ZOWIE DW mouse".into() } else { name.to_string() },
        kind: DeviceKind::Mouse,
        via: if cable { "USB cable" } else { "2.4 GHz receiver" }.into(),
        ..Default::default()
    };
    if mv == 0 {
        s.connected = false;
    } else {
        s.millivolts = mv as u32;
        s.percent = Some(zowie_percent(mv));
        s.charging = cable || mv >= 4250;
    }
    Some(s)
}

/// DW query: command goes out on `out`, the reply (report 09) comes back on `inp`.
fn query_dw(inp: &mut impl HidIo, out: &mut impl HidIo, pid: u16, name: &str) -> Option<DeviceStatus> {
    inp.flush();
    if !out.write(&[0x08, 0xB4, 0x15]) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(1500);
    let mut found = None;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(r) = inp.read(left.as_millis() as i32) else { break };
        if r.len() < 5 {
            break;
        }
        if r[0] == 0x09 {
            found = Some(r);
            break;
        }
    }
    decode_dw(&found?, pid, name)
}

fn poll_cw(ctx: &mut PollContext, pid: u16) -> bool {
    for c in ctx.find(VID, pid, |c| c.is_vendor_page() && c.out_len == 65) {
        let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
        if let Some(s) = query_cw(&mut d, pid) {
            ctx.add(s, VID, pid);
            return true;
        }
    }
    false
}

fn poll_dw(ctx: &mut PollContext, pid: u16) -> bool {
    let cmd = ctx.find(VID, pid, |c| c.usage_page == 0xFF03 && c.out_len == 16);
    let rep = ctx.find(VID, pid, |c| c.usage_page == 0xFF04 && c.in_len == 16);
    let (Some(cmd), Some(rep)) = (cmd.first(), rep.first()) else { return false };
    // open the reply channel first
    let Some(mut inp) = HidDev::open(ctx.api, rep) else { return false };
    let Some(mut out) = HidDev::open(ctx.api, cmd) else { return false };
    match query_dw(&mut inp, &mut out, pid, &cmd.product) {
        Some(s) => {
            ctx.add(s, VID, pid);
            true
        }
        None => false,
    }
}

pub fn poll(ctx: &mut PollContext) {
    for (_vid, pid) in ctx.products(VID) {
        if pid == 0x8005 || pid == 0x8006 {
            poll_cw(ctx, pid);
        } else {
            poll_dw(ctx, pid); // only acts on devices with the DW command/reply collections
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

    fn cw_reply(mv: u16, pct: u8) -> Vec<u8> {
        let mut r = vec![0u8; 63];
        r[0] = mv as u8;
        r[1] = (mv >> 8) as u8;
        r[2] = pct;
        r
    }

    fn dw_reply(mv10: u16, marker: u8) -> Vec<u8> {
        let mut r = vec![0u8; 16];
        r[0] = 0x09;
        r[1] = (mv10 >> 8) as u8;
        r[2] = mv10 as u8;
        r[4] = marker;
        r
    }

    #[test]
    fn percent_curve_anchors() {
        assert_eq!(zowie_percent(4200), 100);
        assert_eq!(zowie_percent(4300), 100);
        assert_eq!(zowie_percent(4000), 75);
        assert_eq!(zowie_percent(3890), 50);
        assert_eq!(zowie_percent(3760), 25);
        assert_eq!(zowie_percent(3680), 10);
        assert_eq!(zowie_percent(3500), 0);
        assert_eq!(zowie_percent(3000), 0);
        assert_eq!(zowie_percent(3945), 62); // midway 4000..3890 -> 50 + 25*55/110
    }

    #[test]
    fn cw_request_and_receiver_reply() {
        let mut m = Mock::new(vec![cw_reply(3900, 64)]);
        let s = query_cw(&mut m, 0x8006).unwrap();
        assert_eq!(m.sent, vec![vec![0x00, 0xA5, 0x03]]);
        assert_eq!(m.flushed, 1);
        assert_eq!(s.key, "zowie:8006");
        assert_eq!(s.name, "ZOWIE EC-CW");
        assert_eq!(s.via, "2.4 GHz receiver");
        assert_eq!(s.millivolts, 3900);
        assert_eq!(s.percent, Some(64));
        assert!(!s.charging && s.connected);
    }

    #[test]
    fn cw_cable_is_charging_and_uses_curve_when_percent_missing() {
        for bad in [0u8, 101, 255] {
            let s = query_cw(&mut Mock::new(vec![cw_reply(4000, bad)]), 0x8005).unwrap();
            assert_eq!(s.percent, Some(75), "pct byte {bad}");
            assert!(s.charging);
            assert_eq!(s.via, "USB cable");
        }
    }

    #[test]
    fn cw_zero_voltage_means_mouse_not_linked() {
        let s = query_cw(&mut Mock::new(vec![cw_reply(0, 50)]), 0x8006).unwrap();
        assert!(!s.connected);
        assert_eq!(s.percent, None);
        assert_eq!(s.millivolts, 0);
    }

    #[test]
    fn cw_rejects_implausible_voltage_short_reply_and_failed_io() {
        assert!(query_cw(&mut Mock::new(vec![cw_reply(2999, 50)]), 0x8006).is_none());
        assert!(query_cw(&mut Mock::new(vec![cw_reply(4501, 50)]), 0x8006).is_none());
        assert!(query_cw(&mut Mock::new(vec![vec![0x10, 0x0F]]), 0x8006).is_none());
        assert!(query_cw(&mut Mock::new(vec![]), 0x8006).is_none());
        let mut m = Mock::new(vec![cw_reply(3900, 50)]);
        m.accept_write = false;
        assert!(query_cw(&mut m, 0x8006).is_none());
    }

    #[test]
    fn dw_request_goes_out_reply_comes_in() {
        let mut inp = Mock::new(vec![dw_reply(392, 0xC8)]); // 3920 mV
        let mut out = Mock::new(vec![]);
        let s = query_dw(&mut inp, &mut out, 0x800A, "ZOWIE U2-DW").unwrap();
        assert_eq!(out.sent, vec![vec![0x08, 0xB4, 0x15]]);
        assert_eq!(inp.flushed, 1);
        assert_eq!(s.key, "zowie:800A");
        assert_eq!(s.name, "ZOWIE U2-DW");
        assert_eq!(s.via, "2.4 GHz receiver");
        assert_eq!(s.millivolts, 3920);
        assert!(!s.charging);
        assert_eq!(s.percent, Some(zowie_percent(3920)));
    }

    #[test]
    fn dw_skips_other_reports_and_handles_names() {
        let mut other = dw_reply(392, 0xC8);
        other[0] = 0x05;
        let mut inp = Mock::new(vec![other, dw_reply(425, 0xC8)]); // 4250 mV
        let s = query_dw(&mut inp, &mut Mock::new(vec![]), 0x800A, "ZOWIE Receiver").unwrap();
        assert_eq!(s.name, "ZOWIE DW mouse");
        assert!(s.charging); // >= 4250 mV
        let s = decode_dw(&dw_reply(400, 0xC8), 0x800A, "").unwrap();
        assert_eq!(s.name, "ZOWIE DW mouse");
    }

    #[test]
    fn dw_cable_pid_charges_and_zero_is_disconnected() {
        let s = decode_dw(&dw_reply(380, 0xC8), 0x800B, "X").unwrap();
        assert!(s.charging);
        assert_eq!(s.via, "USB cable");
        let s = decode_dw(&dw_reply(0, 0xC8), 0x800A, "X").unwrap();
        assert!(!s.connected && s.percent.is_none());
    }

    #[test]
    fn dw_rejects_error_marker_wrong_signature_and_timeout() {
        let mut e6 = dw_reply(0, 0xC8);
        e6[1] = 0xE6;
        assert!(decode_dw(&e6, 0x800A, "X").is_none());
        assert!(decode_dw(&dw_reply(392, 0x00), 0x800A, "X").is_none());
        assert!(decode_dw(&[0x09, 1, 2], 0x800A, "X").is_none());
        assert!(query_dw(&mut Mock::new(vec![]), &mut Mock::new(vec![]), 0x800A, "X").is_none());
        let mut out = Mock::new(vec![]);
        out.accept_write = false;
        assert!(query_dw(&mut Mock::new(vec![dw_reply(392, 0xC8)]), &mut out, 0x800A, "X").is_none());
    }
}

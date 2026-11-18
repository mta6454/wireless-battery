//! GameSir Nova Lite / Nova 2 / Nova 2 Lite on the 2.4 GHz receiver (3537:1098) or a USB cable.
//! XInput reports these as a wired pad, so the battery is read from the vendor collection
//! (usage page FF7A, 64-byte reports, no report ID) with the "device info" query GameSir's own
//! client uses:
//!   Request: 01 01
//!   Reply:   [0]=01, [1..5] receiver address, [10..11] receiver firmware, [18] battery:
//!            0..100 percent, FF = charging (no percentage while on the cable)
//! Unsolicited "F4 00" state frames arrive on the same collection and are skipped.
//! Protocol reference: github.com/WWWyyy452/GamesirBatteryTray and
//! github.com/AJAY0406/Gamesir-Nova-2-Lite-Battery-Detection (both MIT).

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidDev, HidIo};

const VID: u16 = 0x3537;

/// 1098: receiver with a controller linked (it re-enumerates under another id when the pad is off).
/// 100F: Nova 2 Lite on a cable.
fn is_nova(pid: u16) -> bool {
    pid == 0x1098 || pid == 0x100F
}

/// Sends the device-info query and waits up to `total_ms` for the reply frame (first byte 01),
/// skipping state frames. Returns the reply (at least 19 bytes) or None.
pub fn query(io: &mut impl HidIo, total_ms: u64) -> Option<Vec<u8>> {
    io.flush();
    if !io.write(&[0x00, 0x01, 0x01]) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(total_ms);
    while Instant::now() < end {
        match io.read(200) {
            Some(r) if r.len() >= 19 && r[0] == 0x01 => return Some(r),
            _ => {}
        }
    }
    None
}

/// Battery byte of a reply -> (percent, charging); None for an invalid value.
pub fn parse_battery(reply: &[u8]) -> Option<(Option<u8>, bool)> {
    let b = *reply.get(18)?;
    match b {
        0xFF => Some((None, true)), // percent stays unknown: the last known value is kept
        0..=100 => Some((Some(b), false)),
        _ => None,
    }
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(VID) {
        if !is_nova(pid) {
            continue;
        }
        for c in ctx.find(vid, pid, |c| c.usage_page == 0xFF7A && c.out_len >= 3) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some(r) = query(&mut d, 800) else { continue };
            let Some((percent, charging)) = parse_battery(&r) else { continue };
            let s = DeviceStatus {
                key: format!("gamesir:{}", hex4(pid)),
                name: "GameSir Nova".into(),
                kind: DeviceKind::Gamepad,
                via: if pid == 0x1098 { "2.4 GHz receiver" } else { "USB cable" }.into(),
                percent,
                charging,
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
        frames: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        flushed: bool,
        accept: bool,
    }

    impl Mock {
        fn new(frames: Vec<Vec<u8>>) -> Self {
            Self { frames, sent: vec![], flushed: false, accept: true }
        }
    }

    impl FeatureIo for Mock {
        fn set_feature(&mut self, _d: &[u8]) -> bool { false }
        fn get_feature(&mut self, _i: u8, _l: usize) -> Option<Vec<u8>> { None }
    }

    impl HidIo for Mock {
        fn write(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            self.accept
        }
        fn read(&mut self, _t: i32) -> Option<Vec<u8>> {
            if self.frames.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
                None
            } else {
                Some(self.frames.remove(0))
            }
        }
        fn flush(&mut self) { self.flushed = true; }
    }

    fn frame(first: u8, battery: u8, len: usize) -> Vec<u8> {
        let mut r = vec![0u8; len];
        r[0] = first;
        if len > 18 {
            r[18] = battery;
        }
        r
    }

    #[test]
    fn sends_flush_then_query() {
        let mut m = Mock::new(vec![frame(1, 50, 64)]);
        query(&mut m, 100).unwrap();
        assert!(m.flushed);
        assert_eq!(m.sent, vec![vec![0x00, 0x01, 0x01]]);
    }

    #[test]
    fn skips_state_frames_and_short_reads() {
        let mut m = Mock::new(vec![frame(0xF4, 99, 64), frame(1, 70, 10), frame(1, 42, 64)]);
        let r = query(&mut m, 100).unwrap();
        assert_eq!(parse_battery(&r), Some((Some(42), false)));
    }

    #[test]
    fn no_reply_or_failed_write_gives_none() {
        assert!(query(&mut Mock::new(vec![frame(0xF4, 1, 64)]), 30).is_none());
        let mut m = Mock::new(vec![frame(1, 50, 64)]);
        m.accept = false;
        assert!(query(&mut m, 30).is_none());
    }

    #[test]
    fn battery_byte_semantics() {
        assert_eq!(parse_battery(&frame(1, 0, 64)), Some((Some(0), false)));
        assert_eq!(parse_battery(&frame(1, 100, 64)), Some((Some(100), false)));
        assert_eq!(parse_battery(&frame(1, 0xFF, 64)), Some((None, true)));
        assert_eq!(parse_battery(&frame(1, 101, 64)), None);
        assert_eq!(parse_battery(&frame(1, 0, 18)), None);
    }

    #[test]
    fn product_table() {
        assert!(is_nova(0x1098) && is_nova(0x100F) && !is_nova(0x1099));
    }
}

//! AULA F75 / F87 Pro / F99 wireless keyboards on the Compx 2.4 GHz receiver (VID 3554, PID FA09).
//! The receiver exposes a 20-byte vendor collection on usage page FF02 / usage 02. Battery is a
//! 20-byte output report 0x13 carrying command 0x4A and a trailing sum checksum. The reply is
//! another 0x13 frame whose command byte, with its top bit masked, is 0x4A. Its payload starts at
//! byte 5: the level (1..100), then the state byte (0x01 on battery, 0x10 with the cable in).
//! The receiver also pushes unrelated 0x13 frames (a 0x0A status among them), so the report id
//! alone does not identify the reply.
//! From github.com/PyFlat/Device-Battery-Info (PR 6) and github.com/deepan-alve/womier-l65-linux.

use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, HidDev, HidIo};

/// Sum of the first `n` bytes, modulo 256.
fn sum(b: &[u8], n: usize) -> u8 {
    b[..n].iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

fn build_request() -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0] = 0x13;
    req[1] = 0x4A;
    req[19] = sum(&req, 19);
    req
}

fn query_battery(io: &mut impl HidIo) -> Option<Vec<u8>> {
    io.flush();
    if !io.write(&build_request()) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(1000);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = io.read(left)?;
        if r.len() < 20 || r[0] != 0x13 || (r[1] & 0x7F) != 0x4A || r[19] != sum(&r, 19) {
            continue;
        }
        return Some(r);
    }
}

/// Query with one retry (the first request can go unanswered while the 2.4 GHz link wakes up),
/// returning `(level, charging)`.
fn read_battery(io: &mut impl HidIo) -> Option<(u8, bool)> {
    let r = query_battery(io).or_else(|| query_battery(io))?;
    let level = r[5];
    if !(1..=100).contains(&level) {
        return None;
    }
    Some((level, r[6] == 0x10))
}

pub fn poll(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x3554) {
        if pid != 0xFA09 {
            continue; // other 3554 products are mice on a different protocol
        }
        for c in ctx.find(vid, pid, |c| c.usage_page == 0xFF02 && c.usage == 0x0002 && c.out_len >= 20 && c.in_len >= 20) {
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some((level, charging)) = read_battery(&mut d) else { continue };
            let s = DeviceStatus {
                key: format!("aula:{}:{}", hex4(vid), hex4(pid)),
                name: "AULA wireless keyboard".into(),
                kind: DeviceKind::Keyboard,
                via: "2.4 GHz receiver".into(),
                percent: Some(level),
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
        fn write(&mut self, d: &[u8]) -> bool {
            self.sent.push(d.to_vec());
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
    fn frame(cmd: u8, level: u8, state: u8) -> Vec<u8> {
        let mut r = vec![0u8; 20];
        r[0] = 0x13;
        r[1] = cmd;
        r[5] = level;
        r[6] = state;
        r[19] = sum(&r, 19);
        r
    }

    #[test]
    fn request_layout() {
        let r = build_request();
        assert_eq!(r[0], 0x13);
        assert_eq!(r[1], 0x4A);
        assert_eq!(r[19], 0x13 + 0x4A);
        assert!(r[2..19].iter().all(|&b| b == 0));
    }

    #[test]
    fn decodes_level_and_cable_state() {
        assert_eq!(read_battery(&mut mock(vec![frame(0x4A, 64, 0x01)])), Some((64, false)));
        assert_eq!(read_battery(&mut mock(vec![frame(0x4A, 100, 0x10)])), Some((100, true)));
        // top bit of the command byte is masked
        assert_eq!(read_battery(&mut mock(vec![frame(0xCA, 30, 0x01)])), Some((30, false)));
    }

    #[test]
    fn skips_unrelated_and_corrupt_frames() {
        let status = frame(0x0A, 50, 0);
        let mut bad_sum = frame(0x4A, 77, 1);
        bad_sum[19] ^= 0xFF;
        let mut other_id = frame(0x4A, 11, 1);
        other_id[0] = 0x14;
        let short = vec![0x13; 10];
        let mut m = mock(vec![status, bad_sum, other_id, short, frame(0x4A, 42, 1)]);
        assert_eq!(read_battery(&mut m), Some((42, false)));
        assert_eq!(m.sent.len(), 1);
    }

    #[test]
    fn retries_once_when_unanswered() {
        // Both attempts see only junk, then silence: two requests go out, no reading.
        let mut m = Mock { replies: vec![frame(0x0A, 1, 1)], sent: vec![] };
        assert_eq!(read_battery(&mut m), None);
        assert_eq!(m.sent.len(), 2);
    }

    #[test]
    fn out_of_range_level_is_rejected() {
        assert_eq!(read_battery(&mut mock(vec![frame(0x4A, 0, 1)])), None);
        assert_eq!(read_battery(&mut mock(vec![frame(0x4A, 101, 1)])), None);
    }
}

//! Razer BlackShark V3 / V3 Pro headsets (1532:0576/0577 V3 Pro, 0579/057A V3), interface 5.
//! 64-byte output report 02: [02][00][60][00 00][00][size][00 00][dir][class][sub][argc][args]..
//! [62] = XOR of bytes 0..61. GET battery = class 21 -> reply args [level %][charging].
//! The headset drops its 2.4 GHz link if the battery is queried every few seconds, so it is asked at
//! most once every 5 minutes and the last reading is reused in between.
//! Protocol from OpenRazer PR #2794 (docs/v3_pro_protocol.md) and github.com/ReedME/omarchy-blackshark-v3.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{hex4, sleep_ms, HidDev, HidIo};

const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn packet(dir: u8, cls: u8) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[0] = 0x02;
    b[2] = 0x60;
    b[6] = 0x04; // data size: dir, class, sub, argc
    b[9] = dir;
    b[10] = cls;
    b[62] = b[..62].iter().fold(0, |c, x| c ^ x);
    b
}

/// Sends a GET for `cls` and waits up to 600 ms for the matching reply.
fn get(d: &mut impl HidIo, cls: u8) -> Option<Vec<u8>> {
    if !d.write(&packet(0x80, cls)) {
        return None;
    }
    let end = Instant::now() + Duration::from_millis(600);
    loop {
        let left = end.checked_duration_since(Instant::now())?;
        let r = d.read(left.as_millis() as i32)?;
        if r.len() < 15 {
            return None;
        }
        if r[0] == 0x02 && r[2] == 0x60 && r[10] == cls {
            return Some(r);
        }
    }
}

/// Handshake, then battery query. Returns (percent, charging). `settle_ms` is the pause before
/// stale input is flushed (250 on real hardware).
fn query(d: &mut impl HidIo, settle_ms: u64) -> Option<(u8, bool)> {
    // Handshake the headset expects before reads (init, charging state on both links).
    d.write(&packet(0x00, 0x02));
    d.write(&packet(0x00, 0x2A));
    d.write(&packet(0x80, 0x2A));
    sleep_ms(settle_ms);
    d.flush();
    let r = get(d, 0x21)?;
    if r[13] > 100 {
        return None;
    }
    Some((r[13], r[14] != 0))
}

fn name(pid: u16) -> &'static str {
    if pid == 0x0576 || pid == 0x0577 {
        "Razer BlackShark V3 Pro"
    } else {
        "Razer BlackShark V3"
    }
}

struct Cached {
    at: Instant,
    status: DeviceStatus,
}

fn cache() -> &'static Mutex<HashMap<u16, Cached>> {
    static C: OnceLock<Mutex<HashMap<u16, Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn status_for(pid: u16, percent: u8, charging: bool) -> DeviceStatus {
    DeviceStatus {
        key: format!("blackshark:{}", hex4(pid)),
        name: name(pid).to_string(),
        kind: DeviceKind::Headset,
        via: "2.4 GHz receiver".into(),
        percent: Some(percent),
        charging,
        ..Default::default()
    }
}

pub fn poll(ctx: &mut PollContext) {
    for pid in [0x0576u16, 0x0577, 0x0579, 0x057A] {
        if ctx.is_claimed(0x1532, pid) {
            continue;
        }
        // The original requires USB interface 5; macOS does not report interface numbers, so an
        // unknown interface is accepted (the report length filter still has to match).
        let colls = ctx.find(0x1532, pid, |c| (c.iface == 5 || c.iface < 0) && c.out_len >= 64 && c.in_len >= 15);
        let Some(coll) = colls.first() else { continue };
        let cached = cache()
            .lock()
            .ok()
            .and_then(|c| c.get(&pid).filter(|e| e.at.elapsed() < MIN_INTERVAL).map(|e| e.status.clone()));
        if let Some(s) = cached {
            ctx.add(s, 0x1532, pid);
            continue;
        }
        let Some(mut d) = HidDev::open(ctx.api, coll) else { continue };
        let Some((percent, charging)) = query(&mut d, 250) else { continue };
        let s = status_for(pid, percent, charging);
        if let Ok(mut c) = cache().lock() {
            c.insert(pid, Cached { at: Instant::now(), status: s.clone() });
        }
        ctx.add(s, 0x1532, pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::FeatureIo;

    /// Scripted device: records writes, serves queued input reports.
    struct Mock {
        writes: Vec<Vec<u8>>,
        inputs: Vec<Vec<u8>>,
        flushed_at: Option<usize>,
        accept_write: bool,
    }

    impl Mock {
        fn new(inputs: Vec<Vec<u8>>) -> Self {
            Self { writes: vec![], inputs, flushed_at: None, accept_write: true }
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
            self.writes.push(data.to_vec());
            self.accept_write
        }
        fn read(&mut self, _timeout_ms: i32) -> Option<Vec<u8>> {
            if self.inputs.is_empty() { None } else { Some(self.inputs.remove(0)) }
        }
        fn flush(&mut self) {
            self.flushed_at = Some(self.writes.len());
        }
    }

    fn reply(cls: u8, pct: u8, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x02;
        r[2] = 0x60;
        r[10] = cls;
        r[13] = pct;
        r[14] = charging;
        r
    }

    #[test]
    fn packet_layout_and_checksum() {
        let p = packet(0x80, 0x21);
        assert_eq!(p.len(), 64);
        assert_eq!((p[0], p[2], p[6], p[9], p[10]), (0x02, 0x60, 0x04, 0x80, 0x21));
        // xor of bytes 0..61 = 02 ^ 60 ^ 04 ^ 80 ^ 21
        assert_eq!(p[62], 0x02 ^ 0x60 ^ 0x04 ^ 0x80 ^ 0x21);
        assert_eq!(p[63], 0);
        assert_eq!(packet(0x00, 0x02)[62], 0x02 ^ 0x60 ^ 0x04 ^ 0x02);
    }

    #[test]
    fn handshake_then_battery_get() {
        let mut m = Mock::new(vec![reply(0x21, 77, 1)]);
        assert_eq!(query(&mut m, 0), Some((77, true)));
        let seq: Vec<(u8, u8)> = m.writes.iter().map(|w| (w[9], w[10])).collect();
        assert_eq!(seq, vec![(0x00, 0x02), (0x00, 0x2A), (0x80, 0x2A), (0x80, 0x21)]);
        assert_eq!(m.flushed_at, Some(3)); // flushed after the handshake, before the GET
    }

    #[test]
    fn skips_unrelated_reports_until_matching_class() {
        let mut other_cls = reply(0x2A, 5, 0);
        other_cls[10] = 0x2A;
        let mut wrong_dev = reply(0x21, 9, 0);
        wrong_dev[2] = 0x00;
        let mut m = Mock::new(vec![other_cls, wrong_dev, reply(0x21, 42, 0)]);
        assert_eq!(query(&mut m, 0), Some((42, false)));
    }

    #[test]
    fn rejects_bad_replies() {
        assert_eq!(query(&mut Mock::new(vec![reply(0x21, 101, 0)]), 0), None); // out of range
        assert_eq!(query(&mut Mock::new(vec![vec![0x02; 14]]), 0), None); // too short
        assert_eq!(query(&mut Mock::new(vec![]), 0), None); // no answer
        let mut m = Mock::new(vec![reply(0x21, 50, 0)]);
        m.accept_write = false;
        assert_eq!(query(&mut m, 0), None);
    }

    #[test]
    fn charging_is_any_nonzero() {
        assert_eq!(query(&mut Mock::new(vec![reply(0x21, 100, 2)]), 0), Some((100, true)));
        assert_eq!(query(&mut Mock::new(vec![reply(0x21, 0, 0)]), 0), Some((0, false)));
    }

    #[test]
    fn names_and_status_fields() {
        assert_eq!(name(0x0576), "Razer BlackShark V3 Pro");
        assert_eq!(name(0x0577), "Razer BlackShark V3 Pro");
        assert_eq!(name(0x0579), "Razer BlackShark V3");
        assert_eq!(name(0x057A), "Razer BlackShark V3");
        let s = status_for(0x057A, 30, true);
        assert_eq!(s.key, "blackshark:057A");
        assert_eq!((s.kind, s.percent, s.charging, s.via.as_str()), (DeviceKind::Headset, Some(30), true, "2.4 GHz receiver"));
    }
}

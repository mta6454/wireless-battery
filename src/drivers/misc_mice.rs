//! Smaller mouse brands, each with a community-documented battery protocol:
//!  * Motospeed X6 (Realtek VID 0BDA, receivers FFE0 / FFF2, interface 4): output report B5 [06],
//!    reply report B4, byte 20 = level (+128 while charging). The cable (FFF1) reports no battery.
//!    From github.com/Jacobz888/LGSTrayBattery-MotospeedX6.
//!  * Xenta receivers (Delux M900 Pro, Attack Shark X6 / R1 / X11 / X3 Max / V3 Pro; VID 1D57): the receiver
//!    broadcasts input report 03 on usage page 000A every ~2 s: [03][model][40][state][percent],
//!    state 2 = full, 3 = charging (percent frozen). From github.com/AeonArcHQ/omarchy-delux-m900.
//!  * Ajazz AJ179 family (VID 249A receiver, 248A cable, interface 2): unnumbered 32-byte report,
//!    command 10 (device info, answered by the receiver): reply [10][00 = ok]..., [4..7] model id,
//!    [12] charging, [13] percent, [14] mouse connected (receiver), [31] = sum([4..30]).
//!    From github.com/Rockeyxx/aj179-linux (docs/PROTOCOL.md).
//!  * AJAZZ AJ-series mice on the SONiX vendor id 3151 (AJ159 PRO / APEX and siblings). The dongle
//!    exposes a 65-byte feature collection on usage page FFFF / usage 02. A SET_FEATURE status poll
//!    (report 0x00, opcode 0xF7, zero payload) brings the 2.4 GHz telemetry link up; ~30 ms later
//!    GET_FEATURE report 0x05 carries the charge in byte 3 (0..100). A frame with a non-zero byte 1
//!    or 2 is transient reconnect garbage and is rejected.
//!    From github.com/Aiacos/ajazz-control-center (docs/protocols/mouse/aj_series.md).

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4, sleep_ms, FeatureIo, HidColl, HidDev, HidIo};

/// Reads input reports until `matches` accepts one (or `timeout_ms` passes / the device errors).
fn read_until(io: &mut impl HidIo, timeout_ms: u64, matches: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
    let end = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            return None;
        }
        let r = io.read(left)?;
        if matches(&r) {
            return Some(r);
        }
    }
}

/// macOS hidapi never reports interface numbers (iface < 0): there the other filters decide.
fn iface_is(c: &HidColl, n: i32) -> bool {
    c.iface == n || c.iface < 0
}

// ---- Motospeed X6 ----------------------------------------------------------------------------

/// Reply byte 20: level, +128 while charging. Returns (charging, percent).
fn decode_motospeed(raw: u8) -> (bool, u8) {
    let charging = raw >= 128;
    (charging, clamp100(if charging { raw as i32 - 128 } else { raw as i32 }))
}

fn motospeed_query(io: &mut impl HidIo) -> Option<(bool, u8)> {
    io.flush();
    if !io.write(&[0xB5, 0x06]) {
        return None;
    }
    let r = read_until(io, 1500, |r| r.len() > 20 && r[0] == 0xB4)?;
    Some(decode_motospeed(r[20]))
}

fn poll_motospeed(ctx: &mut PollContext) {
    for pid in [0xFFE0u16, 0xFFF2, 0xFFF1] {
        if ctx.is_claimed(0x0BDA, pid) {
            continue;
        }
        for c in ctx.find(0x0BDA, pid, |c| iface_is(c, 4) && c.out_len >= 21) {
            let mut s = DeviceStatus {
                key: format!("motospeed:{}", hex4(pid)),
                name: "Motospeed X6".into(),
                kind: DeviceKind::Mouse,
                ..Default::default()
            };
            if pid == 0xFFF1 {
                // plugged in: charging, no battery reading over the cable
                s.via = "USB cable".into();
                s.charging = true;
                ctx.add(s, 0x0BDA, pid);
                break;
            }
            let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
            let Some((charging, percent)) = motospeed_query(&mut d) else { continue };
            s.via = "2.4 GHz receiver".into();
            s.charging = charging;
            s.percent = Some(percent);
            ctx.add(s, 0x0BDA, pid);
            break;
        }
    }
}

// ---- Xenta receivers -------------------------------------------------------------------------
// Xenta receiver (1D57:FA60, also FA65; cable FA55 / 2155), used by Delux and Attack Shark mice.
// The receiver broadcasts [03][model][40][state][level] every ~2 s; nothing is sent to it. The handle
// stays open between polls so these frames queue up. A reading counts once two frames agree, because
// every link-up starts with a false 100% (github.com/qu1ncky/BatteryTray, XentaReceiverProvider).

struct XentaModel {
    code: u8,
    name: &'static str,
    /// Level reported in tens (0-10).
    tens: bool,
}

const XENTA_MODELS: &[XentaModel] = &[
    XentaModel { code: 0x3D, name: "Delux M900 Pro", tens: false },
    XentaModel { code: 0x55, name: "Attack Shark X11", tens: false },
    XentaModel { code: 0x4E, name: "Attack Shark X3 Max", tens: false },
    XentaModel { code: 0xE5, name: "Attack Shark V3 Pro", tens: false },
    XentaModel { code: 0x10, name: "Attack Shark X6 / R1", tens: true },
];

/// The two most recent battery frames seen on a receiver.
#[derive(Default)]
struct XentaState {
    last: Vec<u8>,
    prev: Vec<u8>,
    last_at: Option<Instant>,
}

/// Takes everything queued since the last poll; a new listener waits (up to `fresh_wait_ms`) for
/// two frames.
fn xenta_drain(io: &mut impl HidIo, st: &mut XentaState, fresh: bool, fresh_wait_ms: u64) {
    let until = Instant::now() + Duration::from_millis(if fresh { fresh_wait_ms } else { 0 });
    let mut got = 0;
    loop {
        let remaining = until.saturating_duration_since(Instant::now()).as_millis() as i64;
        let waiting = fresh && got < 2;
        if waiting && remaining <= 0 {
            break;
        }
        let wait = if waiting { remaining.max(1) as i32 } else { 30 };
        let Some(r) = io.read(wait) else { break };
        if r.len() < 5 {
            break;
        }
        if r[0] != 0x03 || r[2] != 0x40 {
            continue;
        }
        st.prev = std::mem::take(&mut st.last);
        st.last = r[..5].to_vec();
        st.last_at = Some(Instant::now());
        got += 1;
    }
}

/// Status from the last frames, or `None` when there is none or it is older than 15 s (mouse off).
fn xenta_status(st: &XentaState, pid: u16) -> Option<DeviceStatus> {
    let age = st.last_at?.elapsed();
    if st.last.is_empty() || age > Duration::from_secs(15) {
        return None;
    }
    let cable = pid == 0xFA55 || pid == 0x2155;
    let (code, state, level) = (st.last[1], st.last[3], st.last[4]);
    let model = XENTA_MODELS.iter().filter(|m| m.code == code).last();
    let mut s = DeviceStatus {
        key: format!("xenta:{}", hex4(pid)),
        name: model.map_or("Wireless mouse", |m| m.name).to_string(),
        kind: DeviceKind::Mouse,
        via: if cable { "USB cable" } else { "2.4 GHz receiver" }.into(),
        ..Default::default()
    };
    let agreed = st.prev == st.last;
    if cable || state == 0x02 || state == 0x03 {
        s.charging = true; // the level is frozen on external power
        if state == 0x02 && !cable {
            s.percent = Some(100);
        }
    } else if agreed {
        if model.is_some_and(|m| m.tens) {
            if (1..=10).contains(&level) {
                s.percent = Some(level * 10);
            }
        } else if state == 0x01 && (1..=100).contains(&level) {
            s.percent = Some(level);
        }
    }
    if s.percent.is_none() && !s.charging {
        s.asleep = true; // shown with the last known value
    }
    Some(s)
}

struct XentaListener {
    dev: HidDev,
    state: XentaState,
}

fn xenta_listeners() -> &'static Mutex<HashMap<CString, XentaListener>> {
    static L: OnceLock<Mutex<HashMap<CString, XentaListener>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn poll_xenta(ctx: &mut PollContext) {
    let Ok(mut listeners) = xenta_listeners().lock() else { return };
    let mut present: HashSet<CString> = HashSet::new();
    for pid in [0xFA60u16, 0xFA65, 0xFA55, 0x2155] {
        if ctx.is_claimed(0x1D57, pid) {
            continue;
        }
        for c in ctx.find(0x1D57, pid, |c| c.usage_page == 0x000A && c.in_len >= 5) {
            present.insert(c.path.clone());
            let fresh = !listeners.contains_key(&c.path);
            if fresh {
                let Some(dev) = HidDev::open(ctx.api, &c) else { continue };
                listeners.insert(c.path.clone(), XentaListener { dev, state: XentaState::default() });
            }
            let l = listeners.get_mut(&c.path).expect("listener inserted above");
            xenta_drain(&mut l.dev, &mut l.state, fresh, 5000);
            let Some(s) = xenta_status(&l.state, pid) else { break };
            ctx.add(s, 0x1D57, pid);
            break;
        }
    }
    listeners.retain(|k, _| present.contains(k)); // forget receivers that were unplugged
}

// ---- Ajazz AJ179 family ----------------------------------------------------------------------

fn ajazz_frame_ok(r: &[u8]) -> bool {
    if r.len() < 32 || r[0] != 0x10 || r[1] != 0x00 {
        return false;
    }
    let sum = r[4..=30].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    sum == r[31]
}

/// Command 10 (report id 0, zero payload so the checksum is 0) and its checksummed reply.
fn ajazz_query(io: &mut impl HidIo) -> Option<Vec<u8>> {
    io.flush();
    if !io.write(&[0x00, 0x10]) {
        return None;
    }
    let r = read_until(io, 1500, ajazz_frame_ok)?;
    (r[13] <= 100).then_some(r)
}

fn ajazz_status(r: &[u8], vid: u16, pid: u16) -> DeviceStatus {
    let mut model = String::new();
    for &b in &r[4..8] {
        if !(0x20..0x7F).contains(&b) {
            break;
        }
        model.push(b as char);
    }
    let name = if model.len() == 4 && model.starts_with('M') { format!("Ajazz AJ{}", &model[1..]) } else { "Ajazz mouse".into() };
    let receiver = vid == 0x249A;
    let connected = !receiver || r[14] != 0;
    DeviceStatus {
        key: format!("ajazz:{}:{}", hex4(vid), hex4(pid)),
        name,
        kind: DeviceKind::Mouse,
        via: if receiver { "2.4 GHz receiver" } else { "USB cable" }.into(),
        connected,
        charging: r[12] != 0,
        percent: connected.then_some(r[13]),
        ..Default::default()
    }
}

fn poll_ajazz(ctx: &mut PollContext) {
    for vendor in [0x249Au16, 0x248A] {
        for (vid, pid) in ctx.products(vendor) {
            for c in ctx.find(vid, pid, |c| iface_is(c, 2) && c.out_len == 33 && c.in_len == 33) {
                let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
                let Some(r) = ajazz_query(&mut d) else { continue };
                ctx.add(ajazz_status(&r, vid, pid), vid, pid);
                break;
            }
        }
    }
}

// ---- Ajazz AJ-series on SONiX 3151 -----------------------------------------------------------

/// Status poll + report 0x05; returns the charge byte, or `None` for garbage / out-of-range.
fn ajseries_query(io: &mut impl FeatureIo, feat_len: usize, wait_ms: u64) -> Option<u8> {
    let mut req = vec![0u8; feat_len];
    req[0] = 0x00; // unnumbered report
    req[1] = 0xF7; // status poll: brings the 2.4 GHz telemetry link up
    if !io.set_feature(&req) {
        return None;
    }
    sleep_ms(wait_ms);
    let r = io.get_feature(0x05, feat_len)?;
    if r.len() < 4 || r[0] != 0x05 || r[1] != 0 || r[2] != 0 {
        return None; // malformed / reconnect frame
    }
    (r[3] <= 100).then_some(r[3])
}

fn ajseries_status(vid: u16, pid: u16, product: &str, pct: u8) -> DeviceStatus {
    let connected = pct > 0; // 0 means the link is up but the mouse is off
    DeviceStatus {
        key: format!("ajazzaj:{}:{}", hex4(vid), hex4(pid)),
        name: if product.is_empty() { "AJAZZ mouse".into() } else { product.to_string() },
        kind: DeviceKind::Mouse,
        via: "2.4 GHz receiver".into(),
        connected,
        percent: connected.then_some(pct),
        ..Default::default()
    }
}

fn poll_ajazz_aj_series(ctx: &mut PollContext) {
    for (vid, pid) in ctx.products(0x3151) {
        for c in ctx.find(vid, pid, |c| c.usage_page == 0xFFFF && c.usage == 0x02 && c.feat_len >= 65) {
            let Some(mut d) = HidDev::open_feature_only(ctx.api, &c) else { continue };
            let Some(pct) = ajseries_query(&mut d, c.feat_len as usize, 30) else { continue };
            ctx.add(ajseries_status(vid, pid, &c.product, pct), vid, pid);
            break;
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    poll_motospeed(ctx);
    poll_xenta(ctx);
    poll_ajazz(ctx);
    poll_ajazz_aj_series(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        replies: Vec<Vec<u8>>,
        feature: Option<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        accept: bool,
    }
    impl Mock {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Self { replies, accept: true, ..Default::default() }
        }
    }
    impl FeatureIo for Mock {
        fn set_feature(&mut self, d: &[u8]) -> bool {
            self.sent.push(d.to_vec());
            self.accept
        }
        fn get_feature(&mut self, id: u8, _: usize) -> Option<Vec<u8>> {
            assert_eq!(id, 0x05);
            self.feature.clone()
        }
    }
    impl HidIo for Mock {
        fn write(&mut self, d: &[u8]) -> bool {
            self.sent.push(d.to_vec());
            self.accept
        }
        fn read(&mut self, _: i32) -> Option<Vec<u8>> {
            if self.replies.is_empty() { None } else { Some(self.replies.remove(0)) }
        }
        fn flush(&mut self) {}
    }

    // ---- Motospeed
    fn moto(raw: u8) -> Vec<u8> {
        let mut r = vec![0u8; 32];
        r[0] = 0xB4;
        r[20] = raw;
        r
    }

    #[test]
    fn motospeed_request_and_levels() {
        let mut m = Mock::new(vec![moto(57)]);
        assert_eq!(motospeed_query(&mut m), Some((false, 57)));
        assert_eq!(m.sent[0], vec![0xB5, 0x06]);
        assert_eq!(motospeed_query(&mut Mock::new(vec![moto(128 + 40)])), Some((true, 40)));
        assert_eq!(decode_motospeed(128), (true, 0));
        assert_eq!(decode_motospeed(127), (false, 100)); // clamped
    }

    #[test]
    fn motospeed_ignores_other_reports() {
        let mut other = moto(10);
        other[0] = 0xB3;
        let short = vec![0xB4; 20];
        assert_eq!(motospeed_query(&mut Mock::new(vec![other, short, moto(70)])), Some((false, 70)));
        assert_eq!(motospeed_query(&mut Mock::new(vec![])), None);
        let mut m = Mock::new(vec![moto(70)]);
        m.accept = false;
        assert_eq!(motospeed_query(&mut m), None);
    }

    // ---- Xenta
    fn xframe(code: u8, state: u8, level: u8) -> Vec<u8> {
        vec![0x03, code, 0x40, state, level]
    }
    fn xenta(frames: Vec<Vec<u8>>, fresh: bool) -> XentaState {
        let mut st = XentaState::default();
        xenta_drain(&mut Mock::new(frames), &mut st, fresh, 50);
        st
    }

    #[test]
    fn xenta_needs_two_agreeing_frames() {
        let one = xenta(vec![xframe(0x3D, 1, 100)], false);
        let s = xenta_status(&one, 0xFA60).unwrap();
        assert_eq!(s.percent, None);
        assert!(s.asleep && !s.charging);
        let two = xenta(vec![xframe(0x3D, 1, 100), xframe(0x3D, 1, 80)], false);
        assert!(xenta_status(&two, 0xFA60).unwrap().asleep); // disagree
        let agreed = xenta(vec![xframe(0x3D, 1, 80), xframe(0x3D, 1, 80)], true);
        let s = xenta_status(&agreed, 0xFA60).unwrap();
        assert_eq!((s.percent, s.name.as_str(), s.via.as_str()), (Some(80), "Delux M900 Pro", "2.4 GHz receiver"));
        assert_eq!(s.key, "xenta:FA60");
    }

    #[test]
    fn xenta_charging_states() {
        let full = xenta(vec![xframe(0x55, 2, 77)], false);
        let s = xenta_status(&full, 0xFA60).unwrap();
        assert!(s.charging && !s.asleep);
        assert_eq!(s.percent, Some(100));
        let chg = xenta(vec![xframe(0x55, 3, 30)], false);
        let s = xenta_status(&chg, 0xFA60).unwrap();
        assert!(s.charging);
        assert_eq!(s.percent, None); // frozen level is not shown
        let cable = xenta(vec![xframe(0x55, 2, 77)], false);
        let s = xenta_status(&cable, 0xFA55).unwrap();
        assert!(s.charging && s.percent.is_none());
        assert_eq!(s.via, "USB cable");
    }

    #[test]
    fn xenta_tens_model_and_unknown_model() {
        let st = xenta(vec![xframe(0x10, 1, 7), xframe(0x10, 1, 7)], false);
        let s = xenta_status(&st, 0xFA65).unwrap();
        assert_eq!((s.percent, s.name.as_str()), (Some(70), "Attack Shark X6 / R1"));
        let st = xenta(vec![xframe(0x10, 1, 11), xframe(0x10, 1, 11)], false);
        assert_eq!(xenta_status(&st, 0xFA65).unwrap().percent, None);
        let st = xenta(vec![xframe(0x99, 1, 55), xframe(0x99, 1, 55)], false);
        let s = xenta_status(&st, 0xFA65).unwrap();
        assert_eq!((s.percent, s.name.as_str()), (Some(55), "Wireless mouse"));
        // state other than 1 on a non-tens model: no percent
        let st = xenta(vec![xframe(0x3D, 0, 55), xframe(0x3D, 0, 55)], false);
        assert_eq!(xenta_status(&st, 0xFA60).unwrap().percent, None);
    }

    #[test]
    fn xenta_skips_foreign_frames_and_goes_stale() {
        let st = xenta(vec![vec![0x04, 0, 0x40, 1, 50], vec![0x03, 0, 0x41, 1, 50], xframe(0x3D, 1, 20), xframe(0x3D, 1, 20)], false);
        assert_eq!(xenta_status(&st, 0xFA60).unwrap().percent, Some(20));
        assert!(xenta_status(&XentaState::default(), 0xFA60).is_none());
        let mut stale = xenta(vec![xframe(0x3D, 1, 20)], false);
        stale.last_at = Instant::now().checked_sub(Duration::from_secs(16));
        assert!(xenta_status(&stale, 0xFA60).is_none());
        // a short report ends the drain
        let st = xenta(vec![vec![0x03, 0, 0x40], xframe(0x3D, 1, 20)], false);
        assert!(st.last.is_empty());
    }

    // ---- Ajazz AJ179
    fn ajazz_frame(model: &[u8; 4], chg: u8, pct: u8, conn: u8) -> Vec<u8> {
        let mut r = vec![0u8; 32];
        r[0] = 0x10;
        r[4..8].copy_from_slice(model);
        r[12] = chg;
        r[13] = pct;
        r[14] = conn;
        r[31] = r[4..=30].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        r
    }

    #[test]
    fn ajazz_request_and_decode() {
        let mut m = Mock::new(vec![ajazz_frame(b"M179", 1, 66, 1)]);
        let r = ajazz_query(&mut m).unwrap();
        assert_eq!(m.sent[0], vec![0x00, 0x10]);
        let s = ajazz_status(&r, 0x249A, 0x0001);
        assert_eq!((s.name.as_str(), s.percent, s.charging, s.connected), ("Ajazz AJ179", Some(66), true, true));
        assert_eq!(s.key, "ajazz:249A:0001");
        assert_eq!(s.via, "2.4 GHz receiver");
    }

    #[test]
    fn ajazz_receiver_with_mouse_off_and_cable() {
        let off = ajazz_frame(b"M179", 0, 50, 0);
        let s = ajazz_status(&off, 0x249A, 1);
        assert!(!s.connected);
        assert_eq!(s.percent, None);
        let cable = ajazz_status(&off, 0x248A, 2);
        assert!(cable.connected);
        assert_eq!((cable.percent, cable.via.as_str()), (Some(50), "USB cable"));
    }

    #[test]
    fn ajazz_names_and_validation() {
        assert_eq!(ajazz_status(&ajazz_frame(&[1, 2, 3, 4], 0, 1, 1), 0x249A, 1).name, "Ajazz mouse");
        assert_eq!(ajazz_status(&ajazz_frame(b"X179", 0, 1, 1), 0x249A, 1).name, "Ajazz mouse");
        assert_eq!(ajazz_status(&ajazz_frame(b"M17\0", 0, 1, 1), 0x249A, 1).name, "Ajazz mouse");
        let mut bad_sum = ajazz_frame(b"M179", 0, 50, 1);
        bad_sum[31] ^= 1;
        let mut err = ajazz_frame(b"M179", 0, 50, 1);
        err[1] = 1;
        err[31] = err[4..=30].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        let short = vec![0x10; 31];
        assert_eq!(ajazz_query(&mut Mock::new(vec![bad_sum, err, short])), None);
        assert_eq!(ajazz_query(&mut Mock::new(vec![ajazz_frame(b"M179", 0, 101, 1)])), None);
    }

    // ---- AJ-series
    fn ajs(b1: u8, b2: u8, pct: u8) -> Vec<u8> {
        let mut r = vec![0u8; 65];
        r[0] = 0x05;
        r[1] = b1;
        r[2] = b2;
        r[3] = pct;
        r
    }
    fn ajs_mock(reply: Option<Vec<u8>>) -> Mock {
        Mock { feature: reply, accept: true, ..Default::default() }
    }

    #[test]
    fn ajseries_request_and_decode() {
        let mut m = ajs_mock(Some(ajs(0, 0, 91)));
        assert_eq!(ajseries_query(&mut m, 65, 0), Some(91));
        assert_eq!(m.sent[0].len(), 65);
        assert_eq!(m.sent[0][0], 0x00);
        assert_eq!(m.sent[0][1], 0xF7);
        assert!(m.sent[0][2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn ajseries_rejects_garbage() {
        assert_eq!(ajseries_query(&mut ajs_mock(Some(ajs(1, 0, 50))), 65, 0), None);
        assert_eq!(ajseries_query(&mut ajs_mock(Some(ajs(0, 1, 50))), 65, 0), None);
        assert_eq!(ajseries_query(&mut ajs_mock(Some(ajs(0, 0, 101))), 65, 0), None);
        let mut wrong_id = ajs(0, 0, 50);
        wrong_id[0] = 0;
        assert_eq!(ajseries_query(&mut ajs_mock(Some(wrong_id)), 65, 0), None);
        assert_eq!(ajseries_query(&mut ajs_mock(Some(vec![5, 0, 0])), 65, 0), None);
        assert_eq!(ajseries_query(&mut ajs_mock(None), 65, 0), None);
        let mut m = ajs_mock(Some(ajs(0, 0, 50)));
        m.accept = false;
        assert_eq!(ajseries_query(&mut m, 65, 0), None);
    }

    #[test]
    fn ajseries_status_and_names() {
        let s = ajseries_status(0x3151, 0x5006, "AJ159 PRO", 40);
        assert_eq!((s.name.as_str(), s.percent, s.connected), ("AJ159 PRO", Some(40), true));
        assert_eq!(s.key, "ajazzaj:3151:5006");
        let off = ajseries_status(0x3151, 0x5006, "", 0);
        assert_eq!((off.name.as_str(), off.percent, off.connected), ("AJAZZ mouse", None, false));
    }

    #[test]
    fn iface_filter_accepts_unknown_interface() {
        let mut c = HidColl {
            path: Default::default(),
            vid: 0,
            pid: 0,
            iface: -1,
            usage_page: 0,
            usage: 0,
            product: String::new(),
            in_len: 0,
            out_len: 0,
            feat_len: 0,
        };
        assert!(iface_is(&c, 4));
        c.iface = 4;
        assert!(iface_is(&c, 4) && !iface_is(&c, 2));
    }
}

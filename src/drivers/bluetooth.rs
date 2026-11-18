//! Bluetooth devices (earbuds, headphones, BLE mice and keyboards) with the battery level the OS
//! already knows.
//!  * Windows: the level Windows reads over HFP (classic) or the GATT Battery Service (LE) lives
//!    in a device property, the value Settings > Bluetooth & devices shows. Read through SetupAPI.
//!  * macOS: `system_profiler SPBluetoothDataType -json` for connected devices that publish a
//!    level, complemented by `ioreg` for Apple HID devices (Magic Mouse / Keyboard / Trackpad).
//!
//! Everything except the thin OS access layers is plain functions so it is testable anywhere.

// The Windows and macOS halves are both compiled everywhere (so their tests run anywhere); each
// platform leaves the other half unused.
#![allow(dead_code)]

use std::collections::BTreeMap;

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::guess_kind;

// ---- shared, platform independent logic ------------------------------------------------------

/// Kind from a Bluetooth Class of Device value (major class 4 = audio/video, 5 = peripheral).
pub fn kind_from_class_of_device(cod: u32) -> DeviceKind {
    let major = (cod >> 8) & 0x1F;
    let minor = (cod >> 2) & 0x3F;
    if major == 0x04 {
        return DeviceKind::Headset;
    }
    if major == 0x05 {
        if (minor & 0x0F) == 0x01 || (minor & 0x0F) == 0x02 {
            return DeviceKind::Gamepad;
        }
        if (minor >> 4) == 1 {
            return DeviceKind::Keyboard;
        }
        if (minor >> 4) == 2 {
            return DeviceKind::Mouse;
        }
    }
    DeviceKind::Other
}

/// Drops the service suffix Windows appends to the name of a profile node.
pub fn strip_service_suffix(n: &str) -> String {
    for suf in [" Hands-Free AG", " Hands-Free", " Avrcp Transport", " Stereo", " Audio Gateway"] {
        if n.len() > suf.len() && n.ends_with(suf) {
            return n[..n.len() - suf.len()].to_string();
        }
    }
    n.to_string()
}

/// One SetupAPI device node under BTHENUM / BTHLE / BTHLEDEVICE, already read into plain values.
#[derive(Clone, Debug, Default)]
pub struct BtNode {
    pub instance_id: String,
    /// Container id formatted like StringFromGUID2 (`{XXXXXXXX-...}`); None when the node has none.
    pub container: Option<String>,
    pub name: String,
    pub class_of_device: Option<u32>,
    pub connected: Option<bool>,
    /// Battery percentage property, if the node has one.
    pub battery: Option<u8>,
    /// CM_Get_DevNode_Status reported DN_HAS_PROBLEM.
    pub has_problem: bool,
}

/// `{8-4-4-4-12}` upper-case, the format StringFromGUID2 produces.
pub fn format_guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d1, d2, d3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    )
}

fn is_bt_instance(inst: &str) -> bool {
    inst.starts_with("BTHENUM\\") || inst.starts_with("BTHLE\\") || inst.starts_with("BTHLEDEVICE\\")
}

fn is_main_device_node(inst: &str) -> bool {
    inst.starts_with("BTHENUM\\DEV_") || inst.starts_with("BTHLE\\DEV_")
}

/// Groups Bluetooth device nodes by container, prefers a live battery node, and returns the
/// connected devices with a level. `existing` holds names already reported by other drivers.
pub fn windows_devices(nodes: &[BtNode], existing: &[String]) -> Vec<DeviceStatus> {
    struct Found {
        pct: u8,
        name: String,
        connected: bool,
    }
    let mut container_name: BTreeMap<String, String> = BTreeMap::new();
    let mut container_cod: BTreeMap<String, u32> = BTreeMap::new();
    let mut container_conn: BTreeMap<String, bool> = BTreeMap::new();
    let mut battery: BTreeMap<String, Found> = BTreeMap::new();

    for n in nodes {
        if !is_bt_instance(&n.instance_id) {
            continue;
        }
        let cid = n.container.clone().unwrap_or_else(|| n.instance_id.clone());
        if is_main_device_node(&n.instance_id) {
            container_name.insert(cid.clone(), n.name.clone());
            if let Some(c) = n.class_of_device {
                container_cod.insert(cid.clone(), c);
            }
            if let Some(c) = n.connected {
                container_conn.insert(cid.clone(), c);
            }
        }
        let Some(pct) = n.battery else { continue };
        // The service node's own connected flag is only a fallback: headphones keep their level on
        // the Hands-Free node, which reads as disconnected while audio runs over A2DP. The main
        // device node decides below when there is one.
        let conn = n.connected.unwrap_or(true);
        if n.has_problem || pct > 100 {
            continue;
        }
        if let Some(prev) = battery.get(&cid) {
            if prev.connected && !conn {
                continue; // prefer a live node
            }
        }
        battery.insert(cid, Found { pct, name: strip_service_suffix(&n.name), connected: conn });
    }

    let mut out = Vec::new();
    for (cid, f) in &battery {
        let connected = match container_conn.get(cid) {
            Some(c) => *c,
            None => f.connected,
        };
        if !connected {
            continue;
        }
        let mut name = match container_name.get(cid) {
            Some(n) if !n.is_empty() => n.clone(),
            _ => f.name.clone(),
        };
        if name.is_empty() {
            name = "Bluetooth device".to_string();
        }
        // Skip devices a vendor driver already reported (HID++ / Bragi on the same link).
        if existing.iter().any(|e| *e == name) {
            continue;
        }
        let mut kind = match container_cod.get(cid) {
            Some(c) => kind_from_class_of_device(*c),
            None => DeviceKind::Other,
        };
        if kind == DeviceKind::Other {
            kind = guess_kind(&name);
        }
        out.push(DeviceStatus {
            key: format!("bt:{cid}"),
            name,
            via: "Bluetooth".to_string(),
            kind,
            percent: Some(f.pct),
            ..Default::default()
        });
    }
    out
}

// ---- minimal JSON reader (used for system_profiler output) -----------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Key order is kept: system_profiler lists devices as one-key objects.
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    p: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.p < self.s.len() && matches!(self.s[self.p], b' ' | b'\n' | b'\r' | b'\t') {
            self.p += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.p).copied()
    }
    fn parse(&mut self, depth: u32) -> Option<Json> {
        if depth > 32 {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'{' => {
                self.p += 1;
                let mut m = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.p += 1;
                    return Some(Json::Obj(m));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return None;
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return None;
                    }
                    self.p += 1;
                    let v = self.parse(depth + 1)?;
                    m.push((key, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.p += 1,
                        Some(b'}') => {
                            self.p += 1;
                            return Some(Json::Obj(m));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.p += 1;
                let mut a = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.p += 1;
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(self.parse(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.p += 1,
                        Some(b']') => {
                            self.p += 1;
                            return Some(Json::Arr(a));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' | b'f' | b'n' => {
                let start = self.p;
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.p += 1;
                }
                match &self.s[start..self.p] {
                    b"true" => Some(Json::Bool(true)),
                    b"false" => Some(Json::Bool(false)),
                    b"null" => Some(Json::Null),
                    _ => None,
                }
            }
            _ => {
                let start = self.p;
                while self.peek().is_some_and(|c| matches!(c, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')) {
                    self.p += 1;
                }
                let txt = std::str::from_utf8(&self.s[start..self.p]).ok()?;
                txt.parse::<f64>().ok().map(Json::Num)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = self.s.get(self.p..self.p + 4)?;
        let v = u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()?;
        self.p += 4;
        Some(v)
    }

    /// Reads a string; the cursor is on the opening quote. Handles \uXXXX incl. surrogate pairs.
    fn string(&mut self) -> Option<String> {
        self.p += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.s.get(self.p)?;
            self.p += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.s.get(self.p)?;
                    self.p += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.s.get(self.p..self.p + 2) == Some(b"\\u") {
                                self.p += 2;
                                let lo = self.hex4()?;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            let ch = char::from_u32(cp).unwrap_or('?');
                            let mut b = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        }
                        e => out.push(e),
                    }
                }
                c => out.push(c),
            }
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

fn parse_json(text: &str) -> Option<Json> {
    let start = text.find('{')?;
    Parser { s: text[start..].as_bytes(), p: 0 }.parse(0)
}

// ---- macOS parsers ----------------------------------------------------------------------------

/// A battery percentage like "85%" or "85".
fn parse_percent(s: &str) -> Option<u8> {
    let v: u32 = s.trim().trim_end_matches('%').trim().parse().ok()?;
    (v <= 100).then_some(v as u8)
}

/// Level for one device: the main level when present, otherwise the lowest of the left/right
/// buds. The charging case is never used on its own. (The original reports one percent per
/// device; the lowest earbud is the figure that tells when the pair will die.)
fn pick_level(main: Option<u8>, left: Option<u8>, right: Option<u8>) -> Option<u8> {
    main.or_else(|| match (left, right) {
        (Some(l), Some(r)) => Some(l.min(r)),
        (Some(l), None) => Some(l),
        (None, Some(r)) => Some(r),
        _ => None,
    })
}

fn kind_from_minor_type(t: &str) -> DeviceKind {
    let t = t.to_lowercase();
    if t.contains("mouse") || t.contains("trackpad") {
        DeviceKind::Mouse
    } else if t.contains("keyboard") {
        DeviceKind::Keyboard
    } else if t.contains("gamepad") || t.contains("game") || t.contains("joystick") {
        DeviceKind::Gamepad
    } else if t.contains("head") || t.contains("speaker") || t.contains("audio") || t.contains("earbud") {
        DeviceKind::Headset
    } else {
        DeviceKind::Other
    }
}

/// Parses `system_profiler SPBluetoothDataType -json`: connected devices that publish a level.
/// Garbage gives an empty list. Keys are `bt:<lower-case address>`.
pub fn parse_system_profiler(text: &str) -> Vec<DeviceStatus> {
    let mut out = Vec::new();
    let Some(root) = parse_json(text) else { return out };
    let Some(Json::Arr(sections)) = root.get("SPBluetoothDataType") else { return out };
    for sec in sections {
        let Some(Json::Arr(list)) = sec.get("device_connected") else { continue };
        for entry in list {
            let Json::Obj(items) = entry else { continue };
            for (name, props) in items {
                let level = |k: &str| props.get(k).and_then(Json::as_str).and_then(parse_percent);
                let Some(pct) =
                    pick_level(level("device_batteryLevelMain"), level("device_batteryLevelLeft"), level("device_batteryLevelRight"))
                else {
                    continue;
                };
                let name = name.trim().to_string();
                let addr = props.get("device_address").and_then(Json::as_str).unwrap_or("").to_lowercase();
                let kind = match props.get("device_minorType").and_then(Json::as_str).map(kind_from_minor_type) {
                    Some(k) if k != DeviceKind::Other => k,
                    _ => guess_kind(&name),
                };
                out.push(DeviceStatus {
                    key: if addr.is_empty() { format!("bt:{name}") } else { format!("bt:{addr}") },
                    name: if name.is_empty() { "Bluetooth device".to_string() } else { name },
                    via: "Bluetooth".to_string(),
                    kind,
                    percent: Some(pct),
                    ..Default::default()
                });
            }
        }
    }
    out
}

/// Connected Bluetooth devices from `system_profiler SPBluetoothDataType -json` as
/// (name, kind, lower-case address), whether or not they publish a level there.
pub fn connected_devices(text: &str) -> Vec<(String, DeviceKind, String)> {
    let mut out = Vec::new();
    let Some(root) = parse_json(text) else { return out };
    let Some(Json::Arr(sections)) = root.get("SPBluetoothDataType") else { return out };
    for sec in sections {
        let Some(Json::Arr(list)) = sec.get("device_connected") else { continue };
        for entry in list {
            let Json::Obj(items) = entry else { continue };
            for (name, props) in items {
                let name = name.trim().to_string();
                let addr = props.get("device_address").and_then(Json::as_str).unwrap_or("").to_lowercase();
                let kind = match props.get("device_minorType").and_then(Json::as_str).map(kind_from_minor_type) {
                    Some(k) if k != DeviceKind::Other => k,
                    _ => guess_kind(&name),
                };
                if !name.is_empty() {
                    out.push((name, kind, addr));
                }
            }
        }
    }
    out
}

/// Parses `pmset -g accps`: lines like `-MCHOSE G3 A (id=50272119)\t70%; ...`. macOS knows the
/// level of many BLE HID devices that system_profiler does not list one for. Only entries that
/// match a connected Bluetooth device are used, so batteries and UPSes are ignored.
pub fn parse_pmset(text: &str, connected: &[(String, DeviceKind, String)]) -> Vec<DeviceStatus> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('-') else { continue };
        let Some(id) = rest.rfind(" (id=") else { continue };
        let name = rest[..id].trim();
        let Some(pct) = rest[id..].split('\t').nth(1).and_then(|v| v.split(';').next()).and_then(parse_percent) else { continue };
        let Some((cname, kind, addr)) = connected.iter().find(|(n, _, _)| n == name) else { continue };
        out.push(DeviceStatus {
            key: if addr.is_empty() { format!("bt:{cname}") } else { format!("bt:{addr}") },
            name: cname.clone(),
            via: "Bluetooth".to_string(),
            kind: *kind,
            percent: Some(pct),
            ..Default::default()
        });
    }
    out
}

/// Value of `"Key" = ...` on one ioreg line, quotes removed.
fn ioreg_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\" = ");
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    if let Some(q) = rest.strip_prefix('"') {
        return q.find('"').map(|e| &q[..e]);
    }
    // Unquoted value; inside the one-line `{...}` dictionaries (-w0) keys are separated by commas.
    let end = rest.find([',', '}', ' ']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Parses `ioreg -r -k BatteryPercent -l -w0` text: each registry entry that carries a
/// BatteryPercent becomes a device (name from `Product`, address from `DeviceAddress`). Entries
/// whose `Transport` is present and not Bluetooth (a cable-connected Magic Keyboard) are skipped.
pub fn parse_ioreg(text: &str) -> Vec<DeviceStatus> {
    #[derive(Default)]
    struct Entry {
        product: Option<String>,
        addr: Option<String>,
        transport: Option<String>,
        pct: Option<u8>,
    }
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        if line.contains("+-o ") {
            entries.push(Entry::default());
            continue;
        }
        let Some(e) = entries.last_mut() else { continue };
        if e.pct.is_none() {
            if let Some(v) = ioreg_value(line, "BatteryPercent") {
                e.pct = parse_percent(v);
            }
        }
        if e.product.is_none() {
            e.product = ioreg_value(line, "Product").map(str::to_string);
        }
        if e.addr.is_none() {
            e.addr = ioreg_value(line, "DeviceAddress").map(str::to_string);
        }
        if e.transport.is_none() {
            e.transport = ioreg_value(line, "Transport").map(str::to_string);
        }
    }
    let mut out = Vec::new();
    for e in entries {
        let Some(pct) = e.pct else { continue };
        if e.transport.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case("bluetooth")) {
            continue;
        }
        let name = e.product.unwrap_or_default().trim().to_string();
        if name.is_empty() {
            continue;
        }
        let addr = e.addr.unwrap_or_default().replace('-', ":").to_lowercase();
        out.push(DeviceStatus {
            key: if addr.is_empty() { format!("bt:{name}") } else { format!("bt:{addr}") },
            kind: guess_kind(&name),
            name,
            via: "Bluetooth".to_string(),
            percent: Some(pct),
            ..Default::default()
        });
    }
    out
}

/// system_profiler first, ioreg fills in what it did not list. A device is skipped when its name
/// is already in `existing` (reported by a vendor driver) or was taken from an earlier source.
pub fn merge_macos(profiler: Vec<DeviceStatus>, ioreg: Vec<DeviceStatus>, pmset: Vec<DeviceStatus>, existing: &[String]) -> Vec<DeviceStatus> {
    let mut names: Vec<String> = existing.to_vec();
    let mut out: Vec<DeviceStatus> = Vec::new();
    for d in profiler.into_iter().chain(ioreg).chain(pmset) {
        if names.contains(&d.name) || out.iter().any(|o| o.key == d.key) {
            continue;
        }
        names.push(d.name.clone());
        out.push(d);
    }
    out
}

// ---- macOS command runner ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn run(program: &str, args: &[&str], timeout: std::time::Duration) -> String {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
    else {
        return String::new();
    };
    let Some(mut stdout) = child.stdout.take() else { return String::new() };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let mut bytes = Vec::new();
        if stdout.read_to_end(&mut bytes).is_ok() {
            buf = String::from_utf8_lossy(&bytes).into_owned();
        }
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(s) => {
            let _ = child.wait();
            s
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            String::new()
        }
    }
}

// system_profiler answers in ~0.2 s here, so no cache is needed for the 60 s poll loop.
#[cfg(target_os = "macos")]
fn poll_os(existing: &[String]) -> Vec<DeviceStatus> {
    let t = std::time::Duration::from_secs(5);
    let sp_text = run("/usr/sbin/system_profiler", &["SPBluetoothDataType", "-json"], t);
    let sp = parse_system_profiler(&sp_text);
    let io = parse_ioreg(&run("/usr/sbin/ioreg", &["-r", "-k", "BatteryPercent", "-l", "-w0"], t));
    let pm = parse_pmset(&run("/usr/bin/pmset", &["-g", "accps"], t), &connected_devices(&sp_text));
    merge_macos(sp, io, pm, existing)
}

// ---- Windows SetupAPI layer -------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::{format_guid, BtNode};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows_sys::Win32::Devices::Properties::*;
    use windows_sys::Win32::Foundation::DEVPROPKEY;

    const BT_BATTERY: DEVPROPKEY =
        DEVPROPKEY { fmtid: GUID::from_u128(0x104EA319_6EE2_4701_BD47_8DDBF425BBE5), pid: 2 };
    const IS_CONNECTED: DEVPROPKEY =
        DEVPROPKEY { fmtid: GUID::from_u128(0x83DA6326_97A6_4088_9453_A1923F573B29), pid: 15 };
    const CLASS_OF_DEVICE: DEVPROPKEY =
        DEVPROPKEY { fmtid: GUID::from_u128(0x2BD67D8B_8BEB_48D5_87E0_6CDA3428040A), pid: 10 };

    /// Raw property bytes when the property exists with the expected type.
    fn prop(di: HDEVINFO, dd: &SP_DEVINFO_DATA, key: &DEVPROPKEY, want: u32, buf: &mut [u8]) -> Option<usize> {
        let mut ty: u32 = 0;
        let mut need: u32 = 0;
        // SAFETY: buf and the out-pointers are valid for the sizes passed; di/dd come from SetupAPI.
        let ok = unsafe {
            SetupDiGetDevicePropertyW(di, dd, key, &mut ty, buf.as_mut_ptr(), buf.len() as u32, &mut need, 0)
        };
        (ok != 0 && ty == want).then_some((need as usize).min(buf.len()))
    }

    fn string_prop(di: HDEVINFO, dd: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> String {
        let mut buf = [0u8; 510];
        match prop(di, dd, key, DEVPROP_TYPE_STRING, &mut buf) {
            Some(n) => {
                let w: Vec<u16> = buf[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                String::from_utf16_lossy(&w)
            }
            None => String::new(),
        }
    }

    /// Every present device node under the Bluetooth enumerators.
    pub fn nodes() -> Vec<BtNode> {
        let mut out = Vec::new();
        // SAFETY: plain SetupAPI calls; the list is destroyed before returning.
        unsafe {
            let di = SetupDiGetClassDevsW(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), DIGCF_ALLCLASSES | DIGCF_PRESENT);
            if di == -1 {
                return out;
            }
            let mut dd: SP_DEVINFO_DATA = std::mem::zeroed();
            dd.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            let mut i = 0;
            while SetupDiEnumDeviceInfo(di, i, &mut dd) != 0 {
                i += 1;
                let mut inst = [0u16; 512];
                if SetupDiGetDeviceInstanceIdW(di, &dd, inst.as_mut_ptr(), 512, std::ptr::null_mut()) == 0 {
                    continue;
                }
                let len = inst.iter().position(|&c| c == 0).unwrap_or(inst.len());
                let instance_id = String::from_utf16_lossy(&inst[..len]);
                if !(instance_id.starts_with("BTHENUM\\")
                    || instance_id.starts_with("BTHLE\\")
                    || instance_id.starts_with("BTHLEDEVICE\\"))
                {
                    continue;
                }
                let mut b16 = [0u8; 16];
                let container = prop(di, &dd, &DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID, &mut b16).map(|_| {
                    format_guid(
                        u32::from_le_bytes([b16[0], b16[1], b16[2], b16[3]]),
                        u16::from_le_bytes([b16[4], b16[5]]),
                        u16::from_le_bytes([b16[6], b16[7]]),
                        [b16[8], b16[9], b16[10], b16[11], b16[12], b16[13], b16[14], b16[15]],
                    )
                });
                let mut name = string_prop(di, &dd, &DEVPKEY_Device_FriendlyName);
                if name.is_empty() {
                    name = string_prop(di, &dd, &DEVPKEY_Device_DeviceDesc);
                }
                let mut b4 = [0u8; 4];
                let class_of_device =
                    prop(di, &dd, &CLASS_OF_DEVICE, DEVPROP_TYPE_UINT32, &mut b4).map(|_| u32::from_le_bytes(b4));
                let mut b1 = [0u8; 1];
                let connected = prop(di, &dd, &IS_CONNECTED, DEVPROP_TYPE_BOOLEAN, &mut b1).map(|_| b1[0] != DEVPROP_FALSE);
                let battery = prop(di, &dd, &BT_BATTERY, DEVPROP_TYPE_BYTE, &mut b1).map(|_| b1[0]);
                let (mut st, mut prob) = (0u32, 0u32);
                let has_problem = CM_Get_DevNode_Status(&mut st, &mut prob, dd.DevInst, 0) == CR_SUCCESS && (st & DN_HAS_PROBLEM) != 0;
                out.push(BtNode { instance_id, container, name, class_of_device, connected, battery, has_problem });
            }
            SetupDiDestroyDeviceInfoList(di);
        }
        out
    }
}

#[cfg(windows)]
fn poll_os(existing: &[String]) -> Vec<DeviceStatus> {
    windows_devices(&win::nodes(), existing)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn poll_os(_existing: &[String]) -> Vec<DeviceStatus> {
    Vec::new()
}

/// True for entries read through a 2.4 GHz receiver, which keeps answering with its last value after
/// the mouse has been switched to Bluetooth.
fn is_receiver(d: &DeviceStatus) -> bool {
    d.via.to_lowercase().contains("receiver")
}

/// A device the OS lists as connected over Bluetooth is in Bluetooth mode, so an entry of the same
/// name read through a receiver is stale: the Bluetooth reading replaces it.
pub fn replace_receiver_entries(out: &mut Vec<DeviceStatus>, bt: &[DeviceStatus]) {
    out.retain(|o| !(is_receiver(o) && bt.iter().any(|b| b.name == o.name)));
}

pub fn poll(ctx: &mut PollContext) {
    let existing: Vec<String> = ctx.out.iter().filter(|o| !is_receiver(o)).map(|o| o.name.clone()).collect();
    let found = poll_os(&existing);
    replace_receiver_entries(&mut ctx.out, &found);
    ctx.out.extend(found);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from real `system_profiler SPBluetoothDataType -json` output; names and addresses
    // replaced with generic ones.
    const SP: &str = r#"{
 "SPBluetoothDataType": [
  {
   "controller_properties": { "controller_address": "AA:BB:CC:00:00:01", "controller_state": "attrib_on" },
   "device_connected": [
    { "Test Keyboard ": { "device_address": "11:22:33:44:55:01", "device_batteryLevelMain": "100%", "device_minorType": "Keyboard", "device_services": "0x400020 < HID BLE >" } },
    { "Test Mouse": { "device_address": "11:22:33:44:55:02", "device_minorType": "Mouse", "device_productID": "0x32B0" } },
    { "Test Buds": { "device_address": "11:22:33:44:55:03", "device_batteryLevelCase": "90%", "device_batteryLevelLeft": "60%", "device_batteryLevelRight": "45%", "device_minorType": "Headphones" } },
    { "Test Pad": { "device_address": "11:22:33:44:55:04", "device_batteryLevelMain": "7%", "device_minorType": "Unknown" } },
    { "Case Only": { "device_address": "11:22:33:44:55:05", "device_batteryLevelCase": "50%", "device_minorType": "Headphones" } },
    { "Tést 🎧": { "device_address": "11:22:33:44:55:06", "device_batteryLevelMain": "33%", "device_minorType": "Headset" } }
   ],
   "device_not_connected": [
    { "Away Buds": { "device_address": "11:22:33:44:55:07", "device_batteryLevelMain": "80%", "device_minorType": "Headset" } }
   ]
  }
 ]
}"#;

    #[test]
    fn profiler_parses_connected_with_levels() {
        let d = parse_system_profiler(SP);
        let names: Vec<_> = d.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Test Keyboard", "Test Buds", "Test Pad", "T\u{e9}st \u{1f3a7}"]);
        assert_eq!(d[0].key, "bt:11:22:33:44:55:01");
        assert_eq!(d[0].percent, Some(100));
        assert_eq!(d[0].kind, DeviceKind::Keyboard);
        assert_eq!(d[0].via, "Bluetooth");
        assert_eq!(d[1].percent, Some(45)); // lowest bud, case ignored
        assert_eq!(d[1].kind, DeviceKind::Headset);
        assert_eq!(d[2].percent, Some(7));
        assert_eq!(d[2].kind, DeviceKind::Other);
        assert_eq!(d[3].kind, DeviceKind::Headset);
    }

    #[test]
    fn bluetooth_reading_replaces_stale_receiver_entry() {
        let mk = |name: &str, via: &str, pct| DeviceStatus {
            name: name.into(),
            via: via.into(),
            percent: Some(pct),
            ..Default::default()
        };
        let mut out = vec![mk("MCHOSE G3 A", "2.4 GHz receiver", 70), mk("Pad", "2.4 GHz receiver", 50), mk("Kbd", "USB cable", 99)];
        replace_receiver_entries(&mut out, &[mk("MCHOSE G3 A", "Bluetooth", 71), mk("Kbd", "Bluetooth", 98)]);
        let names: Vec<_> = out.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Pad", "Kbd"], "only the same-named receiver entry goes; cable entries stay");
    }

    #[test]
    fn pmset_levels_for_connected_devices_only() {
        let conn = vec![
            ("MCHOSE G3 A".to_string(), DeviceKind::Mouse, "c9:16:ac:ac:75:e9".to_string()),
            ("LB Hi75C Pro 5.0".to_string(), DeviceKind::Keyboard, "f6:0d:06:17:0a:d2".to_string()),
        ];
        let text = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=36700259)\t100%; charged; 0:00 remaining present: true\n -MCHOSE G3 A (id=50272119)\t70%; \n -LB Hi75C Pro 5.0  (id=50272120)\t100%; \n -Other Pad (id=1)\t50%; \n";
        let d = parse_pmset(text, &conn);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].name.as_str(), d[0].percent, d[0].kind), ("MCHOSE G3 A", Some(70), DeviceKind::Mouse));
        assert_eq!(d[0].key, "bt:c9:16:ac:ac:75:e9");
        assert_eq!(d[1].percent, Some(100));
        assert!(parse_pmset("", &conn).is_empty());
    }

    #[test]
    fn profiler_garbage_is_empty() {
        assert!(parse_system_profiler("").is_empty());
        assert!(parse_system_profiler("not json {").is_empty());
        assert!(parse_system_profiler("{\"x\":1}").is_empty());
        assert!(parse_system_profiler("{\"SPBluetoothDataType\":[{\"device_connected\":[{\"A\":{\"device_batteryLevelMain\":\"150%\"}}]}]}").is_empty());
    }

    #[test]
    fn level_choice() {
        assert_eq!(pick_level(Some(80), Some(10), Some(20)), Some(80));
        assert_eq!(pick_level(None, Some(10), Some(20)), Some(10));
        assert_eq!(pick_level(None, None, Some(20)), Some(20));
        assert_eq!(pick_level(None, None, None), None);
        assert_eq!(parse_percent(" 42% "), Some(42));
        assert_eq!(parse_percent("101%"), None);
        assert_eq!(parse_percent("n/a"), None);
    }

    // Shape of `ioreg -r -k BatteryPercent -l -w0` for an Apple HID device (written from the
    // documented property names; no Apple HID battery device was attached on the build machine).
    const IO: &str = "+-o AppleDeviceManagementHIDEventService  <class AppleDeviceManagementHIDEventService, id 0x1, registered, matched, active, busy 0 (0 ms), retain 7>\n\
    |   {\n\
    |     \"Product\" = \"Test Magic Mouse\"\n\
    |     \"DeviceAddress\" = \"AA-BB-CC-11-22-33\"\n\
    |     \"Transport\" = \"Bluetooth\"\n\
    |     \"BatteryPercent\" = 87\n\
    |   }\n\
    +-o AppleDeviceManagementHIDEventService  <class AppleDeviceManagementHIDEventService, id 0x2, registered>\n\
    |   {\n\
    |     \"Product\" = \"Test Magic Keyboard\"\n\
    |     \"Transport\" = \"USB\"\n\
    |     \"BatteryPercent\" = 55\n\
    |   }\n\
    +-o AppleDeviceManagementHIDEventService  <class X, id 0x3>\n\
    |   {\n\
    |     \"Product\" = \"Test Trackpad\"\n\
    |     \"BatteryPercent\" = 12\n\
    |   }\n";

    #[test]
    fn ioreg_parses_bluetooth_hid() {
        let d = parse_ioreg(IO);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].name, "Test Magic Mouse");
        assert_eq!(d[0].key, "bt:aa:bb:cc:11:22:33");
        assert_eq!(d[0].percent, Some(87));
        assert_eq!(d[0].kind, DeviceKind::Mouse);
        assert_eq!(d[1].name, "Test Trackpad");
        assert_eq!(d[1].key, "bt:Test Trackpad");
        assert!(parse_ioreg("").is_empty());
    }

    #[test]
    fn ioreg_inline_dictionary() {
        let line = "+-o X  <class X>\n  | {\"Product\" = \"Inline Mouse\", \"BatteryPercent\" = 64, \"Transport\" = \"Bluetooth\"}\n";
        let d = parse_ioreg(line);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "Inline Mouse");
        assert_eq!(d[0].percent, Some(64));
    }

    #[test]
    fn merge_dedups_by_name_and_key() {
        let sp = parse_system_profiler(SP);
        let io = parse_ioreg(IO);
        let m = merge_macos(sp, io.clone(), vec![], &["Test Pad".to_string()]);
        assert!(m.iter().all(|d| d.name != "Test Pad"));
        assert!(m.iter().any(|d| d.name == "Test Magic Mouse"));
        let again = merge_macos(io.clone(), io, vec![], &[]);
        assert_eq!(again.len(), 2);
    }

    #[test]
    fn class_of_device_mapping() {
        assert_eq!(kind_from_class_of_device(0x240404), DeviceKind::Headset); // wearable headset
        assert_eq!(kind_from_class_of_device(0x002540), DeviceKind::Keyboard);
        assert_eq!(kind_from_class_of_device(0x002580), DeviceKind::Mouse);
        assert_eq!(kind_from_class_of_device(0x002508), DeviceKind::Gamepad);
        assert_eq!(kind_from_class_of_device(0x002504), DeviceKind::Gamepad);
        assert_eq!(kind_from_class_of_device(0x000100), DeviceKind::Other);
        assert_eq!(kind_from_class_of_device(0), DeviceKind::Other);
    }

    #[test]
    fn suffix_stripping() {
        assert_eq!(strip_service_suffix("Test Buds Hands-Free AG"), "Test Buds");
        assert_eq!(strip_service_suffix("Test Buds Hands-Free"), "Test Buds");
        assert_eq!(strip_service_suffix("Test Buds Stereo"), "Test Buds");
        assert_eq!(strip_service_suffix("Test Buds"), "Test Buds");
        assert_eq!(strip_service_suffix(" Stereo"), " Stereo"); // nothing left to keep
    }

    #[test]
    fn guid_format() {
        assert_eq!(
            format_guid(0x8c7ed206, 0x3f8a, 0x4827, [0xb3, 0xab, 0xae, 0x9e, 0x1f, 0xae, 0xfc, 0x6c]),
            "{8C7ED206-3F8A-4827-B3AB-AE9E1FAEFC6C}"
        );
    }

    fn node(inst: &str, cid: &str, name: &str) -> BtNode {
        BtNode { instance_id: inst.into(), container: Some(cid.into()), name: name.into(), ..Default::default() }
    }

    #[test]
    fn windows_groups_by_container_and_prefers_main_node() {
        let mut main = node("BTHENUM\\DEV_001122334455\\X", "{A}", "Test Buds");
        main.class_of_device = Some(0x240404);
        main.connected = Some(true);
        let mut hf = node("BTHENUM\\{0000111e}_VID\\Y", "{A}", "Test Buds Hands-Free AG");
        hf.battery = Some(70);
        hf.connected = Some(false); // HFP node reads disconnected while A2DP runs
        let d = windows_devices(&[main, hf], &[]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].key, "bt:{A}");
        assert_eq!(d[0].name, "Test Buds");
        assert_eq!(d[0].percent, Some(70));
        assert_eq!(d[0].kind, DeviceKind::Headset);
    }

    #[test]
    fn windows_main_node_disconnected_hides_device() {
        let mut main = node("BTHLE\\DEV_AA\\X", "{B}", "Test Mouse");
        main.connected = Some(false);
        let mut svc = node("BTHLEDEVICE\\{0000180f}_DEV_AA\\Z", "{B}", "Battery");
        svc.battery = Some(50);
        assert!(windows_devices(&[main, svc], &[]).is_empty());
    }

    #[test]
    fn windows_prefers_live_node_and_filters() {
        let mut live = node("BTHENUM\\A", "{C}", "Test Keyboard");
        live.battery = Some(40);
        live.connected = Some(true);
        let mut dead = node("BTHENUM\\B", "{C}", "Test Keyboard");
        dead.battery = Some(10);
        dead.connected = Some(false);
        let d = windows_devices(&[live.clone(), dead.clone()], &[]);
        assert_eq!(d[0].percent, Some(40));
        // a dead node seen first is replaced by a live one
        let d = windows_devices(&[dead, live], &[]);
        assert_eq!(d[0].percent, Some(40));
        assert_eq!(d[0].kind, DeviceKind::Keyboard); // guess_kind fallback, no class of device

        let mut prob = node("BTHENUM\\P", "{D}", "X");
        prob.battery = Some(40);
        prob.has_problem = true;
        let mut over = node("BTHENUM\\O", "{E}", "Y");
        over.battery = Some(101);
        let mut other = node("USB\\Q", "{F}", "Z");
        other.battery = Some(40);
        assert!(windows_devices(&[prob, over, other], &[]).is_empty());
    }

    #[test]
    fn windows_skips_known_names_and_defaults_name() {
        let mut a = node("BTHENUM\\A", "{G}", "Test Headset");
        a.battery = Some(40);
        assert!(windows_devices(&[a], &["Test Headset".to_string()]).is_empty());
        let mut b = node("BTHENUM\\B", "{H}", "");
        b.battery = Some(20);
        let d = windows_devices(&[b], &[]);
        assert_eq!(d[0].name, "Bluetooth device");
    }

    #[test]
    fn windows_node_without_container_uses_instance_id() {
        let mut a = node("BTHENUM\\A", "", "Solo");
        a.container = None;
        a.battery = Some(5);
        let d = windows_devices(&[a], &[]);
        assert_eq!(d[0].key, "bt:BTHENUM\\A");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn run_times_out_and_missing_tool_is_silent() {
        let t = std::time::Duration::from_millis(300);
        assert_eq!(run("/nonexistent/tool", &[], t), "");
        let start = std::time::Instant::now();
        assert_eq!(run("/bin/sleep", &["5"], t), "");
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(run("/bin/echo", &["hi"], t).trim(), "hi");
    }
}

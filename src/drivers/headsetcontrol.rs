//! Optional bridge to HeadsetControl (github.com/Sapd/HeadsetControl), which supports many more
//! headsets than the built-in drivers. If `headsetcontrol` (`headsetcontrol.exe` on Windows) sits
//! next to the app's executable or is on PATH, it is run with `-b -o json` and any headset not
//! already handled here is added. Without it the driver quietly does nothing.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4};

// ---- minimal JSON reader (objects, arrays, strings, numbers, literals) -----------------------

#[derive(Debug, Default, Clone, PartialEq)]
enum Json {
    #[default]
    Null,
    Bool,
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(k),
            _ => None,
        }
    }
    /// String content, empty for any other type (like the C++ `str` member).
    fn str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            _ => "",
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
                let mut m = BTreeMap::new();
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
                    m.insert(key, v);
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
            c @ (b't' | b'f' | b'n') => {
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.p += 1;
                }
                Some(if c == b'n' { Json::Null } else { Json::Bool })
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

    /// Reads a string; the cursor is on the opening quote. Escapes other than \n and \t keep the
    /// escaped character, \uXXXX becomes '?' (only ASCII fields are used).
    fn string(&mut self) -> Option<String> {
        self.p += 1;
        let mut out = Vec::new();
        while self.p < self.s.len() && self.s[self.p] != b'"' {
            if self.s[self.p] == b'\\' && self.p + 1 < self.s.len() {
                self.p += 1;
                let c = self.s[self.p];
                self.p += 1;
                match c {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        self.p = (self.p + 4).min(self.s.len());
                        out.push(b'?');
                    }
                    c => out.push(c),
                }
            } else {
                out.push(self.s[self.p]);
                self.p += 1;
            }
        }
        if self.p >= self.s.len() {
            return None;
        }
        self.p += 1;
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

/// `strtoul(s, nullptr, 16)` truncated to 16 bits: optional leading blanks and 0x prefix,
/// stops at the first non-hex character.
fn parse_hex16(s: &str) -> u16 {
    let t = s.trim_start();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let mut v: u64 = 0;
    for c in t.chars() {
        let Some(d) = c.to_digit(16) else { break };
        v = v.wrapping_mul(16).wrapping_add(d as u64);
    }
    v as u16
}

/// One headset as last reported by HeadsetControl.
#[derive(Clone, Debug)]
pub struct HscDevice {
    pub vid: u16,
    pub pid: u16,
    pub status: DeviceStatus,
}

/// Parses `headsetcontrol -o json` output (text before the first `{` is ignored). Entries missing
/// `id_vendor`, `id_product` or a `battery` object are skipped; garbage gives an empty list.
pub fn parse(text: &str) -> Vec<HscDevice> {
    let mut out = Vec::new();
    let Some(start) = text.find('{') else { return out };
    let mut ps = Parser { s: text[start..].as_bytes(), p: 0 };
    let Some(root) = ps.parse(0) else { return out };
    let Some(Json::Arr(devices)) = root.get("devices") else { return out };
    for d in devices {
        let (Some(vid_j), Some(pid_j), Some(bat)) = (d.get("id_vendor"), d.get("id_product"), d.get("battery")) else {
            continue;
        };
        if !matches!(bat, Json::Obj(_)) {
            continue;
        }
        let vid = parse_hex16(vid_j.str());
        let pid = parse_hex16(pid_j.str());
        let st = bat.get("status").map_or("", Json::str);
        let name = d.get("device").map_or("", Json::str);
        let mut s = DeviceStatus {
            key: format!("hsc:{}:{}", hex4(vid), hex4(pid)),
            name: if name.is_empty() { "Headset".into() } else { name.into() },
            kind: DeviceKind::Headset,
            via: "HeadsetControl".into(),
            charging: st == "BATTERY_CHARGING",
            connected: st == "BATTERY_AVAILABLE" || st == "BATTERY_CHARGING",
            ..Default::default()
        };
        if let (true, Some(Json::Num(n))) = (s.connected, bat.get("level")) {
            if *n >= 0.0 {
                s.percent = Some(clamp100(*n as i32));
            }
        }
        out.push(HscDevice { vid, pid, status: s });
    }
    out
}

// ---- running the executable -------------------------------------------------------------------

#[cfg(windows)]
const EXE_NAME: &str = "headsetcontrol.exe";
#[cfg(not(windows))]
const EXE_NAME: &str = "headsetcontrol";

/// Next to the app's own executable first, then on PATH.
fn find_exe() -> Option<PathBuf> {
    if let Some(local) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(EXE_NAME))) {
        if local.is_file() {
            return Some(local);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(EXE_NAME)).find(|p| p.is_file())
}

fn exe() -> Option<&'static PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(find_exe).as_ref()
}

/// Only the battery is requested (-b), with a short HID timeout so a sleeping headset cannot
/// stall it.
fn args(test_device: bool) -> Vec<&'static str> {
    let mut a = vec!["-b", "-o", "json", "--timeout", "1500"];
    if test_device {
        a.push("--test-device"); // for development
    }
    a
}

/// Runs the program and returns its stdout (empty on failure or after `timeout`).
fn run(program: &std::path::Path, args: &[&str], timeout: Duration) -> String {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else { return String::new() };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return String::new();
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(1 << 20).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let out = match rx.recv_timeout(timeout) {
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => {
            let _ = child.kill();
            String::new()
        }
    };
    let _ = child.wait();
    out
}

// HeadsetControl can take several seconds per headset, so after the first poll it runs on its
// own thread and the regular poll uses its latest result instead of waiting for it.
struct Cache {
    devices: Vec<HscDevice>,
    updated: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { devices: Vec::new(), updated: None });
static RUNNING: AtomicBool = AtomicBool::new(false);

const REFRESH_AFTER: Duration = Duration::from_secs(50);

fn refresh() {
    let found = match exe() {
        Some(p) => parse(&run(p, &args(std::env::var_os("WB_HSC_TEST_DEVICE").is_some()), Duration::from_secs(30))),
        None => Vec::new(),
    };
    if let Ok(mut c) = CACHE.lock() {
        c.devices = found;
        c.updated = Some(Instant::now());
    }
}

pub fn poll(ctx: &mut PollContext) {
    if exe().is_none() {
        return;
    }
    let updated = CACHE.lock().ok().and_then(|c| c.updated);
    match updated {
        None => refresh(), // first poll: wait for it, so the first display (and --list) is complete
        Some(t) if t.elapsed() > REFRESH_AFTER => {
            if RUNNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                let spawned = std::thread::Builder::new().spawn(|| {
                    refresh();
                    RUNNING.store(false, Ordering::Release);
                });
                if spawned.is_err() {
                    RUNNING.store(false, Ordering::Release);
                }
            }
        }
        _ => {}
    }
    let devices = CACHE.lock().map(|c| c.devices.clone()).unwrap_or_default();
    for h in devices {
        if !ctx.is_claimed(h.vid, h.pid) {
            ctx.add(h.status, h.vid, h.pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"Some banner
{
  "name": "HeadsetControl", "version": "3.0.0", "api_version": "1.0",
  "device_count": 2,
  "devices": [
    {
      "status": "success", "device": "SteelSeries Arctis 7", "vendor": "SteelSeries", "product": "Arctis 7",
      "id_vendor": "0x1038", "id_product": "0x12ad",
      "capabilities": ["CAP_BATTERY_STATUS"],
      "battery": { "status": "BATTERY_AVAILABLE", "level": 73 }
    },
    {
      "device": "Logitech G733", "id_vendor": "0x046d", "id_product": "0x0ab5",
      "battery": { "status": "BATTERY_CHARGING", "level": 120 }
    },
    { "device": "No battery", "id_vendor": "0x1", "id_product": "0x2" },
    { "device": "Unavailable", "id_vendor": "0x3", "id_product": "0x4", "battery": { "status": "BATTERY_UNAVAILABLE", "level": -1 } },
    { "id_vendor": "0x5", "id_product": "0x6", "battery": { "status": "BATTERY_AVAILABLE", "level": 5.9 } }
  ]
}"#;

    #[test]
    fn parses_headsets() {
        let d = parse(SAMPLE);
        assert_eq!(d.len(), 4);
        let a = &d[0];
        assert_eq!((a.vid, a.pid), (0x1038, 0x12AD));
        assert_eq!(a.status.key, "hsc:1038:12AD");
        assert_eq!(a.status.name, "SteelSeries Arctis 7");
        assert_eq!(a.status.via, "HeadsetControl");
        assert_eq!(a.status.kind, DeviceKind::Headset);
        assert_eq!(a.status.percent, Some(73));
        assert!(a.status.connected && !a.status.charging);
    }

    #[test]
    fn charging_clamps_and_unavailable_is_disconnected() {
        let d = parse(SAMPLE);
        assert!(d[0].status.connected);
        assert!(d[1].status.charging && d[1].status.connected);
        assert_eq!(d[1].status.percent, Some(100));
        let u = d.iter().find(|h| h.vid == 3).unwrap();
        assert!(!u.status.connected && u.status.percent.is_none());
    }

    #[test]
    fn missing_name_defaults_and_fraction_truncates() {
        let d = parse(SAMPLE);
        let h = d.iter().find(|h| h.vid == 5).unwrap();
        assert_eq!(h.status.name, "Headset");
        assert_eq!(h.status.percent, Some(5));
    }

    #[test]
    fn bad_input_gives_empty() {
        assert!(parse("").is_empty());
        assert!(parse("no json here").is_empty());
        assert!(parse("{\"devices\": [").is_empty());
        assert!(parse("{\"devices\": {}}").is_empty());
        assert!(parse("[1,2]").is_empty());
        assert!(parse("{\"devices\": []}").is_empty());
        let deep = format!("{{\"devices\":{}1{}}}", "[".repeat(40), "]".repeat(40));
        assert!(parse(&deep).is_empty());
    }

    #[test]
    fn json_strings_escapes_and_literals() {
        let t = r#"{"devices":[{"device":"A\"B\\C\u00e9\n","id_vendor":"0X1038","id_product":"12ad","flag":true,"x":null,"y":false,
            "battery":{"status":"BATTERY_AVAILABLE","level":1e1}}]}"#;
        let d = parse(t);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].status.name, "A\"B\\C?\n");
        assert_eq!((d[0].vid, d[0].pid), (0x1038, 0x12AD));
        assert_eq!(d[0].status.percent, Some(10));
    }

    #[test]
    fn hex_ids() {
        assert_eq!(parse_hex16("0x046d"), 0x046D);
        assert_eq!(parse_hex16("  1038"), 0x1038);
        assert_eq!(parse_hex16("zz"), 0);
        assert_eq!(parse_hex16("0x1ffff"), 0xFFFF);
    }

    #[test]
    fn command_line() {
        assert_eq!(args(false), ["-b", "-o", "json", "--timeout", "1500"]);
        assert_eq!(args(true).last(), Some(&"--test-device"));
    }

    #[cfg(unix)]
    #[test]
    fn run_captures_stdout_and_times_out() {
        let sh = std::path::Path::new("/bin/sh");
        assert_eq!(run(sh, &["-c", "echo hi"], Duration::from_secs(5)).trim(), "hi");
        let t = Instant::now();
        assert_eq!(run(sh, &["-c", "exec sleep 5"], Duration::from_millis(100)), "");
        assert!(t.elapsed() < Duration::from_secs(3));
        assert_eq!(run(std::path::Path::new("/nonexistent/x"), &[], Duration::from_secs(1)), "");
    }
}

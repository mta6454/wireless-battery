//! ATK N9 Pro headset (VID 373B: dongle 11CB, wired 11CA). Unlike other headsets it is not
//! queried over HID but over a USB virtual COM port (9600 8N1), with 64-byte frames:
//!   request [len = 2][cmd][sub][0...]        reply [len][cmd][sub][data...]
//!   cmd 00 sub 04 = status  -> data[1] == 1 when the headset is connected to the dongle
//!   cmd 00 sub 03 = battery -> data[1] = percent (bits 0-6), charging (bit 7)
//! Protocol from ATK's own configuration software (ATK HUB, "n9-pro" model).
//! If the ATK desktop app is running it holds the port and the headset is skipped.
//! Ports are found through serialport's USB enumeration (COMx on Windows, /dev/cu.usbmodem* on
//! macOS), replacing the SetupAPI/registry lookup of the C++ original.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPortType, StopBits};

use super::PollContext;
use crate::device::{DeviceKind, DeviceStatus};
use crate::hid::{clamp100, hex4};

const VID: u16 = 0x373B;
const PID_DONGLE: u16 = 0x11CB;
const PID_WIRED: u16 = 0x11CA;
const FRAME: usize = 64;
const REPLY_MS: u64 = 800;

/// Minimal byte-stream view of the port so tests can script it.
trait Link {
    /// Writes the whole buffer; false on any failure.
    fn write_all(&mut self, data: &[u8]) -> bool;
    /// Reads whatever is available (a timeout is `Some(0)`); `None` on a real error.
    fn read_some(&mut self, buf: &mut [u8]) -> Option<usize>;
}

/// The request frame for (cmd, sub).
fn build_request(cmd: u8, sub: u8) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[0] = 2;
    f[1] = cmd;
    f[2] = sub;
    f
}

/// Consumes whole 64-byte frames from `buf`, dropping unsolicited ones, and returns the data
/// bytes (frame[3..64]) of the first frame answering (cmd, sub). A partial frame stays buffered.
fn take_reply(buf: &mut Vec<u8>, cmd: u8, sub: u8) -> Option<Vec<u8>> {
    while buf.len() >= FRAME {
        let hit = buf[1] == cmd && buf[2] == sub;
        let frame: Vec<u8> = buf.drain(..FRAME).collect();
        if hit {
            return Some(frame[3..].to_vec());
        }
    }
    None
}

/// Sends (cmd, sub) and returns the data bytes of the matching reply within `total`.
fn command(link: &mut impl Link, cmd: u8, sub: u8, total: Duration) -> Option<Vec<u8>> {
    if !link.write_all(&build_request(cmd, sub)) {
        return None;
    }
    let mut buf = Vec::new();
    let end = Instant::now() + total;
    while Instant::now() < end {
        let mut chunk = [0u8; 128];
        let n = link.read_some(&mut chunk)?;
        buf.extend_from_slice(&chunk[..n]);
        if let Some(d) = take_reply(&mut buf, cmd, sub) {
            return Some(d);
        }
    }
    None
}

/// Status reply -> headset connected to the dongle.
fn parse_connected(data: &[u8]) -> bool {
    data.get(1) == Some(&1)
}

/// Battery reply -> (percent, charging).
fn parse_battery(data: &[u8]) -> Option<(u8, bool)> {
    let b = *data.get(1)?;
    Some((clamp100((b & 0x7F) as i32), b & 0x80 != 0))
}

fn via_for(pid: u16) -> &'static str {
    if pid == PID_WIRED { "USB cable" } else { "2.4 GHz receiver" }
}

/// Queries one link and builds the status (None when the status query gets no answer).
fn read_status(link: &mut impl Link, pid: u16, total: Duration) -> Option<DeviceStatus> {
    let data = command(link, 0x00, 0x04, total)?;
    let mut s = DeviceStatus {
        key: format!("atk-serial:{}", hex4(pid)),
        name: "ATK N9 Pro".into(),
        kind: DeviceKind::Headset,
        via: via_for(pid).into(),
        connected: parse_connected(&data),
        ..Default::default()
    };
    if s.connected {
        if let Some(d) = command(link, 0x00, 0x03, total) {
            if let Some((p, c)) = parse_battery(&d) {
                s.percent = Some(p);
                s.charging = c;
            }
        }
    }
    Some(s)
}

/// Drops `tty.X` entries when `cu.X` is listed too (macOS lists both nodes of one port).
fn prefer_cu(ports: Vec<(String, u16)>) -> Vec<(String, u16)> {
    let names: Vec<String> = ports.iter().map(|p| p.0.clone()).collect();
    ports
        .into_iter()
        .filter(|(n, _)| match n.rsplit_once("/tty.") {
            Some((dir, rest)) => !names.contains(&format!("{dir}/cu.{rest}")),
            None => true,
        })
        .collect()
}

fn find_ports() -> Vec<(String, u16)> {
    let Ok(list) = serialport::available_ports() else { return vec![] };
    let ports = list
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(u) if u.vid == VID && (u.pid == PID_DONGLE || u.pid == PID_WIRED) => {
                Some((p.port_name, u.pid))
            }
            _ => None,
        })
        .collect();
    prefer_cu(ports)
}

struct Serial(Box<dyn serialport::SerialPort>);

impl Serial {
    /// None when the port cannot be opened (ATK app holds it) or configured.
    fn open(name: &str) -> Option<Self> {
        let mut p = serialport::new(name, 9600)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(Duration::from_millis(300))
            .open()
            .ok()?;
        p.write_data_terminal_ready(true).ok()?;
        p.write_request_to_send(true).ok()?;
        p.clear(ClearBuffer::All).ok()?;
        Some(Self(p))
    }
}

impl Link for Serial {
    fn write_all(&mut self, data: &[u8]) -> bool {
        Write::write_all(&mut self.0, data).is_ok() && self.0.flush().is_ok()
    }
    fn read_some(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self.0.read(buf) {
            Ok(n) => Some(n),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Some(0),
            Err(_) => None,
        }
    }
}

pub fn poll(ctx: &mut PollContext) {
    for (name, pid) in find_ports() {
        if ctx.is_claimed(VID, pid) {
            continue;
        }
        let Some(mut port) = Serial::open(&name) else { continue };
        let Some(s) = read_status(&mut port, pid, Duration::from_millis(REPLY_MS)) else { continue };
        ctx.add(s, VID, pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted link: each `read_some` pops one chunk; an empty script behaves like a timeout.
    struct Mock {
        chunks: Vec<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        accept: bool,
        fail_read: bool,
    }

    impl Mock {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks, sent: vec![], accept: true, fail_read: false }
        }
    }

    impl Link for Mock {
        fn write_all(&mut self, data: &[u8]) -> bool {
            self.sent.push(data.to_vec());
            self.accept
        }
        fn read_some(&mut self, buf: &mut [u8]) -> Option<usize> {
            if self.fail_read {
                return None;
            }
            if self.chunks.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
                return Some(0);
            }
            let c = self.chunks.remove(0);
            buf[..c.len()].copy_from_slice(&c);
            Some(c.len())
        }
    }

    fn reply(cmd: u8, sub: u8, d1: u8) -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[0] = 3;
        f[1] = cmd;
        f[2] = sub;
        f[4] = d1;
        f
    }

    const T: Duration = Duration::from_millis(100);

    #[test]
    fn request_layout() {
        let f = build_request(0, 4);
        assert_eq!(f.len(), 64);
        assert_eq!(&f[..4], &[2, 0, 4, 0]);
        assert!(f[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn reply_data_starts_after_header() {
        let mut m = Mock::new(vec![reply(0, 4, 1)]);
        let d = command(&mut m, 0, 4, T).unwrap();
        assert_eq!(d.len(), 61);
        assert_eq!(d[1], 1);
        assert_eq!(m.sent.len(), 1);
        assert_eq!(&m.sent[0][..3], &[2, 0, 4]);
    }

    #[test]
    fn skips_unsolicited_frames() {
        let mut both = reply(0, 9, 7);
        both.extend(reply(1, 4, 7)); // two unsolicited frames in one 128-byte read
        let mut m = Mock::new(vec![both, reply(0, 4, 1)]);
        assert_eq!(command(&mut m, 0, 4, T).unwrap()[1], 1);
    }

    #[test]
    fn frame_split_across_reads() {
        let f = reply(0, 3, 0x55);
        let mut m = Mock::new(vec![f[..10].to_vec(), f[10..63].to_vec(), f[63..].to_vec()]);
        assert_eq!(command(&mut m, 0, 3, T).unwrap()[1], 0x55);
    }

    #[test]
    fn partial_frame_kept_after_skipped_one() {
        let mut first = reply(5, 5, 0);
        let second = reply(0, 3, 9);
        first.extend_from_slice(&second[..20]);
        let mut m = Mock::new(vec![first, second[20..].to_vec()]);
        assert_eq!(command(&mut m, 0, 3, T).unwrap()[1], 9);
    }

    #[test]
    fn timeout_when_no_matching_reply() {
        let start = Instant::now();
        let mut m = Mock::new(vec![reply(0, 9, 1)]);
        assert!(command(&mut m, 0, 4, Duration::from_millis(40)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn write_or_read_failure_gives_none() {
        let mut m = Mock::new(vec![reply(0, 4, 1)]);
        m.accept = false;
        assert!(command(&mut m, 0, 4, T).is_none());
        let mut m = Mock::new(vec![]);
        m.fail_read = true;
        assert!(command(&mut m, 0, 4, T).is_none());
    }

    #[test]
    fn battery_decoding() {
        assert_eq!(parse_battery(&[0, 75]), Some((75, false)));
        assert_eq!(parse_battery(&[0, 0x80 | 40]), Some((40, true)));
        assert_eq!(parse_battery(&[0, 0xFF]), Some((100, true))); // 127 clamps to 100
        assert_eq!(parse_battery(&[0]), None);
    }

    #[test]
    fn connected_headset_full_status() {
        let mut m = Mock::new(vec![reply(0, 4, 1), reply(0, 3, 0x80 | 64)]);
        let s = read_status(&mut m, PID_DONGLE, T).unwrap();
        assert!(s.connected && s.charging);
        assert_eq!(s.percent, Some(64));
        assert_eq!(s.key, "atk-serial:11CB");
        assert_eq!(s.name, "ATK N9 Pro");
        assert_eq!(s.kind, DeviceKind::Headset);
        assert_eq!(s.via, "2.4 GHz receiver");
        assert_eq!(m.sent.len(), 2);
        assert_eq!(&m.sent[1][..3], &[2, 0, 3]);
    }

    #[test]
    fn disconnected_headset_skips_battery_query() {
        let mut m = Mock::new(vec![reply(0, 4, 0)]);
        let s = read_status(&mut m, PID_WIRED, T).unwrap();
        assert!(!s.connected);
        assert_eq!(s.percent, None);
        assert_eq!(s.key, "atk-serial:11CA");
        assert_eq!(s.via, "USB cable");
        assert_eq!(m.sent.len(), 1);
    }

    #[test]
    fn battery_timeout_keeps_connected_without_percent() {
        let mut m = Mock::new(vec![reply(0, 4, 1)]);
        let s = read_status(&mut m, PID_DONGLE, Duration::from_millis(30)).unwrap();
        assert!(s.connected);
        assert_eq!(s.percent, None);
    }

    #[test]
    fn silent_port_is_skipped() {
        assert!(read_status(&mut Mock::new(vec![]), PID_DONGLE, Duration::from_millis(30)).is_none());
    }

    #[test]
    fn prefers_cu_over_tty() {
        let v = prefer_cu(vec![
            ("/dev/tty.usbmodem1".into(), 0x11CB),
            ("/dev/cu.usbmodem1".into(), 0x11CB),
            ("/dev/tty.usbmodem2".into(), 0x11CA),
            ("COM3".into(), 0x11CA),
        ]);
        let names: Vec<_> = v.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(names, ["/dev/cu.usbmodem1", "/dev/tty.usbmodem2", "COM3"]);
    }
}

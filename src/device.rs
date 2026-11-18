//! Data model shared by all drivers (port of devices.h).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DeviceKind {
    #[default]
    Other,
    Mouse,
    Headset,
    Keyboard,
    Gamepad,
}

/// One battery-powered device, as seen during a single poll.
#[derive(Clone, Debug)]
pub struct DeviceStatus {
    /// Stable identity across polls (used for the tray icon id).
    pub key: String,
    pub name: String,
    /// "2.4 GHz receiver", "Bluetooth", "USB cable", ...
    pub via: String,
    pub kind: DeviceKind,
    /// 0..=100, or `None` when unknown right now.
    pub percent: Option<u8>,
    /// Set when the device only reports coarse levels ("Low", "Full"...).
    pub level_text: String,
    pub charging: bool,
    /// Receiver answered but the device is sleeping (keep last value).
    pub asleep: bool,
    /// false: receiver present, device switched off (hidden).
    pub connected: bool,
    /// Battery voltage when reported (0 = unknown).
    pub millivolts: u32,
    /// Keep showing the last value this long if the device stops answering.
    pub grace_sec: u32,
}

impl Default for DeviceStatus {
    fn default() -> Self {
        Self {
            key: String::new(),
            name: String::new(),
            via: String::new(),
            kind: DeviceKind::Other,
            percent: None,
            level_text: String::new(),
            charging: false,
            asleep: false,
            connected: true,
            millivolts: 0,
            grace_sec: 0,
        }
    }
}

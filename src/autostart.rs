//! "Start with Windows" / "Start at login".
//!
//! Windows writes the per-user Run value `WirelessBatteryCross` = the quoted exe path, which is
//! exactly what the installer writes, so the installer's checkbox and the tray menu item stay in
//! sync and a portable copy never claims the installed copy's entry. macOS and Linux use the
//! `auto-launch` crate (a LaunchAgent on macOS).

/// Registry value name / LaunchAgent label. Differs from the original C++ app's `WirelessBattery`
/// so both can be installed side by side.
pub const NAME: &str = "WirelessBatteryCross";

/// The Run-key value for an exe path: the quoted path, as Windows expects for paths with spaces.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn run_value(exe: &str) -> String {
    format!("\"{exe}\"")
}

/// Windows paths compare case-insensitively.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn same_command(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Autostart {
        exe: String,
    }

    impl Autostart {
        pub fn new() -> Option<Self> {
            Some(Self { exe: std::env::current_exe().ok()?.to_str()?.to_string() })
        }

        pub fn is_enabled(&self) -> bool {
            let (key, name) = (wide(RUN_KEY), wide(NAME));
            let mut buf = vec![0u16; 1024];
            let mut size = (buf.len() * 2) as u32;
            // SAFETY: valid NUL-terminated strings and a buffer of `size` bytes.
            let rc = unsafe {
                RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size)
            };
            if rc != ERROR_SUCCESS {
                return false;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            same_command(&String::from_utf16_lossy(&buf[..len]), &run_value(&self.exe))
        }

        pub fn set(&self, on: bool) {
            let key = wide(RUN_KEY);
            let mut hkey: HKEY = std::ptr::null_mut();
            // SAFETY: standard registry calls with valid arguments; the key is closed below.
            unsafe {
                if RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) != ERROR_SUCCESS {
                    return;
                }
                let name = wide(NAME);
                if on {
                    let v = wide(&run_value(&self.exe));
                    RegSetValueExW(hkey, name.as_ptr(), 0, REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32);
                } else {
                    RegDeleteValueW(hkey, name.as_ptr());
                }
                RegCloseKey(hkey);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use auto_launch::{AutoLaunch, AutoLaunchBuilder};

    pub struct Autostart(AutoLaunch);

    impl Autostart {
        pub fn new() -> Option<Self> {
            let exe = std::env::current_exe().ok()?;
            AutoLaunchBuilder::new().set_app_name(NAME).set_app_path(exe.to_str()?).build().ok().map(Self)
        }

        pub fn is_enabled(&self) -> bool {
            self.0.is_enabled().unwrap_or(false)
        }

        pub fn set(&self, on: bool) {
            let _ = if on { self.0.enable() } else { self.0.disable() };
        }
    }
}

pub use imp::Autostart;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_is_the_quoted_path() {
        assert_eq!(run_value(r"C:\Program Files\Wireless Battery\WirelessBattery.exe"), r#""C:\Program Files\Wireless Battery\WirelessBattery.exe""#);
    }

    #[test]
    fn paths_compare_case_insensitively() {
        assert!(same_command(r#""C:\A\B.exe""#, r#""c:\a\b.EXE""#));
        assert!(!same_command(r#""C:\A\B.exe""#, r#""C:\A\C.exe""#));
        // The portable copy must not match the installed copy's entry.
        assert!(!same_command(&run_value(r"D:\portable\WirelessBattery.exe"), &run_value(r"C:\Users\u\AppData\Local\Programs\Wireless Battery Cross\WirelessBattery.exe")));
    }
}

//! macOS: reverse the scroll direction of a mouse wheel without touching the trackpad, and keep
//! doing it after the menu-bar app has been quit (like Mac Mouse Fix's helper).
//!
//! macOS has one "natural scrolling" switch for both, so a small background process
//! (`wireless-battery --scroll-agent`) holds an event tap that flips the deltas of wheel events.
//! Wheel mice send discrete ("not continuous") scroll events; the trackpad and Magic Mouse send
//! continuous ones with gesture phases, and those are left alone. The agent is a LaunchAgent that
//! the menu item installs or removes; it survives Exit and starts again at login.
//! Creating the tap needs the Accessibility permission, which only the user can grant; the agent
//! waits for it and starts working as soon as it is granted.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, CallbackResult, EventField,
};

pub const LABEL: &str = "app.wirelessbattery.scroll";

// ---- flipping the deltas ----------------------------------------------------------------------

/// The scroll fields of an event, so the flip can be tested without a real event.
trait Fields {
    fn int(&self, f: u32) -> i64;
    fn set_int(&mut self, f: u32, v: i64);
    fn dbl(&self, f: u32) -> f64;
    fn set_dbl(&mut self, f: u32, v: f64);
}

impl Fields for &CGEvent {
    fn int(&self, f: u32) -> i64 {
        self.get_integer_value_field(f)
    }
    fn set_int(&mut self, f: u32, v: i64) {
        self.set_integer_value_field(f, v)
    }
    fn dbl(&self, f: u32) -> f64 {
        self.get_double_value_field(f)
    }
    fn set_dbl(&mut self, f: u32, v: f64) {
        self.set_double_value_field(f, v)
    }
}

const INT_FIELDS: [u32; 4] = [
    EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
    EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
    EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
    EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
];
const FIXED_FIELDS: [u32; 2] = [
    EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
    EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
];

/// Negates the scroll deltas of a wheel event. Continuous (trackpad / Magic Mouse) events are left
/// alone. Returns true when the event was changed.
fn reverse_wheel(e: &mut impl Fields) -> bool {
    if e.int(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
        return false;
    }
    // read everything first: writing one delta can rewrite the derived ones
    let ints = INT_FIELDS.map(|f| e.int(f));
    let fixed = FIXED_FIELDS.map(|f| e.dbl(f));
    for (f, v) in INT_FIELDS.iter().zip(ints) {
        e.set_int(*f, -v);
    }
    for (f, v) in FIXED_FIELDS.iter().zip(fixed) {
        e.set_dbl(*f, -v);
    }
    true
}

// ---- permission and the tap -------------------------------------------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

/// Whether the app may use the Accessibility API. With `prompt`, macOS shows its own permission
/// dialog (the user decides; the app never changes the setting).
pub fn trusted(prompt: bool) -> bool {
    // SAFETY: the key is a static CFString owned by the system; the dictionary lives for the call.
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let opts = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::from(prompt))]);
        AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
    }
}

/// The tap's mach port, so the callback can switch it back on if macOS disabled it (it does so when
/// a callback is too slow).
static PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Runs the scroll agent forever: wait for the Accessibility permission, install the tap, serve it.
pub fn run_agent() -> ! {
    loop {
        if !trusted(false) && trusted_in_fresh_process() {
            // macOS keeps answering "not allowed" in a process that started before the grant, so
            // quit; launchd (KeepAlive) restarts the agent and the new process sees the permission.
            std::process::exit(0);
        }
        if trusted(false) {
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                vec![CGEventType::ScrollWheel],
                |_proxy, kind, event| {
                    match kind {
                        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                            let p = PORT.load(Ordering::Relaxed);
                            if !p.is_null() {
                                // SAFETY: the port stays alive while the run loop below is running.
                                unsafe { CGEventTapEnable(p, true) };
                            }
                        }
                        _ => {
                            let mut e = event;
                            reverse_wheel(&mut e);
                        }
                    }
                    CallbackResult::Keep
                },
            );
            if let Ok(tap) = tap {
                if let Ok(source) = tap.mach_port().create_runloop_source(0) {
                    PORT.store(tap.mach_port().as_concrete_TypeRef() as *mut c_void, Ordering::Relaxed);
                    // SAFETY: kCFRunLoopCommonModes is a constant owned by CoreFoundation.
                    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
                    tap.enable();
                    CFRunLoop::run_current(); // serves the tap until the process is stopped
                    PORT.store(std::ptr::null_mut(), Ordering::Relaxed);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Asks a short-lived copy of this program whether Accessibility is allowed (always a fresh answer).
fn trusted_in_fresh_process() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Command::new(exe).arg("--ax-check").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().ok())
        .is_some_and(|s| s.success())
}

// ---- the LaunchAgent --------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist_text(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>Label</key>\n\t<string>{LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>--scroll-agent</string>\n\t</array>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>KeepAlive</key>\n\t<true/>\n\
\t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
</dict>\n</plist>\n",
        xml_escape(&exe.to_string_lossy())
    )
}

fn plist_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn uid() -> Option<String> {
    let out = Command::new("/usr/bin/id").arg("-u").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn domain() -> Option<String> {
    Some(format!("gui/{}", uid()?))
}

/// Whether the agent is installed (the menu check mark).
pub fn installed() -> bool {
    plist_path().is_some_and(|p| p.exists())
}

/// Whether the agent process is loaded in launchd right now.
pub fn running() -> bool {
    domain().is_some_and(|d| launchctl(&["print", &format!("{d}/{LABEL}")]))
}

fn load(exe: &Path) -> std::io::Result<()> {
    let path = plist_path().ok_or_else(|| std::io::Error::other("no home folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, plist_text(exe))?;
    let d = domain().ok_or_else(|| std::io::Error::other("cannot tell the user id"))?;
    let _ = launchctl(&["bootout", &format!("{d}/{LABEL}")]); // reload if an older one is loaded
    if launchctl(&["bootstrap", &d, &path.to_string_lossy()]) {
        Ok(())
    } else {
        Err(std::io::Error::other("launchctl bootstrap failed"))
    }
}

/// Installs and starts the agent for the executable at `exe`.
pub fn enable(exe: &Path) -> std::io::Result<()> {
    load(exe)
}

/// Stops the agent and removes it, so it no longer starts at login.
pub fn disable() {
    if let Some(d) = domain() {
        let _ = launchctl(&["bootout", &format!("{d}/{LABEL}")]);
    }
    if let Some(p) = plist_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// If the app was moved or updated, point the installed agent at the executable that is running now.
pub fn refresh(exe: &Path) {
    let Some(path) = plist_path() else { return };
    if std::fs::read_to_string(&path).is_ok_and(|t| t != plist_text(exe)) {
        let _ = load(exe);
    }
}

/// True while Mac Mouse Fix's helper runs: it also reverses the wheel, so both together cancel out.
pub fn mouse_fix_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-f", "Mac Mouse Fix Helper"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        ints: HashMap<u32, i64>,
        dbls: HashMap<u32, f64>,
    }

    impl Fields for Fake {
        fn int(&self, f: u32) -> i64 {
            *self.ints.get(&f).unwrap_or(&0)
        }
        fn set_int(&mut self, f: u32, v: i64) {
            self.ints.insert(f, v);
        }
        fn dbl(&self, f: u32) -> f64 {
            *self.dbls.get(&f).unwrap_or(&0.0)
        }
        fn set_dbl(&mut self, f: u32, v: f64) {
            self.dbls.insert(f, v);
        }
    }

    fn wheel(continuous: i64) -> Fake {
        let mut e = Fake::default();
        e.ints.insert(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, continuous);
        e.ints.insert(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, 3);
        e.ints.insert(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, -1);
        e.ints.insert(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1, 30);
        e.dbls.insert(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1, 3.0);
        e
    }

    #[test]
    fn wheel_events_are_negated() {
        let mut e = wheel(0);
        assert!(reverse_wheel(&mut e));
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1), -3);
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2), 1);
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1), -30);
        assert_eq!(e.dbl(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1), -3.0);
        assert!(reverse_wheel(&mut e), "twice restores the original");
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1), 3);
    }

    #[test]
    fn trackpad_events_are_untouched() {
        let mut e = wheel(1);
        assert!(!reverse_wheel(&mut e));
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1), 3);
        assert_eq!(e.int(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1), 30);
    }

    #[test]
    fn plist_runs_the_agent_flag_and_escapes_the_path() {
        let t = plist_text(Path::new("/Apps/R&D <x>/WirelessBattery"));
        assert!(t.contains("<string>/Apps/R&amp;D &lt;x&gt;/WirelessBattery</string>"));
        assert!(t.contains("<string>--scroll-agent</string>"));
        assert!(t.contains(&format!("<string>{LABEL}</string>")));
        assert!(t.contains("<key>KeepAlive</key>"));
    }
}

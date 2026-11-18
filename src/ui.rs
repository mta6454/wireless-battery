//! Tray UI. macOS: one menu-bar item showing "🎧 30%  🖱 93%" as text. Windows/Linux: one tray icon
//! per device with the percentage drawn into it (the tray there can only show images).
//! A worker thread polls the devices every 60 s (or on "Refresh now") and hands results to the
//! event loop, which owns all UI objects.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use hidapi::HidApi;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::autostart::Autostart;
use crate::device::DeviceStatus;
use crate::drivers;
use crate::instance;
use crate::render::{render_icon, SIZE};
use crate::settings::Settings;
use crate::state::{status_text, summary, tooltip, Entry, Notice, Tracker};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How often the (cheap, ~3 ms) HID device list is compared to notice plugging and unplugging.
const WATCH_INTERVAL: Duration = Duration::from_millis(1500);
/// While a game (or any full-screen app) is in front: poll rarely and stop watching the HID list, so
/// the app never competes with it for the mouse or the CPU.
const GAME_POLL_INTERVAL: Duration = Duration::from_secs(300);
const MAIN_KEY: &str = "\0main"; // the single macOS item / the "no devices" placeholder

enum UserEvent {
    Results(Vec<DeviceStatus>),
    Menu(MenuEvent),
    Quit,
    /// The Windows taskbar widget was clicked: show the menu.
    #[cfg_attr(not(windows), allow(dead_code))]
    WidgetClick,
}

/// Lets the widget's window procedure reach the event loop.
#[cfg(windows)]
static WIDGET_PROXY: std::sync::Mutex<Option<EventLoopProxy<UserEvent>>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn on_widget_click() {
    if let Some(p) = WIDGET_PROXY.lock().ok().and_then(|g| g.clone()) {
        let _ = p.send_event(UserEvent::WidgetClick);
    }
}

struct App {
    tracker: Tracker,
    settings: Settings,
    trays: HashMap<String, TrayIcon>,
    refresh_tx: mpsc::Sender<()>,
    autostart: Option<Autostart>,
    #[cfg_attr(not(windows), allow(dead_code))]
    ticks: u32,
}

pub fn run() {
    // One tray app at a time; also lets `--quit` find and close this one (Windows).
    let Some(instance) = instance::acquire() else { return };
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory); // menu bar only, no Dock icon
    }
    let proxy = event_loop.create_proxy();

    let quit_proxy = proxy.clone();
    instance.watch_quit(move || {
        let _ = quit_proxy.send_event(UserEvent::Quit);
    });

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e));
    }));

    #[cfg(windows)]
    {
        *WIDGET_PROXY.lock().unwrap() = Some(proxy.clone());
    }

    let mut app = App {
        tracker: Tracker::default(),
        settings: Settings::load(),
        trays: HashMap::new(),
        refresh_tx: spawn_worker(proxy),
        autostart: Autostart::new(),
        ticks: 0,
    };

    event_loop.run(move |event, _, flow| {
        let _keep_alive = &instance; // released when the loop ends, which frees the mutex
        // The Windows widget has to follow the taskbar (Explorer moves things around), so tick once a second.
        *flow = if cfg!(windows) && app.settings.taskbar_widget {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1))
        } else {
            ControlFlow::Wait
        };
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => app.tick(),
            // Tray objects must be created once the loop is running (required on macOS).
            Event::NewEvents(StartCause::Init) => {
                #[cfg(target_os = "macos")]
                if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
                    crate::scroll::refresh(&exe); // the app may have moved since the agent was installed
                }
                app.refresh_ui();
            }
            Event::UserEvent(UserEvent::Results(r)) => {
                for n in app.tracker.on_results(r, Instant::now()) {
                    notify(n);
                }
                app.refresh_ui();
            }
            Event::UserEvent(UserEvent::Quit) => *flow = ControlFlow::Exit,
            Event::UserEvent(UserEvent::WidgetClick) => app.show_widget_menu(),
            Event::UserEvent(UserEvent::Menu(ev)) => {
                if app.on_menu(ev.id.0.as_str()) {
                    *flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}

fn spawn_worker(proxy: EventLoopProxy<UserEvent>) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut api = match HidApi::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Cannot initialise HID: {e}");
                return;
            }
        };
        let mut seen = device_signature(&api);
        let mut next_poll = Instant::now();
        let mut follow_up: Option<Instant> = None;
        let mut was_gaming = false;
        loop {
            let gaming = fullscreen_app_active();
            if was_gaming && !gaming {
                next_poll = Instant::now() + Duration::from_secs(2); // game closed: catch up
            }
            was_gaming = gaming;
            if Instant::now() >= next_poll || follow_up.is_some_and(|t| Instant::now() >= t) {
                follow_up = None;
                let _ = api.refresh_devices();
                seen = device_signature(&api);
                let results = drivers::poll_all(&api);
                if proxy.send_event(UserEvent::Results(results)).is_err() {
                    break; // event loop is gone
                }
                next_poll = Instant::now() + if gaming { GAME_POLL_INTERVAL } else { POLL_INTERVAL };
            }
            let wait = next_poll.saturating_duration_since(Instant::now()).min(WATCH_INTERVAL);
            match rx.recv_timeout(wait) {
                Err(RecvTimeoutError::Disconnected) => break,
                Ok(()) => next_poll = Instant::now(), // "refresh now"
                Err(RecvTimeoutError::Timeout) if gaming => {}
                Err(RecvTimeoutError::Timeout) => {
                    // A device appeared or went away: read it now, and once more after the link settles
                    // (a dongle shows up before its mouse has paired).
                    let _ = api.refresh_devices();
                    let now = device_signature(&api);
                    if now != seen {
                        seen = now;
                        next_poll = Instant::now() + Duration::from_millis(500);
                        follow_up = Some(Instant::now() + Duration::from_secs(6));
                    }
                }
            }
        }
    });
    tx
}

/// Windows: a full-screen game, presentation or other busy full-screen app is in front. Never true elsewhere.
#[cfg(windows)]
fn fullscreen_app_active() -> bool {
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;
    let mut state = 0;
    // SAFETY: `state` is a valid out pointer for the duration of the call.
    let hr = unsafe { SHQueryUserNotificationState(&mut state) };
    // 2 = QUNS_BUSY (full-screen window), 3 = Direct3D full screen, 4 = presentation mode
    hr >= 0 && matches!(state, 2..=4)
}
#[cfg(not(windows))]
fn fullscreen_app_active() -> bool {
    false
}

/// Identity of everything currently on the HID bus; changes when a device is plugged or unplugged.
fn device_signature(api: &HidApi) -> Vec<(u16, u16, String)> {
    let mut v: Vec<_> =
        api.device_list().map(|d| (d.vendor_id(), d.product_id(), d.path().to_string_lossy().into_owned())).collect();
    v.sort();
    v
}

/// macOS: name the sending app up front. Without this the notification library asks AppleScript to
/// look up an application called "use_default", and macOS shows a "Where is use_default?" dialog.
#[cfg(target_os = "macos")]
fn set_notification_app() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let in_bundle = std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
        // the bundle id must match Info.plist in scripts/package-macos.sh; a bare binary borrows Finder's
        let id = if in_bundle { "app.wirelessbattery.monitor" } else { "com.apple.Finder" };
        let _ = notify_rust::set_application(id);
    });
}

/// `--notify-test`: sends one notification the way the app does and waits for it to be shown.
pub fn notify_test() -> bool {
    #[cfg(target_os = "macos")]
    set_notification_app();
    notify_rust::Notification::new().summary("Wireless Battery").body("Notification test").show().is_ok()
}

fn notify(n: Notice) {
    // Showing a notification can block (macOS), so keep it off the UI thread.
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        set_notification_app();
        let _ = notify_rust::Notification::new().summary(&n.title).body(&n.body).show();
    });
}

impl App {
    fn build_menu(&self) -> Menu {
        let m = Menu::new();
        let header = if self.tracker.entries.is_empty() {
            "No devices with battery info found"
        } else {
            "Click a device to show or hide it"
        };
        let _ = m.append(&MenuItem::new(header, false, None));
        for (k, e) in &self.tracker.entries {
            let line = format!("{}:  {}   [{}]", e.st.name, status_text(e), e.st.via);
            let shown = !self.settings.hidden.contains(k);
            let _ = m.append(&CheckMenuItem::with_id(format!("dev:{k}"), line, true, shown, None));
        }
        let _ = m.append(&PredefinedMenuItem::separator());
        let _ = m.append(&CheckMenuItem::with_id(
            "hide_last_known",
            "Hide last known / asleep devices",
            true,
            self.settings.hide_last_known,
            None,
        ));
        #[cfg(target_os = "macos")]
        let _ = m.append(&CheckMenuItem::with_id(
            "reverse_scroll",
            "Reverse mouse scroll (keeps working after Exit)",
            true,
            crate::scroll::installed(),
            None,
        ));
        if cfg!(windows) {
            let _ = m.append(&CheckMenuItem::with_id(
                "taskbar_widget",
                "Show on the taskbar (icon + percentage)",
                true,
                self.settings.taskbar_widget,
                None,
            ));
        }
        let _ = m.append(&PredefinedMenuItem::separator());
        let _ = m.append(&MenuItem::with_id("refresh", "Refresh now", true, None));
        if let Some(al) = &self.autostart {
            let label = if cfg!(target_os = "windows") { "Start with Windows" } else { "Start at login" };
            let on = al.is_enabled();
            let _ = m.append(&CheckMenuItem::with_id("autostart", label, true, on, None));
        }
        let _ = m.append(&PredefinedMenuItem::separator());
        let _ = m.append(&MenuItem::with_id("quit", "Exit", true, None));
        m
    }

    /// Returns true when the app should quit.
    fn on_menu(&mut self, id: &str) -> bool {
        match id {
            "quit" => return true,
            "refresh" => {
                let _ = self.refresh_tx.send(());
            }
            "hide_last_known" => {
                self.settings.hide_last_known = !self.settings.hide_last_known;
                self.settings.save();
            }
            "reverse_scroll" => self.toggle_scroll(),
            "taskbar_widget" => {
                self.settings.taskbar_widget = !self.settings.taskbar_widget;
                self.settings.save();
            }
            "autostart" => {
                if let Some(al) = &self.autostart {
                    al.set(!al.is_enabled());
                }
            }
            other => {
                if let Some(key) = other.strip_prefix("dev:") {
                    self.settings.toggle_hidden(key);
                    self.settings.save();
                }
            }
        }
        self.refresh_ui();
        false
    }

    /// Windows: makes the taskbar strip match the chosen mode. Returns true when it is showing the
    /// devices; false means tray icons must be used (mode off, or no horizontal taskbar to attach to).
    #[cfg(windows)]
    fn sync_widget(&self, visible: &[&Entry], tip: &str) -> bool {
        if !self.settings.taskbar_widget {
            crate::widget::destroy();
            return false;
        }
        if !crate::widget::alive() && !crate::widget::create(crate::widget::light_taskbar(), on_widget_click) {
            return false;
        }
        let items = visible
            .iter()
            .map(|e| crate::widget::Item {
                kind: e.st.kind,
                percent: e.shown(),
                text: if e.st.percent.is_some() { e.st.level_text.clone() } else { String::new() },
                charging: e.st.charging,
                dim: e.st.percent.is_none(),
                low: e.is_low(),
                bluetooth: e.st.via == "Bluetooth",
            })
            .collect();
        let tip = if visible.is_empty() { "Wireless Battery: no devices with battery info found" } else { tip };
        crate::widget::set_items(items, tip);
        true
    }

    #[cfg(not(windows))]
    fn sync_widget(&self, _visible: &[&Entry], _tip: &str) -> bool {
        false
    }

    /// Once a second on Windows: follow the taskbar, the theme, and recover after an Explorer restart.
    fn tick(&mut self) {
        #[cfg(windows)]
        {
            if !self.settings.taskbar_widget {
                return;
            }
            self.ticks = self.ticks.wrapping_add(1);
            if crate::widget::alive() {
                crate::widget::set_light(crate::widget::light_taskbar());
                crate::widget::reposition();
            } else if self.ticks % 3 == 0 {
                // Explorer restarted (or the strip never attached): try again, else keep tray icons.
                crate::widget::destroy();
                self.refresh_ui();
            }
        }
    }

    fn toggle_scroll(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if crate::scroll::installed() {
                crate::scroll::disable();
                return;
            }
            let exe = std::env::current_exe().and_then(|p| p.canonicalize());
            match exe.and_then(|e| crate::scroll::enable(&e)) {
                Ok(()) => {
                    let mut body = String::from("The mouse wheel is reversed by a background agent that keeps running after Exit.");
                    if !crate::scroll::trusted(true) {
                        body = "Allow WirelessBattery in System Settings > Privacy & Security > Accessibility. The mouse wheel is reversed as soon as you do.".into();
                    }
                    if crate::scroll::mouse_fix_running() {
                        body.push_str(" Mac Mouse Fix is running too: if it also reverses scrolling, the two cancel out.");
                    }
                    notify(Notice { title: "Reverse mouse scroll".into(), body });
                }
                Err(e) => notify(Notice { title: "Cannot reverse mouse scroll".into(), body: e.to_string() }),
            }
        }
    }

    fn show_widget_menu(&self) {
        #[cfg(windows)]
        if let Some(hwnd) = crate::widget::hwnd() {
            use tray_icon::menu::ContextMenu;
            let menu = self.build_menu();
            // SAFETY: hwnd is our live widget window; muda posts the chosen item as a MenuEvent.
            unsafe { menu.show_context_menu_for_hwnd(hwnd, None) };
        }
    }

    fn refresh_ui(&mut self) {
        let visible: Vec<&Entry> = self.tracker.visible(&self.settings);
        let tip = tooltip(&visible);
        let mut wanted: Vec<(String, Option<Icon>, String)> = Vec::new(); // (key, icon, title)

        if self.sync_widget(&visible, &tip) {
            self.trays.clear(); // the strip replaces the tray icons
            return;
        }

        if cfg!(target_os = "macos") {
            wanted.push((MAIN_KEY.into(), None, summary(&visible)));
        } else if visible.is_empty() {
            wanted.push((MAIN_KEY.into(), make_icon(None, false, true), String::new()));
        } else {
            for e in &visible {
                let icon = make_icon(e.shown(), e.st.charging, e.st.percent.is_none());
                wanted.push((e.st.key.clone(), icon, String::new()));
            }
        }

        self.trays.retain(|k, _| wanted.iter().any(|(w, _, _)| w == k));
        for (key, icon, title) in wanted {
            let tip = if key == MAIN_KEY { tip.clone() } else { per_device_tip(&visible, &key) };
            let menu = self.build_menu();
            match self.trays.get(&key) {
                Some(t) => {
                    if let Some(i) = icon {
                        let _ = t.set_icon(Some(i));
                    }
                    t.set_title(Some(title));
                    let _ = t.set_tooltip(Some(tip));
                    t.set_menu(Some(Box::new(menu)));
                }
                None => {
                    let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip(tip).with_title(title);
                    if let Some(i) = icon {
                        b = b.with_icon(i);
                    }
                    match b.build() {
                        Ok(t) => {
                            self.trays.insert(key, t);
                        }
                        Err(e) => eprintln!("Cannot create tray icon: {e}"),
                    }
                }
            }
        }
    }
}

fn per_device_tip(visible: &[&Entry], key: &str) -> String {
    visible
        .iter()
        .find(|e| e.st.key == key)
        .map(|e| format!("{}\n{}", e.st.name, status_text(e)))
        .unwrap_or_else(|| "Wireless Battery".into())
}

fn make_icon(pct: Option<u8>, charging: bool, dim: bool) -> Option<Icon> {
    Icon::from_rgba(render_icon(pct, charging, dim), SIZE as u32, SIZE as u32).ok()
}

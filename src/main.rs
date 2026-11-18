// Release builds on Windows are GUI apps: no console window next to the tray icon.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod autostart;
mod device;
mod drivers;
mod hid;
mod instance;
mod render;
#[cfg(target_os = "macos")]
mod scroll;
mod settings;
mod state;
mod ui;
#[cfg(windows)]
mod widget;

use hidapi::HidApi;

const USAGE: &str = "Wireless Battery\n\nUsage: wireless-battery [--once | --dump | --quit | --reverse-scroll on|off|status | --help]\n  (no option)  run in the tray / menu bar\n  --once       print the battery levels once and exit\n  --dump       list every HID collection (for adding support for new devices)\n  --quit       close the running instance (used by the Windows installer)\n  --reverse-scroll on|off|status   macOS: reverse the mouse wheel in a background agent (keeps running after Exit)";

#[cfg(windows)]
fn attach_parent_console() {
    // A GUI-subsystem exe has no console; reuse the one it was started from so --once/--dump print.
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    unsafe {
        AttachConsole(u32::MAX); // ATTACH_PARENT_PROCESS
    }
}
#[cfg(not(windows))]
fn attach_parent_console() {}

/// `--reverse-scroll on|off|status` (macOS).
#[cfg(target_os = "macos")]
fn reverse_scroll_cli(action: Option<&str>) -> i32 {
    match action {
        Some("on") => {
            let exe = std::env::current_exe().and_then(|p| p.canonicalize());
            match exe.and_then(|e| scroll::enable(&e)) {
                Ok(()) => {
                    println!("Reverse mouse scroll: on (background agent installed)");
                    if !scroll::trusted(false) {
                        println!("Allow this app in System Settings > Privacy & Security > Accessibility; it starts working as soon as you do.");
                    }
                    if scroll::mouse_fix_running() {
                        println!("Mac Mouse Fix is running too: if it also reverses scrolling, the two cancel out.");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("Cannot install the agent: {e}");
                    1
                }
            }
        }
        Some("off") => {
            scroll::disable();
            println!("Reverse mouse scroll: off");
            0
        }
        Some("status") => {
            println!(
                "agent installed: {}, running: {}, accessibility allowed: {}",
                scroll::installed(),
                scroll::running(),
                scroll::trusted(false)
            );
            0
        }
        _ => {
            eprintln!("usage: wireless-battery --reverse-scroll on|off|status");
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    if has("--help") || has("-h") {
        attach_parent_console();
        println!("{USAGE}");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        if has("--ax-check") {
            std::process::exit(if scroll::trusted(false) { 0 } else { 1 });
        }
        if has("--scroll-agent") {
            scroll::run_agent(); // never returns
        }
        if let Some(i) = args.iter().position(|a| a == "--reverse-scroll") {
            std::process::exit(reverse_scroll_cli(args.get(i + 1).map(String::as_str)));
        }
    }
    if has("--notify-test") {
        attach_parent_console();
        let ok = ui::notify_test();
        println!("notification {}", if ok { "sent" } else { "failed" });
        std::process::exit(if ok { 0 } else { 1 });
    }
    if has("--quit") {
        // Used by the installer before upgrading or uninstalling; silent unless it fails.
        attach_parent_console();
        std::process::exit(if instance::request_quit() { 0 } else { 1 });
    }
    if has("--dump") || has("--once") {
        attach_parent_console();
        let api = match HidApi::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Cannot initialise HID: {e}");
                std::process::exit(1);
            }
        };
        if has("--dump") {
            println!("VID:PID    IF  UsagePg:Usage  In  Out Feat  Product");
            for c in hid::enum_hid(&api) {
                println!("{:04X}:{:04X}  {:2}  {:04X}:{:04X}    {:3} {:3} {:3}   {}", c.vid, c.pid, c.iface, c.usage_page, c.usage, c.in_len, c.out_len, c.feat_len, c.product);
            }
        } else {
            let list = drivers::poll_all(&api);
            if list.is_empty() {
                println!("No devices with battery info found.");
            }
            for d in list {
                let pct = d.percent.map(|p| format!("{p}%")).unwrap_or_else(|| "?".into());
                println!(
                    "{:<34} {:>4}  {:?}  via {}{}{}",
                    d.name, pct, d.kind, d.via,
                    if d.charging { "  charging" } else { "" },
                    if d.asleep { "  asleep" } else { "" }
                );
            }
        }
        return;
    }
    ui::run();
}

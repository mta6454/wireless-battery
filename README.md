# Wireless Battery (cross-platform)

Battery levels of your wireless mouse, headset, keyboard and controller, in the **Windows** system tray and the **macOS** menu bar.

It is written in Rust, runs on both platforms from one code base, and builds on the research of the projects listed under [Credits](#credits).

> **Status: early.** Every driver has unit tests against scripted mock devices, and CI builds and tests on Windows and macOS, but **the drivers have not yet been verified against real hardware**. If you own one of the supported devices, please report whether it works (see [CONTRIBUTING.md](CONTRIBUTING.md)).

- **Small:** a single native binary of about 1.3 MB, no runtime, no background service.
- **No vendor software needed:** it talks to the devices directly. G HUB, Synapse, iCUE and the like can stay closed.
- **Many brands:** Logitech, Razer, SteelSeries, Corsair, HyperX, Glorious, PlayStation and Xbox controllers, Bluetooth devices the OS knows a battery level for, and every headset [HeadsetControl](https://github.com/Sapd/HeadsetControl) supports.

## Display

| | Windows | macOS |
|---|---|---|
| Where | one tray icon per device, with the percentage drawn into the icon | one menu-bar item with text, e.g. `🎧 30%  🖱 93%⚡` |
| Low battery | red number at 15% or less, plus a notification | `⚠` after the number, plus a notification |
| Charging | blue bar | `⚡` after the number |
| Asleep | dimmed icon, last known value | `~` before the last known value |

Hover (Windows) for details. Click for the menu: every device (click one to hide or show it), *Hide last known / asleep devices*, *Refresh now*, *Start at login* and *Exit*. Levels refresh every 30 seconds; plugging in or unplugging a device is noticed within about two seconds. While a full-screen game or presentation is in front (Windows), it reads only every 5 minutes and stops watching for plugged devices, so it stays out of the way; *Refresh now* still works.

On Windows 10/11 with a horizontal taskbar the default is a taskbar widget: one strip next to the notification area (`🎧 30%  🖱 93%⚡`, dimmed when asleep, red when low). Switch it off in the menu (*Show on the taskbar*) to get one tray icon per device instead; a vertical taskbar uses tray icons automatically.

### Reverse mouse scroll (macOS)

The menu has *Reverse mouse scroll (keeps working after Exit)*. It reverses the wheel of a regular mouse and leaves the trackpad and Magic Mouse alone. Like Mac Mouse Fix, it does this with a small background agent (`wireless-battery --scroll-agent`, installed as the LaunchAgent `app.wirelessbattery.scroll`) that keeps running after you quit the menu-bar app and starts again at login. Choosing the item again removes the agent.

It needs the *Accessibility* permission: macOS asks the first time; allow *WirelessBattery* under *System Settings > Privacy & Security > Accessibility*, and the agent starts working without a restart. Because the app is only ad-hoc signed, macOS may ask again after an update. If Mac Mouse Fix (or another tool) also reverses the wheel, the two cancel out, so use only one. The same switch is available from the terminal: `wireless-battery --reverse-scroll on|off|status`.

## Install

Download the file for your system from the Releases page of this repository (or from the artifacts of a CI run).:

- **Windows:** `WirelessBattery-Setup-<version>.exe`. Run it and click through. It needs **no administrator rights** and nothing else installed (HeadsetControl comes bundled). It adds a Start menu entry and an uninstaller (*Settings → Apps*), and can start the app when you sign in. Prefer no installer? Download `WirelessBattery-<version>-portable.zip`, unzip it anywhere and run `WirelessBattery.exe`.
- **macOS:** `WirelessBattery-<version>-macos.zip` (universal: Apple silicon and Intel, macOS 11 or later). Unzip, move `WirelessBattery.app` to *Applications* and open it. It lives in the menu bar and has no Dock icon.

**Notes:**
- **Windows SmartScreen:** the exe is not code-signed, so Windows may say "Windows protected your PC". Click *More info → Run anyway*, or build it yourself.
- **Windows 11:** new tray icons go to the `^` overflow. Drag them onto the taskbar, or enable them in *Settings → Personalization → Taskbar → Other system tray icons*.
- **macOS Gatekeeper:** the app is only ad-hoc signed, not notarized. The first time, right-click it, choose *Open*, and confirm. If macOS still refuses, run `xattr -dr com.apple.quarantine /Applications/WirelessBattery.app`.
- **macOS permissions:** macOS only lets an app open mice and keyboards over HID after you grant it *System Settings → Privacy & Security → Input Monitoring*. Without it, the app cannot read the standard mouse and keyboard parts of a device. Vendor-specific interfaces, which most battery protocols use, are not affected.
- **HeadsetControl:** the Windows installer and portable zip bundle it. On macOS (or a bare `.exe`), put the `headsetcontrol` executable on your `PATH` (or next to the exe on Windows). Without it you only lose the extra headsets it supports.

## Command line

The binary is `wireless-battery` (`.exe` on Windows) when built from source, and `WirelessBattery.exe` / `WirelessBattery.app/Contents/MacOS/WirelessBattery` in the release zips. Only one copy runs at a time; starting a second one exits quietly.

```
wireless-battery            # run in the tray / menu bar
wireless-battery --once     # print every device and its battery level, then exit
wireless-battery --dump     # print every HID collection (useful when asking for a new device)
wireless-battery --quit     # close the running instance (the Windows installer uses this before upgrading)
wireless-battery --help
```

Hidden devices and the *Hide last known* choice are stored in `%APPDATA%\WirelessBattery\settings.txt` (Windows) or `~/Library/Application Support/WirelessBattery/settings.txt` (macOS). The installer's uninstaller removes the Windows one.

## Supported devices

| Vendor | Devices | How |
|---|---|---|
| **Logitech** | Mice and keyboards on Unifying, Bolt, Lightspeed and Nano receivers, over USB or Bluetooth; G headsets (G533, G535, G633/G635/G733/G933/G935, G PRO/PRO X) | HID++ 1.0 / 2.0 with feature discovery: unified battery, battery level, voltage, ADC |
| **Razer** | Wireless mice, keyboards and dongles; BlackShark V3 / V3 Pro headsets | Razer feature-report protocol; headset envelope protocol (queried every 5 min) |
| **SteelSeries** | Arctis 1/7/7+/9/Pro/Pro Wireless, Arctis Nova 3P/5/7/7X/7P/Pro Wireless, GameBuds; Aerox 3/5/9 Wireless, Prime Wireless, Rival 3 Wireless (Gen 2), Rival 650, Rival Pro | Vendor HID reports |
| **Corsair** | Void / Void Pro / Void Elite / HS70 / HS80 wireless; Bragi devices (Virtuoso, Void V2, HS80 MAX, Slipstream mice and keyboards) | Vendor HID reports, Bragi `GET` (read-only) |
| **Cooler Master** | MM712, MM731 (2.4 GHz dongle or USB cable) | Vendor HID status report (read-only) |
| **HyperX** | Cloud III Wireless, Cloud III S Wireless, Cloud II Wireless (HP, Kingston, DTS), Cloud II Core Wireless, Cloud Alpha Wireless, Cloud Alpha 2 Wireless, Cloud Flight, Cloud Flight S | Vendor HID reports |
| **Glorious** | Model O / O- / D / D- Wireless, Model O 2 / D 2 PRO | Vendor feature report |
| **Hitscan** | Hyperlight X, Hyperlight N | Vendor feature report (same protocol as Glorious) |
| **Kysona / Pulsar / ATK (VXE) / Hitscan** | Kysona M600 / Mercury, Pulsar and ATK VXE wireless mice (e.g. VXE X3 Pro, ATK F1 Extreme), Hitscan Hyperlight | Shared vendor protocol with checksummed replies |
| **ATK headsets** | ATK N9 Pro | Virtual COM port protocol (the ATK desktop app must be closed) |
| **G-Wolves** | Fenrir, Fenrir Pro / Max / Asym, HTX, HTS, HSK, HTR, HT-S2, Lycan, WARG, VUK and their 8K dongles | Glorious or Kysona protocol, depending on the model |
| **Keychron mice** | M4 and other Keychron mice on the 2.4 GHz receiver; 8K mice (M5 8K, G6 HE 8K ...) on the TurboLink 8K receiver | Vendor report from Keychron Launcher |
| **MCHOSE** | G3 A (2.4 GHz dongle `A8A5:2255`, cable `A8A4:2255`) | Read-only queries from MCHOSE's M HUB web configurator; not verified on the cable variant |
| **LAMZU** | Maya X, Maya, Maya M, Tachi, Inca, Paro, Thorn, Atlantis Mini / OG Champion, Orcus (cable, 1K and 4K/8K dongles) | Vendor feature report from LAMZU Aurora |
| **Redragon** | Compx-based wireless mice: M916 Pro 1K (tested), M913 Impact Elite (tri-mode), G49 (receiver or cable) | Kysona protocol |
| **AULA** | F75 / F87 Pro / F99 wireless keyboards on the 2.4 GHz receiver | Report 0x13 frame, command 0x4A, sum checksum (Bluetooth and wired modes untested) |
| **ASUS ROG** | Strix Scope II 96 (RX), Azoth, Falchion keyboards; Keris, Harpe, Gladius, Chakram, Spatha, Pugio, Strix Impact mice; the Omni Receiver | ROG HID battery query |
| **ZOWIE** | EC1/EC2/EC3-CW and DW series (U2-DW, S2-DW ...) | Community-documented read-only vendor commands |
| **Rapoo VT** | VT3 Max Gen 2 (tested); other Nordic 54L VT mice exposing the same status interface; legacy VT3 Pro (experimental) | Passive HID status report `07` on `FF00:0002`; legacy `BB` restricted to VT3 Pro `1215/4415`. Receiver and USB cable, with charging state |
| **Motospeed / Delux / Ajazz** | Motospeed X6, Delux M900 Pro, Ajazz AJ179 family; AJ159 and other AJ-series mice on the 3151 receiver | Vendor reports (Delux: passive status broadcasts; AJ-series: 0xF7 status poll, then feature report 0x05) |
| **Attack Shark and other Chinese brands** | Rongyuan receivers (81 models: Attack Shark R3, X82 PRO HE, Ajazz keyboards, some MSI and others); Xenta receivers (Attack Shark X6 / R1, X11, X3 Max, V3 Pro) | Rongyuan status report; Xenta passive broadcasts |
| **Sony** | DualShock 4, DualSense, DualSense Edge (USB, Bluetooth, wireless adapter); INZONE H9 II and H5 headsets | Input report; INZONE vendor protocol |
| **GameSir** | Nova Lite, Nova 2, Nova 2 Lite on the 2.4 GHz receiver; Nova 2 Lite on USB | Vendor device-info query (usage page FF7A) |
| **Xbox** | Controllers on the Xbox Wireless adapter, USB or Bluetooth; headsets plugged into a controller | Windows.Gaming.Input battery report (real %), XInput fallback |
| **Bluetooth** | Anything that shows a battery in *Settings → Bluetooth & devices* (earbuds, headphones, BLE mice and keyboards) | Windows device property |
| **Generic HID** | Devices reporting the standard HID *Battery Strength* usage | HID usage page 0x06 / 0x85 |
| **HeadsetControl** | [Every headset it supports](https://github.com/Sapd/HeadsetControl#supported-headsets) (Audeze, Jabra, Sony INZONE, newer Logitech and more) | Bundled; runs `headsetcontrol -o json` |

**Differences by platform.** These entries read their data differently on each system:

- **Bluetooth:** Windows reads the device property the OS keeps; macOS reads `system_profiler SPBluetoothDataType` and `ioreg` (`BatteryPercent`).
- **Xbox:** Windows uses `Windows.Gaming.Input` (real percentage) with an XInput fallback. On macOS the app reads `BatteryPercent` from `ioreg` for Xbox (VID 045E) controllers; if macOS does not expose it for your controller, nothing is shown.
- **ATK N9 Pro:** needs its USB virtual COM port. The ATK desktop app must be closed.
- **Windows-only reading:** Windows Bluetooth and Xbox code paths have only been compiled in CI, not run on real devices yet.

## Build

Install Rust with [rustup](https://rustup.rs/). Rust needs its usual linker: the Visual Studio Build Tools on Windows, the Xcode command line tools (`xcode-select --install`) on macOS. No other C library or compiler step is involved (the Windows HID backend is pure Rust).

```
cargo build --release     # target/release/wireless-battery(.exe)
cargo test                # unit tests, no hardware needed
```

To make the macOS app bundle and zip (ad-hoc signed):

```
scripts/package-macos.sh 1.0.0 target/release/wireless-battery dist
```

To build the Windows installer and portable zip, install [Inno Setup](https://jrsoftware.org/isinfo.php) 6 or 7, then:

```
$env:WB_VERSION = "1.0.0"   # version shown in the exe's properties
cargo build --release
powershell -File installer\build-installer.ps1 -Version 1.0.0
```

The script downloads the pinned HeadsetControl release and verifies its checksum before bundling it. Before packaging, CI also runs `python3 scripts/third-party-licenses.py --target <triple> -o installer/THIRD_PARTY_LICENSES.txt`, which collects the license texts of the Rust crates each platform links against (the installer, the portable zip and the macOS app include the result). `build.rs` embeds the icon, manifest (per-monitor DPI awareness) and version resource into the exe.

GitHub Actions ([`build.yml`](.github/workflows/build.yml)) runs the tests on Windows and macOS for every push and pull request. Pushing a tag like `v1.0.0` builds the Windows installer, the Windows portable zip, the universal macOS zip and the HeadsetControl source (as its GPL license requires), and publishes them as a release.

## Limitations

- **Windows taskbar widget:** only type-checked, never run on a real taskbar.
- **Windows installer, icon and manifest:** built by CI but not yet run on a real machine.
- **MCHOSE 2.4 GHz dongle:** the dongle never reports that the mouse was switched off (it answers with the last values it remembers), so a switched-off mouse keeps its last percentage. Bluetooth mode is detected normally.
- **Sleeping devices:** a device that is asleep may not answer. Its last value stays visible, dimmed. Logitech receivers drop sleeping devices from the link, so their last value is kept for 30 minutes.
- **Keychron keyboards:** they only report their battery over Bluetooth (shown via the Bluetooth support). Over the 2.4 GHz receiver the firmware does not expose it. Keychron mice are supported.
- **Rapoo:** supported VT mice push their status, so a refresh can wait up to 4 seconds per matching interface. Sleeping mice may stay dimmed with their last reading for up to 30 minutes. Support is for Nordic 54L report `07` and legacy VT3 Pro `BB`; CH584 report `08`, office MT/M-series mice and other firmware protocols are not covered by this driver.
- **Not supported yet:** MSI wireless mice and keyboards, Razer Kraken / Barracuda headsets, HyperX Pulsefire mice and the Xbox Wireless Headset on the Xbox adapter: their battery protocols are not documented. The Xbox Wireless Headset works over Bluetooth. A USB capture from an owner (see CONTRIBUTING.md) is the way to add them.
- **Other devices:** some 2.4 GHz devices use undocumented protocols and are not supported yet. See [CONTRIBUTING.md](CONTRIBUTING.md) for how to help add one.
- **macOS standard devices:** see the Input Monitoring note above. Also macOS does not report USB interface numbers, so drivers match by usage page and report length instead of interface number, which can mean a few extra harmless probes.

## How it works

Each vendor driver in [`src/drivers/`](src/drivers) recognises its devices by USB vendor and product ID, and only ever sends read-only battery queries. Unknown devices are never written to. The generic fallbacks (HID battery usage, Bluetooth, Xbox) run last and only look at what the vendor drivers did not claim.

HID access goes through [hidapi](https://crates.io/crates/hidapi). Because hidapi does not report report sizes, [`src/hid.rs`](src/hid.rs) reads each device's report descriptor and works out the input, output and feature report lengths that most drivers use to pick the right interface. Drivers are written against a small `HidIo` trait so they can be tested with scripted mock devices.

A worker thread polls every 60 seconds (or when you choose *Refresh now*) and the UI ([`src/ui.rs`](src/ui.rs), built on `tao` and `tray-icon`) updates the tray from the results. Device bookkeeping (last known level, grace period for sleeping devices, low-battery warnings) lives in [`src/state.rs`](src/state.rs).

## Credits

The protocols come from these projects' research:

- [HeadsetControl](https://github.com/Sapd/HeadsetControl): SteelSeries, Corsair, HyperX and Logitech headsets
- [OpenRazer](https://github.com/openrazer/openrazer): Razer
- [Solaar](https://github.com/pwr-Solaar/Solaar) and the Logitech HID++ 2.0 documents: Logitech
- [rivalcfg](https://github.com/flozz/rivalcfg): SteelSeries mice (Rival Pro from its pull request #294)
- [glorious-indicator](https://github.com/ikajdan/glorious-indicator) and [glorious-ctl](https://github.com/louis4craft/glorious-ctl): Glorious
- Hitscan's web configurator ([control.hitscan.com](https://control.hitscan.com)): Hitscan
- Linux `hid-kysona` (Lode Willems): Kysona, Pulsar and ATK
- ATK's web configurator ([hub.atk.pro](https://hub.atk.pro)): ATK mice and the N9 Pro headset
- G-Wolves' web driver ([mouse.xyz](https://mouse.xyz)): G-Wolves models and their protocols
- Keychron Launcher ([launcher.keychron.com](https://launcher.keychron.com)): Keychron mice
- MCHOSE M HUB web configurator ([mchose.store/pages/mchose-hub](https://mchose.store/pages/mchose-hub)): MCHOSE G3 A protocol
- LAMZU Aurora ([lamzu.net](https://www.lamzu.net)): LAMZU mice
- [redragon-battery-omarchy](https://github.com/EliottV17/redragon-battery-omarchy) and [g49-web-control](https://github.com/pingpingjinping/g49-web-control): Redragon ids
- [peripheral-battery-tray](https://github.com/SteveWang92/peripheral-battery-tray) / [G-Helper](https://github.com/seerge/g-helper): ASUS ROG
- [zowie-battery](https://github.com/yleshchuk/zowie-battery) and [zowie-battery-tray](https://github.com/IgnacioBarb/zowie-battery-tray): ZOWIE EC-CW and DW
- [LGSTrayBattery-MotospeedX6](https://github.com/Jacobz888/LGSTrayBattery-MotospeedX6), [omarchy-delux-m900](https://github.com/AeonArcHQ/omarchy-delux-m900), [aj179-linux](https://github.com/Rockeyxx/aj179-linux): Motospeed, Delux, Ajazz
- OpenRazer PR #2794 and [omarchy-blackshark-v3](https://github.com/ReedME/omarchy-blackshark-v3): BlackShark V3
- [HyperXWM](https://github.com/Medprj/HyperXWM): HyperX Cloud III Wireless
- [HyperHeadset](https://github.com/LennardKittner/HyperHeadset): HyperX Cloud III S, Cloud II Core, Cloud Flight S
- [BatteryTray](https://github.com/qu1ncky/BatteryTray): Rongyuan and Xenta receivers (Attack Shark and others)
- [coolermaster-usb-protocol](https://github.com/syntactic-spice/coolermaster-usb-protocol) and [MM731-Controller](https://github.com/riVFerd/MM731-Controller): Cooler Master MM712 and MM731
- [GamesirBatteryTray](https://github.com/WWWyyy452/GamesirBatteryTray) and [Gamesir-Nova-2-Lite-Battery-Detection](https://github.com/AJAY0406/Gamesir-Nova-2-Lite-Battery-Detection): GameSir Nova
- Linux `hid-playstation`: DualShock 4 / DualSense report layout

## License

[MIT](LICENSE). The bundled HeadsetControl executable is a separate program under the GPL v3; its
license text ships with the installer as `GPL-3.0.txt`.

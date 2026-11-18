# Contributing

## Reporting whether a device works

Most drivers were written from protocol documentation and could not be tested on every model, and this Rust port has not yet been verified against real hardware at all. Please open an issue with:

- your operating system (Windows or macOS, and the version)
- the device name, and whether it is connected by receiver, cable or Bluetooth
- the output of `wireless-battery --once`
- the output of `wireless-battery --dump`
- what the vendor's own software shows as the battery level

On macOS, say whether you granted the app *Input Monitoring*; without it standard mouse and keyboard collections cannot be read.

## Adding a device

1. Run `wireless-battery --dump` and find your device's vendor and product IDs, and the vendor-defined collections (usage page `FFxx`) with their report sizes (the `In`, `Out` and `Feat` columns).
2. Find the battery protocol. Good places to look: HeadsetControl, OpenRazer, Solaar, rivalcfg, OpenRGB, OpenLinkHub, or a USB capture of the vendor software (Wireshark + USBPcap).
3. If the vendor already has a driver in `src/drivers/`, add the product ID (and any differences) there. Otherwise create `src/drivers/<vendor>.rs`, register it with `pub mod <vendor>;` in `src/drivers/mod.rs` and call its `poll` from `poll_all` (order matters: vendor drivers run before the generic ones). A driver looks like this:

   ```rust
   use super::PollContext;
   use crate::device::{DeviceKind, DeviceStatus};
   use crate::hid::{clamp100, hex4, HidDev, FeatureIo};

   pub fn poll(ctx: &mut PollContext) {
       for (vid, pid) in ctx.products(0x1234) {                 // unclaimed products of this vendor
           for c in ctx.find(vid, pid, |c| c.usage_page == 0xFF00 && c.out_len == 65) {
               let Some(mut d) = HidDev::open(ctx.api, &c) else { continue };
               let mut req = [0u8; 65];                              // [0] = report id
               req[3] = 0x01;                                        // ... the vendor's request bytes
               if !d.set_feature(&req) { continue; }
               let Some(reply) = d.get_feature(0, 65) else { continue };
               if reply.len() < 9 { continue; }                      // not this protocol
               let mut s = DeviceStatus { key: format!("acme:{}", hex4(pid)), name: "Acme Mouse".into(), kind: DeviceKind::Mouse, ..Default::default() };
               s.percent = Some(clamp100(reply[8] as i32));
               ctx.add(s, vid, pid);                             // reports it and claims the product
               break;
           }
       }
   }
   ```

4. Keep the request building and reply decoding in plain functions and test them: write the driver's I/O against the `HidIo` / `FeatureIo` traits in [`src/hid.rs`](src/hid.rs) and drive it with a scripted mock, as [`src/drivers/glorious.rs`](src/drivers/glorious.rs) does.
5. Only send read-only battery queries, and only to devices you recognise by vendor and product ID. Never write to unknown devices.
6. macOS does not report USB interface numbers (`iface` is always -1), so filter by usage page and report lengths rather than relying on `iface`.

Run `cargo test` before opening a pull request.

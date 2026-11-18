//! Each driver looks at the enumerated HID collections, talks to the devices it recognises and
//! appends what it finds. A driver claims a device (vendor/product id) once it has handled it so
//! the generic fallbacks further down the chain do not report it twice.

use std::collections::BTreeSet;

use hidapi::HidApi;

use crate::device::DeviceStatus;
use crate::hid::HidColl;

pub mod glorious;
pub mod atk_serial;
pub mod aula;
pub mod bluetooth;
pub mod kysona;
pub mod keychron;
pub mod lamzu;
pub mod mchose;
pub mod rongyuan;
pub mod sony_inzone;
pub mod rog;
pub mod xinput;
pub mod zowie;
pub mod rapoo;
pub mod misc_mice;
pub mod razer_headset;
pub mod hyperx;
pub mod logitech;
pub mod razer;
pub mod steelseries;
pub mod corsair;
pub mod coolermaster;
pub mod playstation;
pub mod gamesir;
pub mod headsetcontrol;
pub mod generic;

pub struct PollContext<'a> {
    pub api: &'a HidApi,
    pub colls: Vec<HidColl>,
    claimed: BTreeSet<(u16, u16)>,
    pub out: Vec<DeviceStatus>,
}

impl<'a> PollContext<'a> {
    pub fn new(api: &'a HidApi, colls: Vec<HidColl>) -> Self {
        Self { api, colls, claimed: BTreeSet::new(), out: Vec::new() }
    }

    pub fn is_claimed(&self, vid: u16, pid: u16) -> bool {
        self.claimed.contains(&(vid, pid))
    }

    pub fn claim(&mut self, vid: u16, pid: u16) {
        self.claimed.insert((vid, pid));
    }

    pub fn add(&mut self, s: DeviceStatus, vid: u16, pid: u16) {
        self.out.push(s);
        self.claim(vid, pid);
    }

    /// Distinct (vid, pid) pairs for a vendor that are not claimed yet.
    pub fn products(&self, vid: u16) -> Vec<(u16, u16)> {
        let set: BTreeSet<_> = self
            .colls
            .iter()
            .filter(|c| c.vid == vid && !self.is_claimed(c.vid, c.pid))
            .map(|c| (c.vid, c.pid))
            .collect();
        set.into_iter().collect()
    }

    /// Collections of one product, optionally filtered.
    pub fn find(&self, vid: u16, pid: u16, pred: impl Fn(&HidColl) -> bool) -> Vec<HidColl> {
        self.colls.iter().filter(|c| c.vid == vid && c.pid == pid && pred(c)).cloned().collect()
    }
}

/// Runs every driver in order. Vendor drivers go first and claim the devices they handle; the
/// generic fallbacks then only look at what is left.
pub fn poll_all(api: &HidApi) -> Vec<DeviceStatus> {
    let colls = crate::hid::enum_hid(api);
    let mut ctx = PollContext::new(api, colls);
    glorious::poll(&mut ctx);
    aula::poll(&mut ctx); // before Kysona, which shares the 3554 vendor id and would probe it first
    atk_serial::poll(&mut ctx); // before Kysona, which shares the ATK vendor id
    kysona::poll(&mut ctx);
    keychron::poll(&mut ctx);
    lamzu::poll(&mut ctx);
    mchose::poll(&mut ctx);
    rongyuan::poll(&mut ctx);
    sony_inzone::poll(&mut ctx);
    rog::poll(&mut ctx);
    zowie::poll(&mut ctx);
    rapoo::poll(&mut ctx);
    misc_mice::poll(&mut ctx);
    razer_headset::poll(&mut ctx); // before the generic Razer driver, which shares the vendor id
    hyperx::poll(&mut ctx);
    logitech::poll(&mut ctx);
    razer::poll(&mut ctx);
    steelseries::poll(&mut ctx);
    corsair::poll(&mut ctx);
    coolermaster::poll(&mut ctx);
    playstation::poll(&mut ctx);
    gamesir::poll(&mut ctx);
    headsetcontrol::poll(&mut ctx);
    generic::poll(&mut ctx); // generic HID battery usage, last of the HID drivers
    bluetooth::poll(&mut ctx);
    xinput::poll(&mut ctx); // Xbox controllers, last
    ctx.out
}

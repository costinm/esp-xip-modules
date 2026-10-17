//! Wi-Fi for modules: what the radio should do, what it is doing, and the advanced driver settings.
//!
//! A module never starts or stops Wi-Fi. It states the **intended state** with the `wifi` component and the host (the
//! firmware's Wi-Fi owner) makes it true: it joins, retries, and follows a channel change. Look the component up once with
//! `get_component("wifi")` (Rust: [`Hw::wifi`](crate::Hw::wifi)); it is absent on a host without a Wi-Fi owner.
//!
//! | Component | Table | Purpose |
//! |---|---|---|
//! | `"wifi"` | [`Wifi`] | `set` the intended state ([`WifiIntent`]), `get` it back, read what is true now ([`WifiState`]). |
//! | `"wifi-tuning"` | [`WifiTuner`] | Rarely needed driver settings ([`WifiTuning`]): aggregation, 11b rates, raw receive, buffers. |
//!
//! Both are plain C structs with no CBOR in them. Every function takes a context first: pass the `user` of the host table
//! the module was called with (`host->user`), which lets the host tell who is calling. They never block and may be called
//! from any module handler. The tables and the structs are valid for the firmware's lifetime.
//!
//! # The intended state
//!
//! The radio is **off** or **on**. With the radio on, ESP-NOW is always available; on top of it a module chooses the
//! station (join a network), the soft AP, both, or neither:
//!
//! | `radio` | `sta` | `ap` | Result |
//! |---:|---:|---:|---|
//! | 0 | - | - | Radio off. |
//! | 1 | 0 | 0 | ESP-NOW only. |
//! | 1 | 1 | 0 | Station + ESP-NOW. |
//! | 1 | 0 | 1 | Soft AP + ESP-NOW. |
//! | 1 | 1 | 1 | Soft AP + station + ESP-NOW. |
//!
//! [`Wifi::set`] replaces the whole intent: there is no merge and no special "off". A module that only wants another
//! channel reads with `get`, edits the field, and writes it back. Acceptance is not completion: the join happens afterwards
//! and `state` tells when it is up. Credentials are stored as given and are visible to every module (modules are not
//! isolated from each other).
//!
//! # Events
//!
//! Every running module gets a `HOST_WIFI` call (`HOST_COMPONENT`, method `HOST_WIFI`) when the intended state or the
//! tuning changed; `call.value` is the new generation. A module that cares calls `get` again, and one that waits for the
//! network reads `state`. `HOST_NETWORK` (value 1/0) separately says whether the *firmware* has a path to the network.
//!
//! # Example
//!
//! ```c
//! const xip_wifi *wifi = host->get_component(host->user, "wifi");
//! xip_wifi_intent want = { .size = sizeof want, .radio = 1, .sta = 1 };
//! memcpy(want.sta_ssid, "home", 4);   want.sta_ssid_len = 4;
//! memcpy(want.sta_psk, "hunter22", 8); want.sta_psk_len = 8;
//! uint32_t generation;
//! if (wifi->set(host->user, &want, &generation) != 0) { /* XIP_ERR_ARG: a length or the channel is invalid */ }
//! /* later, on HOST_WIFI or an alarm: */
//! xip_wifi_state now = { .size = sizeof now };
//! if (wifi->state(host->user, &now) == 0 && now.sta == 2) { /* joined: now.ip4, now.rssi */ }
//! ```
//!
//! # Status and scan for text and network front ends
//!
//! Two host calls return CBOR for the console and for remote tools. They are not for modules that need the values (read
//! `state`) and only exist on hosts with `FEAT_WIFI`:
//!
//! * `wifi_info(out, capacity)`: one map `{1: state, 2: ssid, 3: ip, 4: rssi, 5: ap_up, 6: channel}`. `state` is 0 off, 1
//!   joining, 2 up. `ssid` (text), `ip` (IPv4 as text) and `rssi` (signed dBm) are present once the station is up; `ap_up`
//!   (bool) when the soft AP is running; `channel` when the radio has one.
//! * `wifi_scan(out, capacity)`: never blocks, because a scan takes seconds and a handler has a budget of milliseconds. The
//!   first call starts it and returns `ERR_BUSY`; poll with an alarm (about once a second). The call that finds it done
//!   returns the answer, once: `{1: [networks]}`, strongest first, each a map `{1: ssid, 2: bssid, 3: rssi, 4: channel, 5: auth}`. `ssid`
//!   and `bssid` are byte strings (a name is not always text), `rssi` is signed dBm, `auth` is 0 open, 1 WEP, 2 WPA, 3 WPA2, 4
//!   WPA/WPA2, 5 enterprise, 6 WPA3 (anything else: the driver's number). The radio is switched on for the scan and off again
//!   if it was off; while a join is under way the call returns `ERR_BUSY`. A network in range that is not listed was missed;
//!   scan again. The `scan` of the `wifi` component table returns the same list to a module, in every radio mode.
//!
//! # Device settings
//!
//! The driver's boot buffer sizes are device settings (`nv`, text, applied at the next radio start): `wifi.rx_static`,
//! `wifi.rx_dynamic`, `wifi.tx_dynamic`, `wifi.rx_ba`. [`WifiTuning`] overrides them while the firmware runs.
//!
//! The same fields as text, JSON and CBOR for consoles, gateways and tools are described in `api/wifi.API.md`, the source of
//! the generated `api/generated/wifi.tools.json` and `wifi.manifest`.
//!
//! Design notes: `notes/ai/2026-10-16-wifi-intent-component.md`.
use core::ffi::{c_int, c_void};

/// Version of the [`Wifi`] table.
pub const WIFI_VERSION: u32 = 1;

/// What a module wants the radio to do, for `set` and `get`. All fields are plain values. Names and passphrases are byte
/// arrays with a separate length: they are not NUL-terminated and may hold any bytes except NUL. Unused bytes are zero.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiIntent {
    /// `sizeof(xip_wifi_intent)`, set by the caller; lets the struct grow.
    pub size: u32,
    /// 0: the radio is off, 1: on (ESP-NOW included).
    pub radio: u8,
    /// 1: join the station network (`sta_ssid`, or `bssid`) while the radio is on.
    pub sta: u8,
    /// 1: run the soft AP (`ap_ssid`) while the radio is on.
    pub ap: u8,
    /// 0: any, else 1..14. Applies to the AP and speeds up a join.
    pub channel: u8,
    /// Join this access point only; all zero: none. Together with `channel` it avoids a scan.
    pub bssid: [u8; 6],
    /// Length of `sta_ssid`: 1..32, or 0 when only `bssid` is given.
    pub sta_ssid_len: u8,
    /// Length of `sta_psk`: 8..63, or 0 for an open network.
    pub sta_psk_len: u8,
    /// Length of `ap_ssid`: 1..32, or 0 for the host's default name.
    pub ap_ssid_len: u8,
    /// Length of `ap_psk`: 8..63, or 0 for an open AP.
    pub ap_psk_len: u8,
    /// Zero.
    pub reserved: [u8; 2],
    /// Network name, 1..32 bytes, no NUL.
    pub sta_ssid: [u8; 32],
    /// Passphrase, 8..63 bytes, or empty for an open network.
    pub sta_psk: [u8; 64],
    /// Name of the soft AP; empty: the host's default.
    pub ap_ssid: [u8; 32],
    /// Passphrase of the soft AP, 8..63 bytes, or empty.
    pub ap_psk: [u8; 64],
}
impl Default for WifiIntent {
    fn default() -> Self {
        let mut i: WifiIntent = unsafe { core::mem::zeroed() };
        i.size = core::mem::size_of::<WifiIntent>() as u32;
        i
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<WifiIntent>() == 212);

impl WifiIntent {
    /// The rules of the component, and nothing else (the same as every host applies): `ERR_ARG` for a name or passphrase of the
    /// wrong length or with a NUL, a channel above 14, a flag above 1, a `size` too small, or `sta` with neither an SSID nor a BSSID.
    pub fn validate(&self) -> Result<(), i32> {
        let text = |s: &[u8], n: u8, max: usize, min: usize| {
            let n = n as usize;
            n <= max && n <= s.len() && (n == 0 || n >= min) && !s[..n].contains(&0)
        };
        if (self.size as usize) < core::mem::size_of::<WifiIntent>() || self.radio > 1 || self.sta > 1 || self.ap > 1 || self.channel > 14 {
            return Err(crate::ERR_ARG);
        }
        if !text(&self.sta_ssid, self.sta_ssid_len, 32, 1) || !text(&self.ap_ssid, self.ap_ssid_len, 32, 1)
            || !text(&self.sta_psk, self.sta_psk_len, 63, 8) || !text(&self.ap_psk, self.ap_psk_len, 63, 8) {
            return Err(crate::ERR_ARG);
        }
        if self.sta == 1 && self.sta_ssid_len == 0 && self.bssid == [0; 6] {
            return Err(crate::ERR_ARG);
        }
        Ok(())
    }
    /// The same intent with `size` and `reserved` normalized, so two equal intents compare equal bytewise.
    pub fn normalized(&self) -> WifiIntent {
        let mut i = *self;
        i.size = core::mem::size_of::<WifiIntent>() as u32;
        i.reserved = [0; 2];
        i
    }
}
impl PartialEq for WifiIntent {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.normalized(), other.normalized());
        // plain bytes: the struct has no padding on any target (the layout is asserted above)
        unsafe {
            core::slice::from_raw_parts((&a as *const Self).cast::<u8>(), core::mem::size_of::<Self>())
                == core::slice::from_raw_parts((&b as *const Self).cast::<u8>(), core::mem::size_of::<Self>())
        }
    }
}

/// What is true now, for `state`. Zero is "off / none" in every field.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WifiState {
    /// `sizeof(xip_wifi_state)`, set by the caller.
    pub size: u32,
    /// Station: 0 off, 1 joining, 2 up.
    pub sta: u8,
    /// Soft AP: 0 off, 1 up.
    pub ap: u8,
    /// The channel in use, 0 when the radio is off.
    pub channel: u8,
    /// Signal of the joined network in dBm, valid when `sta` is 2.
    pub rssi: i8,
    /// The joined access point, valid when `sta` is 2.
    pub bssid: [u8; 6],
    pub ssid_len: u8,
    /// Zero.
    pub reserved: u8,
    /// The joined network.
    pub ssid: [u8; 32],
    /// IPv4 address of the station interface, zero when none.
    pub ip4: [u8; 4],
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<WifiState>() == 52);

/// Promiscuous (raw) receive, for [`Wifi::monitor`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiMonitor {
    /// `sizeof(xip_wifi_monitor)`, set by the caller; lets the struct grow.
    pub size: u32,
    /// 1: promiscuous receive on; 0: off.
    pub enable: u8,
    /// 0: leave the channel; 1..13: pin the unassociated radio to this channel (HT20) first.
    pub channel: u8,
    /// Zero.
    pub reserved: [u8; 2],
}
impl Default for WifiMonitor {
    fn default() -> Self {
        WifiMonitor { size: core::mem::size_of::<WifiMonitor>() as u32, enable: 0, channel: 0, reserved: [0; 2] }
    }
}
const _: () = assert!(core::mem::size_of::<WifiMonitor>() == 8);

/// One network of a scan, as [`Wifi::scan`] returns it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiAp {
    /// The network name (not always text), `ssid_len` bytes of it.
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub bssid: [u8; 6],
    /// Signal strength, dBm.
    pub rssi: i8,
    /// Primary channel.
    pub channel: u8,
    /// 0 open, 1 WEP, 2 WPA, 3 WPA2, 4 WPA/WPA2, 5 enterprise, 6 WPA3 (anything else: the driver's number).
    pub auth: u8,
}
const _: () = assert!(core::mem::size_of::<WifiAp>() == 42);

/// The `wifi` component table, from `get_component("wifi")`. Valid for the firmware lifetime. The functions take no
/// context argument and may be called from any module handler; they never block.
#[repr(C)]
pub struct Wifi {
    /// `WIFI_VERSION`.
    pub version: u32,
    /// `sizeof(xip_wifi)`: members past it do not exist.
    pub size: u32,
    /// Replace the intended state with `intent` (set `intent.size`); `*generation` receives the new generation. Returns 0, or
    /// `ERR_ARG` for an invalid intent: a name or passphrase of the wrong length, a channel above 14, or `sta` with neither
    /// an SSID nor a BSSID. An identical intent keeps the generation and wakes nobody. Acceptance is not completion: the
    /// host joins afterwards, so read `state`. With the radio on, ESP-NOW is on: with `sta` and `ap` both 0 the radio is
    /// unassociated on `channel` (6 when 0) and only ESP-NOW runs (the `espnow` link of the `link` component).
    pub set: Option<unsafe extern "C" fn(ctx: *mut c_void, intent: *const WifiIntent, generation: *mut u32) -> c_int>,
    /// Read the intended state and its generation (set `intent.size` first). `ERR_SIZE` if `size` is too small.
    pub get: Option<unsafe extern "C" fn(ctx: *mut c_void, intent: *mut WifiIntent, generation: *mut u32) -> c_int>,
    /// Read what is true now (set `state.size` first). Everything zero while the radio is off. `ERR_UNSUPPORTED` when the
    /// application drives the radio itself and does not report it.
    pub state: Option<unsafe extern "C" fn(ctx: *mut c_void, state: *mut WifiState) -> c_int>,
    /// Scan for networks, for a module that reports the surroundings (a control plane decides the topology from it). **Never
    /// blocks**, because a scan takes seconds: the first call starts it and returns `ERR_BUSY`; call again (an alarm of about a
    /// second) until it returns the count, at most `max` entries of `out` filled (16 at most), once per scan. It works in
    /// **every radio mode**: from the station's idle epoch, from ESP-NOW only, with the soft AP on and while joined; with the radio
    /// off it is switched on for the scan only. A scan leaves the channel for about two seconds, so the `espnow` link and a soft AP
    /// do not hear their channel meanwhile; the host puts the radio back on its channel when the scan is done. `ERR_BUSY` as
    /// well while the driver is busy joining (try again), `ERR_IO` when it refuses. A network in range that is not listed was
    /// missed; scan again.
    pub scan: Option<unsafe extern "C" fn(ctx: *mut c_void, out: *mut WifiAp, max: usize) -> c_int>,
    /// Switch **promiscuous receive** on or off, which a NAN module needs: NAN discovery-window frames are not delivered any
    /// other way (see `components/xip_runtime/ESPNOW.md`). The radio keeps receiving as before; the frames the driver then hands
    /// up also arrive on the link `promisc` of the `link` component, one packet each: `call.data` the 802.11 frame from its MAC
    /// header (the radio's FCS removed), `call.id` the transmitter address ([`mac_token`](crate::mac_token)), `call.value` the
    /// RSSI ([`signal`](crate::signal)), `call.time_us` the receive time. The runtime applies an early receive filter (frames not for this device, or not of our networks or NAN, are dropped before they are copied) and makes **no
    /// power-save change**: the module decides when to switch it off again (a window of tens of milliseconds
    /// in a discovery window), what to do about power save, and whether to stay awake longer. `channel` pins the unassociated radio
    /// first. The state of the link is `LINK_UP` while receive is on.
    ///
    /// Needs the radio on (`ERR_UNSUPPORTED` otherwise); a radio restart clears it, so a module that wants it re-applies it when
    /// `state` shows the radio again. `ERR_ARG` for a size that is too small or a channel above 13; `ERR_IO` when the driver
    /// refuses.
    pub monitor: Option<unsafe extern "C" fn(ctx: *mut c_void, monitor: *const WifiMonitor) -> c_int>,
}

/// A switch of [`WifiTuning`]: the host's own default.
pub const TUNE_DEFAULT: u8 = 0;
/// A switch of [`WifiTuning`]: enabled.
pub const TUNE_ON: u8 = 1;
/// A switch of [`WifiTuning`]: disabled.
pub const TUNE_OFF: u8 = 2;

/// Version of the [`WifiTuner`] table.
pub const WIFI_TUNING_VERSION: u32 = 1;

/// Advanced driver settings, for measurements and unusual links; the defaults of a host are the supported baseline.
/// Switches are `TUNE_DEFAULT`, `TUNE_ON` or `TUNE_OFF`; numbers are `0` for the default. `ampdu` and the four
/// buffer sizes are read when the driver starts, so a change restarts the radio (the station rejoins); the other
/// fields apply to the next association.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WifiTuning {
    /// `sizeof(xip_wifi_tuning)`, set by the caller; lets the struct grow.
    pub size: u32,
    /// A-MPDU aggregation in both directions. Default: on.
    pub ampdu: u8,
    /// 802.11b rates; applied before the radio starts. Default: off (DMesh's tested baseline: the legacy basic-rate set is not negotiated).
    pub rates_11b: u8,
    /// The firmware takes the received frames of the station before the network stack (raw RX). Rust host only.
    pub raw_rx: u8,
    /// Station egress through the driver's normal data path (`TUNE_OFF`: raw 802.11 injection). Rust host only.
    pub driver_tx: u8,
    /// Standard BSSID filtering of the station's receive lane. Default: off, so frames of other BSSIDs reach unassociated ESP-NOW receive.
    pub bssid_check: u8,
    /// Raw transmit rate (driver rate code); 0: the driver default. Rust host only.
    pub raw_tx_rate: u8,
    /// Zero.
    pub reserved: [u8; 2],
    /// Static receive buffers, 2..64 (the `wl` settings give the boot default).
    pub rx_static: u16,
    /// Dynamic receive buffers, 2..128.
    pub rx_dynamic: u16,
    /// Dynamic transmit buffers, 4..128.
    pub tx_dynamic: u16,
    /// Receive block-ack window, 2..64; the driver limits it by the receive buffers.
    pub rx_ba: u16,
}
impl Default for WifiTuning {
    fn default() -> Self {
        let mut t: WifiTuning = unsafe { core::mem::zeroed() };
        t.size = core::mem::size_of::<WifiTuning>() as u32;
        t
    }
}
const _: () = assert!(core::mem::size_of::<WifiTuning>() == 20);

impl WifiTuning {
    /// `ERR_ARG` for a switch above `TUNE_OFF`, a buffer size out of range, or a `size` too small.
    pub fn validate(&self) -> Result<(), i32> {
        let ok = |v: u16, lo: u16, hi: u16| v == 0 || (lo..=hi).contains(&v);
        if (self.size as usize) < core::mem::size_of::<WifiTuning>()
            || [self.ampdu, self.rates_11b, self.raw_rx, self.driver_tx, self.bssid_check].iter().any(|v| *v > TUNE_OFF)
            || !(ok(self.rx_static, 2, 64) && ok(self.rx_dynamic, 2, 128) && ok(self.tx_dynamic, 4, 128) && ok(self.rx_ba, 2, 64)) {
            return Err(crate::ERR_ARG);
        }
        Ok(())
    }
    /// The same settings with `size` and `reserved` normalized.
    pub fn normalized(&self) -> WifiTuning {
        let mut t = *self;
        t.size = core::mem::size_of::<WifiTuning>() as u32;
        t.reserved = [0; 2];
        t
    }
    /// True when a setting only a host with the raw UDP6 path has (`raw_rx`, `driver_tx`, `raw_tx_rate`) is not left at the default.
    pub fn wants_raw_path(&self) -> bool {
        self.raw_rx != TUNE_DEFAULT || self.driver_tx != TUNE_DEFAULT || self.raw_tx_rate != 0
    }
}
impl PartialEq for WifiTuning {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.normalized(), other.normalized());
        a.ampdu == b.ampdu && a.rates_11b == b.rates_11b && a.raw_rx == b.raw_rx && a.driver_tx == b.driver_tx
            && a.bssid_check == b.bssid_check && a.raw_tx_rate == b.raw_tx_rate && a.rx_static == b.rx_static
            && a.rx_dynamic == b.rx_dynamic && a.tx_dynamic == b.tx_dynamic && a.rx_ba == b.rx_ba
    }
}

/// The `wifi-tuning` component table, from `get_component("wifi-tuning")`; absent on a host without a Wi-Fi owner.
/// Same rules as [`Wifi`]: `ctx` first, never blocks, valid for the firmware lifetime.
#[repr(C)]
pub struct WifiTuner {
    /// `WIFI_TUNING_VERSION`.
    pub version: u32,
    /// `sizeof(xip_wifi_tuner)`: members past it do not exist.
    pub size: u32,
    /// Replace the settings. `ERR_ARG` for a value out of range, `ERR_UNSUPPORTED` for a setting this host does not
    /// have and that is not left at the default. An identical value changes nothing.
    pub set: Option<unsafe extern "C" fn(ctx: *mut c_void, tuning: *const WifiTuning) -> c_int>,
    /// Read the settings in force: switches the host has report `TUNE_ON` or `TUNE_OFF`, numbers left at the default
    /// report 0, settings the host does not have report 0. Set `tuning.size` first.
    pub get: Option<unsafe extern "C" fn(ctx: *mut c_void, tuning: *mut WifiTuning) -> c_int>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_intent_is_valid_by_the_rules_of_the_component() {
        let mut i = WifiIntent::default();
        assert_eq!(i.validate(), Ok(()));
        i.radio = 1; i.sta = 1;
        assert_eq!(i.validate(), Err(crate::ERR_ARG), "sta needs a name or a BSSID");
        i.bssid = [1, 2, 3, 4, 5, 6];
        assert_eq!(i.validate(), Ok(()));
        i.sta_ssid_len = 33;
        assert_eq!(i.validate(), Err(crate::ERR_ARG));
        i.sta_ssid_len = 4; i.sta_psk_len = 3;
        assert_eq!(i.validate(), Err(crate::ERR_ARG), "a passphrase is 8..63 bytes or empty");
        i.sta_psk_len = 0; i.channel = 15;
        assert_eq!(i.validate(), Err(crate::ERR_ARG));
        i.channel = 14; i.size = 8;
        assert_eq!(i.validate(), Err(crate::ERR_ARG), "a size too small");
    }

    #[test]
    fn two_intents_with_the_same_content_are_equal() {
        let mut a = WifiIntent::default();
        let mut b = WifiIntent::default();
        a.reserved = [1, 2];
        assert!(a == b, "reserved is not content");
        b.sta_ssid[0] = b'x'; b.sta_ssid_len = 1;
        assert!(a != b);
    }

    #[test]
    fn tuning_ranges_and_the_raw_path() {
        let mut t = WifiTuning::default();
        assert_eq!(t.validate(), Ok(()));
        assert!(!t.wants_raw_path());
        t.bssid_check = TUNE_ON;
        assert!(!t.wants_raw_path(), "the BSSID check belongs to ESP-NOW receive");
        t.raw_rx = TUNE_OFF;
        assert!(t.wants_raw_path());
        t.rx_static = 1;
        assert_eq!(t.validate(), Err(crate::ERR_ARG));
        t.rx_static = 64; t.ampdu = 3;
        assert_eq!(t.validate(), Err(crate::ERR_ARG));
    }
}

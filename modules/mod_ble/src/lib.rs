//! The BLE module of the C runtime. It hosts the `ble` component (104, `API.md`; manifest key 11) and owns the policy of the DMesh companion link: the
//! service UUIDs, the advertisement and its intervals, the device name, the PSM, the scan filters and the CoC pool size. The mechanics are the
//! runtime's `nimble` component (components/xip_ble), driven with exactly what this module passes. The component is served as CBOR requests
//! and, to other modules, as a table (`get_component("ble")`, thunks of the runtime into this module); both come from `api_gen.rs`. Requests are CBOR maps keyed by the field tags of `API.md`: direct requests to component 104, or requests the runtime
//! took from a numeric envelope `{1: 104, 2: method, 3: id, 5: fields}` in a 0x444d0002 datagram (`call.is_envelope()`; the runtime parses it
//! and wraps the answer as `{1, 2, 3, 6: result}` or, for `Hw::reply_error`, `{.., 7: error}`: this module never sees the envelope).
//! Boot policy: the persistent setting `ble.boot` (1: start the service after the network came up, once). Pairing, bonding and the
//! rest of the link are the component's.
#![no_std]

use core::ffi::c_void;
use xip_abi::cbor::Writer;
use xip_abi::*;

pub use api_gen::COMPONENT;
// internal: the alarm that starts the service at boot
const M_BOOT: i32 = 1000;
const BOOT_DELAY_MS: u32 = 5000;
const BOOT_KEY: &[u8] = b"ble.boot";
const RESP_MAX: usize = 512;

/// The state block: this module's `ble` table and the context its functions take (built when the runtime asks for it: `api_gen::Hosted`).
pub const STATE_BYTES: usize = 128;
#[derive(Default)]
pub struct State {
    hosted: api_gen::Hosted,
}

mod api_gen;
use api_gen::Api;

// ---- the policy of the DMesh companion link: the `nimble` component does the mechanics with exactly what is given here ----
/// The 16-bit service UUID, and the 128-bit one of a device in its pairing window and of a paired one.
const SERVICE_UUID16: u16 = 0x1820;
const PAIRING_UUID128: [u8; 16] = [0x80, 0x6f, 0x6b, 0x5f, 0x2a, 0x4f, 0x4a, 0x6f, 0x8c, 0x42, 0x4d, 0x65, 0x73, 0x68, 0x00, 0x01];
const OPERATIONAL_UUID128: [u8; 16] = [0x80, 0x6f, 0x6b, 0x5f, 0x2a, 0x4f, 0x4a, 0x6f, 0x8c, 0x42, 0x4d, 0x65, 0x73, 0x68, 0x00, 0x02];
const DEVICE_NAME: &[u8] = b"DMesh";
/// Advertising interval, units of 0.625 ms (20 to 40 ms).
const ADV_MIN: u16 = 0x20;
const ADV_MAX: u16 = 0x40;
/// The dynamic LE PSM of the CoC channel when a request names none; the PSMs a client may name.
const COC_PSM: u16 = 0x0080;
const COC_PSM_RANGE: core::ops::RangeInclusive<u16> = 0x0080..=0x00ff;
/// One SDU is one QUIC packet (1500 is `PKT_MAX`); the pool needs the receive buffer, the SDU being copied and the transmit mbuf.
const COC_MTU: u16 = 1500;
const COC_BUFFERS: u8 = 3;
const SCAN_DEFAULT_MS: u32 = 10_000;
const SCAN_MS: core::ops::RangeInclusive<u32> = 1_000..=30_000;

fn nimble(hw: &Hw) -> Result<&'static Nimble, i32> {
    hw.nimble().ok_or(ERR_UNSUPPORTED)
}

fn done(rc: i32) -> Result<(), i32> {
    if rc < 0 { Err(rc) } else { Ok(()) }
}

macro_rules! call {
    ($hw:expr, $f:ident $(, $arg:expr)*) => {{
        let n = nimble($hw)?;
        done(unsafe { n.$f.ok_or(ERR_UNSUPPORTED)?($hw.ctx() $(, $arg)*) })
    }};
}

/// What `start` and `listen` ask the component for: the name, the CoC pool and server, and, for `start`, the advertisement
/// (flags, the 16-bit service UUID and the pairing or operational 128-bit one; the name in the scan response).
fn start_request(advertise: Option<bool>, psm: u16) -> NimbleStart {
    let mut r = NimbleStart::default();
    r.psm = psm;
    r.coc_mtu = COC_MTU;
    r.coc_buffers = COC_BUFFERS;
    r.name[..DEVICE_NAME.len()].copy_from_slice(DEVICE_NAME);
    r.name_len = DEVICE_NAME.len() as u8;
    if let Some(paired) = advertise {
        let uuid = if paired { &OPERATIONAL_UUID128 } else { &PAIRING_UUID128 };
        let head = [0x02, 0x01, 0x06, 0x03, 0x02, SERVICE_UUID16 as u8, (SERVICE_UUID16 >> 8) as u8, 0x11, 0x07];
        r.adv[..head.len()].copy_from_slice(&head);
        r.adv[head.len()..head.len() + 16].copy_from_slice(uuid);
        r.adv_len = (head.len() + 16) as u8;
        r.scan_rsp[0] = DEVICE_NAME.len() as u8 + 1;
        r.scan_rsp[1] = 0x09; // complete local name
        r.scan_rsp[2..2 + DEVICE_NAME.len()].copy_from_slice(DEVICE_NAME);
        r.scan_rsp_len = DEVICE_NAME.len() as u8 + 2;
        r.adv_min = ADV_MIN;
        r.adv_max = ADV_MAX;
    }
    r
}

/// An advertisement is a DMesh device's when it lists the 16-bit service UUID or either 128-bit one.
fn scan_filters() -> [NimbleFilter; 3] {
    let mut f = [NimbleFilter { ad_types: [0; 2], len: 0, reserved: 0, data: [0; 16] }; 3];
    f[0] = NimbleFilter { ad_types: [0x02, 0x03], len: 2, reserved: 0, data: [0; 16] };
    f[0].data[..2].copy_from_slice(&SERVICE_UUID16.to_le_bytes());
    f[1] = NimbleFilter { ad_types: [0x06, 0x07], len: 16, reserved: 0, data: PAIRING_UUID128 };
    f[2] = NimbleFilter { ad_types: [0x06, 0x07], len: 16, reserved: 0, data: OPERATIONAL_UUID128 };
    f
}

/// The methods over the runtime's `nimble` component (components/xip_ble); the field tags, the methods and the answers are `api_gen.rs`'s, from `API.md`.
impl Api for State {
    fn start(&mut self, hw: &Hw, request: &BleStart) -> Result<(), i32> {
        let r = start_request(Some(request.paired != 0), COC_PSM);
        call!(hw, start, &r)
    }
    fn stop(&mut self, hw: &Hw) -> Result<(), i32> { call!(hw, stop) }
    fn scan(&mut self, hw: &Hw, request: &BleScan) -> Result<(), i32> {
        let mut r = NimbleScan::default();
        r.duration_ms = if request.duration_ms == 0 { SCAN_DEFAULT_MS } else { request.duration_ms.clamp(*SCAN_MS.start(), *SCAN_MS.end()) };
        r.filters = scan_filters();
        call!(hw, scan, &r)
    }
    fn status(&mut self, hw: &Hw, response: &mut BleStatus) -> Result<(), i32> { call!(hw, status, response) }
    fn scan_stop(&mut self, hw: &Hw) -> Result<(), i32> { call!(hw, scan_stop) }
    fn connect(&mut self, hw: &Hw, request: &BleConnect) -> Result<(), i32> {
        let psm = if request.psm == 0 { COC_PSM } else { request.psm };
        if request.addr == [0; 6] || !COC_PSM_RANGE.contains(&psm) {
            return Err(ERR_ARG);
        }
        let mut r = NimbleConnect::default();
        r.addr = request.addr;
        r.addr_type = request.addr_type;
        r.psm = psm;
        r.coc_mtu = COC_MTU;
        call!(hw, connect, &r)
    }
    fn listen(&mut self, hw: &Hw, request: &BleListen) -> Result<(), i32> {
        let psm = if request.psm == 0 { COC_PSM } else { request.psm };
        if !COC_PSM_RANGE.contains(&psm) {
            return Err(ERR_ARG);
        }
        let r = start_request(None, psm);
        call!(hw, start, &r)
    }
    fn bonds_clear(&mut self, hw: &Hw) -> Result<(), i32> { call!(hw, bonds_clear) }
    fn mode(&mut self, hw: &Hw, request: &BleMode) -> Result<(), i32> {
        if request.mode > BLE_MODE_RENDEZVOUS || request.boot > BLE_BOOT_ON {
            return Err(ERR_ARG);
        }
        call!(hw, mode, request)?;
        if request.boot != BLE_BOOT_KEEP {
            // the persistent setting: read at the next boot by the application and by this module
            let rc = hw.nv(BOOT_KEY, Some(if request.boot == BLE_BOOT_ON { b"1" } else { b"0" }), &mut []);
            if rc < 0 {
                return Err(rc);
            }
        }
        Ok(())
    }
    fn scan_results(&mut self, hw: &Hw, response: &mut BleScanResults) -> Result<(), i32> { call!(hw, scan_results, response) }
    fn suspend(&mut self, hw: &Hw) -> Result<(), i32> { call!(hw, suspend) }
    fn resume(&mut self, hw: &Hw) -> Result<(), i32> { call!(hw, resume) }
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, c: &CallView) -> i32 {
        const CONTINUE: i32 = RET_CONTINUE as i32;
        if c.is_host(HOST_TABLE) {
            // the runtime asks for the `ble` table (manifest key 11) once this module started
            if c.data == b"ble" {
                api_gen::provide::<State>(hw, c, core::ptr::addr_of_mut!(self.hosted), self as *mut State);
            }
            return CONTINUE;
        }
        if c.flags & CALL_LINK != 0 {
            return CONTINUE; // a raw frame: this module has no port, and only gets requests
        }
        if c.component == HOST_COMPONENT as i32 {
            if c.method == HOST_START as i32 {
                // the persistent boot setting: start the service once the radio had time to come up (the classic coexistence allocator
                // must not race the first WPA authentication)
                let mut v = [0u8; 4];
                let on = hw.nv(BOOT_KEY, None, &mut v) > 0 && v[0] == b'1';
                if let Ok(n) = nimble(hw) {
                    // the component's copy of the flag follows the setting (status tag 29)
                    let r = BleMode { boot: if on { BLE_BOOT_ON } else { BLE_BOOT_OFF }, ..Default::default() };
                    if let Some(f) = n.mode {
                        unsafe { f(hw.ctx(), &r) };
                    }
                }
                if on {
                    hw.alarm_set(BOOT_DELAY_MS, 0, 0, ALARM_EXACT, COMPONENT, M_BOOT);
                }
            }
            return CONTINUE;
        }
        if c.component != COMPONENT {
            return CONTINUE;
        }
        if c.method == M_BOOT {
            // the pairing window UUID: this module knows no pairing secret (the C runtime has none)
            let _ = self.start(hw, &BleStart::default());
            return CONTINUE;
        }
        if !c.wants_response() {
            return CONTINUE;
        }
        let mut result = [0u8; RESP_MAX];
        match api_gen::dispatch(self, hw, c.method, c.data, &mut result) {
            Ok(n) => {
                hw.reply(c, &result[..n]);
            }
            // an envelope request gets the runtime's one error shape; a direct request keeps `{99: code}` of API.md
            Err(rc) if c.is_envelope() => {
                hw.reply_error(c, rc);
            }
            Err(rc) => {
                let mut e = [0u8; 12];
                let mut w = Writer::new(&mut e);
                w.map(1).uint(99).int(rc as i64);
                if let Some(r) = w.finish() {
                    hw.reply(c, r);
                }
            }
        }
        CONTINUE
    }
}

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
#[cfg(feature = "sim")]
export!();

#[allow(dead_code)]
fn _unused(_: *mut c_void) {}

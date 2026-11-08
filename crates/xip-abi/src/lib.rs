//! The esp-xip-modules ABI for Rust modules and applications: the host table and the C ABI of `include/xip.h` (generated
//! from [`Host`] by `scripts/gen.sh`), the components a host offers ([`Wifi`], [`WifiTuner`], [`Crypto`]), and the safe
//! [`Hw`] wrapper a module uses. The model is event driven: a module is a [`Module`] whose `handle` runs to completion
//! for every call and never waits. [`Hw`] keeps the raw table pointer, checks the table `size` before each call (an older
//! host has fewer slots) and returns [`ERR_UNSUPPORTED`] for a call the host does not have. Nothing here allocates unless a
//! module asks for `alloc` (features `allocator-api`, `global-alloc`: [`heap`]); the crate is `no_std`.
//!
//! How to write a module: `docs/writing-modules.md`. The CBOR, JSON and text view of the APIs (field tags, names) is in
//! `api/*.API.md`; this crate and `include/xip.h` are the reference for the C ABI.
//!
//! # Model
//!
//! A module is a set of **handlers**. It never waits, sleeps or loops: the host calls
//! `xip_handle(host, state, call)` when something happens, the handler does its bounded work and returns, and the
//! host sleeps until the next alarm, interrupt or request. One host task runs every module's handlers, one at a
//! time, so a module costs memory for its image and its state block only.
//!
//! ```c
//! int xip_handle(const xip_host *host, void *state, const xip_call *call);
//! ```
//!
//! * `state` is a zeroed block of `state_bytes` (image header) that the host keeps for the module. An image has no
//!   writable data, so everything the module remembers between calls lives there.
//! * `call->component` and `call->method` say what happened. They are a pair of integers chosen by **whoever
//!   created the event** and returned unchanged, as in the ssh-mesh envelope (keys `component`, `method`): the
//!   module picks them when it registers an alarm or an interrupt, main picks them when it posts a request. A
//!   module dispatches on them; component 0 is the host's own (`HOST`).
//! * `call->data` / `call->len` is the payload (a request, usually one CBOR map keyed by field tags) and is valid
//!   during the call only. A handler answers a request with `Hw::reply`.
//! * Time budget: a handler returns within `CONFIG_XIP_HANDLER_MS` (default 20 ms). `spi_transfer`,
//!   `i2c_transfer` and `adc_read` are bounded and synchronous. Anything longer (a radio transmission, a settle
//!   time) is started, and its end is an `alarm` or an
//!   `interrupt`.
//! * Return value: `0` keep running; `RET_DONE` the module is finished (the host destroys the instance after
//!   the call); a negative `result` is logged and also destroys the instance.
//!
//! Every call to the host takes `host->user` first; the table lists the other arguments. Check
//! `XIP_HOST_HAS(host, call)` before using a call newer than the version a module was built against.
//!
//! # Results
//!
//! Success is 0 or positive, errors are negative: `OK`, `ERR_ARG`, `ERR_UNSUPPORTED`, ... (see the constants).
//!
//! # Helpers
//!
//! Not host calls: header-only code in `include/xip_cbor.h` (C) and `xip-abi`'s `cbor` module (Rust), for
//! modules and hosts alike.
//!
//! * Encoder: `xip_cbor_uint`, `xip_cbor_int`, `xip_cbor_bytes`, `xip_cbor_text`, `xip_cbor_map`, `xip_cbor_array`,
//!   `xip_cbor_bool` write to a bounded buffer and report overflow once, at the end.
//! * Decoder: `xip_cbor_next` reads one item (type, integer, or a pointer and length for bytes and text) and
//!   `xip_cbor_skip` steps over a whole item; `xip_cbor_field` finds the value of an unsigned-integer key in a map.
//! * Only definite lengths, integers up to 64 bits, byte and text strings, arrays, maps and booleans; floats and
//!   tags are skipped but not decoded.
#![no_std]
#![cfg_attr(feature = "allocator-api", feature(allocator_api))]

#[cfg(test)]
extern crate std;

pub mod cbor;
pub mod fields;
pub mod heap;
pub mod task;
mod host;
pub use host::*;
mod vt;
pub mod schema;
mod wifi;
pub use wifi::*;
mod link;
pub use link::*;
mod serial;
pub use serial::*;
mod stream;
pub use stream::*;
mod quic;
pub use quic::*;
mod power;
pub use power::*;
mod ble;
pub use ble::*;
mod ble_vtable;
pub use ble_vtable::Ble;
mod nimble;
pub use nimble::*;
mod hardware;
pub use hardware::*;

mod crypto;
pub use crypto::*;
mod identity;
pub use identity::*;
mod native;
pub use native::*;

/// The host calls of a module compiled to WebAssembly (`modules/WASM.md`): named imports of the module `xip`, in the one
/// address space the host and all modules share. Only the calls listed here exist so far; the rest answer `ERR_UNSUPPORTED`.
/// cbindgen:ignore
#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::Host;
    pub struct Empty(pub Host);
    unsafe impl Sync for Empty {}
    pub static EMPTY: Empty = Empty(Host {
        abi_version: crate::ABI_VERSION,
        ..unsafe { core::mem::zeroed() }
    });

    /// A host call this module needs that the wasm bridge does not have: the host names it (a porting aid), the module gets `ERR_UNSUPPORTED`.
    pub fn unsupported(name: &str) -> i32 {
        unsafe { note_unsupported(name.as_ptr(), name.len()) };
        crate::ERR_UNSUPPORTED
    }

    #[link(wasm_import_module = "xip")]
    extern "C" {
        fn note_unsupported(name: *const u8, len: usize);
        pub fn log(data: *const u8, len: usize) -> i32;
        pub fn now_ms() -> u64;
        pub fn mem_alloc(size: usize) -> *mut u8;
        pub fn mem_free(ptr: *mut u8) -> i32;
        pub fn pkt_info(headroom: *mut u32, max: *mut u32) -> i32;
        pub fn pkt_alloc(out: *mut u32) -> i32;
        pub fn pkt_buf(pkt: u32, ptr: *mut *mut u8, max: *mut usize) -> i32;
        pub fn pkt_keep(pkt: u32) -> i32;
        pub fn pkt_free(pkt: u32) -> i32;
        pub fn send(pkt: u32, len: usize, dst: u32, component: i32, method: i32, id: u64) -> i32;
        pub fn device_info(out: *mut u8, cap: usize) -> i32;
        pub fn info_publish(component: i32, method: i32, data: *const u8, len: usize) -> i32;
        pub fn pkt_span(pkt: u32, base: *mut *mut u8, cap: *mut usize) -> i32;
        pub fn socket_read(socket: u32, out: *mut u8, len: usize) -> i32;
        pub fn socket_write(socket: u32, data: *const u8, len: usize) -> i32;
        pub fn socket_close(socket: u32) -> i32;
        pub fn socket_peer(socket: u32, address: *mut u8, port: *mut u16, scope: *mut u32) -> i32;
        pub fn udp_send(handle: u32, ip: *const u8, port: u16, scope: u32, data: *const u8, len: usize) -> i32;
        pub fn socket_write_pkt(socket: u32, pkt: u32, start: u32, len: u32) -> i32;
        pub fn modules_info(out: *mut u8, cap: usize) -> i32;
        pub fn module_start(name: *const u8, name_len: usize, request: *const u8, request_len: usize) -> i32;
        pub fn module_stop(name: *const u8, name_len: usize) -> i32;
        pub fn module_manifest(name: *const u8, name_len: usize, out: *mut u8, cap: usize) -> i32;
        pub fn module_rescan() -> i32;
        pub fn reset() -> i32;
        pub fn heap_info(out: *mut u8, cap: usize) -> i32;
        pub fn heap_dump(kind: i32) -> i32;
        pub fn nv(key: *const u8, key_len: usize, value: *const u8, value_len: usize, out: *mut u8, cap: usize) -> i32;
        pub fn flash_erase(address: u32, offset: u32, len: u32, component: i32, method: i32, id: u64) -> i32;
        pub fn flash_write(address: u32, offset: u32, pkt: u32, start: u32, len: u32, component: i32, method: i32, id: u64) -> i32;
        pub fn flash_read(address: u32, offset: u32, out: *mut u8, len: usize) -> i32;
        pub fn spi_open(config: *const crate::SpiConfig) -> i32;
        pub fn spi_transfer(handle: i32, tx: *const u8, rx: *mut u8, len: usize) -> i32;
        pub fn spi_close(handle: i32) -> i32;
        pub fn i2c_transfer(port: i32, sda: i32, scl: i32, hz: u32, address: u8, tx: *const u8, tx_len: usize, rx: *mut u8, rx_len: usize, timeout_ms: u32) -> i32;
        pub fn adc_read(pin: i32, ref_mv: u32, raw: *mut i32, mv: *mut u32, unit: *mut i32, channel: *mut i32) -> i32;
        pub fn rgbled_write(pin: i32, red: u8, green: u8, blue: u8) -> i32;
        pub fn alarm_set(delay: u32, window: u32, interval: u32, flags: u32, component: i32, method: i32) -> i32;
        pub fn alarm_cancel(alarm: i32) -> i32;
        pub fn get_setting(key: *const u8, key_len: usize, value: *mut u8, cap: usize, len: *mut usize) -> i32;
        pub fn set_setting(key: *const u8, key_len: usize, value: *const u8, len: usize) -> i32;
        pub fn gpio_config(pin: i32, mode: i32, pull: i32, level: i32) -> i32;
        pub fn gpio_read(pin: i32) -> i32;
        pub fn gpio_write(pin: i32, level: i32) -> i32;
        pub fn irq_register(pin: i32, edge: i32, component: i32, method: i32) -> i32;
        pub fn irq_unregister(pin: i32) -> i32;
        pub fn irq_enable(pin: i32, enabled: i32) -> i32;
    }
}

/// Table access for a module. Copy it freely: it is a pointer.
#[derive(Clone, Copy)]
pub struct Hw {
    host: *const Host,
}

/// wasm32: a host call is a named import when there is one (`wasm`), `ERR_UNSUPPORTED` otherwise (the arguments are still checked).
#[cfg(target_arch = "wasm32")]
macro_rules! wasm_imp {
    (pkt_info ( $($a:expr),* )) => { unsafe { wasm::pkt_info($($a),*) } };
    (pkt_alloc ( $($a:expr),* )) => { unsafe { wasm::pkt_alloc($($a),*) } };
    (pkt_buf ( $($a:expr),* )) => { unsafe { wasm::pkt_buf($($a),*) } };
    (pkt_keep ( $($a:expr),* )) => { unsafe { wasm::pkt_keep($($a),*) } };
    (pkt_free ( $($a:expr),* )) => { unsafe { wasm::pkt_free($($a),*) } };
    (send ( $($a:expr),* )) => { unsafe { wasm::send($($a),*) } };
    (device_info ( $($a:expr),* )) => { unsafe { wasm::device_info($($a),*) } };
    (info_publish ( $($a:expr),* )) => { unsafe { wasm::info_publish($($a),*) } };
    (pkt_span ( $($a:expr),* )) => { unsafe { wasm::pkt_span($($a),*) } };
    (socket_read ( $($a:expr),* )) => { unsafe { wasm::socket_read($($a),*) } };
    (socket_write ( $($a:expr),* )) => { unsafe { wasm::socket_write($($a),*) } };
    (socket_close ( $($a:expr),* )) => { unsafe { wasm::socket_close($($a),*) } };
    (socket_peer ( $($a:expr),* )) => { unsafe { wasm::socket_peer($($a),*) } };
    (udp_send ( $($a:expr),* )) => { unsafe { wasm::udp_send($($a),*) } };
    (socket_write_pkt ( $($a:expr),* )) => { unsafe { wasm::socket_write_pkt($($a),*) } };
    (modules_info ( $($a:expr),* )) => { unsafe { wasm::modules_info($($a),*) } };
    (module_start ( $($a:expr),* )) => { unsafe { wasm::module_start($($a),*) } };
    (module_stop ( $($a:expr),* )) => { unsafe { wasm::module_stop($($a),*) } };
    (module_manifest ( $($a:expr),* )) => { unsafe { wasm::module_manifest($($a),*) } };
    (module_rescan ( $($a:expr),* )) => { unsafe { wasm::module_rescan($($a),*) } };
    (reset ( $($a:expr),* )) => { unsafe { wasm::reset($($a),*) } };
    (heap_info ( $($a:expr),* )) => { unsafe { wasm::heap_info($($a),*) } };
    (heap_dump ( $($a:expr),* )) => { unsafe { wasm::heap_dump($($a),*) } };
    (nv ( $($a:expr),* )) => { unsafe { wasm::nv($($a),*) } };
    (flash_erase ( $($a:expr),* )) => { unsafe { wasm::flash_erase($($a),*) } };
    (flash_write ( $($a:expr),* )) => { unsafe { wasm::flash_write($($a),*) } };
    (flash_read ( $($a:expr),* )) => { unsafe { wasm::flash_read($($a),*) } };
    (alarm_set ( $($a:expr),* )) => { unsafe { wasm::alarm_set($($a),*) } };
    (alarm_cancel ( $($a:expr),* )) => { unsafe { wasm::alarm_cancel($($a),*) } };
    (get_setting ( $($a:expr),* )) => { unsafe { wasm::get_setting($($a),*) } };
    (set_setting ( $($a:expr),* )) => { unsafe { wasm::set_setting($($a),*) } };
    ($other:ident ( $($a:expr),* )) => {{ $( let _ = &$a; )* wasm::unsupported(stringify!($other)) }};
}

#[cfg(target_arch = "wasm32")]
macro_rules! call {
    ($self:ident, $slot:ident, $field:ident ( $($arg:expr),* )) => {{ let _ = &$self; wasm_imp!($field ( $($arg),* )) }};
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! call {
    ($self:ident, $slot:ident, $field:ident ( $($arg:expr),* )) => {{
        let h = $self.table();
        if (h.size as usize) < slot::$slot + 4 { ERR_UNSUPPORTED } else {
            match h.$field { Some(f) => unsafe { f(h.user $(, $arg)*) }, None => ERR_UNSUPPORTED }
        }
    }};
}

/// A call through the `hw` component (`ERR_UNSUPPORTED` when the host has none or leaves the member empty).
#[cfg(target_arch = "wasm32")]
macro_rules! hwcall {
    ($self:ident, $field:ident ( $($arg:expr),* )) => {{ let _ = &$self; wasm_hw!($field ( $($arg),* )) }};
}

/// wasm32: the `hw` component is a set of named imports (`gpio_config`, ...) instead of a table found with `get_component`.
#[cfg(target_arch = "wasm32")]
macro_rules! wasm_hw {
    (spi_open ( $($a:expr),* )) => { unsafe { wasm::spi_open($($a),*) } };
    (spi_transfer ( $($a:expr),* )) => { unsafe { wasm::spi_transfer($($a),*) } };
    (spi_close ( $($a:expr),* )) => { unsafe { wasm::spi_close($($a),*) } };
    (i2c_transfer ( $($a:expr),* )) => { unsafe { wasm::i2c_transfer($($a),*) } };
    (adc_read ( $($a:expr),* )) => { unsafe { wasm::adc_read($($a),*) } };
    (rgbled_write ( $($a:expr),* )) => { unsafe { wasm::rgbled_write($($a),*) } };
    (gpio_config ( $($a:expr),* )) => { unsafe { wasm::gpio_config($($a),*) } };
    (gpio_read ( $($a:expr),* )) => { unsafe { wasm::gpio_read($($a),*) } };
    (gpio_write ( $($a:expr),* )) => { unsafe { wasm::gpio_write($($a),*) } };
    (irq_register ( $($a:expr),* )) => { unsafe { wasm::irq_register($($a),*) } };
    (irq_unregister ( $($a:expr),* )) => { unsafe { wasm::irq_unregister($($a),*) } };
    (irq_enable ( $($a:expr),* )) => { unsafe { wasm::irq_enable($($a),*) } };
    ($other:ident ( $($a:expr),* )) => {{ $( let _ = &$a; )* wasm::unsupported(stringify!($other)) }};
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! hwcall {
    ($self:ident, $field:ident ( $($arg:expr),* )) => {{
        match $self.hardware() {
            Some(t) => match t.$field { Some(f) => unsafe { f($self.ctx() $(, $arg)*) }, None => ERR_UNSUPPORTED },
            None => ERR_UNSUPPORTED,
        }
    }};
}

impl Hw {
    /// # Safety
    /// `host` must be the pointer the handler received, and stay valid during the call.
    pub unsafe fn new(host: *const Host) -> Option<Hw> {
        // wasm32: the host is reached through named imports (`wasm`), not a table; the pointer only has to be non-null
        if cfg!(target_arch = "wasm32") {
            return if host.is_null() { None } else { Some(Hw { host }) };
        }
        if host.is_null() || (*host).abi_version != ABI_VERSION || ((*host).size as usize) < slot::LOG + 4 {
            return None;
        }
        Some(Hw { host })
    }

    /// The raw table pointer, for a module that keeps it (the context of a hosted component).
    pub fn as_ptr(&self) -> *const Host {
        self.host
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn table(&self) -> &Host {
        unsafe { &*self.host }
    }

    /// wasm32 modules have no table: this empty one (`size` 0) makes every call that has no import yet `ERR_UNSUPPORTED`.
    #[cfg(target_arch = "wasm32")]
    fn table(&self) -> &Host {
        &wasm::EMPTY.0
    }

    pub fn abi_version(&self) -> u32 {
        self.table().abi_version
    }


    /// Structured device status, including cached component/method contributions.
    pub fn device_info(&self, out: &mut [u8]) -> Result<usize, i32> {
        let n = call!(self, DEVICE_INFO, device_info(out.as_mut_ptr(), out.len()));
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }

    /// Publish a copied CBOR map. An empty slice clears this component/method key.
    pub fn info_publish(&self, component: i32, method: i32, map: &[u8]) -> i32 {
        call!(self, INFO_PUBLISH, info_publish(component, method, map.as_ptr(), map.len()))
    }

    pub fn log(&self, text: &[u8]) -> i32 {
        #[cfg(target_arch = "wasm32")]
        return unsafe { wasm::log(text.as_ptr(), text.len()) };
        #[cfg(not(target_arch = "wasm32"))]
        call!(self, LOG, log(text.as_ptr(), text.len()))
    }

    pub fn now_ms(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        return unsafe { wasm::now_ms() };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let h = self.table();
            if (h.size as usize) < slot::NOW_MS + 4 { return 0; }
            match h.now_ms { Some(f) => unsafe { f(h.user) }, None => 0 }
        }
    }

    /// Monotonic clock for radio synchronization; None on older runtimes.
    pub fn now_us(&self) -> Option<u64> {
        let h = self.table();
        if (h.size as usize) < core::mem::offset_of!(Host, now_us) + core::mem::size_of::<usize>() { return None; }
        h.now_us.map(|f| unsafe { f(h.user) })
    }

    /// `(headroom, max payload)` of this host's packets.
    pub fn pkt_info(&self) -> Result<(u32, u32), i32> {
        let (mut headroom, mut max) = (0u32, 0u32);
        let rc = call!(self, PKT_INFO, pkt_info(&mut headroom, &mut max));
        if rc == 0 { Ok((headroom, max)) } else { Err(rc) }
    }

    /// Memory that outlives a call, recorded by the host: free it before the module stops (`ERR_LEAK` otherwise). Null when
    /// the quota is used.
    pub fn mem_alloc(&self, size: usize) -> *mut u8 {
        #[cfg(target_arch = "wasm32")]
        return unsafe { wasm::mem_alloc(size) };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let h = self.table();
            if (h.size as usize) < slot::MEM_ALLOC + 4 { return core::ptr::null_mut(); }
            match h.mem_alloc { Some(f) => unsafe { f(h.user, size).cast() }, None => core::ptr::null_mut() }
        }
    }

    /// A host-owned table identified by its ordinary component header. NULL if
    /// unavailable. This does not add a context argument to native functions.
    pub fn get_component(&self, name: &core::ffi::CStr) -> *const core::ffi::c_void {
        #[cfg(target_arch = "wasm32")]
        return vt::shim::table(name);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let h = self.table();
            if (h.size as usize) < core::mem::offset_of!(Host, get_component) + core::mem::size_of::<usize>() {
                return core::ptr::null();
            }
            match h.get_component {
                Some(f) => unsafe { f(h.user, name.as_ptr()).cast() },
                None => core::ptr::null(),
            }
        }
    }

    pub fn mem_free(&self, ptr: *mut u8) -> i32 {
        #[cfg(target_arch = "wasm32")]
        return unsafe { wasm::mem_free(ptr) };
        #[cfg(not(target_arch = "wasm32"))]
        call!(self, MEM_FREE, mem_free(ptr.cast()))
    }

    /// A new empty packet held by this module (`ERR_BUSY` at its quota, `ERR_NOMEM` when the pool is low).
    pub fn pkt_alloc(&self) -> Result<u32, i32> {
        let mut pkt = 0u32;
        let rc = call!(self, PKT_ALLOC, pkt_alloc(&mut pkt));
        if rc == 0 { Ok(pkt) } else { Err(rc) }
    }

    /// The payload window of a packet: a pointer and how many bytes may be written (`ERR_BUSY` when it is shared). Valid
    /// until the packet is sent or freed.
    pub fn pkt_buf(&self, pkt: u32) -> Result<(*mut u8, usize), i32> {
        let (mut ptr, mut max) = (core::ptr::null_mut(), 0usize);
        let rc = call!(self, PKT_BUF, pkt_buf(pkt, &mut ptr, &mut max));
        if rc == 0 { Ok((ptr, max)) } else { Err(rc) }
    }

    /// Full slot for reading. Writing requires uniqueness; valid until release or transfer.
    pub fn pkt_span(&self, pkt: u32) -> Result<(*mut u8, usize), i32> {
        let (mut base, mut cap) = (core::ptr::null_mut(), 0usize);
        let rc = call!(self, PKT_SPAN, pkt_span(pkt, &mut base, &mut cap));
        if rc == 0 { Ok((base, cap)) } else { Err(rc) }
    }

    /// POSIX-like nonblocking read: positive count, zero EOF, negative error including ERR_AGAIN.
    pub fn socket_read(&self, socket: u32, out: &mut [u8]) -> i32 {
        call!(self, SOCKET_READ, socket_read(socket, out.as_mut_ptr(), out.len()))
    }

    /// POSIX-like nonblocking write. The host does not retain the slice after return.
    pub fn socket_write(&self, socket: u32, data: &[u8]) -> i32 {
        call!(self, SOCKET_WRITE, socket_write(socket, data.as_ptr(), data.len()))
    }

    pub fn socket_close(&self, socket: u32) -> i32 {
        call!(self, SOCKET_CLOSE, socket_close(socket))
    }

    pub fn socket_peer(&self, socket: u32) -> Result<([u8; 16], u16, u32), i32> {
        let (mut address, mut port, mut scope) = ([0; 16], 0, 0);
        let rc = call!(self, SOCKET_PEER, socket_peer(socket, address.as_mut_ptr(), &mut port, &mut scope));
        if rc == 0 { Ok((address, port, scope)) } else { Err(rc) }
    }

    /// Send a datagram from a runtime-owned UDP socket (the `call.from` handle of a
    /// datagram receive) to `dest`. ERR_BUSY: send again later. The helper copies
    /// like `socket_write`; a failed `send` owns nothing.
    /// Handle 0 selects the native discovery socket for an unsolicited announcement.
    pub fn udp_send(&self, handle: u32, dest: ([u8; 16], u16, u32), data: &[u8]) -> i32 {
        call!(self, UDP_SEND, udp_send(handle, dest.0.as_ptr(), dest.1, dest.2, data.as_ptr(), data.len()))
    }

    /// Transfer a held packet only when the runtime accepts the write. On error the packet remains held.
    pub fn socket_write_pkt(&self, socket: u32, pkt: u32, start: u32, len: u32) -> i32 {
        call!(self, SOCKET_WRITE_PKT, socket_write_pkt(socket, pkt, start, len))
    }

    pub fn modules_info(&self, out: &mut [u8]) -> i32 {
        call!(self, MODULES_INFO, modules_info(out.as_mut_ptr(), out.len()))
    }
    pub fn module_stop(&self, name: &[u8]) -> i32 {
        call!(self, MODULE_STOP, module_stop(name.as_ptr(), name.len()))
    }
    pub fn reset(&self) -> i32 { call!(self, RESET, reset()) }

    /// Answer the request `call` (a call with `wants_response()`): `data` goes back to whoever sent it as a message with the same `component`,
    /// `method` and `id`. By convention one CBOR item, a map keyed by field tags like the request. `ERR_ARG` if the call is not a request, `ERR_NOMEM` if the
    /// pool has no packet, `ERR_SIZE` if `data` does not fit one.
    pub fn reply(&self, call: &CallView, data: &[u8]) -> i32 {
        if !call.wants_response() { return ERR_ARG; }
        self.message(call.from, call.component, call.method, call.id, data)
    }

    /// Answer `HOST_TABLE` for a hosted component: `table` is the module's table of functions (`{u32 version, u32 size, then one function pointer per
    /// method}`, every function `int f(void *ctx, ...)` with at most five more words of arguments) and `ctx` the first argument it is called with. The
    /// runtime hands callers a copy whose functions take the module's lock, call these with `ctx` and return `ERR_NOTFOUND` once the module has stopped.
    /// `schema` is the component's schema (`schema::SCHEMA_*`, a static of the module) that tells a runtime which arguments are structs, strings or bytes
    /// when it cannot call the functions directly (a wasm module); a runtime that can ignores it.
    pub fn provide_table(&self, call: &CallView, table: *const core::ffi::c_void, ctx: *mut core::ffi::c_void, schema: &'static [u8]) -> i32 {
        let w = core::mem::size_of::<usize>();
        let mut bytes = [0u8; 3 * core::mem::size_of::<usize>()];
        bytes[..w].copy_from_slice(&(table as usize).to_ne_bytes());
        bytes[w..2 * w].copy_from_slice(&(ctx as usize).to_ne_bytes());
        bytes[2 * w..].copy_from_slice(&(schema.as_ptr() as usize).to_ne_bytes());
        self.reply(call, &bytes)
    }

    /// Answer a request with an error: a bare negative integer, which the runtime turns into `7: error` for an envelope ([`CALL_ENVELOPE`]). The
    /// shape modules use to refuse a request, so that the caller sees one error form.
    pub fn reply_error(&self, call: &CallView, rc: i32) -> i32 {
        let mut buf = [0u8; 9];
        let mut w = cbor::Writer::new(&mut buf);
        w.int(rc as i64);
        match w.finish() {
            Some(bytes) => self.reply(call, bytes),
            None => ERR_SIZE,
        }
    }

    /// Send `data` as a message to `dst` (`ADDR_MAIN`: the application, or a module's component): the pool packet is allocated, filled and sent in one go. What a
    /// module used to `emit` is a message to the application with its own component and the event number as the method.
    /// The helper releases its internal packet on a blocked link; retry using the original data.
    pub fn message(&self, dst: u32, component: i32, method: i32, id: u64, data: &[u8]) -> i32 {
        let pkt = match self.pkt_alloc() { Ok(p) => p, Err(e) => return e };
        match self.pkt_buf(pkt) {
            Ok((window, room)) if data.len() <= room => unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), window, data.len()) },
            Ok(_) => { self.pkt_free(pkt); return ERR_SIZE; }
            Err(e) => { self.pkt_free(pkt); return e; }
        }
        let result = self.send(pkt, data.len(), dst, component, method, id);
        // A blocked link retains the packet, but this copying helper cannot hand
        // that lease back to its caller. Release it; the caller can retry data.
        if dst >= ADDR_LINK && result == ERR_BUSY { self.pkt_free(pkt); }
        result
    }

    /// An event: `data` as a message to the application with the module's own `component` and the event number as the method (what a module used to
    /// `emit`). Nobody listening is not an error for the module; the host decides what to do with it (a log and routing component will).
    pub fn event(&self, component: i32, id: u16, data: &[u8]) -> i32 {
        match self.message(ADDR_MAIN, component, id as i32, 0, data) {
            ERR_NOTFOUND => 0, // the application listens to nothing: the event is dropped
            rc => rc,
        }
    }

    /// Start the installed module `name` with `request`.
    pub fn module_start(&self, name: &[u8], request: &[u8]) -> i32 {
        call!(self, MODULE_START, module_start(name.as_ptr(), name.len(), request.as_ptr(), request.len()))
    }
    /// The manifest of the installed module `name` (CBOR), or a negative error.
    pub fn module_manifest(&self, name: &[u8], out: &mut [u8]) -> i32 {
        call!(self, MODULE_MANIFEST, module_manifest(name.as_ptr(), name.len(), out.as_mut_ptr(), out.len()))
    }
    /// Rescan the module partition after an install: the highest version of each name becomes the module (`Host::module_rescan`).
    pub fn module_rescan(&self) -> i32 {
        call!(self, MODULE_RESCAN, module_rescan())
    }
    /// Free memory of the device, as CBOR.
    pub fn heap_info(&self, out: &mut [u8]) -> i32 {
        call!(self, HEAP_INFO, heap_info(out.as_mut_ptr(), out.len()))
    }
    /// Print a heap report to the device log (kinds in `include/xip.h`).
    pub fn heap_dump(&self, kind: i32) -> i32 {
        call!(self, HEAP_DUMP, heap_dump(kind))
    }
    /// A device setting: `value` None reads it into `out` (the length, or a negative error); Some sets it (empty: the default).
    pub fn nv(&self, key: &[u8], value: Option<&[u8]>, out: &mut [u8]) -> i32 {
        let (vp, vl) = match value { Some(v) => (v.as_ptr(), v.len()), None => (core::ptr::null(), 0) };
        call!(self, NV, nv(key.as_ptr(), key.len(), vp, vl, out.as_mut_ptr(), out.len()))
    }

    /// The `wifi` component (the wanted state of the radio), or None when this host has none.
    pub fn wifi(&self) -> Option<&'static Wifi> {
        let p = self.get_component(c"wifi") as *const Wifi;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == WIFI_VERSION && t.size as usize >= core::mem::size_of::<Wifi>()).then_some(t)
    }
    /// Native transport-neutral module streams, when supplied by the host.
    pub fn stream(&self) -> Option<&'static Stream> {
        let p=self.get_component(c"stream").cast::<Stream>();
        if p.is_null(){return None;}
        let t=unsafe{&*p};
        (t.version==STREAM_VERSION && t.size as usize>=core::mem::size_of::<Stream>()).then_some(t)
    }
    /// The `link` component (frames between the runtime, modules and transports), or None when this host has none.
    pub fn link(&self) -> Option<&'static Link> {
        let p = self.get_component(c"link") as *const Link;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == LINK_VERSION && t.size as usize >= core::mem::size_of::<Link>()).then_some(t)
    }
    /// The `hw` component (pins, interrupts, SPI, I2C, ADC, RGB LED), or None when this host has none.
    pub fn hardware(&self) -> Option<&'static Hardware> {
        let p = self.get_component(c"hw") as *const Hardware;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == HARDWARE_VERSION && t.size as usize >= core::mem::size_of::<Hardware>()).then_some(t)
    }
    /// The `power` component (sleep, power and FreeRTOS state), or None when this host has none.
    pub fn power(&self) -> Option<&'static Power> {
        let p = self.get_component(c"power") as *const Power;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == POWER_VERSION && t.size as usize >= core::mem::size_of::<Power>()).then_some(t)
    }
    /// The `ble` component (component 104; hosted by `mod_ble`), or None when no module hosts it.
    pub fn ble(&self) -> Option<&'static Ble> {
        let p = self.get_component(c"ble") as *const Ble;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == BLE_VERSION && t.size as usize >= core::mem::size_of::<Ble>()).then_some(t)
    }
    /// The `nimble` component (the NimBLE subset the runtime offers: advertising, scan, CoC, bonds), or None when this host has none.
    pub fn nimble(&self) -> Option<&'static Nimble> {
        let p = self.get_component(c"nimble") as *const Nimble;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == NIMBLE_VERSION && t.size as usize >= core::mem::size_of::<Nimble>()).then_some(t)
    }
    /// The `ctx` that the functions of the component tables take first (the `user` of the host table).
    pub fn ctx(&self) -> *mut core::ffi::c_void {
        self.table().user
    }
    /// The `wifi-tuning` component (advanced driver settings), or None when this host has none.
    pub fn wifi_tuner(&self) -> Option<&'static WifiTuner> {
        let p = self.get_component(c"wifi-tuning") as *const WifiTuner;
        if p.is_null() { return None; }
        let t = unsafe { &*p };
        (t.version == WIFI_TUNING_VERSION && t.size as usize >= core::mem::size_of::<WifiTuner>()).then_some(t)
    }
    pub fn flash_erase(&self, address: u32, offset: u32, len: u32, component: i32, method: i32, id: u64) -> i32 {
        call!(self, FLASH_ERASE, flash_erase(address, offset, len, component, method, id))
    }

    /// Transfer a uniquely held pool packet only when the job is accepted.
    pub fn flash_write(&self, address: u32, offset: u32, pkt: u32, start: u32, len: u32, component: i32, method: i32, id: u64) -> i32 {
        call!(self, FLASH_WRITE, flash_write(address, offset, pkt, start, len, component, method, id))
    }

    pub fn flash_read(&self, address: u32, offset: u32, out: &mut [u8]) -> i32 {
        call!(self, FLASH_READ, flash_read(address, offset, out.as_mut_ptr(), out.len()))
    }

    /// Retain one additional reference; each clone must be released or transferred.
    pub fn pkt_keep(&self, pkt: u32) -> i32 {
        call!(self, PKT_KEEP, pkt_keep(pkt))
    }

    pub fn pkt_free(&self, pkt: u32) -> i32 {
        call!(self, PKT_FREE, pkt_free(pkt))
    }

    /// Hand a packet over: the first `len` payload bytes go to `dst` (`ADDR_MAIN` or a module's component) as a call
    /// `(component, method)` with `id`. The handle is consumed whether it works or not.
    pub fn send(&self, pkt: u32, len: usize, dst: u32, component: i32, method: i32, id: u64) -> i32 {
        call!(self, SEND, send(pkt, len, dst, component, method, id))
    }

    /// Call me later: at `delay_ms` the host calls the handler with `(component, method)` and `value` = lateness in
    /// ms. `interval_ms` > 0 repeats it (a poll). Returns an alarm id (>= 0) or a negative error.
    pub fn alarm_set(&self, delay_ms: u32, window_ms: u32, interval_ms: u32, flags: u32, component: i32, method: i32) -> i32 {
        call!(self, ALARM_SET, alarm_set(delay_ms, window_ms, interval_ms, flags, component, method))
    }

    pub fn alarm_cancel(&self, alarm: i32) -> i32 {
        call!(self, ALARM_CANCEL, alarm_cancel(alarm))
    }


    pub fn get_setting(&self, key: &[u8], value: &mut [u8]) -> Result<usize, i32> {
        let mut len = 0usize;
        let rc = call!(self, GET_SETTING, get_setting(key.as_ptr(), key.len(), value.as_mut_ptr(), value.len(), &mut len));
        if rc == 0 { Ok(len) } else { Err(rc) }
    }

    pub fn set_setting(&self, key: &[u8], value: &[u8]) -> i32 {
        call!(self, SET_SETTING, set_setting(key.as_ptr(), key.len(), value.as_ptr(), value.len()))
    }

    pub fn gpio_config(&self, pin: i32, mode: u32, pull: u32, level: i32) -> i32 {
        hwcall!(self, gpio_config(pin, mode as i32, pull as i32, level))
    }

    pub fn gpio_read(&self, pin: i32) -> i32 {
        hwcall!(self, gpio_read(pin))
    }

    pub fn gpio_write(&self, pin: i32, level: i32) -> i32 {
        hwcall!(self, gpio_write(pin, level))
    }

    /// Report the edges of `pin` as calls `(component, method)` with `value` = the level.
    pub fn irq_register(&self, pin: i32, edge: u32, component: i32, method: i32) -> i32 {
        hwcall!(self, irq_register(pin, edge as i32, component, method))
    }

    pub fn irq_unregister(&self, pin: i32) -> i32 {
        hwcall!(self, irq_unregister(pin))
    }

    pub fn irq_enable(&self, pin: i32, enabled: bool) -> i32 {
        hwcall!(self, irq_enable(pin, enabled as i32))
    }

    /// A handle (>= 0) or a negative error.
    pub fn spi_open(&self, config: &SpiConfig) -> i32 {
        hwcall!(self, spi_open(config))
    }

    pub fn spi_transfer(&self, handle: i32, tx: &[u8], rx: &mut [u8]) -> i32 {
        if rx.len() < tx.len() { return ERR_ARG; }
        hwcall!(self, spi_transfer(handle, tx.as_ptr(), rx.as_mut_ptr(), tx.len()))
    }

    pub fn spi_close(&self, handle: i32) -> i32 {
        hwcall!(self, spi_close(handle))
    }

    /// Write `tx`, then read `rx.len()` bytes in one transaction (either may be empty).
    pub fn i2c_transfer(&self, port: i32, sda: i32, scl: i32, hz: u32, address: u8, tx: &[u8], rx: &mut [u8],
                        timeout_ms: u32) -> i32 {
        hwcall!(self, i2c_transfer(port, sda, scl, hz, address, tx.as_ptr(), tx.len(),
                                              rx.as_mut_ptr(), rx.len(), timeout_ms))
    }

    /// One-shot reading of `pin`: `(raw, millivolts, adc unit, channel)`.
    pub fn adc_read(&self, pin: i32, ref_mv: u32) -> Result<(i32, u32, i32, i32), i32> {
        let (mut raw, mut mv, mut unit, mut channel) = (0i32, 0u32, 0i32, 0i32);
        let rc = hwcall!(self, adc_read(pin, ref_mv, &mut raw, &mut mv, &mut unit, &mut channel));
        if rc == 0 { Ok((raw, mv, unit, channel)) } else { Err(rc) }
    }

    pub fn rgbled_write(&self, pin: i32, red: u8, green: u8, blue: u8) -> i32 {
        hwcall!(self, rgbled_write(pin, red, green, blue))
    }
}

impl SpiConfig {
    /// A config with every optional pin unused and the size field set.
    pub const fn new(bus: i32) -> SpiConfig {
        SpiConfig {
            size: core::mem::size_of::<SpiConfig>() as u32,
            bus, sck: -1, miso: -1, mosi: -1, cs: -1, hz: 0, mode: 0,
            ready_pin: -1, ready_level: 0, ready_timeout_ms: 0,
        }
    }
}

/// What a handler is called with (the safe view of [`Call`](host::Call)).
pub struct CallView<'a> {
    pub component: i32,
    pub method: i32,
    pub value: i32,
    pub flags: u32,
    pub data: &'a [u8],
    /// Radio receive time, low 32 bits of the host microsecond clock.
    pub time_us: u32,
    /// The owned packet reference delivered with this call (0: none). handle_owned receives its Drop guard.
    pub pkt: u32,
    /// Who sent it (`ADDR_MAIN` = 0: the application): answer with `Hw::send` to this address.
    pub from: u32,
    /// The sender's correlation number, returned in the answer.
    pub id: u64,
    /// For a request that arrived as an envelope (`CALL_ENVELOPE`): the envelope's own id (field 3), `None` when it had none. `component`,
    /// `method` and `data` (the fields) are the envelope's too; `id` stays the reply token. A module that keeps a request open (a subscription)
    /// keys it by this.
    pub request_id: Option<u64>,
    /// Source of a datagram received on a runtime-owned UDP socket (`CALL_LINK`,
    /// `from` its handle): `(address, port, scope)` to answer with
    /// [`Hw::udp_send`]; `None` on other bearers (and on a shorter call).
    pub src: Option<([u8; 16], u16, u32)>,
}

impl<'a> CallView<'a> {
    /// # Safety
    /// `call` must be the pointer the host passed to the handler.
    pub unsafe fn new(call: *const Call) -> Option<CallView<'a>> {
        // an older host passes a shorter call (no packet fields): read only what its size covers
        if call.is_null() || ((*call).size as usize) < core::mem::offset_of!(Call, pkt) {
            return None;
        }
        let c = &*call;
        let has_packet = (c.size as usize) >= core::mem::offset_of!(Call, request_id);
        let has_request = (c.size as usize) >= core::mem::size_of::<Call>();
        let data = if c.len == 0 || c.data.is_null() { &[][..] } else { core::slice::from_raw_parts(c.data, c.len) };
        let (pkt, from, id) = if has_packet { (c.pkt, c.from, c.id) } else { (0, 0, 0) };
        let src = ((c.size as usize) >= core::mem::size_of::<Call>() && c.src_port != 0)
            .then_some((c.src_ip, c.src_port, c.src_scope));
        Some(CallView { component: c.component, method: c.method, value: c.value, flags: c.flags, data, time_us: if (c.size as usize) >= core::mem::offset_of!(Call, time_us)+4 {c.time_us} else {0}, pkt, from, id, request_id: (has_request && c.request_has_id != 0).then_some(c.request_id), src })
    }

    /// The call is a request that expects an answer ([`Hw::reply`]).
    pub fn wants_response(&self) -> bool {
        self.flags & 1 != 0
    }

    /// The request arrived as an envelope that the runtime parsed ([`CALL_ENVELOPE`]); `component`, `method` and `data` (the fields) are the
    /// envelope's and the runtime wraps the answer.
    pub fn is_envelope(&self) -> bool {
        self.flags & CALL_ENVELOPE != 0
    }

    /// `HOST_*` call of the host itself (start, stop, ...).
    pub fn is_host(&self, method: u32) -> bool {
        self.component == HOST_COMPONENT as i32 && self.method == method as i32
    }
}

/// One owned packet reference. Drop releases it; into_raw transfers it.
/// Construct only from a reference transferred by the host or retained explicitly.
pub struct Packet { hw: Hw, handle: u32 }
impl Packet {
    /// Take ownership of a live raw reference, exactly once.
    pub unsafe fn from_raw(hw: Hw, handle: u32) -> Self { Self { hw, handle } }
    pub fn raw(&self) -> u32 { self.handle }
    pub fn into_raw(mut self) -> u32 { let h=self.handle; self.handle=0; h }
    pub fn try_clone(&self) -> Result<Self,i32> {
        let rc=self.hw.pkt_keep(self.handle);
        if rc<0 {Err(rc)} else {Ok(unsafe {Self::from_raw(self.hw,self.handle)})}
    }
}
impl Drop for Packet { fn drop(&mut self) { if self.handle!=0 { self.hw.pkt_free(self.handle); } } }

/// A module's state and handler. The host keeps the state block (`state_bytes` of the image header), zeroed at
/// the start: **an all-zero `Self` must be a valid initial value** (integers, bools, arrays of them, `Option` of
/// those; no references or enums without a zero variant). [`export_module!`] wires it to `xip_handle`.
pub trait Module: Sized {
    /// Handle one call. Return [`RET_CONTINUE`], [`RET_DONE`] or a negative error.
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32;
    /// Owned packet delivery. Override to move a packet into longer-lived work.
    /// The default releases it when the ordinary handler returns.
    fn handle_owned(&mut self, hw: &Hw, call: &CallView, _packet: Option<Packet>) -> i32 {
        self.handle(hw,call)
    }
}

/// Export `xip_handle` for a [`Module`] type; `state = N` asserts at compile time that the type fits the state
/// block that the image header asks for (`mkmod.py --state N`).
#[macro_export]
macro_rules! export_module {
    ($ty:ty, state = $n:expr) => {
        const _: () = assert!(core::mem::size_of::<$ty>() <= $n && core::mem::align_of::<$ty>() <= 8);
        #[no_mangle]
        pub unsafe extern "C" fn xip_handle(host: *const $crate::Host, state: *mut u8, call: *const $crate::Call) -> i32 {
            let Some(hw) = $crate::Hw::new(host) else { return $crate::ERR_ARG };
            let Some(view) = $crate::CallView::new(call) else { return $crate::ERR_ARG };
            let module = &mut *(state as *mut $ty);
            let saved = $crate::heap::enter(host);
            let packet=(view.pkt!=0).then(|| $crate::Packet::from_raw(hw,view.pkt));
            let rc = <$ty as $crate::Module>::handle_owned(module, &hw, &view, packet);
            $crate::heap::leave(saved);
            rc
        }
    };
}

mod nan;
pub use nan::{NanTune, NanTransport, Serial};
mod nan_vtable;
pub use nan_vtable::Nan;
mod lora_vtable;
pub use lora_vtable::Lora;

mod system;
pub mod files;
pub use files::*;
pub use system::{SystemHeapInfo, SystemHeapDump};
mod system_vtable;
pub use system_vtable::System;

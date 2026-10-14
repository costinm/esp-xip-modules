//! The esp-xip-modules ABI for Rust modules and applications (abi/API.md; the model is event driven: a module is a
//! [`Module`] whose `handle` runs to completion for every call and never waits).
//!
//! `generated` mirrors `include/espmod.h` (written by `tools/gen_abi.py`, never edited by hand).
//! [`Hw`] is the safe, small wrapper a module uses: it keeps the raw table pointer, checks the table `size`
//! before each call (an older host has fewer slots) and returns [`ERR_UNSUPPORTED`] for a call the host
//! does not have. Nothing here allocates unless a module asks for `alloc` (features `allocator-api`, `global-alloc`: [`heap`]);
//! the crate is `no_std`.
#![no_std]
#![cfg_attr(feature = "allocator-api", feature(allocator_api))]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]

#[cfg(test)]
extern crate std;

pub mod cbor;
/// Desired bearer declaration and tagged-CBOR codec generated from abi/API.md.
pub mod profile;
pub mod heap;
mod generated;
pub use generated::*;

pub mod psa;

/// Table access for a module. Copy it freely: it is a pointer.
#[derive(Clone, Copy)]
pub struct Hw {
    host: *const Host,
}

macro_rules! call {
    ($self:ident, $slot:ident, $field:ident ( $($arg:expr),* )) => {{
        let h = $self.table();
        if (h.size as usize) < slot::$slot + 4 { ERR_UNSUPPORTED } else {
            match h.$field { Some(f) => unsafe { f(h.user $(, $arg)*) }, None => ERR_UNSUPPORTED }
        }
    }};
}

impl Hw {
    /// # Safety
    /// `host` must be the pointer the handler received, and stay valid during the call.
    pub unsafe fn new(host: *const Host) -> Option<Hw> {
        if host.is_null() || (*host).abi_version == 0 || ((*host).size as usize) < slot::LOG + 4 {
            return None;
        }
        Some(Hw { host })
    }

    fn table(&self) -> &Host {
        unsafe { &*self.host }
    }

    pub fn abi_version(&self) -> u32 {
        self.table().abi_version
    }

    /// `FEAT_*` bits the host really implements.
    pub fn features(&self) -> u32 {
        self.table().features
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
        call!(self, LOG, log(text.as_ptr(), text.len()))
    }

    pub fn now_ms(&self) -> u64 {
        let h = self.table();
        if (h.size as usize) < slot::NOW_MS + 4 { return 0; }
        match h.now_ms { Some(f) => unsafe { f(h.user) }, None => 0 }
    }

    /// Answer the current request (only valid inside a call with [`Call::wants_response`]): one CBOR item, copied.
    pub fn respond(&self, data: &[u8]) -> i32 {
        call!(self, RESPOND, respond(data.as_ptr(), data.len()))
    }

    /// Send an event to the application.
    pub fn emit(&self, event_id: u16, value_type: u32, value: &[u8]) -> i32 {
        call!(self, EMIT, emit(event_id, value_type as u8, value.as_ptr(), value.len()))
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
        let h = self.table();
        if (h.size as usize) < slot::MEM_ALLOC + 4 { return core::ptr::null_mut(); }
        match h.mem_alloc { Some(f) => unsafe { f(h.user, size) }, None => core::ptr::null_mut() }
    }

    /// A host-owned table identified by its ordinary component header. NULL if
    /// unavailable. This does not add a context argument to native functions.
    pub fn get_component(&self, name: &core::ffi::CStr) -> *const core::ffi::c_void {
        let h = self.table();
        if (h.size as usize) < core::mem::offset_of!(Host, get_component) + core::mem::size_of::<usize>() {
            return core::ptr::null();
        }
        match h.get_component {
            Some(f) => unsafe { f(h.user, name.as_ptr()).cast() },
            None => core::ptr::null(),
        }
    }

    pub fn mem_free(&self, ptr: *mut u8) -> i32 {
        call!(self, MEM_FREE, mem_free(ptr))
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

    /// Full uniquely owned slot for in-place prefix/suffix framing. Valid until release or transfer.
    pub fn pkt_span(&self, pkt: u32) -> Result<(*mut u8, usize), i32> {
        let (mut base, mut cap) = (core::ptr::null_mut(), 0usize);
        let rc = call!(self, PKT_SPAN, pkt_span(pkt, &mut base, &mut cap));
        if rc == 0 { Ok((base, cap)) } else { Err(rc) }
    }

    pub fn tcp_accept(&self, port: u16, mode: u32, component: i32, method: i32) -> Result<u32, i32> {
        let mut listener = 0;
        let rc = call!(self, TCP_ACCEPT, tcp_accept(port, mode, component, method, &mut listener));
        if rc == 0 { Ok(listener) } else { Err(rc) }
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

    /// Atomically apply a tagged-CBOR partial desired transport request.
    pub fn transport_set(&self, request: &[u8]) -> Result<u32, i32> {
        let mut generation = 0;
        let rc = call!(self, TRANSPORT_SET, transport_set(request.as_ptr(), request.len(), &mut generation));
        if rc == 0 { Ok(generation) } else { Err(rc) }
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

    /// Keep the packet of this call beyond the call.
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

    /// Does main have a network path?
    pub fn connected(&self) -> bool {
        call!(self, CONNECTED, connected()) == 1
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
        call!(self, GPIO_CONFIG, gpio_config(pin, mode as i32, pull as i32, level))
    }

    pub fn gpio_read(&self, pin: i32) -> i32 {
        call!(self, GPIO_READ, gpio_read(pin))
    }

    pub fn gpio_write(&self, pin: i32, level: i32) -> i32 {
        call!(self, GPIO_WRITE, gpio_write(pin, level))
    }

    /// Report the edges of `pin` as calls `(component, method)` with `value` = the level.
    pub fn irq_register(&self, pin: i32, edge: u32, component: i32, method: i32) -> i32 {
        call!(self, IRQ_REGISTER, irq_register(pin, edge as i32, component, method))
    }

    pub fn irq_unregister(&self, pin: i32) -> i32 {
        call!(self, IRQ_UNREGISTER, irq_unregister(pin))
    }

    pub fn irq_enable(&self, pin: i32, enabled: bool) -> i32 {
        call!(self, IRQ_ENABLE, irq_enable(pin, enabled as i32))
    }

    /// A handle (>= 0) or a negative error.
    pub fn spi_open(&self, config: &SpiConfig) -> i32 {
        call!(self, SPI_OPEN, spi_open(config))
    }

    pub fn spi_transfer(&self, handle: i32, tx: &[u8], rx: &mut [u8]) -> i32 {
        if rx.len() < tx.len() { return ERR_ARG; }
        call!(self, SPI_TRANSFER, spi_transfer(handle, tx.as_ptr(), rx.as_mut_ptr(), tx.len()))
    }

    pub fn spi_close(&self, handle: i32) -> i32 {
        call!(self, SPI_CLOSE, spi_close(handle))
    }

    /// Write `tx`, then read `rx.len()` bytes in one transaction (either may be empty).
    pub fn i2c_transfer(&self, port: i32, sda: i32, scl: i32, hz: u32, address: u8, tx: &[u8], rx: &mut [u8],
                        timeout_ms: u32) -> i32 {
        call!(self, I2C_TRANSFER, i2c_transfer(port, sda, scl, hz, address, tx.as_ptr(), tx.len(),
                                              rx.as_mut_ptr(), rx.len(), timeout_ms))
    }

    /// One-shot reading of `pin`: `(raw, millivolts, adc unit, channel)`.
    pub fn adc_read(&self, pin: i32, ref_mv: u32) -> Result<(i32, u32, i32, i32), i32> {
        let (mut raw, mut mv, mut unit, mut channel) = (0i32, 0u32, 0i32, 0i32);
        let rc = call!(self, ADC_READ, adc_read(pin, ref_mv, &mut raw, &mut mv, &mut unit, &mut channel));
        if rc == 0 { Ok((raw, mv, unit, channel)) } else { Err(rc) }
    }

    pub fn rgbled_write(&self, pin: i32, red: u8, green: u8, blue: u8) -> i32 {
        call!(self, RGBLED_WRITE, rgbled_write(pin, red, green, blue))
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

/// What a handler is called with (the safe view of [`Call`](generated::Call)).
pub struct CallView<'a> {
    pub component: i32,
    pub method: i32,
    pub value: i32,
    pub flags: u32,
    pub data: &'a [u8],
    /// The packet `data` lives in (0: none), lent for this call: `Hw::pkt_keep` it or `Hw::send` it on.
    pub pkt: u32,
    /// Who sent it (`ADDR_MAIN` = 0: the application): answer with `Hw::send` to this address.
    pub from: u32,
    /// The sender's correlation number, returned in the answer.
    pub id: u64,
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
        let has_packet = (c.size as usize) >= core::mem::size_of::<Call>();
        let data = if c.len == 0 || c.data.is_null() { &[][..] } else { core::slice::from_raw_parts(c.data, c.len) };
        let (pkt, from, id) = if has_packet { (c.pkt, c.from, c.id) } else { (0, 0, 0) };
        Some(CallView { component: c.component, method: c.method, value: c.value, flags: c.flags, data, pkt, from, id })
    }

    /// The call is a request that expects [`Hw::respond`].
    pub fn wants_response(&self) -> bool {
        self.flags & 1 != 0
    }

    /// `HOST_*` call of the host itself (start, stop, ...).
    pub fn is_host(&self, method: u32) -> bool {
        self.component == HOST_COMPONENT as i32 && self.method == method as i32
    }
}

/// A module's state and handler. The host keeps the state block (`state_bytes` of the image header), zeroed at
/// the start: **an all-zero `Self` must be a valid initial value** (integers, bools, arrays of them, `Option` of
/// those; no references or enums without a zero variant). [`export_module!`] wires it to `espmod_handle`.
pub trait Module: Sized {
    /// Handle one call. Return [`RET_CONTINUE`], [`RET_DONE`] or a negative error.
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32;
}

/// Export `espmod_handle` for a [`Module`] type; `state = N` asserts at compile time that the type fits the state
/// block that the image header asks for (`mkmod.py --state N`).
#[macro_export]
macro_rules! export_module {
    ($ty:ty, state = $n:expr) => {
        const _: () = assert!(core::mem::size_of::<$ty>() <= $n && core::mem::align_of::<$ty>() <= 8);
        #[no_mangle]
        pub unsafe extern "C" fn espmod_handle(host: *const $crate::Host, state: *mut u8, call: *const $crate::Call) -> i32 {
            let Some(hw) = $crate::Hw::new(host) else { return $crate::ERR_ARG };
            let Some(view) = $crate::CallView::new(call) else { return $crate::ERR_ARG };
            let module = &mut *(state as *mut $ty);
            let saved = $crate::heap::enter(host);
            let rc = <$ty as $crate::Module>::handle(module, &hw, &view);
            $crate::heap::leave(saved);
            rc
        }
    };
}

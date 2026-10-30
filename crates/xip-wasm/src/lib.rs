//! Loads and runs modules compiled to WebAssembly (`modules/WASM.md`) for a runtime that has its own host table.
//!
//! This is not a runtime: the C runtime and `xip-runtime-host` keep their pools, alarms, pins and links. A [`WasmSpace`] is
//! only the place wasm modules live in and the bridge to the runtime: [`WasmSpace::invoke`] calls `xip_handle` of a module
//! with the runtime's [`Host`] table, and the `xip` imports of the module call the functions of that table.
//!
//! **One linear memory.** Every module of a runtime imports the same memory (`--import-memory`), created here with a fixed
//! size, so that its base address never changes. Layout, low to high: page 0 (reserved: 0 is never a valid pointer), the
//! module slots (module N is linked at `N * 64 KiB`, data then stack), then *regions* the runtime asks for with
//! [`WasmSpace::region`]: the packet pool storage, a heap per module for `mem_alloc`, state blocks, and the host's scratch area
//! (the `Call`). A pointer the runtime gives a module (`pkt_buf`, `mem_alloc`) has to lie in a region: the runtime builds its
//! pool and heaps over them. A module sees a pointer as an offset into the memory.
//!
//! The only memory modules share is packets (borrowed or handed over through `send`). Not checked yet: that a module only
//! touches its own slot, heap and the packets it holds.
mod vtable;
pub mod arena;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use wasmi::{AsContextMut, Caller, Engine, Linker, Memory, MemoryType, Module, Store, Table, TypedFunc, Val};
use xip_abi::{Call, Host, Hw, ERR_ARG, ERR_UNSUPPORTED};

pub const PAGE: usize = 65536;
/// Module `i` (1-based) is linked at `i * SLOT_BYTES`.
pub const SLOT_BYTES: usize = PAGE;
pub const SLOTS: usize = 8;
const CALL_BYTES: usize = 88;
const SCRATCH: usize = 256 * 1024;

/// The link base of a module slot, for `--global-base`.
pub fn slot_base(slot: usize) -> usize {
    assert!((1..=SLOTS).contains(&slot));
    slot * SLOT_BYTES
}

/// Memory the runtime builds a pool or a heap over: `ptr` is native, `addr` is what modules see.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub addr: u32,
    pub ptr: *mut u8,
    pub len: usize,
}

struct Ctx {
    /// The host tables of the calls in progress (a call can start another module: the innermost is last).
    hosts: Vec<*const Host>,
    /// The components modules asked for by name: a handle is the position plus one (see `vtable.rs`).
    components: std::cell::RefCell<Vec<&'static str>>,
    base: usize,
    size: usize,
}

impl Ctx {
    fn host(&self) -> Hw {
        // SAFETY: the table lives as long as the call that pushed it
        unsafe { Hw::new(*self.hosts.last().expect("a wasm module runs inside invoke")).expect("host table") }
    }
    /// A native pointer to `len` bytes at `addr`, or None when they are not inside the memory.
    fn ptr(&self, addr: u32, len: usize) -> Option<*mut u8> {
        let a = addr as usize;
        (a != 0 && a.checked_add(len)? <= self.size).then_some((self.base + a) as *mut u8)
    }
    /// The address of a native pointer into the memory (0 for null or outside).
    fn addr(&self, ptr: *const u8) -> u32 {
        let p = ptr as usize;
        if p > self.base && p < self.base + self.size { (p - self.base) as u32 } else { 0 }
    }

    /// A readable native pointer to `len` bytes at `addr` (null for an empty request).
    fn rd(&self, addr: u32, len: u32) -> Option<*const u8> {
        self.ptr_or_null(addr, len).map(|p| p as *const u8)
    }
    fn ptr_or_null(&self, addr: u32, len: u32) -> Option<*mut u8> {
        if addr == 0 && len == 0 { Some(std::ptr::null_mut()) } else { self.ptr(addr, len as usize) }
    }
    fn slice(&self, addr: u32, len: usize) -> Option<&'static [u8]> {
        // SAFETY: inside the memory, which lives as long as the space
        self.ptr(addr, len).map(|p| unsafe { std::slice::from_raw_parts(p, len) })
    }
    fn slice_mut(&self, addr: u32, len: usize) -> Option<&'static mut [u8]> {
        self.ptr(addr, len).map(|p| unsafe { std::slice::from_raw_parts_mut(p, len) })
    }
    fn put32(&self, addr: u32, v: u32) -> bool {
        self.slice_mut(addr, 4).map(|s| s.copy_from_slice(&v.to_le_bytes())).is_some()
    }
}


/// A call of the host table in progress.
fn tfn(c: &Ctx, f: impl FnOnce(&Host) -> Option<i32>) -> i32 {
    // SAFETY: the table lives as long as the call that pushed it
    let t = unsafe { &*c.host().as_ptr() };
    f(t).unwrap_or(ERR_UNSUPPORTED)
}

/// A table function that writes `len` bytes at `out`.
fn tcall(c: &Ctx, out: u32, len: u32, f: impl FnOnce(&Host, *mut u8) -> Option<i32>) -> i32 {
    let Some(p) = c.ptr_or_null(out, len) else { return ERR_ARG };
    tfn(c, |t| f(t, p))
}

/// A function of the `hw` component (`ERR_UNSUPPORTED` when the host has none).
fn hcall(c: &Ctx, f: impl FnOnce(&xip_abi::Hardware, *mut c_void) -> Option<i32>) -> i32 {
    let hw = c.host();
    match hw.hardware() {
        Some(h) => f(h, hw.ctx()).unwrap_or(ERR_UNSUPPORTED),
        None => ERR_UNSUPPORTED,
    }
}

thread_local! {
    /// The `Caller` of the import being served, so that a call a table function makes to another module runs inside the same store.
    static ACTIVE: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
}

/// Run `f` with `c` as the active caller (restored afterwards).
fn active<R>(c: &mut Caller<'_, Ctx>, f: impl FnOnce(&Ctx) -> R) -> R {
    let prev = ACTIVE.with(|a| a.replace(c as *mut Caller<'_, Ctx> as *mut c_void));
    let r = f(c.data());
    ACTIVE.with(|a| a.set(prev));
    r
}

struct Mod {
    handle: TypedFunc<(u32, u32, u32), i32>,
    /// The function table (`__indirect_function_table`, linked with `--export-table`): a function pointer of a module is an index in it.
    table: Option<Table>,
}

/// An argument of [`WasmSpace::call_table`] after the context.
pub enum TableArg<'a> {
    /// A number, passed as the function's i32 or i64 parameter.
    Val(u64),
    /// Bytes copied into the memory before the call; the function gets their address.
    In(&'a [u8]),
    /// Bytes copied in before and back out after the call (a response struct).
    Out(*mut u8, usize),
}

pub struct WasmSpace {
    engine: Engine,
    linker: Linker<Ctx>,
    store: RefCell<Store<Ctx>>,
    memory: Memory,
    base: usize,
    size: usize,
    next: Cell<usize>,
    scratch: Region,
    scratch_next: Cell<usize>,
    /// The instance of each slot (index 1..=SLOTS).
    modules: RefCell<Vec<Option<Mod>>>,
}

macro_rules! imp {
    ($linker:ident, $name:literal, |$c:ident $(, $a:ident : $t:ty)*| -> $r:ty $body:block) => {
        $linker.func_wrap("xip", $name, |mut caller: Caller<'_, Ctx> $(, $a: $t)*| -> $r {
            active(&mut caller, |$c| $body)
        }).map_err(|e| e.to_string())?;
    };
}

impl WasmSpace {
    /// A space of `bytes` (the slots, the scratch area and every region the runtime will ask for).
    pub fn new(bytes: usize) -> Result<WasmSpace, String> {
        let first = (SLOTS + 1) * SLOT_BYTES;
        let total = (first + SCRATCH + bytes).next_multiple_of(PAGE);
        let engine = Engine::default();
        let pages = (total / PAGE) as u32;
        let mut store = Store::new(&engine, Ctx { hosts: Vec::new(), components: Default::default(), base: 0, size: total });
        let memory = Memory::new(&mut store, MemoryType::new(pages, Some(pages))).map_err(|e| e.to_string())?;
        let base = memory.data_mut(&mut store).as_mut_ptr() as usize;
        store.data_mut().base = base;
        let mut linker = <Linker<Ctx>>::new(&engine);
        linker.define("env", "memory", memory).map_err(|e| e.to_string())?;

        imp!(linker, "log", |c, ptr: u32, len: u32| -> i32 { c.slice(ptr, len as usize).map_or(ERR_ARG, |s| c.host().log(s)) });
        imp!(linker, "note_unsupported", |c, name: u32, len: u32| -> () {
            if let (Some(n), true) = (c.slice(name, len as usize), std::env::var_os("XIP_WASM_TRACE").is_some()) {
                eprintln!("wasm: unsupported host call {}", String::from_utf8_lossy(n));
            }
        });
        imp!(linker, "now_ms", |c| -> u64 { c.host().now_ms() });
        imp!(linker, "mem_alloc", |c, size: u32| -> u32 { c.addr(c.host().mem_alloc(size as usize)) });
        imp!(linker, "mem_free", |c, ptr: u32| -> i32 {
            match c.ptr(ptr, 0) {
                Some(p) => c.host().mem_free(p),
                None => ERR_ARG,
            }
        });
        imp!(linker, "pkt_info", |c, headroom: u32, max: u32| -> i32 {
            match c.host().pkt_info() {
                Ok((h, m)) => if c.put32(headroom, h) && c.put32(max, m) { 0 } else { ERR_ARG },
                Err(e) => e,
            }
        });
        imp!(linker, "pkt_alloc", |c, out: u32| -> i32 {
            match c.host().pkt_alloc() {
                Ok(h) => if c.put32(out, h) { 0 } else { ERR_ARG },
                Err(e) => { c.put32(out, 0); e }
            }
        });
        imp!(linker, "pkt_buf", |c, pkt: u32, ptr: u32, max: u32| -> i32 {
            match c.host().pkt_buf(pkt) {
                Ok((p, m)) => {
                    let a = c.addr(p);
                    if a == 0 { return ERR_ARG; }
                    if c.put32(ptr, a) && c.put32(max, m as u32) { 0 } else { ERR_ARG }
                }
                Err(e) => e,
            }
        });
        imp!(linker, "pkt_keep", |c, pkt: u32| -> i32 { c.host().pkt_keep(pkt) });
        imp!(linker, "pkt_free", |c, pkt: u32| -> i32 { c.host().pkt_free(pkt) });
        imp!(linker, "send", |c, pkt: u32, len: u32, dst: u32, component: i32, method: i32, id: u64| -> i32 {
            c.host().send(pkt, len as usize, dst, component, method, id)
        });
        imp!(linker, "get_setting", |c, k: u32, kn: u32, v: u32, cap: u32, vl: u32| -> i32 {
            let (Some(key), Some(value)) = (c.slice(k, kn as usize), c.slice_mut(v, cap as usize).or((cap == 0).then_some(&mut [][..]))) else { return ERR_ARG };
            match c.host().get_setting(key, value) {
                Ok(n) => if c.put32(vl, n as u32) { 0 } else { ERR_ARG },
                Err(e) => e,
            }
        });
        imp!(linker, "set_setting", |c, k: u32, kn: u32, v: u32, vn: u32| -> i32 {
            let (Some(key), Some(value)) = (c.slice(k, kn as usize), if vn == 0 { Some(&[][..]) } else { c.slice(v, vn as usize) }) else { return ERR_ARG };
            c.host().set_setting(key, value)
        });
        imp!(linker, "alarm_set", |c, delay: u32, window: u32, interval: u32, flags: u32, component: i32, method: i32| -> i32 {
            c.host().alarm_set(delay, window, interval, flags, component, method)
        });
        imp!(linker, "alarm_cancel", |c, i: i32| -> i32 { c.host().alarm_cancel(i) });
        imp!(linker, "gpio_config", |c, pin: i32, mode: i32, pull: i32, level: i32| -> i32 { c.host().gpio_config(pin, mode as u32, pull as u32, level) });
        imp!(linker, "gpio_read", |c, pin: i32| -> i32 { c.host().gpio_read(pin) });
        imp!(linker, "gpio_write", |c, pin: i32, level: i32| -> i32 { c.host().gpio_write(pin, level) });
        imp!(linker, "irq_register", |c, pin: i32, edge: i32, component: i32, method: i32| -> i32 { c.host().irq_register(pin, edge as u32, component, method) });
        imp!(linker, "irq_unregister", |c, pin: i32| -> i32 { c.host().irq_unregister(pin) });
        imp!(linker, "irq_enable", |c, pin: i32, enabled: i32| -> i32 { c.host().irq_enable(pin, enabled != 0) });


        vtable::register(&mut linker)?;
        // ---- the rest of the host table, argument by argument; a pointer has to lie in the memory ----
        imp!(linker, "device_info", |c, out: u32, cap: u32| -> i32 { tcall(c, out, cap, |t, p| Some(unsafe { (t.device_info?)(t.user, p, cap as usize) })) });
        imp!(linker, "info_publish", |c, component: i32, method: i32, data: u32, len: u32| -> i32 {
            let Some(d) = c.rd(data, len) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.info_publish?)(t.user, component, method, d, len as usize) }))
        });
        imp!(linker, "pkt_span", |c, pkt: u32, base: u32, cap: u32| -> i32 {
            let (mut b, mut n) = (std::ptr::null_mut(), 0usize);
            let rc = tfn(c, |t| Some(unsafe { (t.pkt_span?)(t.user, pkt, &mut b, &mut n) }));
            if rc != 0 { return rc; }
            let a = c.addr(b);
            if a == 0 || !c.put32(base, a) || !c.put32(cap, n as u32) { ERR_ARG } else { 0 }
        });
        imp!(linker, "socket_read", |c, socket: u32, out: u32, len: u32| -> i32 { tcall(c, out, len, |t, p| Some(unsafe { (t.socket_read?)(t.user, socket, p, len as usize) })) });
        imp!(linker, "socket_write", |c, socket: u32, data: u32, len: u32| -> i32 {
            let Some(d) = c.rd(data, len) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.socket_write?)(t.user, socket, d, len as usize) }))
        });
        imp!(linker, "socket_close", |c, socket: u32| -> i32 { tfn(c, |t| Some(unsafe { (t.socket_close?)(t.user, socket) })) });
        imp!(linker, "socket_peer", |c, socket: u32, address: u32, port: u32, scope: u32| -> i32 {
            let (mut a, mut p, mut s) = ([0u8; 16], 0u16, 0u32);
            let rc = tfn(c, |t| Some(unsafe { (t.socket_peer?)(t.user, socket, a.as_mut_ptr(), &mut p, &mut s) }));
            if rc < 0 { return rc; }
            let ok = c.slice_mut(address, 16).map(|d| d.copy_from_slice(&a)).is_some()
                && c.slice_mut(port, 2).map(|d| d.copy_from_slice(&p.to_le_bytes())).is_some()
                && c.put32(scope, s);
            if ok { rc } else { ERR_ARG }
        });
        imp!(linker, "udp_send", |c, handle: u32, ip: u32, port: u32, scope: u32, data: u32, len: u32| -> i32 {
            let (Some(ipp), Some(d)) = (c.rd(ip, 16), c.rd(data, len)) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.udp_send?)(t.user, handle, ipp, port as u16, scope, d, len as usize) }))
        });
        imp!(linker, "socket_write_pkt", |c, socket: u32, pkt: u32, start: u32, len: u32| -> i32 { tfn(c, |t| Some(unsafe { (t.socket_write_pkt?)(t.user, socket, pkt, start, len) })) });
        imp!(linker, "modules_info", |c, out: u32, cap: u32| -> i32 { tcall(c, out, cap, |t, p| Some(unsafe { (t.modules_info?)(t.user, p, cap as usize) })) });
        imp!(linker, "module_start", |c, name: u32, nl: u32, req: u32, rl: u32| -> i32 {
            let (Some(n), Some(r)) = (c.rd(name, nl), c.rd(req, rl)) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.module_start?)(t.user, n, nl as usize, r, rl as usize) }))
        });
        imp!(linker, "module_stop", |c, name: u32, nl: u32| -> i32 {
            let Some(n) = c.rd(name, nl) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.module_stop?)(t.user, n, nl as usize) }))
        });
        imp!(linker, "module_manifest", |c, name: u32, nl: u32, out: u32, cap: u32| -> i32 {
            let (Some(n), Some(o)) = (c.rd(name, nl), c.ptr_or_null(out, cap)) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.module_manifest?)(t.user, n, nl as usize, o, cap as usize) }))
        });
        imp!(linker, "module_rescan", |c| -> i32 { tfn(c, |t| Some(unsafe { (t.module_rescan?)(t.user) })) });
        imp!(linker, "reset", |c| -> i32 { tfn(c, |t| Some(unsafe { (t.reset?)(t.user) })) });
        imp!(linker, "heap_info", |c, out: u32, cap: u32| -> i32 { tcall(c, out, cap, |t, p| Some(unsafe { (t.heap_info?)(t.user, p, cap as usize) })) });
        imp!(linker, "heap_dump", |c, kind: i32| -> i32 { tfn(c, |t| Some(unsafe { (t.heap_dump?)(t.user, kind) })) });
        imp!(linker, "nv", |c, key: u32, kl: u32, val: u32, vl: u32, out: u32, cap: u32| -> i32 {
            let (Some(k), Some(v), Some(o)) = (c.rd(key, kl), c.ptr_or_null(val, vl), c.ptr_or_null(out, cap)) else { return ERR_ARG };
            tfn(c, |t| Some(unsafe { (t.nv?)(t.user, k, kl as usize, v, vl as usize, o, cap as usize) }))
        });
        imp!(linker, "flash_erase", |c, address: u32, offset: u32, len: u32, component: i32, method: i32, id: u64| -> i32 {
            tfn(c, |t| Some(unsafe { (t.flash_erase?)(t.user, address, offset, len, component, method, id) }))
        });
        imp!(linker, "flash_write", |c, address: u32, offset: u32, pkt: u32, start: u32, len: u32, component: i32, method: i32, id: u64| -> i32 {
            tfn(c, |t| Some(unsafe { (t.flash_write?)(t.user, address, offset, pkt, start, len, component, method, id) }))
        });
        imp!(linker, "flash_read", |c, address: u32, offset: u32, out: u32, len: u32| -> i32 { tcall(c, out, len, |t, p| Some(unsafe { (t.flash_read?)(t.user, address, offset, p, len as usize) })) });
        imp!(linker, "spi_open", |c, config: u32| -> i32 {
            let n = std::mem::size_of::<xip_abi::SpiConfig>() as u32;
            let Some(p) = c.rd(config, n) else { return ERR_ARG };
            let cfg = unsafe { std::ptr::read_unaligned(p as *const xip_abi::SpiConfig) };
            hcall(c, |h, ctx| Some(unsafe { (h.spi_open?)(ctx, &cfg) }))
        });
        imp!(linker, "spi_transfer", |c, handle: i32, tx: u32, rx: u32, len: u32| -> i32 {
            let (Some(t), Some(r)) = (c.rd(tx, len), c.ptr_or_null(rx, len)) else { return ERR_ARG };
            hcall(c, |h, ctx| Some(unsafe { (h.spi_transfer?)(ctx, handle, t, r, len as usize) }))
        });
        imp!(linker, "spi_close", |c, handle: i32| -> i32 { hcall(c, |h, ctx| Some(unsafe { (h.spi_close?)(ctx, handle) })) });
        imp!(linker, "i2c_transfer", |c, port: i32, sda: i32, scl: i32, hz: u32, address: i32, tx: u32, txl: u32, rx: u32, rxl: u32, timeout: u32| -> i32 {
            let (Some(t), Some(r)) = (c.rd(tx, txl), c.ptr_or_null(rx, rxl)) else { return ERR_ARG };
            hcall(c, |h, ctx| Some(unsafe { (h.i2c_transfer?)(ctx, port, sda, scl, hz, address as u8, t, txl as usize, r, rxl as usize, timeout) }))
        });
        imp!(linker, "adc_read", |c, pin: i32, ref_mv: u32, raw: u32, mv: u32, unit: u32, channel: u32| -> i32 {
            let (mut r, mut m, mut u, mut ch) = (0i32, 0u32, 0i32, 0i32);
            let rc = hcall(c, |h, ctx| Some(unsafe { (h.adc_read?)(ctx, pin, ref_mv, &mut r, &mut m, &mut u, &mut ch) }));
            if rc == 0 && !(c.put32(raw, r as u32) && c.put32(mv, m) && c.put32(unit, u as u32) && c.put32(channel, ch as u32)) { return ERR_ARG; }
            rc
        });
        imp!(linker, "rgbled_write", |c, pin: i32, r: i32, g: i32, b: i32| -> i32 { hcall(c, |h, ctx| Some(unsafe { (h.rgbled_write?)(ctx, pin, r as u8, g as u8, b as u8) })) });

        let space = WasmSpace {
            engine,
            linker,
            store: RefCell::new(store),
            memory,
            base,
            size: total,
            next: Cell::new(first),
            scratch: Region { addr: first as u32, ptr: (base + first) as *mut u8, len: SCRATCH },
            scratch_next: Cell::new(0),
            modules: RefCell::new((0..=SLOTS).map(|_| None).collect()),
        };
        space.next.set(first + SCRATCH);
        Ok(space)
    }

    /// Take `len` bytes (rounded up to 16) of the memory for the runtime's own use: a pool, a heap, a state block. Never given back.
    pub fn region(&self, len: usize) -> Result<Region, String> {
        let len = len.max(1).next_multiple_of(16);
        let at = self.next.get();
        if at + len > self.size {
            return Err(format!("wasm space is full ({} bytes asked, {} left)", len, self.size - at));
        }
        self.next.set(at + len);
        Ok(Region { addr: at as u32, ptr: (self.base + at) as *mut u8, len })
    }

    /// Memory regions are used for.
    pub fn size(&self) -> usize {
        self.size
    }

    /// A native pointer to the address `addr` of the memory.
    pub fn native(&self, addr: u32) -> *mut u8 {
        (self.base + addr as usize) as *mut u8
    }

    /// The memory from `addr` to its end: where a module's static data (a schema it carries) is read from.
    pub fn from(&self, addr: u32) -> &[u8] {
        let a = (addr as usize).min(self.size);
        // SAFETY: base..base+size is the memory, which never moves
        unsafe { std::slice::from_raw_parts((self.base + a) as *const u8, self.size - a) }
    }

    /// Whether `ptr` lies in the memory (a pointer a module can be given).
    pub fn contains(&self, ptr: *const u8) -> bool {
        let p = ptr as usize;
        p > self.base && p < self.base + self.size
    }

    /// Instantiate `bytes`, a module linked for `slot` (see [`slot_base`]), replacing what the slot held: its memory is cleared first, so
    /// a module that is loaded again starts like a new one.
    pub fn load(&self, slot: usize, bytes: &[u8]) -> Result<(), String> {
        assert!((1..=SLOTS).contains(&slot));
        let module = Module::new(&self.engine, bytes).map_err(|e| e.to_string())?;
        // SAFETY: the slot is inside the memory
        unsafe { std::ptr::write_bytes((self.base + slot_base(slot)) as *mut u8, 0, SLOT_BYTES) };
        let mut store = self.store.try_borrow_mut().map_err(|_| "cannot load a module inside a call".to_string())?;
        let instance = self.linker.instantiate_and_start(&mut *store, &module).map_err(|e| e.to_string())?;
        let handle = instance.get_typed_func(&*store, "xip_handle").map_err(|e| format!("xip_handle: {e}"))?;
        let table = instance.get_export(&*store, "__indirect_function_table").and_then(|e| e.into_table());
        self.modules.borrow_mut()[slot] = Some(Mod { handle, table });
        Ok(())
    }

    /// Call function `index` of the function table of the module in `slot`, a function a module published in a table it hosts (`ctx`
    /// first, then `args`; the function gets as many of them as it has parameters). `In`/`Out` bytes are copied through the scratch area,
    /// so the caller needs no pointer into the memory. Like [`WasmSpace::invoke`] it can run inside a host function.
    pub fn call_table(&self, slot: usize, host: *const Host, index: u32, ctx: u32, args: &[TableArg]) -> Result<i32, String> {
        let table = self.modules.borrow().get(slot).and_then(|m| m.as_ref()).and_then(|m| m.table).ok_or("no function table in that slot")?;
        let mark = self.scratch_next.get();
        let mut vals = vec![u64::from(ctx)];
        let mut outs = Vec::new();
        for a in args {
            match a {
                TableArg::Val(v) => vals.push(*v),
                TableArg::In(b) => {
                    let r = self.scratch(b.len())?;
                    // SAFETY: scratch memory of the space
                    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), r.ptr, b.len()) };
                    vals.push(u64::from(r.addr));
                }
                TableArg::Out(p, n) => {
                    let r = self.scratch(*n)?;
                    // SAFETY: the caller promises `n` bytes at `p`
                    unsafe { std::ptr::copy_nonoverlapping(*p, r.ptr, *n) };
                    outs.push((*p, r, *n));
                    vals.push(u64::from(r.addr));
                }
            }
        }
        let outer = ACTIVE.with(|a| a.get());
        let rc = if outer.is_null() {
            let mut store = self.store.try_borrow_mut().map_err(|_| "store busy".to_string())?;
            store.data_mut().hosts.push(host);
            let rc = run_table(&mut *store, table, index, &vals);
            store.data_mut().hosts.pop();
            rc
        } else {
            // SAFETY: as in `invoke`
            let caller = unsafe { &mut *(outer as *mut Caller<'static, Ctx>) };
            caller.data_mut().hosts.push(host);
            let rc = run_table(&mut *caller, table, index, &vals);
            caller.data_mut().hosts.pop();
            rc
        };
        for (p, r, n) in outs {
            // SAFETY: as above; `r.len` is rounded up, the caller's buffer holds `n`
            unsafe { std::ptr::copy_nonoverlapping(r.ptr, p, n) };
        }
        self.scratch_next.set(mark);
        rc
    }

    /// `xip_handle(host, state, call)` of the module in `slot`. `state` must lie in a region; `call.data` is copied into the memory unless it
    /// already lies in it (a pool packet). May be called from inside a host function the module is calling (the call then runs in the
    /// same store).
    pub fn invoke(&self, slot: usize, host: *const Host, state: *mut u8, call: &Call) -> Result<i32, String> {
        let handle = self.modules.borrow().get(slot).and_then(|m| m.as_ref()).ok_or("no module in that slot")?.handle;
        let mark = self.scratch_next.get();
        let c = self.marshal(call)?;
        let state_addr = (state as usize).checked_sub(self.base).filter(|a| *a < self.size).ok_or("state block outside the wasm memory")? as u32;
        let args = (1, state_addr, c);
        let outer = ACTIVE.with(|a| a.get());
        let rc = if outer.is_null() {
            let mut store = self.store.try_borrow_mut().map_err(|_| "store busy".to_string())?;
            store.data_mut().hosts.push(host);
            let rc = handle.call(&mut *store, args);
            store.data_mut().hosts.pop();
            rc
        } else {
            // SAFETY: the caller of the import being served is alive until it returns, which is after this call
            let caller = unsafe { &mut *(outer as *mut Caller<'static, Ctx>) };
            caller.data_mut().hosts.push(host);
            let rc = handle.call(&mut *caller, args);
            caller.data_mut().hosts.pop();
            rc
        };
        self.scratch_next.set(mark);
        rc.map_err(|e| e.to_string())
    }

    fn scratch(&self, len: usize) -> Result<Region, String> {
        let len = len.max(1).next_multiple_of(16);
        let at = self.scratch_next.get();
        if at + len > self.scratch.len {
            return Err("call data does not fit the scratch area".into());
        }
        self.scratch_next.set(at + len);
        Ok(Region { addr: self.scratch.addr + at as u32, ptr: unsafe { self.scratch.ptr.add(at) }, len })
    }

    /// The `Call` in wasm32 layout (88 bytes, `include/xip.h` with 4-byte pointers).
    fn marshal(&self, call: &Call) -> Result<u32, String> {
        let data = if call.data.is_null() || call.len == 0 {
            0
        } else if self.contains(call.data) {
            (call.data as usize - self.base) as u32
        } else {
            let r = self.scratch(call.len)?;
            // SAFETY: the host promises `len` readable bytes at `data` for the call
            unsafe { std::ptr::copy_nonoverlapping(call.data, r.ptr, call.len) };
            r.addr
        };
        let r = self.scratch(CALL_BYTES)?;
        let mut b = [0u8; CALL_BYTES];
        let mut put = |at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
        put(0, &(CALL_BYTES as u32).to_le_bytes());
        put(4, &call.component.to_le_bytes());
        put(8, &call.method.to_le_bytes());
        put(12, &call.value.to_le_bytes());
        put(16, &call.flags.to_le_bytes());
        put(20, &data.to_le_bytes());
        put(24, &(call.len as u32).to_le_bytes());
        put(28, &call.pkt.to_le_bytes());
        put(32, &call.from.to_le_bytes());
        put(36, &call.time_us.to_le_bytes());
        put(40, &call.id.to_le_bytes());
        put(48, &call.request_id.to_le_bytes());
        put(56, &call.request_has_id.to_le_bytes());
        put(60, &call.request_reserved.to_le_bytes());
        put(64, &call.src_ip);
        put(80, &call.src_port.to_le_bytes());
        put(84, &call.src_scope.to_le_bytes());
        // SAFETY: `r` is scratch memory of the space
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), r.ptr, CALL_BYTES) };
        Ok(r.addr)
    }
}

/// Call function `index` of `table` with `vals` as its parameters (as many as it has).
fn run_table<C: AsContextMut<Data = Ctx>>(cx: &mut C, table: Table, index: u32, vals: &[u64]) -> Result<i32, String> {
    let func = match table.get(&*cx, u64::from(index)) {
        Some(r) => r.as_func().and_then(|f| f.val().map(|x| **x)).ok_or("empty function table entry")?,
        None => return Err("no such function table entry".into()),
    };
    let ty = func.ty(&*cx);
    let params: Vec<Val> = ty
        .params()
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let v = vals.get(i).copied().unwrap_or(0);
            if matches!(t, wasmi::ValType::I64) { Val::I64(v as i64) } else { Val::I32(v as i32) }
        })
        .collect();
    let mut res = [Val::I32(0)];
    func.call(&mut *cx, &params, &mut res).map_err(|e| e.to_string())?;
    Ok(match res[0] { Val::I32(v) => v, _ => ERR_UNSUPPORTED })
}

#[allow(dead_code)]
const _UNUSED: i32 = ERR_UNSUPPORTED;

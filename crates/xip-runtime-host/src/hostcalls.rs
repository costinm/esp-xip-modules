//! The host table: every call of the host table (`include/xip.h`) the host-runtime implements. `user` is the `Ctx` of the calling instance.
use crate::fixtures::Loopback;
use crate::pool::{HELD_BORROWED, HELD_OWNED};
use crate::runtime::*;
use std::ffi::{c_char, c_void, CStr};
use std::rc::Rc;
use xip_abi::cbor::Writer;
use xip_abi::*;

pub(crate) unsafe fn ctx<'a>(user: *mut c_void) -> (&'a Shared, Rc<Inst>) {
    let c = &*(user as *const Ctx);
    let sh = &*c.sh;
    let me = sh.insts.borrow()[c.id].clone();
    (sh, me)
}

macro_rules! cx {
    ($user:expr, $name:literal) => {{
        let (sh, me) = unsafe { ctx($user) };
        trace_host(sh, &me, $name);
        (sh, me)
    }};
}

unsafe fn sl<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if n == 0 || p.is_null() { &[] } else { std::slice::from_raw_parts(p, n) }
}

/// Copy `data` to the caller's buffer, or `ERR_SIZE`.
unsafe fn put(out: *mut u8, cap: usize, data: &[u8]) -> i32 {
    if data.len() > cap {
        return ERR_SIZE;
    }
    if !data.is_empty() {
        std::ptr::copy_nonoverlapping(data.as_ptr(), out, data.len());
    }
    data.len() as i32
}

fn cbor(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut buf = vec![0u8; 1024];
    let mut w = Writer::new(&mut buf);
    f(&mut w);
    let n = w.finish().map_or(0, |s| s.len());
    buf.truncate(n);
    buf
}

unsafe extern "C" fn h_log(user: *mut c_void, data: *const u8, len: usize) -> i32 {
    let (_sh, me) = cx!(user, "log");
    if data.is_null() {
        return ERR_ARG;
    }
    let mut st = me.st.borrow_mut();
    if st.logs.len() < 64 {
        let n = len.min(99);
        st.logs.push(String::from_utf8_lossy(sl(data, n)).into_owned());
    }
    if len < 97 { 0 } else { ERR_SIZE }
}

unsafe extern "C" fn h_now(user: *mut c_void) -> u64 {
    let (sh, _me) = ctx(user);
    sh.st.borrow().now
}

unsafe extern "C" fn h_get_setting(user: *mut c_void, k: *const u8, kn: usize, v: *mut u8, cap: usize, vl: *mut usize) -> i32 {
    let (_sh, me) = cx!(user, "get_setting");
    if k.is_null() || kn == 0 || kn > 15 || vl.is_null() || (cap > 0 && v.is_null()) {
        return ERR_ARG;
    }
    let st = me.st.borrow();
    let Some(val) = st.settings.get(sl(k, kn)) else { return ERR_NOTFOUND };
    *vl = val.len();
    if cap < val.len() {
        return ERR_SIZE;
    }
    std::ptr::copy_nonoverlapping(val.as_ptr(), v, val.len());
    0
}

unsafe extern "C" fn h_set_setting(user: *mut c_void, k: *const u8, kn: usize, v: *const u8, vn: usize) -> i32 {
    let (_sh, me) = cx!(user, "set_setting");
    if k.is_null() || kn == 0 || kn > 15 || (vn > 0 && v.is_null()) || vn > 256 {
        return ERR_ARG;
    }
    let mut st = me.st.borrow_mut();
    let key = sl(k, kn).to_vec();
    if vn == 0 {
        st.settings.remove(&key);
        return 0;
    }
    if !st.settings.contains_key(&key) && st.settings.len() >= 8 {
        return ERR_NOMEM;
    }
    st.settings.insert(key, sl(v, vn).to_vec());
    0
}

/// Pins are shared by the whole runtime and have one owner at a time.
fn claim(sh: &Shared, me: &Inst, pin: i32) -> i32 {
    if pin < 0 || pin as usize >= PINS {
        return ERR_ARG;
    }
    let mut s = sh.st.borrow_mut();
    let p = &mut s.pins[pin as usize];
    match p.owner {
        Some(o) if o != me.id => ERR_BUSY,
        _ => {
            p.owner = Some(me.id);
            0
        }
    }
}
fn owned(sh: &Shared, me: &Inst, pin: i32) -> bool {
    pin >= 0 && (pin as usize) < PINS && sh.st.borrow().pins[pin as usize].owner == Some(me.id)
}

unsafe extern "C" fn h_gpio_config(user: *mut c_void, pin: i32, mode: i32, pull: i32, level: i32) -> i32 {
    let (sh, me) = cx!(user, "gpio_config");
    let rc = claim(sh, &me, pin);
    if rc != 0 {
        return rc;
    }
    let wire = {
        let mut s = sh.st.borrow_mut();
        let p = &mut s.pins[pin as usize];
        if mode != GPIO_INPUT as i32 {
            p.level = level != 0;
            p.wire.map(|w| (w, level != 0))
        } else {
            if pull == GPIO_PULL_DOWN as i32 {
                p.level = false;
            }
            None
        }
    };
    if let Some((w, l)) = wire {
        drive(sh, w, l);
    }
    0
}

unsafe extern "C" fn h_gpio_read(user: *mut c_void, pin: i32) -> i32 {
    let (sh, me) = cx!(user, "gpio_read");
    if owned(sh, &me, pin) { sh.st.borrow().pins[pin as usize].level as i32 } else { ERR_ARG }
}

unsafe extern "C" fn h_gpio_write(user: *mut c_void, pin: i32, level: i32) -> i32 {
    let (sh, me) = cx!(user, "gpio_write");
    if !owned(sh, &me, pin) {
        return ERR_ARG;
    }
    let wire = {
        let mut s = sh.st.borrow_mut();
        let p = &mut s.pins[pin as usize];
        p.level = level != 0;
        p.wire
    };
    if let Some(w) = wire {
        drive(sh, w, level != 0);
    }
    0
}

unsafe extern "C" fn h_irq_register(user: *mut c_void, pin: i32, edge: i32, component: i32, method: i32) -> i32 {
    let (sh, me) = cx!(user, "irq_register");
    let rc = claim(sh, &me, pin);
    if rc != 0 {
        return rc;
    }
    let mut s = sh.st.borrow_mut();
    let p = &mut s.pins[pin as usize];
    if p.irq.is_some() {
        return ERR_BUSY;
    }
    p.irq = Some(Irq { edge, component, method, masked: false });
    0
}

unsafe extern "C" fn h_irq_unregister(user: *mut c_void, pin: i32) -> i32 {
    let (sh, me) = cx!(user, "irq_unregister");
    if !owned(sh, &me, pin) || sh.st.borrow().pins[pin as usize].irq.is_none() {
        return ERR_NOTFOUND;
    }
    sh.st.borrow_mut().pins[pin as usize].irq = None;
    0
}

unsafe extern "C" fn h_irq_enable(user: *mut c_void, pin: i32, enabled: i32) -> i32 {
    let (sh, me) = cx!(user, "irq_enable");
    if !owned(sh, &me, pin) {
        return ERR_NOTFOUND;
    }
    match sh.st.borrow_mut().pins[pin as usize].irq.as_mut() {
        Some(i) => {
            i.masked = enabled == 0;
            0
        }
        None => ERR_NOTFOUND,
    }
}

unsafe extern "C" fn h_alarm_set(user: *mut c_void, delay: u32, window: u32, interval: u32, flags: u32, component: i32, method: i32) -> i32 {
    let (sh, me) = cx!(user, "alarm_set");
    if flags & !(ALARM_FLEXIBLE | ALARM_CONNECTED | ALARM_WINDOW) != 0 {
        return ERR_ARG;
    }
    let (now, fire) = {
        let s = sh.st.borrow();
        (s.now, s.now + fire_delay(&s, delay, window, flags) as u64)
    };
    let mut st = me.st.borrow_mut();
    for (i, a) in st.alarms.iter_mut().enumerate() {
        if !a.used {
            *a = Alarm { seq: a.seq.wrapping_add(1), used: true, waiting_net: false, due: now + delay as u64, fire, interval, window, flags, component, method };
            return i as i32;
        }
    }
    ERR_NOMEM
}

unsafe extern "C" fn h_alarm_cancel(user: *mut c_void, i: i32) -> i32 {
    let (_sh, me) = cx!(user, "alarm_cancel");
    let mut st = me.st.borrow_mut();
    match st.alarms.get_mut(i as usize) {
        Some(a) if i >= 0 && a.used => {
            a.used = false;
            0
        }
        _ => ERR_NOTFOUND,
    }
}

unsafe extern "C" fn h_spi_open(user: *mut c_void, c: *const SpiConfig) -> i32 {
    let (_sh, me) = cx!(user, "spi_open");
    if c.is_null() {
        return ERR_ARG;
    }
    let c = &*c;
    if (c.size as usize) < std::mem::size_of::<SpiConfig>() || !(0..=3).contains(&c.mode) || c.sck < 0 || c.cs < 0 {
        return ERR_ARG;
    }
    let mut st = me.st.borrow_mut();
    for i in 0..4 {
        if st.spi_open[i].is_none() {
            st.spi_open[i] = Some(c.cs);
            return i as i32;
        }
    }
    ERR_NOMEM
}

unsafe extern "C" fn h_spi_transfer(user: *mut c_void, h: i32, tx: *const u8, rx: *mut u8, len: usize) -> i32 {
    let (sh, me) = cx!(user, "spi_transfer");
    let cs = match (0..4).contains(&h).then(|| me.st.borrow().spi_open[h as usize]).flatten() {
        Some(cs) => cs,
        None => return ERR_ARG,
    };
    if tx.is_null() || len == 0 || len > 272 {
        return ERR_ARG;
    }
    me.st.borrow_mut().spi_transfers += 1;
    let mut buf = vec![0u8; len];
    let mut s = sh.st.borrow_mut();
    let now = s.now;
    let dev = s.spi.entry(cs).or_insert_with(|| Box::new(Loopback));
    match dev.transfer(now, sl(tx, len), &mut buf) {
        Ok(()) => {
            if !rx.is_null() {
                std::ptr::copy_nonoverlapping(buf.as_ptr(), rx, len);
            }
            0
        }
        Err(e) => e,
    }
}

unsafe extern "C" fn h_spi_close(user: *mut c_void, h: i32) -> i32 {
    let (_sh, me) = cx!(user, "spi_close");
    let mut st = me.st.borrow_mut();
    if !(0..4).contains(&h) || st.spi_open[h as usize].is_none() {
        return ERR_ARG;
    }
    st.spi_open[h as usize] = None;
    0
}

unsafe extern "C" fn h_i2c(user: *mut c_void, _p: i32, _sda: i32, _scl: i32, _f: u32, _a: u8, _tx: *const u8, tn: usize, _rx: *mut u8, rn: usize, _to: u32) -> i32 {
    let _ = cx!(user, "i2c_transfer");
    if tn == 0 && rn == 0 { ERR_ARG } else { ERR_IO }
}

unsafe extern "C" fn h_adc(user: *mut c_void, pin: i32, _r: u32, raw: *mut i32, mv: *mut u32, _u: *mut i32, _c: *mut i32) -> i32 {
    let _ = cx!(user, "adc_read");
    if raw.is_null() || mv.is_null() || pin < 0 || pin as usize >= PINS { ERR_ARG } else { ERR_UNSUPPORTED }
}

unsafe extern "C" fn h_led(user: *mut c_void, pin: i32, _r: u8, _g: u8, _b: u8) -> i32 {
    let (sh, me) = cx!(user, "rgbled_write");
    claim(sh, &me, pin)
}

unsafe extern "C" fn h_heap_info(user: *mut c_void, out: *mut u8, cap: usize) -> i32 {
    let _ = cx!(user, "heap_info");
    let v = cbor(|w| {
        w.map(6);
        for (i, x) in [41000u64, 30000, 28000, 230000, 38000, 26000].iter().enumerate() {
            w.uint(i as u64 + 1).uint(*x);
        }
    });
    put(out, cap, &v)
}

unsafe extern "C" fn h_heap_dump(user: *mut c_void, kind: i32) -> i32 {
    let _ = cx!(user, "heap_dump");
    if (0..=5).contains(&kind) { 0 } else { ERR_ARG }
}

unsafe extern "C" fn h_nv(user: *mut c_void, key: *const u8, kn: usize, value: *const u8, vn: usize, out: *mut u8, cap: usize) -> i32 {
    let (sh, _me) = cx!(user, "nv");
    if sl(key, kn) != b"wifi.rx_static" {
        return ERR_NOTFOUND;
    }
    let mut s = sh.st.borrow_mut();
    if !value.is_null() {
        if vn >= 16 {
            return ERR_ARG;
        }
        s.nv_value = String::from_utf8_lossy(sl(value, vn)).into_owned();
        return 0;
    }
    if s.nv_value.is_empty() {
        return ERR_NOTFOUND;
    }
    put(out, cap, s.nv_value.as_bytes())
}

unsafe extern "C" fn h_device_info(user: *mut c_void, out: *mut u8, cap: usize) -> i32 {
    let (sh, _me) = cx!(user, "device_info");
    let mut v = cbor(|w| {
        w.map(5).uint(1).map(1).uint(4).text("main").uint(2).map(0).uint(3).map(0).uint(4);
    });
    v.extend_from_slice(&module_info(sh));
    v.extend_from_slice(&cbor(|w| {
        w.uint(5).map(0);
    }));
    put(out, cap, &v)
}

fn registered(sh: &Shared, name: &[u8]) -> Option<Rc<Inst>> {
    sh.insts.borrow().iter().find(|m| m.registered.get() && m.name.as_bytes() == name).cloned()
}
fn has_registry(sh: &Shared) -> bool { sh.insts.borrow().iter().any(|m| m.registered.get()) }
fn module_info(sh: &Shared) -> Vec<u8> {
    if !sh.st.borrow().modules_cbor.is_empty() { return sh.st.borrow().modules_cbor.clone(); }
    let insts: Vec<_> = sh.insts.borrow().iter().filter(|m| m.registered.get()).cloned().collect();
    let mut data = vec![0; 64 + insts.len() * 128];
    let mut w = Writer::new(&mut data); w.array(insts.len());
    for m in insts {
        let st = m.st.borrow();
        w.map(6).uint(1).text(&m.name).uint(2).uint(st.component.unwrap_or(0) as u64)
            .uint(3).boolean(true).uint(4).boolean(!st.ended && st.calls != 0)
            .uint(5).uint(0).uint(6).int(if st.result == RET_DONE as i32 { 0 } else { st.result as i64 });
    }
    let len = w.finish().map_or(0, |v| v.len()); data.truncate(len); data
}
unsafe extern "C" fn h_modules_info(user: *mut c_void, out: *mut u8, cap: usize) -> i32 {
    let (sh, _me) = cx!(user, "modules_info");
    put(out, cap, &module_info(sh))
}

unsafe extern "C" fn h_module_start(user: *mut c_void, name: *const u8, n: usize, req: *const u8, rn: usize) -> i32 {
    let (sh, _me) = cx!(user, "module_start");
    if let Some(m) = registered(sh, sl(name, n)) {
        let mut st = m.st.borrow_mut();
        if st.ended { return ERR_UNSUPPORTED; } // native restart/rebind is a later slice
        if st.calls != 0 || !st.queue.is_empty() { return ERR_BUSY; }
        if rn > INBOX_DATA { return ERR_SIZE; }
        st.queue.push_back(QItem { component: 0, method: HOST_START as i32, data: sl(req, rn).to_vec(), ..Default::default() });
        sh.notify.notify_one(); return 0;
    }
    if has_registry(sh) { return ERR_NOTFOUND; }
    if sl(name, n) != b"test" {
        return ERR_NOTFOUND;
    }
    let mut s = sh.st.borrow_mut();
    s.started = "test".into();
    s.start_request = String::from_utf8_lossy(sl(req, rn)).into_owned();
    0
}
unsafe extern "C" fn h_module_stop(user: *mut c_void, name: *const u8, n: usize) -> i32 {
    let (sh, _me) = cx!(user, "module_stop");
    if let Some(m) = registered(sh, sl(name, n)) {
        if m.st.borrow().ended { return ERR_NOTFOUND; }
        if m.st.borrow().queue.len() >= QUEUE { return ERR_BUSY; }
        enqueue(&m, 0, HOST_STOP as i32, 0); sh.notify.notify_one(); return 0;
    }
    if has_registry(sh) { return ERR_NOTFOUND; }
    if sl(name, n) != b"test" {
        return ERR_NOTFOUND;
    }
    sh.st.borrow_mut().stopped = "test".into();
    0
}
unsafe extern "C" fn h_module_manifest(user: *mut c_void, name: *const u8, n: usize, out: *mut u8, cap: usize) -> i32 {
    let (sh, _me) = cx!(user, "module_manifest");
    if let Some(m) = registered(sh, sl(name, n)) { return put(out, cap, &m.manifest); }
    if has_registry(sh) { return ERR_NOTFOUND; }
    if sl(name, n) != b"test" {
        return ERR_NOTFOUND;
    }
    put(out, cap, &sh.st.borrow().manifest)
}
unsafe extern "C" fn h_reset(user: *mut c_void) -> i32 {
    let (sh, _me) = cx!(user, "reset");
    sh.st.borrow_mut().reset_called = true;
    0
}
// ---- packets ----

unsafe extern "C" fn h_pkt_alloc(user: *mut c_void, out: *mut u32) -> i32 {
    let (sh, me) = cx!(user, "pkt_alloc");
    if out.is_null() {
        return ERR_ARG;
    }
    *out = 0;
    let mut st = me.st.borrow_mut();
    if st.held.count(HELD_OWNED) >= st.held.quota as u32 {
        return ERR_BUSY;
    }
    let Some(slot) = sh.pool.acquire(2) else { return ERR_NOMEM };
    *out = st.held.add(slot, PKT_HEADROOM as u16, 0, HELD_OWNED);
    0
}

unsafe extern "C" fn h_pkt_buf(user: *mut c_void, pkt: u32, ptr: *mut *mut u8, max: *mut usize) -> i32 {
    let (sh, me) = cx!(user, "pkt_buf");
    let e = me.st.borrow_mut().held.get(pkt);
    let (Some(e), false, false) = (e, ptr.is_null(), max.is_null()) else { return ERR_ARG };
    if sh.pool.refs(e.slot as i32) != 1 {
        return ERR_BUSY;
    }
    *ptr = sh.pool.slot(e.slot as i32).add(e.start as usize);
    *max = PKT_MAX as usize;
    0
}

unsafe extern "C" fn h_pkt_span(user: *mut c_void, pkt: u32, base: *mut *mut u8, cap: *mut usize) -> i32 {
    let (sh, me) = cx!(user, "pkt_span");
    let e = me.st.borrow_mut().held.get(pkt);
    let (Some(e), false, false) = (e, base.is_null(), cap.is_null()) else { return ERR_ARG };
    if sh.pool.refs(e.slot as i32) != 1 {
        return ERR_BUSY;
    }
    *base = sh.pool.slot(e.slot as i32);
    *cap = xip_abi::PKT_SLOT_MIN as usize;
    0
}

unsafe extern "C" fn h_pkt_keep(user: *mut c_void, pkt: u32) -> i32 {
    let (_sh, me) = cx!(user, "pkt_keep");
    let rc = me.st.borrow_mut().held.keep(pkt);
    match rc {
        0 => 0,
        -2 => ERR_BUSY,
        _ => ERR_ARG,
    }
}

unsafe extern "C" fn h_pkt_free(user: *mut c_void, pkt: u32) -> i32 {
    let (sh, me) = cx!(user, "pkt_free");
    let e = me.st.borrow_mut().held.take(pkt);
    match e {
        Some(e) => {
            sh.pool.release(e.slot as i32);
            0
        }
        None => ERR_ARG,
    }
}

unsafe extern "C" fn h_pkt_info(user: *mut c_void, headroom: *mut u32, max: *mut u32) -> i32 {
    let _ = cx!(user, "pkt_info");
    *headroom = PKT_HEADROOM;
    *max = PKT_MAX;
    0
}

unsafe extern "C" fn h_send(user: *mut c_void, pkt: u32, len: usize, dst: u32, component: i32, method: i32, id: u64) -> i32 {
    let (sh, me) = cx!(user, "send");
    if dst >= ADDR_LINK {
        return crate::link::send(sh, &me, pkt, len, dst, component, method, id);     // ERR_BUSY keeps the packet
    }
    let Some(e) = me.st.borrow_mut().held.take(pkt) else { return ERR_ARG };
    let slot = e.slot as i32;
    let from = me.st.borrow().component.unwrap_or(0);
    let rc = if len > PKT_MAX as usize {
        ERR_SIZE
    } else if dst == ADDR_MAIN {
        let data = sl(sh.pool.slot(slot).add(e.start as usize), len).to_vec();
        sh.st.borrow_mut().main.push(MainMsg { inst: Rc::as_ptr(&me) as usize, from: from as u32, component, method, id, data });
        sh.pool.release(slot);
        return 0;
    } else {
        match route(sh, dst as i32) {
            Some(t) => deliver(sh, &t, slot, e.start, len as u16, component, method, from as u32, id, 0, 0, 0),
            None => ERR_NOTFOUND,
        }
    };
    if rc == 0 {
        return 0;
    }
    sh.pool.release(slot);
    rc
}

unsafe extern "C" fn h_mem_alloc(user: *mut c_void, size: usize) -> *mut c_void {
    let (_sh, me) = cx!(user, "mem_alloc");
    let mut st = me.st.borrow_mut();
    st.mem.alloc(size).cast()
}
unsafe extern "C" fn h_mem_free(user: *mut c_void, ptr: *mut c_void) -> i32 {
    let (_sh, me) = cx!(user, "mem_free");
    let mut st = me.st.borrow_mut();
    if st.mem.free(ptr.cast()) { 0 } else { ERR_ARG }
}

unsafe extern "C" fn h_get_component(user: *mut c_void, name: *const c_char) -> *const c_void {
    let (sh, _me) = cx!(user, "get_component");
    if name.is_null() {
        return std::ptr::null();
    }
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    if name == "wifi" {
        return crate::wifi::component().cast();
    }
    if name == "hw" {
        return &HARDWARE as *const Hardware as *const c_void;
    }
    if name == "power" {
        return crate::power::component().cast();
    }
    if name == "link" {
        return crate::link::component().cast();
    }
    if name == "wifi-tuning" {
        return crate::wifi::tuner().cast();
    }
    sh.st.borrow().components.get(&name).map_or(std::ptr::null(), |p| *p as *const c_void)
}

/// get_component("hw"): the pins, buses and converters of the fixture.
static HARDWARE: Hardware = Hardware {
    version: HARDWARE_VERSION,
    size: std::mem::size_of::<Hardware>() as u32,
    gpio_config: Some(h_gpio_config),
    gpio_read: Some(h_gpio_read),
    gpio_write: Some(h_gpio_write),
    irq_register: Some(h_irq_register),
    irq_unregister: Some(h_irq_unregister),
    irq_enable: Some(h_irq_enable),
    spi_open: Some(h_spi_open),
    spi_transfer: Some(h_spi_transfer),
    spi_close: Some(h_spi_close),
    i2c_transfer: Some(h_i2c),
    adc_read: Some(h_adc),
    rgbled_write: Some(h_led),
};

pub(crate) fn fill(t: &mut Host, user: *mut c_void) {
    t.abi_version = ABI_VERSION;
    t.size = std::mem::size_of::<Host>() as u32;
    t.user = user;
    t.log = Some(h_log);
    t.socket_read = Some(crate::network::read);
    t.socket_write = Some(crate::network::write);
    t.socket_close = Some(crate::network::close);
    t.socket_peer = Some(crate::network::peer);
    t.socket_write_pkt = Some(crate::network::write_pkt);
    t.now_ms = Some(h_now);
    t.get_setting = Some(h_get_setting);
    t.set_setting = Some(h_set_setting);
    t.alarm_set = Some(h_alarm_set);
    t.alarm_cancel = Some(h_alarm_cancel);
    t.modules_info = Some(h_modules_info);
    t.module_start = Some(h_module_start);
    t.module_stop = Some(h_module_stop);
    t.module_manifest = Some(h_module_manifest);
    t.reset = Some(h_reset);
    t.pkt_alloc = Some(h_pkt_alloc);
    t.pkt_buf = Some(h_pkt_buf);
    t.pkt_span = Some(h_pkt_span);
    t.pkt_keep = Some(h_pkt_keep);
    t.pkt_free = Some(h_pkt_free);
    t.pkt_info = Some(h_pkt_info);
    t.send = Some(h_send);
    t.mem_alloc = Some(h_mem_alloc);
    t.mem_free = Some(h_mem_free);
    t.heap_info = Some(h_heap_info);
    t.heap_dump = Some(h_heap_dump);
    t.nv = Some(h_nv);
    t.device_info = Some(h_device_info);
    t.get_component = Some(h_get_component);
    let _ = HELD_BORROWED;
}

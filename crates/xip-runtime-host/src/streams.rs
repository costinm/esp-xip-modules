//! Serialized module streams; bodies remain borrowed and output owns one pool lease.
use crate::runtime::Shared;
use std::ffi::c_void;
use xip_abi::*;

#[derive(Default)]
pub(crate) struct Streams {
    next: u32,
    entries: Vec<Entry>,
    services: Vec<(usize, i32, i32)>,
}
struct Entry {
    id: u32,
    provider: usize,
    consumer: Option<usize>,
    component: i32,
    method: i32,
    request: Option<u64>,
    header: Vec<u8>,
    input_fin: bool,
    output_fin: bool,
    pending: bool,
    output: Option<StreamPacket>,
}
#[derive(Clone, Copy)]
struct Target {
    id: u32,
    provider: usize,
    consumer: usize,
    component: i32,
    method: i32,
    request: Option<u64>,
}
fn target(e: &Entry) -> Option<Target> {
    Some(Target {
        id: e.id,
        provider: e.provider,
        consumer: e.consumer?,
        component: e.component,
        method: e.method,
        request: e.request,
    })
}
fn invoke(sh: &Shared, t: Target, event: i32, data: &[u8]) -> i32 {
    let insts = sh.insts.borrow();
    let Some(me) = insts
        .get(t.consumer)
        .filter(|m| !m.st.borrow().ended)
        .cloned()
    else {
        return ERR_NOTFOUND;
    };
    let from = insts
        .get(t.provider)
        .and_then(|m| m.st.borrow().component)
        .unwrap_or(0) as u32;
    drop(insts);
    let c = Call {
        size: core::mem::size_of::<Call>() as u32,
        component: t.component,
        method: t.method,
        value: event,
        flags: CALL_STREAM,
        id: t.id as u64,
        from,
        data: data.as_ptr(),
        len: data.len(),
        request_id: t.request.unwrap_or(0),
        request_has_id: t.request.is_some() as u32,
        pkt: 0,
        time_us: 0,
        request_reserved: 0,
        src_ip: [0; 16],
        src_port: 0,
        src_scope: 0,
    };
    me.run(&c)
}
unsafe extern "C" fn listen(u: *mut c_void, c: i32, m: i32) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    if c <= 0 || m <= 0 {
        return ERR_ARG;
    }
    let mut s = sh.streams.borrow_mut();
    if let Some((owner, _, _)) = s.services.iter().find(|(_, a, b)| *a == c && *b == m) {
        return if *owner == me.id { 0 } else { ERR_BUSY };
    }
    if s.services.len() >= 64 {
        return ERR_BUSY;
    }
    s.services.push((me.id, c, m));
    0
}
fn bind(sh: &Shared, id: u32, data: &[u8]) -> i32 {
    let (kind, e) = crate::envelope::parse(data);
    if kind != crate::envelope::Kind::Request {
        return ERR_ARG;
    }
    let t = {
        let mut s = sh.streams.borrow_mut();
        let consumer = s
            .services
            .iter()
            .find(|(_, c, m)| *c == e.component && *m == e.method)
            .map(|v| v.0)
            .or_else(|| {
                sh.insts
                    .borrow()
                    .iter()
                    .find(|m| {
                        m.registered.get()
                            && m.st.borrow().component == Some(e.component)
                            && !m.st.borrow().ended
                    })
                    .map(|m| m.id)
            });
        let Some(consumer) = consumer else {
            return ERR_NOTFOUND;
        };
        let Some(slot) = s.entries.iter_mut().find(|v| v.id == id) else {
            return ERR_NOTFOUND;
        };
        if consumer == slot.provider {
            return ERR_ARG;
        }
        slot.consumer = Some(consumer);
        slot.component = e.component;
        slot.method = e.method;
        slot.request = e.has_id.then_some(e.id);
        target(slot).unwrap()
    };
    invoke(sh, t, STREAM_OPEN, e.fields)
}
unsafe extern "C" fn open(u: *mut c_void, p: *const u8, n: usize) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    if n > 512 || (p.is_null() && n != 0) || (!p.is_null() && n == 0) {
        return ERR_ARG;
    }
    let id = {
        let mut s = sh.streams.borrow_mut();
        if s.entries.len() >= 8 || s.next == i32::MAX as u32 {
            return ERR_BUSY;
        }
        s.next += 1;
        let id = s.next;
        s.entries.push(Entry {
            id,
            provider: me.id,
            consumer: None,
            component: 0,
            method: 0,
            request: None,
            header: Vec::new(),
            input_fin: false,
            output_fin: false,
            pending: false,
            output: None,
        });
        id
    };
    if n != 0 {
        let rc = bind(sh, id, core::slice::from_raw_parts(p, n));
        if rc < 0 {
            remove(sh, id, me.id);
            return rc;
        }
    }
    id as i32
}
unsafe extern "C" fn feed(u: *mut c_void, id: u32, p: *const u8, n: usize, fin: u32) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    if (p.is_null() && n != 0) || (fin != 0 && n != 0) {
        return ERR_ARG;
    }
    let mut consumed = 0;
    loop {
        let t = {
            let s = sh.streams.borrow();
            let Some(e) = s.entries.iter().find(|e| e.id == id && e.provider == me.id) else {
                return ERR_NOTFOUND;
            };
            if e.input_fin {
                return ERR_ARG;
            }
            target(e)
        };
        if let Some(t) = t {
            let rc = invoke(
                sh,
                t,
                if fin != 0 { STREAM_FIN } else { STREAM_DATA },
                if n > consumed {
                    core::slice::from_raw_parts(p.add(consumed), n - consumed)
                } else {
                    &[]
                },
            );
            if rc < 0 {
                return rc;
            }
            if fin != 0 {
                if let Some(e) = sh
                    .streams
                    .borrow_mut()
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                {
                    e.input_fin = true;
                }
                return 0;
            }
            return if rc as usize <= n - consumed {
                consumed as i32 + rc
            } else {
                ERR_ARG
            };
        }
        if consumed == n {
            return if fin != 0 { ERR_ARG } else { consumed as i32 };
        }
        let ready = {
            let mut s = sh.streams.borrow_mut();
            let e = s.entries.iter_mut().find(|e| e.id == id).unwrap();
            if e.header.len() == 512 {
                return ERR_SIZE;
            }
            e.header.push(*p.add(consumed));
            consumed += 1;
            let mut r = cbor::Reader::new(&e.header);
            if r.skip() && r.rest().is_empty() {
                Some(e.header.clone())
            } else {
                None
            }
        };
        if let Some(h) = ready {
            let rc = bind(sh, id, &h);
            if rc < 0 {
                return rc;
            }
            if let Some(e) = sh
                .streams
                .borrow_mut()
                .entries
                .iter_mut()
                .find(|e| e.id == id)
            {
                e.header.clear();
            }
        }
    }
}
fn remove(sh: &Shared, id: u32, owner: usize) -> i32 {
    let e = {
        let mut s = sh.streams.borrow_mut();
        let Some(i) = s
            .entries
            .iter()
            .position(|e| e.id == id && (e.provider == owner || e.consumer == Some(owner)))
        else {
            return ERR_NOTFOUND;
        };
        s.entries.remove(i)
    };
    if let Some(ref p) = e.output {
        if let Some(v) = sh.pool.get(p.packet) {
            sh.pool.release(v.slot as i32);
        }
    }
    if e.consumer != Some(owner) {
        if let Some(t) = target(&e) {
            invoke(sh, t, STREAM_CLOSED, &[]);
        }
    }
    0
}
unsafe extern "C" fn close(u: *mut c_void, id: u32) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    remove(sh, id, me.id)
}
unsafe extern "C" fn write_pkt(u: *mut c_void, id: u32, p: u32, n: u32, fin: u32) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    let Some(packet) = sh.pool.get(p) else {
        return ERR_ARG;
    };
    if n > PKT_MAX || n > sh.pool.slot_size() as u32 - packet.start as u32 {
        return ERR_SIZE;
    }
    let mut s = sh.streams.borrow_mut();
    let Some(e) = s
        .entries
        .iter_mut()
        .find(|e| e.id == id && e.consumer == Some(me.id))
    else {
        return ERR_NOTFOUND;
    };
    if e.output_fin {
        return ERR_ARG;
    }
    if e.pending {
        return ERR_BUSY;
    }
    e.pending = true;
    e.output_fin = fin != 0;
    e.output = Some(StreamPacket {
        packet: p,
        len: n,
        fin,
    });
    0
}
unsafe extern "C" fn take(u: *mut c_void, id: u32, out: *mut StreamPacket) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    if out.is_null() {
        return ERR_ARG;
    }
    let mut s = sh.streams.borrow_mut();
    let Some(e) = s
        .entries
        .iter_mut()
        .find(|e| e.id == id && e.provider == me.id)
    else {
        return ERR_NOTFOUND;
    };
    *out = e.output.take().unwrap_or_default();
    0
}
unsafe extern "C" fn written(u: *mut c_void, id: u32) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    let mut s = sh.streams.borrow_mut();
    let Some(e) = s
        .entries
        .iter_mut()
        .find(|e| e.id == id && e.provider == me.id)
    else {
        return ERR_NOTFOUND;
    };
    if !e.pending || e.output.is_some() {
        return ERR_ARG;
    }
    e.pending = false;
    0
}
pub(crate) fn release(sh: &Shared, owner: usize) {
    let ids: Vec<_> = sh
        .streams
        .borrow()
        .entries
        .iter()
        .filter(|e| e.provider == owner || e.consumer == Some(owner))
        .map(|e| e.id)
        .collect();
    for id in ids {
        remove(sh, id, owner);
    }
    sh.streams.borrow_mut().services.retain(|v| v.0 != owner);
}
static TABLE: Stream = Stream {
    version: STREAM_VERSION,
    size: core::mem::size_of::<Stream>() as u32,
    listen: Some(listen),
    open: Some(open),
    feed: Some(feed),
    close: Some(close),
    write_pkt: Some(write_pkt),
    take: Some(take),
    written: Some(written),
};
pub(crate) fn component() -> *const Stream {
    &TABLE
}

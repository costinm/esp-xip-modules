//! Relay policy/control. Packet forwarding and use timestamps stay native.
use super::*;
pub(super) const TICK: i32 = 101;
fn table(hw: &Hw) -> Result<&'static Quic, i32> {
    let p = hw.get_component(c"quic").cast::<Quic>();
    if p.is_null() {
        return Err(ERR_UNSUPPORTED);
    }
    if unsafe {
        (*p).version != QUIC_VERSION || ((*p).size as usize) < core::mem::size_of::<Quic>()
    } {
        return Err(ERR_UNSUPPORTED);
    }
    Ok(unsafe { &*p })
}
fn field<'a>(data: &'a [u8], tag: u64) -> Result<Option<Item<'a>>, i32> {
    Ok(cbor::get(data, tag)
        .map_err(|_| ERR_ARG)?
        .and_then(|mut r| r.next()))
}
fn flag(data: &[u8], tag: u64, default: bool) -> Result<bool, i32> {
    match field(data, tag)? {
        None => Ok(default),
        Some(Item::Bool(v)) => Ok(v),
        _ => Err(ERR_ARG),
    }
}
fn idle(data: &[u8], default: u32) -> Result<u32, i32> {
    match field(data, 7)? {
        None => Ok(default),
        Some(Item::Uint(v)) => v.try_into().map_err(|_| ERR_ARG),
        _ => Err(ERR_ARG),
    }
}
fn cid(data: &[u8]) -> Result<&[u8], i32> {
    match field(data, 1)? {
        Some(Item::Bytes(b)) if !b.is_empty() && b.len() <= 20 => Ok(b),
        _ => Err(ERR_ARG),
    }
}
fn hop(hw: &Hw, cx: &mut Context, data: &[u8]) -> Result<RelayHop, i32> {
    let name = match field(data, 2)? {
        Some(Item::Text(b))
            if !b.is_empty() && b.len() < 16 && !b.contains(&0) && b != b"relay" =>
        {
            b
        }
        _ => return Err(ERR_ARG),
    };
    let mut h = RelayHop::default();
    h.token = match field(data, 3)? {
        Some(Item::Uint(v)) => v,
        Some(Item::Bytes(b)) if b.len() == 6 => mac_token(b.try_into().unwrap()),
        Some(Item::Bytes(b)) if b.len() == 22 && name.starts_with(b"udp") => {
            if b[16..18] == [0, 0] {
                return Err(ERR_ARG);
            }
            h.address_len = 22;
            h.address[..22].copy_from_slice(b);
            0
        }
        _ => return Err(ERR_ARG),
    };
    let rewrite = match field(data, 4)? {
        Some(Item::Bytes(b)) if !b.is_empty() && b.len() <= 20 => b,
        _ => return Err(ERR_ARG),
    };
    h.cid_len = rewrite.len() as u8;
    h.rewritten_dcid[..rewrite.len()].copy_from_slice(rewrite);
    h.weight = match field(data, 5)? {
        None => 1,
        Some(Item::Uint(v)) => v.try_into().map_err(|_| ERR_ARG)?,
        _ => return Err(ERR_ARG),
    };
    let mut z = [0; 16];
    z[..name.len()].copy_from_slice(name);
    let link = hw.link().ok_or(ERR_UNSUPPORTED)?;
    let addr = unsafe {
        link.open.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), z.as_ptr().cast(), COMPONENT, LINK_EGRESS)
    };
    if addr < 0 {
        return Err(addr);
    }
    h.link = addr as u32;
    if !cx.relay_links.contains(&h.link) {
        *cx.relay_links
            .iter_mut()
            .find(|a| **a == 0)
            .ok_or(ERR_NOMEM)? = h.link;
    }
    Ok(h)
}
fn same(a: &RelayHop, b: &RelayHop) -> bool {
    a.link == b.link
        && a.token == b.token
        && a.address_len == b.address_len
        && a.address == b.address
        && a.cid_len == b.cid_len
        && a.rewritten_dcid == b.rewritten_dcid
}
fn get(hw: &Hw, t: &Quic, wanted: &[u8]) -> Result<RelayRoute, i32> {
    for index in 0..64 {
        let mut r = RelayRoute::default();
        let rc = unsafe { t.relay_get.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), index, &mut r) };
        if rc < 0 {
            return Err(rc);
        }
        if &r.dcid[..r.dcid_len as usize] == wanted {
            return Ok(r);
        }
    }
    Err(ERR_NOTFOUND)
}
fn stats(hw: &Hw, t: &Quic, r: &RelayRoute) -> Result<RelayStats, i32> {
    let mut s = RelayStats::default();
    let rc = unsafe {
        t.relay_stats.ok_or(ERR_UNSUPPORTED)?(
            hw.ctx(),
            r.dcid.as_ptr(),
            r.dcid_len as usize,
            &mut s,
        )
    };
    if rc < 0 {
        Err(rc)
    } else {
        Ok(s)
    }
}
fn encode(
    hw: &Hw,
    t: &Quic,
    w: &mut Writer,
    r: &RelayRoute,
    reused: Option<bool>,
) -> Result<(), i32> {
    let s = stats(hw, t, r)?;
    let link = hw.link().ok_or(ERR_UNSUPPORTED)?;
    w.map(if reused.is_some() { 7 } else { 6 })
        .uint(1)
        .bytes(&r.dcid[..r.dcid_len as usize])
        .uint(2)
        .array(r.hop_count as usize);
    for h in &r.hops[..r.hop_count as usize] {
        let mut info = LinkInfo::default();
        let rc = unsafe { link.info.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), h.link, &mut info) };
        if rc < 0 {
            return Err(rc);
        }
        let n = info.name.iter().position(|b| *b == 0).unwrap_or(16);
        w.map(4).uint(1).text_raw(&info.name[..n]).uint(2);
        if h.address_len == 22 {
            w.bytes(&h.address[..22]);
        } else {
            w.uint(h.token);
        }
        w.uint(3)
            .bytes(&h.rewritten_dcid[..h.cid_len as usize])
            .uint(4)
            .uint(h.weight as u64);
    }
    w.uint(3)
        .boolean(r.endpoint != 0)
        .uint(4)
        .uint(s.idle_ms as u64)
        .uint(5)
        .uint(s.last_used_us)
        .uint(6)
        .uint(hw.now_us().unwrap_or(0).saturating_sub(s.last_used_us) / 1000);
    if let Some(reused) = reused {
        w.uint(7).boolean(reused);
    }
    Ok(())
}
fn arm(hw: &Hw, cx: &mut Context) -> Result<(), i32> {
    if cx.relay_alarm == 0 {
        let alarm = hw.alarm_set(1000, 0, 1000, ALARM_EXACT, COMPONENT, TICK);
        if alarm < 0 {
            return Err(alarm);
        }
        cx.relay_alarm = alarm as u32 + 1;
    }
    Ok(())
}
pub(super) fn tick(hw: &Hw, cx: &mut Context) {
    if let Ok(t) = table(hw) {
        if let Some(expire) = t.relay_expire {
            unsafe {
                expire(hw.ctx());
            }
        }
        sync_links(hw, cx, t);
    }
}
pub(super) fn handle(
    hw: &Hw,
    cx: &mut Context,
    method: i32,
    data: &[u8],
    reply: Reply,
) -> Result<(), i32> {
    let t = table(hw)?;
    let rc = unsafe { t.relay_expire.ok_or(ERR_UNSUPPORTED)?(hw.ctx()) };
    if rc < 0 {
        return Err(rc);
    }
    let mut out = [0; 1280];
    let mut w = Writer::new(&mut out);
    let result = (|| {
        match method {
            3 => {
                if field(data, 1)?.is_some() {
                    return Err(ERR_ARG);
                }
                let idle = idle(data, 60_000)?;
                let reuse = flag(data, 8, true)?;
                let endpoint = flag(data, 6, false)?;
                let mut r = RelayRoute::default();
                r.hop_count = 1;
                r.endpoint = endpoint as u8;
                r.hops[0] = hop(hw, cx, data)?;
                if r.hops[0].weight != 0 && idle > 0 {
                    arm(hw, cx)?;
                }
                let rc = unsafe {
                    t.relay_create.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &mut r, idle, reuse as u32)
                };
                if rc < 0 {
                    return Err(rc);
                }
                encode(hw, t, &mut w, &r, Some(rc == 1))?;
            }
            4 | 8 => {
                let mut r = get(hw, t, cid(data)?)?;
                let h = hop(hw, cx, data)?;
                let at = r.hops[..r.hop_count as usize]
                    .iter()
                    .position(|old| same(old, &h));
                if method == 4 {
                    let index = if let Some(at) = at {
                        at
                    } else {
                        if r.hop_count == 4 {
                            return Err(ERR_NOMEM);
                        }
                        let at = r.hop_count as usize;
                        r.hop_count += 1;
                        at
                    };
                    r.hops[index] = h;
                    let rc = unsafe { t.relay_set.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &r, 1) };
                    if rc < 0 {
                        return Err(rc);
                    }
                    encode(hw, t, &mut w, &r, None)?;
                } else {
                    let at = at.ok_or(ERR_NOTFOUND)?;
                    let n = r.hop_count as usize;
                    for i in at..n - 1 {
                        r.hops[i] = r.hops[i + 1];
                    }
                    r.hops[n - 1] = RelayHop::default();
                    r.hop_count -= 1;
                    let rc = if r.hop_count == 0
                        || r.hops[..r.hop_count as usize].iter().all(|h| h.weight == 0)
                    {
                        unsafe {
                            t.relay_remove.ok_or(ERR_UNSUPPORTED)?(
                                hw.ctx(),
                                r.dcid.as_ptr(),
                                r.dcid_len as usize,
                            )
                        }
                    } else {
                        unsafe { t.relay_set.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &r, 1) }
                    };
                    if rc < 0 {
                        return Err(rc);
                    }
                    w.map(0);
                }
            }
            5 => {
                let r = get(hw, t, cid(data)?)?;
                encode(hw, t, &mut w, &r, None)?;
            }
            6 => {
                let index: u32 = cbor::get_int(data, 1)
                    .map_err(|_| ERR_ARG)?
                    .unwrap_or(0)
                    .try_into()
                    .map_err(|_| ERR_ARG)?;
                let mut routes = [RelayRoute::default(); 2];
                let mut n = 0;
                for route in &mut routes {
                    let rc = unsafe {
                        t.relay_get.ok_or(ERR_UNSUPPORTED)?(
                            hw.ctx(),
                            index.saturating_add(n as u32),
                            route,
                        )
                    };
                    if rc == ERR_NOTFOUND {
                        break;
                    }
                    if rc < 0 {
                        return Err(rc);
                    }
                    n += 1;
                }
                w.map(2).uint(1).array(n);
                for r in &routes[..n] {
                    encode(hw, t, &mut w, r, None)?;
                }
                w.uint(2).uint(index as u64 + n as u64);
            }
            7 => {
                let cid = cid(data)?;
                let rc = unsafe {
                    t.relay_remove.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), cid.as_ptr(), cid.len())
                };
                if rc < 0 {
                    return Err(rc);
                }
                w.map(0);
            }
            9 => {
                let r = get(hw, t, cid(data)?)?;
                if field(data, 7)?.is_none() {
                    return Err(ERR_ARG);
                }
                let idle = idle(data, 0)?;
                if idle > 0 {
                    arm(hw, cx)?;
                }
                let rc = unsafe {
                    t.relay_idle.ok_or(ERR_UNSUPPORTED)?(
                        hw.ctx(),
                        r.dcid.as_ptr(),
                        r.dcid_len as usize,
                        idle,
                    )
                };
                if rc < 0 {
                    return Err(rc);
                }
                encode(hw, t, &mut w, &r, None)?;
            }
            _ => return Err(ERR_UNSUPPORTED),
        }
        reply.send(hw, w.finish().ok_or(ERR_SIZE)?);
        Ok(())
    })();
    sync_links(hw, cx, t);
    result
}
fn sync_links(hw: &Hw, cx: &mut Context, t: &Quic) {
    let Some(link) = hw.link() else { return };
    let mut used = [false; 12];
    let mut expiring = false;
    let Some(get) = t.relay_get else { return };
    for index in 0..64 {
        let mut route = RelayRoute::default();
        let rc = unsafe { get(hw.ctx(), index, &mut route) };
        if rc == ERR_NOTFOUND {
            break;
        }
        if rc < 0 {
            return;
        }
        expiring |= stats(hw, t, &route).is_ok_and(|s| s.idle_ms != 0);
        for h in &route.hops[..route.hop_count as usize] {
            for (i, addr) in cx.relay_links.iter().enumerate() {
                if *addr == h.link {
                    used[i] = true;
                }
            }
        }
    }
    if !expiring && cx.relay_alarm != 0 {
        hw.alarm_cancel((cx.relay_alarm - 1) as i32);
        cx.relay_alarm = 0;
    }
    for (i, addr) in cx.relay_links.iter_mut().enumerate() {
        if *addr != 0 && !used[i] {
            if let Some(close) = link.close {
                unsafe {
                    close(hw.ctx(), *addr);
                }
            }
            *addr = 0;
        }
    }
}
pub(super) fn stop(hw: &Hw, cx: &mut Context) {
    if cx.relay_alarm != 0 {
        hw.alarm_cancel((cx.relay_alarm - 1) as i32);
        cx.relay_alarm = 0;
    }
    if let Ok(t) = table(hw) {
        if let (Some(get), Some(remove)) = (t.relay_get, t.relay_remove) {
            loop {
                let mut route = RelayRoute::default();
                if unsafe { get(hw.ctx(), 0, &mut route) } < 0 {
                    break;
                }
                if unsafe { remove(hw.ctx(), route.dcid.as_ptr(), route.dcid_len as usize) } < 0 {
                    break;
                }
            }
        }
        sync_links(hw, cx, t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::{c_char, c_void};
    #[derive(Default)]
    struct Fake {
        opens: usize,
    }
    unsafe extern "C" fn open(ctx: *mut c_void, _: *const c_char, _: i32, _: i32) -> i32 {
        (*(ctx as *mut Fake)).opens += 1;
        ADDR_LINK as i32
    }
    static LINK: Link = Link {
        version: LINK_VERSION,
        size: core::mem::size_of::<Link>() as u32,
        open: Some(open),
        close: None,
        provide: None,
        rx: None,
        tx_done: None,
        ready: None,
        set_state: None,
        info: None,
        classify: None,
        route: None,
        unroute: None,
    };
    unsafe extern "C" fn component(_: *mut c_void, name: *const c_char) -> *const c_void {
        if core::ffi::CStr::from_ptr(name) == c"link" {
            (&LINK as *const Link).cast()
        } else {
            core::ptr::null()
        }
    }
    #[test]
    fn flat_destination_is_validated_before_opening_a_link() {
        let mut f = Fake::default();
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION;
        h.size = core::mem::size_of::<Host>() as u32;
        h.user = (&mut f as *mut Fake).cast();
        h.get_component = Some(component);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut cx = Context {
            record: [0; announce::INFO_MAX],
            len: 0,
            channel: 6,
            next_id: 0,
            pending: None,
            relay_links: [0; 12],
            relay_alarm: 0,
        };
        let mut b = [0; 128];
        let mut w = Writer::new(&mut b);
        w.map(3)
            .uint(2)
            .text("espnow")
            .uint(3)
            .bytes(&[1, 2, 3, 4, 5, 6])
            .uint(4)
            .bytes(&[]);
        assert!(matches!(
            hop(&hw, &mut cx, w.finish().unwrap()),
            Err(ERR_ARG)
        ));
        assert_eq!(f.opens, 0);
        let mut w = Writer::new(&mut b);
        w.map(4)
            .uint(2)
            .text("espnow")
            .uint(3)
            .bytes(&[1, 2, 3, 4, 5, 6])
            .uint(4)
            .bytes(&[3])
            .uint(5)
            .uint(2);
        let h = hop(&hw, &mut cx, w.finish().unwrap()).unwrap();
        assert_eq!(h.token, mac_token([1, 2, 3, 4, 5, 6]));
        assert_eq!(h.weight, 2);
        assert_eq!(f.opens, 1);
    }
}

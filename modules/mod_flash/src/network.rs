//! Recovery service policy; native runtime supplies Wi-Fi and UDP mechanisms.
use xip_abi::{
    cbor, CallView, Hw, WifiIntent, WifiState, ERR_ARG, ERR_BUSY, ERR_TIMEOUT, ERR_UNSUPPORTED,
};
pub(crate) fn event(hw: Hw, code: u64) {
    let mut data=[0;24]; let mut w=cbor::Writer::new(&mut data);
    w.map(2).uint(0).uint(code).uint(1).uint(hw.now_ms());
    if let Some(data)=w.finish(){let _=hw.info_publish(44,101,data);}
}
pub struct Network {
    saved: WifiIntent,
    next_scan: u64,
    owned: bool,
    version: u32,
    pending: Option<Pending>,
}
#[derive(Clone, Copy)]
struct Reply {
    from: u32,
    component: i32,
    method: i32,
    id: u64,
    envelope: (u64, u64),
}
struct Pending {
    reply: Reply,
    deadline: u64,
}
impl Reply {
    fn send(self, hw: Hw, result: Result<&[u8], i32>) -> i32 {
        let mut b = [0; 320];
        let mut w = cbor::Writer::new(&mut b);
        let (method, id) = self.envelope;
        w.map(4)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(method)
            .uint(3)
            .uint(id);
        match result {
            Ok(data) => {
                w.uint(6).raw(data);
            }
            Err(rc) => {
                w.uint(7).int(rc as i64);
            }
        }
        match w.finish() {
            Some(b) => hw.message(self.from, self.component, self.method, self.id, b),
            None => xip_abi::ERR_SIZE,
        }
    }
}
/// An active recovery operation defers sleep without changing another module's policy.
pub fn hold(hw: Hw) {
    if let Some(power)=hw.power() {
        // NAN checks active_window before changing Wi-Fi roles. An awake-only
        // vote prevents physical sleep but does not preserve the TCP bearer.
        if let Some(activate)=power.activate { unsafe {activate(hw.ctx(),10000);} }
        else if let Some(awake)=power.awake_for { unsafe {awake(hw.ctx(),10000);} }
    }
}

fn info(hw: Hw, b: &mut [u8]) -> Result<&[u8], i32> {
    let s = state(hw).ok_or(ERR_UNSUPPORTED)?;
    if s.ssid_len as usize > s.ssid.len() {
        return Err(ERR_ARG);
    }
    let ready = s.sta == 2 && s.ip4 != [0; 4];
    let mut w = cbor::Writer::new(b);
    w.map(7)
        .uint(1)
        .boolean(ready)
        .uint(2)
        .text_raw(&s.ssid[..s.ssid_len as usize])
        .uint(3)
        .bytes(&s.ip4)
        .uint(4)
        .uint(5227)
        .uint(5)
        .uint(s.sta as u64)
        .uint(6)
        .bytes(&s.sta_mac)
        .uint(7)
        .uint(s.channel as u64);
    w.finish().ok_or(xip_abi::ERR_SIZE)
}
fn state(hw: Hw) -> Option<WifiState> {
    let mut s = WifiState::default();
    s.size = core::mem::size_of::<WifiState>() as u32;
    let get = hw.wifi()?.state?;
    (unsafe { get(hw.ctx(), &mut s) } >= 0).then_some(s)
}
fn apply(hw: Hw, i: &WifiIntent) {
    if let Some(set) = hw.wifi().and_then(|w| w.set) {
        let mut g = 0;
        unsafe {
            set(hw.ctx(), i, &mut g);
        }
    }
}
impl Network {
    #[inline(never)]
    pub fn start(hw: Hw) -> Self {
        let mut i = WifiIntent::default();
        let mut generation = 0;
        if let Some(get) = hw.wifi().and_then(|w| w.get) {
            unsafe {
                get(hw.ctx(), &mut i, &mut generation);
            }
        }
        let owned = state(hw).is_none_or(|s| s.radio == 0);
        if owned {
            if let Some(t) = hw.wifi_tuner() {
                let mut tuning = xip_abi::WifiTuning::default();
                if let (Some(get), Some(set)) = (t.get, t.set) {
                    if unsafe { get(hw.ctx(), &mut tuning) } >= 0 {
                        tuning.power = xip_abi::POWER_NONE;
                        unsafe {
                            set(hw.ctx(), &tuning);
                        }
                    }
                }
            }
        }
        if i.sta_ssid_len != 0 {
            i.radio = 1;
            i.sta = 1;
            if owned {
                i.now = 0;
                i.ap = 0;
            }
            apply(hw, &i);
            event(hw,10);
        } else {
            event(hw,11);
        }
        Self {
            saved: i,
            next_scan: hw.now_ms() + if i.sta_ssid_len == 0 { 0 } else { 30000 },
            owned,
            version: runtime_version(hw),
            pending: None,
        }
    }
    /// Recovery control is a raw CBOR envelope on PPP 250. UDP is a locator only.
    pub fn control(&mut self, hw: Hw, call: &CallView) -> bool {
        if call.flags & xip_abi::CALL_LINK == 0 || call.src.is_some() {
            return false;
        }
        let mut r = cbor::Reader::new(call.data);
        if !r.skip() || !r.rest().is_empty() || cbor::get_int(call.data, 1) != Ok(Some(44)) {
            return false;
        }
        let Ok(Some(method @ (21 | 22))) = cbor::get_int(call.data, 2) else {
            return false;
        };
        let Ok(Some(id)) = cbor::get_int(call.data, 3) else {
            return false;
        };
        if id < 0 {
            return false;
        }
        let Ok(Some(r)) = cbor::get(call.data, 5) else {
            return false;
        };
        let fields = r.rest();
        let envelope = (method as u64, id as u64);
        let reply = Reply {
            from: call.from,
            component: call.component,
            method: call.method,
            id: call.id,
            envelope,
        };
        if method == 22 {
            let mut b = [0; 192];
            reply.send(hw, info(hw, &mut b));
            return true;
        }
        if self.pending.is_some() {
            // Retransmission of the same PPP request keeps the original deadline.
            let p = self.pending.as_ref().unwrap();
            if !(p.reply.from == reply.from && p.reply.envelope == reply.envelope) {
                reply.send(hw, Err(ERR_BUSY));
            }
            return true;
        }
        match self.join(hw, fields) {
            Ok(()) => {
                self.pending = Some(Pending {
                    reply,
                    deadline: hw.now_ms() + 60000,
                })
            }
            Err(rc) => {
                reply.send(hw, Err(rc));
            }
        }
        true
    }
    fn join(&mut self, hw: Hw, fields: &[u8]) -> Result<(), i32> {
        let ssid = cbor::get_text(fields, 1)
            .map_err(|_| ERR_ARG)?
            .ok_or(ERR_ARG)?;
        let psk = cbor::get_text(fields, 2)
            .map_err(|_| ERR_ARG)?
            .unwrap_or(b"");
        if ssid.is_empty()
            || ssid.len() > 32
            || ssid.contains(&0)
            || psk.contains(&0)
            || (!psk.is_empty() && !(8..=63).contains(&psk.len()))
        {
            return Err(ERR_ARG);
        }
        let wifi = hw.wifi().ok_or(ERR_UNSUPPORTED)?;
        let mut i = self.saved;
        if let Some(get) = wifi.get {
            let mut generation = 0;
            let rc = unsafe { get(hw.ctx(), &mut i, &mut generation) };
            if rc < 0 {
                return Err(rc);
            }
        }
        i.radio = 1;
        i.sta = 1;
        // Preserve other active link roles, but discard stale station selection.
        i.channel = 0;
        i.bssid = [0; 6];
        i.sta_ssid = [0; 32];
        i.sta_ssid[..ssid.len()].copy_from_slice(ssid);
        i.sta_ssid_len = ssid.len() as u8;
        i.sta_psk = [0; 64];
        i.sta_psk[..psk.len()].copy_from_slice(psk);
        i.sta_psk_len = psk.len() as u8;
        let set = wifi.set.ok_or(ERR_UNSUPPORTED)?;
        let mut generation = 0;
        let rc = unsafe { set(hw.ctx(), &i, &mut generation) };
        if rc < 0 {
            return Err(rc);
        }
        self.saved = i;
        self.next_scan = hw.now_ms() + 60000;
        event(hw,12);
        Ok(())
    }
    fn poll_join(&mut self, hw: Hw) {
        if self.pending.is_some() {
            hold(hw);
        }
        let Some(p) = self.pending.as_ref() else {
            return;
        };
        let ready = state(hw).is_some_and(|s| {
            s.sta == 2
                && s.ip4 != [0; 4]
                && s.ssid_len == self.saved.sta_ssid_len
                && s.ssid[..s.ssid_len as usize]
                    == self.saved.sta_ssid[..self.saved.sta_ssid_len as usize]
        });
        let result = if ready {
            let mut b = [0; 192];
            p.reply.send(hw, info(hw, &mut b))
        } else if hw.now_ms() >= p.deadline {
            p.reply.send(hw, Err(ERR_TIMEOUT))
        } else {
            return;
        };
        // Pool or link backpressure must not discard the only completion.
        if result >= 0 {
            self.pending = None;
        }
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn tick(&mut self, hw: Hw) {
        self.poll_join(hw);
        if self.pending.is_none()
            && self.owned
            && state(hw).is_some_and(|s| s.sta != 2)
            && hw.now_ms() >= self.next_scan
        {
            let mut aps = [xip_abi::WifiAp {
                ssid: [0; 32],
                ssid_len: 0,
                bssid: [0; 6],
                rssi: 0,
                channel: 0,
                auth: 0,
            }; 16];
            if let Some(scan) = hw.wifi().and_then(|w| w.scan) {
                let n = unsafe { scan(hw.ctx(), aps.as_mut_ptr(), aps.len()) };
                if n >= 0 {
                    let configured = &self.saved.sta_ssid[..self.saved.sta_ssid_len as usize];
                    let choices = &aps[..(n as usize).min(aps.len())];
                    let best = choices
                        .iter()
                        .filter(|a| a.ssid_len <= 32)
                        .filter(|a| {
                            let s = &a.ssid[..a.ssid_len as usize];
                            !configured.is_empty() && s == configured || is_flash(s)
                        })
                        .max_by_key(|a| {
                            ((a.ssid[..a.ssid_len as usize] == *configured) as u8, a.rssi)
                        });
                    let mut i = self.saved;
                    i.radio = 1;
                    i.now = 0;
                    i.ap = 0;
                    if let Some(a) = best {
                        let ssid = &a.ssid[..a.ssid_len as usize];
                        i.sta = 1;
                        i.channel = a.channel;
                        i.bssid = a.bssid;
                        if ssid != configured {
                            i.sta_ssid = a.ssid;
                            i.sta_ssid_len = a.ssid_len;
                            i.sta_psk = [0; 64];
                            i.sta_psk[..19].copy_from_slice(b"untrusted-open-mode");
                            i.sta_psk_len = 19;
                        }
                        event(hw,13);
                    } else {
                        i.sta = 0;
                        i.ap = 1;
                        i.channel = 1;
                        i.ap_ssid[..10].copy_from_slice(b"ESP-RESCUE");
                        i.ap_ssid_len = 10;
                        i.ap_psk[..19].copy_from_slice(b"untrusted-open-mode");
                        i.ap_psk_len = 19;
                        event(hw,14);
                    }
                    apply(hw, &i);
                    self.next_scan = hw.now_ms() + 30000;
                }
            }
        }
        tick(hw, self.version);
    }
}
fn is_flash(s: &[u8]) -> bool {
    s.len() == 15
        && s[..7].eq_ignore_ascii_case(b"DIRECT-")
        && s[9..].eq_ignore_ascii_case(b"-flash")
}
fn announcement(hw: Hw, version: u32) -> Option<(xip_abi::Packet, usize)> {
    let state = state(hw)?;
    let sta = state.sta == 2;
    let mac = if state.sta_mac != [0;6] { state.sta_mac } else { state.ap_mac };
    let chip = if cfg!(feature="esp32") { 1 } else if cfg!(feature="esp32s3") { 2 } else { 3 };
    let packet = unsafe { xip_abi::Packet::from_raw(hw, hw.pkt_alloc().ok()?) };
    let (ptr, cap) = hw.pkt_buf(packet.raw()).ok()?;
    let data = unsafe { core::slice::from_raw_parts_mut(ptr, cap) };
    let mut w = cbor::Writer::new(data);
    w.map(8).uint(0).uint(44).uint(1).uint(1).uint(2).uint(version as u64)
        .uint(3).bytes(&mac).uint(4).uint(5227).uint(5).uint(chip)
        .uint(6).boolean(sta).uint(7).uint(hw.now_ms()/1000);
    let len = w.finish()?.len();
    Some((packet, len))
}
pub fn announce(hw: Hw, handle: u32, dest: ([u8; 16], u16, u32), version: u32) {
    if let Some((packet,len)) = announcement(hw, version) {
        if let Ok((ptr,_)) = hw.pkt_buf(packet.raw()) {
            // udp_send borrows bytes synchronously; Packet returns the lease on all outcomes.
            hw.udp_send(handle,dest,unsafe{core::slice::from_raw_parts(ptr,len)});
        }
    }
}
pub fn serial(hw: Hw, call: &CallView, version: u32) {
    if call.data == b"discover" {
        if let Some((packet,len)) = announcement(hw, version) {
            let packet=packet.into_raw();
            let rc=hw.send(packet,len,call.from,call.component,call.method,call.id);
            // Native link BUSY retains ownership; discovery drops instead of queuing.
            if call.from>=xip_abi::ADDR_LINK && rc==ERR_BUSY {hw.pkt_free(packet);}
        }
    }
}
fn normal_discovery_running(hw: Hw) -> bool {
    let mut running = false;
    crate::for_modules(hw, |name, component, live, _| {
        running |= live && component == 47 && name == b"disc";
    }) && running
}
fn tick(hw: Hw, version: u32) {
    if normal_discovery_running(hw) {
        return;
    }
    let mut group = [0u8; 16];
    group[0] = 255;
    group[1] = 2;
    group[14] = 0x52;
    group[15] = 0x27;
    announce(hw, 0, (group, 5227, 0), version);
}

#[inline(never)]
fn runtime_version(hw: Hw) -> u32 {
    let Some((base, _)) = crate::running_partition(hw) else {
        return 0;
    };
    let mut version = [0u8; 32];
    if hw.flash_read(base, 48, &mut version) != 32 {
        return 0;
    }
    let mut n = 0u32;
    for b in version {
        if !b.is_ascii_digit() {
            break;
        }
        let Some(v) = n
            .checked_mul(10)
            .and_then(|v| v.checked_add((b - b'0') as u32))
        else {
            return 0;
        };
        n = v;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;
    unsafe extern "C" fn modules(user: *mut c_void, out: *mut u8, cap: usize) -> i32 {
        let status = *(user as *const i32);
        if status < 0 {
            return status;
        }
        let b = core::slice::from_raw_parts_mut(out, cap);
        let mut w = xip_abi::cbor::Writer::new(b);
        w.array(if status == 0 { 0 } else { 1 });
        if status != 0 {
            w.map(3)
                .uint(1)
                .text("disc")
                .uint(2)
                .uint(47)
                .uint(4)
                .boolean(status == 2);
        }
        w.finish().unwrap().len() as i32
    }
    #[test]
    fn recovery_broadcast_yields_only_to_running_discovery_and_resumes_on_stop() {
        let mut state = 0i32;
        let host = xip_abi::Host {
            abi_version: xip_abi::ABI_VERSION,
            size: core::mem::size_of::<xip_abi::Host>() as u32,
            user: (&mut state as *mut i32).cast(),
            modules_info: Some(modules),
            ..unsafe { core::mem::zeroed() }
        };
        let hw = unsafe { Hw::new(&host) }.unwrap();
        for (status, expected) in [(0, false), (1, false), (2, true), (1, false), (-1, false)] {
            state = status;
            assert_eq!(normal_discovery_running(hw), expected);
        }
    }
    struct Pool { data: [u8; 256], owned: bool, fail_alloc: bool, fail_buf: bool, freed: u32 }
    unsafe extern "C" fn wifi_state(_: *mut c_void,out:*mut WifiState)->i32 {
        (*out).sta=2; (*out).sta_mac=[2,0,0,0,0,1]; 0
    }
    static WIFI:xip_abi::Wifi=xip_abi::Wifi {version:xip_abi::WIFI_VERSION,size:core::mem::size_of::<xip_abi::Wifi>() as u32,state:Some(wifi_state),..unsafe{core::mem::zeroed()}};
    unsafe extern "C" fn component(_: *mut c_void,name:*const core::ffi::c_char)->*const c_void {
        if core::ffi::CStr::from_ptr(name).to_bytes()==b"wifi" {(&WIFI as *const xip_abi::Wifi).cast()} else {core::ptr::null()}
    }
    unsafe extern "C" fn alloc(user:*mut c_void,out:*mut u32)->i32 {
        let p=&mut *(user as *mut Pool);if p.fail_alloc || p.owned {return xip_abi::ERR_NOMEM;}p.owned=true;*out=1;0
    }
    unsafe extern "C" fn buf(user:*mut c_void,_:u32,out:*mut *mut u8,cap:*mut usize)->i32 {
        let p=&mut *(user as *mut Pool);if p.fail_buf{return xip_abi::ERR_BUSY;}*out=p.data.as_mut_ptr();*cap=p.data.len();0
    }
    unsafe extern "C" fn busy_send(_: *mut c_void,_:u32,_:usize,_:u32,_:i32,_:i32,_:u64)->i32 {ERR_BUSY}
    unsafe extern "C" fn free(user:*mut c_void,_:u32)->i32 {let p=&mut *(user as *mut Pool);assert!(p.owned);p.owned=false;p.freed+=1;0}
    #[test]
    fn discovery_uses_one_packet_and_releases_it_on_failure() {
        let mut pool=Pool{data:[0;256],owned:false,fail_alloc:false,fail_buf:false,freed:0};
        let host=xip_abi::Host{abi_version:xip_abi::ABI_VERSION,size:core::mem::size_of::<xip_abi::Host>() as u32,
            user:(&mut pool as *mut Pool).cast(),get_component:Some(component),pkt_alloc:Some(alloc),pkt_buf:Some(buf),pkt_free:Some(free),send:Some(busy_send),..unsafe{core::mem::zeroed()}};
        let hw=unsafe{Hw::new(&host)}.unwrap();
        let (packet,n)=announcement(hw,123).unwrap();assert!(pool.owned && n<256);
        assert_eq!(cbor::get_int(&pool.data[..n],0),Ok(Some(44)));
        assert_eq!(cbor::get_int(&pool.data[..n],2),Ok(Some(123)));
        drop(packet);assert!(!pool.owned && pool.freed==1);
        pool.fail_buf=true;assert!(announcement(hw,123).is_none());assert!(!pool.owned && pool.freed==2);
        pool.fail_buf=false;pool.fail_alloc=true;assert!(announcement(hw,123).is_none());assert_eq!(pool.freed,2);
        pool.fail_alloc=false;
        let call=CallView{component:44,method:0,value:0,flags:0,data:b"discover",time_us:0,pkt:0,
            from:xip_abi::ADDR_LINK,id:1,request_id:None,src:None};
        serial(hw,&call,123);assert!(!pool.owned && pool.freed==3);
    }

    unsafe extern "C" fn active(user:*mut c_void,ms:u32)->i32 {assert_eq!(ms,10000);*(user as *mut u32)=1;0}
    unsafe extern "C" fn awake(user:*mut c_void,_:u32)->i32 {*(user as *mut u32)=2;0}
    static POWER:xip_abi::Power=xip_abi::Power {version:xip_abi::POWER_VERSION,size:core::mem::size_of::<xip_abi::Power>() as u32,activate:Some(active),awake_for:Some(awake),..unsafe{core::mem::zeroed()}};
    unsafe extern "C" fn power_component(_: *mut c_void,name:*const core::ffi::c_char)->*const c_void {
        if core::ffi::CStr::from_ptr(name).to_bytes()==b"power" {(&POWER as *const xip_abi::Power).cast()} else {core::ptr::null()}
    }
    #[test]
    fn active_transfer_holds_the_policy_window_not_just_cpu_awake() {
        let mut called=0u32;
        let host=xip_abi::Host{abi_version:xip_abi::ABI_VERSION,size:core::mem::size_of::<xip_abi::Host>() as u32,user:(&mut called as *mut u32).cast(),get_component:Some(power_component),..unsafe{core::mem::zeroed()}};
        hold(unsafe{Hw::new(&host)}.unwrap());assert_eq!(called,1);
    }

}

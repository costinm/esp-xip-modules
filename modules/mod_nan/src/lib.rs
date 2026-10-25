//! NAN service discovery and NOW beacon-equivalent synchronization.
//! The runtime classifies radio input into the manifest's virtual NAN port.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
mod diagnostics;
mod discovery;
mod publication;
mod frames;
mod protocol;
mod sleep;
mod wake;
mod watch;
mod station;
use protocol::Engine;
use xip_abi::{
    cbor::{self, Item, Writer},
    *,
};
const C_SELF: i32 = 46;
const TICK: i32 = 100;
const ACQUIRE_RETRY: i32 = 101;
const CONTINUE: i32 = RET_CONTINUE as i32;
pub const STATE_BYTES: usize = 8;
#[derive(Default)]
pub struct State {
    context: usize,
}
struct Context {
    engine: Engine,
    original: WifiIntent,
    generation: u32,
    channel: u8,
    alarm: i32,
    now_link: i32,
    action_link: i32,
    monitoring: bool,
    sta_mac: [u8; 6],
    ap_mac: [u8; 6],
    radio_paused: bool,
    last_public: Option<([u8; 6], u64)>,
    last_sync: Option<([u8; 6], u64)>,
    armed: bool,
    discovery: discovery::Discovery,
    watches: watch::Watches,
    remote_wake: Option<alloc::boxed::Box<RemoteWake>>,
    station: Option<alloc::boxed::Box<station::Station>>,
    auto_reconnect_original: Option<bool>,
    // Each entry is an exact pool lease. BUSY keeps it; stop releases it.
    tx: [Option<QueuedTx>; 4],
    sleep: sleep::Policy,
    sleep_original: Option<alloc::boxed::Box<WifiIntent>>,
}
#[derive(Clone, Copy)]
struct QueuedTx {
    pkt: u32,
    len: usize,
    now: bool,
    until: u64,
    destination: u64,
    owner: u64,
}
struct RemoteWake {
    target: [u8;6], record: [u8;96], len: usize, id: u64,
    now: bool, until: u64, next: u64,
}
const APP_MESSAGE: i32 = 1000;
const APP_REPLY: i32 = 1001;

fn item(data: &[u8], key: u64) -> Option<&[u8]> {
    let mut r = cbor::get(data, key).ok()??;
    let rest = r.rest();
    r.skip().then_some(&rest[..rest.len() - r.rest().len()])
}
fn bytes(data: &[u8], key: u64) -> Option<&[u8]> {
    match cbor::get(data, key).ok()??.next()? {
        Item::Bytes(b) => Some(b),
        _ => None,
    }
}
fn watch_duration(fields: &[u8]) -> Result<u64,i32> {
    match cbor::get(fields,1).map_err(|_|ERR_ARG)? {
        None=>Ok(5000),
        Some(mut r)=>match r.next() {Some(Item::Uint(ms)) if ms<=300_000=>Ok(ms),_=>Err(ERR_ARG)},
    }
}
// TODO: standalone NOW, with the normal awake NOW radio configuration, needs
// no promiscuous receive after acquisition. Use native NOW RX in that case.
// Sleepy devices need explicit receive/wake coordination. An associated STA
// may need a bounded awake vote / Wi-Fi tuning for the DW pair, then restoration
// of its prior modem power-save policy. Do not make promiscuous mode or a
// permanently awake station a protocol requirement merely to get NOW sync.
fn monitor(hw: &Hw, channel: u8, enabled: bool) -> i32 {
    let Some(f) = hw.wifi().and_then(|w| w.monitor) else {
        return ERR_UNSUPPORTED;
    };
    if enabled {
        if let Some(state) = hw.wifi().and_then(|w| w.state) {
            let mut actual = WifiState {
                size: core::mem::size_of::<WifiState>() as u32,
                ..Default::default()
            };
            let rc = unsafe { state(hw.ctx(), &mut actual) };
            if rc < 0 {
                return rc;
            }
            if actual.sta == 2 && actual.channel != channel {
                return ERR_ARG;
            }
        }
    }
    let m = WifiMonitor {
        enable: enabled as u8,
        channel: if enabled { channel } else { 0 },
        ..Default::default()
    };
    unsafe { f(hw.ctx(), &m) }
}
fn link(hw: &Hw, name: &core::ffi::CStr) -> Result<i32, i32> {
    let f = hw.link().and_then(|l| l.open).ok_or(ERR_UNSUPPORTED)?;
    let rc = unsafe { f(hw.ctx(), name.as_ptr(), C_SELF, 0) };
    if rc < 0 {
        Err(rc)
    } else {
        // Resolve the native egress address without subscribing to arbitrary
        // NOW traffic. Radio ingress arrives only on the virtual NAN port.
        if let Some(close) = hw.link().and_then(|l| l.close) {
            unsafe {
                close(hw.ctx(), rc as u32);
            }
        }
        Ok(rc)
    }
}
// An updater's netif must not be restarted by a module's channel hint.
fn radio_intent(original: WifiIntent, channel: u8) -> Result<WifiIntent, i32> {
    if original.radio != 0 && (original.sta != 0 || original.ap != 0) {
        if original.now == 0 {
            return Err(ERR_UNSUPPORTED);
        }
        Ok(original)
    } else {
        Ok(WifiIntent {
            radio: 1,
            now: 1,
            channel,
            ..original
        })
    }
}
// Sleeping NAN/NOW has no IP role. Credentials remain available to restore
// active mode, but no STA/AP association or IP listener is resumed for a DW.
fn sleepy_intent(original: WifiIntent, channel: u8) -> WifiIntent {
    WifiIntent { radio: 1, sta: 0, ap: 0, now: 1, channel, ..original }
}
impl Context {
    fn station_tick(&mut self, hw: &Hw, now: u64) {
        if self.sleep.enabled {return;}
        let Some(wifi)=hw.wifi() else {return};
        if self.auto_reconnect_original.is_none() {
            let Some(f)=wifi.auto_reconnect else {return};
            let rc=unsafe {f(hw.ctx(),false)};if rc<0 {return;}
            self.auto_reconnect_original=Some(rc!=0);
        }
        let (Some(get),Some(set),Some(state))=(wifi.get,wifi.set,wifi.state) else {return};
        let mut intent=WifiIntent::default();let mut generation=0;
        if unsafe {get(hw.ctx(),&mut intent,&mut generation)}<0 {return;}
        if generation!=self.generation {
            // Adopt a new explicit owner intent without restoring an older
            // configuration when this module is subsequently stopped.
            self.original=intent;
            if self.station.as_ref().is_some_and(|p|p.generation!=generation) {self.station=None;}
        }
        if self.station.is_none() && intent.sta!=0 {
            let ptr=unsafe {alloc::alloc::alloc(core::alloc::Layout::new::<station::Station>())}.cast::<station::Station>();
            if ptr.is_null() {self.engine.with_stats(|s|s.last_error=ERR_NOMEM);return;}
            unsafe {ptr.write(station::Station::new(intent,generation,now));self.station=Some(alloc::boxed::Box::from_raw(ptr));}
        }
        let mut actual=WifiState {size:core::mem::size_of::<WifiState>() as u32,..Default::default()};
        if unsafe {state(hw.ctx(),&mut actual)}<0 {return;}
        let capture=self.engine.plan(now).capture;
        if let Some(p)=self.station.as_mut() {
            match p.poll(hw,&actual,now,capture) {
                Ok(Some(want))=>{
                    let rc=unsafe {set(hw.ctx(),&want,&mut generation)};
                    if rc==0 {self.generation=generation;p.generation=generation;self.monitoring=false;}
                    else {self.engine.with_stats(|s|s.last_error=rc);}
                },
                Err(rc)=>self.engine.with_stats(|s|s.last_error=rc),_=>{}
            }
        }
    }
    fn owns_mac(&self, mac: [u8; 6]) -> bool {
        mac == self.engine.local || mac == self.sta_mac || (self.ap_mac != [0;6] && mac == self.ap_mac)
    }
    fn wake_request<'a>(&self, record: &'a [u8]) -> Result<Option<(dmesh_server::control::TransportKind, dmesh_server::control::TransportConfig<'a>)>, i32> {
        let result = wake::request(record, self.engine.local);
        if result != Err(ERR_ARG) { return result; }
        for mac in [self.sta_mac, self.ap_mac] {
            if mac != [0;6] { let result = wake::request(record, mac); if result != Err(ERR_ARG) { return result; } }
        }
        Err(ERR_ARG)
    }
    /// Query real driver state, not a cached monitor intent. Restart and failed
    /// action handoff both clear SDK capture independently of the module.
    fn radio_ready(&mut self, hw: &Hw) -> bool {
        let Some(state) = hw.wifi().and_then(|w| w.state) else { return true };
        let mut actual = WifiState { size: core::mem::size_of::<WifiState>() as u32, ..Default::default() };
        if unsafe { state(hw.ctx(), &mut actual) } < 0 { return false; }
        self.sta_mac = actual.sta_mac;
        self.ap_mac = actual.ap_mac;
        let source = if actual.ap != 0 && actual.sta != 2 { actual.ap_mac } else { actual.sta_mac };
        if source != [0;6] && source != self.engine.local {
            self.engine.local = source;
            self.engine.local_source_changed(source);
        }
        self.monitoring = actual.monitoring != 0;
        let ready = actual.radio != 0 && !self.station.as_ref().is_some_and(|p|p.scanning)
            && (actual.sta == 0 || (actual.sta == 2 && actual.channel == self.channel));
        if !ready {
            if self.monitoring { let _ = monitor(hw, self.channel, false); self.monitoring = false; }
            self.radio_paused = true;
        } else if self.radio_paused {
            self.radio_paused = false;
            self.armed = false;
        }
        ready
    }
    fn enter_sleepy(&mut self, hw: &Hw, trace: bool, cadence: u64) -> Result<(), i32> {
        self.sleep.enable(hw, trace)?;
        if self.sleep_original.is_none() {
            let wifi = hw.wifi().ok_or(ERR_UNSUPPORTED)?;
            let mut original = WifiIntent::default(); let mut generation = 0;
            let rc = unsafe { wifi.get.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &mut original, &mut generation) };
            if rc < 0 { self.sleep.disable(hw); return Err(rc); }
            let ptr = unsafe { alloc::alloc::alloc(core::alloc::Layout::new::<WifiIntent>()) }.cast::<WifiIntent>();
            if ptr.is_null() { self.sleep.disable(hw); return Err(ERR_NOMEM); }
            unsafe { ptr.write(original); }
            let saved = unsafe { alloc::boxed::Box::from_raw(ptr) };
            let quiet = sleepy_intent(original, self.channel);
            let rc = unsafe { wifi.set.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &quiet, &mut generation) };
            if rc < 0 { self.sleep.disable(hw); return Err(rc); }
            self.generation = generation;
            self.sleep_original = Some(saved);
            self.monitoring = false; // role change tears down/rebuilds the radio
        }
        self.engine.wake_dw = cadence;
        Ok(())
    }
    fn leave_sleepy(&mut self, hw: &Hw) {
        self.sleep.disable(hw);
        self.engine.wake_dw = protocol::WAKE_DW;
        if let Some(original) = self.sleep_original.take() {
            if let Some(wifi) = hw.wifi() {
                let mut current = WifiIntent::default(); let mut generation = 0;
                if let (Some(get), Some(set)) = (wifi.get, wifi.set) {
                    if unsafe { get(hw.ctx(), &mut current, &mut generation) } == 0 && generation == self.generation {
                        let rc = unsafe { set(hw.ctx(), &*original, &mut generation) };
                        if rc == 0 { self.generation = generation; self.monitoring = false; }
                        else { self.engine.with_stats(|s| s.last_error = rc); }
                    }
                }
            }
        }
    }
    fn cluster_bssid(&self) -> Result<[u8; 6], i32> {
        let cluster = self.engine.nan.cluster().ok_or(ERR_AGAIN)?.0;
        Ok(if cluster[..3] == dmesh_rawnan::NAN_BSSID_OUI {
            cluster
        } else {
            [0x50, 0x6f, 0x9a, cluster[3], cluster[4], cluster[5]]
        })
    }
    fn enqueue(&mut self, hw: &Hw, path: discovery::ReturnPath, payload: &[u8], now: u64) -> Result<(),i32> {
        self.enqueue_type(hw,path,payload,now,7)
    }
    fn enqueue_type(
        &mut self,
        hw: &Hw,
        path: discovery::ReturnPath,
        payload: &[u8],
        now: u64,
        msg_type: u8,
    ) -> Result<(), i32> {
        let at = self.tx.iter().position(Option::is_none).ok_or(ERR_BUSY)?;
        let bssid = if path.now {
            [0; 6]
        } else {
            self.cluster_bssid()?
        };
        let pkt = hw.pkt_alloc()?;
        let n = hw.pkt_buf(pkt).ok().and_then(|(ptr, cap)| {
            let out = unsafe { core::slice::from_raw_parts_mut(ptr, cap) };
            if path.now {
                self.engine.now_message(now, payload, out)
            } else {
                self.discovery.sequence = self.discovery.sequence.wrapping_add(1).max(1);
                frames::followup_type(
                    out,
                    self.engine.local,
                    path.peer,
                    bssid,
                    path.instance,
                    self.discovery.sequence,
                    payload,
                    msg_type,
                )
            }
        });
        let Some(len) = n else {
            hw.pkt_free(pkt);
            return Err(ERR_SIZE);
        };
        self.tx[at] = Some(QueuedTx {
            pkt,
            len,
            now: path.now,
            destination: mac_token(path.peer),
            owner: self.remote_wake.as_ref().filter(|w| payload==&w.record[..w.len]).map_or(0,|w|w.id),
            until: now + 2 * protocol::WAKE_PERIOD_US,
        });
        Ok(())
    }
    fn flush(&mut self, hw: &Hw, now: u64, nan_open: bool) {
        for slot in &mut self.tx {
            let Some(tx) = *slot else { continue };
            if tx.until <= now {
                hw.pkt_free(tx.pkt);
                *slot = None;
                continue;
            }
            if !tx.now && !nan_open {
                continue;
            }
            // The exact prepared lease remains queued on BUSY. Every other
            // result consumes it under the common link ownership contract.
            let rc = hw.send(
                tx.pkt,
                tx.len,
                if tx.now {
                    self.now_link
                } else {
                    self.action_link
                } as u32,
                0,
                0,
                tx.destination,
            );
            if rc != ERR_BUSY {
                *slot = None;
            }
            if rc < 0 {
                self.engine.with_stats(|s| {
                    s.tx_errors += 1;
                    s.last_error = rc;
                });
            }
        }
    }
    fn activate_radio(&mut self, hw: &Hw, kind: dmesh_server::control::TransportKind,
        config: dmesh_server::control::TransportConfig<'_>, path: discovery::ReturnPath, now: u64) -> Result<(), i32> {
        let wifi = hw.wifi().ok_or(ERR_UNSUPPORTED)?;
        let mut current = WifiIntent::default(); let mut generation = 0;
        let rc = unsafe { wifi.get.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &mut current, &mut generation) };
        if rc < 0 { return Err(rc); }
        // A newer native owner supersedes our saved snapshot.
        let saved = if generation == self.generation { self.sleep_original.as_deref().copied().unwrap_or(current) } else { current };
        let (intent, dw) = wake::intent(kind, config, saved)?;
        let rc = unsafe { wifi.set.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), &intent, &mut generation) };
        if rc < 0 { return Err(rc); }
        self.station=None;
        let was_sleepy = self.sleep.enabled;
        let trace = was_sleepy && self.sleep.tracing();
        let changed = generation != self.generation;
        self.sleep.disable(hw);
        self.sleep_original = None;
        self.generation = generation;
        self.engine.wake_dw = dw;
        if was_sleepy || changed { self.monitoring = false; }
        if trace {
            // Phase 3 means an accepted radio activation, not association/IP
            // readiness. Passive PPP observation avoids waking the target by UART.
            let mut data = [0; 64]; let mut w = Writer::new(&mut data);
            w.map(5).uint(1).uint(3).uint(2).uint(now).uint(3).bytes(&path.peer)
                .uint(4).boolean(path.now).uint(5).uint(generation as u64);
            if let (Some(data), Ok(serial)) = (w.finish(), link(hw, c"serial")) {
                hw.message(serial as u32, 0, 0, 250, data);
            }
        }
        Ok(())
    }
    fn post_discovery(&mut self, hw: &Hw, payload: &[u8], subscribe: bool, via_now: bool, now: u64) -> Result<(),i32> {
        if via_now {return self.enqueue(hw,discovery::ReturnPath {peer:[0xff;6],instance:0,now:true},payload,now);}
        let at=self.tx.iter().position(Option::is_none).ok_or(ERR_BUSY)?;
        let bssid=self.cluster_bssid()?;let pkt=hw.pkt_alloc()?;
        let n=hw.pkt_buf(pkt).ok().and_then(|(ptr,cap)| frames::sdf(unsafe {core::slice::from_raw_parts_mut(ptr,cap)},self.engine.local,
            dmesh_rawnan::NAN_DISCOVERY_MAC,bssid,1,0,if subscribe {0x11}else{0x10},payload));
        let Some(len)=n else {hw.pkt_free(pkt);return Err(ERR_SIZE)};
        self.tx[at]=Some(QueuedTx {pkt,len,now:false,until:now+2*protocol::WAKE_PERIOD_US,destination:0,owner:0});Ok(())
    }
    fn start_remote_wake(&mut self, target: [u8;6], via_now: bool, kind: i64, now: u64) -> Result<u64,i32> {
        if self.remote_wake.is_some() {return Err(ERR_BUSY)}
        if target==[0;6] || target[0]&1!=0 || self.owns_mac(target) {return Err(ERR_ARG)}
        let nan=match kind {1=>false,6=>true,_=>return Err(ERR_UNSUPPORTED)};
        let mut record=[0;96];let id=now.max(1);
        let len=wake::encode_activation(&mut record,target,nan,id).ok_or(ERR_SIZE)?;
        let ptr=unsafe {alloc::alloc::alloc(core::alloc::Layout::new::<RemoteWake>())}.cast::<RemoteWake>();
        if ptr.is_null() {return Err(ERR_NOMEM)}
        unsafe {ptr.write(RemoteWake {target,record,len,id,now:via_now,until:now+20_000_000,next:now});self.remote_wake=Some(alloc::boxed::Box::from_raw(ptr));}
        Ok(id)
    }
    fn clear_remote_wake(&mut self, hw: &Hw) {
        if let Some(w)=self.remote_wake.take() {
            for slot in &mut self.tx {
                if slot.is_some_and(|tx|tx.owner==w.id) {
                    hw.pkt_free(slot.take().unwrap().pkt);
                }
            }
        }
    }
    fn remote_wake_tick(&mut self, hw: &Hw, now: u64) {
        let Some(w)=self.remote_wake.as_ref() else {return};
        if now>=w.until {self.clear_remote_wake(hw);return;}
        if now<w.next {return;}
        let via_now=w.now;let target=w.target;let mut body=[0;96];let n=w.len;body[..n].copy_from_slice(&w.record[..n]);
        let rc=if via_now {self.enqueue(hw,discovery::ReturnPath {peer:target,instance:0,now:true},&body[..n],now)}
            else {self.active_discovery(hw,now).map(|_|())};
        if let Some(w)=self.remote_wake.as_mut() {w.next=now+if rc==Err(ERR_BUSY) {20_000}else{protocol::PERIOD_US};}
    }
    fn remote_wake_peer(&mut self, hw: &Hw, path: discovery::ReturnPath, payload: &[u8], now: u64) {
        let Some(w)=self.remote_wake.as_ref() else {return};
        if path.peer!=w.target || path.now!=w.now {return;}
        if dmesh_server::tagged::decode(payload).is_some_and(|r|r.id==Some(w.id)&&(r.result.is_some()||r.error.is_some())
            && r.component==Some(dmesh_server::tagged::Name::Tag(1)) && r.method==Some(dmesh_server::tagged::Name::Tag(4))) {
            self.clear_remote_wake(hw);return;
        }
        if path.now {return;}
        let mut body=[0;96];let n=w.len;body[..n].copy_from_slice(&w.record[..n]);
        let _=self.enqueue_type(hw,path,&body[..n],now,2);
    }
    /// One event of a watch. A native route gets `result` as another answer to the request (`SEND_MORE`: the runtime wraps it with the request's id and
    /// keeps the request open; `last` ends it); a radio route gets the envelope that goes over the air, which this module builds for the peer.
    fn deliver_event(&self, hw: &Hw, route: watch::Route, id: u64, result: &[u8], last: bool, now: u64) {
        match route {
            watch::Route::Native { address, token } => {
                hw.message(address, C_SELF, 7 | if last { 0 } else { SEND_MORE }, token, result);
            }
            watch::Route::Radio(path) => {
                let mut enc=[0;1400];
                let Some(n)=dmesh_server::tagged::encode_numeric_response(46,7,id,result,&mut enc) else { return };
                let data=&enc[..n];
                // No discovery backlog: a BUSY/failed destination loses this
                // event and its lease immediately. Control replies keep their
                // separate bounded transaction ownership.
                let Ok(pkt)=hw.pkt_alloc() else { return };
                let len=hw.pkt_buf(pkt).ok().and_then(|(ptr,cap)| {
                    let out=unsafe {core::slice::from_raw_parts_mut(ptr,cap)};
                    if path.now { self.engine.now_message(now,data,out) } else {
                        frames::followup(out,self.engine.local,path.peer,self.cluster_bssid().ok()?,path.instance,1,data)
                    }
                });
                if let Some(n)=len {
                    let rc=hw.send(pkt,n,if path.now {self.now_link}else{self.action_link} as u32,0,0,mac_token(path.peer));
                    if rc==ERR_BUSY {hw.pkt_free(pkt);}
                } else {hw.pkt_free(pkt);}
            }
        }
    }
    fn observation(&self, hw: &Hw, path: discovery::ReturnPath, record: &[u8], now: u64, rssi: i8) {
        for listener in self.watches.listeners().iter().filter(|s| s.until > now) {
            let mut result=[0;1280]; let mut w=Writer::new(&mut result);
            w.map(6).uint(1).uint(1).uint(2).bytes(&path.peer).uint(3).bytes(record)
                .uint(4).uint(now).uint(5).boolean(path.now).uint(6).int(rssi as i64);
            let Some(result)=w.finish() else {continue};
            self.deliver_event(hw,listener.route,listener.id,result,false,now);
        }
    }
    fn expire_watches(&mut self, hw: &Hw, now: u64) {
        // Four route descriptors on the stack, never retained observations.
        let mut ended=[None;4]; let mut n=0;
        self.watches.expire(now, |s| {ended[n]=Some(s);n+=1;});
        for s in ended.into_iter().flatten() {
            self.deliver_event(hw,s.route,s.id,&[0xa1,1,2],true,now);
        }
    }
    fn dispatch_record(
        &mut self,
        hw: &Hw,
        path: discovery::ReturnPath,
        record: &[u8],
        now: u64,
    ) -> Result<(), i32> {
        if record.is_empty() {
            return Ok(());
        }
        if let Some((kind, config)) = self.wake_request(record)? {
            if !self.discovery.admit_request(path, record, now) { return Ok(()); }
            let result = self.activate_radio(hw, kind, config, path, now);
            // Android's basic wake is one-way. Replying would trigger its
            // pending directed-message callback to transmit the wake again.
            if let Some(id) = dmesh_server::tagged::decode(record).and_then(|r| r.id) {
                let mut out = [0; 128];
                let n = if result.is_ok() {
                    dmesh_server::tagged::encode_numeric_response(1, 4, id, &[0xa0], &mut out)
                } else {
                    dmesh_server::tagged::encode_numeric_error(1, 4, id, b"unsupported or invalid activation", &mut out)
                }.ok_or(ERR_SIZE)?;
                self.enqueue(hw, path, &out[..n], now)?;
            }
            return result;
        }
        // Observations are forwarded live; identity verification/history belongs
        // to the consumer. Signed announcements are data, never handler calls.
        if publication::is_announcement(record) { return Ok(()); }
        // A reply is received transport data, never another handler request.
        if dmesh_server::tagged::decode(record)
            .is_some_and(|r| r.result.is_some() || r.error.is_some())
        {
            return Ok(());
        }
        if !self.discovery.admit_request(path, record, now) {
            return Ok(());
        }
        if let Some(id) = dmesh_server::announce::discovery_request_id(record) {
            let mut response = [0; 255];
            let n = publication::response(&self.engine.info[..self.engine.info_len], id, &mut response)
                .ok_or(ERR_UNSUPPORTED)?;
            return self.enqueue(hw, path, &response[..n], now);
        }
        // Shared connectionless admission policy. Application
        // handlers still enforce target/owner checks. Discovery is owned here.
        if !dmesh_server::connectionless::allowed(record) {
            return Err(ERR_ARG);
        }
        let token = self.discovery.reserve(path, now)?;
        let mut data = [0; 384];
        let mut w = Writer::new(&mut data);
        w.map(2).uint(1).bytes(&path.peer).uint(2).bytes(record);
        let data = w.finish().ok_or(ERR_SIZE)?;
        let mut rc = hw.message(ADDR_MAIN, C_SELF, APP_MESSAGE, token, data);
        if rc == ERR_NOTFOUND {
            // Generic Main: the runtime's envelope dispatch (the code of the 0x444d0002 path) delivers the request to the module of its
            // component, or to the console for a native one, and the wrapped answer comes back here with `token` as its id.
            rc = hw.message(ADDR_ENVELOPE, 0, 0, token, record);
        }
        if rc < 0 {
            self.discovery.take(token, now);
            return Err(rc);
        }
        Ok(())
    }
    fn receive_message(&mut self, hw: &Hw, path: discovery::ReturnPath, payload: &[u8], now: u64, rssi: i8) {
        let Some((payload, wake_only)) = wake::content(payload, self.engine.local)
            .or_else(|| wake::content(payload, self.sta_mac))
            .or_else(|| if self.ap_mac != [0;6] { wake::content(payload, self.ap_mac) } else { None }) else { return };
        // Type 2 cannot be used to smuggle an unrelated handler request.
        if wake_only && !matches!(self.wake_request(payload), Ok(Some(_))) { return; }
        self.remote_wake_peer(hw,path,payload,now);
        self.observation(hw, path, payload, now, rssi);
        if let Some(record) = dmesh_server::tagged::decode(payload) {
            if record.component == Some(dmesh_server::tagged::Name::Tag(46)) && record.method == Some(dmesh_server::tagged::Name::Tag(7)) {
                if let Some(id) = record.id {
                    let rc = watch_duration(record.fields.unwrap_or(&[0xa0])).and_then(|ms|self.watches.register(watch::Route::Radio(path),id,ms,now));
                    let mut out=[0;64]; let n=match rc {
                        Ok(()) => dmesh_server::tagged::encode_numeric_response(46,7,id,&[0xa0],&mut out),
                        Err(_) => dmesh_server::tagged::encode_numeric_error(46,7,id,b"invalid or full watch",&mut out),
                    };
                    if let Some(n)=n { let _=self.enqueue(hw,path,&out[..n],now); }
                }
                return;
            }
        }
        if let Err(rc) = self.dispatch_record(hw, path, payload, now) {
            self.engine.with_stats(|s| s.last_error = rc);
        }
    }
    fn receive_radio_messages(&mut self, hw: &Hw, source: [u8; 6], data: &[u8], at: u64, rssi: i8) {
        use dmesh_rawnan as wire;
        if self.owns_mac(source) {
            return;
        }
        if let Some(sync) = wire::parse_now_sync_body(data) {
            self.receive_message(
                hw,
                discovery::ReturnPath {
                    peer: source,
                    instance: 0,
                    now: true,
                },
                sync.service_info,
                at, rssi,
            );
            return;
        }
        if wire::is_nan_followup(data) {
            if !data.get(4..10).and_then(|m| m.try_into().ok()).is_some_and(|m| self.owns_mac(m)) { return; }
            if let Some(payload) = wire::followup_service_info(data, wire::DMESH_SERVICE_ID) {
                let instance = wire::service_descriptors(data)
                    .into_iter()
                    .find(|d| d.service_id == wire::DMESH_SERVICE_ID)
                    .map_or(1, |d| d.descriptor.instance);
                self.receive_message(
                    hw,
                    discovery::ReturnPath {
                        peer: source,
                        instance,
                        now: false,
                    },
                    payload,
                    at, rssi,
                );
            }
            return;
        }
        if !wire::is_nan_sdf(data) {
            return;
        }
        let active = wire::active_subscribe_service_info(data, wire::DMESH_SERVICE_ID);
        let mut empty_subscribe = None;
        let mut payload_seen = false;
        for item in wire::service_descriptors(data) {
            if item.service_id != wire::DMESH_SERVICE_ID {
                continue;
            }
            let descriptor = item.descriptor;
            if descriptor.control & 3 == 1 {
                empty_subscribe = Some(descriptor.instance);
            }
            if !descriptor.payload.is_empty() {
                payload_seen = true;
                self.receive_message(
                    hw,
                    discovery::ReturnPath {
                        peer: source,
                        instance: descriptor.instance,
                        now: false,
                    },
                    descriptor.payload,
                    at, rssi,
                );
            }
        }
        if let Some(active) = active.filter(|a| !a.service_info.is_empty()) {
            self.receive_message(
                hw,
                discovery::ReturnPath {
                    peer: source,
                    instance: active.instance,
                    now: false,
                },
                active.service_info,
                at, rssi,
            );
        } else if !payload_seen {
            if let Some(instance) = empty_subscribe {
                // Android obtains its PeerHandle through an empty Subscribe;
                // answer with the cached signed discovery record, as FW does.
                let mut request = [0; 32];
                if let Some(n) = dmesh_server::announce::encode_discovery_request(0, &mut request) {
                    self.receive_message(
                        hw,
                        discovery::ReturnPath {
                            peer: source,
                            instance,
                            now: false,
                        },
                        &request[..n],
                        at, rssi,
                    );
                }
            }
        }
    }
    fn active_discovery(&mut self, hw: &Hw, now: u64) -> Result<u64, i32> {
        let at = self.tx.iter().position(Option::is_none).ok_or(ERR_BUSY)?;
        let bssid = self.cluster_bssid()?;
        let mut request = [0; 32];
        let n =
            dmesh_server::announce::encode_discovery_request(now, &mut request).ok_or(ERR_SIZE)?;
        let pkt = hw.pkt_alloc()?;
        let len = hw.pkt_buf(pkt).ok().and_then(|(ptr, cap)| {
            frames::sdf(
                unsafe { core::slice::from_raw_parts_mut(ptr, cap) },
                self.engine.local,
                dmesh_rawnan::NAN_DISCOVERY_MAC,
                bssid,
                1,
                0,
                0x11,
                &request[..n],
            )
        });
        let Some(len) = len else {
            hw.pkt_free(pkt);
            return Err(ERR_SIZE);
        };
        self.tx[at] = Some(QueuedTx {
            pkt,
            len,
            now: false,
            destination: 0,
            owner: 0,
            until: now + 2 * protocol::WAKE_PERIOD_US,
        });
        Ok(now)
    }

    fn schedule(&mut self, hw: &Hw, at: u64, method: i32) {
        if self.alarm >= 0 {
            hw.alarm_cancel(self.alarm);
        }
        let now = hw.now_us().unwrap_or(0);
        let ms = at.saturating_sub(now).div_ceil(1000).min(u32::MAX as u64) as u32;
        self.alarm = hw.alarm_set(ms.max(1), 0, 0, ALARM_EXACT | ALARM_WINDOW, C_SELF, method);
        if self.alarm < 0 {
            self.engine.with_stats(|s| s.last_error = self.alarm);
        }
    }
    fn arm(&mut self, hw: &Hw) {
        if !self.radio_ready(hw) { self.schedule(hw, hw.now_us().unwrap_or(0) + 1_000_000, ACQUIRE_RETRY); return; }
        let called = hw.now_us().unwrap_or(0);
        let rc = monitor(hw, self.channel, true);
        if rc < 0 {
            self.engine.with_stats(|s| s.last_error = rc);
            if hw
                .now_us()
                .unwrap_or(0)
                .saturating_sub(self.engine.started_us)
                < 3_000_000
            {
                self.schedule(hw, hw.now_us().unwrap_or(0) + 20_000, ACQUIRE_RETRY);
            }
            return;
        }
        self.engine.with_stats(|s| s.last_error = 0);
        self.monitoring = true;
        self.armed = true;
        let now = hw.now_us().unwrap_or(0);
        self.engine.acquire(now);
        let plan = self.engine.plan(now);
        self.engine.with_stats(|s| s.open(called, now, &plan));
        self.tick(hw);
    }
    fn transmit(&mut self, hw: &Hw, now: u64, public: bool) {
        if !public && !self.engine.emits_sync() {
            return;
        }
        let Ok(pkt) = hw.pkt_alloc() else {
            self.engine.with_stats(|s| s.tx_errors += 1);
            return;
        };
        let n = hw.pkt_buf(pkt).ok().and_then(|(ptr, cap)| {
            let out = unsafe { core::slice::from_raw_parts_mut(ptr, cap) };
            if public {
                self.engine.publish(out)
            } else {
                self.engine.now_body(now, out)
            }
        });
        let Some(n) = n else {
            hw.pkt_free(pkt);
            return;
        };
        let dest = if public {
            self.action_link
        } else {
            self.now_link
        };
        // Link BUSY retains the exact lease. Periodic advertisements may be
        // dropped explicitly; the next window produces a fresh one.
        let rc = hw.send(pkt, n, dest as u32, 0, 0, 0);
        if rc == 0 {
            if public {
                self.engine.with_stats(|s| s.sdf_tx += 1);
            } else {
                self.engine.with_stats(|s| s.sync_tx += 1);
            }
        } else {
            if rc == ERR_BUSY {
                hw.pkt_free(pkt);
            }
            self.engine.with_stats(|s| s.tx_errors += 1);
            self.engine.with_stats(|s| s.last_error = rc);
        }
    }
    fn tick(&mut self, hw: &Hw) {
        self.station_tick(hw,hw.now_us().unwrap_or(0));
        self.expire_watches(hw, hw.now_us().unwrap_or(0));
        if !self.radio_ready(hw) {
            let now=hw.now_us().unwrap_or(0);
            let next=self.station.as_ref().map_or(now+1_000_000,|p|p.next.max(now+1000).min(now+1_000_000));
            self.schedule(hw,next,TICK);return;
        }
        if !self.armed { self.arm(hw); return; }
        let Some(now) = hw.now_us() else {
            self.engine.with_stats(|s| s.last_error = ERR_UNSUPPORTED);
            return;
        };
        self.engine.tick(now);
        let mut plan = self.engine.plan(now);
        let hold = self
            .engine
            .stats
            .as_mut()
            .and_then(|s| s.hold_until(now, self.monitoring && !plan.capture));
        if let Some(until) = hold {
            plan.capture = true;
            plan.public = false;
            plan.sync = false;
            plan.next_us = until;
        }
        if plan.capture != self.monitoring {
            let called = hw.now_us().unwrap_or(now);
            let rc = monitor(hw, self.channel, plan.capture);
            let returned = hw.now_us().unwrap_or(now);
            if rc < 0 {
                self.engine.with_stats(|s| s.last_error = rc);
                self.schedule(hw, now + 20_000, TICK);
                return;
            }
            self.monitoring = plan.capture;
            self.engine.with_stats(|s| {
                if plan.capture {
                    s.open(called, returned, &plan);
                } else {
                    s.close(returned);
                }
            });
        }
        if self.remote_wake.as_ref().is_some_and(|w|now>=w.until) {self.clear_remote_wake(hw);}
        if self.remote_wake.as_ref().is_some_and(|w|if w.now {plan.sync}else{plan.public}) {self.remote_wake_tick(hw,now);}
        self.flush(hw, now, plan.public);
        let key = (self.engine.nan.cluster().map_or([0; 6], |m| m.0), plan.slot);
        if plan.public && self.last_public != Some(key) {
            self.last_public = Some(key);
            self.transmit(hw, now, true);
        }
        if plan.sync && self.last_sync != Some(key) {
            self.last_sync = Some(key);
            self.transmit(hw, now, false);
        }
        if let Some(p)=self.station.as_ref() {plan.next_us=plan.next_us.min(p.next.max(now+1000));}
        if let Some(expiry)=self.watches.next_expiry() {plan.next_us=plan.next_us.min(expiry);}
        if let Some(w)=self.remote_wake.as_ref() {plan.next_us=plan.next_us.min(w.until);}
        let retry = self.tx.iter().flatten().any(|tx| tx.now || plan.public);
        if self.sleep.enabled {
            let pending = self.remote_wake.is_some() || self.tx.iter().any(Option::is_some)
                || self.discovery.pending.iter().flatten().any(|p| p.until > now);
            match self.sleep.between_windows(hw, &plan, pending, now) {
                Ok(true) => {
                    // Keep a WINDOW alarm alive for HOST_WAKE, but move its
                    // deadline past sleep/resume. HOST_WAKE schedules the DW.
                    self.schedule(hw, plan.next_us, TICK);
                    return;
                }
                Ok(false) => {}
                Err(rc) => self.engine.with_stats(|s| s.last_error = rc),
            }
        }
        if !self.sleep.enabled && self.sleep_original.is_some() { self.leave_sleepy(hw); }
        self.schedule(
            hw,
            if retry {
                (now + 1000).min(plan.next_us)
            } else {
                plan.next_us
            },
            TICK,
        );
    }
    fn status(&self, hw: &Hw, out: &mut [u8]) -> usize {
        let e = &self.engine;
        let stats = e.stats.as_deref();
        let mut w = Writer::new(out);
        w.map(if stats.is_some() { 27 } else { 18 })
            .uint(1)
            .bytes(&e.local)
            .uint(2)
            .bytes(&e.nan.cluster().map_or([0; 6], |m| m.0))
            .uint(3)
            .boolean(e.nan.is_now_master())
            .uint(4)
            .uint(e.nan.last_beacon_tsf_us())
            .uint(5)
            .uint(e.nan.last_beacon_local_us())
            .uint(13)
            .boolean(self.monitoring)
            .uint(17)
            .bytes(&if e.nan.is_now_master() {
                e.local
            } else {
                e.master.unwrap_or([0; 6])
            })
            .uint(18)
            .uint(if e.nan.is_now_master() {
                1
            } else if e.master_is_beacon {
                2
            } else if e.master.is_some() {
                3
            } else {
                0
            })
            .uint(19)
            .uint(e.nan.beacon_interval_tu() as u64)
            .uint(20)
            .boolean(stats.is_some());
        let mut power = PowerInfo::default();
        if let Some(info) = hw.power().and_then(|p| p.info) { unsafe { info(hw.ctx(), &mut power); } }
        w.uint(27).uint(power.ip_sockets as u64);
        w.uint(24).boolean(self.sleep.enabled)
            .uint(25).boolean(self.sleep.waiting)
            .uint(26).uint(self.sleep.resume_lead_us);
        w.uint(21)
            .boolean(false) // retired local signature-verification flag
            .uint(22)
            .uint(e.info_len as u64)
            .uint(23).array(0); // retired inventory field; use live watch

        if let Some(s) = stats {
            w.uint(6)
                .uint(s.sync_rx as u64)
                .uint(7)
                .uint(s.sdf_rx as u64)
                .uint(8)
                .uint(s.beacons as u64)
                .uint(9)
                .uint(s.sync_tx as u64)
                .uint(10)
                .uint(s.sdf_tx as u64)
                .uint(11)
                .uint(s.tx_errors as u64)
                .uint(12)
                .int(s.last_error as i64)
                .uint(15)
                .uint(s.selected_beacons as u64)
                .uint(16)
                .uint(s.max_beacon_gap_us);
        }
        w.uint(14).array(0); // retired inventory field
        w.finish().map_or(0, |r| r.len())
    }
}
impl State {
    fn start(&mut self, hw: &Hw, data: &[u8]) -> Result<(), i32> {
        hw.now_us().ok_or(ERR_UNSUPPORTED)?;
        let channel = cbor::get_int(data, 1).ok().flatten().unwrap_or(6);
        if !(1..=13).contains(&channel) {
            return Err(ERR_ARG);
        }
        let mut device = [0; 1500];
        let n = hw.device_info(&mut device)?;
        let mac = bytes(item(&device[..n], 1).ok_or(ERR_ARG)?, 7).ok_or(ERR_UNSUPPORTED)?;
        let local = <[u8; 6]>::try_from(mac).map_err(|_| ERR_ARG)?;
        let mut original = WifiIntent::default();
        let mut previous = 0;
        let wifi = hw.wifi().ok_or(ERR_UNSUPPORTED)?;
        let get = wifi.get.ok_or(ERR_UNSUPPORTED)?;
        let rc = unsafe { get(hw.ctx(), &mut original, &mut previous) };
        if rc < 0 {
            return Err(rc);
        }
        let now_link = link(hw, c"espnow")?;
        let action_link = link(hw, c"wifi-action")?;
        let mut engine = Engine::new(local, hw.now_us().unwrap());
        let diagnostic = match cbor::get(data, 3).ok().flatten().and_then(|mut r| r.next()) {
            None => false,
            Some(Item::Bool(on)) => on,
            _ => return Err(ERR_ARG),
        };
        let wait = cbor::get_int(data, 4).ok().flatten().unwrap_or(0);
        if !(0..=512).contains(&wait) || (wait != 0 && !diagnostic) {
            return Err(ERR_ARG);
        }
        let lead = cbor::get_int(data, 5).ok().flatten().unwrap_or(8);
        if !(1..=64).contains(&lead) || (lead != 8 && !diagnostic) {
            return Err(ERR_ARG);
        }
        if diagnostic {
            if !engine.enable_stats(wait as u64 * 1000) {
                return Err(ERR_NOMEM);
            }
            engine.with_stats(|s| s.lead_us = lead as u64 * 1000);
        }
        // Supplied records are opaque. Sync/sleep work without a companion
        // publication; signing and certificate policy belong to the supplier.
        engine.set_publication(publication::start_record(data)?)?;
        let discovery = discovery::Discovery::new(engine.started_us);
        let set = wifi.set.ok_or(ERR_UNSUPPORTED)?;
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Context>())
            .cast::<Context>();
        if ptr.is_null() {
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Context {
                engine,
                original,
                generation: previous,
                channel: channel as u8,
                alarm: -1,
                now_link,
                action_link,
                monitoring: false,
                sta_mac: local, ap_mac: [0; 6], radio_paused: false,
                last_public: None,
                last_sync: None,
                armed: false,
                discovery,
                watches: watch::Watches::default(), remote_wake: None, station:None, auto_reconnect_original:None,
                tx: [None; 4],
                sleep: sleep::Policy::default(),
                sleep_original: None,
            });
        }
        self.context = ptr as usize;
        let want = match radio_intent(original, channel as u8) {
            Ok(want) => want,
            Err(rc) => {
                self.stop(hw);
                return Err(rc);
            }
        };
        let rc = unsafe { set(hw.ctx(), &want, &mut (*ptr).generation) };
        if rc < 0 {
            self.stop(hw);
            return Err(rc);
        }
        unsafe {
            (*ptr).schedule(hw, hw.now_us().unwrap() + 20_000, ACQUIRE_RETRY);
        }
        Ok(())
    }
    fn stop(&mut self, hw: &Hw) {
        if self.context == 0 {
            return;
        }
        let ptr = self.context as *mut Context;
        self.context = 0;
        let cx = unsafe { &mut *ptr };
        cx.leave_sleepy(hw);
        if let Some(original)=cx.auto_reconnect_original {
            if let Some(f)=hw.wifi().and_then(|w|w.auto_reconnect) {unsafe {f(hw.ctx(),original);}}
        }
        for tx in &mut cx.tx {
            if let Some(tx) = tx.take() {
                hw.pkt_free(tx.pkt);
            }
        }
        if cx.alarm >= 0 {
            hw.alarm_cancel(cx.alarm);
        }
        if cx.monitoring {
            monitor(hw, cx.channel, false);
        }
        if let Some(wifi) = hw.wifi() {
            let mut current = WifiIntent::default();
            let mut generation = 0;
            if let (Some(get), Some(set)) = (wifi.get, wifi.set) {
                if unsafe { get(hw.ctx(), &mut current, &mut generation) } == 0
                    && generation == cx.generation
                {
                    unsafe { set(hw.ctx(), &cx.original, &mut generation) };
                }
            }
        }
        if let Some(close) = hw.link().and_then(|l| l.close) {
            unsafe {
                close(hw.ctx(), cx.now_link as u32);
                close(hw.ctx(), cx.action_link as u32);
            }
        }
        unsafe {
            ptr.drop_in_place();
        }
        hw.mem_free(ptr.cast());
    }
}
impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_START) {
            return match self.start(hw, call.data) {
                Ok(()) => CONTINUE,
                Err(rc) => rc,
            };
        }
        if call.is_host(HOST_STOP) {
            self.stop(hw);
            return CONTINUE;
        }
        if self.context == 0 {
            return CONTINUE;
        }
        let cx = unsafe { &mut *(self.context as *mut Context) };
        if call.is_host(HOST_NETWORK) {
            if !cx.armed { cx.arm(hw); } else { cx.tick(hw); }
            return CONTINUE;
        }
        if call.is_host(HOST_WIFI) {
            if cx.sleep.enabled && call.value as u32 != cx.generation {
                // A newer explicit radio owner supersedes this policy. Never
                // restore our saved profile over that owner's new intent.
                cx.leave_sleepy(hw);
                cx.monitoring = false;
            }
            if !cx.armed { cx.arm(hw); } else { cx.tick(hw); }
            return CONTINUE;
        }
        if call.is_host(HOST_WAKE) {
            let own_boundary = cx.sleep.wake(hw, call.value);
            if own_boundary || call.value == 4 || call.value < 0 {
                // Wi-Fi shutdown removed promiscuous RX. Preserve the TSF
                // anchor, but force native receive re-arming before this DW.
                cx.monitoring = false;
            }
            if !cx.sleep.enabled && cx.sleep_original.is_some() { cx.leave_sleepy(hw); }
            cx.tick(hw);
            return CONTINUE;
        }
        if call.component == C_SELF && call.method == ACQUIRE_RETRY {
            if !cx.armed {
                cx.arm(hw);
            }
            return CONTINUE;
        }
        if call.component == C_SELF && call.method == TICK {
            cx.tick(hw);
            return CONTINUE;
        }
        // the answer of the application (APP_REPLY), or of the module the runtime's envelope dispatch (ADDR_ENVELOPE) delivered a peer's request to:
        // a message from a module for another component than this one (the request's own component and method) whose id is the token of a peer that waits
        if (call.component == C_SELF && call.method == APP_REPLY && call.from == ADDR_MAIN)
            || (call.component != C_SELF && call.component != HOST_COMPONENT as i32 && call.pkt != 0 && call.from != ADDR_MAIN && call.from < ADDR_LINK
                && call.id != 0 && cx.discovery.pending(call.id))
        {
            let now = hw.now_us().unwrap_or(0);
            let (token, payload) = if call.method == APP_REPLY {
                (
                    cbor::get(call.data, 1)
                        .ok()
                        .flatten()
                        .and_then(|mut r| match r.next() {
                            Some(Item::Uint(id)) => Some(id),
                            _ => None,
                        })
                        .unwrap_or(0),
                    bytes(call.data, 2).unwrap_or(&[]),
                )
            } else {
                (call.id, call.data)
            };
            if let Some(path) = cx.discovery.take(token, now) {
                if let Err(rc) = cx.enqueue(hw, path, payload, now) {
                    cx.engine.with_stats(|s| s.last_error = rc);
                }
                cx.tick(hw);
            }
            return CONTINUE;
        }
        // A request, direct or taken by the runtime from an envelope (`call.is_envelope()`: component, method and the fields are the envelope's, the
        // answer is wrapped by the runtime). Frames of a link are not requests.
        if call.wants_response() {
            let method = call.method;
            let mut out = [0u8; 1400];
            let n = if method == 1 {
                cx.status(hw, &mut out)
            } else if method == 2 {
                if let Some(s) = cx.engine.stats.as_deref() {
                    s.encode(&mut out)
                } else {
                    let mut w = Writer::new(&mut out);
                    w.map(1).uint(1).boolean(false);
                    w.finish().map_or(0, |r| r.len())
                }
            } else if method == 3 {
                let mut w = Writer::new(&mut out);
                w.map(1)
                    .uint(1)
                    .bytes(&cx.engine.info[..cx.engine.info_len]);
                w.finish().map_or(0, |b| b.len())
            } else if method == 4 {
                let rc = cx.active_discovery(hw, hw.now_us().unwrap_or(0));
                let mut w = Writer::new(&mut out);
                match rc {
                    Ok(id) => {
                        w.map(1).uint(1).uint(id);
                    }
                    Err(rc) => {
                        w.int(rc as i64);
                    }
                }
                w.finish().map_or(0, |b| b.len())
            } else if method == 11 {
                let fields = call.data;
                let result = bytes(fields, 1).ok_or(ERR_ARG)
                    .and_then(|record| cx.engine.set_publication(record));
                let mut w = Writer::new(&mut out);
                match result { Ok(()) => { w.map(0); }, Err(rc) => { w.int(rc as i64); } }
                w.finish().map_or(0, |b| b.len())
            } else if method == 10 {
                let fields=call.data;
                let ms=cbor::get_int(fields,1).ok().flatten().unwrap_or(-1);
                let rc=if !(0..=300_000).contains(&ms) {ERR_ARG} else {
                    hw.power().and_then(|p|p.awake_for).map_or(ERR_UNSUPPORTED,|f|unsafe {f(hw.ctx(),ms as u32)})
                };
                let mut w=Writer::new(&mut out);if rc<0 {w.int(rc as i64);}else{w.map(1).uint(1).uint(rc as u64);}
                w.finish().map_or(0,|b|b.len())
            } else if method == 8 || method == 9 {
                let fields=call.data;
                let result=(|| {
                    let via_now=match cbor::get(fields,3).ok().flatten().and_then(|mut r|r.next()) {None=>false,Some(Item::Bool(b))=>b,_=>return Err(ERR_ARG)};
                    let now=hw.now_us().unwrap_or(0);
                    if method==8 {
                        let payload=bytes(fields,1).ok_or(ERR_ARG)?;
                        let sub=match cbor::get(fields,2).ok().flatten().and_then(|mut r|r.next()) {None=>false,Some(Item::Bool(b))=>b,_=>return Err(ERR_ARG)};
                        cx.post_discovery(hw,payload,sub,via_now,now)?;Ok(0)
                    } else {
                        let target=bytes(fields,1).ok_or(ERR_ARG)?.try_into().map_err(|_|ERR_ARG)?;
                        let kind=cbor::get_int(fields,2).map_err(|_|ERR_ARG)?.unwrap_or(1);
                        cx.start_remote_wake(target,via_now,kind,now)
                    }
                })();
                let mut w=Writer::new(&mut out);match result {Ok(id)=>{w.map(1).uint(1).uint(id);},Err(rc)=>{w.int(rc as i64);}}
                w.finish().map_or(0,|b|b.len())
            } else if method == 7 {
                let fields=call.data;
                // a watch is its request: keyed by the envelope id (the runtime puts it on every event of the route)
                let id=call.request_id.unwrap_or(call.id);
                let rc=watch_duration(fields).and_then(|ms|cx.watches.register(watch::Route::Native {address:call.from,token:call.id},id,ms,hw.now_us().unwrap_or(0)));
                let mut w=Writer::new(&mut out);match rc {Ok(())=>{w.map(0);},Err(rc)=>{w.int(rc as i64);}}
                w.finish().map_or(0,|b|b.len())
            } else if method == 6 {
                let fields = call.data;
                let result = (|| {
                    let enabled = match cbor::get(fields, 1).ok().flatten().and_then(|mut r| r.next()) {
                        Some(Item::Bool(on)) => on,
                        _ => return Err(ERR_ARG),
                    };
                    let trace = match cbor::get(fields, 2).ok().flatten().and_then(|mut r| r.next()) {
                        None => false,
                        Some(Item::Bool(on)) => on,
                        _ => return Err(ERR_ARG),
                    };
                    let cadence = cbor::get_int(fields, 3).ok().flatten().unwrap_or(1);
                    if !matches!(cadence, 1 | 8 | 16) { return Err(ERR_ARG); }
                    if enabled { cx.enter_sleepy(hw, trace, cadence as u64)?; } else { cx.leave_sleepy(hw); }
                    Ok::<_, i32>(())
                })();
                let mut w = Writer::new(&mut out);
                match result { Ok(()) => { w.map(0); }, Err(rc) => { w.int(rc as i64); } }
                w.finish().map_or(0, |b| b.len())
            } else if method == 5 {
                let fields = call.data;
                let result = (|| {
                    let peer: [u8; 6] = bytes(fields, 1)
                        .ok_or(ERR_ARG)?
                        .try_into()
                        .map_err(|_| ERR_ARG)?;
                    let payload = bytes(fields, 2).ok_or(ERR_ARG)?;
                    let instance = cbor::get_int(fields, 3).ok().flatten().unwrap_or(1);
                    if !(1..=255).contains(&instance) {
                        return Err(ERR_ARG);
                    }
                    let now_link = match cbor::get(fields, 4)
                        .ok()
                        .flatten()
                        .and_then(|mut r| r.next())
                    {
                        None => false,
                        Some(Item::Bool(b)) => b,
                        _ => return Err(ERR_ARG),
                    };
                    cx.enqueue(
                        hw,
                        discovery::ReturnPath {
                            peer,
                            instance: instance as u8,
                            now: now_link,
                        },
                        payload,
                        hw.now_us().unwrap_or(0),
                    )
                })();
                let mut w = Writer::new(&mut out);
                match result {
                    Ok(()) => {
                        w.map(0);
                    }
                    Err(rc) => {
                        w.int(rc as i64);
                    }
                }
                w.finish().map_or(0, |b| b.len())
            } else {
                let mut w = Writer::new(&mut out);
                w.int(ERR_UNSUPPORTED as i64);
                w.finish().map_or(0, |r| r.len())
            };
            if (4..=10).contains(&method) {
                cx.tick(hw);
            }
            // an error is a bare negative integer: `7: error` in an envelope, wrapped by the runtime
            hw.reply(call, &out[..n]);
            return CONTINUE;
        }
        if call.flags & CALL_LINK != 0 && call.flags & CALL_LINK_EVENT == 0 {
            let mut source = [0; 6];
            for (i, b) in source.iter_mut().enumerate() {
                *b = (call.id >> (8 * i)) as u8;
            }
            let now = hw.now_us().unwrap_or(0);
            let delta = (now as u32).wrapping_sub(call.time_us) as u64;
            let at = if call.time_us != 0 && delta < 1 << 31 {
                now.saturating_sub(delta)
            } else {
                now
            };
            if cx.owns_mac(source) { return CONTINUE; }
            let before = cx.engine.nan.nan_sync_timing();
            cx.engine.receive(source, call.data, at, call.value as i8);
            cx.receive_radio_messages(hw, source, call.data, at, call.value as i8);
            if before != cx.engine.nan.nan_sync_timing() || cx.tx.iter().any(Option::is_some) {
                cx.tick(hw);
            }
        }
        CONTINUE
    }
}
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn sleepy_radio_has_no_ip_roles_and_preserves_active_credentials() {
        let mut active=WifiIntent { radio:1,sta:1,ap:1,now:1,channel:0,..Default::default() };
        active.sta_ssid_len=4; active.sta_ssid[..4].copy_from_slice(b"test");
        active.sta_psk_len=8; active.sta_psk[..8].copy_from_slice(b"password");
        let quiet=sleepy_intent(active,6);
        assert_eq!((quiet.radio,quiet.sta,quiet.ap,quiet.now,quiet.channel),(1,0,0,1,6));
        assert_eq!(quiet.sta_ssid,active.sta_ssid);
        assert_eq!(quiet.sta_psk,active.sta_psk);
        assert_eq!((active.sta,active.ap),(1,1));
    }
    #[test]
    fn shared_wifi_preserves_channel_hint_and_credentials() {
        let mut original = WifiIntent {
            radio: 1,
            sta: 1,
            now: 1,
            channel: 0,
            ..Default::default()
        };
        original.sta_ssid_len = 4;
        original.sta_ssid[..4].copy_from_slice(b"test");
        let want = radio_intent(original, 6).unwrap();
        assert_eq!(want.channel, 0);
        assert_eq!(want.sta, 1);
        assert_eq!(want.sta_ssid, original.sta_ssid);
        assert_eq!(want.sta_psk, original.sta_psk);
        original.now = 0;
        assert!(matches!(radio_intent(original, 6), Err(ERR_UNSUPPORTED)));
        let want = radio_intent(WifiIntent::default(), 6).unwrap();
        assert_eq!(
            (want.radio, want.sta, want.ap, want.now, want.channel),
            (1, 0, 0, 1, 6)
        );
    }
}

#[cfg(test)]
mod packet_lifetime_tests {
    use super::*;
    use std::{collections::BTreeMap, vec::Vec};
    #[derive(Default)]
    struct Fixture {
        next: u32,
        packets: BTreeMap<u32, Vec<u8>>,
        busy: bool,
        sent: Vec<Vec<u8>>,
        sent_methods: Vec<i32>,
        freed: usize,
        context_freed: usize,
        wifi: WifiIntent,
        generation: u32,
        wifi_sets: usize,
        reject_wifi: i32,
        actual: WifiState,
        monitor_changes: usize,
    }
    unsafe fn fixture<'a>(p: *mut core::ffi::c_void) -> &'a mut Fixture {
        &mut *p.cast()
    }
    unsafe extern "C" fn alloc(p: *mut core::ffi::c_void, out: *mut u32) -> i32 {
        let f = fixture(p);
        f.next += 1;
        f.packets.insert(f.next, std::vec![0;1500]);
        *out = f.next;
        0
    }
    unsafe extern "C" fn buf(
        p: *mut core::ffi::c_void,
        id: u32,
        out: *mut *mut u8,
        cap: *mut usize,
    ) -> i32 {
        let data = fixture(p).packets.get_mut(&id).unwrap();
        *out = data.as_mut_ptr();
        *cap = data.len();
        0
    }
    unsafe extern "C" fn free(p: *mut core::ffi::c_void, id: u32) -> i32 {
        let f = fixture(p);
        assert!(f.packets.remove(&id).is_some(), "double free");
        f.freed += 1;
        0
    }
    unsafe extern "C" fn send(
        p: *mut core::ffi::c_void,
        id: u32,
        n: usize,
        _: u32,
        _: i32,
        method: i32,
        _: u64,
    ) -> i32 {
        let f = fixture(p);
        if f.busy {
            return ERR_BUSY;
        }
        let data = f.packets.remove(&id).expect("send owns lease");
        f.sent.push(data[..n].to_vec());
        f.sent_methods.push(method);
        0
    }
    unsafe extern "C" fn release_context(
        p: *mut core::ffi::c_void,
        ptr: *mut core::ffi::c_void,
    ) -> i32 {
        fixture(p).context_freed += 1;
        std::alloc::dealloc(ptr.cast(), core::alloc::Layout::new::<Context>());
        0
    }
    unsafe extern "C" fn wifi_get(p: *mut core::ffi::c_void, out: *mut WifiIntent, generation: *mut u32) -> i32 {
        let f = fixture(p); *out = f.wifi; *generation = f.generation; 0
    }
    unsafe extern "C" fn wifi_set(p: *mut core::ffi::c_void, intent: *const WifiIntent, generation: *mut u32) -> i32 {
        let f = fixture(p); if f.reject_wifi < 0 { return f.reject_wifi; }
        f.wifi = *intent; f.generation += 1; *generation = f.generation; f.wifi_sets += 1; 0
    }
    unsafe extern "C" fn wifi_state(p: *mut core::ffi::c_void, out: *mut WifiState) -> i32 { *out=fixture(p).actual; 0 }
    unsafe extern "C" fn wifi_monitor(p: *mut core::ffi::c_void, m: *const WifiMonitor) -> i32 {
        let f=fixture(p);f.monitor_changes+=1;
        assert!((*m).enable==0 || f.actual.sta==0 || (f.actual.sta==2 && f.actual.channel==(*m).channel), "retuned an associated or joining STA");
        f.actual.monitoring=(*m).enable;0
    }
    static RADIO_WIFI: Wifi=Wifi {version:WIFI_VERSION,size:core::mem::size_of::<Wifi>() as u32,
        get:Some(wifi_get),set:Some(wifi_set),state:Some(wifi_state),scan:None,monitor:Some(wifi_monitor),auto_reconnect:None,scan_channels:None};
    unsafe extern "C" fn radio_component(_: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *const core::ffi::c_void {
        if core::ffi::CStr::from_ptr(name).to_bytes()==b"wifi" {(&RADIO_WIFI as *const Wifi).cast()}else{core::ptr::null()}
    }
    static WIFI: Wifi = Wifi { version: WIFI_VERSION, size: core::mem::size_of::<Wifi>() as u32,
        get: Some(wifi_get), set: Some(wifi_set), state: None, scan: None, monitor: None, auto_reconnect:None,scan_channels:None };
    unsafe extern "C" fn component(_: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *const core::ffi::c_void {
        if core::ffi::CStr::from_ptr(name).to_bytes() == b"wifi" { (&WIFI as *const Wifi).cast() } else { core::ptr::null() }
    }
    fn host(f: &mut Fixture) -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION;
        h.size = core::mem::size_of::<Host>() as u32;
        h.user = (f as *mut Fixture).cast();
        h.pkt_alloc = Some(alloc);
        h.pkt_buf = Some(buf);
        h.pkt_free = Some(free);
        h.send = Some(send);
        h.mem_free = Some(release_context);
        h.get_component = Some(component);
        h
    }
    fn context() -> Context {
        let mut engine = Engine::new([1; 6], 1);
        engine.tick(protocol::ACQUIRE_US + 1);
        Context {
            engine,
            original: WifiIntent::default(),
            generation: 0,
            channel: 6,
            alarm: -1,
            now_link: ADDR_LINK as i32 + 1,
            action_link: ADDR_LINK as i32 + 2,
            monitoring: false,
            sta_mac: [1;6], ap_mac: [0;6], radio_paused: false,
            last_public: None,
            last_sync: None,
            armed: true,
            discovery: discovery::Discovery::new(100),
            watches: watch::Watches::default(), remote_wake: None, station:None, auto_reconnect_original:None,
            tx: [None; 4],
            sleep: sleep::Policy::default(),
                sleep_original: None,
        }
    }
    unsafe extern "C" fn startup_now(_: *mut core::ffi::c_void) -> u64 { 1234 }
    unsafe extern "C" fn startup_info(_: *mut core::ffi::c_void, out: *mut u8, cap: usize) -> i32 {
        let mut w = Writer::new(core::slice::from_raw_parts_mut(out,cap));
        w.map(1).uint(1).map(1).uint(7).bytes(&[2,0,0,0,0,1]);
        w.finish().unwrap().len() as i32
    }
    unsafe extern "C" fn startup_alloc(_: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void {
        assert_eq!(size,core::mem::size_of::<Context>());
        std::alloc::alloc(core::alloc::Layout::new::<Context>()).cast()
    }
    unsafe extern "C" fn startup_link(_: *mut core::ffi::c_void, _: *const core::ffi::c_char, _: i32, _: i32) -> i32 { ADDR_LINK as i32 + 1 }
    static STARTUP_LINK: Link = Link {version:LINK_VERSION,size:core::mem::size_of::<Link>() as u32,open:Some(startup_link),..unsafe {core::mem::zeroed()} };
    unsafe extern "C" fn startup_component(_: *mut core::ffi::c_void, name:*const core::ffi::c_char) -> *const core::ffi::c_void {
        match core::ffi::CStr::from_ptr(name).to_bytes() {
            b"wifi" => (&WIFI as *const Wifi).cast(),
            b"link" => (&STARTUP_LINK as *const Link).cast(),
            b"identity" | b"psa" => panic!("NAN startup must not request identity or crypto"),
            _ => core::ptr::null(),
        }
    }
    #[test]
    fn startup_without_publication_or_crypto_still_sets_up_sync() {
        for data in [&[][..], &[0xa0], &[0xa1,2,0x43,b'r',b'a',b'w']] {
            let mut f = Fixture::default(); let mut h = host(&mut f);
            h.now_us=Some(startup_now);h.device_info=Some(startup_info);
            h.mem_alloc=Some(startup_alloc);h.get_component=Some(startup_component);
            let hw=unsafe {Hw::new(&h)}.unwrap();let mut state=State::default();
            state.start(&hw,data).unwrap();
            let cx=unsafe {&*(state.context as *const Context)};
            assert_eq!(&cx.engine.info[..cx.engine.info_len], publication::start_record(data).unwrap());
            assert_eq!(cx.engine.started_us,1234);assert_eq!(f.wifi.now,1);
            state.stop(&hw);assert_eq!(f.context_freed,1);assert!(f.packets.is_empty());
        }
    }

    #[test]
    fn publication_replacement_preserves_clock_sleep_watches_and_prepared_leases() {
        let mut f = Fixture::default(); let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut cx = context();
        cx.engine.set_publication(b"old").unwrap();
        cx.sleep.enabled = true;
        cx.alarm = 77;
        cx.watches.register(watch::Route::Native { address: 52, token: 19 }, 19, 5000, 0).unwrap();
        cx.enqueue(&hw, discovery::ReturnPath { peer: [2;6], instance: 1, now: false }, b"prepared", 100).unwrap();
        let lease = cx.tx[0].unwrap().pkt;
        let clock = (cx.engine.nan.cluster(), cx.engine.master, cx.engine.started_us, cx.engine.wake_dw);
        let ptr = std::boxed::Box::into_raw(std::boxed::Box::new(cx));
        let mut state = State { context: ptr as usize };
        for (fields, expected) in [
            (&[0xa1,1,0x43,b'n',b'e',b'w'][..], 0),
            (&[0xa1,1,0x61,b'x'][..], ERR_ARG),
            (&[0xa0][..], ERR_ARG),
        ] {
            // an envelope {1: 46, 2: 11, 3: id, 5: fields} as the runtime hands it over; it wraps the answer
            let call = CallView { component: C_SELF, method: 11, value: 0, flags: 1 | CALL_ENVELOPE,
                data: fields, time_us: 0, pkt: 0, from: ADDR_LINK, id: 9, request_id: None };
            state.handle(&hw, &call);
            let reply = f.sent.last().unwrap();
            if expected == 0 { assert_eq!(reply, &[0xa0]); }
            else { assert_eq!(cbor::Reader::new(reply).next().and_then(|i| i.as_int()), Some(expected as i64)); }
            let cx = unsafe { &*ptr };
            assert_eq!(&cx.engine.info[..cx.engine.info_len], b"new");
            assert_eq!((cx.engine.nan.cluster(),cx.engine.master,cx.engine.started_us,cx.engine.wake_dw),clock);
            assert!(cx.sleep.enabled); assert_eq!(cx.alarm,77);
            assert_eq!(cx.watches.next_expiry(),Some(5_000_000));
            assert_eq!(cx.tx[0].unwrap().pkt,lease);
        }
        // A direct module call uses the same method, then an empty value clears.
        let fields = [0xa1,1,0x40];
        let call = CallView { component:C_SELF, method:11, value:0, flags:1, data:&fields,
            time_us:0,pkt:0,from:52,id:19, request_id: None };
        state.handle(&hw,&call); assert_eq!(f.sent.last().unwrap(), &[0xa0]);
        assert_eq!(unsafe { &*ptr }.engine.info_len,0);
        assert!(unsafe { &*ptr }.engine.info.iter().all(|b|*b==0));
        state.stop(&hw); assert!(f.packets.is_empty());
    }

    #[test]
    fn sleepy_command_over_ppp_returns_a_success_result() {
        let mut f = Fixture::default(); let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let ptr = std::boxed::Box::into_raw(std::boxed::Box::new(context()));
        let mut state = State { context: ptr as usize };
        // {1: 46, 2: 6, 3: 42, 5: {1: false}} as the runtime hands it over: method 6, the fields; it wraps the answer in the envelope
        let data = &[0xa1, 1, 0xf4];
        let call = CallView { component: C_SELF, method: 6, value: 0,
            flags: 1 | CALL_ENVELOPE, data, time_us: 0, pkt: 0, from: ADDR_LINK, id: 9, request_id: None };
        assert_eq!(state.handle(&hw, &call), CONTINUE);
        assert_eq!(f.sent.len(), 1);
        assert_eq!(f.sent[0], [0xa0]);
        state.stop(&hw);
    }
    #[test]
    fn nan_reply_waits_for_dw_and_busy_retains_exact_prepared_lease() {
        let mut f = Fixture::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut cx = context();
        cx.enqueue(
            &hw,
            discovery::ReturnPath {
                peer: [2; 6],
                instance: 9,
                now: false,
            },
            b"reply",
            100,
        )
        .unwrap();
        let tx = cx.tx[0].unwrap();
        let ptr = f.packets[&tx.pkt].as_ptr();
        cx.flush(&hw, 101, false);
        assert_eq!(f.sent.len(), 0);
        f.busy = true;
        cx.flush(&hw, 102, true);
        assert_eq!(cx.tx[0].unwrap().pkt, tx.pkt);
        assert_eq!(f.packets[&tx.pkt].as_ptr(), ptr);
        assert_eq!(f.next, 1);
        f.busy = false;
        cx.flush(&hw, 103, true);
        assert!(cx.tx[0].is_none());
        assert!(f.packets.is_empty());
        let body = dmesh_rawnan::followup_service_info(&f.sent[0], dmesh_rawnan::DMESH_SERVICE_ID)
            .unwrap();
        assert_eq!(
            dmesh_rawnan::parse_dmesh_nan_followup(body)
                .unwrap()
                .payload,
            b"reply"
        );
        assert_eq!(f.sent[0][40], 9, "Android requestor instance");
    }
    #[test]
    fn stop_update_releases_every_queued_packet_and_cache_context_once() {
        let mut f = Fixture::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let ptr =
            unsafe { std::alloc::alloc(core::alloc::Layout::new::<Context>()).cast::<Context>() };
        assert!(!ptr.is_null());
        unsafe {
            ptr.write(context());
        }
        let cx = unsafe { &mut *ptr };
        for now in [false, true] {
            cx.enqueue(
                &hw,
                discovery::ReturnPath {
                    peer: [2; 6],
                    instance: 9,
                    now,
                },
                b"pending",
                100,
            )
            .unwrap();
        }
        let mut state = State {
            context: ptr as usize,
        };
        state.stop(&hw);
        state.stop(&hw);
        assert!(f.packets.is_empty());
        assert_eq!(f.freed, 2);
        assert_eq!(f.context_freed, 1);
        assert_eq!(state.context, 0);
    }
    #[test]
    fn transport_request_uses_common_connectionless_policy() {
        let request=[0xa4,1,1,2,4,3,0x18,45,5,0xa1,1,1];
        assert!(dmesh_server::connectionless::allowed(&request));
    }
    #[test]
    fn ordinary_handler_responses_are_not_redispatched_as_requests() {
        let mut f = Fixture::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut cx = context();
        let error = [0xa4, 1, 1, 2, 4, 3, 0x18, 44, 7, 0x21];
        assert!(cx
            .dispatch_record(
                &hw,
                discovery::ReturnPath {
                    peer: [2; 6],
                    instance: 1,
                    now: false
                },
                &error,
                100
            )
            .is_ok());
        assert_eq!(f.next, 0);
    }
    #[test]
    fn malformed_or_other_target_followups_never_create_response_work() {
        let mut f = Fixture::default();
        let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut cx = context();
        let mut frame = [0; 512];
        let mut request = [0; 32];
        let n = dmesh_server::announce::encode_discovery_request(42, &mut request).unwrap();
        let len = frames::followup(
            &mut frame,
            [2; 6],
            [3; 6],
            [0x50, 0x6f, 0x9a, 1, 2, 3],
            9,
            1,
            &request[..n],
        )
        .unwrap();
        cx.receive_radio_messages(&hw, [2; 6], &frame[..len], 100,-43);
        assert_eq!(f.next, 0);
        frame[4..10].copy_from_slice(&cx.engine.local); // outer destination alone is insufficient
        cx.receive_radio_messages(&hw, [2; 6], &frame[..len], 101,-43);
        assert_eq!(f.next, 0);
    }
    fn android_wake(target: [u8; 6], id: Option<u64>, kind: dmesh_server::control::TransportKind,
        config: dmesh_server::control::TransportConfig<'_>) -> Vec<u8> {
        let mut record = [0; 256];
        let n = dmesh_server::control::encode_request(dmesh_server::control::Request::TransportSet {
            kind, config: dmesh_server::control::TransportConfig { wake_target: Some(target), ..config },
        }, id, &mut record).unwrap();
        dmesh_rawnan::build_dmesh_followup_payload(2, 1, [9; 6], target, &record[..n]).unwrap()
    }
    fn sleepy(cx: &mut Context) {
        let mut saved = WifiIntent::default(); saved.radio = 1; saved.sta = 1;
        saved.sta_ssid_len = 4; saved.sta_ssid[..4].copy_from_slice(b"test");
        cx.sleep_original = Some(std::boxed::Box::new(saved)); cx.sleep.enabled = true;
    }
    #[test]
    fn android_type_two_with_logical_id_wakes_over_nan_and_now_without_uart() {
        use dmesh_server::control::*;
        for now in [false, true] {
            let mut f = Fixture::default(); let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
            let mut cx = context(); sleepy(&mut cx);
            let body = android_wake(cx.engine.local, None, TransportKind::Sta, TransportConfig::default());
            let frame = if now {
                dmesh_rawnan::build_now_sync_body([2;6], [2;6], 100, 512, 1, &body).unwrap()
            } else {
                dmesh_rawnan::build_nan_followup_sdf(cx.engine.local, [2;6], [0x50,0x6f,0x9a,1,2,3],
                    dmesh_rawnan::DMESH_SERVICE_ID, 9, &body)
            };
            cx.receive_radio_messages(&hw, [2;6], &frame, 100,-43);
            assert!(!cx.sleep.enabled); assert!(cx.sleep_original.is_none());
            assert_eq!(f.wifi_sets, 1); assert_eq!(f.wifi.sta, 1);
            cx.receive_radio_messages(&hw, [2;6], &frame, 101,-43);
            assert_eq!(f.wifi_sets, 1, "duplicate callbacks cannot reapply activation");
            assert_eq!(&f.wifi.sta_ssid[..4], b"test");
            assert!(cx.tx.iter().all(Option::is_none), "one-way wake must not start an ACK echo loop");
        }
    }
    #[test]
    fn android_extended_nan_activation_has_correlated_response_to_rf_peer() {
        use dmesh_server::control::*;
        let mut f=Fixture::default(); let h=host(&mut f); let hw=unsafe { Hw::new(&h) }.unwrap();
        let mut cx=context(); sleepy(&mut cx);
        let body=android_wake(cx.engine.local,Some(77),TransportKind::Nan,TransportConfig {
            ap:Some(1), now:Some(1), ble:Some(0), nan_dw_interval:Some(1), ..Default::default() });
        cx.receive_message(&hw,discovery::ReturnPath { peer:[2;6],instance:9,now:false },&body,100,-43);
        assert!(!cx.sleep.enabled); assert_eq!((f.wifi.radio,f.wifi.sta,f.wifi.ap,f.wifi.now),(1,0,1,1));
        assert_eq!(cx.engine.wake_dw,1);
        let tx=cx.tx[0].unwrap(); assert_eq!(tx.destination,mac_token([2;6]));
        let frame=&f.packets[&tx.pkt];
        let wrapper=dmesh_rawnan::parse_dmesh_nan_followup(dmesh_rawnan::followup_service_info(&frame[..tx.len],dmesh_rawnan::DMESH_SERVICE_ID).unwrap()).unwrap();
        assert_eq!(dmesh_server::tagged::decode(wrapper.payload).unwrap().id,Some(77));
        assert_eq!(item(wrapper.payload,6),Some(&[0xa0][..]));
        hw.pkt_free(tx.pkt);
    }
    #[test]
    fn wrong_target_corruption_discovery_and_failed_set_cannot_wake() {
        use dmesh_server::control::*;
        let mut f=Fixture::default(); let h=host(&mut f); let hw=unsafe { Hw::new(&h) }.unwrap();
        let mut cx=context(); sleepy(&mut cx);
        let path=discovery::ReturnPath { peer:[2;6],instance:9,now:true };
        let body=android_wake([3;6],None,TransportKind::Sta,TransportConfig::default());
        cx.receive_message(&hw,path,&body,100,-43); assert!(cx.sleep.enabled);
        let mut inner_other=body.clone(); inner_other[12..18].copy_from_slice(&cx.engine.local);
        cx.receive_message(&hw,path,&inner_other,100,-43); assert!(cx.sleep.enabled);
        let mut body=android_wake(cx.engine.local,None,TransportKind::Sta,TransportConfig::default());
        body[20]^=1; cx.receive_message(&hw,path,&body,101,-43); assert!(cx.sleep.enabled);
        let mut discovery=[0;32]; let n=dmesh_server::announce::encode_discovery_request(42,&mut discovery).unwrap();
        cx.receive_message(&hw,path,&discovery[..n],102,-43); assert!(cx.sleep.enabled);
        let body=android_wake(cx.engine.local,None,TransportKind::Sta,TransportConfig::default());
        f.reject_wifi=ERR_BUSY; cx.receive_message(&hw,path,&body,103,-43);
        assert!(cx.sleep.enabled); assert!(cx.sleep_original.is_some()); assert_eq!(f.wifi_sets,0);
        let body=android_wake(cx.engine.local,None,TransportKind::Nan,TransportConfig {ble:Some(1),..Default::default()});
        f.reject_wifi=0; cx.receive_message(&hw,path,&body,104,-43);
        assert!(cx.sleep.enabled); assert_eq!(f.wifi_sets,0);
    }

    #[test]
    fn radio_restart_rearms_and_offchannel_sta_is_never_retuned() {
        let mut f=Fixture::default();f.actual=WifiState {radio:1,sta:2,channel:6,sta_mac:[1;6],ap_mac:[2;6],..Default::default()};
        let mut h=host(&mut f);h.get_component=Some(radio_component);let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();
        cx.monitoring=true;assert!(cx.radio_ready(&hw));assert!(!cx.monitoring,"driver restart invalidates cached capture");
        assert_eq!(monitor(&hw,6,true),0);assert!(cx.radio_ready(&hw));assert!(cx.monitoring);
        f.actual.channel=11;assert!(!cx.radio_ready(&hw));assert_eq!(f.actual.channel,11);assert!(!cx.monitoring);assert!(cx.radio_paused);
        f.actual.channel=6;assert!(cx.radio_ready(&hw));assert!(!cx.armed,"new epoch must re-acquire/re-arm");
        f.actual.sta=1;assert!(!cx.radio_ready(&hw),"joining cannot be pinned by NAN");
        f.actual.radio=0;assert!(!cx.radio_ready(&hw));
    }
    #[test]
    fn both_owned_mac_aliases_accept_targeted_activation() {
        use dmesh_server::control::*;
        for target in [[1;6],[3;6]] {
            let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();
            cx.ap_mac=[3;6];sleepy(&mut cx);
            let body=android_wake(target,None,TransportKind::Sta,TransportConfig::default());
            cx.receive_message(&hw,discovery::ReturnPath {peer:[2;6],instance:9,now:true},&body,100,-43);
            assert!(!cx.sleep.enabled);assert_eq!(f.wifi_sets,1);
        }
    }
    #[test]
    fn watch_forwards_only_new_data_and_busy_never_retains_an_event() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();
        let path=discovery::ReturnPath {peer:[2;6],instance:1,now:true};
        cx.observation(&hw,path,b"before",100,-43);assert_eq!(f.next,0);
        let route=watch::Route::Native {address:ADDR_LINK+1,token:246};
        cx.watches.register(route,77,1000,101).unwrap();cx.observation(&hw,path,b"new",102,-43);
        assert_eq!(f.sent.len(),1);assert_eq!(bytes(&f.sent[0],3),Some(&b"new"[..]));
        assert_eq!(f.sent_methods[0],7|SEND_MORE,"an event keeps the request open");
        cx.expire_watches(&hw,1_000_101);assert_eq!(f.sent.len(),2);assert_eq!(f.sent_methods[1],7,"the last one ends it");
        cx.observation(&hw,path,b"after",1_000_102,-43);assert_eq!(f.sent.len(),2);
        cx.watches.register(watch::Route::Radio(path),78,1000,2_000_000).unwrap();f.busy=true;
        cx.observation(&hw,path,b"drop",2_000_001,-43);
        assert!(f.packets.is_empty());assert!(cx.tx.iter().all(Option::is_none));assert_eq!(f.freed,1);
    }

    #[test]
    fn arbitrary_discovery_content_and_android_like_remote_wake_keep_wire_shape() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();
        cx.post_discovery(&hw,b"opaque",true,false,100).unwrap();
        let tx=cx.tx[0].take().unwrap();let frame=&f.packets[&tx.pkt];
        assert_eq!(dmesh_rawnan::active_subscribe_service_info(&frame[..tx.len],dmesh_rawnan::DMESH_SERVICE_ID).unwrap().service_info,b"opaque");
        hw.pkt_free(tx.pkt);
        let id=cx.start_remote_wake([2;6],false,1,100).unwrap();
        assert_eq!(cx.start_remote_wake([3;6],false,1,100),Err(ERR_BUSY));
        cx.remote_wake_peer(&hw,discovery::ReturnPath {peer:[2;6],instance:9,now:false},&[0xa0],101);
        let tx=cx.tx[0].unwrap();let frame=&f.packets[&tx.pkt];
        let p=dmesh_rawnan::parse_dmesh_nan_followup(dmesh_rawnan::followup_service_info(&frame[..tx.len],dmesh_rawnan::DMESH_SERVICE_ID).unwrap()).unwrap();
        assert_eq!(p.msg_type,2);assert_eq!(p.target_id,[2;6]);assert_eq!(dmesh_server::tagged::decode(p.payload).unwrap().id,Some(id));
        assert_eq!(tx.owner,id);
        cx.remote_wake_tick(&hw,20_000_100);assert!(cx.remote_wake.is_none());assert!(f.packets.is_empty());
    }

}

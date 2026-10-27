//! NAN service discovery and NOW beacon-equivalent synchronization.
//! The runtime classifies radio input into the manifest's virtual NAN port.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
mod api_gen;
mod power_gen;
mod power_api;
use api_gen::Api;
mod report;
mod companion;
mod observed;
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
// The DMesh announce component's discovery stream commands (dmesh-cli's
// `discovery.active` / `discovery.nodes`): this module is the NAN/NOW-capable
// observer a NAN-less host extends its discovery through.
const DISCOVERY_COMPONENT: i32 = 6;
const DISCOVERY_NODES: i32 = 9;
const DISCOVERY_ACTIVE: i32 = 10;
const TICK: i32 = 100;
const ACQUIRE_RETRY: i32 = 101;
const CONTINUE: i32 = RET_CONTINUE as i32;
/// Runtime state reservation, including the hosted table.
pub const STATE_BYTES: usize = 128;
#[derive(Default)]
/// Runtime-owned state; HOST_START allocates the reclaimable protocol context.
pub struct State {
    hosted: api_gen::Hosted,
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
    cadence: u64,
    usb: bool,
    auto_sleep: bool,
    ble_restore: Option<u8>,
    radio_request: Option<alloc::vec::Vec<u8>>,
    radio_suspended: bool,
    usb_sleep: bool,
    reports: report::Reports,
    companion: Option<companion::Session>,
    observed: observed::Observed,
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
    // Hold the egress address across radio sleep/restart without subscribing
    // to arbitrary NOW traffic. stop closes these references.
    let rc = unsafe { f(hw.ctx(), name.as_ptr(), C_SELF, xip_abi::LINK_EGRESS) };
    if rc < 0 { Err(rc) } else { Ok(rc) }
}
// An updater's netif must not be restarted by a module's channel hint.
fn radio_intent(original: WifiIntent, channel: u8) -> Result<WifiIntent, i32> {
    if original.radio != 0 && (original.sta != 0 || original.ap != 0) {
        Ok(WifiIntent {now:1,..original})
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
        if (self.usb || serial_usb(hw)) && !self.usb_sleep { return Err(ERR_BUSY); }
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
        // Put application bearers in their coordinated low-power roles once;
        // the runtime still owns each physical radio/sleep transition.
        if let Err(rc) = self.quiet_transports(hw) {
            self.leave_sleepy(hw);
            return Err(rc);
        }
        self.engine.wake_dw = cadence;
        self.station = None;
        Ok(())
    }
    fn leave_sleepy(&mut self, hw: &Hw) {
        self.sleep.disable(hw);
        self.engine.wake_dw = protocol::WAKE_DW;
        if let Some(mode) = self.ble_restore.take() {
            if let Err(rc) = ble_mode(hw, mode) { self.engine.with_stats(|s|s.last_error=rc); }
        }
        if self.radio_suspended {
            if let Some(request) = self.radio_request.as_ref() {
                match lora_configure(hw,request) {
                    Ok(()) => self.radio_suspended=false,
                    Err(rc) => self.engine.with_stats(|s|s.last_error=rc),
                }
            }
        }
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
        let request=wake::selection(kind,config)?;
        let trace=self.sleep.enabled && self.sleep.tracing();
        self.transport(hw,&request)?;
        let generation=self.generation;
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
    fn observation(&mut self, hw: &Hw, path: discovery::ReturnPath, record: &[u8], now: u64, rssi: i8) {
        // The bounded cache is the `discovery.nodes` answer: every heard
        // transmission, keyed by radio MAC, before any fan-out.
        self.observed.note(path.peer, rssi, self.channel, record, (now / 1_000).min(u32::MAX as u64) as u32);
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
        let wrapped=dmesh_rawnan::parse_dmesh_nan_followup(payload.strip_prefix(&[0x50,0x6f,0x9a,2]).unwrap_or(payload)).is_some();
        let Some((payload, wake_only)) = wake::content(payload, self.engine.local)
            .or_else(|| wake::content(payload, self.sta_mac))
            .or_else(|| if self.ap_mac != [0;6] { wake::content(payload, self.ap_mac) } else { None }) else { return };
        // Type 2 cannot be used to smuggle an unrelated handler request.
        if wake_only && !matches!(self.wake_request(payload), Ok(Some(_))) { return; }
        if wrapped {directed_activity(hw,payload);}
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
                directed_activity(hw,payload);
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
        let at=if self.auto_sleep && self.cadence!=0 && now<16_000_000 {at.min(16_000_000)} else {at};
        let ms = at.saturating_sub(now).div_ceil(1000).min(u32::MAX as u64) as u32;
        self.alarm = hw.alarm_set(ms.max(1), 0, 0, ALARM_EXACT | ALARM_WINDOW, C_SELF, method);
        if self.alarm < 0 {
            self.engine.with_stats(|s| s.last_error = self.alarm);
        }
    }
    fn offline_report(&mut self, hw: &Hw) {
        let mut data=[0;32];
        if let Some(n)=self.reports.offline(hw.now_us().unwrap_or(0),&mut data) {self.emit_report(hw,&data[..n]);}
    }
    fn arm(&mut self, hw: &Hw) {
        // Acquisition must also advance bounded STA fallback while the radio is joining.
        self.station_tick(hw,hw.now_us().unwrap_or(0));
        // The boot deadline also applies when STA is off the NAN channel and
        // capture cannot start. Quiet mode then releases STA and restores NAN.
        self.boot_policy(hw);
        if !self.radio_ready(hw) { self.offline_report(hw); self.schedule(hw, hw.now_us().unwrap_or(0) + 1_000_000, ACQUIRE_RETRY); return; }
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
            } else {
                // A transient capture failure must not permanently stop synchronization.
                self.schedule(hw,hw.now_us().unwrap_or(0)+1_000_000,ACQUIRE_RETRY);
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
        self.boot_policy(hw);
        self.station_tick(hw,hw.now_us().unwrap_or(0));
        self.expire_watches(hw, hw.now_us().unwrap_or(0));
        if !self.radio_ready(hw) {
            self.offline_report(hw);
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
        self.reports.plan(&plan,now);
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
        let mut report_data=[0;64];
        let now_cluster=self.engine.nan.is_now_master() || (self.engine.master.is_some() && !self.engine.master_is_beacon);
        if let Some(n)=self.reports.periodic(&plan,self.engine.nan.cluster().map(|m|m.0),now_cluster,&mut report_data) {
            self.emit_report(hw,&report_data[..n]);
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
    /// The runtime calls at module start to acquire radio links and begin synchronization.
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
                cadence: boot_cadence(hw), usb: serial_usb(hw), auto_sleep: true,
                ble_restore: None, radio_request: None, radio_suspended: false, usb_sleep: usb_sleep_allowed(hw), reports: report::Reports::default(), companion: None, observed: observed::Observed::default(),
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
        // Register the method without creating a TCP listener. Providers use the same stream API.
        if let Some(listen)=hw.stream().and_then(|s|s.listen) {
            let rc=unsafe {listen(hw.ctx(),C_SELF,12)};
            if rc<0 {self.stop(hw);return Err(rc);}
            // The DMesh announce component's discovery stream commands: this
            // module is the NAN/NOW observer a NAN-less host extends through.
            // `discovery.active` (10) queues an active SD pass into the next
            // public window; `discovery.nodes` (9) streams the bounded
            // observation cache back.
            for (component,method) in [(DISCOVERY_COMPONENT,DISCOVERY_ACTIVE),(DISCOVERY_COMPONENT,DISCOVERY_NODES)] {
                let rc=unsafe {listen(hw.ctx(),component,method)};
                if rc<0 {self.stop(hw);return Err(rc);}
            }
        }
        unsafe {
            (*ptr).schedule(hw, hw.now_us().unwrap() + 20_000, ACQUIRE_RETRY);
        }
        Ok(())
    }
    /// The runtime calls before unload to cancel work, restore owned roles and free all state.
    fn stop(&mut self, hw: &Hw) {
        if self.context == 0 {
            return;
        }
        let ptr = self.context as *mut Context;
        self.context = 0;
        let cx = unsafe { &mut *ptr };
        if let Some(session)=cx.companion.take() {session.dispose(hw);}
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
fn serial(hw: &Hw) -> Option<&Serial> {
    let ptr=hw.get_component(c"serial").cast::<Serial>();
    if ptr.is_null() {None} else {
        let s=unsafe {&*ptr};
        (s.version==1 && s.size as usize>=core::mem::size_of::<Serial>()).then_some(s)
    }
}
fn serial_usb(hw: &Hw) -> bool {
    serial(hw).and_then(|s|s.usb_connected).is_some_and(|f|unsafe {f(hw.ctx())}>0)
}
fn usb_sleep_allowed(hw: &Hw) -> bool {
    serial(hw).and_then(|s|s.usb_sleep_allowed).is_some_and(|f|unsafe {f(hw.ctx())}>0)
}
fn lora(hw: &Hw) -> Result<&Lora,i32> {
    let ptr=hw.get_component(c"lora").cast::<Lora>();
    if ptr.is_null() {return Err(ERR_UNSUPPORTED);}
    let table=unsafe {&*ptr};
    if table.version!=1 || (table.size as usize)<core::mem::size_of::<Lora>() {return Err(ERR_UNSUPPORTED);}
    Ok(table)
}
fn lora_configure(hw: &Hw, record: &[u8]) -> Result<(),i32> {
    let rc=unsafe {lora(hw)?.configure.ok_or(ERR_UNSUPPORTED)?(hw.ctx(),record.as_ptr(),record.len())};
    if rc<0 {Err(rc)} else {Ok(())}
}
fn lora_suspend(hw: &Hw) -> Result<(),i32> {
    let rc=unsafe {lora(hw)?.suspend.ok_or(ERR_UNSUPPORTED)?(hw.ctx())};
    if rc<0 {Err(rc)} else {Ok(())}
}
fn directed_activity(hw: &Hw, record: &[u8]) {
    if let Some(p)=hw.power() {
        if p.size as usize>=core::mem::size_of::<Power>() {
            if let Some(f)=p.received {unsafe {f(hw.ctx(),record.as_ptr(),record.len(),1);}}
        }
    }
}
fn boot_cadence(hw: &Hw) -> u64 {
    let mut v=[0;8];let n=hw.nv(b"cadence",None,&mut v);
    if n<=0 {return 1;}
    match &v[..(n as usize).min(v.len())] {
        b"0"|b"1"=>1,b"8"=>8,b"16"=>16,b"17"=>0,_=>1,
    }
}
fn ble_mode(hw: &Hw, mode: u8) -> Result<(),i32> {
    let ptr=hw.get_component(c"ble").cast::<Ble>();
    if ptr.is_null() {return Err(ERR_UNSUPPORTED);}
    let b=unsafe {&*ptr};
    if b.version!=1 || (b.size as usize)<core::mem::size_of::<Ble>() {return Err(ERR_UNSUPPORTED);}
    let r=BleMode {mode,..Default::default()};
    let rc=unsafe {b.mode.ok_or(ERR_UNSUPPORTED)?(hw.ctx(),&r)};
    if rc<0 {Err(rc)} else {Ok(())}
}
impl Context {
    fn set_sleepy(&mut self, hw: &Hw, enabled: bool, cadence: u64, trace: bool, immediate: bool) -> Result<(),i32> {
        if enabled {
            // Only explicit sleep bypasses packet/serial activity grace. Keep
            // other modules' temporary work holds and the USB policy intact.
            if immediate {
                let f=hw.power().and_then(|p|p.activate).ok_or(ERR_UNSUPPORTED)?;
                let rc=unsafe {f(hw.ctx(),0)};if rc<0 {return Err(rc);}
            }
            self.enter_sleepy(hw,trace,cadence)?;
            self.cadence=cadence;
        } else {self.leave_sleepy(hw);}
        self.auto_sleep=enabled;
        Ok(())
    }
    fn boot_policy(&mut self, hw: &Hw) {
        // USB is also checked synchronously: a pending notification cannot admit sleep.
        let usb=serial_usb(hw);
        if usb && !self.usb_sleep {
            self.usb=true;
            if self.sleep.enabled {self.leave_sleepy(hw);}
        }
        if self.auto_sleep && (!self.usb || self.usb_sleep) && self.cadence!=0 && !self.sleep.enabled
            && hw.now_us().unwrap_or(0)>=16_000_000 {
            // Activity holds defer entry; do not clear the shared activity grace.
            let mut info=PowerInfo::default();
            if hw.power().and_then(|p|p.info).is_some_and(|f|unsafe {f(hw.ctx(),&mut info)}==0 && info.active_window!=0) {return;}
            if let Err(rc)=self.enter_sleepy(hw,false,self.cadence) {self.engine.with_stats(|s|s.last_error=rc);}
        }
    }
    fn quiet_transports(&mut self, hw: &Hw) -> Result<(),i32> {
        // No BLE service is started just to suspend it. Native status reads are allocation-free.
        if let Some(n)=hw.nimble() {
            let mut status=BleStatus::default();
            if n.status.is_some_and(|f|unsafe {f(hw.ctx(),&mut status)}==0) && status.mode==BLE_MODE_ALWAYS {
                ble_mode(hw,BLE_MODE_RENDEZVOUS)?;
                self.ble_restore=Some(status.mode);
            }
        }
        if self.radio_request.is_some() && !self.radio_suspended {
            lora_suspend(hw)?;
            self.radio_suspended=true;
        }
        Ok(())
    }
    fn transport(&mut self, hw: &Hw, request: &NanTransport) -> Result<(),i32> {
        request.validate()?;
        let wifi=hw.wifi().ok_or(ERR_UNSUPPORTED)?;
        let (get,set)=(wifi.get.ok_or(ERR_UNSUPPORTED)?,wifi.set.ok_or(ERR_UNSUPPORTED)?);
        let mut current=WifiIntent::default();let mut generation=0;
        let rc=unsafe {get(hw.ctx(),&mut current,&mut generation)};if rc<0 {return Err(rc);}
        let saved=if generation==self.generation {self.sleep_original.as_deref().copied().unwrap_or(current)} else {current};
        let mut intent=saved;
        // Wi-Fi roles are independent patches; synchronization remains available.
        intent.radio=1;intent.now=1;
        if request.sta!=255 {intent.sta=request.sta;}
        if intent.sta!=0 && intent.sta_ssid_len==0 && intent.bssid==[0;6] {return Err(ERR_ARG);}
        if request.ap!=255 {intent.ap=request.ap;}
        if request.now!=255 {intent.now=u8::from(request.now!=2 || matches!(request.cadence,1|8|16));}
        // Sync remains the control plane even when NOW application traffic is disabled.
        if intent.now==0 {return Err(ERR_UNSUPPORTED);}
        let uart_baud=if request.uart_baud!=0 {request.uart_baud} else {match request.uart {0|1=>115200,6=>230400,7=>460800,9=>921600,255|8=>0,_=>return Err(ERR_UNSUPPORTED)}};
        let serial=if request.uart!=255 || uart_baud!=0 {Some(serial(hw).ok_or(ERR_UNSUPPORTED)?)} else {None};
        if let Some(s)=serial {if request.uart!=255 && s.mode.is_none() || uart_baud!=0 && s.baud.is_none() {return Err(ERR_UNSUPPORTED);}}
        let radio=if request.radio_mode==1 || request.radio_mode==2 {
            let mut data=alloc::vec![0;128];let mut w=Writer::new(&mut data);
            let count=1+u64::from(request.radio_frequency!=0)+u64::from(request.radio_bandwidth!=0)
                +u64::from(request.radio_sf!=0)+u64::from(request.radio_cr!=0)+u64::from(request.radio_power!=127);
            w.map(count as usize).uint(0).text(if request.radio_mode==1 {"rx"}else{"fsk"});
            if request.radio_frequency!=0 {w.uint(2).uint(request.radio_frequency as u64);}
            if request.radio_bandwidth!=0 {w.uint(3).uint(request.radio_bandwidth as u64);}
            if request.radio_sf!=0 {w.uint(4).uint(request.radio_sf as u64);}
            if request.radio_cr!=0 {w.uint(25).uint(request.radio_cr as u64);}
            if request.radio_power!=127 {w.uint(7).int(request.radio_power as i64);}
            let len=w.finish().ok_or(ERR_SIZE)?.len();data.truncate(len);Some(data)
        } else {None};
        // Validate first; failures remain errors, never a successful intent ACK.
        if request.ble!=255 && request.ble!=0 {
            let ptr=hw.get_component(c"ble").cast::<Ble>();
            if ptr.is_null() {return Err(ERR_UNSUPPORTED);}
            let ble=unsafe {&*ptr};
            if ble.version!=1 || (ble.size as usize)<core::mem::size_of::<Ble>() {return Err(ERR_UNSUPPORTED);}
            let rc=if request.ble==2 {unsafe {ble.stop.ok_or(ERR_UNSUPPORTED)?(hw.ctx())}}
                else {unsafe {ble.start.ok_or(ERR_UNSUPPORTED)?(hw.ctx(),&BleStart::default())}};
            if rc<0 {return Err(rc);}
        }
        if let Some(s)=serial {
            if uart_baud!=0 {let rc=unsafe {s.baud.unwrap()(hw.ctx(),uart_baud)};if rc<0 {return Err(rc);}}
            if request.uart!=255 {let rc=unsafe {s.mode.unwrap()(hw.ctx(),u32::from(request.uart!=8))};if rc<0 {return Err(rc);}}
        }
        if request.radio_mode!=255 {
            if let Some(data)=radio.as_ref() {lora_configure(hw,data)?;}
            else {
                if self.radio_request.is_some() {lora_suspend(hw)?;}
                let rc=hw.module_stop(b"lora");if rc<0 && rc!=ERR_NOTFOUND {return Err(rc);}
            }
            self.radio_request=radio;self.radio_suspended=false;
        }
        let rc=unsafe {set(hw.ctx(),&intent,&mut generation)};if rc<0 {return Err(rc);}
        if request.ble!=255 {self.ble_restore=None;}
        self.usb_sleep=usb_sleep_allowed(hw);
        self.usb=serial_usb(hw);
        self.sleep_original=None;
        self.generation=generation;
        let cadence=if request.cadence==255 {self.engine.wake_dw} else if request.cadence==0 {protocol::WAKE_DW} else {request.cadence as u64};
        self.leave_sleepy(hw);
        self.generation=generation;self.station=None;self.monitoring=false;
        self.engine.wake_dw=cadence;
        self.auto_sleep=true;
        Ok(())
    }
}
impl Api for State {
    /// Runtime USB transitions preserve sync and restore active transports on attachment.
    fn usb(&mut self, hw: &Hw, connected: u32) -> Result<(),i32> {
        if connected>1 {return Err(ERR_ARG);}
        if self.context==0 {return Err(ERR_NOTFOUND);}
        let cx=unsafe {&mut *(self.context as *mut Context)};
        cx.usb=connected!=0;
        if cx.usb && !cx.usb_sleep {cx.leave_sleepy(hw);} else {cx.auto_sleep=true;}
        cx.tick(hw);Ok(())
    }
    /// Runtime sleep completion/UART activity re-arms capture and restores active roles.
    fn wake(&mut self, hw: &Hw, cause: i32) -> Result<(),i32> {
        if self.context==0 {return Err(ERR_NOTFOUND);}
        let cx=unsafe {&mut *(self.context as *mut Context)};
        let boundary=cx.sleep.wake(hw,cause);
        if boundary {
            let mut info=PowerInfo::default();
            if let Some(f)=hw.power().and_then(|p|p.info) {unsafe {f(hw.ctx(),&mut info);}}
            cx.reports.wake(hw.now_us().unwrap_or(0).saturating_sub(info.last_resume_us as u64));
        }
        if boundary || cause==4 || cause<0 {cx.monitoring=false;}
        // UART opens a bounded runtime hold; persisted cadence resumes after it expires.
        if !cx.sleep.enabled && cx.sleep_original.is_some() {cx.leave_sleepy(hw);}
        cx.tick(hw);Ok(())
    }
    /// Local callers choose coordinated sleep; the remote sleepy request has the same semantics.
    fn sleepy(&mut self, hw: &Hw, enabled: u32, cadence: u32, trace: u32, immediate: u32) -> Result<(),i32> {
        if enabled>1 || trace>1 || immediate>1 || !matches!(cadence,1|8|16) {return Err(ERR_ARG);}
        if self.context==0 {return Err(ERR_NOTFOUND);}
        let cx=unsafe {&mut *(self.context as *mut Context)};
        cx.set_sleepy(hw,enabled!=0,cadence as u64,trace!=0,immediate!=0)?;
        cx.tick(hw);Ok(())
    }
    /// Publication suppliers replace opaque bytes without a handler message/ACK round trip.
    fn set_announce(&mut self, _hw: &Hw, record: *const u8, len: usize) -> Result<(),i32> {
        if self.context==0 {return Err(ERR_NOTFOUND);}
        if len>255 || (len!=0 && record.is_null()) {return Err(ERR_ARG);}
        let data=if len==0 {&[]} else {unsafe {core::slice::from_raw_parts(record,len)}};
        unsafe {&mut *(self.context as *mut Context)}.engine.set_publication(data)
    }
    /// Direct wire requests and local native callers select managed transports through NAN policy.
    fn transport_set(&mut self, hw: &Hw, request: *const NanTransport) -> Result<(),i32> {
        if request.is_null() {return Err(ERR_ARG);}
        if self.context==0 {return Err(ERR_NOTFOUND);}
        let cx=unsafe {&mut *(self.context as *mut Context)};
        cx.transport(hw,unsafe {&*request})?;cx.tick(hw);Ok(())
    }
}
impl Context {
    fn emit_report(&mut self,hw:&Hw,record:&[u8]) {
        if let Some(session)=self.companion.as_ref() {
            if let Err(rc)=session.report(hw,record) {
                if !matches!(rc,ERR_BUSY|ERR_AGAIN|ERR_NOMEM) {
                    if let Some(session)=self.companion.take() {session.dispose(hw);}
                }
            }
        } else {report::emit(hw,record);}
    }
}
impl Module for State {
    /// The runtime delivers transport-neutral stream events or ordinary module calls.
    fn handle(&mut self,hw:&Hw,call:&CallView)->i32 {
        if call.flags & CALL_STREAM!=0 {return self.companion_event(hw,call);}
        self.handle_event(hw,call)
    }
}
impl State {
    fn reply_to(&mut self,hw:&Hw,call:&CallView,body:&[u8])->i32 {
        hw.reply(call,body);CONTINUE
    }
    fn companion_event(&mut self,hw:&Hw,call:&CallView)->i32 {
        if self.context==0 {return ERR_NOTFOUND;}
        // The discovery stream commands answer one bounded response per open:
        // `discovery.active` queues a NAN active SD pass into the next public
        // window (DW0/DW8; sleepy peers report there — the TX scheduler owns
        // the wait) and closes; `discovery.nodes` streams the bounded
        // observation cache. Neither holds a session.
        if call.component==DISCOVERY_COMPONENT {
            if !matches!(call.method,DISCOVERY_ACTIVE|DISCOVERY_NODES) {return ERR_UNSUPPORTED;}
            if call.value!=STREAM_OPEN {return 0;}
            let cx=unsafe {&mut *(self.context as *mut Context)};
            // One bounded response carrying the FIN flag — mod_flash's
            // upload-reply pattern. The FIN ends the stream, so no explicit
            // close here: a close with the output still pending drops it.
            let Some(write)=hw.stream().and_then(|s|s.write_pkt) else {return ERR_UNSUPPORTED;};
            let mut out=[0;1600];
            let body: &[u8] = if call.method==DISCOVERY_ACTIVE {
                // Admission only queues the SDF; RF delivery is the window's.
                if let Err(rc)=cx.active_discovery(hw,hw.now_us().unwrap_or(0)) {return rc;}
                &[0xa0]
            } else {
                let Some(n)=cx.observed.nodes_response(call.id as u64,&mut out) else {return ERR_SIZE;};
                &out[..n]
            };
            let packet=match hw.pkt_alloc() {Ok(p)=>p,Err(rc)=>return rc};
            let rc=(|| {
                let (p,cap)=hw.pkt_buf(packet)?;
                if body.len()>cap {return Err(ERR_SIZE);}
                unsafe {core::ptr::copy_nonoverlapping(body.as_ptr(),p,body.len());}
                let rc=unsafe {write(hw.ctx(),call.id as u32,packet,body.len() as u32,1)};
                if rc<0 {Err(rc)} else {Ok(())}
            })();
            if rc.is_err() {hw.pkt_free(packet);}
            return rc.err().unwrap_or(0);
        }
        if call.component!=C_SELF || call.method!=12 {return ERR_UNSUPPORTED;}
        let cx=unsafe {&mut *(self.context as *mut Context)};
        match call.value {
            STREAM_OPEN=>{
                if cx.companion.is_some() {return ERR_BUSY;}
                cx.companion=Some(companion::Session::new(call.id as u32));
                let mut snapshot=[0;64];
                let now=cx.engine.nan.is_now_master() || (cx.engine.master.is_some() && !cx.engine.master_is_beacon);
                if let Some(n)=cx.reports.snapshot(cx.engine.nan.cluster().map(|m|m.0),now,&mut snapshot) {
                    cx.emit_report(hw,&snapshot[..n]);
                }
                0
            },
            event=>{
                if cx.companion.as_ref().is_none_or(|s|s.handle!=call.id as u32) {return ERR_NOTFOUND;}
                match event {
                    // A watch has no body. Peer FIN only closes its sending half.
                    STREAM_FIN=>0,
                    STREAM_CLOSED=>{cx.companion=None;0},
                    STREAM_DATA if call.data.is_empty()=>0,
                    STREAM_DATA=>{
                        if let Some(session)=cx.companion.take() {session.dispose(hw);}
                        ERR_UNSUPPORTED
                    },
                    _=>ERR_ARG,
                }
            }
        }
    }
}
impl State {
    /// Lifecycle, datagram control and synchronization policy; stream events are handled above.
    fn handle_event(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_TABLE) {
            if call.data==b"nan" {
                api_gen::provide::<State>(hw,call,core::ptr::addr_of_mut!(self.hosted),self as *mut State);
            }
            return CONTINUE;
        }
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
        if call.component == C_SELF && call.wants_response() && matches!(call.method,13..=15) {
            let mut out = [0u8; 256];
            match power_gen::dispatch(self, hw, call.method, call.data, &mut out) {
                Ok(n) => return self.reply_to(hw,call,&out[..n]),
                Err(rc) => {let mut out=[0;16];let mut w=Writer::new(&mut out);w.int(rc as i64);return self.reply_to(hw,call,w.finish().unwrap());}
            }
        }
        if self.context == 0 {
            return CONTINUE;
        }
        // Runtime routes component 1 directly here; other callers can use the
        // hosted native method. Both entries share the same NAN policy.
        if call.is_envelope() && call.component == 1 {
            let result = if call.method == 4 {
                NanTransport::decode(call.data).and_then(|request| self.transport_set(hw, &request))
            } else { Err(ERR_UNSUPPORTED) };
            match result {
                Ok(()) => return self.reply_to(hw,call,&[0xa0]),
                Err(rc) => {let mut out=[0;16];let mut w=Writer::new(&mut out);w.int(rc as i64);return self.reply_to(hw,call,w.finish().unwrap());}
            }
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
            let _=self.wake(hw,call.value);
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
                // Clients query synchronization, sleep and driver state.
                cx.status(hw, &mut out)
            } else if method == 2 {
                // Developers query optional timing diagnostics.
                if let Some(s) = cx.engine.stats.as_deref() {
                    s.encode(&mut out)
                } else {
                    let mut w = Writer::new(&mut out);
                    w.map(1).uint(1).boolean(false);
                    w.finish().map_or(0, |r| r.len())
                }
            } else if method == 3 {
                // Suppliers and clients read the current opaque publication.
                let mut w = Writer::new(&mut out);
                w.map(1)
                    .uint(1)
                    .bytes(&cx.engine.info[..cx.engine.info_len]);
                w.finish().map_or(0, |b| b.len())
            } else if method == 4 {
                // Discovery clients request one active subscription transmission.
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
            } else if method == 12 {
                // Operators query the current report; passive reporting does not issue requests.
                let now_cluster=cx.engine.nan.is_now_master() || (cx.engine.master.is_some() && !cx.engine.master_is_beacon);
                cx.reports.snapshot(cx.engine.nan.cluster().map(|m|m.0),now_cluster,&mut out).unwrap_or(0)
            } else if method == 11 {
                let fields = call.data;
                // Companion/module suppliers replace the cache without restarting sync.
                let result = bytes(fields, 1).ok_or(ERR_ARG)
                    .and_then(|record| cx.engine.set_publication(record));
                let mut w = Writer::new(&mut out);
                match result { Ok(()) => { w.map(0); }, Err(rc) => { w.int(rc as i64); } }
                w.finish().map_or(0, |b| b.len())
            } else if method == 10 {
                // Clients extend or query a temporary shared awake hold.
                let fields=call.data;
                let ms=cbor::get_int(fields,1).ok().flatten().unwrap_or(-1);
                let rc=if !(0..=300_000).contains(&ms) {ERR_ARG} else {
                    hw.power().and_then(|p|p.awake_for).map_or(ERR_UNSUPPORTED,|f|unsafe {f(hw.ctx(),ms as u32)})
                };
                let mut w=Writer::new(&mut out);if rc<0 {w.int(rc as i64);}else{w.map(1).uint(1).uint(rc as u64);}
                w.finish().map_or(0,|b|b.len())
            } else if method == 8 || method == 9 {
                // Applications post discovery content (8) or request targeted remote activation (9).
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
                // Discovery clients register or cancel contextful observation streams.
                let fields=call.data;
                // a watch is its request: keyed by the envelope id (the runtime puts it on every event of the route)
                let id=call.request_id.unwrap_or(call.id);
                let rc=watch_duration(fields).and_then(|ms|cx.watches.register(watch::Route::Native {address:call.from,token:call.id},id,ms,hw.now_us().unwrap_or(0)));
                let mut w=Writer::new(&mut out);match rc {Ok(())=>{w.map(0);},Err(rc)=>{w.int(rc as i64);}}
                w.finish().map_or(0,|b|b.len())
            } else if method == 6 {
                // Operators explicitly arm or leave coordinated sleep.
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
                    let cadence = cbor::get_int(fields, 3).map_err(|_|ERR_ARG)?.unwrap_or(boot_cadence(hw).max(1) as i64);
                    if !matches!(cadence, 1 | 8 | 16) { return Err(ERR_ARG); }
                    let immediate=match cbor::get(fields,4).map_err(|_|ERR_ARG)?.and_then(|mut r|r.next()) {
                        None=>false,Some(Item::Bool(on))=>on,_=>return Err(ERR_ARG),
                    };
                    cx.set_sleepy(hw,enabled,cadence as u64,trace,immediate)?;
                    Ok::<_, i32>(())
                })();
                let mut w = Writer::new(&mut out);
                match result { Ok(()) => { w.map(0); }, Err(rc) => { w.int(rc as i64); } }
                w.finish().map_or(0, |b| b.len())
            } else if method == 5 {
                // Applications queue an opaque record for a peer.
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
            return self.reply_to(hw,call,&out[..n]);
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
            cx.reports.received(at,before!=cx.engine.nan.nan_sync_timing());
            report::forward_to(source,call.data,call.value as i8,&mut |record|cx.emit_report(hw,record));
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
        assert_eq!(radio_intent(original,6).unwrap().now,1);
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
        report_outputs: bool,
        coc: bool,
        sent_links: Vec<u32>,
        sent_tokens: Vec<u64>,
        freed: usize,
        context_freed: usize,
        wifi: WifiIntent,
        generation: u32,
        wifi_sets: usize,
        reject_wifi: i32,
        actual: WifiState,
        monitor_changes: usize,
        usb: bool,
        now_us: u64,
        nv_cadence: Vec<u8>,
        active_window: bool,
        cancelled: usize,
        serial_mode: u32,
        module_starts: Vec<Vec<u8>>,
        module_stops: usize,
        module_unloads: usize,
        power_sleeps: Vec<(u32,u16,u16)>,
        power_debug: Vec<u8>,
        power_error: i32,
        stream_attempts: Vec<(u32,usize)>,
        stream_closes: Vec<u32>,
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
        destination: u32,
        _: i32,
        method: i32,
        token: u64,
    ) -> i32 {
        let f = fixture(p);
        if f.busy {
            return ERR_BUSY;
        }
        let data = f.packets.remove(&id).expect("send owns lease");
        f.sent.push(data[..n].to_vec());
        f.sent_methods.push(method);f.sent_links.push(destination);f.sent_tokens.push(token);
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
    unsafe extern "C" fn component(p: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *const core::ffi::c_void {
        match core::ffi::CStr::from_ptr(name).to_bytes() {
            b"stream" => (&COMPANION_STREAM as *const Stream).cast(),
            b"wifi" => (&WIFI as *const Wifi).cast(),
            b"serial" => (&SERIAL as *const Serial).cast(),
            b"power" => (&POWER as *const Power).cast(),
            b"lora" => (&LORA as *const Lora).cast(),
            b"link" if fixture(p).report_outputs => (&REPORT_LINK as *const Link).cast(),
            b"nimble" if fixture(p).report_outputs => (&REPORT_BLE as *const Nimble).cast(),
            _ => core::ptr::null(),
        }
    }
    unsafe extern "C" fn companion_write(p:*mut core::ffi::c_void,handle:u32,packet:u32,n:u32,fin:u32)->i32 {
        assert_eq!(fin,0);let f=fixture(p);let ptr=f.packets.get(&packet).unwrap().as_ptr() as usize;
        f.stream_attempts.push((packet,ptr));send(p,packet,n as usize,handle,46,12,0)
    }
    unsafe extern "C" fn companion_close(p:*mut core::ffi::c_void,handle:u32)->i32 {fixture(p).stream_closes.push(handle);0}
    unsafe extern "C" fn companion_listen(_: *mut core::ffi::c_void,component:i32,method:i32)->i32 {
        // The report stream plus the discovery stream commands (component 6).
        assert!(matches!((component,method),(46,12)|(6,10)|(6,9)));0
    }
    static COMPANION_STREAM:Stream=Stream {version:STREAM_VERSION,size:core::mem::size_of::<Stream>() as u32,
        listen:Some(companion_listen),write_pkt:Some(companion_write),close:Some(companion_close),..unsafe{core::mem::zeroed()}};
    unsafe extern "C" fn report_open(_: *mut core::ffi::c_void, name: *const core::ffi::c_char, _: i32, _: i32) -> i32 {
        ADDR_LINK as i32+if core::ffi::CStr::from_ptr(name).to_bytes()==b"ble" {20}else{21}
    }
    unsafe extern "C" fn report_close(_: *mut core::ffi::c_void, _: u32) -> i32 {0}
    unsafe extern "C" fn report_ble(p: *mut core::ffi::c_void, out: *mut BleStatus) -> i32 {(*out).coc_connected=u8::from(fixture(p).coc);0}
    static REPORT_LINK: Link=Link {version:LINK_VERSION,size:core::mem::size_of::<Link>() as u32,
        open:Some(report_open),close:Some(report_close),..unsafe {core::mem::zeroed()}};
    static REPORT_BLE: Nimble=Nimble {version:1,size:core::mem::size_of::<Nimble>() as u32,
        status:Some(report_ble),..unsafe {core::mem::zeroed()}};
    unsafe extern "C" fn usb(p: *mut core::ffi::c_void) -> i32 {fixture(p).usb as i32}
    unsafe extern "C" fn mode(p: *mut core::ffi::c_void, mode: u32) -> i32 {fixture(p).serial_mode=mode;0}
    unsafe extern "C" fn info(p: *mut core::ffi::c_void, out: *mut PowerInfo) -> i32 {let f=fixture(p);(*out).active_window=f.active_window as u8;(*out).sleep_entries=42;(*out).last_duration_us=4000000;f.power_error}
    unsafe extern "C" fn activate(p: *mut core::ffi::c_void, ms: u32) -> i32 {fixture(p).active_window=ms!=0;0}
    unsafe extern "C" fn cancel(p: *mut core::ffi::c_void) -> i32 {fixture(p).cancelled+=1;0}
    unsafe extern "C" fn until(_: *mut core::ffi::c_void, _: u64, _: u32) -> i32 {0}
    unsafe extern "C" fn policy_now(p: *mut core::ffi::c_void) -> u64 {fixture(p).now_us}
    unsafe extern "C" fn policy_nv(p: *mut core::ffi::c_void, key: *const u8, n: usize, _: *const u8, _: usize, out: *mut u8, cap: usize) -> i32 {
        assert_eq!(core::slice::from_raw_parts(key,n),b"cadence");
        let data=&fixture(p).nv_cadence;
        if data.len()>cap {return ERR_SIZE;}
        core::ptr::copy_nonoverlapping(data.as_ptr(),out,data.len());data.len() as i32
    }
    unsafe extern "C" fn module_start(p: *mut core::ffi::c_void, _: *const u8, _: usize, data: *const u8, n: usize) -> i32 {
        fixture(p).module_starts.push(core::slice::from_raw_parts(data,n).to_vec());0
    }
    unsafe extern "C" fn module_stop(p: *mut core::ffi::c_void, _: *const u8, _: usize) -> i32 {fixture(p).module_unloads+=1;0}
    unsafe extern "C" fn radio_configure(p: *mut core::ffi::c_void, data: *const u8, n: usize) -> i32 {
        fixture(p).module_starts.push(core::slice::from_raw_parts(data,n).to_vec());0
    }
    unsafe extern "C" fn radio_suspend(p: *mut core::ffi::c_void) -> i32 {fixture(p).module_stops+=1;0}
    static LORA: Lora=Lora {version:1,size:core::mem::size_of::<Lora>() as u32,configure:Some(radio_configure),suspend:Some(radio_suspend)};
    static SERIAL: Serial=Serial {version:1,size:core::mem::size_of::<Serial>() as u32,usb_connected:Some(usb),mode:Some(mode),baud:None,usb_sleep_allowed:None};
    unsafe extern "C" fn power_sleep(p: *mut core::ffi::c_void, req: *const PowerSleep) -> i32 {
        assert_eq!((*req).size as usize,core::mem::size_of::<PowerSleep>());
        let f=fixture(p);f.power_sleeps.push(((*req).duration_ms,(*req).cycles,(*req).awake_ms));f.power_error
    }
    unsafe extern "C" fn locks(p: *mut core::ffi::c_void) -> i32 {let f=fixture(p);f.power_debug.push(3);f.power_error}
    unsafe extern "C" fn tasks(p: *mut core::ffi::c_void) -> i32 {let f=fixture(p);f.power_debug.push(4);f.power_error}
    static POWER: Power=Power {version:POWER_VERSION,size:core::mem::size_of::<Power>() as u32,
        sleep:Some(power_sleep),info:Some(info),dump_locks:Some(locks),dump_tasks:Some(tasks),sleep_until:Some(until),cancel:Some(cancel),activate:Some(activate),awake_for:None,received:None};
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
        h.now_us=Some(policy_now);h.nv=Some(policy_nv);
        h.module_start=Some(module_start);h.module_stop=Some(module_stop);
        h
    }
    fn companion_state()->State {
        let p=unsafe{std::alloc::alloc(core::alloc::Layout::new::<Context>()).cast::<Context>()};
        assert!(!p.is_null());unsafe{p.write(context());}
        State{context:p as usize,..Default::default()}
    }
    fn companion_call(value:i32,data:&[u8])->CallView<'_>{
        CallView{component:46,method:12,value,flags:CALL_STREAM,
            data,time_us:0,pkt:0,from:48,id:77,request_id:Some(11), src: None}
    }
    #[test]
    fn report_stream_sends_bodies_and_keeps_output_after_peer_fin(){
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe{Hw::new(&h)}.unwrap();let mut state=companion_state();
        assert_eq!(state.handle(&hw,&companion_call(STREAM_OPEN,&[])),0);
        assert_eq!(f.sent.len(),1);assert_eq!(cbor::get_int(&f.sent[0],1).unwrap(),Some(0));
        assert_eq!(state.handle(&hw,&companion_call(STREAM_FIN,&[])),0);
        let cx=unsafe{&mut*(state.context as *mut Context)};
        cx.emit_report(&hw,&[0xa2,1,0,3,9]);assert_eq!(f.sent.last().unwrap(),&[0xa2,1,0,3,9]);
        assert!(f.stream_closes.is_empty());assert!(f.packets.is_empty());
        state.handle(&hw,&companion_call(STREAM_CLOSED,&[]));
        assert!(unsafe{&*(state.context as *mut Context)}.companion.is_none());
        state.stop(&hw);assert!(f.packets.is_empty());
    }
    #[test]
    fn report_stream_drops_busy_observations_without_retry_or_backlog(){
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe{Hw::new(&h)}.unwrap();let mut state=companion_state();
        state.handle(&hw,&companion_call(STREAM_OPEN,&[]));f.busy=true;
        let cx=unsafe{&mut*(state.context as *mut Context)};
        cx.emit_report(&hw,&[0xa2,1,0,3,9]);assert!(f.packets.is_empty());assert_eq!(f.sent.len(),1);
        f.busy=false;cx.emit_report(&hw,&[0xa2,1,0,3,10]);
        assert_eq!(f.sent.len(),2);assert_eq!(f.sent.last().unwrap(),&[0xa2,1,0,3,10]);
        state.stop(&hw);assert_eq!(f.stream_closes,[77]);assert!(f.packets.is_empty());
    }
    #[test]
    fn report_stream_rejects_commands_without_allocating_input(){
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe{Hw::new(&h)}.unwrap();let mut state=companion_state();
        state.handle(&hw,&companion_call(STREAM_OPEN,&[]));let allocated=f.next;
        assert_eq!(state.handle(&hw,&companion_call(STREAM_DATA,&[0xa0])),ERR_UNSUPPORTED);
        assert_eq!(f.next,allocated);assert_eq!(f.stream_closes,[77]);assert!(f.packets.is_empty());
        state.stop(&hw);
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
            cadence: 0, usb: false, auto_sleep: true, ble_restore: None,
            radio_request: None, radio_suspended: false, usb_sleep: false, reports: report::Reports::default(), companion: None, observed: observed::Observed::default(),
        }
    }
    #[test]
    fn reports_prefer_connected_coc_and_never_vote_for_activity() {
        let record=[0xa2,1,0,5,0xf5];
        for coc in [false,true] {
            let mut f=Fixture {report_outputs:true,coc,..Default::default()};
            let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
            report::emit(&hw,&record);assert_eq!(f.sent.len(),1);
            assert!(!f.active_window);
            assert_eq!(f.sent_links[0],ADDR_LINK+if coc {20}else{21});
            if coc {
                assert_eq!(&f.sent[0][..7],&[0xc0,0x44,0x4d,0,2,0,0]);
                let env=&f.sent[0][7..];
                assert_eq!(cbor::get_int(env,2).unwrap(),Some(12));
                assert_eq!(item(env,6).unwrap(),record);
            } else {assert_eq!(f.sent[0],record);assert_eq!(f.sent_tokens[0],PORT_NAN_REPORT as u64);}
        }
    }
    #[test]
    fn busy_report_output_drops_without_retaining_a_pool_lease() {
        let mut f=Fixture {report_outputs:true,busy:true,..Default::default()};
        let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        report::emit(&hw,&[0xa1,1,0]);
        assert!(f.sent.is_empty());assert!(f.packets.is_empty());assert_eq!(f.freed,1);
    }
    #[test]
    fn forwarded_pub_sub_and_followups_preserve_payload_and_signed_signal() {
        let mut f=Fixture {report_outputs:true,..Default::default()};
        let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        for (control,kind) in [(0x10,1),(0x11,2),(0x12,3)] {
            let mut frame=[0;512];
            let n=frames::sdf(&mut frame,[2;6],[0xff;6],[0x50,0x6f,0x9a,1,2,3],1,0,control,b"opaque").unwrap();
            report::forward(&hw,[2;6],&frame[..n],-53);
            let record=f.sent.last().unwrap();
            assert_eq!(cbor::get_int(record,1).unwrap(),Some(kind));
            assert_eq!(cbor::get_int(record,8).unwrap(),Some(-53));
            assert_eq!(bytes(record,7).unwrap(),b"opaque");
        }
        assert_eq!(f.sent.len(),3);assert!(!f.active_window);
    }
    #[test]
    fn boot_policy_has_grace_and_usb_veto_without_losing_sync() {
        for cadence in [0,1,8,16] {
            let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
            let mut cx=context();cx.cadence=cadence;
            f.now_us=15_999_999;cx.boot_policy(&hw);assert!(!cx.sleep.enabled);
            f.now_us=16_000_000;f.usb=true;cx.boot_policy(&hw);assert!(!cx.sleep.enabled);
            f.usb=false;cx.usb=false;cx.boot_policy(&hw);
            assert_eq!(cx.sleep.enabled,cadence!=0);
            if cadence!=0 {assert_eq!(cx.engine.wake_dw,cadence);}
            let anchor=cx.engine.started_us;
            f.usb=true;cx.boot_policy(&hw);assert!(!cx.sleep.enabled);
            assert_eq!(cx.engine.started_us,anchor);
        }
    }
    #[test]
    fn usb_sleep_opt_in_allows_cadence_without_treating_reattach_as_activity() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let mut cx=context();cx.usb_sleep=true;cx.usb=true;cx.cadence=8;
        f.usb=true;f.now_us=16_000_000;cx.boot_policy(&hw);
        assert!(cx.sleep.enabled);assert_eq!(cx.engine.wake_dw,8);
    }
    #[test]
    fn nv_cadence_defaults_to_one_and_seventeen_disables_sleep() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        for (value,want) in [(&b""[..],1),(&b"0"[..],1),(&b"1"[..],1),(&b"8"[..],8),(&b"16"[..],16),(&b"17"[..],0),(&b"bad"[..],1)] {
            f.nv_cadence=value.to_vec();assert_eq!(boot_cadence(&hw),want);
        }
    }
    #[test]
    fn usb_veto_also_applies_to_explicit_sleep_and_activity_defers_boot() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let mut cx=context();cx.cadence=8;f.now_us=16_000_000;
        f.usb=true;assert_eq!(cx.enter_sleepy(&hw,false,8),Err(ERR_BUSY));assert_eq!(f.wifi_sets,0);
        f.usb=false;f.active_window=true;cx.boot_policy(&hw);assert!(!cx.sleep.enabled);
        f.active_window=false;cx.boot_policy(&hw);assert!(cx.sleep.enabled);
    }
    #[test]
    fn public_power_methods_dispatch_without_console_or_nan_capture_context() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let mut state=State::default();
        for (method,data) in [(14,&[0xa3,1,0x19,1,0xf4,2,2,3,10][..]),(13,&[0xa0][..]),(15,&[0xa0][..])] {
            let call=CallView {component:C_SELF,method,value:0,flags:1|CALL_ENVELOPE,data,
                time_us:0,pkt:0,from:ADDR_LINK,id:12,request_id:Some(99), src: None};
            assert_eq!(state.handle(&hw,&call),CONTINUE);
            let reply=f.sent.last().unwrap();
            if method==13 {
                assert_eq!(cbor::get_int(reply,13).unwrap(),Some(42));
                assert_eq!(cbor::get_int(reply,16).unwrap(),Some(4000000));
                assert_eq!(cbor::get_int(reply,27).unwrap(),Some(0));
            } else {assert_eq!(reply,&[0xa0]);}
        }
        assert_eq!(f.power_sleeps,[(500,2,10)]);assert_eq!(f.power_debug,[3]);
        f.power_error=ERR_BUSY;
        let mut out=[0u8;256];
        assert_eq!(power_gen::dispatch(&mut state,&hw,13,&[0xa0],&mut out),Err(ERR_BUSY));
        assert_eq!(power_gen::dispatch(&mut state,&hw,99,&[0xa0],&mut out),Err(ERR_UNSUPPORTED));
        assert_eq!(power_gen::dispatch(&mut state,&hw,14,&[0xa1,1,0x20],&mut out),Err(ERR_ARG));
    }
    #[test]
    fn native_transport_patches_roles_and_uart_off_preserves_sync() {
        let mut f=Fixture::default();f.wifi.ap=1;
        let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();
        cx.transport(&hw,&NanTransport {uart:8,..Default::default()}).unwrap();
        assert_eq!(f.serial_mode,0);
        assert_eq!((f.wifi.radio,f.wifi.now,f.wifi.sta,f.wifi.ap),(1,1,0,1));
        cx.transport(&hw,&NanTransport {ap:0,..Default::default()}).unwrap();
        assert_eq!(f.wifi.ap,0);assert_eq!(f.serial_mode,0);
        assert_eq!(cx.enter_sleepy(&hw,false,8),Ok(()));
    }
    #[test]
    fn direct_transport_envelope_uses_nan_policy_and_rejects_retired_fields() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let ptr=std::boxed::Box::into_raw(std::boxed::Box::new(context()));
        let mut state=State {context:ptr as usize,..Default::default()};
        for (method,fields,error) in [
            (4,&[0xa2,0x10,1,0x12,8][..],0),
            (4,&[0xa1,1,6][..],ERR_ARG),
            (4,&[0xa1,0x18,25,0x46,0,0,0,0,0,0][..],ERR_UNSUPPORTED),
            (1,&[0xa0][..],ERR_UNSUPPORTED),
        ] {
            let call=CallView {component:1,method,value:0,flags:1|CALL_ENVELOPE,data:fields,
                time_us:0,pkt:0,from:ADDR_LINK,id:12,request_id:Some(99), src: None};
            state.handle(&hw,&call);
            let reply=f.sent.last().unwrap();
            if error==0 {assert_eq!(reply,&[0xa0]);}
            else {assert_eq!(cbor::Reader::new(reply).next().and_then(|v|v.as_int()),Some(error as i64));}
            assert_eq!((f.wifi.ap,f.serial_mode),(1,0));
        }
        unsafe {drop(std::boxed::Box::from_raw(ptr));}
    }
    #[test]
    fn boot_sleep_policy_runs_before_off_channel_capture_retry() {
        unsafe extern "C" fn off_channel(p: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *const core::ffi::c_void {
            if core::ffi::CStr::from_ptr(name).to_bytes()==b"wifi" {(&RADIO_WIFI as *const Wifi).cast()}
            else {component(p,name)}
        }
        let mut f=Fixture {now_us:20_000_000,actual:WifiState {radio:1,sta:2,channel:1,..Default::default()},..Default::default()};
        let mut h=host(&mut f);h.get_component=Some(off_channel);
        let hw=unsafe {Hw::new(&h)}.unwrap();let mut cx=context();cx.cadence=8;
        cx.arm(&hw);
        assert!(cx.sleep.enabled,"off-channel association must not bypass the boot sleep deadline");
        assert_eq!((f.wifi.sta,f.wifi.now,f.wifi.channel),(0,1,6));
    }
    #[test]
    fn lora_and_fsk_start_records_are_valid_and_sleep_releases_owned_radio() {
        for mode in [1,2] {
            let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
            let mut cx=context();
            cx.transport(&hw,&NanTransport {radio_mode:mode,radio_frequency:915_000_000,..Default::default()}).unwrap();
            let record=&f.module_starts[0];
            assert_eq!(cbor::get_text(record,0).unwrap().unwrap(),if mode==1 {&b"rx"[..]}else{&b"fsk"[..]});
            assert_eq!(cbor::get_int(record,2).unwrap(),Some(915_000_000));
            cx.enter_sleepy(&hw,false,8).unwrap();assert!(cx.radio_suspended);
            cx.leave_sleepy(&hw);assert!(!cx.radio_suspended);assert_eq!(f.module_starts.len(),2);
            assert_eq!(f.module_unloads,0,"sleep/restore must not queue a module stop");
            cx.transport(&hw,&NanTransport {radio_mode:mode,..Default::default()}).unwrap();
            assert_eq!(f.module_starts.len(),3);
            assert_eq!(f.module_unloads,0,"role replacement is a direct call");
            cx.transport(&hw,&NanTransport {radio_mode:0,..Default::default()}).unwrap();
            assert_eq!(f.module_unloads,1);
            assert!(cx.radio_request.is_none());
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
            b"stream" => (&COMPANION_STREAM as *const Stream).cast(),
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
        let mut state = State { context: ptr as usize, ..Default::default() };
        for (fields, expected) in [
            (&[0xa1,1,0x43,b'n',b'e',b'w'][..], 0),
            (&[0xa1,1,0x61,b'x'][..], ERR_ARG),
            (&[0xa0][..], ERR_ARG),
        ] {
            // an envelope {1: 46, 2: 11, 3: id, 5: fields} as the runtime hands it over; it wraps the answer
            let call = CallView { component: C_SELF, method: 11, value: 0, flags: 1 | CALL_ENVELOPE,
                data: fields, time_us: 0, pkt: 0, from: ADDR_LINK, id: 9, request_id: None, src: None };
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
            time_us:0,pkt:0,from:52,id:19, request_id: None, src: None };
        state.handle(&hw,&call); assert_eq!(f.sent.last().unwrap(), &[0xa0]);
        assert_eq!(unsafe { &*ptr }.engine.info_len,0);
        assert!(unsafe { &*ptr }.engine.info.iter().all(|b|*b==0));
        state.stop(&hw); assert!(f.packets.is_empty());
    }

    #[test]
    fn hosted_table_calls_share_policy_and_publication_without_messages() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let ptr=std::boxed::Box::into_raw(std::boxed::Box::new(context()));
        let mut state=State {context:ptr as usize,..Default::default()};
        let call=CallView {component:HOST_COMPONENT as i32,method:HOST_TABLE as i32,value:0,
            flags:1,data:b"nan",time_us:0,pkt:0,from:ADDR_MAIN,id:1,request_id:None, src: None};
        assert_eq!(state.handle(&hw,&call),CONTINUE);
        let table=&state.hosted.table;
        let ctx=core::ptr::addr_of!(state.hosted.ctx) as *mut core::ffi::c_void;
        assert_eq!(unsafe {table.set_announce.unwrap()(ctx,b"native".as_ptr(),6)},0);
        assert_eq!(unsafe {&*ptr}.engine.info_len,6);
        f.usb=true;
        assert_eq!(unsafe {table.usb.unwrap()(ctx,1)},0);
        assert_eq!(unsafe {table.sleepy.unwrap()(ctx,1,8,0,0)},ERR_BUSY);
        assert!(!unsafe {&*ptr}.sleep.enabled);
        state.stop(&hw);
    }
    #[test]
    fn immediate_sleep_clears_grace_but_ordinary_sleep_preserves_it() {
        let mut f=Fixture::default();let h=host(&mut f);let hw=unsafe {Hw::new(&h)}.unwrap();
        let mut cx=context();f.active_window=true;
        cx.set_sleepy(&hw,true,8,false,false).unwrap();assert!(f.active_window);
        cx.set_sleepy(&hw,true,8,false,true).unwrap();assert!(!f.active_window);
    }
    #[test]
    fn sleepy_command_over_ppp_returns_a_success_result() {
        let mut f = Fixture::default(); let h = host(&mut f);
        let hw = unsafe { Hw::new(&h) }.unwrap();
        let ptr = std::boxed::Box::into_raw(std::boxed::Box::new(context()));
        let mut state = State { context: ptr as usize, ..Default::default() };
        // {1: 46, 2: 6, 3: 42, 5: {1: false}} as the runtime hands it over: method 6, the fields; it wraps the answer in the envelope
        let data = &[0xa1, 1, 0xf4];
        let call = CallView { component: C_SELF, method: 6, value: 0,
            flags: 1 | CALL_ENVELOPE, data, time_us: 0, pkt: 0, from: ADDR_LINK, id: 9, request_id: None, src: None };
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
            hosted: api_gen::Hosted::default(),
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

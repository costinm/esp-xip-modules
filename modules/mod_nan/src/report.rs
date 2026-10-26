//! Live best-effort observations; no subscriptions, peer cache or output queue.
use xip_abi::{cbor::Writer, *};
use crate::protocol::Plan;

/// UART report port; BLE carries the same map as an unsolicited nan.report result.
pub const PORT: i32 = PORT_NAN_REPORT as i32;
#[derive(Default)]
pub struct Reports {
    window: Option<u64>,
    wake: Option<u64>,
    opened: u64,
    first: Option<u64>,
    sync: Option<u64>,
    last_cycle: Option<([u8; 6], u64)>,
    completed: Option<(Option<u16>,Option<u16>)>,
    offline_cycle: Option<u64>,
}
impl Reports {
    /// Native wake delivery marks CPU wake before the next capture is opened.
    pub fn wake(&mut self, at: u64) { self.wake=Some(at);self.opened=at;self.first=None;self.sync=None; }
    /// The scheduler tracks the actual receive window, including its early lead.
    pub fn plan(&mut self, plan: &Plan, now: u64) {
        if !plan.capture || plan.acquiring { return; }
        let slot=plan.slot+u64::from(now<plan.start_us);
        if self.window!=Some(slot) {
            self.window=Some(slot);
            if let Some(wake)=self.wake.take() {self.opened=wake;} else {
                self.opened=now;self.first=None;self.sync=None;
            }
        }
    }
    /// Radio ingress records the first observed NAN/NOW packet and clock update.
    pub fn received(&mut self, at: u64, sync: bool) {
        if (self.window.is_none() && self.wake.is_none()) || at<self.opened {return;}
        self.first.get_or_insert(at);
        if sync { self.sync.get_or_insert(at); }
    }
    /// The scheduler reports near the start of DW0, using the latest completed window.
    pub fn periodic(&mut self, plan: &Plan, cluster: Option<[u8;6]>, now_cluster: bool, out: &mut [u8]) -> Option<usize> {
        if !plan.capture && !plan.acquiring && self.window.take().is_some() {
            self.completed=Some((self.delay(self.first),self.delay(self.sync)));
        }
        if plan.acquiring || !plan.public || plan.slot%16!=0 {return None;}
        let key=(cluster.unwrap_or([0;6]),plan.slot/16);
        if self.last_cycle==Some(key) {return None;}
        self.last_cycle=Some(key);
        self.snapshot(cluster,now_cluster,out)
    }
    fn delay(&self, at: Option<u64>) -> Option<u16> {
        at.map(|at|(at.saturating_sub(self.opened)/1000).min(u16::MAX as u64) as u16)
    }
    /// Startup/off-channel retry paths keep a heartbeat when no radio clock is available.
    pub fn offline(&mut self, now: u64, out: &mut [u8]) -> Option<usize> {
        let cycle=now/(16*crate::protocol::PERIOD_US);
        if cycle==0 || self.offline_cycle==Some(cycle) {return None;}
        self.offline_cycle=Some(cycle);
        let mut w=Writer::new(out);
        w.map(3).uint(1).uint(0).uint(2).bytes(&[]).uint(5).boolean(false);
        w.finish().map(|b|b.len())
    }
    /// Status callers read the current receive timing without keeping report history.
    pub fn snapshot(&self, cluster: Option<[u8;6]>, now_cluster: bool, out: &mut [u8]) -> Option<usize> {
        let (first,sync)=self.completed.unwrap_or((self.delay(self.first),self.delay(self.sync)));
        let mut w=Writer::new(out);
        w.map(3+usize::from(first.is_some())+usize::from(sync.is_some()))
            .uint(1).uint(0).uint(2).bytes(cluster.as_ref().map_or(&[],|c|&c[3..]))
            .uint(5).boolean(now_cluster);
        for (tag,ms) in [(3,first),(4,sync)] {
            if let Some(ms)=ms {w.uint(tag).uint(ms as u64);}
        }
        w.finish().map(|b|b.len())
    }
}

/// Scheduler and radio ingress deliver live reports, preferring an active CoC.
pub fn emit(hw: &Hw, record: &[u8]) {
    let mut status=BleStatus::default();
    let ble=hw.nimble().and_then(|n|n.status).is_some_and(|f|unsafe {f(hw.ctx(),&mut status)}==0 && status.coc_connected!=0);
    let name=if ble {c"ble"}else{c"serial"};
    let Ok(link)=crate::link(hw,name) else {return;};
    if ble {
        // The CoC link carries QUIC datagrams. Keep its normal native CBOR
        // envelope framing: a nan.report event with no request ID, not a request.
        let mut packet=[0;1500];
        packet[..7].copy_from_slice(&[0xc0,0x44,0x4d,0,2,0,0]);
        let mut w=Writer::new(&mut packet[7..]);
        w.map(3).uint(1).uint(46).uint(2).uint(12).uint(6).raw(record);
        if let Some(data)=w.finish() {let n=data.len();hw.message(link as u32,0,0,0,&packet[..7+n]);}
    } else {
        hw.message(link as u32,0,0,PORT as u64,record);
    }
}

/// Radio ingress mirrors a service observation immediately, without retaining it.
fn observation_to(kind: u64, peer: [u8;6], service: Option<[u8;6]>, instance: u8, now: bool, rssi: i8, payload: &[u8], emit: &mut impl FnMut(&[u8])) {
    let mut data=[0;1400];let mut w=Writer::new(&mut data);
    w.map(6+usize::from(service.is_some())).uint(1).uint(kind).uint(6).bytes(&peer)
        .uint(7).bytes(payload).uint(8).int(rssi as i64).uint(10).uint(instance as u64).uint(11).boolean(now);
    if let Some(service)=service {w.uint(9).bytes(&service);}
    if let Some(data)=w.finish() {emit(data);}
}

// SSI may be in SDA or in either supported SDEA spelling. Select by instance,
// never the first matching service: Android can coalesce PUB and SUB in one SDF.
fn payload<'a>(frame: &'a [u8], d: &dmesh_rawnan::ServiceDescriptor<'a>) -> &'a [u8] {
    if !d.payload.is_empty() {return d.payload;}
    let mut at=dmesh_rawnan::NAN_ACTION_START;
    while let Some(header)=frame.get(at..at+3) {
        let len=u16::from_le_bytes([header[1],header[2]]) as usize;
        let start=at+3;let Some(body)=frame.get(start..start+len) else {break;};
        if header[0]==0x0e && body.first()==Some(&d.instance) {
            for (length,info,prefix) in [(3,9,&[0x50,0x6f,0x9a,2][..]),(5,11,&[0x50,0x6f,0x9a,0][..])] {
                if body.get(info-4..info)==Some(prefix) && body.len()>=length+2 {
                    let declared=u16::from_le_bytes([body[length],body[length+1]]) as usize;
                    if declared>=4 && info+declared-4==body.len() {return &body[info..];}
                }
            }
        }
        at=start+len;
    }
    &[]
}

/// Radio ingress forwards each PUB/SUB/Follow-up, including non-DMesh services.
pub fn forward(hw: &Hw, peer: [u8;6], frame: &[u8], rssi: i8) {
    forward_to(peer,frame,rssi,&mut |data|emit(hw,data));
}
pub fn forward_to(peer: [u8;6], frame: &[u8], rssi: i8, emit: &mut impl FnMut(&[u8])) {
    if let Some(sync)=dmesh_rawnan::parse_now_sync_body(frame) {
        if !sync.service_info.is_empty() {observation_to(4,peer,None,0,true,rssi,sync.service_info,emit);}
        return;
    }
    for item in dmesh_rawnan::service_descriptors(frame) {
        let d=item.descriptor;
        let kind=match d.control&3 {0=>1,1=>2,2=>3,_=>continue};
        observation_to(kind,peer,Some(item.service_id),d.instance,false,rssi,payload(frame,&d),emit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(slot: u64,capture: bool) -> Plan {
        Plan {capture,public:capture,sync:false,slot,next_us:0,start_us:1000,end_us:2000,acquiring:false}
    }
    #[test]
    fn dw0_summary_is_bounded_and_once_per_eight_second_cycle() {
        let mut r=Reports::default();let mut out=[0;64];
        r.wake(0);r.plan(&plan(0,true),1000);r.received(9000,false);r.received(18000,true);
        let n=r.periodic(&plan(0,true),Some([0x50,0x6f,0x9a,1,2,3]),true,&mut out).unwrap();
        assert!(n<=24);
        assert_eq!(cbor::get_int(&out[..n],3).unwrap(),Some(9));
        assert_eq!(cbor::get_int(&out[..n],4).unwrap(),Some(18));
        r.plan(&plan(8,true),1000);assert!(r.periodic(&plan(8,true),None,false,&mut out).is_none());
        r.plan(&plan(16,true),1000);assert!(r.periodic(&plan(16,true),None,false,&mut out).is_some());
        assert!(r.periodic(&plan(16,true),None,false,&mut out).is_none());
    }
    #[test]
    fn no_clock_still_reports_at_an_eight_second_cadence() {
        let mut r=Reports::default();let mut out=[0;32];let period=16*crate::protocol::PERIOD_US;
        assert!(r.offline(period-1,&mut out).is_none());
        assert!(r.offline(period,&mut out).is_some());
        assert!(r.offline(period+1,&mut out).is_none());
        assert!(r.offline(2*period,&mut out).is_some());
    }
    #[test]
    fn service_info_from_pub_sub_and_followup_is_preserved() {
        for control in [0x10,0x11,0x12] {
            let mut frame=[0;512];
            let n=crate::frames::sdf(&mut frame,[2;6],[0xff;6],[0x50,0x6f,0x9a,1,2,3],1,0,control,b"opaque").unwrap();
            let descriptors=dmesh_rawnan::service_descriptors(&frame[..n]);
            assert_eq!(descriptors.len(),1);
            assert_eq!(payload(&frame[..n],&descriptors[0].descriptor),b"opaque");
        }
    }
}

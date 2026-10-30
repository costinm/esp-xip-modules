//! Opt-in opaque datagram scheduling. Route interpretation belongs to disc.
use super::*;
use crate::api::{Received, Solicitation};
const RECORD_MAX: usize = datagram::PAYLOAD_MAX;

const OWNER: u64 = u64::MAX;
pub(super) fn owns(owner: u64) -> bool {
    owner == OWNER
}
const LIFETIME: u64 = 2 * protocol::WAKE_PERIOD_US + protocol::PERIOD_US;
struct Pending {
    record: [u8; RECORD_MAX],
    len: usize,
    path: Option<discovery::ReturnPath>,
    until: u64,
    ready_at: u64,
    immediate: bool,
    ordinary: bool,
    // Each rendezvous phase has separate NAN/NOW admission bits.
    phases: u8,
}
#[derive(Default)]
pub(super) struct Solicitor {
    pending: [Option<Pending>; 4],
    owner: u32,
    callback: i32,
    pub completed: u32,
    pub expired: u32,
    pub last_error: i32,
    submitted: u32,
    refused: u32,
    busy: u32,
}
pub(super) fn transmitted(ptr: Option<usize>, owner: u64, rc: i32) {
    if owner != OWNER {
        return;
    }
    let Some(ptr) = ptr else { return };
    let s = unsafe { &mut *(ptr as *mut Solicitor) };
    if rc >= 0 {
        s.submitted += 1;
    } else if rc == ERR_BUSY {
        s.busy += 1;
    } else {
        s.refused += 1;
        s.last_error = rc;
    }
}
impl Solicitor {
    pub fn owner(&self) -> u32 {
        self.owner
    }
    pub fn active(&self) -> bool {
        self.pending.iter().any(Option::is_some)
    }
    pub fn ordinary(&self) -> bool {
        self.pending.iter().flatten().any(|p| !p.ordinary)
    }
    fn add(
        &mut self,
        payload: &[u8],
        path: Option<discovery::ReturnPath>,
        now: u64,
        delay_ms: u8,
    ) -> Result<(), i32> {
        if payload.len() > RECORD_MAX {
            return Err(ERR_ARG);
        }
        // Retries must not create another schedule or extend its lifetime.
        if self
            .pending
            .iter()
            .flatten()
            .any(|p| &p.record[..p.len] == payload && p.path == path)
        {
            return Ok(());
        }
        let slot = self
            .pending
            .iter_mut()
            .find(|p| p.is_none())
            .ok_or(ERR_BUSY)?;
        let mut record = [0; RECORD_MAX];
        record[..payload.len()].copy_from_slice(payload);
        *slot = Some(Pending {
            record,
            len: payload.len(),
            path,
            until: now + LIFETIME,
            ready_at: now + delay_ms as u64 * 1000,
            immediate: false,
            ordinary: false,
            phases: 0,
        });
        Ok(())
    }
}
impl Context {
    pub(super) fn receive_hold_plan(&self, hw: &Hw, now: u64, plan: &mut protocol::Plan) {
        if self.solicitor.is_none() {
            return;
        }
        let remaining = hw
            .power()
            .and_then(|p| p.awake_for)
            .map_or(0, |f| unsafe { f(hw.ctx(), 0) });
        if remaining > 0 {
            // An awake unassociated device still needs capture for NSY1 RX.
            // Keep the ordinary TX gates and deadlines; only extend reception.
            plan.capture = true;
            plan.next_us = plan.next_us.min(now + remaining as u64 * 1000);
        }
    }
    pub(super) fn participate(
        &mut self,
        hw: &Hw,
        enabled: bool,
        owner: u32,
        callback: i32,
    ) -> Result<(), i32> {
        if let Some(ptr) = self.solicitor {
            if unsafe { (*(ptr as *const Solicitor)).owner } != owner {
                return Err(ERR_BUSY);
            }
        }
        if enabled && self.solicitor.is_none() {
            let ptr = hw
                .mem_alloc(core::mem::size_of::<Solicitor>())
                .cast::<Solicitor>();
            if ptr.is_null() {
                return Err(ERR_NOMEM);
            }
            unsafe {
                ptr.write(Solicitor {
                    owner,
                    callback,
                    ..Solicitor::default()
                });
            }
            self.solicitor = Some(ptr as usize);
        } else if !enabled {
            if let Some(ptr) = self.solicitor.take() {
                unsafe {
                    core::ptr::drop_in_place(ptr as *mut Solicitor);
                }
                hw.mem_free(ptr as *mut u8);
            }
            for slot in &mut self.tx {
                if slot.is_some_and(|p| p.owner == OWNER) {
                    hw.pkt_free(slot.take().unwrap().pkt);
                }
            }
        }
        Ok(())
    }
    pub(super) fn solicit(&mut self, owner: u32, fields: &[u8], now: u64) -> Result<(), i32> {
        let request = Solicitation::decode(fields).ok_or(ERR_ARG)?;
        let payload = request.payload;
        let path = request.peer.map(|p| discovery::ReturnPath {
            peer: p.mac,
            instance: p.instance,
            now: false,
        });
        let ptr = self.solicitor.ok_or(ERR_NOTFOUND)? as *mut Solicitor;
        if unsafe { (*ptr).owner } != owner {
            return Err(ERR_ARG);
        }
        unsafe { (*ptr).add(payload, path, now, request.delay_ms) }
    }
    fn solicit_send(&mut self, hw: &Hw, p: &Pending, now_link: bool, now: u64) -> Result<(), i32> {
        let at = self.tx.iter().position(Option::is_none).ok_or(ERR_BUSY)?;
        match p.path {
            Some(path) => self.enqueue(
                hw,
                discovery::ReturnPath {
                    now: now_link,
                    ..path
                },
                &p.record[..p.len],
                now,
            )?,
            None => self.post_discovery(hw, &p.record[..p.len], true, now_link, now)?,
        }
        self.tx[at].as_mut().unwrap().owner = OWNER;
        Ok(())
    }
    pub(super) fn solicit_tick(&mut self, hw: &Hw, now: u64, plan: Option<&protocol::Plan>) {
        let Some(ptr) = self.solicitor else {
            return;
        };
        // The allocation is separate from Context and callbacks are serialized.
        let s = unsafe { &mut *(ptr as *mut Solicitor) };
        for slot in &mut s.pending {
            let Some(p) = slot.as_mut() else { continue };
            if now >= p.until {
                *slot = None;
                s.expired += 1;
                continue;
            }
            if now < p.ready_at {
                continue;
            }
            if !p.immediate {
                match self.solicit_send(hw, p, true, now) {
                    Ok(()) => p.immediate = true,
                    Err(rc) => s.last_error = rc,
                }
            }
            // Both halves belong to the paired receive window. The serialized
            // owner can reach this turn after the short publication half;
            // the NOW half remains a valid opportunity for these datagrams.
            // Neither flag is set in the resume lead, so we never send early.
            if let Some(plan) = plan.filter(|p| (p.public || p.sync) && !p.acquiring) {
                if !p.ordinary {
                    match self.solicit_send(hw, p, false, now) {
                        Ok(()) => p.ordinary = true,
                        Err(rc) => s.last_error = rc,
                    }
                }
                if plan.slot % 8 == 0 {
                    let shift = if plan.slot % 16 == 0 { 0 } else { 2 };
                    for transport in 0..2 {
                        let mask = 1 << (shift + transport);
                        if p.phases & mask == 0 {
                            match self.solicit_send(hw, p, transport == 1, now) {
                                Ok(()) => p.phases |= mask,
                                Err(rc) => s.last_error = rc,
                            }
                        }
                    }
                }
            }
            if p.immediate && p.ordinary && p.phases == 15 {
                *slot = None;
                s.completed += 1;
            }
        }
    }
    pub(super) fn solicitation_deadline(&self, now: u64) -> Option<u64> {
        let s = unsafe { &*(self.solicitor? as *const Solicitor) };
        s.pending
            .iter()
            .flatten()
            .map(|p| {
                if p.ready_at > now {
                    p.ready_at
                } else {
                    p.until
                }
            })
            .min()
    }
    pub(super) fn solicitation_plan(&self, now: u64) -> protocol::Plan {
        let base = self.engine.plan(now);
        let Some(ptr) = self.solicitor else {
            return base;
        };
        let s = unsafe { &*(ptr as *const Solicitor) };
        if !s.active() {
            return base;
        }
        let extra = self
            .engine
            .plan_with_cadence(now, if s.ordinary() { 1 } else { 8 });
        let mut plan = if extra.capture || (!base.capture && extra.next_us < base.next_us) {
            extra
        } else {
            base
        };
        if let Some(deadline) = self.solicitation_deadline(now) {
            plan.next_us = plan.next_us.min(deadline);
        }
        plan
    }
    /// This adapter never extends power holds or admits arbitrary envelopes.
    pub(super) fn discovery_datagram(
        &self,
        hw: &Hw,
        path: discovery::ReturnPath,
        payload: &[u8],
        now: u64,
        rssi: i8,
    ) {
        if self.solicitor.is_some() {
            let mut out = [0; 400];
            // NOW can arrive on an associated STA's AP channel while NAN's
            // configured channel is unavailable. Do not report that policy hint
            // as the actual radio channel.
            let mut actual = WifiState {
                size: core::mem::size_of::<WifiState>() as u32,
                ..Default::default()
            };
            let channel = hw
                .wifi()
                .and_then(|w| w.state)
                .and_then(|f| {
                    (unsafe { f(hw.ctx(), &mut actual) } >= 0 && (1..=13).contains(&actual.channel))
                        .then_some(actual.channel)
                })
                .unwrap_or(self.channel);
            let record = Received {
                peer: datagram::Peer {
                    mac: path.peer,
                    instance: path.instance,
                },
                now: path.now,
                channel,
                rssi,
                received_us: now,
                payload,
            };
            let s = unsafe { &*(self.solicitor.unwrap() as *const Solicitor) };
            if let Some(data) = record.encode(&mut out) {
                hw.message(s.owner, s.owner as i32, s.callback, 0, data);
            }
        }
    }
    pub(super) fn solicit_status(&self, out: &mut [u8]) -> usize {
        let mut w = Writer::new(out);
        w.map(8).uint(1).boolean(self.solicitor.is_some());
        let s = self.solicitor.map(|p| unsafe { &*(p as *const Solicitor) });
        w.uint(2)
            .uint(s.map_or(0, |s| s.pending.iter().flatten().count()) as u64)
            .uint(3)
            .uint(s.map_or(0, |s| s.completed) as u64)
            .uint(4)
            .uint(s.map_or(0, |s| s.expired) as u64)
            .uint(5)
            .int(s.map_or(0, |s| s.last_error) as i64)
            .uint(6)
            .uint(s.map_or(0, |s| s.submitted) as u64)
            .uint(7)
            .uint(s.map_or(0, |s| s.refused) as u64)
            .uint(8)
            .uint(s.map_or(0, |s| s.busy) as u64);
        w.finish().map_or(0, |b| b.len())
    }
}

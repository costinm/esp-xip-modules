#![no_std]
extern crate alloc;
use alloc::{boxed::Box, vec, vec::Vec};
use ucel::{Context, Error, Host, Value};
use xip_abi::{cbor::{self, Item, Writer}, CallView, Hw, Module, HOST_STOP, RET_CONTINUE, RET_DONE, ERR_ARG, ERR_BUSY, ERR_IO, ERR_SIZE, ERR_TIMEOUT};

const COMPONENT: i32 = 53;
const FUEL: usize = 4096;
const MAX: usize = 16384;
const TIMER_COMPONENT: i32 = -2147483646;
struct Pending { method: i32, alarm: i32, rules: Vec<u8>, context: Context }
struct Load { from: u32, id: u64, flags: u32, name: Vec<u8>, context: Context, token: u64, method: i32, alarm: i32, version: u32, size: usize, data: Vec<u8> }
#[derive(Default)]
pub struct State { inner: Option<Box<Inner>> }
#[derive(Default)]
struct Inner { pending: Vec<Pending>, next: i32, load: Option<Load> }
struct EvalHost<'a> { hw: &'a Hw, delays: Vec<u32> }
fn validate_schedule(pending: usize, delays: usize, next: i32, context: &Context) -> Result<(), Error> {
    if pending + delays > 8 { return Err(Error::Limit); }
    let mut encoded = vec![0; MAX];
    ucel::encode_context(context, &mut encoded)?;
    if next.checked_add(delays as i32).is_none() { return Err(Error::Limit); }
    Ok(())
}
impl Host for EvalHost<'_> {
    fn call(&mut self, name: &str, args: &[Value], ctx: &mut Context) -> Result<Value, Error> {
        match (name, args) {
            ("set", [Value::Uint(key), value]) => {
                if let Some((_, v)) = ctx.iter_mut().find(|(k, _)| k == key) { *v = value.clone(); }
                else { if ctx.len() >= 1024 { return Err(Error::Limit); } ctx.push((*key, value.clone())); }
                Ok(value.clone())
            }
            ("log", [Value::String(s)]) => if self.hw.log(s.as_bytes()) < 0 { Err(Error::Host) } else { Ok(Value::Null) },
            ("now_ms", []) => u32::try_from(self.hw.now_ms()).map(Value::Uint).map_err(|_| Error::Overflow),
            ("set" | "log" | "now_ms", _) => Err(Error::Type),
            _ => Err(Error::UnknownFunction),
        }
    }
    fn after(&mut self, delay: u32, _: &Context) -> Result<(), Error> {
        if delay == 0 || self.delays.len() >= 8 { return Err(Error::Limit); }
        self.delays.push(delay); Ok(())
    }
}
fn write_value(w: &mut Writer<'_>, v: &Value) {
    match v { Value::Null => {w.null();}, Value::Bool(v) => {w.boolean(*v);}, Value::Int(v) => {w.int(*v as i64);}, Value::Uint(v) => {w.uint(*v as u64);}, Value::String(v) => {w.text(v);}, Value::Bytes(v) => {w.bytes(v);} }
}
impl Inner {
    fn load_next(hw: &Hw, load: &Load) -> i32 {
        let mut buf = [0; 64];
        let mut w = Writer::new(&mut buf);
        w.map(if load.version == 0 {3} else {4}).uint(1).bytes(&load.name)
            .uint(6).uint(load.data.len() as u64).uint(7).uint(1024);
        if load.version != 0 { w.uint(5).uint(load.version as u64); }
        hw.message(15, 15, 17, load.token, w.finish().unwrap())
    }
    fn run(&mut self, hw: &Hw, rules: &[u8], mut context: Context) -> Result<(Vec<Value>, Context), Error> {
        let mut host = EvalHost { hw, delays: Vec::new() };
        let results = ucel::evaluate(rules, &mut context, &mut host, FUEL)?;
        // Bound retained snapshots across requests, not just after() calls in
        // a single evaluation, and reject contexts that cannot be returned.
        validate_schedule(self.pending.len(), host.delays.len(), self.next, &context)?;
        let mut created = Vec::new();
        for delay in host.delays {
            // Unique callback method, not an alarm slot: slots can be reused while a callback runs.
            self.next = self.next.checked_add(1).ok_or(Error::Limit)?;
            let method = self.next;
            let alarm = hw.alarm_set(delay, 0, 0, 0, TIMER_COMPONENT, method);
            if alarm < 0 {
                for p in created { let p: Pending = p; hw.alarm_cancel(p.alarm); }
                return Err(Error::Host);
            }
            created.push(Pending { method, alarm, rules: rules.into(), context: context.clone() });
        }
        self.pending.extend(created);
        Ok((results, context))
    }
}
fn load_call(load: &Load) -> CallView<'static> {
    CallView { component: COMPONENT, method: 2, value: 0, flags: load.flags, data: &[], time_us: 0, pkt: 0, from: load.from, id: load.id, request_id: None, src: None }
}
fn respond(hw: &Hw, call: &CallView, result: Result<(Vec<Value>, Context), Error>) {
    if !call.wants_response() { return; }
    match result {
        Ok((values, context)) => {
            let mut ctx = vec![0; MAX]; let mut out = vec![0; MAX];
            if let Ok(n) = ucel::encode_context(&context, &mut ctx) {
                let mut w = Writer::new(&mut out); w.map(2).uint(1).array(values.len());
                for v in &values { write_value(&mut w, v); }
                w.uint(2).bytes(&ctx[..n]);
                if let Some(b) = w.finish() { hw.reply(call, b); } else { hw.reply_error(call, ERR_ARG); }
            } else { hw.reply_error(call, ERR_ARG); }
        }
        Err(_) => { hw.reply_error(call, ERR_ARG); }
    }
}
fn uint(data: &[u8], key: u64) -> Option<u64> { cbor::get(data, key).ok()??.next()?.as_uint() }
fn bytes(data: &[u8], key: u64) -> Result<Option<&[u8]>, ()> {
    match cbor::get(data, key)? { None => Ok(None), Some(mut r) => match r.next() { Some(Item::Bytes(b)) => Ok(Some(b)), _ => Err(()) } }
}
impl Module for State {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_STOP) {
            if let Some(inner) = self.inner.take() { for p in &inner.pending { hw.alarm_cancel(p.alarm); } if let Some(load) = inner.load { hw.alarm_cancel(load.alarm); } }
            return RET_DONE as i32;
        }
        if let Some(inner) = self.inner.as_mut() {
            let matched = inner.load.as_ref().map_or(false, |l| (call.component == 15 && call.method == 17 && call.id == l.token) || (call.component == TIMER_COMPONENT && call.method == l.method));
            if matched {
                let mut load = inner.load.take().unwrap();
                let rc = if call.component == TIMER_COMPONENT { Err(ERR_TIMEOUT) } else { (|| {
                    if let Some(Item::Neg(n)) = cbor::Reader::new(call.data).next() { return Err(i32::try_from(-1i64 - i64::try_from(n).unwrap_or(i64::MAX)).unwrap_or(ERR_IO)); }
                    let version = uint(call.data, 1).filter(|v| *v > 0 && *v <= u32::MAX as u64).ok_or(ERR_IO)? as u32;
                    let size = uint(call.data, 2).ok_or(ERR_IO)?;
                    if size > MAX as u64 { return Err(ERR_SIZE); }
                    let offset = uint(call.data, 6).ok_or(ERR_IO)?;
                    let chunk = bytes(call.data, 13).map_err(|_| ERR_IO)?.ok_or(ERR_IO)?;
                    if offset != load.data.len() as u64 || (load.version != 0 && (load.version != version || load.size != size as usize)) || chunk.len() > 1024 || chunk.len() > size as usize - load.data.len().min(size as usize) || (chunk.is_empty() && offset < size) { return Err(ERR_IO); }
                    load.version = version; load.size = size as usize; load.data.extend_from_slice(chunk);
                    if load.data.len() == load.size { Ok(true) } else { let rc = Inner::load_next(hw, &load); if rc < 0 { Err(rc) } else { Ok(false) } }
                })() };
                match rc {
                    Ok(false) => inner.load = Some(load),
                    other => { hw.alarm_cancel(load.alarm); let original = load_call(&load); match other { Ok(true) => respond(hw, &original, inner.run(hw, &load.data, load.context.clone())), Err(e) => { if original.wants_response() { hw.reply_error(&original, e); } }, _ => {} } }
                }
                return RET_CONTINUE as i32;
            }
        }
        if call.component == TIMER_COMPONENT {
            if let Some(inner) = self.inner.as_mut() {
                if let Some(i) = inner.pending.iter().position(|p| p.method == call.method) {
                    let pending = inner.pending.swap_remove(i);
                    if inner.run(hw, &pending.rules, pending.context).is_err() { hw.log(b"cel: scheduled evaluation failed"); }
                }
            }
        } else if call.component == COMPONENT && call.method == 2 {
            let inner = self.inner.get_or_insert_with(|| Box::new(Inner::default()));
            let result = (|| {
                if inner.load.is_some() { return Err(ERR_BUSY); }
                let name = bytes(call.data, 1).map_err(|_| ERR_ARG)?.filter(|n| !n.is_empty() && n.len() <= 8).ok_or(ERR_ARG)?;
                let context = match bytes(call.data, 2).map_err(|_| ERR_ARG)? { Some(b) => ucel::decode_context(b).map_err(|_| ERR_ARG)?, None => Vec::new() };
                inner.next = inner.next.checked_add(1).ok_or(ERR_BUSY)?;
                let method = inner.next;
                let alarm = hw.alarm_set(30000, 0, 0, 0, TIMER_COMPONENT, method);
                if alarm < 0 { return Err(alarm); }
                let load = Load { from: call.from, id: call.id, flags: call.flags, name: name.into(), context, token: 0xa15f000000000000 | method as u64, method, alarm, version: 0, size: 0, data: Vec::new() };
                let rc = Inner::load_next(hw, &load);
                if rc < 0 { hw.alarm_cancel(alarm); return Err(rc); }
                inner.load = Some(load); Ok(())
            })();
            if let Err(e) = result { if call.wants_response() { hw.reply_error(call, e); } }
        } else if call.component == COMPONENT && call.method == 1 {
            let result = (|| {
                let rules = bytes(call.data, 1).map_err(|_| Error::Malformed)?.ok_or(Error::Malformed)?;
                let context = match bytes(call.data, 2).map_err(|_| Error::Malformed)? { Some(b) => ucel::decode_context(b)?, None => Vec::new() };
                self.inner.get_or_insert_with(|| Box::new(Inner::default())).run(hw, rules, context)
            })();
            respond(hw, call, result);
        }
        RET_CONTINUE as i32
    }
}
#[macro_export]
macro_rules! export { () => { xip_abi::export_module!($crate::State, state=dynamic, components=[(53,1,$crate::handle)]); }; }
pub fn handle(state: &mut State, hw: &Hw, call: &CallView) -> i32 { state.handle(hw, call) }
#[cfg(not(feature = "flat-image"))]
export!();
#[cfg(not(feature = "flat-image"))]
pub const HANDLER: unsafe extern "C" fn(*const xip_abi::Host, *mut u8, *const xip_abi::Call) -> i32 = xip_handle;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_final_context_cannot_be_scheduled() {
        let context = vec![(1, Value::Bytes(vec![0; MAX]))];
        assert!(validate_schedule(0, 1, 0, &context).is_err());
        assert!(validate_schedule(0, 1, 0, &vec![(1, Value::Uint(1))]).is_ok());
    }
}

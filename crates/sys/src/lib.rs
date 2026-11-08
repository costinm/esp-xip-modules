//! The `sys` component (49): memory and task diagnostics, device control (module lifecycle, status, reset, saved settings and one debug
//! packet send) and the native dispatch of the components that no module hosts (Wi-Fi 10, ...). Its methods are in `API.md`.
//!
//! Requests reach it as calls. A 0x444d0002 datagram is parsed by the runtime (`call.is_envelope()`); this crate has no port and parses no
//! envelope of a port. The module routes component 49 to [`handle`] (its [`State`]) and answers the envelopes of the components that no
//! part serves with [`serve_envelope`], from the native table of [`native`].
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod api_gen;
mod device;
mod native;
mod tables;

use alloc::{vec, vec::Vec};
use api_gen::Api;
use xip_abi::{*, cbor};

/// The state block of this component (the module's `--state` is the sum of the blocks of its components): the table handed to the host.
pub const STATE_BYTES: usize = 64;

/// The component's state: the table handed to the host and the context its functions take.
#[derive(Default)]
pub struct State {
    hosted: api_gen::Hosted,
}
const _: () = assert!(core::mem::size_of::<State>() <= STATE_BYTES);

/// Response buffer size of the device-control and native answers.
pub(crate) const RESP_MAX: usize = 1000;

fn result(rc: i32) -> Result<(), i32> { if rc < 0 {Err(rc)} else {Ok(())} }
impl Api for State {
    /// Monitoring clients query memory usage; no managed allocation is needed.
    fn heap(&mut self, hw: &Hw, response: &mut SysHeapInfo) -> Result<(), i32> {
        let mut data=[0u8;64];
        let n=hw.heap_info(&mut data);if n<0 {return Err(n);}
        let data=data.get(..n as usize).ok_or(ERR_SIZE)?;
        let field=|tag| -> Result<u32,i32> {
            u32::try_from(cbor::get_int(data,tag).map_err(|_|ERR_ARG)?.ok_or(ERR_ARG)?).map_err(|_|ERR_SIZE)
        };
        *response=SysHeapInfo {free:field(1)?,largest:field(2)?,min_free:field(3)?,total:field(4)?,internal_free:field(5)?,internal_largest:field(6)?};
        Ok(())
    }
    /// Debug clients request native heap snapshots or control allocation tracing.
    fn heap_dump(&mut self, hw: &Hw, request: &SysHeapDump) -> Result<(), i32> {
        if !matches!(request.kind,0|1|3|4|5|6) {return Err(ERR_ARG);}
        result(hw.heap_dump(request.kind))
    }
    /// Debug clients inspect task priorities and stack usage alongside heap diagnostics.
    fn dump_tasks(&mut self, hw: &Hw) -> Result<(), i32> { result(hw.heap_dump(2)) }
}

/// Run request `(comp, method)` with the field map `fields`: the component's own methods (diagnostics and device control), the Wi-Fi
/// controls and the native components of the catalog. The runtime delivers a request that arrived as an envelope (port or datagram) as an
/// ordinary call, so this is its only entry; the answer it makes is wrapped by the runtime. Remote module dispatch is performed by the
/// firmware's manifest registry; never synchronously call another module from its shared worker.
fn dispatch_request(state: &mut State, hw: &Hw, comp: i64, method: i64, fields: &[u8]) -> Result<Vec<u8>, i32> {
    if comp <= 0 || method <= 0 || !device::valid_map(fields) { return Err(ERR_ARG); }
    if comp == api_gen::COMPONENT as i64 {
        let method = i32::try_from(method).map_err(|_| ERR_ARG)?;
        return match method {
            api_gen::M_HEAP | api_gen::M_HEAP_DUMP | api_gen::M_DUMP_TASKS => {
                let mut out = [0u8; 96];
                let n = api_gen::dispatch(state, hw, method, fields, &mut out)?;
                Ok(out[..n].to_vec())
            }
            _ => device::binary_method(hw, method, fields),
        };
    }
    if comp == 10 && (method == 3 || method == 4) {
        return native::wifi_binary(hw, method as u64);
    }
    if let Some(nm) = native::nat_find(comp as u64, method as u64) {
        return native::native_binary(hw, nm, fields);
    }
    Err(ERR_UNSUPPORTED)
}

/// An envelope request of any component, answered in the envelope's own id (the runtime wraps the answer). The module calls it for the
/// components that no part serves (the native table, and the components of other modules' tables that nobody hosts).
pub fn serve_envelope(state: &mut State, hw: &Hw, c: &CallView) {
    match dispatch_request(state, hw, c.component as i64, c.method as i64, c.data) {
        Ok(result) => { hw.reply(c, &result); }
        Err(rc) => { hw.reply_error(c, rc); }
    }
}

/// Wire component 49 (`--wire 49:1`, the components table of the module). An envelope is answered with [`serve_envelope`]; a direct call
/// gets its method's answer: the diagnostics reply an ABI error on failure, the device-control methods a signed negative scalar.
pub fn handle(state: &mut State, hw: &Hw, call: &CallView) -> i32 {
    const CONTINUE: i32 = RET_CONTINUE as i32;
    if call.flags & CALL_LINK != 0 { return CONTINUE; }     // a raw frame: requests arrive as calls
    if call.is_envelope() {
        serve_envelope(state, hw, call);
        return CONTINUE;
    }
    if call.component != api_gen::COMPONENT || !call.wants_response() { return CONTINUE; }
    if device::is_device_method(call.method) {
        let out = match device::binary_method(hw, call.method, call.data) {
            Ok(result) => result,
            Err(rc) => {
                let mut out = vec![0; 9];
                let mut w = cbor::Writer::new(&mut out);
                w.int(rc as i64);
                let n = w.finish().map_or(0, |r| r.len());
                out.truncate(n);
                out
            }
        };
        if !out.is_empty() {
            hw.reply(call, &out);
        }
        return CONTINUE;
    }
    let mut out = [0u8; 96];
    match api_gen::dispatch(state, hw, call.method, call.data, &mut out) {
        Ok(n) => { hw.reply(call, &out[..n]); }
        Err(rc) => { hw.reply_error(call, rc); }
    }
    CONTINUE
}

/// Answer the host's `HOST_TABLE` request for `sys`: the table of this state, whose functions run in the module's allocator context.
pub fn provide(state: &mut State, hw: &Hw, call: &CallView) -> i32 {
    let this: *mut State = state;
    // SAFETY: `this` points at the state the module keeps for this component, which stays valid while the module runs.
    api_gen::provide::<State>(hw, call, unsafe { core::ptr::addr_of_mut!((*this).hosted) }, this)
}

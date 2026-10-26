//! Memory and task diagnostics, independent of console and NAN policy.
#![no_std]
mod api_gen;
#[cfg(feature="sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
use api_gen::Api;
use xip_abi::{*, cbor};
pub const STATE_BYTES: usize = 64;
#[derive(Default)]
pub struct State { hosted: api_gen::Hosted }
fn result(rc: i32) -> Result<(), i32> { if rc < 0 {Err(rc)} else {Ok(())} }
impl Api for State {
    /// Monitoring clients query memory usage; no managed allocation is needed.
    fn heap(&mut self, hw: &Hw, response: &mut SystemHeapInfo) -> Result<(), i32> {
        let mut data=[0u8;64];
        let n=hw.heap_info(&mut data);if n<0 {return Err(n);}
        let data=data.get(..n as usize).ok_or(ERR_SIZE)?;
        let field=|tag| -> Result<u32,i32> {
            u32::try_from(cbor::get_int(data,tag).map_err(|_|ERR_ARG)?.ok_or(ERR_ARG)?).map_err(|_|ERR_SIZE)
        };
        *response=SystemHeapInfo {free:field(1)?,largest:field(2)?,min_free:field(3)?,total:field(4)?,internal_free:field(5)?,internal_largest:field(6)?};
        Ok(())
    }
    /// Debug clients request native heap snapshots or control allocation tracing.
    fn heap_dump(&mut self, hw: &Hw, request: &SystemHeapDump) -> Result<(), i32> {
        if !matches!(request.kind,0|1|3|4|5|6) {return Err(ERR_ARG);}
        result(hw.heap_dump(request.kind))
    }
    /// Debug clients inspect task priorities and stack usage alongside heap diagnostics.
    fn dump_tasks(&mut self, hw: &Hw) -> Result<(), i32> { result(hw.heap_dump(2)) }
}
impl Module for State {
    /// Runtime lifecycle publishes the native table; requests use the generated dispatcher.
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_TABLE) && call.data==b"system" {
            return api_gen::provide::<State>(hw,call,core::ptr::addr_of_mut!(self.hosted),self as *mut State);
        }
        if call.component==api_gen::COMPONENT && call.wants_response() && call.flags & CALL_LINK==0 {
            let mut out=[0u8;96];
            match api_gen::dispatch(self,hw,call.method,call.data,&mut out) {
                Ok(n)=>{hw.reply(call,&out[..n]);},Err(rc)=>{hw.reply_error(call,rc);}
            }
        }
        RET_CONTINUE as i32
    }
}
#[macro_export]
macro_rules! export { () => { xip_abi::export_module!($crate::State,state=$crate::STATE_BYTES); }; }
#[cfg(feature="sim")]
export!();

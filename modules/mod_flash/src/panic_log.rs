//! Persist a retained panic only after the ordinary filesystem commit succeeds.
use xip_abi::{Hw, CallView, cbor};
use xip_abi::ModulePanic as Service;
pub const ID: u64 = 0x70616e69636c6f67;
fn service(hw: Hw) -> Option<&'static Service> {
    let name = unsafe { core::ffi::CStr::from_bytes_with_nul_unchecked(b"module-panic\0") };
    let s = unsafe { hw.get_component(name).cast::<Service>().as_ref()? };
    (s.version == 1 && s.size as usize >= core::mem::size_of::<Service>()).then_some(s)
}
pub fn acknowledge(hw: Hw, call: &CallView) -> bool {
    if call.component != 15 || call.method != 19 || call.id != ID { return false; }
    if matches!(cbor::Reader::new(call.data).next(), Some(cbor::Item::Map(_))) {
        if let Some(s) = service(hw) { unsafe { (s.logged)(hw.ctx()); } }
        let _=hw.info_publish(44,101,b"\xa2\x00\x05\x01\xf5");
    }
    true
}
pub fn available(hw: Hw) -> bool {
    let Some(s)=service(hw) else { return false; };
    let mut data=[0;256]; let mut address=0;
    unsafe { (s.read)(hw.ctx(),data.as_mut_ptr(),data.len(), &mut address) > 0 }
}
pub fn pending(hw: Hw, files: &mut crate::files::Files) -> bool {
    let Some(s) = service(hw) else { return false; };
    let mut payload = [0; 256]; let mut address = 0;
    let n = unsafe { (s.read)(hw.ctx(), payload.as_mut_ptr(), payload.len(), &mut address) };
    if n <= 0 || n as usize > payload.len() { return false; }
    let _=hw.info_publish(44,101,&payload[..n as usize]);
    let Ok(layout) = crate::store::Layout::discover(hw) else { return false; };
    let mut newest = 0u64;
    if layout.logs(hw, |b| { newest = newest.max(b.last()); Ok(()) }).is_err() { return false; }
    let Some(seq) = newest.checked_add(1) else { return false; };
    let mut request = [0; 320]; let mut w = cbor::Writer::new(&mut request);
    w.map(3).uint(10).uint(seq).uint(11).uint(1).uint(13).bytes(&payload[..n as usize]);
    let Some(data) = w.finish() else { return false; };
    let call = CallView { component:15, method:19, value:0, flags:0, data,
        time_us:0, pkt:0, from:address, id:ID, request_id:None, src:None };
    files.request(hw, &call) >= 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::{c_void,CStr};
    struct Fake { acknowledged: bool }
    unsafe extern "C" fn report(_: *mut c_void, _: *const u8, _: usize) {}
    unsafe extern "C" fn read(_: *mut c_void, out: *mut u8, cap: usize, address: *mut u32) -> i32 {
        if cap < 1 { return xip_abi::ERR_SIZE; }
        *out=0xa0; *address=7; 1
    }
    unsafe extern "C" fn logged(user: *mut c_void) { (*(user as *mut Fake)).acknowledged=true; }
    static SERVICE: Service=Service { version:1, size:core::mem::size_of::<Service>() as u32, report, read, logged };
    unsafe extern "C" fn get(_: *mut c_void, name: *const core::ffi::c_char) -> *const c_void {
        if CStr::from_ptr(name).to_bytes()==b"module-panic" { (&SERVICE as *const Service).cast() } else { core::ptr::null() }
    }
    #[test]
    fn failed_commit_retains_panic_and_success_acknowledges_it() {
        let mut fake=Fake{acknowledged:false};
        let host=xip_abi::Host { abi_version:xip_abi::ABI_VERSION, size:core::mem::size_of::<xip_abi::Host>() as u32,
            user:(&mut fake as *mut Fake).cast(), get_component:Some(get), ..unsafe{core::mem::zeroed()} };
        let hw=unsafe{Hw::new(&host)}.unwrap();
        assert!(available(hw));
        let mut call=CallView{component:15,method:19,id:ID,from:7,value:0,flags:0,data:&[0x20],time_us:0,pkt:0,request_id:None,src:None};
        assert!(acknowledge(hw,&call)); assert!(!fake.acknowledged);
        call.id=0; call.data=&[0xa0]; assert!(!acknowledge(hw,&call)); assert!(!fake.acknowledged);
        call.id=ID; assert!(acknowledge(hw,&call)); assert!(fake.acknowledged);
    }
}

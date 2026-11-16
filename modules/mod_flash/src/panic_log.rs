//! Persist a retained panic only after the ordinary filesystem commit succeeds.
use xip_abi::{Hw, CallView, cbor};
use alloc::vec::Vec;
fn buffer(len:usize,value:u8)->Option<Vec<u8>> {let mut v=Vec::new();v.try_reserve_exact(len).ok()?;v.resize(len,value);Some(v)}
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
    { let rc=unsafe { (s.read)(hw.ctx(),data.as_mut_ptr(),data.len(), &mut address) }; rc>0 || rc==xip_abi::ERR_SIZE }
}
pub fn pending(hw: Hw, files: &mut crate::files::Files) -> bool {
    let Some(s) = service(hw) else { return false; };
    let Some(mut payload)=buffer(1024,0)else{return false;}; let mut address = 0;
    let n = unsafe { (s.read)(hw.ctx(), payload.as_mut_ptr(), payload.len(), &mut address) };
    if n <= 0 || n as usize > payload.len() { return false; }
    let _=hw.info_publish(44,101,&payload[..n as usize]);
    let Ok(layout) = crate::store::Layout::discover(hw) else { return false; };
    let mut newest = 0u64;
    if layout.logs(hw, |b| { newest = newest.max(b.last()); Ok(()) }).is_err() { return false; }
    let Some(seq) = newest.checked_add(1) else { return false; };
    let Some(mut request)=buffer(payload.len()+64,0)else{return false;}; let mut w = cbor::Writer::new(&mut request);
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
    unsafe extern "C" fn arm(_: *mut c_void, _: u32, _: u32)->i32 { 0 }
    unsafe extern "C" fn range(user:*mut c_void, address:*mut u32, len:*mut u32){*address=0;*len=*(user as *const u32);}
    unsafe extern "C" fn disarm(_: *mut c_void){}
    static FLASH:xip_abi::PanicFlash=xip_abi::PanicFlash{version:1,size:core::mem::size_of::<xip_abi::PanicFlash>()as u32,arm,range,disarm};
    unsafe extern "C" fn get_flash(_: *mut c_void,name:*const core::ffi::c_char)->*const c_void {
        if CStr::from_ptr(name).to_bytes()==b"panic-flash"{(&FLASH as *const xip_abi::PanicFlash).cast()}else{core::ptr::null()}
    }
    static BUSY_ARMS:core::sync::atomic::AtomicUsize=core::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn busy_arm(_: *mut c_void,_:u32,_:u32)->i32{BUSY_ARMS.fetch_add(1,core::sync::atomic::Ordering::SeqCst);xip_abi::ERR_BUSY}
    unsafe extern "C" fn empty_range(_: *mut c_void,at:*mut u32,len:*mut u32){*at=0;*len=0;}
    static USED_FLASH:xip_abi::PanicFlash=xip_abi::PanicFlash{version:1,size:core::mem::size_of::<xip_abi::PanicFlash>()as u32,arm:busy_arm,range:empty_range,disarm};
    unsafe extern "C" fn get_used(_: *mut c_void,name:*const core::ffi::c_char)->*const c_void {
        if CStr::from_ptr(name).to_bytes()==b"panic-flash"{(&USED_FLASH as *const xip_abi::PanicFlash).cast()}else{core::ptr::null()}
    }
    #[test]
    fn occupied_panic_slot_is_not_prepared_again_until_invalidated(){
        let mut fake=crate::tests::Fake::new(&[],0,false);
        fake.flash[0x8000+12..0x8000+16].copy_from_slice(b"xip\0");
        fake.flash[0x8000+8..0x8000+12].copy_from_slice(&0x3f0000u32.to_le_bytes());
        let at=0x190000;
        fake.flash[at]=b'D';fake.flash[at+4..at+8].copy_from_slice(&65536u32.to_le_bytes());
        fake.flash[at+16..at+20].copy_from_slice(&[0xb1,255,12,4]);
        fake.flash[at+20..at+28].copy_from_slice(&PANIC_NAME);
        fake.flash[at+28..at+32].copy_from_slice(&1u32.to_le_bytes());
        fake.flash[at+32..at+36].copy_from_slice(b"PFL1");
        let mut host=fake.host();host.get_component=Some(get_used);
        let hw=unsafe{Hw::new(&host)}.unwrap();let mut files=crate::files::Files::default();let mut occupied=false;
        BUSY_ARMS.store(0,core::sync::atomic::Ordering::SeqCst);
        assert!(!prepare(hw,&mut files,&mut occupied));assert!(occupied);
        for _ in 0..10{assert!(!prepare(hw,&mut files,&mut occupied));}
        assert_eq!(BUSY_ARMS.load(core::sync::atomic::Ordering::SeqCst),1);
        occupied=false;assert!(!prepare(hw,&mut files,&mut occupied));
        assert_eq!(BUSY_ARMS.load(core::sync::atomic::Ordering::SeqCst),2);
    }
    #[test]
    fn preparation_gate_skips_unsupported_and_armed_hosts(){
        let mut size=0u32;
        let mut host=xip_abi::Host{abi_version:xip_abi::ABI_VERSION,size:core::mem::size_of::<xip_abi::Host>()as u32,
            user:(&mut size as *mut u32).cast(),get_component:Some(get),..unsafe{core::mem::zeroed()}};
        assert!(!needs_prepare(unsafe{Hw::new(&host)}.unwrap()));
        host.get_component=Some(get_flash);
        assert!(needs_prepare(unsafe{Hw::new(&host)}.unwrap()));
        size=1024;assert_eq!(size,1024);
        assert!(!needs_prepare(unsafe{Hw::new(&host)}.unwrap()));
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

pub const PREPARE_ID:u64=ID+1;
pub const PANIC_NAME:[u8;8]=*b"panic\0\0\0";
fn flash_service(hw:Hw)->Option<&'static xip_abi::PanicFlash>{
    let s=unsafe{hw.get_component(c"panic-flash").cast::<xip_abi::PanicFlash>().as_ref()?};
    (s.version==1 && s.size as usize>=core::mem::size_of::<xip_abi::PanicFlash>()).then_some(s)
}
pub fn pinned(hw:Hw,base:u32,len:u32)->bool {
    let Some(s)=flash_service(hw) else{return false;};let(mut at,mut size)=(0,0);
    unsafe{(s.range)(hw.ctx(),&mut at,&mut size);}
    size!=0 && base<at.saturating_add(size) && at<base.saturating_add(len)
}
pub fn disarm(hw:Hw){if let Some(s)=flash_service(hw){unsafe{(s.disarm)(hw.ctx());}}}
/// Cheap gate before allocating a filesystem worker on a tick.
pub fn needs_prepare(hw:Hw)->bool {
    let Some(s)=flash_service(hw)else{return false;};
    let(mut at,mut size)=(0,0);unsafe{(s.range)(hw.ctx(),&mut at,&mut size);}
    size==0
}
/// Called only after a successful filesystem commit; never starts another write.
/// True means the retained slot is occupied and must be removed before retrying.
pub fn arm_existing(hw:Hw)->bool {
    let Some(s)=flash_service(hw)else{return false;};
    let Ok(layout)=crate::store::Layout::discover(hw)else{return false;};
    if let Ok(Some(f))=layout.lookup(hw,PANIC_NAME){
        return arm_slot(hw,s,layout.base+f.at+16,f.size);
    }
    false
}
fn arm_slot(hw:Hw,s:&xip_abi::PanicFlash,at:u32,size:u32)->bool {
    size<1024 || unsafe{(s.arm)(hw.ctx(),at,size)}==xip_abi::ERR_BUSY
}
/// Normal worker operation only. A used/partial panic file is preserved until removed.
pub fn prepare(hw:Hw,files:&mut crate::files::Files,occupied:&mut bool)->bool{
    if *occupied{return false;}
    let Some(s)=flash_service(hw)else{return false;};
    let(mut at,mut size)=(0,0);unsafe{(s.range)(hw.ctx(),&mut at,&mut size);}
    if size!=0{return false;}
    let Ok(layout)=crate::store::Layout::discover(hw)else{return false;};
    match layout.lookup(hw,PANIC_NAME){
        Ok(Some(f))=>{*occupied=arm_slot(hw,s,layout.base+f.at+16,f.size);false}
        Ok(None)=>{
            let Some(mut request)=buffer(1100,0)else{return false;};let mut w=cbor::Writer::new(&mut request);
            let Some(blank)=buffer(1024,255)else{return false;};
            w.map(3).uint(1).bytes(&PANIC_NAME).uint(4).boolean(false).uint(13).bytes(&blank);
            let Some(data)=w.finish()else{return false;};
            let call=CallView{component:15,method:18,value:0,flags:0,data,time_us:0,pkt:0,from:44,id:PREPARE_ID,request_id:None,src:None};
            files.request(hw,&call)>=0
        }
        _=>false,
    }
}

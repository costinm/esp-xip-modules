//! Optional native memory backend. Leases pin storage; only bytes go over the wire.
use xip_abi::{Hw, MemoryFile, MemoryFiles, ERR_UNSUPPORTED};
pub fn service(hw: Hw) -> Result<&'static MemoryFiles,i32> {
    let p=hw.get_component(c"memory-files").cast::<MemoryFiles>();
    let s=unsafe {p.as_ref()}.ok_or(ERR_UNSUPPORTED)?;
    if s.version!=1 || (s.size as usize)<core::mem::size_of::<MemoryFiles>() {return Err(ERR_UNSUPPORTED);}
    Ok(s)
}
pub struct Lease { pub region: MemoryFile, ctx: *mut core::ffi::c_void, service: &'static MemoryFiles }
impl Lease {
    pub fn open(hw: Hw,ns:u8,name:&[u8],mode:u8,size:u32,version:u32)->Result<Self,i32> {
        let service=service(hw)?;
        let mut region=unsafe{core::mem::zeroed()};
        let rc=unsafe{(service.open)(hw.ctx(),ns,name.as_ptr(),name.len(),mode,size,version,&mut region)};
        if rc<0{return Err(rc);}
        Ok(Self{region,ctx:hw.ctx(),service})
    }
    pub fn commit(&mut self)->Result<(),i32>{
        let token=core::mem::take(&mut self.region.token);
        let rc=unsafe{(self.service.close)(self.ctx,token,true)};
        if rc<0{Err(rc)}else{Ok(())}
    }
}
impl Drop for Lease { fn drop(&mut self){if self.region.token!=0 {unsafe{(self.service.close)(self.ctx,self.region.token,false);}}} }

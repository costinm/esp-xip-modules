//! Physical references outlive their producer; only TX BUSY retains ownership.
use xip_abi::*;
use xip_runtime_host::{Config,NativeTx,Runtime};
#[repr(C)]
struct Owner { packets:[u32;20] }
impl Module for Owner {
    fn handle(&mut self,hw:&Hw,call:&CallView)->i32 {
        if call.is_host(HOST_START) {
            for p in &mut self.packets { *p=hw.pkt_alloc().unwrap(); }
        } else if call.is_host(HOST_STOP) {
            for p in &mut self.packets {if *p!=0 {hw.pkt_free(*p);*p=0;}}
        } else if call.method==1 {
            let h=self.packets[0];
            let (ptr,_)=hw.pkt_buf(h).unwrap();
            unsafe {core::ptr::copy_nonoverlapping(b"multipath".as_ptr(),ptr,9);}
            let host=unsafe {&*hw.as_ptr()};
            let tx=host.pkt_tx.unwrap();
            let busy=unsafe {tx(hw.ctx(),h,PKT_HEADROOM,9,ADDR_LINK,17)};
            assert_eq!(busy,ERR_BUSY);
            assert_eq!(unsafe {core::slice::from_raw_parts(ptr,9)},b"multipath");
            assert_eq!(unsafe {tx(hw.ctx(),h,PKT_HEADROOM,9,ADDR_LINK+1,17)},0);
            self.packets[0]=0;
        }
        RET_CONTINUE as i32
    }
}
unsafe extern "C" fn entry(host:*const Host,state:*mut u8,call:*const Call)->i32 {
    let hw=Hw::new(host).unwrap();let view=CallView::new(call).unwrap();
    let packet=(view.pkt!=0).then(||Packet::from_raw(hw,view.pkt));
    (&mut *state.cast::<Owner>()).handle_owned(&hw,&view,packet)
}
#[test]
fn no_module_quota_busy_keeps_bytes_and_driver_finishes_after_stop() {
    let rt=Runtime::new(Config{pool_slots:32,..Default::default()});
    rt.add_native_link("busy",1500,NativeTx::Busy);
    rt.add_native_link("async",1500,NativeTx::Async);
    let owner=rt.add_handler("owner",entry,core::mem::size_of::<Owner>());
    owner.set_component(21);owner.start(b"");rt.run(0);
    assert_eq!(rt.packets_in_use(),20,"no eight-entry table or per-module quota");
    assert_eq!(owner.send(21,1,0,b"request"),0);rt.run(0);
    assert_eq!(rt.take_link_tx("async"),vec![(17,b"multipath".to_vec())]);
    assert_eq!(rt.packets_in_use(),20,"incoming guard released its reference");
    owner.stop();rt.run(0);
    assert_eq!(rt.packets_in_use(),1,"the driver retains its accepted reference");
    rt.native_tx_done("async",0);rt.run(0);
    assert_eq!(rt.packets_in_use(),0);
}

//! Private bridge from the generic C socket HAL to the shared Rust poller.
use alloc::boxed::Box;
use core::ffi::c_void;
use crate::lwip_io_esp::{self as io, Registration};
extern "C" {
    fn xip_socket_ready(fd:i32,events:i16)->i32;
    fn xip_socket_poll_install(watch:unsafe extern "C" fn(i32,i16)->*mut c_void,
        update:unsafe extern "C" fn(*mut c_void,i32,i16),clear:unsafe extern "C" fn(*mut c_void));
}
fn ready(reg:Registration,fd:i32,events:i16){
    io::suspend(reg);
    if unsafe{xip_socket_ready(fd,events)}==xip_abi::ERR_BUSY{io::update(reg,fd,events,10);}
}
unsafe extern "C" fn watch(fd:i32,events:i16)->*mut c_void{
    io::register_socket(fd,events,ready).map(|r|Box::into_raw(Box::new(r)).cast()).unwrap_or(core::ptr::null_mut())
}
unsafe extern "C" fn update(ptr:*mut c_void,fd:i32,events:i16){if !ptr.is_null(){io::update(*(ptr as *const Registration),fd,events,0);}}
unsafe extern "C" fn clear(ptr:*mut c_void){if !ptr.is_null(){let reg=Box::from_raw(ptr as *mut Registration);io::clear(*reg);}}
pub(crate) fn install(){unsafe{xip_socket_poll_install(watch,update,clear);}}

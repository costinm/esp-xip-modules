//! The `power` component fixture: a request is recorded (and counted as done at once), and `info` reports it. No real sleep, no real radio.
use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use xip_abi::{Power, PowerInfo, PowerSleep, ERR_ARG, ERR_SIZE, POWER_VERSION};

#[derive(Default)]
pub(crate) struct Fixture {
    pub requests: Vec<PowerSleep>,
}

thread_local! {
    pub(crate) static FIX: RefCell<Fixture> = RefCell::new(Fixture::default());
}

unsafe extern "C" fn sleep(_ctx: *mut c_void, r: *const PowerSleep) -> c_int {
    if r.is_null() || (unsafe { (*r).size } as usize) < core::mem::size_of::<PowerSleep>() || unsafe { (*r).duration_ms } < 10 { return ERR_ARG; }
    FIX.with(|f| f.borrow_mut().requests.push(unsafe { *r }));
    0
}

unsafe extern "C" fn info(_ctx: *mut c_void, i: *mut PowerInfo) -> c_int {
    if i.is_null() { return ERR_ARG; }
    if (unsafe { (*i).size } as usize) < core::mem::size_of::<PowerInfo>() { return ERR_SIZE; }
    let n = FIX.with(|f| f.borrow().requests.iter().map(|r| u32::from(r.cycles.max(1))).sum::<u32>());
    let mut out = PowerInfo::default();
    out.sleep_attempts = n;
    out.sleep_entries = n;
    unsafe { *i = out };
    0
}

unsafe extern "C" fn dump(_ctx: *mut c_void) -> c_int { 0 }

static COMPONENT: Power = Power {
    version: POWER_VERSION,
    size: core::mem::size_of::<Power>() as u32,
    sleep: Some(sleep),
    info: Some(info),
    dump_locks: Some(dump),
    dump_tasks: Some(dump),
};

pub(crate) fn component() -> *const Power { &COMPONENT }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleep_request_is_validated_and_counted() {
        unsafe {
            let mut r = PowerSleep::default();
            r.duration_ms = 5;
            assert_eq!(sleep(core::ptr::null_mut(), &r), ERR_ARG);
            r.duration_ms = 500;
            r.cycles = 3;
            assert_eq!(sleep(core::ptr::null_mut(), &r), 0);
            let mut i = PowerInfo::default();
            assert_eq!(info(core::ptr::null_mut(), &mut i), 0);
            assert_eq!((i.sleep_attempts, i.sleep_entries), (3, 3));
        }
    }
}

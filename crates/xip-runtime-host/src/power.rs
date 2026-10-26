//! The `power` component fixture: a request is recorded (and counted as done at once), and `info` reports it. No real sleep, no real radio.
use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use xip_abi::{Power, PowerInfo, PowerSleep, ERR_ARG, ERR_SIZE, POWER_VERSION};

#[derive(Default)]
pub(crate) struct Fixture {
    pub requests: Vec<PowerSleep>,
    pub deadlines: Vec<(u64, u32)>,
    pub active: bool,
    pub awake_until: u64,
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
    out.active_window = u8::from(FIX.with(|f| f.borrow().active));
    out.sleep_attempts = n;
    out.sleep_entries = n;
    unsafe { *i = out };
    0
}

unsafe extern "C" fn dump(_ctx: *mut c_void) -> c_int { 0 }

unsafe extern "C" fn until(_ctx: *mut c_void, at: u64, flags: u32) -> c_int {
    if flags & !xip_abi::POWER_TRACE_UART != 0 { return ERR_ARG; }
    FIX.with(|f| f.borrow_mut().deadlines.push((at, flags)));
    0
}
unsafe extern "C" fn cancel(_ctx: *mut c_void) -> c_int { 0 }
unsafe extern "C" fn activate(_ctx: *mut c_void, ms: u32) -> c_int {
    FIX.with(|f| f.borrow_mut().active = ms != 0);
    0
}
fn extend_awake(until: &mut u64, now: u64, ms: u32) -> i32 {
    if ms>300_000 {return ERR_ARG;}
    if ms!=0 {*until=(*until).max(now+ms as u64);}
    until.saturating_sub(now).min(300_000) as i32
}
unsafe extern "C" fn awake_for(_ctx: *mut c_void, ms: u32) -> c_int {
    if ms > 300_000 { return ERR_ARG; }
    let now = unsafe { crate::hostcalls::ctx(_ctx) }.0.st.borrow().now;
    FIX.with(|f| extend_awake(&mut f.borrow_mut().awake_until,now,ms))
}
static COMPONENT: Power = Power {
    version: POWER_VERSION,
    size: core::mem::size_of::<Power>() as u32,
    sleep: Some(sleep),
    info: Some(info),
    dump_locks: Some(dump),
    dump_tasks: Some(dump),
    sleep_until: Some(until),
    cancel: Some(cancel),
    activate: Some(activate),
    awake_for: Some(awake_for),
    received: None,
};

pub(crate) fn component() -> *const Power { &COMPONENT }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_awake_deadline_renews_expires_and_cannot_be_shortened() {
        let mut until=0;
        assert_eq!(extend_awake(&mut until,10,300001),ERR_ARG);assert_eq!(until,0);
        assert_eq!(extend_awake(&mut until,10,300000),300000);
        assert_eq!(extend_awake(&mut until,20,1),299990);
        assert_eq!(extend_awake(&mut until,20,0),299990);
        assert_eq!(extend_awake(&mut until,300011,0),0);
        assert_eq!(extend_awake(&mut until,300011,5000),5000);
    }
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

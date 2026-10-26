//! Module policy only: the runtime owns UART, radio teardown and sleep entry.
use crate::protocol::Plan;
use xip_abi::*;

// Same conservative cold C6/S3 margin as FW. Subsequent slow restarts grow it.
const RESUME_FLOOR_US: u64 = 130_000;
const RESUME_MARGIN_US: u64 = 25_000;
#[derive(Default)]
pub struct Policy {
    pub enabled: bool,
    pub waiting: bool,
    pub resume_lead_us: u64,
    trace: bool,
}
impl Policy {
    /// Activation replies query whether the operator requested boundary diagnostics.
    pub fn tracing(&self) -> bool { self.trace }
    fn power(hw: &Hw) -> Result<&'static Power, i32> {
        let p = hw.power().ok_or(ERR_UNSUPPORTED)?;
        if p.version < 2 || (p.size as usize) < core::mem::size_of::<Power>()
            || p.sleep_until.is_none() || p.cancel.is_none() || p.activate.is_none() || p.info.is_none() {
            return Err(ERR_UNSUPPORTED);
        }
        Ok(p)
    }
    /// NAN policy arms coordinated sleep while preserving decoded-packet activity grace.
    pub fn enable(&mut self, hw: &Hw, trace: bool) -> Result<(), i32> {
        Self::power(hw)?;
        // Activity from the command is a real directed packet too; its grace
        // expires naturally before native sleep can be admitted.
        self.enabled = true;
        self.trace = trace;
        self.resume_lead_us = RESUME_FLOOR_US;
        Ok(())
    }
    /// NAN stop, USB attachment or activation cancels its pending sleep.
    pub fn disable(&mut self, hw: &Hw) {
        if self.enabled || self.waiting {
            if let Ok(p) = Self::power(hw) { unsafe { p.cancel.unwrap()(hw.ctx()); } }
        }
        self.enabled = false;
        self.waiting = false;
    }
    /// The NAN scheduler requests native sleep only outside capture and owned work.
    pub fn between_windows(&mut self, hw: &Hw, plan: &Plan, pending: bool, now: u64) -> Result<bool, i32> {
        if !self.enabled || self.waiting || plan.capture || plan.acquiring || pending { return Ok(false); }
        let p = Self::power(hw)?;
        let mut info = PowerInfo::default();
        let rc = unsafe { p.info.unwrap()(hw.ctx(), &mut info) };
        if rc < 0 { self.disable(hw); return Err(rc); }
        if info.active_window != 0 { return Ok(false); }
        if p.awake_for.is_some_and(|f| unsafe { f(hw.ctx(), 0) } > 0) { return Ok(false); }
        self.resume_lead_us = RESUME_FLOOR_US.max(info.last_resume_us as u64 + RESUME_MARGIN_US);
        let wake_at = plan.next_us.saturating_sub(self.resume_lead_us);
        if wake_at < now + 10_000 { return Ok(false); }
        let rc = unsafe { p.sleep_until.unwrap()(hw.ctx(), wake_at, if self.trace { POWER_TRACE_UART } else { 0 }) };
        if rc == ERR_BUSY { return Ok(false); }
        if rc < 0 { self.disable(hw); return Err(rc); }
        self.waiting = true;
        Ok(true)
    }
    /// Returns true only after our radio-off boundary, not for unrelated wake
    /// notifications. Directed activity restores active roles until the shared grace expires.
    /// Runtime wake delivery clears the pending boundary and checks activity vetoes.
    pub fn wake(&mut self, hw: &Hw, cause: i32) -> bool {
        let was_waiting = self.waiting;
        self.waiting = false;
        if self.enabled {
            let mut info = PowerInfo::default();
            let active = Self::power(hw).ok().and_then(|p| p.info).is_some_and(|f|
                unsafe { f(hw.ctx(), &mut info) } == 0 && info.active_window != 0);
            if cause < 0 || (cause != 4 && (was_waiting || active)) || active { self.disable(hw); }
        }
        was_waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::{c_char, c_void, CStr};
    #[derive(Default)]
    struct Fixture { active: bool, deadline: u64, flags: u32, cancelled: usize, resume_us: u32, reject: i32, held_ms: i32 }
    unsafe extern "C" fn info(ctx: *mut c_void, out: *mut PowerInfo) -> i32 {
        let f = &*(ctx as *const Fixture);
        (*out).active_window = u8::from(f.active);
        (*out).last_resume_us = f.resume_us;
        0
    }
    unsafe extern "C" fn activate(ctx: *mut c_void, ms: u32) -> i32 {
        (*(ctx as *mut Fixture)).active = ms != 0; 0
    }
    unsafe extern "C" fn until(ctx: *mut c_void, at: u64, flags: u32) -> i32 {
        let f = &mut *(ctx as *mut Fixture); f.deadline = at; f.flags = flags; f.reject
    }
    unsafe extern "C" fn cancel(ctx: *mut c_void) -> i32 {
        (*(ctx as *mut Fixture)).cancelled += 1; 0
    }
    unsafe extern "C" fn awake_for(ctx: *mut c_void, _: u32) -> i32 { (*(ctx as *const Fixture)).held_ms }
    static POWER: Power = Power { version: POWER_VERSION, size: core::mem::size_of::<Power>() as u32,
        sleep: None, info: Some(info), dump_locks: None, dump_tasks: None,
        sleep_until: Some(until), cancel: Some(cancel), activate: Some(activate), awake_for: Some(awake_for), received: None };
    unsafe extern "C" fn component(_: *mut c_void, name: *const c_char) -> *const c_void {
        if CStr::from_ptr(name).to_bytes() == b"power" { (&POWER as *const Power).cast() } else { core::ptr::null() }
    }
    fn host(f: &mut Fixture) -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION;
        h.size = core::mem::size_of::<Host>() as u32;
        h.user = (f as *mut Fixture).cast(); h.get_component = Some(component); h
    }
    fn plan() -> Plan {
        Plan { capture: false, public: false, sync: false, slot: 8, next_us: 5_000_000,
            start_us: 5_008_000, end_us: 5_040_768, acquiring: false }
    }
    #[test]
    fn failed_runtime_admission_does_not_leave_policy_waiting() {
        let mut f=Fixture { reject: ERR_BUSY, ..Default::default() };
        let h=host(&mut f); let hw=unsafe { Hw::new(&h) }.unwrap();
        let mut p=Policy::default(); p.enable(&hw,false).unwrap();
        assert_eq!(p.between_windows(&hw,&plan(),false,1),Ok(false));
        assert!(p.enabled); assert!(!p.waiting);
    }
    #[test]
    fn phase_deadline_includes_resume_and_does_not_sleep_again_before_dw() {
        let mut f = Fixture { active: true, resume_us: 150_000, ..Default::default() };
        let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut p = Policy::default(); p.enable(&hw, true).unwrap();
        assert!(f.active); // enabling never discards the command's activity grace
        f.active=false;
        assert!(p.between_windows(&hw, &plan(), false, 1_000_000).unwrap());
        assert_eq!(f.deadline, 4_825_000); assert_eq!(f.flags, POWER_TRACE_UART);
        assert!(p.wake(&hw, 4)); assert!(p.enabled);
        assert!(!p.between_windows(&hw, &plan(), false, 4_825_000).unwrap());
    }
    #[test]
    fn acquisition_capture_and_owned_work_veto_sleep() {
        let mut f = Fixture::default(); let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut p = Policy::default(); p.enable(&hw, false).unwrap();
        let mut window = plan(); window.capture = true;
        assert!(!p.between_windows(&hw, &window, false, 1).unwrap());
        window.capture = false; window.acquiring = true;
        assert!(!p.between_windows(&hw, &window, false, 1).unwrap());
        assert!(!p.between_windows(&hw, &plan(), true, 1).unwrap());
        assert_eq!(f.deadline, 0);
    }
    #[test]
    fn uart_during_idle_or_radio_off_returns_policy_to_awake() {
        let mut f = Fixture::default(); let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut p = Policy::default(); p.enable(&hw, false).unwrap();
        f.active = true;
        assert!(!p.between_windows(&hw, &plan(), false, 1).unwrap());
        assert!(p.enabled); // grace defers, rather than permanently disabling cadence
        f.active=false;
        p.enable(&hw, false).unwrap();
        assert!(p.between_windows(&hw, &plan(), false, 1).unwrap());
        assert!(p.wake(&hw, 8)); assert!(!p.enabled); assert!(!p.waiting);
        assert_eq!(f.cancelled, 1);
    }
    #[test]
    fn rejected_sleep_disarms_and_stopping_cancels_once() {
        let mut f = Fixture::default(); let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut p = Policy::default(); p.enable(&hw, false).unwrap();
        p.between_windows(&hw, &plan(), false, 1).unwrap();
        assert!(p.wake(&hw, -1)); assert!(!p.enabled);
        p.enable(&hw, false).unwrap(); p.disable(&hw); p.disable(&hw);
        assert_eq!(f.cancelled, 2);
    }    #[test]
    fn temporary_work_hold_preserves_sleepy_policy_until_expiry() {
        let mut f=Fixture { held_ms:5000, ..Default::default() }; let h=host(&mut f); let hw=unsafe {Hw::new(&h)}.unwrap();
        let mut p=Policy::default();p.enable(&hw,false).unwrap();
        assert!(!p.between_windows(&hw,&plan(),false,1).unwrap());assert!(p.enabled);assert!(!p.waiting);
        f.held_ms=0;assert_eq!(f.held_ms,0);assert!(p.between_windows(&hw,&plan(),false,2).unwrap());assert!(p.enabled);
    }

}

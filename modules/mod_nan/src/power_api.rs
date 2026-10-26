//! NAN owns public power requests; native C retains the power mechanisms.
use super::*;

fn table(hw: &Hw) -> Result<&Power, i32> {
    let power = hw.power().ok_or(ERR_UNSUPPORTED)?;
    if power.version != POWER_VERSION || (power.size as usize) < core::mem::size_of::<Power>() {
        return Err(ERR_UNSUPPORTED);
    }
    Ok(power)
}
fn result(rc: i32) -> Result<(), i32> {
    if rc < 0 { Err(rc) } else { Ok(()) }
}
impl power_gen::Api for State {
    /// Remote diagnostic callers request timer cycles; normal NAN sleep uses sleepy policy.
    fn powersleep(&mut self, hw: &Hw, request: &PowerSleep) -> Result<(), i32> {
        let power = table(hw)?;
        result(unsafe { power.sleep.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), request) })
    }
    /// CLI and monitoring clients read runtime sleep counters without console dispatch.
    fn powerinfo(&mut self, hw: &Hw, response: &mut PowerInfo) -> Result<(), i32> {
        let power = table(hw)?;
        result(unsafe { power.info.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), response) })
    }
    /// Diagnostic clients ask the runtime to print the locks preventing sleep.
    fn dump_locks(&mut self, hw: &Hw) -> Result<(), i32> {
        let power = table(hw)?;
        result(unsafe { power.dump_locks.ok_or(ERR_UNSUPPORTED)?(hw.ctx()) })
    }
}

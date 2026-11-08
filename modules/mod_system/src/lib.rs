//! The `sys` module: one image for the wire components `sys` (49, memory and task diagnostics, device control and the native dispatch) and
//! `hw` (45, hardware policy). Each component is an independent crate under `crates/` (its API in its `API.md`); this crate composes them
//! into the image: the components table of [`export!`] routes each wire component to its part, and `Module::handle` takes the host calls
//! and the envelopes that no part serves.
#![no_std]
#[cfg(test)]
extern crate std;

#[cfg(feature="sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

use xip_abi::{CallView, Hw, Module, CALL_LINK, HOST_STOP, HOST_TABLE, RET_CONTINUE};

/// One field per component (public: the components table of [`export!`] names them). The state is a dynamic object of the runtime
/// (`state = dynamic`): the image asks for no state block (`--state 0`), so the module costs nothing while it is idle.
/// The state needs `Default` (zeroed) only.
#[derive(Default)]
pub struct State {
    pub hw: hw::State,
    pub sys: sys::State,
}

impl Module for State {
    /// Host calls (the lifecycle and the native table) and the envelopes that no part serves: a native envelope (for example `wifi` 10 or
    /// `ip` 12) is answered by the sys component.
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_STOP) {
            return hw::handle(&mut self.hw, hw, call);
        }
        if call.is_host(HOST_TABLE) && call.data==b"sys" {
            return sys::provide(&mut self.sys, hw, call);
        }
        if call.flags & CALL_LINK == 0 && call.is_envelope() {
            sys::serve_envelope(&mut self.sys, hw, call);
        }
        RET_CONTINUE as i32
    }
}

/// Export `xip_handle` for the flash image; the components table is the manifest's key 13 (`--wire 45:1,49:1`).
#[macro_export]
macro_rules! export { () => { xip_abi::export_module!($crate::State, state=dynamic,
    components=[(45,1,hw,::hw::handle),(49,1,sys,::sys::handle)]); }; }
#[cfg(feature="sim")]
export!();

#[cfg(test)]
mod tests {
    /// The image asks for no state block: the state is a runtime object (`state = dynamic`), created on the first call.
    #[test]
    fn the_cargo_toml_asks_for_no_state_block() {
        let toml = include_str!("../Cargo.toml");
        assert!(toml.contains("--state 0"), "Cargo.toml must say `--state 0`");
        assert!(include_str!("lib.rs").contains("state=dynamic"), "the export must use the dynamic state");
    }
}

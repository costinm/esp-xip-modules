//! Bounded live subscriptions. No observations, packets or device inventory.
use alloc::vec::Vec;
use crate::discovery::ReturnPath;
use xip_abi::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route { Native { address: u32, token: u64 }, Radio(ReturnPath) }
#[derive(Clone, Copy)]
pub struct Listener { pub route: Route, pub id: u64, pub until: u64 }
#[derive(Default)]
pub struct Watches { listeners: Vec<Listener> }
impl Watches {
    /// The watch request handler adds, renews or cancels the caller's observation route.
    pub fn register(&mut self, route: Route, id: u64, ms: u64, now: u64) -> Result<(), i32> {
        if ms > 300_000 { return Err(ERR_ARG); }
        if let Some(i) = self.listeners.iter().position(|s| s.route == route && s.id == id) { self.listeners.swap_remove(i); }
        self.listeners.retain(|s| s.until > now);
        if ms != 0 {
            if self.listeners.len() >= 4 { return Err(ERR_BUSY); }
            self.listeners.try_reserve(1).map_err(|_| ERR_NOMEM)?;
            self.listeners.push(Listener { route, id, until: now + ms * 1000 });
        }
        if self.listeners.is_empty() { self.listeners = Vec::new(); }
        Ok(())
    }
    /// The NAN scheduler uses this to wake in time to expire subscriptions.
    pub fn next_expiry(&self) -> Option<u64> { self.listeners.iter().map(|s| s.until).min() }
    /// Observation delivery reads the live routes without caching peer inventories.
    pub fn listeners(&self) -> &[Listener] { &self.listeners }
    /// The NAN tick removes expired routes and sends their completion events.
    pub fn expire(&mut self, now: u64, mut send: impl FnMut(Listener)) {
        let mut i=0;
        while i < self.listeners.len() {
            if self.listeners[i].until <= now { send(self.listeners.swap_remove(i)); } else { i+=1; }
        }
        if self.listeners.is_empty() { self.listeners = Vec::new(); }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_observations_have_no_history_and_free_registration_storage() {
        let mut w=Watches::default(); let route=Route::Native { address:1, token:2 };
        assert_eq!(w.listeners.capacity(),0);
        w.register(route,42,1000,10).unwrap();
        assert_eq!(w.next_expiry(),Some(1_000_010));
        let mut ended=0; w.expire(1_000_009, |_| ended+=1); assert_eq!(ended,0);
        w.expire(1_000_010, |s| { assert_eq!(s.id,42); ended+=1; });
        assert_eq!(ended,1); assert_eq!(w.listeners.capacity(),0);
        assert_eq!(w.register(route,42,300001,0),Err(ERR_ARG));
    }
    #[test]
    fn renewal_cancellation_and_capacity_are_bounded() {
        let mut w=Watches::default(); let route=Route::Native { address:1,token:2 };
        for id in 0..4 { w.register(route,id,5000,0).unwrap(); }
        assert_eq!(w.register(route,5,5000,0),Err(ERR_BUSY));
        w.register(route,0,10000,1).unwrap(); assert_eq!(w.listeners.len(),4);
        for id in 0..4 { w.register(route,id,0,2).unwrap(); }
        assert_eq!(w.listeners.capacity(),0);
    }
}

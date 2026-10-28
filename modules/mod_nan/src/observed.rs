//! A bounded live discovery stream. No peer inventory or retained payloads.
use xip_abi::*;
use dmesh_server::discovery_nodes::{self as wire, Observation, Options};

#[cfg(test)]
const WINDOW_US: u64 = 5_000_000;
const FIN_GRACE_US: u64 = 1_000_000;

#[derive(Default)]
pub struct Observed {
    // Managed allocation exists only while the caller is waiting for observations.
    session: usize,
}
struct Session {
    handle: u32,
    until: u64,
}
impl Observed {
    pub fn open(&mut self, hw: &Hw, handle: u32, now: u64, id: u64, options: Options) -> Result<(), i32> {
        if self.session != 0 { return Err(ERR_BUSY); }
        let stream = hw.stream().ok_or(ERR_UNSUPPORTED)?;
        if stream.write_pkt.is_none() || stream.close.is_none() { return Err(ERR_UNSUPPORTED); }
        let p = hw.mem_alloc(core::mem::size_of::<Session>()).cast::<Session>();
        if p.is_null() { return Err(ERR_NOMEM); }
        unsafe { p.write(Session { handle, until: now.saturating_add(u64::from(options.duration_ms)*1000) }); }
        self.session = p as usize;
        if let Err(rc)=self.write(hw,false,|out|wire::header(id,out)) {self.stop(hw);return Err(rc);}
        Ok(())
    }
    fn session(&self) -> Option<&Session> {
        if self.session == 0 { None } else { Some(unsafe { &*(self.session as *const Session) }) }
    }
    fn forget(&mut self, hw: &Hw) {
        let p = core::mem::replace(&mut self.session, 0);
        if p != 0 { hw.mem_free(p as *mut u8); }
    }
    pub fn stop(&mut self, hw: &Hw) {
        if let Some(s) = self.session() {
            if let Some(close) = hw.stream().and_then(|s| s.close) {
                unsafe { close(hw.ctx(), s.handle); }
            }
        }
        self.forget(hw);
    }
    pub fn event(&mut self, hw: &Hw, handle: u32, event: i32, empty: bool) -> i32 {
        if self.session().is_none_or(|s| s.handle != handle) { return ERR_NOTFOUND; }
        match event {
            STREAM_FIN => 0, // Request FIN closes only the peer's sending half.
            STREAM_CLOSED => { self.forget(hw); 0 }
            STREAM_DATA if empty => 0,
            _ => { self.stop(hw); ERR_UNSUPPORTED }
        }
    }
    /// Encode in the pool lease itself. Success transfers ownership to Stream;
    /// a refused write leaves ownership here and immediately returns the lease.
    fn write(&self, hw: &Hw, fin: bool, encode: impl FnOnce(&mut [u8]) -> Option<usize>) -> Result<(), i32> {
        let handle = self.session().ok_or(ERR_NOTFOUND)?.handle;
        let write = hw.stream().and_then(|s| s.write_pkt).ok_or(ERR_UNSUPPORTED)?;
        let packet = hw.pkt_alloc()?;
        let result = (|| {
            let (ptr, cap) = hw.pkt_buf(packet)?;
            let out = unsafe { core::slice::from_raw_parts_mut(ptr, cap) };
            let n = encode(out).ok_or(ERR_SIZE)?;
            let rc = unsafe { write(hw.ctx(), handle, packet, n as u32, u32::from(fin)) };
            if rc < 0 { Err(rc) } else { Ok(()) }
        })();
        if result.is_err() { hw.pkt_free(packet); }
        result
    }
    pub fn note(&mut self, hw: &Hw, peer: [u8; 6], rssi: i8, channel: u8, payload: &[u8], now: u64, via_now: bool) {
        let Some(s) = self.session() else { return; };
        if now >= s.until { self.expire(hw, now); return; }
        let result = self.write(hw, false, |out| {
            let mut row=Observation::new(payload,if via_now {"now"}else{"nan"});
            row.peer=Some(peer);row.rssi_dbm=Some(rssi as i16);row.channel=Some(channel);row.received_us=Some(now);
            row.encode(out)
        });
        if let Err(rc) = result {
            // Transient backpressure drops this event, never queues a retry.
            if !matches!(rc, ERR_BUSY | ERR_AGAIN | ERR_NOMEM | ERR_SIZE) { self.stop(hw); }
        }
    }
    pub fn next_expiry(&self, now: u64) -> Option<u64> {
        self.session().map(|s| if now < s.until { s.until } else {
            now.saturating_add(20_000).min(s.until.saturating_add(FIN_GRACE_US))
        })
    }
    pub fn expire(&mut self, hw: &Hw, now: u64) {
        if self.session().is_none_or(|s| now < s.until) { return; }
        // A terminal CBOR item makes an empty observation window distinguishable
        // from a truncated response. FIN transfers this packet to Stream too.
        let result = self.write(hw, true, |out| {
            if out.len()<wire::END.len(){return None;}
            out[..wire::END.len()].copy_from_slice(wire::END);Some(wire::END.len())
        });
        match result {
            Ok(()) => self.forget(hw),
            // Do not abort a stream merely because its last observation is
            // still pending. Retry only the terminal item, never event data.
            Err(ERR_BUSY | ERR_AGAIN | ERR_NOMEM)
                if self.session().is_some_and(|s| now < s.until.saturating_add(FIN_GRACE_US)) => {}
            Err(_) => self.stop(hw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, vec::Vec};
    #[derive(Default)]
    struct Fixture {
        live_bytes: usize,
        peak_bytes: usize,
        allocations: usize,
        fail_alloc: bool,
        next: u32,
        packets: BTreeMap<u32, Vec<u8>>,
        // Stream retains successful writes until its simulated completion.
        accepted: Vec<(u32, Vec<u8>, bool)>,
        rc: i32,
        cap: usize,
        closed: Vec<u32>,
        returned: usize,
    }
    unsafe fn f<'a>(p: *mut core::ffi::c_void) -> &'a mut Fixture { &mut *p.cast() }
    unsafe extern "C" fn allocate(p: *mut core::ffi::c_void, n: usize) -> *mut core::ffi::c_void {
        let f = f(p);
        if f.fail_alloc { return core::ptr::null_mut(); }
        assert_eq!(n, core::mem::size_of::<Session>());
        f.live_bytes += n;
        f.peak_bytes = f.peak_bytes.max(f.live_bytes);
        f.allocations += 1;
        std::alloc::alloc(core::alloc::Layout::new::<Session>()).cast()
    }
    unsafe extern "C" fn deallocate(p: *mut core::ffi::c_void, ptr: *mut core::ffi::c_void) -> i32 {
        f(p).live_bytes -= core::mem::size_of::<Session>();
        std::alloc::dealloc(ptr.cast(), core::alloc::Layout::new::<Session>());
        0
    }
    unsafe extern "C" fn packet(p: *mut core::ffi::c_void, out: *mut u32) -> i32 {
        let f = f(p); f.next += 1;
        f.packets.insert(f.next, std::vec![0; f.cap]); *out = f.next; 0
    }
    unsafe extern "C" fn buffer(p: *mut core::ffi::c_void, id: u32, out: *mut *mut u8, cap: *mut usize) -> i32 {
        let b = f(p).packets.get_mut(&id).expect("caller owns packet");
        *out = b.as_mut_ptr(); *cap = b.len(); 0
    }
    unsafe extern "C" fn release(p: *mut core::ffi::c_void, id: u32) -> i32 {
        assert!(f(p).packets.remove(&id).is_some(), "cannot free a stream-owned packet");
        f(p).returned += 1; 0
    }
    unsafe extern "C" fn write(p: *mut core::ffi::c_void, _: u32, id: u32, n: u32, fin: u32) -> i32 {
        let f = f(p);
        if f.rc < 0 { return f.rc; }
        let b = f.packets.remove(&id).unwrap();
        f.accepted.push((id, b[..n as usize].to_vec(), fin != 0)); 0
    }
    unsafe extern "C" fn close(p: *mut core::ffi::c_void, h: u32) -> i32 { f(p).closed.push(h); 0 }
    static STREAM: Stream = Stream { version: STREAM_VERSION, size: core::mem::size_of::<Stream>() as u32,
        write_pkt: Some(write), close: Some(close), ..unsafe { core::mem::zeroed() } };
    unsafe extern "C" fn component(_: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *const core::ffi::c_void {
        if core::ffi::CStr::from_ptr(name).to_bytes() == b"stream" { (&STREAM as *const Stream).cast() } else { core::ptr::null() }
    }
    fn host(f: &mut Fixture) -> Host {
        let mut h: Host = unsafe { core::mem::zeroed() };
        h.abi_version = ABI_VERSION; h.size = core::mem::size_of::<Host>() as u32;
        h.user = (f as *mut Fixture).cast(); h.mem_alloc = Some(allocate); h.mem_free = Some(deallocate);
        h.pkt_alloc = Some(packet); h.pkt_buf = Some(buffer); h.pkt_free = Some(release); h.get_component = Some(component); h
    }
    #[test]
    fn idle_has_no_allocation_and_stream_owns_successful_packets() {
        let mut f = Fixture { cap: 1500, ..Default::default() };
        let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut observed = Observed::default();
        observed.note(&hw, [2;6], -43, 6, b"signed payload", 1, false);
        assert_eq!((f.allocations, f.next), (0, 0));
        for _ in 0..3 {
            observed.open(&hw, 77, 10, 42, Options::default()).unwrap();
            let header=dmesh_server::tagged::decode(&f.accepted.last().unwrap().1).unwrap();
            assert!(wire::is_header(header));assert_eq!(header.id,Some(42),"wire request ID is distinct from stream handle");
            assert_eq!(f.live_bytes, core::mem::size_of::<Session>());
            assert_eq!(observed.open(&hw, 78, 11, 42, Options::default()), Err(ERR_BUSY));
            assert_eq!(observed.event(&hw, 77, STREAM_FIN, true), 0);
            observed.note(&hw, [2;6], -43, 6, b"signed payload", 11, false);
            let data = &f.accepted.last().unwrap().1;
            assert_eq!(crate::bytes(data, 6), Some(&b"signed payload"[..]));
            assert_eq!(cbor::get_int(data, 2).unwrap(), Some(-43));
            assert!(f.packets.is_empty());
            observed.expire(&hw, WINDOW_US + 10);
            assert_eq!(f.live_bytes, 0);
            assert!(f.accepted.last().unwrap().2);
            assert_eq!(f.returned, 0, "module never frees transferred packets");
            // Provider completion returns all its owned leases to the pool.
            f.accepted.clear();
        }
        assert_eq!(f.peak_bytes, core::mem::size_of::<Session>());
        assert!(f.closed.is_empty());
    }
    #[test]
    fn backpressure_errors_disconnect_and_stop_reclaim_session_storage() {
        let mut f = Fixture { cap: 1500, ..Default::default() };
        let h = host(&mut f); let hw = unsafe { Hw::new(&h) }.unwrap();
        let mut observed = Observed::default();
        f.fail_alloc = true;
        assert_eq!(observed.open(&hw, 1, 0, 42, Options::default()), Err(ERR_NOMEM));
        assert_eq!(f.live_bytes, 0); f.fail_alloc = false;
        for rc in [ERR_BUSY, ERR_AGAIN, ERR_NOMEM, ERR_SIZE, ERR_NOTFOUND] {
            f.rc=0; observed.open(&hw, 1, 0, 42, Options::default()).unwrap(); f.rc = rc;
            observed.note(&hw, [2;6], -40, 6, b"payload", 1, true);
            assert!(f.packets.is_empty()); f.accepted.clear();
            observed.stop(&hw); assert_eq!(f.live_bytes, 0);
        }
        f.rc = 0; f.cap = 1500;
        observed.open(&hw, 1, 0, 42, Options::default()).unwrap();
        f.cap=1; observed.note(&hw, [2;6], -40, 6, b"payload", 1, true);
        assert!(f.packets.is_empty());
        assert_eq!(observed.event(&hw, 2, STREAM_CLOSED, true), ERR_NOTFOUND);
        assert_eq!(observed.event(&hw, 1, STREAM_CLOSED, true), 0);
        assert_eq!(f.live_bytes, 0);
        f.cap=1500; observed.open(&hw, 1, 0, 42, Options::default()).unwrap(); f.rc = ERR_BUSY;
        observed.expire(&hw, WINDOW_US);
        assert_ne!(f.live_bytes, 0, "terminal waits for pending stream output");
        observed.expire(&hw, WINDOW_US + FIN_GRACE_US); assert_eq!(f.live_bytes, 0);
        assert!(f.packets.is_empty());
        f.rc=0; observed.open(&hw, 1, 0, 42, Options::default()).unwrap();
        assert_eq!(observed.event(&hw, 1, STREAM_DATA, false), ERR_UNSUPPORTED);
        assert_eq!(f.live_bytes, 0);
    }
}

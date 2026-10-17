//! The `wifi` component fixture: the wanted state a module sets, and a scripted answer for what is true now.
//! By default a station intent "joins" at once (its own name, 192.168.49.7, -40 dBm, channel 6); a test can set the state it wants instead.
//! The component functions take no context, so the fixture lives in a thread-local: one runtime per thread.
use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use xip_abi::{Wifi, WifiAp, WifiMonitor, WifiIntent, WifiState, WifiTuner, WifiTuning, ERR_ARG, TUNE_OFF, WIFI_TUNING_VERSION, WIFI_VERSION};

pub(crate) struct Fixture {
    pub intent: WifiIntent,
    pub generation: u32,
    pub state: WifiState,
    /// `Some`: the test scripted the state; `None`: derived from the intent.
    pub scripted: bool,
    /// A change has not been announced to the modules yet (`HOST_WIFI`).
    pub pending: bool,
    /// What a scan finds, and whether one is under way (the first call answers `ERR_BUSY`, the next returns the list).
    pub aps: Vec<WifiAp>,
    pub scanning: bool,
    /// The last accepted `monitor` request (`enable` 0 after it was switched off).
    pub monitor: Option<WifiMonitor>,
}

thread_local! {
    pub(crate) static TUNING: RefCell<WifiTuning> = RefCell::new(WifiTuning::default());
    pub(crate) static FIX: RefCell<Fixture> = RefCell::new(Fixture::new());
}

impl Fixture {
    pub fn new() -> Self {
        Fixture { intent: WifiIntent::default(), generation: 0, state: WifiState::default(), scripted: false, pending: false, aps: Vec::new(), scanning: false, monitor: None }
    }
    fn derive(&mut self) {
        if self.scripted { return; }
        let i = &self.intent;
        let mut st = WifiState::default();
        st.size = core::mem::size_of::<WifiState>() as u32;
        if i.radio == 1 {
            st.channel = if i.channel != 0 { i.channel } else { 6 };
            st.ap = i.ap;
            if i.sta == 1 {
                st.sta = 2; st.rssi = -40; st.ip4 = [192, 168, 49, 7];
                st.ssid_len = i.sta_ssid_len;
                st.ssid[..i.sta_ssid_len as usize].copy_from_slice(&i.sta_ssid[..i.sta_ssid_len as usize]);
            }
        }
        self.state = st;
    }
}

fn text_ok(s: &[u8], n: u8, max: usize, min: usize) -> bool {
    let n = n as usize;
    n <= max && n <= s.len() && (n == 0 || n >= min) && !s[..n].contains(&0)
}

unsafe extern "C" fn set(_ctx: *mut c_void, intent: *const WifiIntent, generation: *mut u32) -> c_int {
    if intent.is_null() || generation.is_null() { return ERR_ARG; }
    let i = unsafe { *intent };
    if (i.size as usize) < core::mem::size_of::<WifiIntent>() || i.radio > 1 || i.sta > 1 || i.ap > 1 || i.channel > 14 { return ERR_ARG; }
    if !text_ok(&i.sta_ssid, i.sta_ssid_len, 32, 1) || !text_ok(&i.ap_ssid, i.ap_ssid_len, 32, 1)
        || !text_ok(&i.sta_psk, i.sta_psk_len, 63, 8) || !text_ok(&i.ap_psk, i.ap_psk_len, 63, 8) { return ERR_ARG; }
    if i.sta == 1 && i.sta_ssid_len == 0 && i.bssid == [0; 6] { return ERR_ARG; }
    FIX.with(|f| {
        let mut f = f.borrow_mut();
        let mut next = i;
        next.size = core::mem::size_of::<WifiIntent>() as u32;
        next.reserved = [0; 2];
        let same = f.intent.radio == next.radio && f.intent.sta == next.sta && f.intent.ap == next.ap && f.intent.channel == next.channel
            && f.intent.bssid == next.bssid && f.intent.sta_ssid == next.sta_ssid && f.intent.sta_ssid_len == next.sta_ssid_len
            && f.intent.sta_psk == next.sta_psk && f.intent.sta_psk_len == next.sta_psk_len && f.intent.ap_ssid == next.ap_ssid
            && f.intent.ap_ssid_len == next.ap_ssid_len && f.intent.ap_psk == next.ap_psk && f.intent.ap_psk_len == next.ap_psk_len;
        if !same {
            f.intent = next;
            f.generation = f.generation.wrapping_add(1).max(1);
            f.pending = true;
            f.derive();
        }
        unsafe { *generation = f.generation };
    });
    0
}

unsafe extern "C" fn get(_ctx: *mut c_void, intent: *mut WifiIntent, generation: *mut u32) -> c_int {
    if intent.is_null() || generation.is_null() { return ERR_ARG; }
    if (unsafe { (*intent).size } as usize) < core::mem::size_of::<WifiIntent>() { return xip_abi::ERR_SIZE; }
    FIX.with(|f| {
        let f = f.borrow();
        unsafe { *intent = f.intent; *generation = f.generation };
    });
    0
}

unsafe extern "C" fn state(_ctx: *mut c_void, state: *mut WifiState) -> c_int {
    if state.is_null() || (unsafe { (*state).size } as usize) < core::mem::size_of::<WifiState>() { return ERR_ARG; }
    FIX.with(|f| unsafe { *state = f.borrow().state });
    0
}

unsafe extern "C" fn scan(_ctx: *mut c_void, out: *mut WifiAp, max: usize) -> c_int {
    if out.is_null() || max == 0 { return ERR_ARG; }
    FIX.with(|f| {
        let mut f = f.borrow_mut();
        if !f.scanning { f.scanning = true; return xip_abi::ERR_BUSY; }
        f.scanning = false;
        let n = f.aps.len().min(max).min(16);
        for (i, ap) in f.aps[..n].iter().enumerate() { unsafe { out.add(i).write(*ap) }; }
        n as c_int
    })
}

unsafe extern "C" fn monitor(_ctx: *mut c_void, m: *const WifiMonitor) -> c_int {
    if m.is_null() { return ERR_ARG; }
    let m = unsafe { *m };
    if (m.size as usize) < core::mem::size_of::<WifiMonitor>() || m.channel > 13 { return ERR_ARG; }
    FIX.with(|f| {
        let mut f = f.borrow_mut();
        if f.intent.radio != 1 { return xip_abi::ERR_UNSUPPORTED; }
        f.monitor = Some(m);
        0
    })
}

static COMPONENT: Wifi = Wifi {
    version: WIFI_VERSION,
    size: core::mem::size_of::<Wifi>() as u32,
    set: Some(set),
    get: Some(get),
    state: Some(state),
    scan: Some(scan),
    monitor: Some(monitor),
};

pub(crate) fn component() -> *const Wifi { &COMPONENT }

unsafe extern "C" fn tuning_set(_ctx: *mut c_void, t: *const WifiTuning) -> c_int {
    if t.is_null() { return ERR_ARG; }
    let mut t = unsafe { *t };
    let ok = |v: u16, lo: u16, hi: u16| v == 0 || (lo..=hi).contains(&v);
    if (t.size as usize) < core::mem::size_of::<WifiTuning>()
        || [t.ampdu, t.rates_11b, t.raw_rx, t.driver_tx, t.bssid_check].iter().any(|v| *v > TUNE_OFF)
        || !(ok(t.rx_static, 2, 64) && ok(t.rx_dynamic, 2, 128) && ok(t.tx_dynamic, 4, 128) && ok(t.rx_ba, 2, 64)) { return ERR_ARG; }
    t.size = core::mem::size_of::<WifiTuning>() as u32;
    t.reserved = [0; 2];
    let changed = TUNING.with(|c| {
        let mut c = c.borrow_mut();
        let same = c.to_bytes() == t.to_bytes();
        *c = t;
        !same
    });
    if changed { FIX.with(|f| { let mut f = f.borrow_mut(); f.generation = f.generation.wrapping_add(1).max(1); f.pending = true; }); }
    0
}

unsafe extern "C" fn tuning_get(_ctx: *mut c_void, t: *mut WifiTuning) -> c_int {
    if t.is_null() { return ERR_ARG; }
    if (unsafe { (*t).size } as usize) < core::mem::size_of::<WifiTuning>() { return xip_abi::ERR_SIZE; }
    TUNING.with(|c| unsafe { *t = *c.borrow() });
    0
}

static TUNER: WifiTuner = WifiTuner {
    version: WIFI_TUNING_VERSION,
    size: core::mem::size_of::<WifiTuner>() as u32,
    set: Some(tuning_set),
    get: Some(tuning_get),
};

pub(crate) fn tuner() -> *const WifiTuner { &TUNER }

trait Bytes { fn to_bytes(&self) -> Vec<u8>; }
impl Bytes for WifiTuning {
    fn to_bytes(&self) -> Vec<u8> {
        unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), core::mem::size_of::<Self>()) }.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_and_monitor_follow_the_component_contract() {
        let mut ap = WifiAp { ssid: [0; 32], ssid_len: 4, bssid: [1; 6], rssi: -50, channel: 11, auth: 3 };
        ap.ssid[..4].copy_from_slice(b"home");
        FIX.with(|f| *f.borrow_mut() = Fixture::new());
        FIX.with(|f| f.borrow_mut().aps = vec![ap; 3]);
        let mut out = [ap; 8];
        unsafe {
            // the first call starts the scan; the next returns the list once
            assert_eq!(scan(core::ptr::null_mut(), out.as_mut_ptr(), 8), xip_abi::ERR_BUSY);
            assert_eq!(scan(core::ptr::null_mut(), out.as_mut_ptr(), 2), 2);
            assert_eq!(scan(core::ptr::null_mut(), out.as_mut_ptr(), 8), xip_abi::ERR_BUSY);
            // promiscuous receive needs the radio on and a filter
            let mut m = WifiMonitor::default();
            m.enable = 1; m.channel = 6;
            assert_eq!(monitor(core::ptr::null_mut(), &m), xip_abi::ERR_UNSUPPORTED);
            FIX.with(|f| f.borrow_mut().intent.radio = 1);
            assert_eq!(monitor(core::ptr::null_mut(), &m), 0);
            m.channel = 14;
            assert_eq!(monitor(core::ptr::null_mut(), &m), ERR_ARG);
            m.channel = 0;
            m.enable = 0;
            assert_eq!(monitor(core::ptr::null_mut(), &m), 0);
        }
        assert_eq!(FIX.with(|f| f.borrow().monitor.map(|m| m.enable)), Some(0));
    }
}

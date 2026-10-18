//! The `wifi` component of the host-runtime: a module states the wanted radio state, the runtime tells every module
//! (`HOST_WIFI`), and the state a test scripts is what the module reads back.
mod common;
use common::notify;
use xip_abi::*;
use xip_runtime_host::Runtime;

/// START: set an AP+STA intent. HOST_WIFI: emit the generation (event 1) and what `state` says (event 2: sta, ap, channel).
unsafe extern "C" fn module(host: *const Host, _state: *mut u8, call: *const Call) -> i32 {
    let h = &*host;
    let call = &*call;
    let wifi = &*(h.get_component.unwrap()(h.user, c"wifi".as_ptr()) as *const Wifi);
    if call.component != HOST_COMPONENT as i32 { return RET_CONTINUE as i32; }
    if call.method == HOST_START as i32 {
        let mut i = WifiIntent::default();
        i.radio = 1; i.sta = 1; i.ap = 1; i.channel = 11; i.bssid = [1, 2, 3, 4, 5, 6];
        i.sta_ssid[..4].copy_from_slice(b"home"); i.sta_ssid_len = 4;
        i.sta_psk[..8].copy_from_slice(b"password"); i.sta_psk_len = 8;
        let mut generation = 0;
        assert_eq!((wifi.set.unwrap())(h.user, &i, &mut generation), 0);
        // an invalid intent is refused and changes nothing
        i.channel = 15;
        assert_eq!((wifi.set.unwrap())(h.user, &i, &mut generation), ERR_ARG);
        i.channel = 11; i.sta_psk_len = 3;
        assert_eq!((wifi.set.unwrap())(h.user, &i, &mut generation), ERR_ARG);
    } else if call.method == HOST_WIFI as i32 {
        let g = (call.value as u64).to_le_bytes();
        notify(h, 1, &g);
        let mut st = WifiState::default();
        st.size = core::mem::size_of::<WifiState>() as u32;
        assert_eq!((wifi.state.unwrap())(h.user, &mut st), 0);
        let v = [st.sta, st.ap, st.channel];
        notify(h, 2, &v);
    }
    RET_CONTINUE as i32
}

#[test]
fn a_module_sets_the_wanted_state_and_every_module_is_told() {
    let rt = Runtime::default();
    let a = rt.add_handler("a", module, 16);
    let b = rt.add_handler("b", module, 16);
    a.start(b"");
    rt.run(0);
    let (wanted, generation) = rt.wifi_intent();
    assert_eq!(generation, 1);
    assert_eq!((wanted.radio, wanted.sta, wanted.ap, wanted.channel, wanted.bssid), (1, 1, 1, 11, [1, 2, 3, 4, 5, 6]));
    assert_eq!(&wanted.sta_ssid[..4], b"home");
    // both instances got HOST_WIFI with the generation, and the default fixture joined
    for m in [&a, &b] {
        let events = m.events();
        assert_eq!(events[0].data, 1u64.to_le_bytes());
        assert_eq!(events[1].data, [2, 1, 11]);
    }
}

#[test]
fn a_scripted_state_is_what_modules_read() {
    let rt = Runtime::default();
    let a = rt.add_handler("a", module, 16);
    let mut st = WifiState::default();
    st.size = core::mem::size_of::<WifiState>() as u32;
    st.sta = 1; st.channel = 3;
    rt.set_wifi_state(st);
    a.start(b"");
    rt.run(0);
    assert_eq!(a.events()[1].data, [1, 0, 3]);
}

/// START: read the tuning, set A-MPDU off and a buffer size; an out-of-range value is refused.
unsafe extern "C" fn tuning_module(host: *const Host, _state: *mut u8, call: *const Call) -> i32 {
    let h = &*host;
    let call = &*call;
    if call.component != HOST_COMPONENT as i32 || call.method != HOST_START as i32 { return RET_CONTINUE as i32; }
    let tuner = &*(h.get_component.unwrap()(h.user, c"wifi-tuning".as_ptr()) as *const WifiTuner);
    let mut t = WifiTuning::default();
    assert_eq!((tuner.get.unwrap())(h.user, &mut t), 0);
    assert_eq!((t.ampdu, t.rx_static), (TUNE_DEFAULT, 0));
    t.ampdu = TUNE_OFF; t.rx_static = 12;
    assert_eq!((tuner.set.unwrap())(h.user, &t), 0);
    t.rx_static = 1;
    assert_eq!((tuner.set.unwrap())(h.user, &t), ERR_ARG);
    t.rx_static = 12; t.ampdu = 3;
    assert_eq!((tuner.set.unwrap())(h.user, &t), ERR_ARG);
    RET_CONTINUE as i32
}

#[test]
fn a_module_tunes_the_driver_and_the_refused_values_change_nothing() {
    let rt = Runtime::default();
    let a = rt.add_handler("a", tuning_module, 16);
    a.start(b"");
    rt.run(0);
    let t = rt.wifi_tuning();
    assert_eq!((t.ampdu, t.rx_static), (TUNE_OFF, 12));
}

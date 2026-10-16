//! Compatibility adapters for older Wi-Fi ABI slots. Desired state is the
//! transport profile; driver reconciliation runs on the host worker.
use core::ffi::c_char;
use crate::profile;
use crate::profile::{TransportKind, TransportConfig};

unsafe fn credentials<'a>(ssid: *const u8, n: usize, psk: *const u8, m: usize) -> Option<TransportConfig<'a>> {
    if n > 32 || m > 63 || (n > 0 && ssid.is_null()) || (m > 0 && psk.is_null()) { return None; }
    let config = TransportConfig { ssid: (n != 0).then(|| core::slice::from_raw_parts(ssid, n)),
        passphrase: (m != 0).then(|| core::slice::from_raw_parts(psk, m)), ..Default::default() };
    crate::profile::validate_config(config)?; Some(config)
}
pub(crate) unsafe extern "C" fn start(ssid: *const u8, n: usize, psk: *const u8, m: usize) -> i32 {
    let Some(config) = credentials(ssid, n, psk, m) else { return xip_abi::ERR_ARG; };
    profile::update(profile::TransportRequest { mode: Some(TransportKind::Sta), config }).map_or_else(|e| e, |_| 0)
}
pub(crate) unsafe extern "C" fn stop() {
    let _ = profile::update(profile::TransportRequest { mode: Some(TransportKind::Off), ..Default::default() });
}
pub(crate) unsafe extern "C" fn config(ssid: *const u8, n: usize, psk: *const u8, m: usize) -> i32 {
    let Some(config) = credentials(ssid, n, psk, m) else { return xip_abi::ERR_ARG; };
    crate::settings::configure_sta(config.ssid.unwrap_or(&[]), config.passphrase.unwrap_or(&[])).map_or_else(|e| e, |_| 0)
}
pub(crate) unsafe extern "C" fn saved(ssid: *mut c_char, has_psk: *mut i32) -> i32 {
    if ssid.is_null() || has_psk.is_null() { return xip_abi::ERR_ARG; }
    let mut p = profile::TransportProfile::new();
    if crate::settings::load_sta(&mut p).is_err() { return 0; }
    core::ptr::copy_nonoverlapping(p.ssid.as_ptr(), ssid.cast(), 33);
    *has_psk = i32::from(p.sta_passphrase_len != 0); i32::from(p.ssid_len != 0)
}
pub(crate) unsafe extern "C" fn reconcile() {
    let p = profile::snapshot();
    static mut APPLIED: Option<crate::profile::TransportProfile> = None;
    if let Some(a) = APPLIED {
        if a.run_requested == p.run_requested && a.requested_transport == p.requested_transport
            && a.ssid_len == p.ssid_len && a.ssid == p.ssid
            && a.sta_passphrase_len == p.sta_passphrase_len && a.sta_passphrase == p.sta_passphrase
            && a.sta_channel == p.sta_channel && a.sta_bssid_set == p.sta_bssid_set
            && a.sta_bssid == p.sta_bssid && a.sta_ampdu_enabled == p.sta_ampdu_enabled { return; }
    }
    crate::wifi_esp::stop();
    if p.run_requested && p.requested_transport == Some(TransportKind::Sta) {
        if crate::wifi_esp::start(&p) != 0 { return; }
    }
    APPLIED = Some(p);
}

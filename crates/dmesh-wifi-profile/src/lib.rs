//! The Wi-Fi part of the DMesh transport profile as the `wifi` and `wifi-tuning` components of the module ABI (`xip_abi::{WifiIntent,
//! WifiTuning}`): what the firmware answers when a module asks for the intended state. The profile keeps everything else (NAN, UART,
//! BLE, LoRa) to itself. This lives here and not in `dmesh-server` because DMesh crates must not depend on this repository.
use dmesh_server::transport_profile::{valid_passphrase, valid_ssid, TransportKind, TransportProfile};
use xip_abi::{WifiIntent, WifiTuning};

/// The part of the profile the `wifi` component carries. Radio tuning, NAN, UART, BLE and LoRa fields stay in the profile.
pub fn to_intent(profile: &TransportProfile) -> WifiIntent {
    use TransportKind::{Nan, Sta};
    let on = profile.run_requested && matches!(profile.requested_transport, Some(Sta | Nan));
    let mut i = WifiIntent::default();
    i.radio = u8::from(on);
    i.sta = u8::from(on && profile.requested_transport == Some(Sta));
    i.ap = u8::from(on && profile.ap != 0);
    i.channel = profile.sta_channel;
    if profile.sta_bssid_set { i.bssid = profile.sta_bssid; }
    i.sta_ssid_len = profile.ssid_len as u8;
    i.sta_ssid[..profile.ssid_len].copy_from_slice(&profile.ssid[..profile.ssid_len]);
    i.sta_psk_len = profile.sta_passphrase_len as u8;
    i.sta_psk[..profile.sta_passphrase_len].copy_from_slice(&profile.sta_passphrase[..profile.sta_passphrase_len]);
    i.ap_ssid_len = profile.ap_ssid_len as u8;
    i.ap_ssid[..profile.ap_ssid_len].copy_from_slice(&profile.ap_ssid[..profile.ap_ssid_len]);
    i.ap_psk_len = profile.ap_passphrase_len as u8;
    i.ap_psk[..profile.ap_passphrase_len].copy_from_slice(&profile.ap_passphrase[..profile.ap_passphrase_len]);
    i
}

/// Replace the Wi-Fi part of the profile with an intent; `None` when the intent is not valid.
pub fn apply_intent(profile: &mut TransportProfile, i: &WifiIntent) -> Option<()> {
    if (i.size as usize) < core::mem::size_of::<WifiIntent>() || i.radio > 1 || i.sta > 1 || i.ap > 1 { return None; }
    let (sn, pn, an, ap) = (i.sta_ssid_len as usize, i.sta_psk_len as usize, i.ap_ssid_len as usize, i.ap_psk_len as usize);
    if sn > 32 || pn > 63 || an > 32 || ap > 63 || (i.channel != 0 && !(1..=14).contains(&i.channel)) { return None; }
    let bssid_set = i.bssid != [0; 6];
    if i.sta == 1 && sn == 0 && !bssid_set { return None; }
    if sn != 0 { valid_ssid(&i.sta_ssid[..sn])?; }
    if pn != 0 { valid_passphrase(&i.sta_psk[..pn])?; }
    if an != 0 { valid_ssid(&i.ap_ssid[..an])?; }
    if ap != 0 { valid_passphrase(&i.ap_psk[..ap])?; }
    let mut p = *profile;
    p.ssid = [0; 33]; p.ssid[..sn].copy_from_slice(&i.sta_ssid[..sn]); p.ssid_len = sn;
    p.sta_passphrase = [0; 64]; p.sta_passphrase[..pn].copy_from_slice(&i.sta_psk[..pn]); p.sta_passphrase_len = pn;
    p.ap_ssid = [0; 33]; p.ap_ssid[..an].copy_from_slice(&i.ap_ssid[..an]); p.ap_ssid_len = an;
    p.ap_passphrase = [0; 64]; p.ap_passphrase[..ap].copy_from_slice(&i.ap_psk[..ap]); p.ap_passphrase_len = ap;
    p.sta_channel = i.channel; p.sta_bssid_set = bssid_set; p.sta_bssid = i.bssid;
    p.ap = i.ap;
    if i.radio == 0 {
        p.requested_transport = None; p.run_requested = false;
    } else {
        p.requested_transport = Some(if i.sta == 1 { TransportKind::Sta } else { TransportKind::Nan });
        p.run_requested = true;
    }
    p.config()?;
    *profile = p;
    Some(())
}

/// The part of the profile the `wifi-tuning` component carries (the four buffer sizes are not in the profile and
/// report 0). Switches are always explicit: the profile has a value for each.
pub fn to_tuning(profile: &TransportProfile) -> WifiTuning {
    use xip_abi::{TUNE_OFF, TUNE_ON};
    let sw = |on: bool| if on { TUNE_ON } else { TUNE_OFF };
    let mut t = WifiTuning::default();
    t.ampdu = sw(profile.sta_ampdu_enabled);
    t.rates_11b = sw(!profile.sta_11b_rates_disabled);
    t.raw_rx = sw(profile.sta_raw_rx_enabled);
    t.driver_tx = sw(profile.sta_driver_tx);
    t.bssid_check = sw(!profile.sta_bssid_check_disabled);
    t.raw_tx_rate = profile.raw_tx_rate;
    t
}

/// Apply the profile part of a tuning; a switch left at the default takes the profile's default. `None` when a
/// value is out of range (the buffer sizes are checked here and kept by the caller).
pub fn apply_tuning(profile: &mut TransportProfile, t: &WifiTuning) -> Option<()> {
    use xip_abi::{TUNE_DEFAULT, TUNE_OFF, TUNE_ON};
    if (t.size as usize) < core::mem::size_of::<WifiTuning>() { return None; }
    if [t.ampdu, t.rates_11b, t.raw_rx, t.driver_tx, t.bssid_check].iter().any(|v| *v > TUNE_OFF) { return None; }
    let ok = |v: u16, lo: u16, hi: u16| v == 0 || (lo..=hi).contains(&v);
    if !(ok(t.rx_static, 2, 64) && ok(t.rx_dynamic, 2, 128) && ok(t.tx_dynamic, 4, 128) && ok(t.rx_ba, 2, 64)) { return None; }
    let base = TransportProfile::new();
    let pick = |v: u8, default: bool| match v { TUNE_ON => true, TUNE_OFF => false, _ => { let _ = TUNE_DEFAULT; default } };
    profile.sta_ampdu_enabled = pick(t.ampdu, base.sta_ampdu_enabled);
    profile.sta_11b_rates_disabled = !pick(t.rates_11b, !base.sta_11b_rates_disabled);
    profile.sta_raw_rx_enabled = pick(t.raw_rx, base.sta_raw_rx_enabled);
    profile.sta_driver_tx = pick(t.driver_tx, base.sta_driver_tx);
    profile.sta_bssid_check_disabled = !pick(t.bssid_check, !base.sta_bssid_check_disabled);
    profile.raw_tx_rate = t.raw_tx_rate;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_wifi_intent_round_trips_and_is_validated() {
        let mut p = TransportProfile::new();
        let mut i = WifiIntent::default();
        i.radio = 1; i.sta = 1; i.ap = 1; i.channel = 6; i.bssid = [1, 2, 3, 4, 5, 6];
        i.sta_ssid[..4].copy_from_slice(b"home"); i.sta_ssid_len = 4;
        i.sta_psk[..8].copy_from_slice(b"password"); i.sta_psk_len = 8;
        assert!(apply_intent(&mut p, &i).is_some());
        let back = to_intent(&p);
        assert_eq!((back.radio, back.sta, back.ap, back.channel, back.bssid), (1, 1, 1, 6, [1, 2, 3, 4, 5, 6]));
        assert_eq!(&back.sta_ssid[..4], b"home");
        i.sta_psk_len = 4;                       // a passphrase of 4 bytes is not valid
        assert!(apply_intent(&mut p, &i).is_none());
        i.sta_psk_len = 0; i.sta_ssid_len = 0; i.bssid = [0; 6];
        assert!(apply_intent(&mut p, &i).is_none());    // sta without a name or BSSID
        i.sta = 0;
        assert!(apply_intent(&mut p, &i).is_some());
        assert_eq!((p.requested_transport, p.run_requested), (Some(TransportKind::Nan), true));
        i.radio = 0;
        assert!(apply_intent(&mut p, &i).is_some() && to_intent(&p).radio == 0 && p.requested_transport.is_none());
    }
    #[test]
    fn the_wifi_tuning_maps_onto_the_profile() {
        use xip_abi::{TUNE_OFF, TUNE_ON};
        let mut p = TransportProfile::new();
        let d = to_tuning(&p);
        assert_eq!((d.ampdu, d.rates_11b, d.raw_rx, d.driver_tx, d.bssid_check), (TUNE_ON, TUNE_OFF, TUNE_ON, TUNE_ON, TUNE_OFF));
        let mut t = WifiTuning::default();
        t.ampdu = TUNE_OFF; t.rates_11b = TUNE_ON; t.raw_tx_rate = 5;
        assert!(apply_tuning(&mut p, &t).is_some());
        assert!(!p.sta_ampdu_enabled && !p.sta_11b_rates_disabled && p.raw_tx_rate == 5 && p.sta_raw_rx_enabled);
        assert_eq!(to_tuning(&p).rates_11b, TUNE_ON);
        assert!(apply_tuning(&mut p, &WifiTuning::default()).is_some());      // all defaults: the profile defaults
        assert_eq!(p, TransportProfile::new());
        t.rx_static = 1;
        assert!(apply_tuning(&mut p, &t).is_none());
        t.rx_static = 0; t.ampdu = 3;
        assert!(apply_tuning(&mut p, &t).is_none());
    }
}

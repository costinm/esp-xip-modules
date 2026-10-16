//! Rust access to the host settings behind the module ABI. ESP-IDF calls are
//! confined here; drivers use slices and typed values.
use core::ffi::{CStr, c_char};

pub struct Namespace(esp_idf_sys::nvs_handle_t);
impl Namespace {
    pub fn open(name: &CStr, write: bool) -> Result<Self, i32> {
        let mut handle = 0;
        let mode = if write { esp_idf_sys::nvs_open_mode_t_NVS_READWRITE } else { esp_idf_sys::nvs_open_mode_t_NVS_READONLY };
        let result = unsafe { esp_idf_sys::nvs_open(name.as_ptr(), mode, &mut handle) };
        if result == 0 { Ok(Self(handle)) } else { Err(if write { -3 } else { -4 }) }
    }
    pub fn get_u16(&self, key: &CStr) -> Result<u16, i32> {
        let mut value = 0;
        if unsafe { esp_idf_sys::nvs_get_u16(self.0, key.as_ptr(), &mut value) } == 0 { Ok(value) } else { Err(-4) }
    }
    pub fn set_u16(&self, key: &CStr, value: u16) -> Result<(), i32> {
        if unsafe { esp_idf_sys::nvs_set_u16(self.0, key.as_ptr(), value) } == 0 { Ok(()) } else { Err(-3) }
    }
    pub fn get_text(&self, key: &CStr, out: &mut [u8]) -> Result<usize, i32> {
        if out.is_empty() { return Err(-5); }
        let mut len = out.len();
        let rc = unsafe { esp_idf_sys::nvs_get_str(self.0, key.as_ptr(), out.as_mut_ptr().cast(), &mut len) };
        if rc == 0 { Ok(len.saturating_sub(1)) }
        else { Err(if rc == esp_idf_sys::ESP_ERR_NVS_INVALID_LENGTH as i32 { -5 } else { -4 }) }
    }
    pub fn set_text(&self, key: &CStr, value: &CStr) -> Result<(), i32> {
        if unsafe { esp_idf_sys::nvs_set_str(self.0, key.as_ptr(), value.as_ptr()) } == 0 { Ok(()) } else { Err(-3) }
    }
    pub fn get_blob(&self, key: &CStr, out: &mut [u8]) -> Result<usize, i32> {
        if out.is_empty() { return Err(-5); }
        let mut len = out.len();
        let rc = unsafe { esp_idf_sys::nvs_get_blob(self.0, key.as_ptr(), out.as_mut_ptr().cast(), &mut len) };
        if rc == 0 { Ok(len) }
        else { Err(if rc == esp_idf_sys::ESP_ERR_NVS_INVALID_LENGTH as i32 { -5 } else { -4 }) }
    }
    pub fn set_blob(&self, key: &CStr, value: &[u8]) -> Result<(), i32> {
        if unsafe { esp_idf_sys::nvs_set_blob(self.0, key.as_ptr(), value.as_ptr().cast(), value.len()) } == 0 { Ok(()) } else { Err(-3) }
    }
    pub fn erase(&self, key: &CStr) -> Result<(), i32> {
        let rc = unsafe { esp_idf_sys::nvs_erase_key(self.0, key.as_ptr()) };
        if rc == 0 || rc == esp_idf_sys::ESP_ERR_NVS_NOT_FOUND as i32 { Ok(()) } else { Err(-3) }
    }
    pub fn commit(&self) -> Result<(), i32> {
        if unsafe { esp_idf_sys::nvs_commit(self.0) } == 0 { Ok(()) } else { Err(-3) }
    }
}
impl Drop for Namespace { fn drop(&mut self) { unsafe { esp_idf_sys::nvs_close(self.0) }; } }

const WIFI_MEMORY: [(&[u8], &CStr, u16, u16); 4] = [
    (b"wifi.rx_static", c"rx_static", 2, 64), (b"wifi.rx_dynamic", c"rx_dynamic", 2, 128),
    (b"wifi.tx_dynamic", c"tx_dynamic", 4, 128), (b"wifi.rx_ba", c"rx_ba", 2, 64),
];
/// The host-owned device settings, preserving the loader's existing NVS keys.
/// Applications delegate these names from their `nv` callback.
pub fn nv(key: &[u8], value: Option<&[u8]>, out: &mut [u8]) -> i32 {
    // Existing boot keys shared with DMesh. Runtime transport requests never
    // write these; the nv/configuration path is explicitly persistent.
    let string_key = match key {
        b"sta_ssid" => Some((c"dmesh", c"sta_ssid", false)),
        b"sec:sta" => Some((c"sec", c"sta", true)),
        b"uart.baud" => Some((c"dmesh", c"uart.baud", false)),
        b"uart.mode" => Some((c"dmesh", c"uart.mode", false)),
        b"nan_dw_interval" => Some((c"dmesh", c"nan_dw_interval", false)),
        b"mode" => Some((c"dmesh", c"mode", false)),
        _ => None,
    };
    if let Some((namespace, name, secret)) = string_key {
        let result = (|| {
            if secret && value.is_none() { return Err(-4); }
            let ns = Namespace::open(namespace, value.is_some())?;
            if let Some(value) = value {
                if value.len() > 63 || value.contains(&0) { return Err(-1); }
                if value.is_empty() { ns.erase(name)?; }
                else {
                    let valid = match key {
                        b"sta_ssid" => crate::profile::valid_ssid(value).is_some(),
                        b"sec:sta" => (8..=63).contains(&value.len()),
                        b"uart.baud" => crate::profile::parse_uart_baud(value).is_some(),
                        b"uart.mode" => matches!(value, b"ppp" | b"line" | b"off"),
                        b"nan_dw_interval" => matches!(value, b"1" | b"8" | b"16"),
                        b"mode" => matches!(value, b"active" | b"sleepy" | b"sleepy-soft"),
                        _ => false,
                    };
                    if !valid { return Err(-1); }
                    let mut text = [0;64]; text[..value.len()].copy_from_slice(value);
                    ns.set_text(name, CStr::from_bytes_with_nul(&text[..value.len()+1]).map_err(|_| -1)?)?;
                }
                ns.commit()?; Ok(0)
            } else {
                let mut text = [0;64]; let n = ns.get_text(name, &mut text)?;
                if n > out.len() { return Err(-5); } out[..n].copy_from_slice(&text[..n]); Ok(n as i32)
            }
        })();
        return result.unwrap_or_else(|e| e);
    }
    let Some((_, name, low, high)) = WIFI_MEMORY.iter().find(|(name, ..)| *name == key) else { return -4 };
    let result = (|| {
        let ns = Namespace::open(c"wl", value.is_some())?;
        if let Some(value) = value {
            if value.is_empty() { ns.erase(name)?; }
            else {
                if value.len() > 4 || !value.iter().all(u8::is_ascii_digit) { return Err(-1); }
                let number: u16 = core::str::from_utf8(value).map_err(|_| -1)?.parse().map_err(|_| -1)?;
                if number < *low || number > *high { return Err(-1); }
                ns.set_u16(name, number)?;
            }
            ns.commit()?;
            Ok(0)
        } else {
            let mut value = ns.get_u16(name)?;
            let mut digits = [0; 5]; let mut start = 5;
            loop { start -= 1; digits[start] = b'0' + (value % 10) as u8; value /= 10; if value == 0 { break; } }
            let text = &digits[start..];
            if out.len() < text.len() { return Err(-5); }
            out[..text.len()].copy_from_slice(text);
            Ok(text.len() as i32)
        }
    })();
    result.unwrap_or_else(|error| error)
}
pub(crate) unsafe extern "C" fn nv_hook(key: *const c_char, key_len: usize, value: *const u8, value_len: usize, out: *mut u8, cap: usize) -> i32 {
    if key.is_null() || (out.is_null() && cap != 0) { return -1; }
    let name = unsafe { core::slice::from_raw_parts(key.cast(), key_len) };
    let value = if value.is_null() { if value_len != 0 { return -1; } None }
                else { Some(unsafe { core::slice::from_raw_parts(value, value_len) }) };
    let out = if cap == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(out, cap) } };
    nv(name, value, out)
}

/// Existing DMesh boot credentials; transport requests never write these.
pub fn load_sta(profile: &mut crate::profile::TransportProfile) -> Result<(), i32> {
    let settings = Namespace::open(c"dmesh", false)?;
    let secrets = Namespace::open(c"sec", false)?;
    let mut ssid = [0; 33]; let mut secret = [0; 64];
    let n = settings.get_text(c"sta_ssid", &mut ssid)?;
    let m = secrets.get_text(c"sta", &mut secret)?;
    crate::profile::apply_config(crate::profile::TransportConfig { ssid: Some(&ssid[..n]), passphrase: Some(&secret[..m]), ..Default::default() }, profile).ok_or(-1)
}
/// Compatibility wifi_config writes the same individual settings as nv.
/// It does not publish or save a transport profile.
pub(crate) fn configure_sta(ssid: &[u8], secret: &[u8]) -> Result<(), i32> {
    if ssid.len() > 32 || secret.len() > 63 || ssid.contains(&0) || secret.contains(&0) { return Err(-1); }
    if !secret.is_empty() && secret.len() < 8 { return Err(-1); }
    let settings = Namespace::open(c"dmesh", true)?; let secrets = Namespace::open(c"sec", true)?;
    let mut name = [0;33]; name[..ssid.len()].copy_from_slice(ssid);
    let mut password = [0;64]; password[..secret.len()].copy_from_slice(secret);
    if ssid.is_empty() { settings.erase(c"sta_ssid")?; } else { settings.set_text(c"sta_ssid", CStr::from_bytes_with_nul(&name[..ssid.len()+1]).map_err(|_| -1)?)?; }
    if secret.is_empty() { secrets.erase(c"sta")?; } else { secrets.set_text(c"sta", CStr::from_bytes_with_nul(&password[..secret.len()+1]).map_err(|_| -1)?)?; }
    secrets.commit()?; settings.commit()
}

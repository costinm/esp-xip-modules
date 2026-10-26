//! Intended transport state for the NAN policy owner; 255 means retain a selector.
use crate::*;

/// Native selection shared by transport.set and the NAN wake handler.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NanTransport {
    /// Station role: 0 off, 1 on, 255 retain.
    pub sta: u8,
    /// Shared BLE policy: 0 default, 1 on, 2 off; 255 retain.
    pub ble: u8,
    /// Shared UART framing: 0 default or 2 PPP; 255 retain.
    pub uart_mode: u8,
    /// 0 off, 1 LoRa, 2 FSK; 255 retain.
    pub radio_mode: u8,
    /// 0 default, 1/8/16 discovery windows, or 255 retain.
    pub cadence: u8,
    /// 0 default, 1 on, 2 off; 255 retain.
    pub now: u8,
    /// 0 off, 1 on; 255 retain.
    pub ap: u8,
    /// Shared UART selector: 0 default, 1..7 baud selectors, 8 off, 9 921600; 255 retain.
    pub uart: u8,
    /// Reserved for future per-transport settings; must be zero.
    pub reserved: [u8; 8],
    /// Zero retains the baud. Only rates supported by the physical driver are admitted.
    pub uart_baud: u32,
    /// Zero uses the module default for this activation.
    pub radio_frequency: u32,
    /// Zero uses the module default for this activation.
    pub radio_bandwidth: u32,
    /// Zero uses the module default spreading factor.
    pub radio_sf: u32,
    /// Zero uses the module default coding rate (5..8).
    pub radio_cr: u32,
    /// 127 uses the module default power; negative power is currently unsupported.
    pub radio_power: i32,
}
impl Default for NanTransport {
    fn default() -> Self {
        Self { sta: 255, ble: 255, uart_mode: 255, radio_mode: 255, cadence: 255,
            now: 255, ap: 255, uart: 255, reserved: [0;8], uart_baud: 0, radio_frequency: 0,
            radio_bandwidth: 0, radio_sf: 0, radio_cr: 0, radio_power: 127 }
    }
}
impl NanTransport {
    /// A transport.set adapter decodes the supported shared wire fields before invoking NAN.
    pub fn decode(fields: &[u8]) -> Result<Self, i32> {
        let mut r = Self::default();
        let mut reader = cbor::Reader::new(fields);
        let Some(cbor::Item::Map(n)) = reader.next() else { return Err(ERR_ARG) };
        if n == 0 { return Err(ERR_ARG); }
        for _ in 0..n {
            let Some(cbor::Item::Uint(tag)) = reader.next() else { return Err(ERR_ARG) };
            // Reject unimplemented fields instead of acknowledging an unapplied setting.
            if !matches!(tag, 1 | 14 | 15 | 16 | 18 | 26 | 28 | 29 | 31 | 32 | 33 | 35 | 36 | 37) { return Err(ERR_UNSUPPORTED); }
            if !reader.skip() { return Err(ERR_ARG); }
        }
        if !reader.rest().is_empty() { return Err(ERR_ARG); }
        macro_rules! field { ($tag:expr, $name:ident, $ty:ty) => {
            if let Some(v) = cbor::get_int(fields, $tag).map_err(|_| ERR_ARG)? {
                r.$name = <$ty>::try_from(v).map_err(|_| ERR_ARG)?;
            }
        }; }
        field!(1,sta,u8); field!(14,cadence,u8); field!(15,now,u8); field!(16,ap,u8);
        field!(18,uart,u8); field!(26,ble,u8); field!(28,uart_mode,u8); field!(29,uart_baud,u32);
        field!(31,radio_mode,u8); field!(32,radio_frequency,u32); field!(33,radio_bandwidth,u32);
        field!(35,radio_power,i32); field!(36,radio_sf,u32); field!(37,radio_cr,u32);
        r.validate()?;
        Ok(r)
    }
    /// Native callers validate the complete selection before any driver changes.
    pub fn validate(&self) -> Result<(), i32> {
        if !matches!(self.sta,0|1|255) || !matches!(self.cadence,0|1|8|16|255)
            || !matches!(self.ble,0|1|2|255) || !matches!(self.uart_mode,0|2|255)
            || !matches!(self.radio_mode,0|1|2|255) || !matches!(self.now,0|1|2|255)
            || !matches!(self.ap,0|1|255) || !matches!(self.uart,0..=9|255) || self.reserved!=[0;8]
            || !matches!(self.uart_baud,0|115200|230400|460800|921600)
            || !(self.radio_sf==0 || (5..=12).contains(&self.radio_sf))
            || !(self.radio_cr==0 || (5..=8).contains(&self.radio_cr))
            || !(self.radio_power==127 || (-40..=30).contains(&self.radio_power)) {
            return Err(ERR_ARG);
        }
        if (self.radio_mode==255 || self.radio_mode==0) && (self.radio_frequency!=0 || self.radio_bandwidth!=0
            || self.radio_sf!=0 || self.radio_cr!=0 || self.radio_power!=127) {return Err(ERR_ARG);}
        if self.radio_power<0 {return Err(ERR_UNSUPPORTED);}
        // The existing FSK implementation has a fixed 100 kbit/s profile.
        Ok(())
    }
}
const _: () = assert!(core::mem::size_of::<NanTransport>() == 40);

/// Native serial mechanisms used by the coordinated transport owner.
#[repr(C)]
pub struct Serial {
    /// Version 1.
    pub version: u32,
    /// Size of this table.
    pub size: u32,
    /// Query USB SOF attachment independently of the selected console driver.
    pub usb_connected: Option<unsafe extern "C" fn(ctx: *mut core::ffi::c_void) -> core::ffi::c_int>,
    /// Set UART application receive policy: 0 off, 1 on. Recovery wake remains armed.
    pub mode: Option<unsafe extern "C" fn(ctx: *mut core::ffi::c_void, mode: u32) -> core::ffi::c_int>,
    /// Queue the driver's negotiated baud change; zero is not a valid rate.
    pub baud: Option<unsafe extern "C" fn(ctx: *mut core::ffi::c_void, baud: u32) -> core::ffi::c_int>,
    /// Query the experimental NV usb.sleep opt-in (default false).
    pub usb_sleep_allowed: Option<unsafe extern "C" fn(ctx: *mut core::ffi::c_void) -> core::ffi::c_int>,

}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_wire_validation_rejects_unapplied_fields_and_bad_selectors() {
        assert_eq!(NanTransport::decode(&[0xa2,1,0,0x12,8]).unwrap().uart,8);
        assert!(matches!(NanTransport::decode(&[0xa2,1,1,2,1]),Err(ERR_UNSUPPORTED)));
        assert!(matches!(NanTransport::decode(&[0xa2,1,1,0x18,31,3]),Err(ERR_ARG)));
        assert!(matches!(NanTransport::decode(&[0xa1,0x18,28,1]),Err(ERR_ARG)));
        assert!(matches!(NanTransport::decode(&[0xa1,0x18,25,0x46,0,0,0,0,0,0]),Err(ERR_UNSUPPORTED)));
        let r=NanTransport::decode(&[0xa1,0x10,1]).unwrap();assert_eq!(r.ap,1);assert_eq!(r.sta,255);
    }
}

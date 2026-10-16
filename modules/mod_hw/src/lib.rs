//! Hardware-policy module for the esp-xip-modules host: battery level, an ADC probe and a GPIO button with short, long
//! and double press events (`API.md`). Event driven: a measurement that needs a control pin to settle schedules an
//! alarm, the probe is a periodic alarm, the button is an interrupt; between calls nothing runs.
//!
//! Requests are CBOR maps keyed by the field tags of `API.md`; events are CBOR arrays (value type 5). Settings
//! (`battery.pin`, `button.gpio`, ...) are read from the module's own namespace when a request omits a field.
#![no_std]

#[cfg(test)]
extern crate std;

use xip_abi::cbor::{self, Item, Writer};
use xip_abi::{CallView, Hw, Module, ALARM_EXACT, GPIO_INPUT, GPIO_OUTPUT, GPIO_PULL_NONE, GPIO_PULL_UP, IRQ_BOTH,
                 VALUE_CBOR};

/// The component number of the module: its slot tag, as in DMesh.
pub const COMPONENT: i32 = 45;

// methods of API.md
const M_BATTERY: i32 = 1;
const M_PROBE: i32 = 2;
const M_BUTTON: i32 = 3;

/// The method number of an operation name (`battery`, `probe`, `button`), for an application that names them.
pub fn method_from_name(name: &[u8]) -> Option<i32> {
    match name {
        b"battery" => Some(M_BATTERY),
        b"probe" => Some(M_PROBE),
        b"button" => Some(M_BUTTON),
        _ => None,
    }
}
// internal: calls the module asked for
const M_BUTTON_IRQ: i32 = 10;
const M_PROBE_TICK: i32 = 11;
const M_BATTERY_SAMPLE: i32 = 12;

pub const EVENT_SAMPLE: u16 = 110;
pub const EVENT_BATTERY: u16 = 111;
const EVENT_SHORT: u16 = 101;
const EVENT_LONG: u16 = 102;
const EVENT_DOUBLE: u16 = 103;
const LONG_MS: u64 = 2_500;
const DOUBLE_MS: u64 = 500;
const ADC_UNKNOWN: u64 = 255;
/// How long a battery divider's control pin needs before the reading is right.
const SETTLE_MS: u32 = 10;

#[cfg(target_arch = "riscv32")]
const DEFAULT_PIN: i32 = 0; // ESP32-C6 ADC1 GPIO0..GPIO6
#[cfg(not(target_arch = "riscv32"))]
const DEFAULT_PIN: i32 = 35; // classic ESP32 battery ADC convention

pub const ERR_REQUEST: i32 = -2;
pub const ERR_RANGE: i32 = -4;
pub const ERR_CTRL: i32 = -5;

#[derive(Clone, Copy, Default)]
struct Battery {
    pin: i32,
    ref_mv: u32,
    divider_x100: u32,
    min_mv: u32,
    max_mv: u32,
    ctrl_pin: i32,
    ctrl_level: i32,
}

/// The module's state; the host hands it over zeroed, so every field is valid as zero.
#[derive(Default)]
pub struct HwModule {
    battery: Battery,
    battery_waiting: bool,
    probe_alarm: i32, // alarm id + 1
    probe_left: u32,
    probe_forever: bool,
    probe_ref_mv: u32,
    probe_pins: [i32; 4],
    probe_count: u8,
    button_pin: i32,
    button_on: bool,
    pressed_at: u64,
    last_release: u64,
}

pub fn get_setting_i32(hw: &Hw, key: &[u8], default: i32) -> i32 {
    let mut buf = [0u8; 32];
    let Ok(len) = hw.get_setting(key, &mut buf) else { return default };
    core::str::from_utf8(&buf[..len]).ok().and_then(parse_i32).unwrap_or(default)
}

/// A decimal setting ("2.2") as hundredths.
pub fn get_setting_scaled(hw: &Hw, key: &[u8], default: i32) -> i32 {
    let mut buf = [0u8; 32];
    let Ok(len) = hw.get_setting(key, &mut buf) else { return default };
    parse_scaled(&buf[..len], default)
}

fn parse_scaled(text: &[u8], default: i32) -> i32 {
    let mut value = 0i32;
    let mut fraction = 0i32;
    let mut in_fraction = false;
    let mut digits = 0i32;
    for byte in text {
        if *byte == b'.' { if in_fraction { return default; } in_fraction = true; continue; }
        if *byte < b'0' || *byte > b'9' { return default; }
        if in_fraction {
            if digits == 0 { fraction = (*byte - b'0') as i32 * 10; }
            else if digits == 1 { fraction += (*byte - b'0') as i32; }
            digits += 1;
        } else {
            value = match value.checked_mul(10).and_then(|v| v.checked_add((*byte - b'0') as i32)) {
                Some(v) => v,
                None => return default,
            };
        }
    }
    value.checked_mul(100).and_then(|v| v.checked_add(fraction)).unwrap_or(default)
}

fn parse_i32(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    if bytes.is_empty() { return None; }
    let (sign, start) = if bytes[0] == b'-' { (-1i32, 1) } else { (1, 0) };
    let mut out = 0i32;
    for &b in &bytes[start..] {
        let digit = b.wrapping_sub(b'0');
        if digit > 9 { return None; }
        out = out.checked_mul(10)?.checked_add(digit as i32)?;
    }
    Some(out * sign)
}

/// An optional integer field of a request: absent → `None`, present → value; a malformed field is `Err`.
fn field(data: &[u8], key: u64) -> Result<Option<i64>, ()> {
    if data.is_empty() { return Ok(None); }
    cbor::get_int(data, key)
}

fn emit_array(hw: &Hw, id: u16, values: &[u64]) -> i32 {
    let mut buf = [0u8; 96];
    let mut w = Writer::new(&mut buf);
    w.array(values.len());
    for v in values { w.uint(*v); }
    match w.finish() {
        Some(encoded) => hw.emit(id, VALUE_CBOR, encoded),
        None => -3,
    }
}

fn respond_map(hw: &Hw, pairs: &[(u64, u64)]) {
    let mut buf = [0u8; 64];
    let mut w = Writer::new(&mut buf);
    w.map(pairs.len());
    for (k, v) in pairs { w.uint(*k).uint(*v); }
    if let Some(encoded) = w.finish() { let _ = hw.respond(encoded); }
}

/// One ADC reading of `pin`; with a divider it is the battery (millivolts and percent), without one a raw sample.
/// Emits `event_id` and returns the values `[pin, raw, mv, battery_mv, level, ref_mv, unit, channel]`.
fn sample(hw: &Hw, pin: i32, ref_mv: u32, event_id: u16, divider_x100: u32, min_mv: u32, max_mv: u32)
          -> Result<[u64; 8], i32> {
    let (raw, mv, unit, channel) = hw.adc_read(pin, ref_mv)?;
    let battery_mv = (mv as u64).saturating_mul(divider_x100 as u64) / 100;
    let level = if divider_x100 == 0 || max_mv <= min_mv || battery_mv < min_mv.saturating_sub(200) as u64 {
        ADC_UNKNOWN
    } else {
        battery_mv.saturating_sub(min_mv as u64).saturating_mul(100) / (max_mv - min_mv) as u64
    }.min(100);
    let values = [pin.max(0) as u64, raw.max(0) as u64, mv as u64, battery_mv, level, ref_mv as u64,
                  unit.max(0) as u64, channel.max(0) as u64];
    let rc = emit_array(hw, event_id, &values);
    if rc != 0 { return Err(rc); }
    Ok(values)
}

fn gpio_level(hw: &Hw, pin: i32, level: i32) -> i32 {
    let result = hw.gpio_config(pin, GPIO_OUTPUT, GPIO_PULL_NONE, level);
    if result == 0 { hw.gpio_write(pin, level) } else { result }
}

impl HwModule {
    // ---- battery ----

    /// Read the request (or the settings) and measure. With a control pin the measurement waits for the pin to
    /// settle: the result arrives as event 111 and the request is answered empty.
    fn battery(&mut self, hw: &Hw, data: &[u8]) -> i32 {
        let get = |key| field(data, key);
        let (Ok(pin), Ok(ref_mv), Ok(div), Ok(min), Ok(max), Ok(ctrl), Ok(lvl), Ok(enabled)) =
            (get(1), get(2), get(3), get(4), get(5), get(6), get(7), get(8)) else { return ERR_REQUEST };
        let b = Battery {
            pin: pin.map(|v| v as i32).unwrap_or_else(|| get_setting_i32(hw, b"battery.pin", DEFAULT_PIN)),
            ref_mv: ref_mv.map(|v| v as u32).unwrap_or_else(|| get_setting_i32(hw, b"battery.ref_mv", 3300) as u32),
            divider_x100: div.map(|v| v as u32).unwrap_or_else(|| {
                let compact = get_setting_i32(hw, b"battery.divider_x100", -1);
                if compact >= 0 { compact as u32 } else { get_setting_scaled(hw, b"battery.divider", 220).max(0) as u32 }
            }),
            min_mv: min.map(|v| v as u32).unwrap_or_else(|| get_setting_i32(hw, b"battery.min_mv", 3300) as u32),
            max_mv: max.map(|v| v as u32).unwrap_or_else(|| get_setting_i32(hw, b"battery.max_mv", 4200) as u32),
            ctrl_pin: ctrl.map(|v| v as i32).unwrap_or_else(|| get_setting_i32(hw, b"battery.ctrl", -1)),
            ctrl_level: lvl.map(|v| (v != 0) as i32).unwrap_or_else(|| get_setting_i32(hw, b"battery.ctl_lvl", 1)),
        };
        if enabled == Some(0) || b.pin < 0 { return 0; }
        if b.max_mv <= b.min_mv { return ERR_RANGE; }
        self.battery = b;
        if b.ctrl_pin >= 0 {
            if gpio_level(hw, b.ctrl_pin, b.ctrl_level) != 0 { return ERR_CTRL; }
            if self.battery_waiting { return 0; }   // already waiting: the running measurement uses the new values
            self.battery_waiting = true;
            let id = hw.alarm_set(SETTLE_MS, 0, 0, ALARM_EXACT, COMPONENT, M_BATTERY_SAMPLE);
            return if id >= 0 { 0 } else { id };
        }
        match self.measure(hw) {
            Ok(values) => {
                respond_map(hw, &[(1, values[0]), (2, values[1]), (3, values[2]), (4, values[3]), (5, values[4]),
                                  (6, values[5]), (7, values[6]), (8, values[7])]);
                0
            }
            Err(e) => e,
        }
    }

    fn measure(&self, hw: &Hw) -> Result<[u64; 8], i32> {
        let b = &self.battery;
        sample(hw, b.pin, b.ref_mv, EVENT_BATTERY, b.divider_x100, b.min_mv, b.max_mv)
    }

    /// The control pin has settled: measure, release the pin.
    fn battery_sample(&mut self, hw: &Hw) {
        self.battery_waiting = false;
        let _ = self.measure(hw);
        let b = self.battery;
        if b.ctrl_pin >= 0 { let _ = gpio_level(hw, b.ctrl_pin, if b.ctrl_level == 0 { 1 } else { 0 }); }
    }

    // ---- ADC probe ----

    fn probe(&mut self, hw: &Hw, data: &[u8]) -> i32 {
        let (Ok(samples), Ok(interval), Ok(ref_mv)) = (field(data, 1), field(data, 2), field(data, 3)) else { return ERR_REQUEST };
        let samples = samples.unwrap_or(1).max(0) as u32;
        let interval_ms = interval.unwrap_or(1000).clamp(1, 60_000) as u32;
        let mut pins = [34i32, 35, 36, 39];
        let mut count = 4usize;
        if let Ok(Some(mut r)) = cbor::get(data, 4) {
            let Some(Item::Array(n)) = r.next() else { return ERR_REQUEST };
            count = n.min(pins.len());
            for pin in pins.iter_mut().take(count) {
                *pin = r.next().and_then(|i| i.as_int()).unwrap_or(0) as i32;
            }
        }
        self.probe_stop(hw);
        self.probe_pins = pins;
        self.probe_count = count as u8;
        self.probe_ref_mv = ref_mv.unwrap_or(3300).max(1) as u32;
        self.probe_left = samples;
        self.probe_forever = samples == 0;
        let rc = self.probe_round(hw);
        if rc != 0 { return rc; }
        if self.probe_left > 0 || self.probe_forever {
            let id = hw.alarm_set(interval_ms, 0, interval_ms, ALARM_EXACT, COMPONENT, M_PROBE_TICK);
            if id < 0 { return id; }
            self.probe_alarm = id + 1;
        }
        0
    }

    /// Sample every pin once; counts the round.
    fn probe_round(&mut self, hw: &Hw) -> i32 {
        for pin in self.probe_pins.iter().take(self.probe_count as usize) {
            if let Err(e) = sample(hw, *pin, self.probe_ref_mv, EVENT_SAMPLE, 0, 0, 0) {
                self.probe_stop(hw);
                return e;
            }
        }
        if !self.probe_forever { self.probe_left = self.probe_left.saturating_sub(1); }
        0
    }

    fn probe_tick(&mut self, hw: &Hw) {
        if self.probe_round(hw) == 0 && !self.probe_forever && self.probe_left == 0 { self.probe_stop(hw); }
    }

    fn probe_stop(&mut self, hw: &Hw) {
        if self.probe_alarm > 0 { let _ = hw.alarm_cancel(self.probe_alarm - 1); }
        self.probe_alarm = 0;
        self.probe_left = 0;
        self.probe_forever = false;
    }

    // ---- button ----

    fn button(&mut self, hw: &Hw, data: &[u8]) -> i32 {
        let (Ok(pin), Ok(enabled)) = (field(data, 1), field(data, 2)) else { return ERR_REQUEST };
        if self.button_on {
            let _ = hw.irq_unregister(self.button_pin);
            self.button_on = false;
        }
        if enabled == Some(0) { return 0; }
        let pin = pin.map(|v| v as i32).unwrap_or_else(|| get_setting_i32(hw, b"button.gpio", 0));
        if hw.gpio_config(pin, GPIO_INPUT, GPIO_PULL_UP, 1) != 0 { return -3; }
        if hw.irq_register(pin, IRQ_BOTH, COMPONENT, M_BUTTON_IRQ) != 0 { return -4; }
        self.button_pin = pin;
        self.button_on = true;
        self.pressed_at = 0;
        self.last_release = 0;
        0
    }

    /// An edge: low is a press, high a release; the release classifies the press.
    fn button_edge(&mut self, hw: &Hw, level: i32) {
        let time = hw.now_ms();
        if level == 0 { self.pressed_at = time; return; }
        if self.pressed_at == 0 { return; }
        let held = time.saturating_sub(self.pressed_at);
        let id = if held >= LONG_MS { EVENT_LONG }
                 else if time.saturating_sub(self.last_release) <= DOUBLE_MS { EVENT_DOUBLE }
                 else { EVENT_SHORT };
        let _ = emit_array(hw, id, &[self.button_pin.max(0) as u64, held, 0]);
        self.last_release = time;
        self.pressed_at = 0;
    }
}

impl Module for HwModule {
    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(xip_abi::HOST_STOP) {
            self.probe_stop(hw);
            if self.button_on { let _ = hw.irq_unregister(self.button_pin); }
            return 0;
        }
        if call.component != COMPONENT { return xip_abi::RET_CONTINUE as i32; }
        let rc = match call.method {
            M_BATTERY => self.battery(hw, call.data),
            M_PROBE => self.probe(hw, call.data),
            M_BUTTON => self.button(hw, call.data),
            M_BUTTON_IRQ => { self.button_edge(hw, call.value); 0 }
            M_PROBE_TICK => { self.probe_tick(hw); 0 }
            M_BATTERY_SAMPLE => { self.battery_sample(hw); 0 }
            _ => ERR_REQUEST,
        };
        if call.wants_response() && rc != 0 {
            let mut buf = [0u8; 16];
            let mut w = Writer::new(&mut buf);
            w.map(1).uint(9).int(rc as i64);   // 9: error
            if let Some(encoded) = w.finish() { let _ = hw.respond(encoded); }
        }
        // a failed request does not end the module; only the host does
        xip_abi::RET_CONTINUE as i32
    }
}

/// Bytes of state the image asks for (`mkmod` in Cargo.toml).
pub const STATE_BYTES: usize = 128;

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::HwModule, state = $crate::STATE_BYTES);
    };
}

#[cfg(test)]
mod tests;

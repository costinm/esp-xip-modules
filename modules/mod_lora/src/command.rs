//! Requests. A method call of the module is `(component 43, method = operation)` with a CBOR map
//! `{1..30: integer config field, 31: bytes}` as data ([`apply_fields`]). The start request of the module keeps the
//! older forms so a console can still say `start lora rx freq=915000000 sf=9`:
//!
//! * text: `OP [name=value ...] [data=HEX | text=WORD]` (names: [`crate::config::field_id`]; decimal or 0x… numbers);
//! * CBOR: a map `{0: "OP", 1..30: integer, 31: bytes}`.
//!
//! Any config field in a request replaces the module's current value for the rest of the session.

use crate::config::{field_id, LoraConfig};
use xip_abi::cbor::{Item, Reader};

pub const MAX_PAYLOAD: usize = 255;

/// Operations; the numbers are the method ids of `API.md`.
pub const OP_PROBE: u8 = 1;
pub const OP_PROBE127: u8 = 2;
pub const OP_PROBE126: u8 = 3;
pub const OP_RX: u8 = 4;
pub const OP_TX: u8 = 5;
pub const OP_STOP: u8 = 6;
pub const OP_RECONFIGURE: u8 = 7;
pub const OP_FSK: u8 = 8;
pub const OP_STATS: u8 = 9;
/// Only in a start request: bring the module up and do nothing (the first method call brings the board up).
pub const OP_IDLE: u8 = 10;

/// The operation of a name (a chain of comparisons: see `config::field_id`).
pub fn op_from_name(name: &[u8]) -> Option<u8> {
    if name == b"probe" { return Some(OP_PROBE); }
    if name == b"probe127" { return Some(OP_PROBE127); }
    if name == b"probe126" { return Some(OP_PROBE126); }
    if name == b"rx" { return Some(OP_RX); }
    if name == b"tx" { return Some(OP_TX); }
    if name == b"stop" { return Some(OP_STOP); }
    if name == b"reconfigure" { return Some(OP_RECONFIGURE); }
    if name == b"fsk" { return Some(OP_FSK); }
    if name == b"stats" { return Some(OP_STATS); }
    if name == b"idle" { return Some(OP_IDLE); }
    None
}

fn number(s: &[u8]) -> Option<i64> {
    let (neg, s) = if let [b'-', rest @ ..] = s { (true, rest) } else { (false, s) };
    let (radix, digits) = if let [b'0', b'x', rest @ ..] = s { (16u32, rest) } else { (10u32, s) };
    if digits.is_empty() { return None }
    let mut v: i64 = 0;
    for &c in digits {
        v = v.checked_mul(radix as i64)?.checked_add((c as char).to_digit(radix)? as i64)?;
    }
    Some(if neg { -v } else { v })
}

/// Apply the fields of a request map to `config` and copy the data (field 31) into `payload`; returns its length.
/// Errors: -1 malformed, -2 unknown field or value that does not fit.
pub fn apply_fields(map: &[u8], config: &mut LoraConfig, payload: &mut [u8; MAX_PAYLOAD]) -> Result<usize, i32> {
    if map.is_empty() { return Ok(0) }
    let mut r = Reader::new(map);
    let Some(Item::Map(count)) = r.next() else { return Err(-1) };
    let mut data_len = 0usize;
    for _ in 0..count {
        let Some(Item::Uint(key)) = r.next() else { return Err(-1) };
        match (key, r.next().ok_or(-1)?) {
            (31, Item::Bytes(b)) => {
                if b.len() > MAX_PAYLOAD { return Err(-1) }
                payload[..b.len()].copy_from_slice(b);
                data_len = b.len();
            }
            (1..=30, item) => { config.set(key as u8, item.as_int().ok_or(-2)?).ok_or(-2)?; }
            _ => return Err(-2),
        }
    }
    if !r.rest().is_empty() { return Err(-1) }
    Ok(data_len)
}

/// The start request: `(operation, data length)`. Empty means `probe`.
pub fn parse_start(request: &[u8], config: &mut LoraConfig, payload: &mut [u8; MAX_PAYLOAD]) -> Result<(u8, usize), i32> {
    let mut trimmed = request;
    while let [first, rest @ ..] = trimmed { if *first == b' ' || *first == 0 { trimmed = rest } else { break } }
    while let [rest @ .., last] = trimmed { if *last == b' ' || *last == 0 || *last == b'\n' { trimmed = rest } else { break } }
    if trimmed.is_empty() { return Ok((OP_PROBE, 0)) }
    if trimmed[0] & 0xe0 == 0xa0 { parse_cbor(trimmed, config, payload) } else { parse_text(trimmed, config, payload) }
}

fn parse_cbor(request: &[u8], config: &mut LoraConfig, payload: &mut [u8; MAX_PAYLOAD]) -> Result<(u8, usize), i32> {
    let mut r = Reader::new(request);
    let Some(Item::Map(count)) = r.next() else { return Err(-1) };
    let (mut op, mut data_len) = (0u8, 0usize);
    for _ in 0..count {
        let Some(Item::Uint(key)) = r.next() else { return Err(-1) };
        match (key, r.next().ok_or(-1)?) {
            (0, Item::Text(t)) => op = op_from_name(t).ok_or(-2)?,
            (31, Item::Bytes(b)) => {
                if b.len() > MAX_PAYLOAD { return Err(-1) }
                payload[..b.len()].copy_from_slice(b);
                data_len = b.len();
            }
            (1..=30, item) => { config.set(key as u8, item.as_int().ok_or(-2)?).ok_or(-2)?; }
            _ => return Err(-2),
        }
    }
    if !r.rest().is_empty() || op == 0 { return Err(-2) }
    Ok((op, data_len))
}

fn parse_text(request: &[u8], config: &mut LoraConfig, payload: &mut [u8; MAX_PAYLOAD]) -> Result<(u8, usize), i32> {
    let (mut data_len, mut op, mut first) = (0usize, 0u8, true);
    for token in request.split(|&c| c == b' ' || c == b'\t').filter(|t| !t.is_empty()) {
        if first {
            first = false;
            op = op_from_name(token).ok_or(-2)?;
            continue;
        }
        let eq = token.iter().position(|&c| c == b'=').ok_or(-1)?;
        let (name, value) = (&token[..eq], &token[eq + 1..]);
        if name == b"text" {
            if value.len() > MAX_PAYLOAD { return Err(-1) }
            payload[..value.len()].copy_from_slice(value);
            data_len = value.len();
        } else if name == b"data" {
            if value.len() % 2 != 0 || value.len() / 2 > MAX_PAYLOAD { return Err(-1) }
            for j in 0..value.len() / 2 {
                let hi = (value[2 * j] as char).to_digit(16).ok_or(-1)?;
                let lo = (value[2 * j + 1] as char).to_digit(16).ok_or(-1)?;
                payload[j] = (hi * 16 + lo) as u8;
            }
            data_len = value.len() / 2;
        } else {
            let id = field_id(name).ok_or(-2)?;
            config.set(id, number(value).ok_or(-1)?).ok_or(-2)?;
        }
    }
    Ok((op, data_len))
}

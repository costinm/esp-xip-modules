//! The ESP-NOW-compatible action frame: DMesh's `dmesh_rawnan::espnow` codec, ported unchanged (no allocation, no error type) so the
//! `espnow` link and DMesh put the same bytes on the air. A datagram of up to 1400 bytes travels as a vendor action (category 127,
//! Espressif OUI `18:fe:34`): only the first bytes are in the ESP format (the 8-byte action header and one vendor element), the rest of the
//! datagram follows raw (DMesh's `FirstIeThenTail`; the receive filter only looks at the start, nothing checks the rest). The parser also
//! takes DMesh's older sequence of 250-byte elements, so frames from a peer that still sends them are read. The C twin is
//! `components/xip_runtime/src/xip_espnow_frame.h`; both are checked against `tests/data/espnow-frames.txt`, which was generated from DMesh's code.

/// Vendor action category and Espressif OUI.
pub const ACTION_PREFIX: [u8; 4] = [0x7f, 0x18, 0xfe, 0x34];
pub const ACTION_TYPE: u8 = 0x04;
pub const IEEE80211_HEADER_LEN: usize = 24;
const ACTION_HEADER_LEN: usize = 8;
const VENDOR_IE_HEADER_LEN: usize = 7;
const VENDOR_IE_ID: u8 = 0xdd;
/// A datagram may span several vendor elements.
pub const MAX_ACTION_PAYLOAD: usize = 1400;
const MAX_IE_BODY: usize = 250;
const ESPNOW_V2_VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionFrameError {
    PayloadTooLarge,
    OutputTooSmall,
}

/// Bytes of a frame carrying `payload_len` bytes.
pub const fn frame_len(payload_len: usize) -> usize {
    IEEE80211_HEADER_LEN + ACTION_HEADER_LEN + (if payload_len == 0 { 0 } else { VENDOR_IE_HEADER_LEN }) + payload_len
}

fn action_header() -> [u8; ACTION_HEADER_LEN] {
    let mut header = [0; ACTION_HEADER_LEN];
    header[..4].copy_from_slice(&ACTION_PREFIX);
    // the random value of the ESP-NOW envelope; parsers attach no meaning to it
    header[4..8].copy_from_slice(&[0xff; 4]);
    header
}

/// Encode one vendor action frame into caller-owned storage: the length used.
pub fn encode_action_frame(
    out: &mut [u8],
    destination: [u8; 6],
    source: [u8; 6],
    bssid: [u8; 6],
    payload: &[u8],
) -> Result<usize, ActionFrameError> {
    if payload.len() > MAX_ACTION_PAYLOAD {
        return Err(ActionFrameError::PayloadTooLarge);
    }
    let used = frame_len(payload.len());
    if out.len() < used {
        return Err(ActionFrameError::OutputTooSmall);
    }
    out[..used].fill(0);
    out[..4].copy_from_slice(&[0xd0, 0, 0, 0]);
    out[4..10].copy_from_slice(&destination);
    out[10..16].copy_from_slice(&source);
    out[16..22].copy_from_slice(&bssid);
    let action = &mut out[IEEE80211_HEADER_LEN..used];
    action[..ACTION_HEADER_LEN].copy_from_slice(&action_header());
    if !payload.is_empty() {
        // one valid vendor element with the first bytes of the datagram, then the rest raw
        let first = payload.len().min(MAX_IE_BODY);
        let ie = &mut action[ACTION_HEADER_LEN..];
        ie[0] = VENDOR_IE_ID;
        ie[1] = (5 + first) as u8;
        ie[2..5].copy_from_slice(&ACTION_PREFIX[1..4]);
        ie[5] = ACTION_TYPE;
        ie[6] = ESPNOW_V2_VERSION;
        ie[VENDOR_IE_HEADER_LEN..].copy_from_slice(payload);
    }
    Ok(used)
}

/// Parse a complete raw action frame into caller-owned storage: the source address and the datagram length.
pub fn parse_action_frame_into(frame: &[u8], output: &mut [u8]) -> Option<([u8; 6], usize)> {
    if frame.len() < IEEE80211_HEADER_LEN + ACTION_HEADER_LEN + VENDOR_IE_HEADER_LEN || frame[0] != 0xd0 {
        return None;
    }
    let source = frame.get(10..16)?.try_into().ok()?;
    let used = parse_action_body_into(frame.get(IEEE80211_HEADER_LEN..)?, output)?;
    Some((source, used))
}

/// Parse the action body a native receive callback supplies (the 802.11 header is not always available: the private vendor
/// dispatcher hands over the body, already reassembled from its elements) into `output`; the datagram length.
pub fn parse_action_body_into(action: &[u8], output: &mut [u8]) -> Option<usize> {
    if action.get(..4)? != ACTION_PREFIX {
        return None;
    }
    let mut ie_offset: usize = ACTION_HEADER_LEN;
    let mut written: usize = 0;
    loop {
        let ie = action.get(ie_offset..)?;
        let element_len = ie.get(1).copied()? as usize;
        let end = 2usize.checked_add(element_len)?;
        let element = ie.get(..end)?;
        if element[0] != VENDOR_IE_ID
            || element_len < 5
            || element.get(2..5) != Some(&ACTION_PREFIX[1..4])
            || element[5] != ACTION_TYPE
            || element[6] & 0x0f != ESPNOW_V2_VERSION
        {
            return None;
        }
        let body = &element[VENDOR_IE_HEADER_LEN..];
        let next = written.checked_add(body.len())?;
        if next > output.len() {
            return None;
        }
        output[written..next].copy_from_slice(body);
        written = next;
        let more = element[6] & 0x10 != 0;
        ie_offset = ie_offset.checked_add(end)?;
        if !more {
            // the one-element-plus-tail form is an RX-filter experiment: still one frame. Promiscuous metadata may keep the
            // 802.11 FCS: a complete v2 sequence has no tail but those four bytes, which are not datagram bytes.
            let tail = action.get(ie_offset..).unwrap_or_default();
            if tail.len() == 4 {
                return Some(written);
            }
            let next = written.checked_add(tail.len())?;
            if next > output.len() {
                return None;
            }
            output[written..next].copy_from_slice(tail);
            return Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{vec, vec::Vec};

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
    fn mac(s: &str) -> [u8; 6] {
        let v: Vec<u8> = s.split(':').map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
        v.try_into().unwrap()
    }

    /// The reference vectors come from DMesh's code: our bytes on the air are DMesh's.
    #[test]
    fn the_frames_are_those_of_dmesh() {
        let mut encoded = 0;
        let mut parsed = 0;
        for line in include_str!("../../../tests/data/espnow-frames.txt").lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let f: Vec<&str> = line.split(' ').collect();
            match f[0] {
                "encode" => {
                    let n: usize = f[5].parse().unwrap();
                    // the sequence of elements (v2) is what DMesh sent before; for one element it is the same bytes
                    if f[1] == "v2" && n > 250 {
                        continue;
                    }
                    let payload: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
                    let mut out = vec![0u8; 24 + 8 + 7 * n.div_ceil(250) + n + 16];
                    let used = encode_action_frame(&mut out, mac(f[2]), mac(f[3]), mac(f[4]), &payload).unwrap();
                    assert_eq!(out[..used], unhex(f[6])[..], "encode {} {}", f[1], n);
                    assert_eq!(used, frame_len(n));
                    encoded += 1;
                }
                "parse-body" => {
                    let body = unhex(f[1]);
                    let mut out = vec![0u8; 1400];
                    let got = parse_action_body_into(&body, &mut out);
                    if f[2] == "none" {
                        assert_eq!(got, None, "{}", f[1]);
                    } else {
                        assert_eq!(got, Some(f[2].parse().unwrap()), "{}", f[1]);
                        assert_eq!(out[..got.unwrap()], unhex(f[3])[..]);
                    }
                    parsed += 1;
                }
                "parse-small" => {
                    let mut out = [0u8; 4];
                    assert_eq!(parse_action_body_into(&unhex(f[1]), &mut out), None);
                    parsed += 1;
                }
                other => panic!("{other}"),
            }
        }
        assert!(encoded >= 5 && parsed >= 15, "{encoded} {parsed}");
    }

    #[test]
    fn a_whole_frame_round_trips_with_its_source() {
        let payload = [0x40, 1, 2, 3];
        let mut frame = [0u8; 64];
        let used = encode_action_frame(&mut frame, [1, 2, 3, 4, 5, 6], [6, 5, 4, 3, 2, 1], [0xff; 6], &payload).unwrap();
        let mut out = [0u8; 8];
        assert_eq!(parse_action_frame_into(&frame[..used], &mut out), Some(([6, 5, 4, 3, 2, 1], 4)));
        assert_eq!(&out[..4], &payload);
        assert_eq!(encode_action_frame(&mut [0u8; 8], [0; 6], [0; 6], [0; 6], &payload), Err(ActionFrameError::OutputTooSmall));
        assert_eq!(encode_action_frame(&mut [0u8; 4000], [0; 6], [0; 6], [0; 6], &[0u8; 1401]), Err(ActionFrameError::PayloadTooLarge));
    }
}

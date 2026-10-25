//! Android's discovery/Follow-up contract. Logical IDs are not radio MACs.
use dmesh_server::control::{self, Request, TransportConfig, TransportKind};
use xip_abi::*;

pub fn content<'a>(payload: &'a [u8], local: [u8; 6]) -> Option<(&'a [u8], bool)> {
    // Android's Wi-Fi Aware service-specific information may carry the WFA
    // generic-service prefix before the DMesh Follow-up wrapper.
    let payload = payload
        .strip_prefix(&[0x50, 0x6f, 0x9a, 2])
        .unwrap_or(payload);
    if let Some(f) = dmesh_rawnan::parse_dmesh_nan_followup(payload) {
        if f.target_id != local
            || !matches!(f.msg_type, 2 | 7)
            || payload.len() != 24 + f.payload.len()
            || payload.get(20..24)
                != Some(dmesh_rawnan::fnv1a32(f.payload).to_le_bytes().as_slice())
        {
            return None;
        }
        // Android derives device_id from its public-key string; PeerHandle's
        // randomized RF MAC remains the return address. Neither is authentication.
        Some((f.payload, f.msg_type == 2))
    } else if payload.starts_with(&dmesh_rawnan::DMESH_MAGIC) {
        None
    } else {
        Some((payload, false))
    }
}

/// Only explicit, locally targeted activation belongs to this module. Discovery
/// requests and announcements must never change the sleep policy.
pub fn request<'a>(
    record: &'a [u8],
    local: [u8; 6],
) -> Result<Option<(TransportKind, TransportConfig<'a>)>, i32> {
    if dmesh_server::tagged::decode(record).is_some_and(|r| r.result.is_some() || r.error.is_some())
    {
        return Ok(None);
    }
    let Some(Request::TransportSet { kind, config }) = control::decode_request(record) else {
        return Ok(None);
    };
    let Some(target) = config.wake_target else {
        return Ok(None);
    };
    if target != local {
        return Err(ERR_ARG);
    }
    Ok(Some((kind, config)))
}

/// Apply the fields emitted by Android build_sta_activation/build_activation
/// to the saved active intent, retaining provisioned credentials. Validate the
/// entire request before touching the radio or cancelling sleepy mode.
pub fn intent(
    kind: TransportKind,
    config: TransportConfig<'_>,
    mut saved: WifiIntent,
) -> Result<(WifiIntent, u64), i32> {
    let supported = TransportConfig {
        wake_target: config.wake_target,
        nan_dw_interval: config.nan_dw_interval,
        now: config.now,
        ap: config.ap,
        ble: config.ble,
        ..Default::default()
    };
    if config != supported
        || !matches!(kind, TransportKind::Sta | TransportKind::Nan)
        || config.ble.is_some_and(|b| b != 0)
    {
        // Explicit BLE overrides and UART-only activation belong to their
        // respective policy owners; do not acknowledge them as applied here.
        return Err(ERR_UNSUPPORTED);
    }
    let dw = config
        .nan_dw_interval
        .map(u64::from)
        .unwrap_or(crate::protocol::WAKE_DW);
    if !matches!(dw, 1 | 8 | 16) {
        return Err(ERR_UNSUPPORTED);
    }
    saved.radio = 1;
    saved.sta = u8::from(kind == TransportKind::Sta);
    if saved.sta != 0 && saved.sta_ssid_len == 0 && saved.bssid == [0; 6] {
        return Err(ERR_ARG);
    }
    if let Some(ap) = config.ap {
        saved.ap = ap;
    }
    if let Some(now) = config.now {
        saved.now = u8::from(now != 2 || config.nan_dw_interval.is_some_and(|dw| dw != 0));
    }
    // BLE policy 0 is the shared default, not a request to disable BLE.
    Ok((saved, dw))
}

/// Small fixed subset of control.transport.set used by the outbound wake.
/// Kept allocation-free; tests compare byte-for-byte with the shared codec.
pub fn encode_activation(out: &mut [u8], target: [u8;6], nan: bool, id: u64) -> Option<usize> {
    let mut w=xip_abi::cbor::Writer::new(out);
    w.map(4).uint(1).uint(control::CONTROL_COMPONENT).uint(2).uint(control::TRANSPORT_SET)
        .uint(3).uint(id).uint(5).map(if nan {3}else{2}).uint(1).uint(if nan {6}else{1});
    if nan {w.uint(16).uint(1);}
    w.uint(25).bytes(&target);
    w.finish().map(|b|b.len())
}
#[cfg(test)]
mod tests {
    #[test]
    fn fixed_activation_encoding_matches_shared_wire_codec() {
        for nan in [false,true] {
            let mut actual=[0;96];let mut expected=[0;96];let target=[2;6];
            let n=encode_activation(&mut actual,target,nan,77).unwrap();
            let request=control::Request::TransportSet {kind:if nan {TransportKind::Nan}else{TransportKind::Sta},
                config:TransportConfig {wake_target:Some(target),ap:if nan {Some(1)}else{None},..Default::default()}};
            let m=control::encode_request(request,Some(77),&mut expected).unwrap();
            assert_eq!(&actual[..n],&expected[..m]);
        }
    }

    use super::*;
    #[test]
    fn wfa_prefix_and_exact_wrapper_length_are_checked() {
        let body = [0xa0];
        let frame =
            dmesh_rawnan::build_dmesh_followup_payload(2, 1, [9; 6], [1; 6], &body).unwrap();
        let mut prefixed = alloc::vec![0x50, 0x6f, 0x9a, 2];
        prefixed.extend_from_slice(&frame);
        assert_eq!(content(&prefixed, [1; 6]), Some((&body[..], true)));
        prefixed.push(0);
        assert!(content(&prefixed, [1; 6]).is_none());
        assert!(content(&frame[..frame.len() - 1], [1; 6]).is_none());
    }
}

//! Encode directly into a uniquely owned pool packet. No temporary frame Vec.
use dmesh_rawnan as wire;
/// SDEA Generic SSI, with the Android-compatible fixed nine-byte SDA.
/// The discovery sender builds a bounded NAN Subscribe or Publish frame.
pub fn sdf(
    out: &mut [u8],
    local: [u8; 6],
    dest: [u8; 6],
    bssid: [u8; 6],
    instance: u8,
    requestor: u8,
    control: u8,
    payload: &[u8],
) -> Option<usize> {
    let followup = control & 3 == 2;
    let header = if followup { 54 } else { 56 };
    let n = header + payload.len();
    if payload.len() > 255 || out.len() < n {
        return None;
    }
    out[..n].fill(0);
    out[0] = 0xd0;
    out[4..10].copy_from_slice(&dest);
    out[10..16].copy_from_slice(&local);
    out[16..22].copy_from_slice(&bssid);
    out[24..30].copy_from_slice(&[4, 9, 0x50, 0x6f, 0x9a, 0x13]);
    out[30] = 3;
    out[31..33].copy_from_slice(&9u16.to_le_bytes());
    out[33..39].copy_from_slice(&wire::DMESH_SERVICE_ID);
    out[39..42].copy_from_slice(&[instance, requestor, control]);
    out[42] = 0x0e;
    let body = if followup { 9 } else { 11 };
    out[43..45].copy_from_slice(&((body + payload.len()) as u16).to_le_bytes());
    if followup {
        out[45] = instance;
        out[48..50].copy_from_slice(&((4 + payload.len()) as u16).to_le_bytes());
        out[50..54].copy_from_slice(&[0x50, 0x6f, 0x9a, 2]);
    } else {
        // Preserve the shared USD Subscribe spelling, including its control
        // and WFA protocol prefix; this is different from Follow-up SSI.
        out[45..50].copy_from_slice(&[instance, requestor, 0, 0, 4]);
        out[50..52].copy_from_slice(&((4 + payload.len()) as u16).to_le_bytes());
        out[52..56].copy_from_slice(&[0x50, 0x6f, 0x9a, 0]);
    }
    out[header..n].copy_from_slice(payload);
    Some(n)
}
/// Discovery replies build a normal application Follow-up for the observed peer.
pub fn followup(
    out: &mut [u8],
    local: [u8; 6],
    dest: [u8; 6],
    bssid: [u8; 6],
    requestor: u8,
    sequence: u16,
    payload: &[u8],
) -> Option<usize> {
    if payload.len() > wire::NAN_COMMAND_MAX_LEN {
        return None;
    }
    let n = 54 + wire::DMESH_NAN_FOLLOWUP_HEADER_LEN + payload.len();
    if out.len() < n {
        return None;
    }
    // Make the header with an empty SSI, then append the wrapper directly.
    sdf(out, local, dest, bssid, 1, requestor, 2, &[])?;
    let len = wire::DMESH_NAN_FOLLOWUP_HEADER_LEN + payload.len();
    out[43..45].copy_from_slice(&((9 + len) as u16).to_le_bytes());
    out[48..50].copy_from_slice(&((4 + len) as u16).to_le_bytes());
    let p = &mut out[54..n];
    p[..2].copy_from_slice(&wire::DMESH_MAGIC);
    p[2..4].copy_from_slice(&[wire::DMESH_VERSION, 7]);
    p[4..6].copy_from_slice(&sequence.to_le_bytes());
    p[6..12].copy_from_slice(&local);
    p[12..18].copy_from_slice(&dest);
    p[18..20].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    p[20..24].copy_from_slice(&wire::fnv1a32(payload).to_le_bytes());
    p[24..].copy_from_slice(payload);
    Some(n)
}
/// The sender selects normal or Android activation Follow-up framing.
pub fn followup_type(out: &mut [u8], local: [u8;6], dest: [u8;6], bssid: [u8;6], requestor: u8, sequence: u16, payload: &[u8], msg_type: u8) -> Option<usize> {
    let n=followup(out,local,dest,bssid,requestor,sequence,payload)?;
    out[54+3]=msg_type;
    Some(n)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pooled_subscribe_matches_portable_and_keeps_request_correlation() {
        let mut out = [0; 512];
        let mut request = [0; 32];
        let len = dmesh_server::announce::encode_discovery_request(42, &mut request).unwrap();
        let n = sdf(
            &mut out,
            [1; 6],
            wire::NAN_DISCOVERY_MAC,
            [3; 6],
            1,
            0,
            0x11,
            &request[..len],
        )
        .unwrap();
        assert_eq!(
            &out[..n],
            wire::build_nan_usd_sdf_with_bssid(
                wire::NAN_DISCOVERY_MAC,
                [1; 6],
                [3; 6],
                wire::DMESH_SERVICE_ID,
                1,
                0x11,
                &request[..len]
            )
        );
        let got = wire::active_subscribe_service_info(&out[..n], wire::DMESH_SERVICE_ID).unwrap();
        assert_eq!(
            dmesh_server::announce::discovery_request_id(got.service_info),
            Some(42)
        );
    }
    #[test]
    fn pooled_followup_matches_portable_android_sdea_encoding() {
        let mut out = [0; 512];
        let local = [1; 6];
        let peer = [2; 6];
        let bssid = [3; 6];
        let n = followup(&mut out, local, peer, bssid, 7, 18, b"reply").unwrap();
        let inner = wire::build_dmesh_followup_payload(7, 18, local, peer, b"reply").unwrap();
        assert_eq!(
            &out[..n],
            wire::build_nan_followup_sdf_for_requestor(
                peer,
                local,
                bssid,
                wire::DMESH_SERVICE_ID,
                1,
                7,
                &inner
            )
        );
        assert_eq!(
            wire::parse_dmesh_nan_followup(
                wire::followup_service_info(&out[..n], wire::DMESH_SERVICE_ID).unwrap()
            )
            .unwrap()
            .payload,
            b"reply"
        );
    }
}

//! Optional local announcement policy; private keys and crypto remain native.
use alloc::vec::Vec;
use dmesh_server::announce as a;
use xip_abi::*;
pub const INFO_MAX: usize = 255;

// Create a signed announce with our current hardware addresses and identity.
pub fn make_announce(
    hw: &Hw,
    channel: u8,
    out: &mut [u8; INFO_MAX],
) -> Result<(a::Announce, usize), i32> {
    // the runtime's crypto and identity: this module links none and never sees the key
    let crypto = CryptoClient::from_host(*hw).ok_or(ERR_UNSUPPORTED)?;
    let public = crypto.identity_public_key()?;
    make_announce_with(
        hw,
        channel,
        out,
        public,
        |input| crypto.sha256(input),
        |message| crypto.identity_sign(message),
    )
}
fn make_announce_with(
    hw: &Hw,
    channel: u8,
    out: &mut [u8; INFO_MAX],
    public: [u8; 65],
    digest: impl FnOnce(&[u8]) -> Result<[u8; 32], i32>,
    sign: impl FnOnce(&[u8]) -> Result<[u8; 64], i32>,
) -> Result<(a::Announce, usize), i32> {
    let mut compressed = [0; 33];
    compressed[0] = 2 | (public[64] & 1);
    compressed[1..].copy_from_slice(&public[1..33]);
    let digest = digest(&compressed)?;
    let mut hint = [0; 8];
    hint.copy_from_slice(&digest[..8]);
    let mut id = [0; 16];
    id[..8].copy_from_slice(&hint);
    let mut own = a::Announce::discovery(id, 8, 0);
    if !own.set_public_key(&compressed) {
        return Err(ERR_ARG);
    }
    let mut info = Vec::new();
    info.try_reserve_exact(3072).map_err(|_| ERR_NOMEM)?;
    info.resize(3072, 0);
    let n = hw.device_info(&mut info)?;
    let chip = cbor::get(&info[..n], 1)
        .ok()
        .flatten()
        .and_then(|r| cbor::get_int(r.rest(), 2).ok().flatten())
        .unwrap_or(-1);
    let class = match chip {
        13 => a::DEVICE_CLASS_ESP32C6,
        9 => a::DEVICE_CLASS_ESP32S3,
        _ => a::DEVICE_CLASS_ESP32,
    };
    own.set_probe_descriptor(
        class,
        dmesh_server::probe::PROBE_CAP_NAN
            | dmesh_server::probe::PROBE_CAP_NOW
            | dmesh_server::probe::PROBE_CAP_STA
            | dmesh_server::probe::PROBE_CAP_AP,
    );
    drop(info);
    let mut text = [0; 32];
    let n = hw.nv(b"name", None, &mut text);
    if n > 0 {
        if let Ok(s) = core::str::from_utf8(&text[..n as usize]) {
            own.set_device_name(s);
        }
    }
    let n = hw.nv(b"domain", None, &mut text);
    if n > 0 {
        if let Ok(s) = core::str::from_utf8(&text[..n as usize]) {
            own.set_device_domain(s);
        }
    }
    let mut actual = WifiState {
        size: core::mem::size_of::<WifiState>() as u32,
        ..Default::default()
    };
    if let Some(state) = hw.wifi().and_then(|w| w.state) {
        if unsafe { state(hw.ctx(), &mut actual) } == 0 && actual.sta == 2 {
            if let Ok(ssid) = core::str::from_utf8(&actual.ssid[..actual.ssid_len as usize]) {
                own.set_network_name(ssid);
            }
        }
    }
    own.set_wifi_channel(if actual.channel != 0 {
        actual.channel
    } else {
        channel
    });
    let mut signing = [0; 384];
    let n = a::signing_bytes(own, &mut signing).ok_or(ERR_SIZE)?;
    let signature = sign(&signing[..n])?;
    if !own.set_signature(&signature) {
        return Err(ERR_ARG);
    }
    let len = a::encode(own, out).ok_or(ERR_SIZE)?;
    Ok((own, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;
    use dmesh_server::tagged;
    pub fn verify_announce(
        announce: a::Announce,
        digest: impl FnOnce(&[u8]) -> Result<[u8; 32], i32>,
        verify: impl FnOnce(&[u8], &[u8], &[u8]) -> Result<(), i32>,
    ) -> Result<[u8; 8], i32> {
        let key = announce.public_key();
        if !matches!(key.len(), 33 | 65) || announce.signature().len() != 64 {
            return Err(ERR_ARG);
        }
        let hash = digest(key)?;
        let mut hint = [0; 8];
        hint.copy_from_slice(&hash[..8]);
        if announce.device_id() != hint {
            return Err(ERR_ARG);
        }
        let mut signing = [0; 384];
        let len = a::signing_bytes(announce, &mut signing).ok_or(ERR_SIZE)?;
        verify(key, announce.signature(), &signing[..len])?;
        Ok(hint)
    }

    use p256::ecdsa::{
        signature::{Signer, Verifier},
        Signature, SigningKey, VerifyingKey,
    };
    use sha2::{Digest, Sha256};
    fn key() -> SigningKey {
        SigningKey::from_slice(&[7; 32]).unwrap()
    }
    fn public() -> [u8; 65] {
        key().verifying_key().to_encoded_point(false).as_bytes().try_into().unwrap()
    }
    unsafe extern "C" fn info(_: *mut c_void, out: *mut u8, cap: usize) -> i32 {
        let mut w = cbor::Writer::new(core::slice::from_raw_parts_mut(out, cap));
        w.map(1).uint(1).map(1).uint(2).uint(13);
        w.finish().unwrap().len() as i32
    }
    fn digest(input: &[u8]) -> Result<[u8; 32], i32> {
        Ok(Sha256::digest(input).into())
    }
    fn verify(public: &[u8], signature: &[u8], message: &[u8]) -> Result<(), i32> {
        VerifyingKey::from_sec1_bytes(public)
            .map_err(|_| ERR_ARG)?
            .verify(
                message,
                &Signature::from_slice(signature).map_err(|_| ERR_ARG)?,
            )
            .map_err(|_| ERR_ARG)
    }
    #[test]
    fn signs_once_and_reuses_identical_signed_facts_for_every_response() {
        let calls = core::cell::Cell::new(0usize);
        let mut host: Host = unsafe { core::mem::zeroed() };
        host.abi_version = ABI_VERSION;
        host.size = core::mem::size_of::<Host>() as u32;
        host.device_info = Some(info);
        let hw = unsafe { Hw::new(&host) }.unwrap();
        let mut record = [0; INFO_MAX];
        let (own, len) = make_announce_with(&hw, 6, &mut record, public(), digest, |m| {
            calls.set(calls.get() + 1);
            let sig: Signature = key().sign(m);
            Ok(sig.to_bytes().into())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        verify_announce(own, digest, verify).unwrap();
        for id in [0, 1, 42, u64::MAX] {
            let mut response = [0; 255];
            let n = a::encode_discovery_response(own, id, &mut response).unwrap();
            let decoded = tagged::decode(&response[..n]).unwrap();
            assert_eq!(decoded.id, Some(id));
            let replied = a::decode_record(decoded).unwrap();
            verify_announce(replied, digest, verify).unwrap();
            assert_eq!(replied.signature(), own.signature());
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(
            a::decode_announce(&record[..len]).unwrap().public_key(),
            own.public_key()
        );
    }
}

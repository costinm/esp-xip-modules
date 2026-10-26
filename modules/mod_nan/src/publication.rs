//! Opaque publication transport. No signing, certificate or identity policy.
use dmesh_server::{announce, tagged};
use xip_abi::{
    cbor::{self, Item, Reader, Writer},
    ERR_ARG,
};

/// HOST_START uses this to validate the optional opaque publication.

pub fn start_record(data: &[u8]) -> Result<&[u8], i32> {
    if data.is_empty() {
        return Ok(&[]);
    }
    match cbor::get(data, 2).map_err(|_| ERR_ARG)? {
        None => Ok(&[]),
        Some(mut r) => match r.next() {
            Some(Item::Bytes(b)) => Ok(b),
            _ => Err(ERR_ARG),
        },
    }
}

/// Announcements are observations, never another application handler request.
/// Recognize only their envelope; interpretation/verification is a consumer job.
/// The discovery dispatcher uses this to distinguish supplier publications from requests.
pub fn is_announcement(packet: &[u8]) -> bool {
    tagged::decode(packet).is_some_and(|r| {
        r.component == Some(tagged::Name::Tag(announce::ANNOUNCE_COMPONENT))
            && matches!(
                r.method,
                Some(tagged::Name::Tag(
                    announce::ANNOUNCE_DISCOVERY
                        | announce::ANNOUNCE_TRANSITION_BEGIN
                        | announce::ANNOUNCE_SLEEP_PENDING
                        | announce::ANNOUNCE_TRANSITION_COMPLETE
                        | announce::ANNOUNCE_WAKE
                ))
            )
            && r.fields.is_some_and(
                |fields| matches!(Reader::new(fields).next(), Some(Item::Map(n)) if n > 0),
            )
    })
}

/// Correlation is outside the signed facts. Copy those bytes exactly instead of
/// decoding/re-encoding the announcement or depending on a certificate schema.
/// Discovery reply preparation uses this to correlate an opaque supplier envelope.
pub fn response(packet: &[u8], id: u64, out: &mut [u8]) -> Option<usize> {
    let r = tagged::decode(packet)?;
    if r.component != Some(tagged::Name::Tag(announce::ANNOUNCE_COMPONENT))
        || r.method != Some(tagged::Name::Tag(announce::ANNOUNCE_DISCOVERY))
        || r.to.is_some()
        || r.result.is_some()
        || r.error.is_some()
    {
        return None;
    }
    let fields = r.fields?;
    if !matches!(Reader::new(fields).next(), Some(Item::Map(n)) if n > 0) {
        return None;
    }
    let mut w = Writer::new(out);
    w.map(4)
        .uint(1)
        .uint(announce::ANNOUNCE_COMPONENT)
        .uint(2)
        .uint(announce::ANNOUNCE_DISCOVERY)
        .uint(3)
        .uint(id)
        .uint(5)
        .raw(fields);
    w.finish().map(|b| b.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{
        signature::{Signer, Verifier},
        Signature, SigningKey,
    };
    use sha2::{Digest, Sha256};

    #[test]
    fn supplied_signature_and_future_fields_are_not_rewritten_for_correlation() {
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        let public = key.verifying_key().to_encoded_point(true);
        let hash = Sha256::digest(public.as_bytes());
        let mut hint = [0; 16];
        hint[..8].copy_from_slice(&hash[..8]);
        let mut a = announce::Announce::discovery(hint, 8, 0);
        assert!(a.set_public_key(public.as_bytes()));
        let mut signed = [0; 384];
        let n = announce::signing_bytes(a, &mut signed).unwrap();
        let sig: Signature = key.sign(&signed[..n]);
        assert!(a.set_signature(&sig.to_bytes()));
        let mut packet = [0; 255];
        let n = announce::encode(a, &mut packet).unwrap();
        for id in [0, 42, u64::MAX] {
            let mut reply = [0; 280];
            let nreply = response(&packet[..n], id, &mut reply).unwrap();
            let input = tagged::decode(&packet[..n]).unwrap();
            let output = tagged::decode(&reply[..nreply]).unwrap();
            assert_eq!(output.id, Some(id));
            assert_eq!(output.fields, input.fields);
            let replied = announce::decode_record(output).unwrap();
            let nsigned = announce::signing_bytes(replied, &mut signed).unwrap();
            key.verifying_key()
                .verify(
                    &signed[..nsigned],
                    &Signature::from_slice(replied.signature()).unwrap(),
                )
                .unwrap();
        }
        // A future supplier schema is transported without Announce decoding.
        let unknown = [
            0xa3, 1, 6, 2, 2, 5, 0xa1, 0x18, 99, 0xa1, 1, 0x42, 0x7e, 0x7d,
        ];
        let mut out = [0; 80];
        let n = response(&unknown, 7, &mut out).unwrap();
        assert_eq!(
            tagged::decode(&out[..n]).unwrap().fields,
            tagged::decode(&unknown).unwrap().fields
        );
    }

    #[test]
    fn empty_and_non_announcement_publications_do_not_invent_identity_responses() {
        assert_eq!(start_record(&[]), Ok(&[][..]));
        assert_eq!(start_record(&[0xa0]), Ok(&[][..]));
        assert_eq!(start_record(&[0xa1, 2, 0x42, 1, 2]), Ok(&[1, 2][..]));
        assert_eq!(start_record(&[0xa1, 2, 0x61, b'x']), Err(ERR_ARG));
        let mut out = [0; 255];
        for record in [&[][..], b"opaque", &[0xa4, 1, 6, 2, 2, 3, 1, 5, 0xa0]] {
            assert_eq!(response(record, 42, &mut out), None);
            assert!(!is_announcement(record));
        }
        let announcement = [0xa3, 1, 6, 2, 2, 5, 0xa1, 1, 1];
        assert!(is_announcement(&announcement));
        assert_eq!(response(&announcement, 1, &mut [0; 2]), None);
    }
}

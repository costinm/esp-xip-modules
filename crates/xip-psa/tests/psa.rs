//! Known answers (FIPS 180, RFC 4231) and the identity/agreement round trips.
use xip_psa::*;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn sha2_known_answers_and_multipart() {
    assert_eq!(hash(ALG_SHA_256, b"abc").unwrap(), hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    assert_eq!(&hash(ALG_SHA_512, b"abc").unwrap()[..8], &hex("ddaf35a193617aba")[..]);
    assert_eq!(hash(ALG_SHA_384, b"abc").unwrap().len(), 48);
    assert!(hash(0x0200_0001, b"abc").is_none());
    let mut h = Hasher::new(ALG_SHA_256).unwrap();
    h.update(b"a");
    h.update(b"bc");
    assert_eq!(h.finish(), hash(ALG_SHA_256, b"abc").unwrap());
}

#[test]
fn hmac_sha256_rfc4231_cases() {
    assert_eq!(hmac_sha256(&[0x0b; 20], b"Hi There")[..], hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")[..]);
    // case 6: a key longer than the block
    assert_eq!(
        hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")[..],
        hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")[..]
    );
}

#[test]
fn identity_signature_verifies_for_both_encodings_and_rejects_changes() {
    let public = identity_public_key();
    let sig = identity_sign(b"message");
    assert!(verify_p256_sha256(&public, b"message", &sig));
    assert!(!verify_p256_sha256(&public, b"messagf", &sig));
    let mut bad = sig;
    bad[0] ^= 1;
    assert!(!verify_p256_sha256(&public, b"message", &bad));
    // the compressed form of the same point
    let mut compressed = vec![if public[64] & 1 == 0 { 2 } else { 3 }];
    compressed.extend_from_slice(&public[1..33]);
    assert!(verify_p256_sha256(&compressed, b"message", &sig));
    assert!(!verify_p256_sha256(&[0; 65], b"message", &sig));
    assert!(verify_p256_prehash(&public, &hash(ALG_SHA_256, b"message").unwrap(), &sig));
}

#[test]
fn ecdh_agrees() {
    let (a, b) = (Agreement::generate(), Agreement::generate());
    assert_eq!(a.secret(&b.public()), b.secret(&a.public()));
    assert!(a.secret(&[1, 2, 3]).is_none());
}

#[test]
fn random_bytes_differ() {
    let (mut x, mut y) = ([0u8; 32], [0u8; 32]);
    random(&mut x);
    random(&mut y);
    assert_ne!(x, y);
}

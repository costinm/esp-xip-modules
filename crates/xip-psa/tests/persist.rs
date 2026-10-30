//! Its own test binary: the key is created once per process, so the directory must be set before anything else uses the identity.
use p256::elliptic_curve::sec1::ToEncodedPoint;
use xip_psa::*;

#[test]
fn identity_persists_in_the_state_directory() {
    // a separate process would be needed to restart the OnceLock, so check the file format and reload path directly
    let dir = std::env::temp_dir().join(format!("xip-psa-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    set_identity_dir(&dir).unwrap();
    let public = identity_public_key();
    let raw = std::fs::read(dir.join("identity.p256")).expect("key file");
    assert_eq!(raw.len(), 32);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(dir.join("identity.p256")).unwrap().permissions().mode() & 0o777, 0o600);
    let key = p256::ecdsa::SigningKey::from_slice(&raw).unwrap();
    assert_eq!(p256::ecdsa::VerifyingKey::from(&key).to_encoded_point(false).as_bytes(), &public[..]);
    let _ = std::fs::remove_dir_all(&dir);
}

use crate::upload::{Action, Policy, Session};
use crate::wire::{self, cpu, target, Header, Reject, Transport};
use alloc::vec::Vec;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use std::vec;

struct P {
    key: [u8; 65],
}
impl Policy for P {
    fn cpu(&self) -> u8 {
        cpu::ESP32C6
    }
    fn pinned_key(&self) -> Option<[u8; 65]> {
        Some(self.key)
    }
    fn require_pinned_key(&self) -> bool {
        true
    }
    fn min_version(&self, target: u8, name: &[u8]) -> Result<Option<u32>, Reject> {
        if target == target::MODULE && name != b"test" {
            return Err(wire::WRONG_TARGET);
        }
        Ok(Some(1))
    }
}

fn signed_upload(image: &[u8]) -> (Vec<u8>, P) {
    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    let pubkey: [u8; 65] = key
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .unwrap();
    let header = Header::build(cpu::ESP32C6, target::MODULE, 2, image, b"test");
    let sig: Signature = key.sign(&header);
    let mut bytes = Transport::encode(false, false).to_vec();
    bytes.extend_from_slice(&pubkey);
    bytes.extend_from_slice(&sig.to_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&wire::block_table(image));
    bytes.extend_from_slice(image);
    (bytes, P { key: pubkey })
}

#[test]
fn partial_reads_pause_for_flash_and_commit_before_head() {
    let image: Vec<u8> = (0..(wire::BLOCK * 2 + 17)).map(|n| n as u8).collect();
    let (bytes, policy) = signed_upload(&image);
    let mut session = Session::new(crate::psa_test::psa()).unwrap();
    let mut at = 0;
    let mut writes = Vec::new();
    let mut opened = false;
    while at < bytes.len() {
        let end = (at + 37).min(bytes.len());
        let step = session.feed(&bytes[at..end], &policy).unwrap();
        at += step.consumed;
        match step.action {
            Some(Action::Open {
                target,
                name,
                image_size,
                dry,
            }) => {
                assert_eq!(
                    (target, name, image_size, dry),
                    (
                        target::MODULE,
                        b"test".as_slice(),
                        image.len() as u32,
                        false
                    )
                );
                opened = true;
                session.opened(true).unwrap();
            }
            Some(Action::Write { index, bytes }) => {
                assert!(opened);
                assert_ne!(index, 0);
                writes.push((index, bytes.len()));
                session.written(true).unwrap();
            }
            Some(_) => panic!("unexpected action before EOF"),
            None => assert_eq!(at, end),
        }
    }
    assert_eq!(writes, vec![(1, wire::BLOCK), (2, 17)]);
    match session.eof().unwrap() {
        Action::Commit {
            target,
            name,
            version,
            pubkey,
        } => {
            assert_eq!(
                (target, name, version, pubkey),
                (target::MODULE, b"test".as_slice(), 2, &policy.key)
            );
        }
        _ => panic!("missing commit"),
    }
    match session.committed(true).unwrap() {
        Action::Write { index, bytes } => assert_eq!((index, bytes), (0, &image[..wire::BLOCK])),
        _ => panic!("head must be last"),
    }
    session.written(true).unwrap();
    assert_eq!(session.done(), Some((target::MODULE, image.len() as u32)));
}

#[test]
fn bad_signature_rejected_before_flash_open() {
    let (mut bytes, policy) = signed_upload(&[5; wire::BLOCK]);
    bytes[wire::TRANSPORT_SIZE + 65] ^= 1;
    let mut session = Session::new(crate::psa_test::psa()).unwrap();
    let mut at = 0;
    while at < bytes.len() {
        match session.feed(&bytes[at..], &policy) {
            Ok(step) => {
                assert!(step.action.is_none());
                at += step.consumed;
            }
            Err(err) => {
                assert_eq!(err, wire::BAD_SIGNATURE);
                return;
            }
        }
    }
    panic!("bad signature accepted");
}

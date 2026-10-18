//! The device server and the state log against in-memory stand-ins for the stream, the flash and the
//! platform. Packages are signed here with a throwaway key; `tests/data` also holds packages made by
//! the Python tool and a state area made by the C loader's code (interop).

use alloc::{vec::Vec, string::ToString};
use std::cell::RefCell;
use std::io::{Cursor, Read, Write};
use std::rc::Rc;
use std::{format, string::String, vec};

use crate::device::{Device, Outcome, Platform, Sink};
use crate::state::{self, FlashOps, StateLog};
use crate::wire::{self, cpu, target, Header, Package, Reject, Transport};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

const C6: u8 = cpu::ESP32C6;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).unwrap()
}

fn pubkey(k: &SigningKey) -> [u8; 65] {
    k.verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .unwrap()
}

/// A signed package: public key | signature | header | table | image.
fn package(
    k: &SigningKey,
    cpu: u8,
    target: u8,
    version: u32,
    image: &[u8],
    name: &[u8],
) -> Vec<u8> {
    let header = Header::build(cpu, target, version, image, name);
    let sig: Signature = k.sign(&header);
    let mut p = pubkey(k).to_vec();
    p.extend_from_slice(&sig.to_bytes());
    p.extend_from_slice(&header);
    p.extend_from_slice(&wire::block_table(image));
    p.extend_from_slice(image);
    p
}

/// An ESP image: 0xE9 magic and the chip id (u16) at offset 12 of esp_image_header_t.
fn esp_image(len: usize, chip: u8) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
    v[0] = 0xe9;
    v[12] = chip;
    v[13] = 0;
    v
}

struct Wire {
    input: Cursor<Vec<u8>>,
    out: Vec<u8>,
}

impl Wire {
    fn new(transport: [u8; 16], body: &[u8]) -> Self {
        let mut v = transport.to_vec();
        v.extend_from_slice(body);
        Wire {
            input: Cursor::new(v),
            out: Vec::new(),
        }
    }
    fn reply(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
    fn last(&self) -> String {
        self.reply().lines().last().unwrap_or("").to_string()
    }
}

impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(b) // the end of the input is the client's FIN
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.out.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Events(Vec<String>);

struct RecSink(Rc<RefCell<Events>>);
impl Sink for RecSink {
    fn begin(&mut self, len: u32) -> Result<(), ()> {
        self.0.borrow_mut().0.push(format!("begin {len}"));
        Ok(())
    }
    fn write_block(&mut self, index: u32, bytes: &[u8]) -> Result<(), ()> {
        self.0
            .borrow_mut()
            .0
            .push(format!("block {index} {}", bytes.len()));
        Ok(())
    }
    fn finish(&mut self) -> Result<(), ()> {
        self.0.borrow_mut().0.push("finish".into());
        Ok(())
    }
}

struct Plat {
    chip: u8,
    events: Rc<RefCell<Events>>,
}
impl Platform for Plat {
    type Sink = RecSink;
    fn cpu(&self) -> u8 {
        self.chip
    }
    fn open(&mut self, tgt: u8, name: &[u8], size: u32, _dry: bool) -> Result<RecSink, Reject> {
        let cap = match tgt {
            target::LOADER => 0xde000,
            target::STAGE2 => 0x7000,
            target::MODULE => match name {
                b"lora" => 0x20000,
                b"flash" | b"hw" | b"hello" => 0x10000,
                _ => return Err(wire::WRONG_TARGET),
            },
            _ => return Err(wire::WRONG_TARGET),
        };
        if size as usize > cap {
            return Err(wire::BAD_LENGTH);
        }
        Ok(RecSink(self.events.clone()))
    }
    fn module_slot(&self, name: &[u8]) -> Option<u8> {
        [&b"lora"[..], b"flash", b"hw", b"hello"]
            .iter()
            .position(|n| *n == name)
            .map(|i| i as u8)
    }
}

/// 8 KiB of NOR flash: writes only clear bits.
#[derive(Default)]
struct Mem {
    data: Vec<u8>,
    /// Fail the write that would be number N (power loss), once.
    fail_at: Option<usize>,
    writes: usize,
}
impl Mem {
    fn blank() -> Self {
        Mem {
            data: vec![0xff; state::SIZE as usize],
            ..Default::default()
        }
    }
}
impl FlashOps for Mem {
    fn read(&mut self, off: u32, buf: &mut [u8]) -> bool {
        buf.copy_from_slice(&self.data[off as usize..off as usize + buf.len()]);
        true
    }
    fn write(&mut self, off: u32, data: &[u8]) -> bool {
        assert!(off % 4 == 0 && data.len() % 4 == 0);
        self.writes += 1;
        if self.fail_at == Some(self.writes) {
            for (i, b) in data[..data.len() / 2].iter().enumerate() {
                self.data[off as usize + i] &= b; // torn
            }
            return false;
        }
        for (i, b) in data.iter().enumerate() {
            self.data[off as usize + i] &= b;
        }
        true
    }
    fn erase(&mut self, sector_off: u32) -> bool {
        self.data[sector_off as usize..sector_off as usize + 4096].fill(0xff);
        true
    }
}

fn device() -> (Device<Plat, Mem>, Rc<RefCell<Events>>) {
    let events = Rc::new(RefCell::new(Events::default()));
    (
        Device::new(
            Plat {
                chip: C6,
                events: events.clone(),
            },
            Mem::blank(),
        ),
        events,
    )
}

fn upload(d: &mut Device<Plat, Mem>, pkg: &[u8], dry: bool) -> (Outcome, Wire) {
    let mut w = Wire::new(Transport::encode(dry, false), pkg);
    let o = d.serve(&mut w);
    (o, w)
}

#[test]
fn installs_a_loader_head_last_and_pins_the_key() {
    let (mut d, ev) = device();
    let image = esp_image(10_000, 13);
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 5, &image, b""),
        false,
    );
    assert_eq!(
        o,
        Outcome::Installed {
            target: target::LOADER
        },
        "{}",
        w.reply()
    );
    assert!(w.last().starts_with("OK 10000 "), "{}", w.reply());
    assert_eq!(
        ev.borrow().0,
        [
            "begin 10000",
            "block 1 4096",
            "block 2 1808",
            "block 0 4096",
            "finish"
        ]
    );
    assert_eq!(d.state.view.pin, Some(pubkey(&key(1))));
    assert_eq!(d.state.view.minver_loader, Some(5));
    assert_eq!(d.state.view.minver, None);
}

#[test]
fn dry_run_verifies_and_changes_nothing() {
    let (mut d, ev) = device();
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 5, &esp_image(9000, 13), b""),
        true,
    );
    assert_eq!(o, Outcome::None);
    assert!(w.last().ends_with(" dry"), "{}", w.reply());
    assert!(ev.borrow().0.is_empty());
    assert_eq!(d.state.view.pin, None);
}

#[test]
fn rejects_before_writing() {
    let good = package(&key(1), C6, target::LOADER, 5, &esp_image(9000, 13), b"");
    let flip = |at: usize| {
        let mut p = good.clone();
        p[at] ^= 1;
        p
    };
    let last = good.len() - 1;
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("tampered header", flip(65 + 64 + 12), "ERR 7 "),
        ("tampered table", flip(345), "ERR 12 "),
        ("tampered image", flip(last), "ERR 13 "),
        ("truncated", good[..last - 100].to_vec(), "ERR 6 "),
        (
            "extra bytes",
            [good.clone(), b"x".to_vec()].concat(),
            "ERR 6 ",
        ),
        (
            "not an esp image",
            package(&key(1), C6, target::LOADER, 5, &vec![0u8; 9000], b""),
            "ERR 4 ",
        ),
        (
            "other chip",
            package(
                &key(1),
                cpu::ESP32,
                target::LOADER,
                5,
                &esp_image(9000, 0),
                b"",
            ),
            "ERR 10 ",
        ),
        (
            "main with an image",
            package(&key(1), C6, target::MAIN, 5, &esp_image(9000, 13), b""),
            "ERR 10 ",
        ),
        (
            "too large",
            package(&key(1), C6, target::STAGE2, 5, &esp_image(0x7001, 13), b""),
            "ERR 3 ",
        ),
    ];
    for (name, pkg, want) in cases {
        let (mut d, ev) = device();
        let (o, w) = upload(&mut d, &pkg, false);
        assert_eq!(o, Outcome::None, "{name}");
        assert!(w.last().starts_with(want), "{name}: {}", w.reply());
        assert!(
            !ev.borrow()
                .0
                .iter()
                .any(|e| e.starts_with("block 0") || e == "finish"),
            "{name}: {:?}",
            ev.borrow().0
        );
        assert_eq!(d.state.view.pin, None, "{name}");
    }
}

#[test]
fn transport_header_errors() {
    let (mut d, _) = device();
    let pkg = package(&key(1), C6, target::LOADER, 5, &esp_image(9000, 13), b"");
    let mut t = Transport::encode(true, false);
    t[0] = b'b';
    let mut w = Wire::new(t, &pkg);
    d.serve(&mut w);
    assert!(w.last().starts_with("ERR 1 "));
    let mut t = Transport::encode(true, false);
    t[5] |= 1;
    let mut w = Wire::new(t, &pkg);
    d.serve(&mut w);
    assert!(w.last().starts_with("ERR 2 "));
}

#[test]
fn pins_the_first_key_and_refuses_rollback() {
    let (mut d, _) = device();
    let img = esp_image(9000, 13);
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::LOADER, 5, &img, b""),
            false
        )
        .0,
        Outcome::Installed {
            target: target::LOADER
        }
    );
    let (o, w) = upload(
        &mut d,
        &package(&key(2), C6, target::LOADER, 6, &img, b""),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 8 key-mismatch".to_string())
    );
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 4, &img, b""),
        false,
    );
    assert_eq!((o, w.last()), (Outcome::None, "ERR 9 rollback".to_string()));
    // the same version again is fine, and versions are tracked per target
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::LOADER, 5, &img, b""),
            false
        )
        .0,
        Outcome::Installed {
            target: target::LOADER
        }
    );
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::STAGE2, 1, &img, b""),
            false
        )
        .0,
        Outcome::Installed {
            target: target::STAGE2
        }
    );
}

#[test]
fn zero_length_main_manifest_requests_the_loader() {
    let (mut d, ev) = device();
    let m = package(&key(1), C6, target::MAIN, 9, &[], b"");
    let (o, w) = upload(&mut d, &m, false);
    assert_eq!(
        (o, w.last()),
        (Outcome::RequestLoader, "OK 0 request-loader".to_string())
    );
    let (o, w) = upload(&mut d, &m, true);
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "OK 0 request-loader dry".to_string())
    );
    assert!(ev.borrow().0.is_empty());
    // a wrong key once one is pinned
    upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 1, &esp_image(9000, 13), b""),
        false,
    );
    let (o, w) = upload(
        &mut d,
        &package(&key(2), C6, target::MAIN, 9, &[], b""),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 8 key-mismatch".to_string())
    );
}

#[test]
fn an_unsigned_reboot_request_is_refused() {
    let (mut d, _) = device();
    let mut w = Wire::new(Transport::encode(false, true), &[]);
    assert_eq!(d.serve(&mut w), Outcome::None);
    assert!(w.last().starts_with("ERR 2 "), "{}", w.reply());
}

#[test]
fn a_required_pinned_key_blocks_trust_on_first_use() {
    let events = Rc::new(RefCell::new(Events::default()));
    let mut d = Device::new(
        Plat {
            chip: C6,
            events: events.clone(),
        },
        Mem::blank(),
    )
    .require_pinned_key();
    let img = esp_image(9000, 13);
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 5, &img, b""),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 8 no-pinned-key".to_string())
    );
    assert!(events.borrow().0.is_empty());
    // once a key is pinned (here: by an earlier install on a device without the requirement), only it works
    let mut free = Device::new(Plat { chip: C6, events }, Mem::blank());
    upload(
        &mut free,
        &package(&key(1), C6, target::LOADER, 5, &img, b""),
        false,
    );
    let mem = Mem {
        data: free.state.flash_snapshot(),
        ..Default::default()
    };
    let mut d = Device::new(
        Plat {
            chip: C6,
            events: Rc::new(RefCell::new(Events::default())),
        },
        mem,
    )
    .require_pinned_key();
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::LOADER, 6, &img, b""),
            false
        )
        .0,
        Outcome::Installed {
            target: target::LOADER
        }
    );
    let (o, w) = upload(
        &mut d,
        &package(&key(2), C6, target::LOADER, 7, &img, b""),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 8 key-mismatch".to_string())
    );
}

#[test]
fn a_failed_state_write_fails_the_install_before_the_head_is_written() {
    // Cut the power at each of the first writes of the state log while an install saves its key and version.
    // (On a blank log saving the key takes three writes and the version one more.)
    for cut in 1..=4 {
        let (mut d, ev) = device();
        d.state.ops().fail_at = Some(cut);
        let (o, w) = upload(
            &mut d,
            &package(&key(1), C6, target::LOADER, 5, &esp_image(9000, 13), b""),
            false,
        );
        assert_eq!(o, Outcome::None, "cut {cut}: {}", w.reply());
        assert!(
            w.last().starts_with("ERR 5 state-write"),
            "cut {cut}: {}",
            w.reply()
        );
        let e = ev.borrow();
        assert!(
            !e.0.iter()
                .any(|x| x.starts_with("block 0") || x == "finish"),
            "cut {cut}: {:?}",
            e.0
        );
    }
}

#[test]
fn modules_go_to_named_slots_and_may_be_small() {
    let (mut d, ev) = device();
    let module = vec![0x42u8; 100]; // no ESP image header, shorter than one block
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::MODULE, 1, &module, b"hello"),
        false,
    );
    assert_eq!(
        o,
        Outcome::Installed {
            target: target::MODULE
        },
        "{}",
        w.reply()
    );
    assert_eq!(ev.borrow().0, ["begin 100", "block 0 100", "finish"]);
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::MODULE, 1, &module, b"nosuch"),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 10 wrong-chip-or-target".to_string())
    );
    // module versions are tracked per slot
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::MODULE, 4, &module, b"lora"),
            false
        )
        .0,
        Outcome::Installed {
            target: target::MODULE
        }
    );
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::MODULE, 3, &module, b"lora"),
        false,
    );
    assert_eq!((o, w.last()), (Outcome::None, "ERR 9 rollback".to_string()));
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::MODULE, 1, &module, b"hw"),
            false
        )
        .0,
        Outcome::Installed {
            target: target::MODULE
        }
    );
}

#[test]
fn an_interrupted_install_never_writes_the_head() {
    let (mut d, ev) = device();
    let pkg = package(&key(1), C6, target::LOADER, 5, &esp_image(20_000, 13), b"");
    let (o, w) = upload(&mut d, &pkg[..pkg.len() - 6000], false);
    assert_eq!(o, Outcome::None);
    assert!(w.last().starts_with("ERR 6 "));
    let e = ev.borrow();
    assert!(e.0.iter().any(|x| x.starts_with("block 1")));
    assert!(
        !e.0.iter()
            .any(|x| x.starts_with("block 0") || x == "finish"),
        "{:?}",
        e.0
    );
    assert_eq!(d.state.view.pin, None);
}

#[test]
fn python_tool_packages_verify_and_install() {
    // Signed packages made once by the frozen loader tools (see docs/development.md, "Formats"): the format must stay byte compatible.
    let loader = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/loader-esp32c6.pkg"
    ))
    .unwrap();
    let p = Package::parse(&loader).unwrap();
    assert_eq!(p.verify(), None);
    assert_eq!(
        (p.header.target, p.header.cpu, p.header.version),
        (target::LOADER, C6, 9)
    );
    let (mut d, _) = device();
    assert_eq!(
        upload(&mut d, &loader, false).0,
        Outcome::Installed {
            target: target::LOADER
        }
    );
    let module = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/module-lora-esp32c6.pkg"
    ))
    .unwrap();
    let p = Package::parse(&module).unwrap();
    assert_eq!((p.verify(), p.header.name()), (None, &b"lora"[..]));
    assert_eq!(
        upload(&mut d, &module, false).0,
        Outcome::Installed {
            target: target::MODULE
        }
    );
}

#[test]
fn state_log_reads_what_the_c_loader_wrote() {
    let mut mem = Mem::blank();
    mem.data = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/state.bin"
    ))
    .unwrap();
    let log = StateLog::open(mem);
    let v = &log.view;
    assert_eq!(v.pin.unwrap()[..3], [4, 1, 2]);
    assert_eq!(
        (v.minver, v.minver_loader, v.minver_stage2, v.qio),
        (Some(7), Some(33), Some(44), 1)
    );
    assert_eq!(v.header.unwrap()[..2], [0x80, 0x81]);
}

#[test]
fn state_log_survives_compaction_and_power_loss() {
    let mut log = StateLog::open(Mem::blank());
    assert!(log.append(state::REC_PIN, &[4u8; 65]));
    assert!(log.append_u32(state::REC_MINVER_LOADER, 1));
    for v in 2..400u32 {
        assert!(log.append_u32(state::REC_MINVER_LOADER, v));
        assert!(log.append(state::REC_HEADER, &[v as u8; 216]));
    }
    let mem = std::mem::take(log.ops());
    let mut log = StateLog::open(mem);
    assert_eq!(log.view.minver_loader, Some(399));
    assert!(log.view.pin.is_some());
    // cut the power at each write position of one more append
    for cut in 1..8 {
        let mut copy = Mem {
            data: log.flash_snapshot(),
            ..Default::default()
        };
        copy.fail_at = Some(cut);
        let mut l = StateLog::open(copy);
        let _ = l.append_u32(state::REC_MINVER_LOADER, 1000);
        let mem = std::mem::take(l.ops());
        let mut again = StateLog::open(Mem {
            data: mem.data,
            ..Default::default()
        });
        assert!(again.view.pin.is_some(), "cut {cut}");
        assert!(
            matches!(again.view.minver_loader, Some(399) | Some(1000)),
            "cut {cut}"
        );
        assert!(
            again.append_u32(state::REC_MINVER_LOADER, 1001),
            "cut {cut}"
        ); // heals a torn slot
        assert_eq!(
            StateLog::open(std::mem::take(again.ops()))
                .view
                .minver_loader,
            Some(1001)
        );
    }
}

#[test]
fn power_loss_during_compaction_keeps_one_readable_copy() {
    let mut log = StateLog::open(Mem::blank());
    assert!(log.append(state::REC_PIN, &[4u8; 65]));
    assert!(log.append_module_version(2, 9));
    assert!(log.append_u32(state::REC_MINVER_LOADER, 7));
    // fill the active sector until the next 216-byte record no longer fits
    let mut filler = 1u8;
    while log.fits(216) {
        assert!(log.append(state::REC_HEADER, &[filler; 216]));
        filler = filler.wrapping_add(1);
    }
    let snapshot = log.flash_snapshot();
    for cut in 1..16 {
        let mut l = StateLog::open(Mem {
            data: snapshot.clone(),
            fail_at: Some(cut),
            ..Default::default()
        });
        let _ = l.append(state::REC_HEADER, &[0xee; 216]); // compacts into the other sector
        let mut again = StateLog::open(Mem {
            data: std::mem::take(&mut l.ops().data),
            ..Default::default()
        });
        assert!(again.view.pin.is_some(), "cut {cut}");
        assert_eq!(
            (again.view.minver_loader, again.view.minver_module[2]),
            (Some(7), Some(9)),
            "cut {cut}"
        );
        assert!(again.append_u32(state::REC_MINVER_LOADER, 8), "cut {cut}"); // heals a torn sector
        assert_eq!(
            StateLog::open(std::mem::take(again.ops()))
                .view
                .minver_loader,
            Some(8),
            "cut {cut}"
        );
    }
}

#[test]
fn a_real_idf_image_header_is_accepted_and_the_wrong_chip_refused() {
    // The first 16 bytes of an unmodified ESP-IDF image for the ESP32-C6 (the loader build): wp_pin 0xEE at
    // offset 8, chip id 0x000d at offset 12.
    let real = [
        0xe9, 0x05, 0x02, 0x20, 0x16, 0x06, 0x80, 0x40, 0xee, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00,
        0x00,
    ];
    let mut img: Vec<u8> = (0..9000).map(|i| (i * 31 + 7) as u8).collect();
    img[..16].copy_from_slice(&real);
    let (mut d, _) = device();
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::LOADER, 5, &img, b""),
            false
        )
        .0,
        Outcome::Installed {
            target: target::LOADER
        }
    );
    img[12] = 0x09; // an ESP32-S3 image sent to a C6
    let (o, w) = upload(
        &mut d,
        &package(&key(1), C6, target::LOADER, 6, &img, b""),
        false,
    );
    assert_eq!(
        (o, w.last()),
        (Outcome::None, "ERR 4 bad-image-header".to_string())
    );
}

#[test]
fn one_key_signs_everything_the_loader_updates() {
    let events = Rc::new(RefCell::new(Events::default()));
    let mut d = Device::new(Plat { chip: C6, events }, Mem::blank());
    let img = esp_image(9000, 13);
    let module = vec![7u8; 100];
    // the first key seen is pinned, here by a module, and is the only one after that
    assert_eq!(
        upload(
            &mut d,
            &package(&key(2), C6, target::MODULE, 1, &module, b"hello"),
            false
        )
        .0,
        Outcome::Installed {
            target: target::MODULE
        }
    );
    assert_eq!(d.state.view.pin, Some(pubkey(&key(2))));
    // the pinned key also signs the loader and Stage2
    for t in [target::LOADER, target::STAGE2] {
        assert_eq!(
            upload(&mut d, &package(&key(2), C6, t, 5, &img, b""), false).0,
            Outcome::Installed { target: t }
        );
    }
    assert_eq!(
        upload(
            &mut d,
            &package(&key(2), C6, target::MAIN, 1, &[], b""),
            false
        )
        .0,
        Outcome::RequestLoader
    );
    // another key is refused for every target
    for (t, image) in [
        (target::LOADER, &img[..]),
        (target::STAGE2, &img[..]),
        (target::MODULE, &module[..]),
    ] {
        let name: &[u8] = if t == target::MODULE { b"hello" } else { b"" };
        let (o, w) = upload(&mut d, &package(&key(1), C6, t, 9, image, name), false);
        assert_eq!(
            (o, w.last()),
            (Outcome::None, "ERR 8 key-mismatch".to_string()),
            "target {t}"
        );
    }
    assert_eq!(
        upload(
            &mut d,
            &package(&key(1), C6, target::MAIN, 1, &[], b""),
            false
        )
        .0,
        Outcome::None
    );
    assert_eq!(d.state.view.pin, Some(pubkey(&key(2))));
}

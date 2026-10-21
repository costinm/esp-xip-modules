//! The state log (the loader's journal) against an in-memory flash: a state area written by the C loader's code is read back, compaction and
//! power loss keep one readable copy, and packages made by the frozen loader tools verify (`tests/data`). Compatibility tests: the formats must not change.

use alloc::vec::Vec;
use std::vec;

use crate::state::{self, FlashOps, StateLog};
use crate::wire::{cpu, target, Package};

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
fn signed_packages_made_by_the_loader_tools_parse_and_verify() {
    // Made once by the frozen loader tools (docs/development.md, "Formats"): the package format must stay byte compatible.
    let loader = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/loader-esp32c6.pkg")).unwrap();
    let p = Package::parse(&loader).unwrap();
    assert_eq!(p.verify(), None);
    assert_eq!((p.header.target, p.header.cpu, p.header.version), (target::LOADER, cpu::ESP32C6, 9));
    let module = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/module-lora-esp32c6.pkg")).unwrap();
    let p = Package::parse(&module).unwrap();
    assert_eq!((p.verify(), p.header.name()), (None, &b"lora"[..]));
    assert_eq!(p.header.target, target::MODULE);
}

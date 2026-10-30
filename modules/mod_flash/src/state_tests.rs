//! The state log (the loader's journal) against an in-memory flash: a state area written by the C loader's code is read back, compaction and
//! power loss keep one readable copy, and packages made by the frozen loader tools verify (`tests/data` in this module). Compatibility tests: the formats must not change.

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
        "/tests/data/state.bin"
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
            (again.view.minver_loader, again.view.module_floor(2)),
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
    let loader = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/loader-esp32c6.pkg")).unwrap();
    let p = Package::parse(&loader).unwrap();
    assert_eq!(p.verify(), None);
    assert_eq!((p.header.target, p.header.cpu, p.header.version), (target::LOADER, cpu::ESP32C6, 9));
    let module = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/module-lora-esp32c6.pkg")).unwrap();
    let p = Package::parse(&module).unwrap();
    assert_eq!((p.verify(), p.header.name()), (None, &b"lora"[..]));
    assert_eq!(p.header.target, target::MODULE);
}

#[test]
fn all_page_floors_survive_compaction_and_interruption_without_format_change() {
    let mut log = StateLog::open(Mem::blank());
    for page in 0..256 {assert!(log.append_module_version(page as u8,page as u32+7));}
    assert!(log.append_module_version(255,u32::MAX));
    while log.fits(216) {assert!(log.append(state::REC_HEADER,&[42;216]));}
    let snapshot=log.flash_snapshot();
    for cut in [1,2,4,64,128,255,260,265] {
        let mut l=StateLog::open(Mem {data:snapshot.clone(),fail_at:Some(cut),..Default::default()});
        let _=l.append(state::REC_HEADER,&[43;216]);
        let mut healed=StateLog::open(Mem {data:std::mem::take(&mut l.ops().data),..Default::default()});
        for page in 0..256 {assert_eq!(healed.view.module_floor(page),Some(if page==255{u32::MAX}else{page as u32+7}),"page {page}, cut {cut}");}
        assert!(healed.append_u32(state::REC_MINVER_LOADER,123));
    }
}

#[test]
fn expanded_page_journal_compacts_between_c_and_rust_readers() {
    use std::{fs,process::Command};
    let root=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir=std::env::temp_dir().join(std::format!("xip-journal-{}",std::process::id()));fs::create_dir_all(&dir).unwrap();
    let source=dir.join("journal.c");
    fs::write(&source,r#"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "esp_wifi_loader_state.h"
static unsigned char bytes[8192];
static int read_mem(void*c,uint32_t at,void*out,size_t n){(void)c;memcpy(out,bytes+at,n);return 0;}
static int write_mem(void*c,uint32_t at,const void*in,size_t n){(void)c;for(size_t i=0;i<n;i++)bytes[at+i]&=((const unsigned char*)in)[i];return 0;}
static int erase_mem(void*c,uint32_t at){(void)c;memset(bytes+at,255,4096);return 0;}
int main(int argc,char**argv){
 assert(argc==3);wl_flash_ops_t ops={0,read_mem,write_mem,erase_mem};wl_state_t state;
 if(!strcmp(argv[1],"write")){
  memset(bytes,255,sizeof bytes);wl_state_open(&state,&ops);
  for(unsigned i=0;i<256;i++){unsigned char p[5]={(unsigned char)i};uint32_t v=i+7;memcpy(p+1,&v,4);assert(!wl_state_append(&state,WL_REC_MINVER_MODULE,p,5));}
 }else{
  FILE*f=fopen(argv[2],"rb");assert(f);assert(fread(bytes,1,sizeof bytes,f)==sizeof bytes);fclose(f);wl_state_open(&state,&ops);
  for(unsigned i=0;i<256;i++){assert(wl_state_has_module(&state.view,i));assert(state.view.minver_module[i]==(i==255?UINT32_MAX:i+7));}
 }
 unsigned char h[WL_HEADER_SIZE]={42};for(int i=0;i<40;i++)assert(!wl_state_append(&state,WL_REC_HEADER,h,sizeof h));
 FILE*f=fopen(argv[2],"wb");assert(f);assert(fwrite(bytes,1,sizeof bytes,f)==sizeof bytes);fclose(f);return 0;
}
"#).unwrap();
    let bin=dir.join("journal");
    let status=Command::new("cc").args(["-std=c11","-Wall","-Wextra","-Werror","-I"]).arg(root.join("boot/bootloader_components/main"))
        .arg(&source).arg(root.join("boot/bootloader_components/main/state_core.c")).arg("-o").arg(&bin).status().unwrap();assert!(status.success());
    let image=dir.join("state.bin");assert!(Command::new(&bin).arg("write").arg(&image).status().unwrap().success());
    let mut log=StateLog::open(Mem {data:fs::read(&image).unwrap(),..Default::default()});
    for page in 0..256 {assert_eq!(log.view.module_floor(page),Some(page as u32+7));}
    assert!(log.append_module_version(255,u32::MAX));for _ in 0..40 {assert!(log.append(state::REC_HEADER,&[99;216]));}
    fs::write(&image,log.flash_snapshot()).unwrap();assert!(Command::new(&bin).arg("read").arg(&image).status().unwrap().success());
    let log=StateLog::open(Mem {data:fs::read(&image).unwrap(),..Default::default()});assert_eq!(log.view.module_floor(255),Some(u32::MAX));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn journal_view_stack_budget_covers_all_256_pages(){
    assert!(core::mem::size_of::<state::View>()<=1400, "journal view copies must fit the 12 KiB shared worker");
    let mut view=state::View::default();
    // Frozen journal decoding must distinguish absent, zero and max versions.
    let mut log=StateLog::open(Mem::blank());
    assert_eq!(view.module_floor(0),None);
    assert!(log.append_module_version(0,0));assert!(log.append_module_version(255,u32::MAX));
    view=log.view.clone();assert_eq!(view.module_floor(0),Some(0));assert_eq!(view.module_floor(255),Some(u32::MAX));
}

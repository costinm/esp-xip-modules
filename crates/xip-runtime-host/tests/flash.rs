//! Persistent physical flash ABI, including deferred completion and ownership.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use xip_abi::*;
use xip_runtime_host::{flash::FlashFile, Runtime};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "xip-flash-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
unsafe extern "C" fn handler(_: *const Host, s: *mut u8, c: *const Call) -> i32 {
    if (*c).component == 44 && (*c).method == 101 {
        *(s as *mut u64) = (*c).id;
        *(s.add(8) as *mut i32) = (*c).value;
    }
    0
}
#[test]
fn file_is_erased_once_reopens_and_enforces_nor_and_sector_cuts() {
    let p = path();
    {
        let mut f = FlashFile::open(&p, 4 * 1024 * 1024).unwrap();
        let mut b = [0; 16];
        f.read(0, &mut b).unwrap();
        assert_eq!(b, [255; 16]);
        f.program(4096, &[1, 2, 3, 4]).unwrap();
        assert_eq!(f.program(4096, &[255; 4]), Err(ERR_ARG));
        f.cut_after(Some(1));
        assert_eq!(f.erase(0, 8192), Err(ERR_IO));
    }
    {
        let mut f = FlashFile::open(&p, 4 * 1024 * 1024).unwrap();
        let mut b = [0; 4];
        f.read(4096, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4]);
        f.erase(4096, 4096).unwrap();
        f.read(4096, &mut b).unwrap();
        assert_eq!(b, [255; 4]);
    }
    assert!(FlashFile::open(&p, 8192).is_err());
    std::fs::remove_file(p).unwrap();
}
#[test]
fn abi_defers_completion_owns_exact_packet_and_stop_cancels() {
    let p = path();
    let rt = Runtime::default();
    rt.attach_flash(FlashFile::open(&p, 4 * 1024 * 1024).unwrap())
        .unwrap();
    let m = rt.add_handler("flash-test", handler, 32);
    m.set_component(44);
    m.start(&[]);
    rt.run(0);
    let hw = unsafe { Hw::new(m.host()) }.unwrap();
    let free = rt.pool_available();
    let pkt = hw.pkt_alloc().unwrap();
    let (ptr, _) = hw.pkt_buf(pkt).unwrap();
    unsafe {
        ptr.copy_from_nonoverlapping([1u8, 2, 3, 4].as_ptr(), 4);
    }
    assert_eq!(
        hw.flash_write(0x10000, 0, pkt, PKT_HEADROOM, 4, 44, 101, 123),
        0
    );
    assert_eq!(hw.flash_erase(0x10000, 0, 4096, 44, 101, 124), ERR_BUSY);
    assert_eq!(rt.pool_available(), free - 1);
    rt.run(0);
    assert_eq!(rt.pool_available(), free);
    let mut b = [0; 4];
    assert_eq!(hw.flash_read(0x10000, 0, &mut b), 4);
    assert_eq!(b, [1, 2, 3, 4]);
    assert_eq!(hw.flash_erase(u32::MAX, 4, 4096, 44, 101, 0), ERR_ARG);
    // Stop is queued first; a subsequently submitted job must not callback after stop.
    m.stop();
    assert_eq!(hw.flash_erase(0x10000, 0, 4096, 44, 101, 125), 0);
    rt.run(0);
    assert!(m.ended());
    assert_eq!(rt.pool_available(), free);
    drop((m, rt));
    std::fs::remove_file(p).unwrap();
}

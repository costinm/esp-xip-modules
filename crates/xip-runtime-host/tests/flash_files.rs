//! Real mod_flash Wasm, module streams, physical flash ABI, persistent 4 MiB file.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use xip_abi::*;
use xip_runtime_host::{flash::FlashFile, Instance, Runtime};
static NEXT: AtomicU64 = AtomicU64::new(0);
unsafe extern "C" fn provider(_: *const Host, _: *mut u8, _: *const Call) -> i32 {
    0
}
fn image() -> PathBuf {
    std::env::var_os("XIP_HOST_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules"))
        .join("mod_flash.wasm")
}
fn fields(name: &[u8], size: Option<usize>) -> Vec<u8> {
    let mut b = [0; 64];
    let mut w = cbor::Writer::new(&mut b);
    w.map(if size.is_some() { 2 } else { 1 })
        .uint(1)
        .bytes(name);
    if let Some(n) = size {
        w.uint(2).uint(n as u64);
    }
    w.finish().unwrap().to_vec()
}
struct Device {
    rt: Runtime,
    m: Instance,
    p: Instance,
}
impl Device {
    fn open(path: &Path) -> Self {
        Self::open_cut(path, None)
    }
    fn open_cut(path: &Path, cut: Option<u64>) -> Self {
        Self::open_pinned(path, cut, None)
    }
    fn open_pinned(path: &Path, cut: Option<u64>, page: Option<u64>) -> Self {
        let rt = Runtime::default();
        if let Some(page) = page {
            let mut b = [0; 64];
            let mut w = cbor::Writer::new(&mut b);
            w.array(1)
                .map(4)
                .uint(1)
                .text("flash")
                .uint(2)
                .uint(44)
                .uint(4)
                .boolean(true)
                .uint(12)
                .uint(page);
            rt.state_mut().modules_cbor = w.finish().unwrap().to_vec();
        }
        rt.attach_flash(
            FlashFile::open(path, std::fs::metadata(path).unwrap().len() as u32).unwrap(),
        )
        .unwrap();
        rt.flash_cut_after(cut);
        rt.state_mut().running_flash = Some((0x10000, 0x180000));
        let m = rt.load_module(image()).unwrap();
        m.start(&[]);
        rt.run(30000);
        assert!(!m.ended(), "{:?}", m.logs());
        let p = rt.add_handler("provider", provider, 16);
        p.set_component(99);
        p.start(&[]);
        rt.run(0);
        Self { rt, m, p }
    }
    fn interrupt_image(&self, method: i32, fields: &[u8], body: &[u8]) {
        let hw = unsafe { Hw::new(self.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let mut b = [0; 128];
        let mut w = cbor::Writer::new(&mut b);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .int(method as i64)
            .uint(5)
            .raw(fields);
        let b = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) };
        assert!(id > 0);
        let mut at = 0;
        for _ in 0..1000 {
            let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, body[at..].as_ptr(), 997, 0) };
            assert!(rc >= 0);
            at += rc as usize;
            self.rt.run(100);
            if at >= 4000 {
                break;
            }
        }
        assert!(at >= 4000);
        assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
        self.rt.run(100);
    }
    fn call(&self, method: i32, fields: &[u8], body: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let hw = unsafe { Hw::new(self.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let mut e = [0; 256];
        let mut w = cbor::Writer::new(&mut e);
        w.map(4)
            .uint(1)
            .uint(44)
            .uint(2)
            .int(method as i64)
            .uint(3)
            .uint(9)
            .uint(5)
            .raw(fields);
        let e = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), e.as_ptr(), e.len()) };
        assert!(id > 0, "open {method}: {id}, {:?}", self.m.logs());
        let mut at = 0;
        let mut attempts = 0;
        while at < body.len() {
            let n = (body.len() - at).min(chunk);
            let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, body[at..].as_ptr(), n, 0) };
            assert!(rc >= 0, "feed {method}: {rc}");
            at += rc as usize;
            self.rt.run(100);
            attempts += 1;
            assert!(attempts < 100000, "stalled at {at}");
        }
        assert_eq!(
            unsafe { t.feed.unwrap()(hw.ctx(), id as u32, std::ptr::null(), 0, 1) },
            0
        );
        let mut out = Vec::new();
        for _ in 0..10000 {
            self.rt.run(100);
            let mut p = StreamPacket::default();
            assert_eq!(unsafe { t.take.unwrap()(hw.ctx(), id as u32, &mut p) }, 0);
            if p.packet != 0 {
                let (ptr, _) = hw.pkt_buf(p.packet).unwrap();
                out.push(unsafe { std::slice::from_raw_parts(ptr, p.len as usize) }.to_vec());
                hw.pkt_free(p.packet);
                assert_eq!(unsafe { t.written.unwrap()(hw.ctx(), id as u32) }, 0);
                if p.fin != 0 {
                    assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
                    return out;
                }
            }
        }
        panic!("no response, {:?}", self.m.logs());
    }
}
fn fresh() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "mod-flash-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = FlashFile::open(&p, 4 * 1024 * 1024).unwrap();
    let mut table = [255; 64];
    table[..2].copy_from_slice(&[0xaa, 0x50]);
    table[2] = 0;
    table[3] = 0;
    table[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
    table[8..12].copy_from_slice(&0x3f0000u32.to_le_bytes());
    table[12..16].copy_from_slice(b"xip\0");
    f.program(0x8000, &table).unwrap();
    p
}
#[test]
fn streamed_files_survive_fresh_runtime_replace_remove_and_unaligned_chunks() {
    let p = fresh();
    let bytes: Vec<u8> = (0..65504).map(|i| (i * 17) as u8).collect();
    {
        let d = Device::open(&p);
        let out = d.call(4, &fields(b"example", Some(bytes.len())), &bytes, 997);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(1));
        assert_eq!(d.rt.pool_available(), 8);
    }
    {
        let d = Device::open(&p);
        let out = d.call(3, &fields(b"example", None), &[], 1);
        assert_eq!(out[1..].concat(), bytes);
        let replacement = vec![0x23; 17003];
        let out = d.call(
            4,
            &fields(b"example", Some(replacement.len())),
            &replacement,
            3,
        );
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(2));
    }
    {
        let d = Device::open(&p);
        let out = d.call(3, &fields(b"example", None), &[], 1);
        assert_eq!(out[1..].concat(), vec![0x23; 17003]);
        d.call(5, &fields(b"example", None), &[], 1);
    }
    {
        let d = Device::open(&p);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let mut e = [0; 128];
        let mut w = cbor::Writer::new(&mut e);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(2)
            .uint(5)
            .raw(&fields(b"example", None));
        let e = w.finish().unwrap();
        assert_eq!(
            unsafe { t.open.unwrap()(hw.ctx(), e.as_ptr(), e.len()) },
            ERR_NOTFOUND
        );
    }
    std::fs::remove_file(p).unwrap();
}
fn module_fields(size: usize, id: u64) -> Vec<u8> {
    let mut b = [0; 100];
    let mut w = cbor::Writer::new(&mut b);
    w.map(4)
        .uint(1)
        .bytes(b"sample")
        .uint(2)
        .uint(size as u64)
        .uint(8)
        .uint(id)
        .uint(9)
        .uint(17);
    w.finish().unwrap().to_vec()
}
fn module_bytes() -> Vec<u8> {
    let mut bytes = vec![0x37; 100004];
    bytes[..64].fill(0);
    bytes[0] = b'M';
    bytes[1] = 4;
    bytes[2..4].copy_from_slice(&64u16.to_le_bytes());
    let size = bytes.len() as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    let mut b = [0; 48];
    let mut w = cbor::Writer::new(&mut b);
    w.map(2).uint(1).text("sample").uint(8).uint(17);
    let manifest = w.finish().unwrap();
    bytes[44..48].copy_from_slice(&64u32.to_le_bytes());
    bytes[48..52].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
    bytes[64..64 + manifest.len()].copy_from_slice(manifest);
    bytes
}
#[test]
fn interrupted_module_is_reclaimed_after_reset_and_completed_retry_uses_identity() {
    let path = fresh();
    let bytes = module_bytes();
    let fields = module_fields(bytes.len(), 123456);
    {
        let d = Device::open(&path);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let mut e = [0; 256];
        let mut w = cbor::Writer::new(&mut e);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(13)
            .uint(5)
            .raw(&fields);
        let e = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), e.as_ptr(), e.len()) };
        assert!(id > 0, "{id} {:?}", d.m.logs());
        let mut at = 0;
        for _ in 0..1000 {
            let n = 1000.min(bytes.len() - at);
            let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, bytes[at..].as_ptr(), n, 0) };
            assert!(rc >= 0, "{rc}");
            at += rc as usize;
            d.rt.run(100);
            if at >= 4500 {
                break;
            }
        }
        assert!(at >= 4500);
        assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
        d.rt.run(0);
    }
    let before = std::fs::read(&path).unwrap();
    let start = (0x10000..before.len())
        .step_by(65536)
        .find(|at| before[*at] == 0x4f)
        .unwrap();
    {
        let d = Device::open(&path);
        let swept = std::fs::read(&path).unwrap();
        assert!(swept[start..start + bytes.len().div_ceil(65536) * 65536]
            .iter()
            .all(|v| *v == 255));
        let out = d.call(13, &fields, &bytes, 997);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    let after = std::fs::read(&path).unwrap();
    let installed = (0x10000..after.len())
        .step_by(65536)
        .find(|at| after[*at] == b'M')
        .unwrap();
    assert_eq!(&after[installed..installed + bytes.len()], &bytes);
    {
        let d = Device::open(&path);
        let out = d.call(13, &module_fields(bytes.len(), 123457), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        after,
        "completed retry must never erase the installed module"
    );
    std::fs::remove_file(path).unwrap();
}
fn log_fields(size: usize, first: u64, count: u16) -> Vec<u8> {
    let mut b = [0; 64];
    let mut w = cbor::Writer::new(&mut b);
    w.map(3)
        .uint(2)
        .uint(size as u64)
        .uint(10)
        .uint(first)
        .uint(11)
        .uint(count as u64);
    w.finish().unwrap().to_vec()
}
fn seq_field(seq: u64) -> Vec<u8> {
    let mut b = [0; 32];
    let mut w = cbor::Writer::new(&mut b);
    w.map(1).uint(10).uint(seq);
    w.finish().unwrap().to_vec()
}
#[test]
fn logs_stream_in_batches_acknowledge_without_erasing_named_files_and_collect() {
    let path = fresh();
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"keep", Some(4003)), &vec![0x51; 4003], 311);
        d.call(4, &fields(b"keep", Some(5001)), &vec![0x52; 5001], 233);
        let a = vec![0xf5; 3001];
        let b = vec![0x01; 3013];
        d.call(10, &log_fields(a.len(), 1, 3001), &a, 997);
        d.call(10, &log_fields(b.len(), 3002, 3013), &b, 3);
        let out = d.call(11, &seq_field(0), &[], 1);
        let mut i = 0;
        for (first, data) in [(1, a), (3002, b)] {
            assert_eq!(cbor::get_int(&out[i], 1).unwrap(), Some(first));
            let n = data.len().div_ceil(PKT_MAX as usize);
            assert_eq!(out[i + 1..i + 1 + n].concat(), data);
            i += 1 + n;
        }
        assert_eq!(out[i], [0xa0]);
        d.call(12, &seq_field(6014), &[], 1);
        assert_eq!(d.call(11, &seq_field(0), &[], 1), vec![vec![0xa0]]);
        let out = d.call(3, &fields(b"keep", None), &[], 1);
        assert_eq!(out[1..].concat(), vec![0x52; 5001]);
        d.call(9, &[0xa0], &[], 1); // First victim may be an emptied log page; a second call compacts named data.
        d.call(9, &[0xa0], &[], 1);
    }
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"keep", None), &[], 1);
        assert_eq!(out[1..].concat(), vec![0x52; 5001]);
        let list = d.call(6, &[0xa0], &[], 1);
        assert!(list[0].windows(4).any(|v| v == b"keep"));
    }
    std::fs::remove_file(path).unwrap();
}
fn runtime_bytes() -> Vec<u8> {
    runtime_bytes_with_data(0x20000)
}
fn runtime_bytes_with_data(data_len: usize) -> Vec<u8> {
    let end = (32 + data_len | 15) + 1;
    let mut b = vec![0x35; end + 32];
    b[..32].fill(0);
    b[0] = 0xe9;
    b[1] = 1;
    b[12] = 13;
    b[23] = 1;
    b[28..32].copy_from_slice(&(data_len as u32).to_le_bytes());
    b[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
    b[48..80].fill(0);
    b[48..50].copy_from_slice(b"17");
    let sum = b[32..32 + data_len].iter().fold(0xef, |a, v| a ^ v);
    b[end - 1] = sum;
    let hash = xip_psa::hash(xip_psa::ALG_SHA_256, &b[..end]).unwrap();
    b[end..].copy_from_slice(&hash);
    b
}
fn runtime_fields(size: usize, id: u64) -> Vec<u8> {
    runtime_fields_version(size, id, 17)
}
fn runtime_fields_version(size: usize, id: u64, version: u64) -> Vec<u8> {
    let mut b = [0; 100];
    let mut w = cbor::Writer::new(&mut b);
    w.map(3)
        .uint(2)
        .uint(size as u64)
        .uint(8)
        .uint(id)
        .uint(9)
        .uint(version);
    w.finish().unwrap().to_vec()
}
#[test]
fn runtime_stream_verifies_native_checksum_and_sha_and_bad_image_stays_uncommitted() {
    let path = fresh();
    let bytes = runtime_bytes();
    {
        let d = Device::open(&path);
        let out = d.call(14, &runtime_fields(bytes.len(), 9988), &bytes, 3);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    let before = std::fs::read(&path).unwrap();
    {
        let d = Device::open(&path);
        let out = d.call(14, &runtime_fields(bytes.len(), 9990), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    {
        let d = Device::open(&path);
        let mut bad = bytes.clone();
        bad[1000] ^= 1;
        bad[49] = b'8'; // A different version must undergo native validation.
        let out = d.call(14, &runtime_fields_version(bad.len(), 9989, 18), &bad, 1021);
        assert_eq!(cbor::get_int(&out[0], 7).unwrap(), Some(ERR_ARG as i64));
    }
    std::fs::remove_file(path).unwrap();
}
#[test]
fn usb_front_cleanup_crossing_page_boundary_is_repeatable_and_keeps_back_data() {
    let path = fresh();
    let mut image = runtime_bytes()[..65536].to_vec();
    let data_len = 65456usize;
    image[28..32].copy_from_slice(&(data_len as u32).to_le_bytes());
    image[65503] = image[32..32 + data_len].iter().fold(0xef, |a, v| a ^ v);
    let hash = xip_psa::hash(xip_psa::ALG_SHA_256, &image[..65504]).unwrap();
    image[65504..].copy_from_slice(&hash);
    // Exact page boundary: the marker owns the first word of the next page.
    {
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        f.program(0x10000, &image).unwrap();
        f.program(0x10000 + image.len() as u32, &[0xfc, 255, 255, 255])
            .unwrap();
        f.program(0x10000 + 0x170000, &[0; 16]).unwrap();
    }
    {
        let d = Device::open(&path);
        d.rt.run(5000);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let stream = hw.stream().unwrap();
        let mut request = [0; 64];
        let mut w = cbor::Writer::new(&mut request);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(15)
            .uint(5)
            .map(1)
            .uint(6)
            .uint(0);
        let request = w.finish().unwrap();
        assert_eq!(
            unsafe { stream.open.unwrap()(hw.ctx(), request.as_ptr(), request.len()) },
            ERR_BUSY
        );
        d.call(4, &fields(b"back", Some(7)), b"persist", 3);
    }
    let after = std::fs::read(&path).unwrap();
    assert!(after[0x10000 + 0x170000..0x10000 + 0x170010]
        .iter()
        .all(|v| *v == 255));
    assert_eq!(after[0x10000 + image.len()], 0xf8);
    assert_ne!(
        &after[0x10000 + image.len()..0x10000 + image.len() + 4],
        &[255; 4]
    );
    {
        let d = Device::open(&path);
        d.rt.run(5000);
        let out = d.call(3, &fields(b"back", None), &[], 1);
        assert_eq!(out[1..].concat(), b"persist");
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        after,
        "cleaned front must not be erased on the next startup"
    );
    std::fs::remove_file(path).unwrap();
}
#[test]
fn image_remove_reclaims_installed_module() {
    let path = fresh();
    let bytes = module_bytes();
    {
        let d = Device::open(&path);
        let out = d.call(13, &module_fields(bytes.len(), 771), &bytes, 777);
        let start = cbor::get_int(&out[0], 6).unwrap().unwrap() as u32;
        let mut b = [0; 32];
        let mut w = cbor::Writer::new(&mut b);
        w.map(1).uint(6).uint(start as u64);
        let fields = w.finish().unwrap();
        d.call(15, fields, &[], 1);
        let data = std::fs::read(&path).unwrap();
        assert!(
            data[0x10000 + start as usize..0x10000 + start as usize + 0x20000]
                .iter()
                .all(|b| *b == 255)
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn interrupted_file_replacement_keeps_previous_version_after_runtime_restart() {
    let path = fresh();
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"atomic", Some(5)), b"prior", 3);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let request = fields(b"atomic", Some(40001));
        let mut b = [0; 128];
        let mut w = cbor::Writer::new(&mut b);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(4)
            .uint(5)
            .raw(&request);
        let b = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) };
        assert!(id > 0);
        let chunk = [0x73; 997];
        let mut received = 0;
        for _ in 0..1000 {
            let n = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, chunk.as_ptr(), chunk.len(), 0) };
            assert!(n >= 0);
            received += n;
            d.rt.run(100);
            if received >= 5000 {
                break;
            }
        }
        assert!(received >= 5000);
        assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
        d.rt.run(0);
    }
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"atomic", None), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(1));
        assert_eq!(out[1..].concat(), b"prior");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn file_commit_power_cut_sweep_returns_only_complete_old_or_new_content() {
    let base = fresh();
    {
        let d = Device::open(&base);
        d.call(4, &fields(b"cut", Some(5)), b"prior", 3);
    }
    let replacement = vec![0x23; 5003];
    for cut in 0..14 {
        let path = std::env::temp_dir().join(format!("flash-cut-{}-{cut}.bin", std::process::id()));
        std::fs::copy(&base, &path).unwrap();
        {
            let d = Device::open(&path);
            d.rt.flash_cut_after(Some(cut));
            let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
            let t = hw.stream().unwrap();
            let request = fields(b"cut", Some(replacement.len()));
            let mut b = [0; 128];
            let mut w = cbor::Writer::new(&mut b);
            w.map(3)
                .uint(1)
                .uint(44)
                .uint(2)
                .uint(4)
                .uint(5)
                .raw(&request);
            let b = w.finish().unwrap();
            let id = unsafe { t.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) };
            assert!(id > 0);
            let mut at = 0;
            for _ in 0..100 {
                if at == replacement.len() {
                    break;
                }
                let n = 997.min(replacement.len() - at);
                let rc = unsafe {
                    t.feed.unwrap()(hw.ctx(), id as u32, replacement[at..].as_ptr(), n, 0)
                };
                if rc < 0 {
                    break;
                }
                at += rc as usize;
                d.rt.run(100);
            }
            if at == replacement.len() {
                let _ = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, std::ptr::null(), 0, 1) };
                d.rt.run(5000);
            }
            let _ = unsafe { t.close.unwrap()(hw.ctx(), id as u32) };
            d.rt.run(0);
        }
        {
            let d = Device::open(&path);
            let out = d.call(3, &fields(b"cut", None), &[], 1);
            let content = out[1..].concat();
            assert!(
                content == b"prior" || content == replacement,
                "cut {cut}: exposed partial data"
            );
        }
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_file(base).unwrap();
}

#[test]
fn python_reference_builds_rust_reads_and_python_reads_rust_writes() {
    let path = std::env::temp_dir().join(format!(
        "xip-python-{}-{}.bin",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let content = path.with_extension("input");
    let extracted = path.with_extension("output");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/xipfs.py");
    std::fs::write(&content, b"python fixture").unwrap();
    for args in [
        vec!["init".into(), path.clone().into_os_string()],
        vec![
            "put".into(),
            path.clone().into_os_string(),
            "fixture".into(),
            content.clone().into_os_string(),
        ],
    ] {
        let out = std::process::Command::new("python3")
            .arg(&script)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"fixture", None), &[], 1);
        assert_eq!(out[1..].concat(), b"python fixture");
        d.call(4, &fields(b"fixture", Some(10003)), &vec![0x2a; 10003], 37);
    }
    let out = std::process::Command::new("python3")
        .arg(&script)
        .arg("read")
        .arg(&path)
        .arg("fixture")
        .arg(&extracted)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&extracted).unwrap(), vec![0x2a; 10003]);
    for p in [path, content, extracted] {
        std::fs::remove_file(p).unwrap();
    }
}

#[test]
fn startup_reclaims_abandoned_image_without_erasing_named_data() {
    let path = fresh();
    let bytes = module_bytes();
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"keep", Some(4)), b"safe", 3);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let fields = module_fields(bytes.len(), 8181);
        let mut b = [0; 128];
        let mut w = cbor::Writer::new(&mut b);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(13)
            .uint(5)
            .raw(&fields);
        let b = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) };
        assert!(id > 0);
        let mut at = 0;
        for _ in 0..1000 {
            let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, bytes[at..].as_ptr(), 997, 0) };
            assert!(rc >= 0);
            at += rc as usize;
            d.rt.run(100);
            if at > 3000 {
                break;
            }
        }
        assert!(at > 3000);
        assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
        d.rt.run(0);
    }
    let data = std::fs::read(&path).unwrap();
    let start = (0x10000..data.len())
        .step_by(65536)
        .find(|at| data[*at] == 0x4f)
        .unwrap();
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"keep", None), &[], 1);
        assert_eq!(out[1..].concat(), b"safe");
    }
    let data = std::fs::read(&path).unwrap();
    assert!(data[start..start + 0x20000].iter().all(|v| *v == 255));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn startup_reclaims_dirty_pages_and_rejects_short_module_headers() {
    let path = fresh();
    let dirty = 0x10000 + 0x190000;
    let short = dirty + 0x10000;
    {
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        f.program(dirty, &[0x35; 32]).unwrap();
        // A separate orphan has an erased first word but dirty later sectors.
        f.program(short + 0x10000 + 0x1100, &[0; 16]).unwrap();
        for (at, kind) in [(short + 0x20000, 0x4f), (short + 0x40000, 0)] {
            let mut header = [255; 16];
            header[0] = kind;
            header[4..8].copy_from_slice(&65540u32.to_le_bytes());
            f.program(at, &header).unwrap();
            f.program(at + 65536, &[0x35; 16]).unwrap();
        }
        let mut h = [255; 64];
        h[0] = b'M';
        h[4..8].copy_from_slice(&64u32.to_le_bytes());
        f.program(short, &h).unwrap();
    }
    {
        let d = Device::open(&path);
        d.call(7, &[0xa0], &[], 1);
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes[dirty as usize..short as usize + 0x60000]
        .iter()
        .all(|b| *b == 255));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn exhausted_allocator_reads_scale_linearly_through_sixteen_megabytes() {
    for mb in [4u32, 8, 16] {
        let path = std::env::temp_dir().join(format!("flash-scan-{}-{mb}.bin", std::process::id()));
        let size = mb * 1024 * 1024;
        {
            let mut f = FlashFile::open(&path, size).unwrap();
            let mut table = [255; 64];
            table[..2].copy_from_slice(&[0xaa, 0x50]);
            table[2] = 0;
            table[3] = 0;
            table[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
            table[8..12].copy_from_slice(&(size - 0x10000).to_le_bytes());
            table[12..16].copy_from_slice(b"xip\0");
            f.program(0x8000, &table).unwrap();
            for page in (0x190000..size).step_by(65536) {
                let mut header = [255; 20];
                header[0] = b'D';
                header[4..8].copy_from_slice(&65536u32.to_le_bytes());
                header[16] = 0;
                f.program(page, &header).unwrap();
            }
        }
        {
            let d = Device::open(&path);
            let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
            let stream = hw.stream().unwrap();
            let request = fields(b"full", Some(8));
            let mut b = [0; 128];
            let mut w = cbor::Writer::new(&mut b);
            w.map(3)
                .uint(1)
                .uint(44)
                .uint(2)
                .uint(4)
                .uint(5)
                .raw(&request);
            let b = w.finish().unwrap();
            let before = d.rt.flash_read_count().unwrap();
            assert_eq!(
                unsafe { stream.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) },
                -20
            );
            let reads = d.rt.flash_read_count().unwrap() - before;
            let pages = size / 65536;
            assert!(
                reads < 80 * pages as u64,
                "{mb} MiB: {reads} flash reads for {pages} pages"
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn collection_is_busy_until_the_writer_releases_its_stream() {
    let path = fresh();
    {
        let d = Device::open(&path);
        let hw = unsafe { Hw::new(d.p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let request = fields(b"writer", Some(5003));
        let mut b = [0; 128];
        let mut w = cbor::Writer::new(&mut b);
        w.map(3)
            .uint(1)
            .uint(44)
            .uint(2)
            .uint(4)
            .uint(5)
            .raw(&request);
        let b = w.finish().unwrap();
        let id = unsafe { t.open.unwrap()(hw.ctx(), b.as_ptr(), b.len()) };
        assert!(id > 0);
        d.rt.run(2000);
        let chunk = [0x33; 997];
        let n = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, chunk.as_ptr(), chunk.len(), 0) };
        assert!(n > 0);
        d.rt.run(100);
        let before = std::fs::read(&path).unwrap();
        let gc = [0xa3, 1, 0x18, 44, 2, 9, 5, 0xa0];
        assert_eq!(
            unsafe { t.open.unwrap()(hw.ctx(), gc.as_ptr(), gc.len()) },
            ERR_BUSY
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, chunk.as_ptr(), chunk.len(), 0) };
        assert!(rc > 0, "writer was invalidated by rejected collection");
        d.rt.run(100);
        assert_eq!(unsafe { t.close.unwrap()(hw.ctx(), id as u32) }, 0);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn point_lookup_uses_index_without_scanning_unrelated_record_bodies() {
    let path = fresh();
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"aaa", Some(1)), b"x", 1);
        d.call(4, &fields(b"zzz", Some(3)), b"yes", 3);
    }
    {
        let data = std::fs::read(&path).unwrap();
        let at = data.windows(8).position(|b| b == b"aaa\0\0\0\0\0").unwrap() - 4;
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        let mut header: [u8; 4] = data[at..at + 4].try_into().unwrap();
        header[0] &= 0xf0;
        f.program(at as u32, &header).unwrap();
    }
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"zzz", None), &[], 1);
        assert_eq!(out[1..].concat(), b"yes");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn back_runtime_may_exceed_front_reserve() {
    let path = fresh();
    let bytes = runtime_bytes_with_data(0x190000);
    {
        let d = Device::open(&path);
        let out = d.call(14, &runtime_fields(bytes.len(), 0x190001), &bytes, 1500);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    {
        let d = Device::open(&path);
        let out = d.call(14, &runtime_fields(bytes.len(), 0x190001), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn boot_marker_formatting_recovers_from_power_cuts_and_distinguishes_fresh_from_recovery() {
    for marker in [0xfe, 0xfc] {
        let base = fresh();
        {
            let d = Device::open(&base);
            d.call(4, &fields(b"keep", Some(7)), b"persist", 7);
        }
        let image = runtime_bytes();
        let marker_at = 0x10000 + image.len();
        {
            let mut f = FlashFile::open(&base, 4 * 1024 * 1024).unwrap();
            f.program(0x10000, &image).unwrap();
            f.program(marker_at as u32, &[marker, 255, 255, 255])
                .unwrap();
            f.program(0x10000 + 0x170000, &[0; 16]).unwrap();
        }
        // Erase is sector-by-sector. Cut before, during, and after front cleanup,
        // and near completion of the fresh-install back-region formatting.
        for cut in [0, 1, 100, 350, 700, 980, 1100] {
            let path = base.with_extension(format!("{marker}-{cut}.bin"));
            std::fs::copy(&base, &path).unwrap();
            {
                let _ = Device::open_cut(&path, Some(cut));
            }
            let interrupted = std::fs::read(&path).unwrap();
            assert!(matches!(interrupted[marker_at], 0xfe | 0xfc | 0xf8));
            if interrupted[marker_at] == 0xf8 {
                assert!(interrupted[0x180000..0x180010].iter().all(|b| *b == 255));
            }
            {
                let d = Device::open(&path);
                if marker == 0xfc {
                    let out = d.call(3, &fields(b"keep", None), &[], 1);
                    assert_eq!(out[1..].concat(), b"persist");
                } else {
                    let out = d.call(6, &[0xa0], &[], 1);
                    assert!(!out.concat().windows(4).any(|w| w == b"keep"));
                }
            }
            let done = std::fs::read(&path).unwrap();
            assert_eq!(done[marker_at], 0xf8);
            assert_eq!(&done[0x10000..marker_at], image);
            assert_eq!(
                &done[..0x10000],
                &interrupted[..0x10000],
                "NVS/table preserved"
            );
            {
                let _ = Device::open(&path);
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                done,
                "completed formatting repeats no writes"
            );
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(base).unwrap();
    }
}

#[test]
fn installing_second_runtime_clears_stop_marker_and_retry_is_idempotent() {
    let path = fresh();
    let image = runtime_bytes();
    let marker_at = 0x10000 + image.len();
    {
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        f.program(0x10000, &image).unwrap();
        f.program(marker_at as u32, &[0xf8, 255, 255, 255]).unwrap();
    }
    {
        let d = Device::open(&path);
        let mut bad = image.clone();
        bad[1000] ^= 1;
        let out = d.call(14, &runtime_fields(bad.len(), 9910), &bad, 997);
        assert_eq!(cbor::get_int(&out[0], 7).unwrap(), Some(ERR_ARG as i64));
        assert_eq!(std::fs::read(&path).unwrap()[marker_at], 0xf8);
        let out = d.call(14, &runtime_fields(image.len(), 9910), &image, 997);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
        assert_eq!(std::fs::read(&path).unwrap()[marker_at], 0x78);
    }
    let done = std::fs::read(&path).unwrap();
    {
        let d = Device::open(&path);
        let out = d.call(14, &runtime_fields(image.len(), 9910), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
    }
    assert_eq!(std::fs::read(&path).unwrap(), done);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn streamed_front_runtime_has_completed_marker_and_enables_existing_second_image() {
    for second in [false, true] {
        let path = fresh();
        let image = runtime_bytes();
        if second {
            let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
            f.program(0x3c0000, &image).unwrap();
        }
        {
            let d = Device::open(&path);
            d.rt.state_mut().running_flash = Some((0x3c0000, 0x30000));
            let mut b = [0; 64];
            let mut w = cbor::Writer::new(&mut b);
            w.map(4)
                .uint(2)
                .uint(image.len() as u64)
                .uint(8)
                .uint(9911)
                .uint(9)
                .uint(17)
                .uint(12)
                .boolean(true);
            let out = d.call(14, w.finish().unwrap(), &image, 997);
            assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
        }
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0x10000..0x10000 + image.len()], image);
        assert_eq!(
            data[0x10000 + image.len()],
            if second { 0x78 } else { 0xf8 }
        );
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn initial_format_preserves_all_pages_of_a_mapped_module() {
    let path = fresh();
    let image = runtime_bytes();
    let installed = module_bytes();
    assert!(installed.len() > 65536);
    {
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        f.program(0x10000, &image).unwrap();
        f.program(0x10000 + image.len() as u32, &[0xfe, 255, 255, 255])
            .unwrap();
        f.program(0x1a0000, &installed).unwrap();
    }
    {
        let _ = Device::open_pinned(&path, None, Some(0x1a));
    }
    let after = std::fs::read(&path).unwrap();
    assert_eq!(after[0x10000 + image.len()], 0xf8);
    assert_eq!(&after[0x1a0000..0x1a0000 + installed.len()], installed);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn initial_format_never_erases_the_executing_back_runtime() {
    let image_path = image();
    let path = fresh();
    let image = runtime_bytes();
    {
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        f.program(0x10000, &image).unwrap();
        f.program(0x10000 + image.len() as u32, &[0xfe, 255, 255, 255])
            .unwrap();
        f.program(0x3c0000, &image).unwrap();
    }
    {
        let rt = Runtime::default();
        rt.attach_flash(FlashFile::open(&path, 4 * 1024 * 1024).unwrap())
            .unwrap();
        rt.state_mut().running_flash = Some((0x3c0000, 0x30000));
        let m = rt.load_module(image_path).unwrap();
        m.start(&[]);
        rt.run(30000);
        assert!(!m.ended(), "{:?}", m.logs());
    }
    let after = std::fs::read(&path).unwrap();
    assert_eq!(after[0x10000 + image.len()], 0xf8);
    assert_eq!(&after[0x3c0000..0x3c0000 + image.len()], image);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn interrupted_upload_reuses_ram_allocation_and_excludes_it_from_other_writes() {
    let path = fresh();
    let bytes = module_bytes();
    {
        let d = Device::open(&path);
        let before = d.call(8, &[0xa0], &[], 1);
        let free = cbor::get_int(&before[0], 1).unwrap().unwrap();
        d.interrupt_image(13, &module_fields(bytes.len(), 9191), &bytes);
        let pending = std::fs::read(&path).unwrap();
        let start = (0x10000..pending.len())
            .step_by(65536)
            .find(|p| pending[*p] == 0x4f)
            .unwrap();
        let info = d.call(8, &[0xa0], &[], 1);
        assert_eq!(cbor::get_int(&info[0], 1).unwrap(), Some(free - 2));
        assert_eq!(cbor::get_int(&info[0], 2).unwrap(), Some(0));
        d.call(4, &fields(b"other", Some(7)), b"persist", 3);
        let out = d.call(13, &module_fields(bytes.len(), 9191), &bytes, 997);
        assert_eq!(
            cbor::get_int(&out[0], 6).unwrap(),
            Some((start - 0x10000) as i64)
        );
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(17));
        let installed = std::fs::read(&path).unwrap();
        assert_eq!(&installed[start..start + bytes.len()], bytes);
        assert_eq!(
            d.call(3, &fields(b"other", None), &[], 1)[1..].concat(),
            b"persist"
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn interrupted_runtime_is_reclaimed_without_operation_records() {
    let path = fresh();
    let bytes = runtime_bytes();
    let start;
    {
        let d = Device::open(&path);
        d.interrupt_image(14, &runtime_fields(bytes.len(), 9292), &bytes);
        let pending = std::fs::read(&path).unwrap();
        start = (0x190000..pending.len())
            .step_by(65536)
            .find(|p| pending[*p] == 0xe9)
            .unwrap();
    }
    {
        let _ = Device::open(&path);
        let swept = std::fs::read(&path).unwrap();
        assert!(swept[start..start + bytes.len().div_ceil(65536) * 65536]
            .iter()
            .all(|v| *v == 255));
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn recreated_file_is_not_hidden_by_deleted_higher_index_versions() {
    let path=fresh();
    {
        let d=Device::open(&path);
        d.call(4,&fields(b"recreate",Some(3)),b"old",3);
        let replaced=d.call(4,&fields(b"recreate",Some(3)),b"two",3);
        assert_eq!(cbor::get_int(&replaced[0],1).unwrap(),Some(2));
        d.call(5,&fields(b"recreate",None),&[],1);
        let created=d.call(4,&fields(b"recreate",Some(3)),b"new",3);
        assert_eq!(cbor::get_int(&created[0],1).unwrap(),Some(1));
        let read=d.call(3,&fields(b"recreate",None),&[],1);
        assert_eq!(read[1..].concat(),b"new");
    }
    {
        let d=Device::open(&path);
        let read=d.call(3,&fields(b"recreate",None),&[],1);
        assert_eq!(read[1..].concat(),b"new");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn collection_reclaims_eleven_deleted_pages_and_keeps_live_file() {
    let path = fresh();
    let retained = vec![0x47; 60001];
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"keep", Some(retained.len())), &retained, 997);
        for value in 0..11 {
            d.call(4, &fields(b"dead", Some(60001)), &vec![value; 60001], 997);
            d.call(5, &fields(b"dead", None), &[], 1);
        }
        let before = d.call(8, &[0xa0], &[], 1);
        let before = cbor::get_int(&before[0], 1).unwrap().unwrap();
        for count in 1..=11 {
            d.call(9, &[0xa0], &[], 1);
            let after = d.call(8, &[0xa0], &[], 1);
            assert_eq!(cbor::get_int(&after[0], 1).unwrap(), Some(before + count),
                "each collection must return one deleted file page to the allocator");
        }
    }
    {
        let d = Device::open(&path);
        let out = d.call(3, &fields(b"keep", None), &[], 1);
        assert_eq!(out[1..].concat(), retained);
        let stat = d.call(2, &fields(b"keep", None), &[], 1);
        assert_eq!(cbor::get_int(&stat[0], 1).unwrap(), Some(1));
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn tcp_collection_reclaims_deleted_large_file_page() {
    use std::{net::{IpAddr, Ipv4Addr}, time::Duration};
    use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream};
    let path = fresh();
    {
        let d = Device::open(&path);
        d.call(4, &fields(b"dead", Some(60001)), &vec![0x31; 60001], 997);
        d.call(5, &fields(b"dead", None), &[], 1);
        let before = d.call(8, &[0xa0], &[], 1);
        let ports = d.m.listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).await.unwrap();
        let client = async {
            let mut socket = TcpStream::connect(ports.stream.unwrap()).await.unwrap();
            socket.write_all(&[0xa4, 1, 0x18, 44, 2, 9, 3, 1, 5, 0xa0]).await.unwrap();
            // dmesh-cli uses the declared zero body length, without a half-close.
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            assert_eq!(cbor::get_int(&response, 4).unwrap(), Some(1));
        };
        tokio::select! {
            _ = d.rt.run_realtime(Duration::from_secs(45)) => panic!("TCP collect timed out; ended={} logs={:?} data_pages={:?}", d.m.ended(), d.m.logs(), data_pages(&path)),
            _ = client => {}
        }
        let after = d.call(8, &[0xa0], &[], 1);
        assert_eq!(cbor::get_int(&after[0], 1).unwrap(),
            cbor::get_int(&before[0], 1).unwrap().map(|n| n + 1),
            "TCP collection must complete the erase before replying");
    }
    std::fs::remove_file(path).unwrap();
}

fn collect_request(bytes: u32, full: bool, erase: bool) -> Vec<u8> {
    let mut b = [0; 32];
    let mut w = cbor::Writer::new(&mut b);
    w.map(3).uint(2).uint(bytes as u64).uint(3).boolean(full).uint(4).boolean(erase);
    w.finish().unwrap().to_vec()
}

#[test]
fn collection_makes_the_requested_contiguous_run() {
    let path = fresh();
    {
        let d = Device::open(&path);
        let keep = vec![0x47; 60001];
        d.call(4, &fields(b"keep", Some(keep.len())), &keep, 997);
        for value in 0..11 {
            d.call(4, &fields(b"dead", Some(60001)), &vec![value; 60001], 997);
            d.call(5, &fields(b"dead", None), &[], 1);
        }
        let wanted = 37 * 65536;
        let out = d.call(9, &collect_request(wanted, false, true), &[], 1);
        assert!(cbor::get_int(&out[0], 3).unwrap().unwrap() >= wanted as i64);
        assert_eq!(cbor::get_int(&out[0], 4).unwrap(), Some(11));
        assert_eq!(d.call(3, &fields(b"keep", None), &[], 1)[1..].concat(), keep);
        let again = d.call(9, &collect_request(wanted, false, true), &[], 1);
        assert_eq!(cbor::get_int(&again[0], 4).unwrap(), Some(0));
    }
    std::fs::remove_file(path).unwrap();
}

fn fragmented_small_files(d: &Device) {
    for i in 0..5 {
        let name = format!("keep{i}");
        d.call(4, &fields(name.as_bytes(), Some(1000)), &vec![i as u8; 1000], 997);
        d.call(4, &fields(b"padding", Some(63000)), &vec![0x52; 63000], 997);
        d.call(5, &fields(b"padding", None), &[], 1);
    }
}
fn check_small_files(d: &Device) {
    for i in 0..5 {
        let name = format!("keep{i}");
        let out = d.call(3, &fields(name.as_bytes(), None), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 1).unwrap(), Some(1));
        assert_eq!(out[1..].concat(), vec![i as u8; 1000]);
    }
}
fn data_pages(path: &Path) -> Vec<usize> {
    let bytes = std::fs::read(path).unwrap();
    (0x190000..bytes.len()).step_by(65536)
        .filter(|at| bytes[*at] == b'D').collect()
}

#[test]
fn full_collection_packs_live_files_and_does_not_revisit_output() {
    let path = fresh();
    {
        let d = Device::open(&path);
        fragmented_small_files(&d);
        assert!(data_pages(&path).len() > 1);
        let out = d.call(9, &collect_request(0, true, true), &[], 1);
        assert!(cbor::get_int(&out[0], 4).unwrap().unwrap() <= 5);
        assert_eq!(data_pages(&path).len(), 1);
        check_small_files(&d);
    }
    check_small_files(&Device::open(&path));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn full_collection_can_defer_erasing_then_erase_retired_pages() {
    let path = fresh();
    {
        let d = Device::open(&path);
        fragmented_small_files(&d);
        let sources = data_pages(&path);
        let out = d.call(9, &collect_request(0, true, false), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 5).unwrap(), Some(0));
        assert_eq!(cbor::get_int(&out[0], 2).unwrap(), Some(sources.len() as i64));
        let bytes = std::fs::read(&path).unwrap();
        for at in sources {
            assert_eq!(bytes[at], 0);
            assert!(bytes[at + 4..at + 65536].iter().any(|b| *b != 255));
        }
        check_small_files(&d);
        let out = d.call(9, &collect_request(0, true, true), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 2).unwrap(), Some(0));
        assert!(cbor::get_int(&out[0], 5).unwrap().unwrap() > 0);
        check_small_files(&d);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn impossible_collection_target_reports_nospace_and_keeps_data() {
    let path = fresh();
    {
        let d = Device::open(&path);
        let keep = vec![0x47; 60001];
        d.call(4, &fields(b"keep", Some(keep.len())), &keep, 997);
        let out = d.call(9, &collect_request(39 * 65536, false, true), &[], 1);
        assert_eq!(cbor::get_int(&out[0], 7).unwrap(), Some(-20));
        assert_eq!(d.call(3, &fields(b"keep", None), &[], 1)[1..].concat(), keep);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn image_inventory_pages_report_actual_sizes_and_do_not_change_flash() {
    let path = fresh();
    {
        let d = Device::open(&path);
        let module = module_bytes();
        let upload = d.call(13, &module_fields(module.len(), 331122), &module, 997);
        assert_eq!(cbor::get_int(&upload[0], 1).unwrap(), Some(17));
        let offset = cbor::get_int(&upload[0], 6).unwrap().unwrap() as u32;
        let mut slots = [0; 80];
        let mut w = cbor::Writer::new(&mut slots);
        w.array(1).map(4).uint(1).text("sample").uint(2).uint(55)
            .uint(4).boolean(true).uint(12).uint(((0x10000 + offset) / 65536) as u64);
        d.rt.state_mut().modules_cbor = w.finish().unwrap().to_vec();
        let snapshot = std::fs::read(&path).unwrap();
        let listed = d.call(23, &[0xa0], &[], 1);
        assert_eq!(listed.len(), 1);
        let mut entries = cbor::get(&listed[0], 1).unwrap().unwrap();
        assert!(matches!(entries.next(), Some(cbor::Item::Array(1))));
        let item = entries.rest();
        assert_eq!(cbor::get_int(item, 3).unwrap(), Some(17));
        assert_eq!(cbor::get_int(item, 4).unwrap(), Some(module.len() as i64));
        assert_eq!(cbor::get_int(item, 5).unwrap(), Some(131072));
        assert!(matches!(cbor::get(item, 7).unwrap().unwrap().next(), Some(cbor::Item::Bool(true))));
        let offset = cbor::get_int(item, 6).unwrap().unwrap();
        let cursor = cbor::get_int(&listed[0], 2).unwrap().unwrap();
        assert_eq!(cursor, offset + 131072);
        let mut b = [0; 16];
        let mut w = cbor::Writer::new(&mut b);w.map(1).uint(6).uint(cursor as u64);
        let end = d.call(23, w.finish().unwrap(), &[], 1);
        assert_eq!(cbor::get_int(&end[0], 2).unwrap(), Some(0));
        assert_eq!(std::fs::read(&path).unwrap(), snapshot, "inventory is strictly read-only");
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn file_supplied_versions_and_automatic_increments_are_monotonic() {
    let path=fresh();
    {
        let d=Device::open(&path);
        let versioned=|name:&[u8],size:u32,version:u32|{
            let mut b=[0;64];let mut w=cbor::Writer::new(&mut b);
            w.map(3).uint(1).bytes(name).uint(2).uint(size as u64).uint(9).uint(version as u64);w.finish().unwrap().to_vec()
        };
        let out=d.call(4,&versioned(b"epoch",3,42),b"one",3);assert_eq!(cbor::get_int(&out[0],1).unwrap(),Some(42));
        let out=d.call(4,&fields(b"epoch",Some(3)),b"two",3);assert_eq!(cbor::get_int(&out[0],1).unwrap(),Some(43));
        let out=d.call(4,&versioned(b"epoch",3,900),b"new",3);assert_eq!(cbor::get_int(&out[0],1).unwrap(),Some(900));
        let rejected=|fields:&[u8]| {
            let hw=unsafe{Hw::new(d.p.host())}.unwrap();let stream=hw.stream().unwrap();
            let mut b=[0;128];let mut w=cbor::Writer::new(&mut b);w.map(3).uint(1).uint(44).uint(2).uint(4).uint(5).raw(fields);let b=w.finish().unwrap();
            unsafe{stream.open.unwrap()(hw.ctx(),b.as_ptr(),b.len())}
        };
        let snapshot=std::fs::read(&path).unwrap();
        for version in [0,43,900] {
            assert_eq!(rejected(&versioned(b"epoch",0,version)),if version==0{ERR_ARG}else{-21});
            assert_eq!(std::fs::read(&path).unwrap(),snapshot,"rejected epochs cannot allocate or change flash");
        }
        let out=d.call(4,&versioned(b"max",1,u32::MAX),b"x",1);assert_eq!(cbor::get_int(&out[0],1).unwrap(),Some(u32::MAX as i64));
        let snapshot=std::fs::read(&path).unwrap();assert_eq!(rejected(&fields(b"max",Some(0))),ERR_SIZE);assert_eq!(std::fs::read(&path).unwrap(),snapshot);
    }
    std::fs::remove_file(path).unwrap();
}

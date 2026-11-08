//! Production flash Wasm and production C interpreters, using direct module calls.
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use xip_abi::*;
use xip_runtime_host::{flash::FlashFile, Config, Instance, Runtime};
static NEXT: AtomicU64 = AtomicU64::new(1);
unsafe extern "C" fn provider(host: *const Host, _: *mut u8, c: *const Call) -> i32 {
    if (*c).pkt != 0 {
        Hw::new(host).unwrap().pkt_free((*c).pkt);
    }
    0
}
fn encoded(f: impl FnOnce(&mut cbor::Writer)) -> Vec<u8> {
    let mut b = [0; 1500];
    let mut w = cbor::Writer::new(&mut b);
    f(&mut w);
    w.finish().unwrap().to_vec()
}
struct Device {
    rt: Runtime,
    flash: Instance,
    path: PathBuf,
}
impl Device {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "direct-files-{}-{}.bin",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
        let mut table = [255; 64];
        table[..2].copy_from_slice(&[0xaa, 0x50]);
        table[2] = 0;
        table[3] = 0;
        table[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
        table[8..12].copy_from_slice(&0x3f0000u32.to_le_bytes());
        table[12..16].copy_from_slice(b"xip\0");
        f.program(0x8000, &table).unwrap();
        let rt = Runtime::new(Config {
            mem_quota: 512 * 1024,
            ..Default::default()
        });
        rt.attach_flash(f).unwrap();
        rt.state_mut().running_flash = Some((0x10000, 0x180000));
        let image = std::env::var_os("XIP_HOST_MODULES")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules")
            })
            .join("mod_flash.wasm");
        let flash = rt.load_module(image).unwrap();
        flash.start(&[]);
        rt.run(30000);
        assert!(!flash.ended(), "{:?}", flash.logs());
        Self { rt, flash, path }
    }
    fn answer(&self, id: u64, client: &Instance) -> Vec<u8> {
        for _ in 0..310 {
            self.rt.run(100);
            let mut s = self.rt.state_mut();
            if let Some(i) = s.main.iter().position(|m| m.id == id) {
                return s.main.remove(i).data;
            }
        }
        panic!(
            "no reply: flash {:?}, client {:?}, main {:?}",
            self.flash.logs(),
            client.logs(),
            self.rt
                .state_mut()
                .main
                .iter()
                .map(|m| (m.from, m.component, m.method, m.id, m.data.clone()))
                .collect::<Vec<_>>()
        )
    }
    fn call(&self, m: &Instance, component: i32, method: i32, data: &[u8]) -> Vec<u8> {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert_eq!(m.send(component, method, id, data), 0);
        self.answer(id, m)
    }
    fn files(&self, method: i32, data: &[u8]) -> Vec<u8> {
        self.call(&self.flash, FILES_COMPONENT as i32, method, data)
    }
    fn put(&self, name: &[u8], data: &[u8]) -> Vec<u8> {
        self.files(
            18,
            &encoded(|w| {
                w.map(2).uint(1).bytes(name).uint(13).bytes(data);
            }),
        )
    }
    fn stream_put(&self, name: &[u8], data: &[u8]) {
        let p = self.rt.add_handler("writer", provider, 16);
        p.set_component(98);
        p.start(&[]);
        self.rt.run(0);
        let hw = unsafe { Hw::new(p.host()) }.unwrap();
        let t = hw.stream().unwrap();
        let e = encoded(|w| {
            w.map(3)
                .uint(1)
                .uint(44)
                .uint(2)
                .uint(4)
                .uint(5)
                .map(2)
                .uint(1)
                .bytes(name)
                .uint(2)
                .uint(data.len() as u64);
        });
        let id = unsafe { t.open.unwrap()(hw.ctx(), e.as_ptr(), e.len()) };
        assert!(id > 0);
        let mut at = 0;
        for _ in 0..10000 {
            if at == data.len() {
                break;
            }
            let n = 997.min(data.len() - at);
            let rc = unsafe { t.feed.unwrap()(hw.ctx(), id as u32, data[at..].as_ptr(), n, 0) };
            assert!(rc >= 0);
            at += rc as usize;
            self.rt.run(100);
        }
        assert_eq!(at, data.len());
        assert_eq!(
            unsafe { t.feed.unwrap()(hw.ctx(), id as u32, std::ptr::null(), 0, 1) },
            0
        );
        for _ in 0..500 {
            self.rt.run(100);
            let mut out = StreamPacket::default();
            assert_eq!(unsafe { t.take.unwrap()(hw.ctx(), id as u32, &mut out) }, 0);
            if out.packet != 0 {
                let (ptr, _) = hw.pkt_buf(out.packet).unwrap();
                let b = unsafe { std::slice::from_raw_parts(ptr, out.len as usize) };
                assert!(cbor::get_int(b, 1).unwrap().is_some(), "{b:?}");
                hw.pkt_free(out.packet);
                unsafe {
                    t.written.unwrap()(hw.ctx(), id as u32);
                    t.close.unwrap()(hw.ctx(), id as u32);
                }
                p.stop();
                self.rt.run(0);
                return;
            }
        }
        panic!("stream write did not commit");
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.flash.stop();
        self.rt.run(0);
        assert_eq!(self.flash.leaked(), 0);
        assert_eq!(self.flash.mem_leaked(), 0);
        std::fs::remove_file(&self.path).unwrap();
    }
}
fn payload(data: &[u8], key: u64) -> Vec<u8> {
    let mut r = cbor::get(data, key).unwrap().unwrap();
    match r.next() {
        Some(cbor::Item::Bytes(b)) => b.to_vec(),
        other => panic!("{other:?}"),
    }
}
fn negative(data: &[u8]) -> i64 {
    cbor::Reader::new(data).next().unwrap().as_int().unwrap()
}
#[test]
fn direct_files_replace_ranges_expect_and_limits() {
    let d = Device::new();
    let a = d.put(b"cfg", b"first");
    assert_eq!(cbor::get_int(&a, 1).unwrap(), Some(1));
    let get = encoded(|w| {
        w.map(3)
            .uint(1)
            .bytes(b"cfg")
            .uint(6)
            .uint(1)
            .uint(7)
            .uint(3);
    });
    let a = d.files(17, &get);
    assert_eq!(payload(&a, 13), b"irs");
    assert_eq!(cbor::get_int(&a, 2).unwrap(), Some(5));
    let stale = encoded(|w| {
        w.map(3)
            .uint(1)
            .bytes(b"cfg")
            .uint(5)
            .uint(0)
            .uint(13)
            .bytes(b"no");
    });
    assert_eq!(negative(&d.files(18, &stale)), -21);
    assert_eq!(
        cbor::get_int(&d.put(b"cfg", &vec![7; 1024]), 1).unwrap(),
        Some(2)
    );
    assert_eq!(negative(&d.put(b"cfg", &vec![9; 1025])), ERR_ARG as i64);
    let a = d.files(
        17,
        &encoded(|w| {
            w.map(1).uint(1).bytes(b"cfg");
        }),
    );
    assert_eq!(payload(&a, 13), vec![7; 1024]);
    assert_eq!(cbor::get_int(&d.files(8, &[0xa0]), 4).unwrap(), Some(1024));
    assert_eq!(
        negative(&d.files(
            17,
            &encoded(|w| {
                w.map(1).uint(1).bytes(b"missing");
            })
        )),
        ERR_NOTFOUND as i64
    );
}
#[test]
fn telemetry_direct_batches_and_ack() {
    let d = Device::new();
    for (seq, data) in [(1, b"abc".as_slice()), (4, b"def".as_slice())] {
        let a = d.files(
            19,
            &encoded(|w| {
                w.map(3)
                    .uint(10)
                    .uint(seq)
                    .uint(11)
                    .uint(3)
                    .uint(13)
                    .bytes(data);
            }),
        );
        assert_eq!(cbor::get_int(&a, 1).unwrap(), Some(seq as i64 + 2));
    }
    let a = d.files(
        20,
        &encoded(|w| {
            w.map(1).uint(13).uint(3);
        }),
    );
    assert_eq!(cbor::get_int(&a, 1).unwrap(), Some(6));
    assert_eq!(cbor::get_int(&a, 2).unwrap(), Some(1));
    let mut batches = cbor::get(&a, 14).unwrap().unwrap();
    assert_eq!(batches.next(), Some(cbor::Item::Array(1)));
    d.files(
        12,
        &encoded(|w| {
            w.map(1).uint(10).uint(3);
        }),
    );
    let a = d.files(20, &[0xa0]);
    assert_eq!(cbor::get_int(&a, 2).unwrap(), Some(4));
}
#[test]
fn both_interpreters_load_programs_larger_than_a_packet() {
    let d = Device::new();
    let mut source = b"#".to_vec();
    source.extend(vec![b' '; 2500]);
    source.extend(b"\nreturn 6*7");
    d.stream_put(b"berry", &source);
    let program = wat::parse_str(format!(
        "(module (@custom \"pad\" \"{}\") (func (export \"sum\") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))",
        " ".repeat(2500)
    ))
    .unwrap();
    d.stream_put(b"wasm", &program);
    let berry = d.rt.add_handler("berry", berry_c::HANDLER, 1696);
    berry.set_component(50);
    berry.start(&[]);
    let wasm = d.rt.add_handler("wasm3", wasm3_c::HANDLER, 1696);
    wasm.set_component(51);
    wasm.start(&[]);
    d.rt.run(0);
    let a = d.call(
        &berry,
        50,
        2,
        &encoded(|w| {
            w.map(1).uint(1).bytes(b"berry");
        }),
    );
    assert_eq!(cbor::get_int(&a, 3).unwrap(), Some(0));
    assert_eq!(cbor::get_text(&a, 1).unwrap(), Some(b"42".as_slice()));
    let a = d.call(
        &wasm,
        51,
        2,
        &encoded(|w| {
            w.map(3)
                .uint(1)
                .bytes(b"wasm")
                .uint(2)
                .text("sum")
                .uint(3)
                .array(2)
                .int(12)
                .int(31);
        }),
    );
    assert_eq!(cbor::get_int(&a, 3).unwrap(), Some(0));
    assert_eq!(cbor::get_int(&a, 1).unwrap(), Some(43));
    let a = d.call(
        &berry,
        50,
        2,
        &encoded(|w| {
            w.map(1).uint(1).bytes(b"absent");
        }),
    );
    assert_eq!(negative(&a), ERR_NOTFOUND as i64);
    berry.stop();
    wasm.stop();
    d.rt.run(0);
    assert_eq!(berry.mem_leaked(), 0);
    assert_eq!(wasm.mem_leaked(), 0);
    assert_eq!(berry.leaked(), 0);
    assert_eq!(wasm.leaked(), 0);
}

#[test]
fn direct_empty_files_eof_and_bounded_wait_queue() {
    let d = Device::new();
    d.put(b"empty", b"");
    let get = encoded(|w| {
        w.map(2).uint(1).bytes(b"empty").uint(6).uint(200);
    });
    let a = d.files(17, &get);
    assert_eq!(payload(&a, 13), b"");
    assert_eq!(cbor::get_int(&a, 6).unwrap(), Some(0));
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let write = encoded(|w| {
        w.map(2).uint(1).bytes(b"empty").uint(13).bytes(b"new");
    });
    assert_eq!(d.flash.send(15, 18, id, &write), 0);
    d.rt.run(0);
    let mut ids = Vec::new();
    for _ in 0..5 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert_eq!(d.flash.send(15, 17, n, &get), 0);
        d.rt.run(0);
        ids.push(n);
    }
    assert_eq!(negative(&d.answer(ids[4], &d.flash)), ERR_BUSY as i64);
    assert_eq!(cbor::get_int(&d.answer(id, &d.flash), 1).unwrap(), Some(2));
    for n in &ids[..4] {
        let a = d.answer(*n, &d.flash);
        assert_eq!(cbor::get_int(&a, 1).unwrap(), Some(2));
    }
}

#[test]
fn provider_absence_and_stopping_an_inflight_program_release_storage() {
    let rt = Runtime::default();
    let b = rt.add_handler("berry", berry_c::HANDLER, 1696);
    b.set_component(50);
    b.start(&[]);
    rt.run(0);
    let request = encoded(|w| {
        w.map(1).uint(1).bytes(b"absent");
    });
    let a = b.request(50, 2, &request).unwrap();
    assert_eq!(negative(&a), ERR_NOTFOUND as i64);
    b.stop();
    rt.run(0);
    assert_eq!(b.mem_leaked(), 0);
    assert_eq!(b.leaked(), 0);
    let d = Device::new();
    d.put(b"ok", b"return 1");
    let b = d.rt.add_handler("berry", berry_c::HANDLER, 1696);
    b.set_component(50);
    b.start(&[]);
    d.rt.run(0);
    assert_eq!(
        b.send(
            50,
            2,
            991,
            &encoded(|w| {
                w.map(1).uint(1).bytes(b"ok");
            })
        ),
        0
    );
    d.rt.run(0);
    b.stop();
    d.rt.run(1000);
    assert_eq!(b.mem_leaked(), 0);
    assert_eq!(b.leaked(), 0);
}

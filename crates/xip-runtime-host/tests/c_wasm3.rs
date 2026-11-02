//! The wasm3 interpreter as a C module (`modules/mod_wasm3`, compiled in by `crates/wasm3-c`), run by the host runtime as a natively
//! linked handler: the module the device runs, on the host first. A request carries a small wasm app, not aligned to anything; the
//! module answers `{1: result?, 2: logged text or error?, 3: status}` (0 ok, 1 not loadable, 2 trapped, 3 bad request).
use xip_abi::cbor;
use xip_abi::*;
use xip_runtime_host::{Config, Instance, Runtime};

/// The state `modules/C-MODULES` declares for wasm3.
const WASM3_STATE: usize = 1696;

fn wasm3(rt: &Runtime) -> Instance {
    let m = rt.add_handler("wasm3", wasm3_c::HANDLER, WASM3_STATE);
    m.set_component(51);
    m
}

/// The interpreter, the app's value stack, its code and its memory are the module's `mem_alloc` heap: more than the 8 KiB of the default
/// (the cap of the app's own memory is separate and set per request); these tests give apps whole 64 KiB pages, a device would use small ones.
const WASM3_HEAP: u32 = 512 * 1024;

fn loaded() -> (Runtime, Instance) {
    let rt = Runtime::new(Config { mem_quota: WASM3_HEAP, ..Default::default() });
    let m = wasm3(&rt);
    m.start(b"");
    rt.run(0);
    (rt, m)
}

struct Answer {
    result: Option<i64>,
    text: Option<String>,
    status: i64,
}

fn run_with(m: &Instance, wasm: &[u8], export: Option<&str>, args: &[i64], cap: Option<u64>) -> Answer {
    let mut buf = [0u8; 4096];
    let mut w = cbor::Writer::new(&mut buf);
    w.map(1 + export.is_some() as usize + (!args.is_empty()) as usize + cap.is_some() as usize);
    w.uint(1).bytes(wasm);
    if let Some(e) = export { w.uint(2).text(e); }
    if !args.is_empty() {
        w.uint(3).array(args.len());
        for a in args { w.int(*a); }
    }
    if let Some(c) = cap { w.uint(4).uint(c); }
    let request = w.finish().unwrap().to_vec();
    let answer = m.request(51, 1, &request).expect("request");
    Answer {
        result: cbor::get_int(&answer, 1).expect("cbor"),
        text: cbor::get_text(&answer, 2).expect("cbor").map(|t| String::from_utf8_lossy(t).into_owned()),
        status: cbor::get_int(&answer, 3).expect("cbor").expect("status is always present"),
    }
}

fn run(m: &Instance, wat_source: &str, args: &[i64]) -> Answer {
    let wasm = wat::parse_str(wat_source).expect("wat");
    run_with(m, &wasm, None, args, None)
}

const ADD: &str = r#"(module (func (export "run") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))"#;

#[test]
fn wasm3_hello_and_stop() {
    let (rt, m) = loaded();
    assert!(!m.ended(), "the module stays resident");
    assert!(m.logs().iter().any(|l| l.contains("hello from mod_wasm3")), "logs: {:?}", m.logs());
    m.stop();
    rt.run(0);
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn wasm3_runs_an_export_with_arguments() {
    let (_rt, m) = loaded();
    let a = run(&m, ADD, &[40, 2]);
    assert_eq!((a.status, a.result), (0, Some(42)), "{:?}", a.text);
    assert_eq!(m.mem_leaked(), 0, "every block of the interpreter and the app comes back after the call");
    // a second app in the same module: nothing of the first stays
    let a = run(&m, r#"(module (func (export "run") (result i64) i64.const 5000000000))"#, &[]);
    assert_eq!((a.status, a.result), (0, Some(5_000_000_000)));
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn wasm3_app_logs_through_the_host() {
    let (_rt, m) = loaded();
    let app = r#"(module
        (import "xip" "log" (func $log (param i32 i32) (result i32)))
        (import "xip" "now_us" (func $now (result i64)))
        (memory 1)
        (data (i32.const 0) "hello from an app")
        (func (export "run") (result i32)
            (drop (call $log (i32.const 0) (i32.const 17)))
            (i32.wrap_i64 (i64.shr_u (call $now) (i64.const 63)))))"#;
    let a = run_with(&m, &wat::parse_str(app).unwrap(), None, &[], Some(64 * 1024));
    assert_eq!((a.status, a.result), (0, Some(0)), "{:?}", a.text);
    assert_eq!(a.text.as_deref(), Some("hello from an app"));
    assert!(m.logs().iter().any(|l| l.contains("hello from an app")), "logs: {:?}", m.logs());
}

#[test]
fn wasm3_app_with_a_one_byte_page_size() {
    // an app that is not a multiple of 64 KiB: 2 KiB of memory declared in 1-byte pages (the custom page sizes proposal)
    let (_rt, m) = loaded();
    let app = r#"(module (memory 2048 2048 (pagesize 1))
        (func (export "run") (result i32) (memory.size)))"#;
    let a = run(&m, app, &[]);
    assert_eq!((a.status, a.result), (0, Some(2048)), "{:?}", a.text);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn wasm3_memory_cap_applies_to_the_app() {
    let (_rt, m) = loaded();
    let wasm = wat::parse_str(r#"(module (memory 1) (func (export "run") (result i32) (memory.grow (i32.const 1))))"#).unwrap();
    // 64 KiB is declared; the default cap (16 KiB) is below it
    assert_eq!(run_with(&m, &wasm, None, &[], None).status, 1, "memory over the cap does not load");
    let a = run_with(&m, &wasm, None, &[], Some(64 * 1024));
    assert_eq!((a.status, a.result), (0, Some(-1)), "growing past the cap fails inside the app: {:?}", a.text);
    let a = run_with(&m, &wasm, None, &[], Some(256 * 1024));
    assert_eq!((a.status, a.result), (0, Some(1)));
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn wasm3_traps_and_bad_apps_are_answered() {
    let (_rt, m) = loaded();
    let a = run(&m, r#"(module (func (export "run") unreachable))"#, &[]);
    assert_eq!(a.status, 2, "trap: {:?}", a.text);
    assert!(a.text.unwrap().contains("unreachable"));
    assert_eq!(run_with(&m, b"not wasm", None, &[], None).status, 1);
    assert_eq!(run_with(&m, &wat::parse_str(ADD).unwrap(), Some("missing"), &[1, 2], None).status, 1, "no such export");
    assert_eq!(run(&m, ADD, &[1]).status, 3, "wrong number of arguments");
    // the module still serves after all of that
    assert_eq!(run(&m, ADD, &[1, 2]).result, Some(3));
    assert_eq!(m.mem_leaked(), 0, "errors release everything too");
}

//! A C module compiled into this test binary by `crates/berry-c`, run by the host runtime as a
//! natively linked handler: the Berry language module. The hello world arrives through start; the
//! interpreter through the module's own component 50, request method 1 (`eval`), answering
//! `{1: result?, 2: output?, 3: status}`.
use xip_abi::cbor;
use xip_abi::*;
use xip_runtime_host::{Instance, Runtime};

/// The state and the component `modules/C-MODULES` declares for berry: the interpreter needs far
/// more than the bare helper's 64 bytes. A wasm module brings its own (sidecar and manifest); a C
/// module is described here and linked in.
const BERRY_STATE: usize = 1696;

fn berry(rt: &Runtime) -> Instance {
    let m = rt.add_handler("berry", berry_c::HANDLER, BERRY_STATE);
    m.set_component(50);
    m
}

fn loaded(request: &[u8]) -> (Runtime, Instance) {
    let rt = Runtime::default();
    let m = berry(&rt);
    m.start(request);
    rt.run(0);
    (rt, m)
}

/// One `eval` request; returns (status, result text, interpreter output).
fn eval(m: &Instance, src: &str) -> (i64, Option<String>, Option<String>) {
    let answer = m.request(50, 1, &request(src)).expect("request");
    let status = cbor::get_int(&answer, 3).expect("cbor").expect("status is always present");
    let text = |key: u64| {
        cbor::get_text(&answer, key)
            .expect("cbor")
            .map(|t| String::from_utf8_lossy(t).into_owned())
    };
    (status, text(1), text(2))
}

fn request(src: &str) -> Vec<u8> {
    let mut buf = [0u8; 4096];
    let mut w = cbor::Writer::new(&mut buf);
    w.map(1);
    w.uint(1).text(src);
    w.finish().unwrap().to_vec()
}

#[test]
fn berry_hello_world() {
    let (rt, m) = loaded(b"hello");
    assert!(!m.ended(), "the module stays resident after start");
    assert!(m.logs().iter().any(|l| l.contains("hello from mod_berry")), "logs: {:?}", m.logs());
    assert!(m.find_event(1, "hello").is_some(), "the start request comes back as event 1: {:?}", m.events());
    m.stop();
    rt.run(0);
    assert!(m.ended() && m.result() == RET_DONE as i32);
    assert_eq!(m.leaked(), 0);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn berry_hello_as_a_named_component() {
    // the natively linked handler carries the name given to add_handler; set_component stands for
    // the component a wasm module declares in its manifest (modules/C-MODULES says berry is 50).
    let rt = Runtime::default();
    let m = berry(&rt);
    m.start(b"hi");
    rt.run(0);
    assert!(!m.ended());
    assert_eq!(m.name(), "berry");
    assert!(m.find_event(1, "hi").is_some());
}

#[test]
fn berry_eval_runs_and_captures_output() {
    let (rt, m) = loaded(b"");
    let (status, result, out) = eval(&m, "print(1 + 2)");
    assert_eq!(status, 0, "ok");
    assert_eq!(out.as_deref(), Some("3\n"), "what print wrote");
    assert_eq!(result, None, "a statement returns nil, and nil is not answered");
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0, "the VM and everything it allocated comes back");
}

#[test]
fn berry_eval_answers_the_value_of_an_expression() {
    let (rt, m) = loaded(b"");
    let (status, result, out) = eval(&m, "1 + 2");
    assert_eq!(status, 0);
    assert_eq!(result.as_deref(), Some("3"), "the script's value comes back as the result");
    assert_eq!(out, None, "nothing was printed");
    let (status, result, _) = eval(&m, "\"a\" + \"b\"");
    assert_eq!(status, 0);
    assert_eq!(result.as_deref(), Some("ab"), "a string result, without the repl's quotes");
    // A script that allocates and keeps a global variable: the stop still balances to zero (the VM is deleted whole).
    eval(&m, "cache = [1, 2, 3]");
    eval(&m, "cache.push(4)");
    m.stop();
    rt.run(0);
    assert_eq!(m.mem_leaked(), 0);
}

#[test]
fn berry_eval_compile_error() {
    let (_rt, m) = loaded(b"");
    let (status, result, out) = eval(&m, "+");
    assert_eq!(status, 1, "compile error");
    assert_eq!(result, None);
    let out = out.expect("the error text is the output");
    assert!(out.contains("syntax_error") && out.contains("unexpected symbol"), "captured: {out:?}");
}

#[test]
fn berry_eval_undeclared_is_a_compile_error() {
    let (_rt, m) = loaded(b"");
    let (status, _, out) = eval(&m, "print(never_declared)");
    assert_eq!(status, 1, "Berry reports undeclared names while compiling");
    assert!(out.expect("text").contains("undeclared"));
}

#[test]
fn berry_eval_runtime_error() {
    let (_rt, m) = loaded(b"");
    let (status, result, out) = eval(&m, "raise \"boom\"");
    assert_eq!(status, 2, "runtime error");
    assert_eq!(result, None);
    let out = out.expect("the exception and its traceback are the output");
    assert!(out.contains("boom"), "captured: {out:?}");
    assert!(out.contains("request"), "the traceback names the compiled source: {out:?}");
}

#[test]
fn berry_eval_state_survives_between_requests() {
    let (_rt, m) = loaded(b"");
    // a top level assignment in one chunk is a global variable, visible to the next one
    let (status, _, _) = eval(&m, "count = 7");
    assert_eq!(status, 0);
    let (status, result, _) = eval(&m, "count + 1");
    assert_eq!(status, 0);
    assert_eq!(result.as_deref(), Some("8"), "one VM serves every request");
}

#[test]
fn berry_start_run_request_executes_source() {
    let (_rt, m) = loaded(b"run:print(\"from the start request\")");
    assert!(
        m.logs().iter().any(|l| l.contains("from the start request")),
        "the script's output reaches the log: {:?}",
        m.logs()
    );
    assert!(!m.find_event(1, "run:").is_some(), "a run: request is executed, not echoed");
}

#[test]
fn berry_eval_bad_request_or_no_interpreter() {
    let rt = Runtime::default();
    let m = berry(&rt);
    // No start: the interpreter was never made.
    let answer = m.request(50, 1, &request("1 + 2")).expect("request");
    assert_eq!(cbor::get_int(&answer, 3).expect("cbor"), Some(4), "4: no interpreter");
    // Started, but a request without a text field.
    m.start(b"");
    rt.run(0);
    let mut buf = [0u8; 16];
    let mut w = cbor::Writer::new(&mut buf);
    w.map(1);
    w.uint(1).uint(7);
    let bad = w.finish().unwrap().to_vec();
    let answer = m.request(50, 1, &bad).expect("request");
    assert_eq!(cbor::get_int(&answer, 3).expect("cbor"), Some(3), "3: bad request");
}

#[test]
fn port_snprintf_subset_matches_the_reference_c_library() {
    // The formatter a packed image carries (port/xip_libc.c), linked in by crates/berry-c under
    // its own name so the host can call it without interposing libc. Berry's print reaches this on
    // a device: it once printed "-42" for `print(7 * 6)` there while the host, using the real
    // snprintf, printed 42.
    let mine: unsafe extern "C" fn(*mut i8, usize, *const i8, ...) -> i32 = berry_c::xip_fmt_snprintf;
    extern "C" {
        fn snprintf(s: *mut i8, n: usize, fmt: *const i8, ...) -> i32;
    }
    let reference: unsafe extern "C" fn(*mut i8, usize, *const i8, ...) -> i32 = snprintf;
    macro_rules! both {
        ($fmt:literal $(, $arg:expr)*) => {{
            let cfmt = std::ffi::CString::new($fmt).unwrap();
            let (mut a, mut b) = ([0i8; 128], [0i8; 128]);
            let (na, nb) = (
                unsafe { mine(a.as_mut_ptr(), a.len(), cfmt.as_ptr(), $($arg),*) },
                unsafe { reference(b.as_mut_ptr(), b.len(), cfmt.as_ptr(), $($arg),*) },
            );
            let text = |buf: &[i8; 128]| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
            assert_eq!((na, text(&a)), (nb, text(&b)), "format {:?}", $fmt);
            text(&a)
        }};
    }
    let (c, hi) = (b'q' as i32, std::ffi::CString::new("hello").unwrap());
    assert_eq!(both!("%d", 42), "42", "the device regression");
    assert_eq!(both!("%d", -42), "-42");
    assert_eq!(both!("%d", i32::MIN), "-2147483648");
    assert_eq!(both!("%05d", -42), "-0042");
    assert_eq!(both!("%+5d", 42), "  +42");
    assert_eq!(both!("% d", 7), " 7");
    assert_eq!(both!("%-4d|", 7), "7   |");
    assert_eq!(both!("%u", u32::MAX), "4294967295");
    assert_eq!(both!("%x %#x %X", 0x10FFu32, 255u32, 0xabcdefu32), "10ff 0xff ABCDEF");
    assert_eq!(both!("%o %#o", 0o755u32, 8u32), "755 010");
    assert_eq!(both!("%lld", -9_000_000_000i64), "-9000000000");
    assert_eq!(both!("%s %5.2s %c %%", hi.as_ptr(), hi.as_ptr(), c), "hello    he q %");
    assert_eq!(both!("%f %.1f %.0f", 2.5, 2.2, 40.0), "2.500000 2.2 40");
    assert_eq!(both!("%.2f %g", std::f64::consts::PI, 42.5), "3.14 42.5");
    assert_eq!(both!("%e %.3e %E", 1234.5, 1234.5, 0.5), "1.234500e+03 1.234e+03 5.000000E-01");
    assert_eq!(both!("%g %g %g", 1e7, 0.0001, 0.5), "1e+07 0.0001 0.5");
    // the same truncation rule: count what the full text would be, write at most n - 1
    let cfmt = std::ffi::CString::new("%d").unwrap();
    let (mut a, mut b) = ([0i8; 4], [0i8; 4]);
    let (na, nb) = (
        unsafe { mine(a.as_mut_ptr(), 4, cfmt.as_ptr(), 12345) },
        unsafe { reference(b.as_mut_ptr(), 4, cfmt.as_ptr(), 12345) },
    );
    assert_eq!((na, &a[..]), (nb, &b[..]), "truncated %d");
    // and an unsupported conversion answers the failure the interpreter reports (-1), not garbage:
    // the reference libc would print "%y", so that case is checked against the port's own promise.
    assert_eq!(
        unsafe { mine(a.as_mut_ptr(), a.len(), std::ffi::CString::new("%y").unwrap().as_ptr(), 1) },
        -1,
        "a conversion outside the subset fails rather than prints wrong"
    );
}

#[test]
fn berry_huge_and_tiny_reals_format_without_overflowing() {
    // `%f` of 1e100 wrote 101 digits into an 80-byte buffer.
    let (_rt, m) = loaded(b"");
    eval(&m, "import string");
    let (status, result, _) = eval(&m, "string.format('%f', 1e100)");
    assert_eq!(status, 0);
    assert_eq!(result.expect("text").len(), 101 + 7, "101 integer digits, a point, six decimals");
    let (status, result, _) = eval(&m, "string.format('%.30f', 1e-300)");
    assert_eq!(status, 0);
    assert!(result.expect("text").starts_with("0.000"));
}

#[test]
fn berry_real_modulo_near_the_limit_terminates() {
    // `1.7e308 % 1` doubled its divisor to infinity and then looped forever.
    let (_rt, m) = loaded(b"");
    let (status, result, _) = eval(&m, "1.7e308 % 3.0 < 3.0");
    assert_eq!(status, 0);
    assert_eq!(result.as_deref(), Some("true"));
}

#[test]
fn berry_result_whose_tostring_raises_is_a_runtime_error() {
    // Converting the result runs the script's own tostring(); a throw there used to reach the
    // module's abort (an illegal instruction on a device).
    let (_rt, m) = loaded(b"");
    eval(&m, "class A def tostring() raise 'bad', 'tostring' end end");
    let (status, _, out) = eval(&m, "A()");
    assert_eq!(status, 2, "output: {out:?}");
    assert!(out.expect("the exception").contains("bad"));
    let (status, result, _) = eval(&m, "6 * 7");
    assert_eq!((status, result.as_deref()), (0, Some("42")), "the VM is still serving");
}

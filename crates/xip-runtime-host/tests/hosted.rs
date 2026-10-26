//! A component hosted by a module (manifest key 11): the runtime asks for its table after `HOST_START` and hands `get_component` callers a copy whose
//! functions are thunks into the module (its context, not the caller's), `ERR_NOTFOUND` once the module is gone.
use std::ffi::c_void;
use xip_abi::*;
use xip_runtime_host::{Instance, Runtime};

#[repr(C)]
struct Table {
    version: u32,
    size: u32,
    add: Option<unsafe extern "C" fn(*mut c_void, i32, i32) -> i32>,
    unused: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
    scale: Option<unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32, i32) -> i32>,
    /// calls back into its own component through the table a caller holds (a module calling a table from inside a table call)
    nested: Option<unsafe extern "C" fn(*mut c_void) -> i32>,
}

unsafe extern "C" fn add(ctx: *mut c_void, a: i32, b: i32) -> i32 {
    *(ctx as *const i32) + a + b
}
unsafe extern "C" fn scale(ctx: *mut c_void, a: i32, b: i32, c: i32, d: i32, e: i32) -> i32 {
    *(ctx as *const i32) * (a + 2 * b + 3 * c + 4 * d + 5 * e)
}
/// state: [i32 private data][padding][host pointer]; the context is the state
unsafe extern "C" fn nested(ctx: *mut c_void) -> i32 {
    let host = *((ctx as *mut usize).add(1)) as *const Host;
    let h = &*host;
    let t = (h.get_component.unwrap())(h.user, c"demo".as_ptr()) as *const Table;
    if t.is_null() { return -99; }
    1 + (*t).add.unwrap()(std::ptr::null_mut(), 10, 20)
}
unsafe extern "C" fn add_b(ctx: *mut c_void, a: i32, b: i32) -> i32 {
    *(ctx as *const i32) * 2 + a + b
}
static TABLE: Table = Table {
    version: 1,
    size: std::mem::size_of::<Table>() as u32,
    add: Some(add),
    unused: None,
    scale: Some(scale),
    nested: Some(nested),
};
/// the replacement image: other functions at other addresses, one more entry empty
static TABLE_B: Table = Table {
    version: 2,
    size: std::mem::size_of::<Table>() as u32,
    add: Some(add_b),
    unused: None,
    scale: None,
    nested: Some(nested),
};

unsafe fn message(h: &Host, dst: u32, component: i32, method: i32, token: u64, data: &[u8]) -> i32 {
    let (mut pkt, mut window, mut room) = (0u32, std::ptr::null_mut(), 0usize);
    assert_eq!((h.pkt_alloc.unwrap())(h.user, &mut pkt), 0);
    assert_eq!((h.pkt_buf.unwrap())(h.user, pkt, &mut window, &mut room), 0);
    std::ptr::copy_nonoverlapping(data.as_ptr(), window, data.len());
    (h.send.unwrap())(h.user, pkt, data.len(), dst, component, method, token)
}

/// state: the i32 the module's context points at (its "private data")
unsafe extern "C" fn provider(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    provide_with(host, state, call, &TABLE)
}
unsafe extern "C" fn provider_b(host: *const Host, state: *mut u8, call: *const Call) -> i32 {
    provide_with(host, state, call, &TABLE_B)
}
/// A module whose HOST_START refuses.
unsafe extern "C" fn refuses(_: *const Host, _: *mut u8, call: *const Call) -> i32 {
    if (*call).component == HOST_COMPONENT as i32 && (*call).method == HOST_START as i32 { ERR_IO } else { 0 }
}
unsafe fn provide_with(host: *const Host, state: *mut u8, call: *const Call, table: &'static Table) -> i32 {
    let (h, call) = (&*host, &*call);
    if call.component == HOST_COMPONENT as i32 && call.method == HOST_START as i32 {
        *(state as *mut i32) = 1000;
        *((state as *mut usize).add(1)) = host as usize;
    }
    if call.component == HOST_COMPONENT as i32 && call.method == HOST_TABLE as i32 {
        let name = std::slice::from_raw_parts(call.data, call.len);
        let mut answer = Vec::new();
        let (table, ctx) = if name == b"demo" { (table as *const Table as usize, state as usize) } else { (0, 0) };
        answer.extend_from_slice(&table.to_ne_bytes());
        answer.extend_from_slice(&ctx.to_ne_bytes());
        message(h, call.from, call.component, call.method, call.id, &answer);
    }
    0
}

fn lookup(rt_user: &Instance, name: &std::ffi::CStr) -> *const Table {
    let h = rt_user.host();
    unsafe { (h.get_component.unwrap())(h.user, name.as_ptr()) as *const Table }
}

fn setup() -> (Runtime, Instance, Instance) {
    let rt = Runtime::default();
    let p = rt.add_handler("p", provider, 64);
    p.set_component(70);
    p.host_component("demo");
    let c = rt.add_handler("c", provider, 64);
    c.set_component(71);
    p.start(b"");
    c.start(b"");
    rt.run(0);
    (rt, p, c)
}

#[test]
fn a_caller_gets_thunks_that_run_in_the_hosting_module() {
    let (_rt, _p, c) = setup();
    let t = lookup(&c, c"demo");
    assert!(!t.is_null());
    let t = unsafe { &*t };
    assert_eq!((t.version, t.size as usize), (1, std::mem::size_of::<Table>()));
    assert_ne!(t.add.unwrap() as usize, add as *const () as usize, "a thunk of the runtime, not the module's function");
    // the first argument is the caller's context; the module's own is passed instead
    let caller_ctx = 0xdead_beef_usize as *mut c_void;
    assert_eq!(unsafe { t.add.unwrap()(caller_ctx, 2, 3) }, 1005);
    assert_eq!(unsafe { t.scale.unwrap()(caller_ctx, 1, 1, 1, 1, 1) }, 15_000, "five more words arrive intact");
    assert!(t.unused.is_none(), "a method the module does not implement stays empty");
    // the same copy again
    assert_eq!(lookup(&c, c"demo") as usize, t as *const Table as usize);
}

#[test]
fn other_names_and_unstarted_modules_have_no_table() {
    let rt = Runtime::default();
    let p = rt.add_handler("p", provider, 64);
    p.set_component(70);
    p.host_component("demo");
    assert!(lookup(&p, c"demo").is_null(), "not started yet: no table");
    p.start(b"");
    rt.run(0);
    assert!(!lookup(&p, c"demo").is_null());
    assert!(lookup(&p, c"nothing").is_null());
    // the runtime's own components come first
    static OWN: Table = Table { version: 9, size: 8, add: None, unused: None, scale: None, nested: None };
    rt.register_component("demo", &OWN as *const Table as *const c_void);
    assert_eq!(unsafe { (*lookup(&p, c"demo")).version }, 9);
}

#[test]
fn a_stopped_module_answers_notfound_and_its_successor_serves_the_same_table() {
    let (rt, p, c) = setup();
    let t = unsafe { &*lookup(&c, c"demo") };
    assert_eq!(unsafe { t.add.unwrap()(std::ptr::null_mut(), 1, 1) }, 1002);
    p.stop();
    rt.run(0);
    assert_eq!(unsafe { t.add.unwrap()(std::ptr::null_mut(), 1, 1) }, ERR_NOTFOUND);
    assert!(lookup(&c, c"demo").is_null(), "no provider is running");
    // a new instance of the module takes over: the table a caller still holds works again, with the new context
    let p2 = rt.add_handler("p", provider, 64);
    p2.set_component(70);
    p2.host_component("demo");
    p2.start(b"");
    rt.run(0);
    assert_eq!(unsafe { t.add.unwrap()(std::ptr::null_mut(), 1, 1) }, 1002);
    assert_eq!(lookup(&c, c"demo") as usize, t as *const Table as usize);
}

#[test]
fn a_module_is_started_on_demand_by_get_component() {
    use std::cell::Cell;
    use std::rc::Rc;
    let rt = Runtime::default();
    let asked = Rc::new(Cell::new(0));
    let (rt2, asked2) = (rt.clone(), asked.clone());
    rt.on_demand(move |name| {
        asked2.set(asked2.get() + 1);
        match name {
            "demo" => {
                let p = rt2.add_handler("p", provider, 64);
                p.set_component(70);
                p.host_component("demo");
                p.start(b"");
                Some(p)
            }
            "broken" => {
                let p = rt2.add_handler("b", refuses, 64);
                p.host_component("broken");
                p.start(b"");
                Some(p)
            }
            _ => None,
        }
    });
    let c = rt.add_handler("c", provider, 64);
    c.set_component(71);
    c.start(b"");
    rt.run(0);
    // not running: get_component starts it and answers with its table
    let t = lookup(&c, c"demo");
    assert!(!t.is_null());
    assert_eq!(unsafe { (*t).add.unwrap()(std::ptr::null_mut(), 2, 3) }, 1005, "its HOST_START ran before the table was given out");
    assert_eq!(asked.get(), 1);
    assert_eq!(lookup(&c, c"demo") as usize, t as usize);
    assert_eq!(asked.get(), 1, "running: nobody is asked again");
    // nothing installed by that name, and a module that cannot start
    assert!(lookup(&c, c"nothing").is_null());
    assert!(lookup(&c, c"broken").is_null());
}

#[test]
fn stop_and_replacement_rebind_the_table_a_caller_holds() {
    let (rt, p, c) = setup();
    let t = unsafe { &*lookup(&c, c"demo") };
    let held = t.add.unwrap();
    assert_eq!(unsafe { held(std::ptr::null_mut(), 1, 1) }, 1002);
    assert_eq!(unsafe { t.scale.unwrap()(std::ptr::null_mut(), 1, 1, 1, 1, 1) }, 15_000);
    p.stop();
    rt.run(0);
    // stopped: never the stale code
    assert_eq!(unsafe { held(std::ptr::null_mut(), 1, 1) }, ERR_NOTFOUND);
    assert_eq!(unsafe { t.scale.unwrap()(std::ptr::null_mut(), 1, 1, 1, 1, 1) }, ERR_NOTFOUND);
    // the replacement is another image: other functions, one entry gone, a new version
    let p2 = rt.add_handler("p", provider_b, 64);
    p2.set_component(70);
    p2.host_component("demo");
    p2.start(b"");
    rt.run(0);
    assert_eq!(unsafe { held(std::ptr::null_mut(), 1, 1) }, 2002, "the same thunk now runs the new image's function with its context");
    assert_eq!(unsafe { t.scale.unwrap()(std::ptr::null_mut(), 1, 1, 1, 1, 1) }, ERR_UNSUPPORTED, "an entry the new image lacks");
    // and back to the first
    p2.stop();
    rt.run(0);
    assert_eq!(unsafe { held(std::ptr::null_mut(), 1, 1) }, ERR_NOTFOUND);
}

#[test]
fn a_hosted_function_may_call_the_table_again_inline() {
    let (_rt, _p, c) = setup();
    let t = unsafe { &*lookup(&c, c"demo") };
    // nested() calls add(10, 20) through the table while it runs through the same table: no deadlock, no stale state
    assert_eq!(unsafe { t.nested.unwrap()(std::ptr::null_mut()) }, 1 + 1030);
}

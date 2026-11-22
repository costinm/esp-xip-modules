use xip_abi::{cbor::{self, Item, Reader, Writer}, RET_DONE};
use xip_runtime_host::{Instance, Runtime};
#[derive(Default)]
struct Files;
impl xip_abi::Module for Files {
    fn handle(&mut self, hw: &xip_abi::Hw, call: &xip_abi::CallView) -> i32 {
        if call.component != 15 || call.method != 17 { return 0; }
        let Some(Item::Bytes(name)) = cbor::get(call.data, 1).unwrap().unwrap().next() else { panic!("name") };
        if name == b"timeout" { return 0; }
        if name == b"missing" { hw.reply_error(call, xip_abi::ERR_NOTFOUND); return 0; }
        if name == b"bad" { hw.reply(call, &[0xa0]); return 0; }
        let expressions = vec!["count + 1u"; if name == b"ranges" { 200 } else { 1 }];
        let rules = ucel::parser::compile(&expressions, &[("count", 1)]).unwrap();
        if name == b"ranges" { assert!(rules.len() > 1024); }
        let offset = cbor::get(call.data, 6).unwrap().unwrap().next().unwrap().as_uint().unwrap() as usize;
        if offset > 0 { assert_eq!(cbor::get(call.data, 5).unwrap().unwrap().next(), Some(Item::Uint(1))); }
        let size = if name == b"large" { 16385 } else { rules.len() };
        let chunk = &rules[offset..rules.len().min(offset + 1024)];
        let mut b = [0; 1100]; let mut w = Writer::new(&mut b);
        w.map(4).uint(1).uint(1).uint(2).uint(size as u64).uint(6).uint(offset as u64).uint(13).bytes(chunk);
        hw.message(call.from, 15, 17, call.id.wrapping_add(1), &[0xa0]);
        hw.reply(call, w.finish().unwrap()); 0
    }
}
unsafe extern "C" fn files_handler(host: *const xip_abi::Host, _: *mut u8, call: *const xip_abi::Call) -> i32 {
    let hw = xip_abi::Hw::new(host).unwrap(); let view = xip_abi::CallView::new(call).unwrap();
    let packet = (view.pkt != 0).then(|| xip_abi::Packet::from_raw(hw, view.pkt));
    xip_abi::Module::handle_owned(&mut Files, &hw, &view, packet)
}
unsafe extern "C" fn requester_handler(host: *const xip_abi::Host, _: *mut u8, call: *const xip_abi::Call) -> i32 {
    let hw = xip_abi::Hw::new(host).unwrap(); let view = xip_abi::CallView::new(call).unwrap();
    let _packet = (view.pkt != 0).then(|| xip_abi::Packet::from_raw(hw, view.pkt));
    if view.component == 99 && view.method == 1 {
        assert_eq!(hw.message(53, 53, 2, 42, view.data), 0);
    } else if view.component == 53 && view.method == 2 && view.id == 42 {
        assert_eq!(hw.message(xip_abi::ADDR_MAIN, 99, 2, 0, view.data), 0);
    }
    0
}
fn setup() -> (Runtime, Instance) {
    let rt = Runtime::default();
    let m = rt.add_handler("cel", mod_cel::HANDLER, 0);
    m.set_component(53); m.start(b""); rt.run(0); (rt, m)
}
fn request(m: &Instance, rules: &[&str]) -> Vec<u8> {
    let rules = ucel::parser::compile(rules, &[("count", 1)]).unwrap();
    let mut b = [0u8; 4096];
    let mut w = Writer::new(&mut b); w.map(2).uint(1).bytes(&rules).uint(2).bytes(&[0xa1, 1, 0]);
    m.request(53, 1, w.finish().unwrap()).unwrap()
}
#[test]
fn eval_conditional_and_shared_context() {
    let (rt, m) = setup();
    let answer = request(&m, &["set(1u, 7u)", "count == 7u ? log(\"yes\") : log(\"no\")", "count + 1u"]);
    assert!(m.logs().iter().any(|s| s.contains("yes")));
    assert!(!m.logs().iter().any(|s| s.contains("no")));
    let mut r = cbor::get(&answer, 1).unwrap().unwrap();
    assert_eq!(r.next(), Some(Item::Array(3)));
    assert_eq!(r.next(), Some(Item::Uint(7))); assert_eq!(r.next(), Some(Item::Null)); assert_eq!(r.next(), Some(Item::Uint(8)));
    let mut r = cbor::get(&answer, 2).unwrap().unwrap();
    let Some(Item::Bytes(ctx)) = r.next() else { panic!("context") };
    assert_eq!(ucel::decode_context(ctx).unwrap(), vec![(1, ucel::Value::Uint(7))]);
    m.stop(); rt.run(0); assert_eq!(m.result(), RET_DONE as i32); assert_eq!(m.mem_leaked(), 0);
}
#[test]
fn alarm_runs_fresh_rules_with_final_snapshot() {
    let (rt, m) = setup();
    request(&m, &["count == 0u ? after(10u) : log(\"later\")", "set(1u, 9u)"]);
    assert_eq!(m.active_alarms(), 1);
    rt.run(9); assert!(!m.logs().iter().any(|s| s.contains("later")));
    rt.run(1); assert!(m.logs().iter().any(|s| s.contains("later"))); assert_eq!(m.active_alarms(), 0);
    m.stop(); rt.run(0); assert_eq!(m.mem_leaked(), 0);
}
#[test]
fn zero_delay_is_rejected() {
    let (_rt, m) = setup(); let answer = request(&m, &["after(0u)"]);
    assert!(matches!(Reader::new(&answer).next(), Some(Item::Neg(_))));
    assert_eq!(m.active_alarms(), 0);
}
#[test]
fn pending_alarm_limit_is_global_and_stop_cancels() {
    let (rt, m) = setup();
    for _ in 0..8 { request(&m, &["after(100u)"]); }
    assert_eq!(m.active_alarms(), 8);
    let answer = request(&m, &["after(100u)"]);
    assert!(matches!(Reader::new(&answer).next(), Some(Item::Neg(_))));
    assert_eq!(m.active_alarms(), 8);
    m.stop(); rt.run(0);
    assert_eq!(m.active_alarms(), 0);
    assert_eq!(m.mem_leaked(), 0);
}
#[test]
fn file_service_success_and_errors() {
    let (rt, m) = setup();
    let f = rt.add_handler("files", files_handler, 0); f.set_component(15); f.start(b""); rt.run(0);
    for name in [b"ok".as_slice(), b"ranges", b"bad", b"large", b"missing"] {
        let mut b = [0; 64]; let mut w = Writer::new(&mut b);
        w.map(2).uint(1).bytes(name).uint(2).bytes(&[0xa1, 1, 0]);
        let answer = m.request(53, 2, w.finish().unwrap()).unwrap();
        if name == b"ok" || name == b"ranges" { let mut r = cbor::get(&answer, 1).unwrap().unwrap(); let count = if name == b"ok" { 1 } else { 200 }; assert_eq!(r.next(), Some(Item::Array(count))); for _ in 0..count { assert_eq!(r.next(), Some(Item::Uint(1))); } }
        else { assert!(matches!(Reader::new(&answer).next(), Some(Item::Neg(_))), "{name:?}: {answer:?}"); }
        assert_eq!(m.active_alarms(), 0);
    }
    m.stop(); f.stop(); rt.run(0); assert_eq!(m.mem_leaked(), 0);
}
#[test]
fn file_timeout_and_stop_cleanup() {
    let (rt, m) = setup(); let f = rt.add_handler("files", files_handler, 0); f.set_component(15); f.start(b""); rt.run(0);
    let client = rt.add_handler("client", requester_handler, 0); client.set_component(99); client.start(b""); rt.run(0);
    let mut b = [0; 32]; let mut w = Writer::new(&mut b); w.map(1).uint(1).bytes(b"timeout"); let req = w.finish().unwrap();
    assert_eq!(client.post(99, 1, req), 0); rt.run(0); assert_eq!(m.active_alarms(), 1);
    assert!(client.events().is_empty());
    rt.run(30000); assert_eq!(m.active_alarms(), 0);
    let events = client.events(); assert_eq!(events.len(), 1);
    let mut expected = [0; 9]; let mut w = Writer::new(&mut expected); w.int(xip_abi::ERR_TIMEOUT as i64);
    assert_eq!(events[0].data, w.finish().unwrap());
    assert!(m.request(53, 2, req).unwrap().is_empty()); assert_eq!(m.active_alarms(), 1);
    m.stop(); f.stop(); client.stop(); rt.run(0); assert_eq!(m.active_alarms(), 0); assert_eq!(m.mem_leaked(), 0);
}

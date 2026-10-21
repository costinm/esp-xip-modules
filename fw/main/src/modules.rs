//! Module control as tagged-CBOR components: the DMesh side of the esp-xip-modules runtime.
//!
//! The runtime and the loader are the `xip-runtime-esp` crate (esp-xip-modules repository). Modules are optional and loadable:
//! DMesh links none of their crates. What a module answers to (its operation names and method numbers) is read from the
//! installed module's own manifest (`xip_runtime_esp::manifest`); this file only turns a tagged record into a module request.
//! It knows no module's configuration: a `lora` record's numeric fields are passed to the LoRa module as they are
//! (`modules/mod_lora/API.md`).
//! The module runtime owns slot names, tags and placement.
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use dmesh_server::{
    cbor::{Decoder, Encoder},
    tagged::{self, Name, Record},
};
use xip_runtime_esp::{self as em, Features};

/// Module-loader control/status component.
pub const MODULE_COMPONENT: u64 = 1000;
/// Flash-mapped hello module service.
pub const HELLO_COMPONENT: u64 = 1001;
/// Flash-mapped LoRa module service.
pub const LORA_COMPONENT: u64 = 1002;
/// Flash-mapped hardware module service.
pub const HARDWARE_COMPONENT: u64 = 1003;

pub const MODULE_STATUS: u64 = 1;
pub const MODULE_INIT: u64 = 2;
pub const MODULE_STOP: u64 = 3;
pub const MODULE_RUN: u64 = 4;

/// Component numbers of the modules: their slot tags.
const LORA_MODULE: i32 = 43;
const HW_MODULE: i32 = 45;
/// Field id of the packet bytes in a LoRa request map (`modules/mod_lora/API.md`).
const FIELD_DATA: u64 = 31;
/// Longest operation name accepted from a record.
const MAX_OPERATION: usize = 16;

/// The `nv` call of the module runtime (the console's `nv KEY[=VALUE]`): DMesh's own reviewed settings first (`uart.mode`,
/// `quic.pool`, ...), then the host Wi-Fi settings (`wifi.rx_static`, ...). -1 bad argument, -4 unknown or unset, -5 buffer too small.
unsafe extern "C" fn dmesh_nv(key: *const core::ffi::c_char, key_len: usize, value: *const u8, value_len: usize, out: *mut u8, cap: usize) -> i32 {
    use dmesh_fw_transport::main_runtime::{nv_get, nv_known, nv_set};
    let name = unsafe { core::slice::from_raw_parts(key.cast::<u8>(), key_len) };
    if !nv_known(name) {
        let value = if value.is_null() { None } else { Some(unsafe { core::slice::from_raw_parts(value, value_len) }) };
        let output = if cap == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(out, cap) } };
        return xip_runtime_esp::settings::nv(name, value, output);
    }
    if value.is_null() {
        let mut buf = [0u8; 64];
        return match nv_get(name, &mut buf) {
            Some(n) if n <= cap => {
                unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), out, n) };
                n as i32
            }
            Some(_) => -5,
            None => -4,
        };
    }
    let new = unsafe { core::slice::from_raw_parts(value, value_len) };
    if nv_set(name, new) { 0 } else { -1 }
}

// The packets of modules are the QUIC pool's: one budget for the device, grown and shrunk by the node's pool policy. The host keeps no
// pool of its own (xip_pool_ops).
use dmesh_fw_transport::module_pool as qpool;
use core::ffi::c_void;
unsafe extern "C" fn pool_acquire(_: *mut c_void, reserve: u16) -> i32 { qpool::acquire(reserve) }
unsafe extern "C" fn pool_retain(_: *mut c_void, slot: i32) { qpool::retain(slot) }
unsafe extern "C" fn pool_release(_: *mut c_void, slot: i32) { qpool::release(slot) }
unsafe extern "C" fn pool_refs(_: *mut c_void, slot: i32) -> u32 { qpool::refs(slot) }
unsafe extern "C" fn pool_data(_: *mut c_void, slot: i32) -> *mut u8 { qpool::data(slot) }
unsafe extern "C" fn pool_slot_size(_: *mut c_void) -> u16 { qpool::slot_size() }
unsafe extern "C" fn pool_slots(_: *mut c_void) -> u16 { qpool::slots() }
unsafe extern "C" fn pool_available(_: *mut c_void) -> u16 { qpool::available() }
static POOL_OPS: em::PoolOps = em::PoolOps {
    ctx: core::ptr::null_mut(),
    acquire: pool_acquire,
    retain: pool_retain,
    release: pool_release,
    refs: pool_refs,
    data: pool_data,
    slot_size: pool_slot_size,
    slots: pool_slots,
    available: pool_available,
};

static READY: AtomicBool = AtomicBool::new(false);

/// A module emitted an event. DMesh does not interpret module events: the LoRa packet fan-out is a stream service still to come
/// (`main_runtime::forward_lora_packet` is inert), and the module's own packets reach the node through the pool.
fn on_event(_module: &str, _id: u16, _value_type: u8, _value: &[u8]) -> i32 {
    0
}

/// Start the module runtime once after Main initializes NVS. The runtime owns
/// slot layout and installation; DMesh supplies its packet pool and settings.
pub fn ensure_loader() -> i32 {
    if READY.load(Ordering::Acquire) {
        return 0;
    }
    // The console module (a manifest that asks to run at boot) is offered with the module list and start/stop calls; it
    // starts by itself only when this boot does not give the port to the PPP bearer (`uart.mode`, main_runtime::UartMode).
    let ppp = dmesh_fw_transport::main_runtime::uart_mode_from_nvs() == dmesh_fw_transport::main_runtime::UartMode::Ppp;
    let rc = em::init("data\0", Features { modules: true, nv: Some(dmesh_nv), pool: Some(&POOL_OPS), wifi: Some(&dmesh_fw_transport::wifi_component::OWNER), ..Features::default() }, Some(on_event));
    if rc == 0 {
        READY.store(true, Ordering::Release);
        if !ppp {
            em::autostart();
        }
    }
    rc
}

/// Register the module components with `dmesh-server` before a connection is accepted. This does not initialize,
/// map, or execute a module.
pub fn register_tagged_handlers() {
    use dmesh_server::registry::{register, register_all, Method, SyncFn};
    assert!(register_all(
        &[
            Method::new(MODULE_COMPONENT, "module", MODULE_STATUS, "status"),
            Method::new(MODULE_COMPONENT, "module", MODULE_INIT, "init"),
            Method::new(MODULE_COMPONENT, "module", MODULE_STOP, "stop"),
        ],
        SyncFn(handle_module),
    ));
    assert!(register(Method::new(HELLO_COMPONENT, "hello", MODULE_RUN, "run"), SyncFn(handle_hello)));
    assert!(register(Method::new(LORA_COMPONENT, "lora", MODULE_RUN, "run"), SyncFn(handle_lora)));
    assert!(register(Method::new(HARDWARE_COMPONENT, "hardware", MODULE_RUN, "run"), SyncFn(handle_hardware)));
}

fn response(record: Record<'_>, ok: bool, value: i64) -> Option<Vec<u8>> {
    let mut result = [0u8; 24];
    let mut encoder = Encoder::new(&mut result);
    encoder.map(2)?;
    encoder.uint(1)?;
    encoder.boolean(ok)?;
    encoder.uint(2)?;
    // The loader status is an `i32`; keep failures as their absolute diagnostic code because this minimal shared
    // CBOR encoder has no signed-integer writer.
    encoder.uint(value.unsigned_abs())?;
    let used = encoder.len();
    drop(encoder);
    let mut response = vec![0; 64];
    let response_used = tagged::encode_numeric_response(
        match record.component? {
            Name::Tag(tag) => tag,
            _ => return None,
        },
        match record.method? {
            Name::Tag(tag) => tag,
            _ => return None,
        },
        record.id.unwrap_or(0),
        &result[..used],
        &mut response,
    )?;
    response.truncate(response_used);
    Some(response)
}

fn handle_module(record: Record<'_>) -> Option<Vec<u8>> {
    let (Name::Tag(MODULE_COMPONENT), Name::Tag(method)) = (record.component?, record.method?) else { return None };
    let result = match method {
        MODULE_STATUS => {
            // Value: installed slots as a bit mask, running slots in the next byte.
            let ready = READY.load(Ordering::Acquire);
            let mut mask = 0i64;
            if ready {
                for slot in 0..em::slot_count() {
                    let st = em::status(slot);
                    mask |= i64::from(st.installed) << slot;
                    mask |= i64::from(st.running) << (8 + slot);
                }
            }
            return response(record, ready, mask);
        }
        MODULE_INIT => ensure_loader(),
        // Ask every running module to return before the caller erases or replaces a slot.
        MODULE_STOP => i32::from(READY.load(Ordering::Acquire) && !em::stop_all(1_500)),
        _ => return None,
    };
    response(record, result == 0, i64::from(result))
}

fn handle_hello(record: Record<'_>) -> Option<Vec<u8>> {
    if !matches!(record.component, Some(Name::Tag(HELLO_COMPONENT))) || !matches!(record.method, Some(Name::Tag(MODULE_RUN))) {
        return None;
    }
    // hello is a one-shot module: it runs when started, with the record's data as its start request
    let init = ensure_loader();
    let result = match (init, em::find("hello")) {
        (0, Some(slot)) => em::start(slot, record.data.unwrap_or_default()),
        (0, None) => -7, // ERR_NOTFOUND
        (rc, _) => rc,
    };
    response(record, result == 0, i64::from(result))
}

/// The method number of the operation `name` of the installed module `module`, from its manifest (`{3: [{1: id, 2: name, ..}, ..]}`).
/// A module that is not installed has no operations.
fn method_in_manifest(module: &str, name: &[u8]) -> Option<i32> {
    let mut buf = vec![0u8; 2048];
    let len = em::manifest(module, &mut buf).ok().filter(|len| *len > 0)?;
    let mut d = Decoder::new(&buf[..len]);
    let (major, pairs) = d.head()?;
    if major != 5 {
        return None;
    }
    for _ in 0..pairs {
        let key = d.uint()?;
        if key != 3 {
            d.skip()?;
            continue;
        }
        let (major, methods) = d.head()?;
        if major != 4 {
            return None;
        }
        for _ in 0..methods {
            let (major, fields) = d.head()?;
            if major != 5 {
                return None;
            }
            let (mut id, mut found) = (None, false);
            for _ in 0..fields {
                match d.uint()? {
                    1 => id = i32::try_from(d.uint()?).ok(),
                    2 => found = d.text_ref()? == name,
                    _ => d.skip()?,
                }
            }
            if found {
                return id;
            }
        }
        return None;
    }
    None
}

fn handle_hardware(record: Record<'_>) -> Option<Vec<u8>> {
    module_request(record, HARDWARE_COMPONENT, "hw", HW_MODULE, |name| method_in_manifest("hw", name), false)
}

fn handle_lora(record: Record<'_>) -> Option<Vec<u8>> {
    module_request(record, LORA_COMPONENT, "lora", LORA_MODULE, |name| method_in_manifest("lora", name), true)
}

/// A tagged `run` record as one request to a module: `params` is `[operation]`, `fields` the numeric-id map of
/// settings, `data` (if `with_data`) the opaque packet. The module is started idle first when it is not running.
fn module_request(record: Record<'_>, component: u64, module: &str, module_component: i32,
                  method_of: impl Fn(&[u8]) -> Option<i32>, with_data: bool) -> Option<Vec<u8>> {
    if !matches!(record.component, Some(Name::Tag(tag)) if tag == component) || !matches!(record.method, Some(Name::Tag(MODULE_RUN))) {
        return None;
    }
    let (method, request) = module_call(record, method_of, with_data)?;
    let init = ensure_loader();
    let result = if init != 0 {
        init
    } else {
        let mut answer = [0u8; 32];
        // `idle`: mod_lora's start request that brings it up without asking it to do anything (mod_hw ignores it)
        match em::call(module, b"idle", module_component, method, &request, &mut answer) {
            Ok(len) => module_result(&answer[..len]),
            Err(rc) => rc,
        }
    };
    response(record, result == 0, i64::from(result))
}

/// What the module answered: `{1: result}`, or `{9: error}` (hw), or nothing.
fn module_result(answer: &[u8]) -> i32 {
    let mut d = Decoder::new(answer);
    let Some((5, count)) = d.head() else { return 0 };
    for _ in 0..count {
        let Some(key) = d.uint() else { return -1 };
        let Some(value) = d.int() else { return -1 };
        if key == 1 || key == 9 {
            return i32::try_from(value).unwrap_or(-1);
        }
    }
    0
}

/// The method number and request map for a record: `(method, {<field id>: integer ..., 31: data})`.
///
/// `params` is `[operation]`, `fields` the numeric-id map of board and radio settings and `data` the opaque packet. The ids
/// are the module's own (its `API.md`); the module validates them, this keeps the shape and maps the operation name to the
/// method number.
fn module_call(record: Record<'_>, method_of: impl Fn(&[u8]) -> Option<i32>, with_data: bool) -> Option<(i32, Vec<u8>)> {
    let mut params = Decoder::new(record.params?);
    let (major, count) = params.head()?;
    if major != 4 || count == 0 || count == u64::MAX {
        return None;
    }
    let operation = params.text_ref()?;
    if operation.is_empty() || operation.len() > MAX_OPERATION {
        return None;
    }
    let method = method_of(operation)?;
    for _ in 1..count {
        params.skip()?;
    }
    if !params.is_finished() {
        return None;
    }
    let mut pairs: Vec<(u64, i64)> = Vec::new();
    if let Some(fields) = record.fields {
        let mut fields = Decoder::new(fields);
        let (major, count) = fields.head()?;
        if major != 5 || count > 64 {
            return None;
        }
        for _ in 0..count {
            let key = fields.uint()?;
            if key == FIELD_DATA {
                return None;
            }
            pairs.push((key, fields.int()?));
        }
        if !fields.is_finished() {
            return None;
        }
    }
    let data = record.data.filter(|data| with_data && !data.is_empty());
    if data.is_some_and(|data| data.len() > 255) {
        return None;
    }
    let mut out = vec![0u8; 16 + pairs.len() * 12 + data.map_or(0, |d| d.len() + 4)];
    let mut encoder = Encoder::new(&mut out);
    encoder.map((pairs.len() + usize::from(data.is_some())) as u64)?;
    for (key, value) in pairs {
        encoder.uint(key)?;
        encoder.int(value)?;
    }
    if let Some(data) = data {
        encoder.uint(FIELD_DATA)?;
        encoder.bytes(data)?;
    }
    let used = encoder.len();
    drop(encoder);
    out.truncate(used);
    Some((method, out))
}

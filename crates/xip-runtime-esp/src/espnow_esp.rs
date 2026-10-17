//! ESP-NOW as the `espnow` link: DMesh's action-frame bearer (`wifi_espnow_esp.rs`), ported with its driver behaviour intact.
//!
//! Receive is the private non-promiscuous vendor-action dispatcher (`ieee80211_recv_action_register(127, 0, ...)`), which has two call
//! forms, plus the in-band reply hook of an action transmit; transmit is `esp_wifi_action_tx_req`. The frames are DMesh's
//! (`espnow_frame`). What changed is only where the datagram goes: DMesh parses into its QUIC node's pool and classifies the payload;
//! here it is parsed straight into a runtime pool packet and delivered to the subscribers of the link, or forwarded by a route. The
//! classification (NOW sync, tagged control, QUIC) is the subscribers', or a route's key. The C twin is `components/xip_runtime/src/xip_link_espnow.c`.
//!
//! Why raw action frames with the Espressif OUI, and nothing else: `components/xip_runtime/ESPNOW.md`.
//!
//! Started with the radio (`wifi_esp::start`), stopped before it.
use alloc::alloc::{alloc_zeroed, Layout};
use core::{
    ffi::{c_char, c_int, c_void},
    sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering},
};

use crate::espnow_frame as frame;
use crate::wifi_esp::{self, RadioInterface};

/// A packet the runtime owns, as `xip_link_packet` (`xip_runtime.h`).
#[repr(C)]
struct LinkPacket {
    data: *mut u8,
    len: usize,
    headroom: usize,
    slot: c_int,
}
/// A packet to fill, as `xip_link_rx_buf`.
#[repr(C)]
struct RxBuf {
    data: *mut u8,
    capacity: usize,
    slot: c_int,
}

extern "C" {
    // the early receive filter (components/xip_runtime/src/xip_rx_filter.c)
    fn xip_rx_accept(frame: *const u8, len: usize) -> bool;
    // private libnet80211 receive registration: lifecycle state, not ESP-NOW framing
    fn ieee80211_recv_action_register(
        category: u8,
        action: u8,
        callback: Option<unsafe extern "C" fn(*mut c_void, usize, *mut u8, *mut u8) -> i32>,
    ) -> i32;
    fn xip_link_native_register(name: *const c_char, mtu: u32, flags: u32, tx: Option<unsafe extern "C" fn(*mut c_void, u64, *const LinkPacket) -> c_int>, ctx: *mut c_void) -> c_int;
    fn xip_link_native_set_state(addr: c_int, state: u32, rate_kbps: u32) -> c_int;
    fn xip_link_native_ready(addr: c_int);
    fn xip_link_native_rx_begin(addr: c_int, buf: *mut RxBuf) -> c_int;
    fn xip_link_native_rx_commit(addr: c_int, buf: *const RxBuf, len: usize, source: u64, signal: i32);
    fn xip_link_native_rx_abort(buf: *const RxBuf);
}

/// Unicast frames use the Wi-Fi hardware retry path by default; broadcast records are never acknowledged in 802.11.
const NOW_ACTION_TX_WAIT_MS: u32 = 1_000;
/// A busy driver (`ESP_ERR_NO_MEM`) asks the sender to try again in this long.
const NOW_TX_RETRY_MS: u64 = 10;
/// The link's largest frame: DMesh carries 1400, the pool packet 1000.
const MTU: u32 = 1000;
/// Suppress only a near-simultaneous duplicate from the two receive callbacks; a retransmission is separated by a PTO (hundreds of ms).
const DUPLICATE_ACTION_WINDOW_MS: u32 = 20;
const FRAME_CAPACITY: usize = frame::MAX_ACTION_PAYLOAD + 96;

static LINK: AtomicI32 = AtomicI32::new(-1);
static ENABLED: AtomicBool = AtomicBool::new(false); // the private hook cannot be unregistered: stopping makes the callback inert
static MAC_ACK_ENABLED: AtomicBool = AtomicBool::new(true);
static ACTION_PARSE_BUSY: AtomicBool = AtomicBool::new(false); // the callbacks can preempt one another while they use the parser
static TX_BUSY: AtomicBool = AtomicBool::new(false); // one action request buffer: transmits are serialized
static RETRY_TIMER: AtomicPtr<esp_idf_sys::esp_timer> = AtomicPtr::new(core::ptr::null_mut());
static LAST_ACTION_FINGERPRINT: AtomicU32 = AtomicU32::new(0);
static LAST_ACTION_MS: AtomicU32 = AtomicU32::new(0);

static RX_ACTIONS: AtomicU32 = AtomicU32::new(0);
static RX_DROPS: AtomicU32 = AtomicU32::new(0);
static RX_DISPATCHER: AtomicU32 = AtomicU32::new(0);
static RX_TX_RESPONSE_HOOK: AtomicU32 = AtomicU32::new(0);
static RX_INVALID_DROPS: AtomicU32 = AtomicU32::new(0);
static RX_BUSY_DROPS: AtomicU32 = AtomicU32::new(0);
static RX_PARSE_DROPS: AtomicU32 = AtomicU32::new(0);
static RX_SELF_ECHOES: AtomicU32 = AtomicU32::new(0);
static RX_DUPLICATE_ACTIONS: AtomicU32 = AtomicU32::new(0);
static TX_ACTIONS: AtomicU32 = AtomicU32::new(0);
static TX_FAILURES: AtomicU32 = AtomicU32::new(0);
static TX_LAST_ERROR: AtomicI32 = AtomicI32::new(0);

/// Counters: (received, dropped, transmitted, transmit failures).
pub fn stats() -> (u32, u32, u32, u32) {
    (RX_ACTIONS.load(Ordering::Relaxed), RX_DROPS.load(Ordering::Relaxed), TX_ACTIONS.load(Ordering::Relaxed), TX_FAILURES.load(Ordering::Relaxed))
}
/// Runtime action-TX acknowledgement policy: only the immediate action request changes.
pub fn set_mac_ack_enabled(enabled: bool) {
    MAC_ACK_ENABLED.store(enabled, Ordering::Release);
}
pub fn last_tx_error() -> i32 {
    TX_LAST_ERROR.load(Ordering::Relaxed)
}

/// C flexible-array request storage for `esp_wifi_action_tx_req`. The SDK copies the request before returning, and the heap-owned
/// scratch avoids a per-packet allocation.
#[repr(C)]
struct ActionTxRequest {
    request: esp_idf_sys::wifi_action_tx_req_t,
    data: [u8; FRAME_CAPACITY - 24],
}
#[repr(C)]
struct ActionBuffers {
    tx_frame: [u8; FRAME_CAPACITY],
    action_tx_request: ActionTxRequest,
}
static ACTION_BUFFERS: AtomicPtr<ActionBuffers> = AtomicPtr::new(core::ptr::null_mut());

/// Allocated once before the callbacks are registered and kept across radio restarts; callbacks never allocate.
fn ensure_action_buffers() -> bool {
    if !ACTION_BUFFERS.load(Ordering::Acquire).is_null() {
        return true;
    }
    let layout = Layout::new::<ActionBuffers>();
    let allocated = unsafe { alloc_zeroed(layout).cast::<ActionBuffers>() };
    if allocated.is_null() {
        return false;
    }
    if ACTION_BUFFERS.compare_exchange(core::ptr::null_mut(), allocated, Ordering::AcqRel, Ordering::Acquire).is_err() {
        unsafe { alloc::alloc::dealloc(allocated.cast(), layout) };
    }
    true
}

fn now_ms() -> u32 {
    ((unsafe { esp_idf_sys::esp_timer_get_time() }.max(0) as u64) / 1_000) as u32
}

/// Start the link with the radio: DMesh's `start_sta_extensions` (`install_action_ingress`, then `set_now_dispatcher(true)`).
pub(crate) fn start() -> bool {
    if !ensure_action_buffers() {
        return false;
    }
    let mut addr = LINK.load(Ordering::Acquire);
    if addr < 0 {
        addr = unsafe { xip_link_native_register(c"espnow".as_ptr(), MTU, 0, Some(link_tx), core::ptr::null_mut()) };
        if addr < 0 {
            return false;
        }
        LINK.store(addr, Ordering::Release);
    }
    ENABLED.store(true, Ordering::Release);
    // registered after Wi-Fi is live: a stop/start of the driver loses it
    if unsafe { ieee80211_recv_action_register(127, 0, Some(action_rx_callback)) } != 0 {
        ENABLED.store(false, Ordering::Release);
        return false;
    }
    unsafe { xip_link_native_set_state(addr, 1, 1000) };
    true
}

/// Make the callback inert (DMesh's `stop_action_ingress` + `set_now_dispatcher(false)`).
pub(crate) fn stop() {
    ENABLED.store(false, Ordering::Release);
    let addr = LINK.load(Ordering::Acquire);
    if addr >= 0 {
        unsafe { xip_link_native_set_state(addr, 0, 0) };
    }
}

// ---- receive ----

unsafe extern "C" fn action_rx_callback(peer_context: *mut c_void, second: usize, third: *mut u8, fourth: *mut u8) -> i32 {
    if !ENABLED.load(Ordering::Acquire) || peer_context.is_null() || third.is_null() {
        return 0;
    }
    receive_registered_action_payload(peer_context, second, third, fourth);
    0
}

/// Feed the private vendor-action callback. It has two call forms: the vendor-reassembled one is `(context, length, payload)`, the
/// generic STA/AP action ingress `(interface, ieee80211_header, body_start, body_end)`. Both supply the action envelope, so both use the
/// same body parser; this is an ABI adaptation, not a raw 802.11 parser.
fn receive_registered_action_payload(peer_context: *mut c_void, second: usize, third: *mut u8, fourth: *mut u8) {
    RX_DISPATCHER.fetch_add(1, Ordering::Relaxed);
    let mut source = [0u8; 6];
    let (payload, len) = if second <= frame::MAX_ACTION_PAYLOAD {
        // `ieee80211_recv_action_vendor_esp_now`: its first context word is the source-MAC pointer and `second` the body length
        let source_ptr = unsafe { core::ptr::read_unaligned(peer_context.cast::<*const u8>()) };
        if source_ptr.is_null() {
            RX_INVALID_DROPS.fetch_add(1, Ordering::Relaxed);
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        unsafe { source.copy_from_slice(core::slice::from_raw_parts(source_ptr, 6)) };
        (third, second)
    } else {
        // generic STA/AP action ingress: `second` is the 802.11 header address, third/fourth delimit the action body. Compare the raw
        // addresses before creating a slice so a malformed native call cannot underflow or retain a driver buffer.
        let header = second as *const u8;
        let (start, end) = (third as usize, fourth as usize);
        let Some(len) = end.checked_sub(start).filter(|len| *len <= frame::MAX_ACTION_PAYLOAD) else {
            RX_INVALID_DROPS.fetch_add(1, Ordering::Relaxed);
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if header.is_null() {
            RX_INVALID_DROPS.fetch_add(1, Ordering::Relaxed);
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // the early filter, before the body is parsed
        if !unsafe { xip_rx_accept(header, 24) } {
            return;
        }
        unsafe { source.copy_from_slice(core::slice::from_raw_parts(header.add(10), 6)) };
        (third, len)
    };
    let body = unsafe { core::slice::from_raw_parts(payload, len) };
    receive_with(|output| frame::parse_action_body_into(body, output).map(|used| (source, used)));
}

/// ESP-IDF's action-transmit request may receive a co-channel response during its normal response interval: not remain-on-channel, the
/// request's own in-band reply hook, with the same parser and delivery as the dispatcher.
unsafe extern "C" fn action_tx_rx_callback(header: *mut u8, payload: *mut u8, len: usize, _channel: u8) -> i32 {
    RX_TX_RESPONSE_HOOK.fetch_add(1, Ordering::Relaxed);
    if header.is_null() || payload.is_null() || len > FRAME_CAPACITY - 24 {
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    if !unsafe { xip_rx_accept(header, 24) } {
        return 0;
    }
    if ACTION_PARSE_BUSY.swap(true, Ordering::AcqRel) {
        RX_BUSY_DROPS.fetch_add(1, Ordering::Relaxed);
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let header = unsafe { core::slice::from_raw_parts(header, 24) };
    let payload = unsafe { core::slice::from_raw_parts(payload, len) };
    let Ok(source) = <[u8; 6]>::try_from(&header[10..16]) else {
        RX_PARSE_DROPS.fetch_add(1, Ordering::Relaxed);
        ACTION_PARSE_BUSY.store(false, Ordering::Release);
        return 0;
    };
    deliver(|output| match frame::parse_action_body_into(payload, output) {
        Some(used) => Some((source, used)),
        None if payload.starts_with(&frame::ACTION_PREFIX) && payload.len() > 8 => {
            // the driver may validate and remove the vendor elements before invoking its response hook, keeping the eight-byte
            // action header: the suffix is then the complete datagram
            let packet = &payload[8..];
            if packet.len() > output.len() {
                None
            } else {
                output[..packet.len()].copy_from_slice(packet);
                Some((source, packet.len()))
            }
        }
        None => None,
    });
    ACTION_PARSE_BUSY.store(false, Ordering::Release);
    0
}

/// The common entry of the dispatcher forms: serialized against the other callback while the parser scratch is in use. Contention is a
/// counted drop; the endpoint's loss recovery retransmits.
fn receive_with<F: FnOnce(&mut [u8]) -> Option<([u8; 6], usize)>>(parse: F) {
    if ACTION_PARSE_BUSY.swap(true, Ordering::AcqRel) {
        RX_BUSY_DROPS.fetch_add(1, Ordering::Relaxed);
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    deliver(parse);
    ACTION_PARSE_BUSY.store(false, Ordering::Release);
}

/// Parse straight into a pool packet and hand it to the link (DMesh's `receive_now_with`). A locally transmitted action is not ingress,
/// and a near-simultaneous duplicate from the two callbacks must not spend two packets.
fn deliver<F: FnOnce(&mut [u8]) -> Option<([u8; 6], usize)>>(parse: F) {
    let addr = LINK.load(Ordering::Acquire);
    if addr < 0 {
        return;
    }
    let mut buf = RxBuf { data: core::ptr::null_mut(), capacity: 0, slot: -1 };
    match unsafe { xip_link_native_rx_begin(addr, &mut buf) } {
        0 => {}
        1 => return, // nobody listens
        _ => {
            RX_DROPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    let output = unsafe { core::slice::from_raw_parts_mut(buf.data, buf.capacity.min(frame::MAX_ACTION_PAYLOAD)) };
    let Some((source, used)) = parse(output) else {
        unsafe { xip_link_native_rx_abort(&buf) };
        RX_PARSE_DROPS.fetch_add(1, Ordering::Relaxed);
        RX_DROPS.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // ESP-IDF exposes a locally transmitted action to private receive paths
    if is_local_action_source(source) {
        RX_SELF_ECHOES.fetch_add(1, Ordering::Relaxed);
        unsafe { xip_link_native_rx_abort(&buf) };
        return;
    }
    if recently_seen_action(source, &output[..used]) {
        RX_DUPLICATE_ACTIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { xip_link_native_rx_abort(&buf) };
        return;
    }
    RX_ACTIONS.fetch_add(1, Ordering::Relaxed);
    unsafe { xip_link_native_rx_commit(addr, &buf, used, mac_token(source), 0) };
}

fn mac_token(mac: [u8; 6]) -> u64 {
    xip_abi::mac_token(mac)
}

fn is_local_action_source(source: [u8; 6]) -> bool {
    wifi_esp::interface_mac(RadioInterface::Sta) == Some(source) || (wifi_esp::ap_only_active() && wifi_esp::interface_mac(RadioInterface::Ap) == Some(source))
}

/// FNV-1a of the source and payload within a short window; no allocation or retained payload.
fn recently_seen_action(source: [u8; 6], payload: &[u8]) -> bool {
    let mut hash = 0x811c_9dc5u32;
    for byte in source.iter().chain(payload.iter()) {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    // zero is the uninitialized value; keep the comparison contract even for a zero FNV result
    let hash = hash.max(1);
    let now = now_ms();
    let previous_hash = LAST_ACTION_FINGERPRINT.load(Ordering::Acquire);
    let previous_ms = LAST_ACTION_MS.load(Ordering::Acquire);
    let duplicate = previous_hash == hash && now.wrapping_sub(previous_ms) <= DUPLICATE_ACTION_WINDOW_MS;
    LAST_ACTION_FINGERPRINT.store(hash, Ordering::Release);
    LAST_ACTION_MS.store(now, Ordering::Release);
    duplicate
}

// ---- transmit ----

unsafe extern "C" fn retry_ready(_: *mut c_void) {
    let addr = LINK.load(Ordering::Acquire);
    if addr >= 0 {
        unsafe { xip_link_native_ready(addr) };
    }
}

/// Ask for `READY` in `NOW_TX_RETRY_MS`.
fn schedule_ready() {
    unsafe {
        let mut timer = RETRY_TIMER.load(Ordering::Acquire);
        if timer.is_null() {
            let args = esp_idf_sys::esp_timer_create_args_t {
                callback: Some(retry_ready),
                arg: core::ptr::null_mut(),
                dispatch_method: esp_idf_sys::esp_timer_dispatch_t_ESP_TIMER_TASK,
                name: c"espnow-ready".as_ptr(),
                skip_unhandled_events: true,
            };
            if esp_idf_sys::esp_timer_create(&args, &mut timer) != esp_idf_sys::ESP_OK {
                return;
            }
            RETRY_TIMER.store(timer, Ordering::Release);
        }
        let _ = esp_idf_sys::esp_timer_stop(timer);
        let _ = esp_idf_sys::esp_timer_start_once(timer, NOW_TX_RETRY_MS * 1000);
    }
}

/// `send` to the link: DMesh's `transmit_submitted`. `dest` is a MAC token, 0 the broadcast. Returns `XIP_LINK_TX_DONE` (1) once ESP-IDF
/// accepted the request, `ERR_BUSY` (-5) when it is out of memory or a transmit is in progress (READY follows), `ERR_IO` otherwise.
unsafe extern "C" fn link_tx(_ctx: *mut c_void, dest: u64, pkt: *const LinkPacket) -> c_int {
    if !ENABLED.load(Ordering::Acquire) {
        return xip_abi::ERR_UNSUPPORTED;
    }
    let pkt = unsafe { &*pkt };
    let payload = unsafe { core::slice::from_raw_parts(pkt.data, pkt.len) };
    let destination = if dest == 0 {
        [0xff; 6]
    } else {
        let d = dest.to_le_bytes();
        [d[0], d[1], d[2], d[3], d[4], d[5]]
    };
    if TX_BUSY.swap(true, Ordering::AcqRel) {
        schedule_ready();
        return xip_abi::ERR_BUSY;
    }
    let result = transmit_submitted(destination, payload, NOW_ACTION_TX_WAIT_MS);
    TX_BUSY.store(false, Ordering::Release);
    if result == xip_abi::ERR_BUSY {
        schedule_ready();
    }
    result
}

/// ESP-IDF action submission, serialized by `TX_BUSY` so the static request is never initialized twice at once.
fn transmit_submitted(destination: [u8; 6], payload: &[u8], wait_time_ms: u32) -> c_int {
    let peer_is_broadcast = destination == [0xff; 6];
    let Some(buffers) = (!ACTION_BUFFERS.load(Ordering::Acquire).is_null()).then(|| ACTION_BUFFERS.load(Ordering::Acquire)) else {
        TX_FAILURES.fetch_add(1, Ordering::Relaxed);
        return xip_abi::ERR_IO;
    };
    let frame_buf = unsafe { &mut (*buffers).tx_frame };
    // Auto normally means the STA identity; an AP-only epoch has no associated STA peer to reply from, and a client has bound its
    // connection to the AP MAC.
    let interface = if wifi_esp::ap_only_active() { RadioInterface::Ap } else { RadioInterface::Sta };
    let local = wifi_esp::interface_mac(interface).unwrap_or([0; 6]);
    // ESP-NOW-compatible actions use broadcast A3 even when the station is associated
    let Ok(frame_len) = frame::encode_action_frame(frame_buf, destination, local, [0xff; 6], payload) else {
        TX_FAILURES.fetch_add(1, Ordering::Relaxed);
        return xip_abi::ERR_SIZE;
    };
    let action = &frame_buf[24..frame_len];
    let sent = unsafe {
        let request = &mut (*buffers).action_tx_request;
        core::ptr::write_bytes(request as *mut ActionTxRequest, 0, 1);
        request.request.ifx = wifi_esp::interface_id(interface);
        request.request.dest_mac = destination;
        request.request.type_ = esp_idf_sys::wifi_action_tx_t_WIFI_OFFCHAN_TX_REQ;
        let Some((channel, secondary)) = wifi_esp::current_channel() else {
            TX_FAILURES.fetch_add(1, Ordering::Relaxed);
            return xip_abi::ERR_IO;
        };
        request.request.channel = channel;
        request.request.sec_channel = secondary;
        request.request.wait_time_ms = wait_time_ms;
        // a direct packet has one concrete receiver, so use the normal 802.11 ACK and retry; group-addressed records cannot be ACKed
        request.request.no_ack = peer_is_broadcast || !MAC_ACK_ENABLED.load(Ordering::Acquire);
        request.request.rx_cb = Some(action_tx_rx_callback);
        request.request.bssid = [0xff; 6];
        request.request.data_len = action.len() as u32;
        request.data[..action.len()].copy_from_slice(action);
        esp_idf_sys::esp_wifi_action_tx_req(&mut request.request)
    };
    if sent == esp_idf_sys::ESP_OK {
        TX_LAST_ERROR.store(0, Ordering::Relaxed);
        TX_ACTIONS.fetch_add(1, Ordering::Relaxed);
        1 // XIP_LINK_TX_DONE
    } else if sent == esp_idf_sys::ESP_ERR_NO_MEM {
        TX_LAST_ERROR.store(sent, Ordering::Relaxed);
        TX_FAILURES.fetch_add(1, Ordering::Relaxed);
        xip_abi::ERR_BUSY
    } else {
        TX_LAST_ERROR.store(sent, Ordering::Relaxed);
        TX_FAILURES.fetch_add(1, Ordering::Relaxed);
        xip_abi::ERR_IO
    }
}

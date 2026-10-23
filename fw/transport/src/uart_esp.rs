// QUIC pool/consumer glue. C owns UART/USB, framing, the one current TX
// frame, RX commit/abort, cancellation, baud and activity. Rust supplies typed
// pool callbacks and translates completion into the QUIC submission API.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use dmesh_server::firmware_profile::{UART_DEFAULT, UART_OFF};
use quic_lite::{
    EgressSubmission, PacketEgress, PacketSendOutcome, PacketSubmitError, PacketWriter,
    PeerL2Address,
};
use xip_abi::{SerialPacketClient, SerialPacketTx, SerialRxBuffer};

/// UART is an L2 bearer and therefore uses the transport MTU rather than a
/// separate 512-byte diagnostic limit. PPP escaping is framing overhead, not
/// an additional fragmentation layer.
pub const UART_MAX_PACKET: usize = crate::TRANSPORT_MTU + 1;

// The image's physical UART rate is intentionally a build-time choice:
// switching it at runtime would strand a direct UART client before it could
// receive an acknowledgement. C6 selects USB-JTAG only when a host is
// physically attached at boot; otherwise it uses the board's UART0 bridge.
include!(concat!(env!("OUT_DIR"), "/physical_uart_baud.rs"));

// Same escaped-record sizing as the previous DMesh driver; the C runtime
// accepts the bearer limit explicitly rather than substituting its module MTU.
const USB_JTAG_BUFFER_SIZE: usize = 2 * (2 * UART_MAX_PACKET + 2);

unsafe extern "C" {
    fn xip_serial_open(baud: i32, usb_buffer_size: usize) -> i32;
    fn xip_serial_start_packets(client: *const SerialPacketClient) -> i32;
    fn xip_serial_submit_packet(source: *mut c_void, take: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut SerialPacketTx)) -> i32;
    fn xip_serial_receive_enabled(enabled: bool);
    fn xip_serial_tx_available() -> bool;
    fn xip_serial_usb_selected() -> bool;
    fn xip_serial_rearm();
    fn xip_serial_request_baud(baud: u32) -> i32;
    fn xip_serial_activity_window(ms: u32);
    fn xip_serial_activate();
    fn xip_power_activate(ms: u32);
    fn xip_power_clear_activity();
    fn xip_power_active_remaining_ms() -> u32;
    fn xip_serial_buffered_bytes(bytes: *mut usize) -> i32;
}

#[cfg(target_arch = "riscv32")]
static C6_USB_JTAG_SELECTED: AtomicBool = AtomicBool::new(false);

static UART_RX_EVENT_COUNT: AtomicU32 = AtomicU32::new(0);
static UART_RX_BYTE_COUNT: AtomicU32 = AtomicU32::new(0);
static UART_RX_PACKET_COUNT: AtomicU32 = AtomicU32::new(0);
static UART_RX_WAKE_COUNT: AtomicU32 = AtomicU32::new(0);
static UART_RX_OVERRUN_COUNT: AtomicU32 = AtomicU32::new(0);

/// Private driver diagnostics; monotonic counters, not application completion.
/// The final legacy control-queue-depth item is zero; C owns control state.
pub(crate) fn receive_diagnostics() -> [u32; 7] {
    [
        UART_RX_BYTE_COUNT.load(Ordering::Relaxed),
        UART_RX_EVENT_COUNT.load(Ordering::Relaxed),
        UART_RX_PACKET_COUNT.load(Ordering::Relaxed),
        UART_RX_WAKE_COUNT.load(Ordering::Relaxed),
        UART_RX_OVERRUN_COUNT.load(Ordering::Relaxed),
        0, // legacy Rust TX-slot state: C owns the current frame
        0, // legacy control-queue depth: C now owns scalar control state
    ]
}

/// Read-only task-context observation of bytes already in the driver's RX
/// ring. This does not consume bytes or create a second UART reader.
pub(crate) fn buffered_receive_bytes() -> Option<u32> {
    #[cfg(target_arch = "riscv32")]
    if C6_USB_JTAG_SELECTED.load(Ordering::Acquire) {
        return None;
    }
    let mut bytes = 0usize;
    let result = unsafe { xip_serial_buffered_bytes(&mut bytes) };
    (result == 0).then_some(bytes.min(u32::MAX as usize) as u32)
}
/// Optional firmware-owner wake hook. The L2 task has no dependency on Main,
/// Recovery, commands, or a particular executor; an owner can install this
/// one-shot notification so its dispatcher need not wait for a housekeeping
/// timeout after an ingress queue transition.
static INGRESS_NOTIFY: AtomicUsize = AtomicUsize::new(0);
/// Optional task-context notification after the single QUIC pool submission
/// slot is released. It carries no packet; the shared owner retries its own
/// retained stream state after this writable edge.
static EGRESS_NOTIFY: AtomicUsize = AtomicUsize::new(0);

type UartPoolSubmission = EgressSubmission<crate::shared_ingress_esp::SharedPacketLease>;
// QUIC's typed completion token crosses the C callback boundary. The C
// admission slot serializes tx access; it calls complete before admitting the
// next frame. RX and TX fields are independent, as their C tasks are concurrent.
static UART_SEND_BLOCKED: AtomicBool = AtomicBool::new(false);
type RxWriter = <crate::quic_node_esp::FirmwarePool as quic_lite::PacketPool>::Writer;
struct PoolAdapter {
    rx: UnsafeCell<Option<RxWriter>>,
    tx: UnsafeCell<Option<UartPoolSubmission>>,
}

static UART_ALWAYS_ON: AtomicBool = AtomicBool::new(false);
static UART_DEBUG_ENABLED: AtomicBool = AtomicBool::new(true);
pub const COMMAND_GRACE_TICKS: u32 = 8000;

/// Convert the compact transport.start selector to a physical baud rate.
/// Zero is the product-default spelling for 115200; `UART_OFF` has no baud.
/// USB-JTAG builds never call this mapping because their bearer is packetized,
/// not serial.
pub const fn baud_from_selector(selector: u8) -> Option<i32> {
    match selector {
        UART_DEFAULT | 1 => Some(115_200),
        2 => Some(9_600),
        3 => Some(19_200),
        4 => Some(38_400),
        5 => Some(57_600),
        6 => Some(230_400),
        7 => Some(460_800),
        9 => Some(921_600),
        UART_OFF => None,
        _ => None,
    }
}

/// Whether this profile explicitly disables a real UART bearer. USB-JTAG is
/// intentionally not governed by this predicate so hardware debug remains
/// usable when a radio-only profile asks to turn UART off.
pub const fn uart_is_off(selector: u8) -> bool {
    selector == UART_OFF
}

/// Whether this boot selected the packetized native USB-JTAG endpoint rather
/// than a power-gated physical UART.
pub fn packetized_debug_selected() -> bool {
    #[cfg(target_arch = "riscv32")]
    {
        return C6_USB_JTAG_SELECTED.load(Ordering::Acquire);
    }
    #[cfg(not(target_arch = "riscv32"))]
    {
        false
    }
}

/// Install and configure the physical console bearer. This is deliberately
/// the sole ESP UART0/USB-JTAG setup path used by both Main and Recovery.
/// Callers provide only higher-layer dispatch policy; they must never create
/// a competing UART driver, reader, writer, or RX event queue.
pub unsafe fn install_l2_driver(baud_selector: u8) -> bool {
    let Some(baud) = baud_from_selector(baud_selector) else {
        return false;
    };
    install_l2_driver_baud(baud)
}

/// Install with a reviewed physical rate; native USB-JTAG ignores the rate.
pub unsafe fn install_l2_driver_baud(baud: i32) -> bool {
    // open quiesces and releases the runtime's early module-PPP reader before
    // replacing its driver. There must never be competing serial consumers.
    let result = xip_serial_open(baud, USB_JTAG_BUFFER_SIZE);
    #[cfg(target_arch = "riscv32")]
    C6_USB_JTAG_SELECTED.store(result == 0 && xip_serial_usb_selected(), Ordering::Release);
    result == 0
}

static UART_STARTED: AtomicBool = AtomicBool::new(false);
static UART_CLIENT: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

pub unsafe fn start_shared_l2() -> bool {
    if UART_STARTED.load(Ordering::Acquire) { return true; }
    let _ = crate::quic_node_esp::uart_pool(); // registered bearer/pool must exist
    let adapter = Box::into_raw(Box::new(PoolAdapter {
        rx: UnsafeCell::new(None), tx: UnsafeCell::new(None),
    }));
    let client = SerialPacketClient { ctx: adapter.cast(), rx_begin: Some(pool_begin),
        rx_finish: Some(pool_finish), event: Some(serial_event), close: Some(pool_close) };
    if xip_serial_start_packets(&client) != 0 { return false; }
    UART_CLIENT.store(adapter.cast(), Ordering::Release);
    UART_STARTED.store(true, Ordering::Release);
    let _ = send_debug_text(b"DMESH uart: C packet bearer ready");
    crate::commands::send_response(b"DMESH uart: quic ready");
    true
}

unsafe extern "C" fn pool_begin(ctx: *mut c_void, port: u32, out: *mut SerialRxBuffer) -> i32 {
    if port != 0xf7 { return 1; }
    let adapter = &*ctx.cast::<PoolAdapter>();
    assert!((*adapter.rx.get()).is_none());
    let Some(mut writer) = crate::quic_node_esp::acquire_udp_receive_writer() else { return -3 };
    let payload = writer.payload_mut();
    *out = SerialRxBuffer { data: payload.as_mut_ptr(), capacity: payload.len().min(crate::TRANSPORT_MTU) };
    *adapter.rx.get() = Some(writer);
    0
}

unsafe extern "C" fn pool_finish(ctx: *mut c_void, _port: u32, len: usize, outcome: i32) {
    let writer = (&mut *(&*ctx.cast::<PoolAdapter>()).rx.get()).take().expect("C owns one RX writer");
    if outcome == 0 {
        if let Some(packet) = writer.commit(len) {
            crate::quic_node_esp::receive_uart(packet);
            notify_ingress();
        }
    } // Error drops the exact uncommitted writer back to its pool.
}

unsafe extern "C" fn serial_event(_ctx: *mut c_void, event: u32, value: u32) {
    match event {
        1 => { UART_RX_BYTE_COUNT.fetch_add(value, Ordering::Relaxed); }
        2 => { UART_RX_EVENT_COUNT.fetch_add(value, Ordering::Relaxed); }
        3 => { UART_RX_OVERRUN_COUNT.fetch_add(value, Ordering::Relaxed); }
        4 => { UART_RX_PACKET_COUNT.fetch_add(value, Ordering::Relaxed); }
        5 => {
            UART_RX_WAKE_COUNT.fetch_add(value, Ordering::Relaxed);
            UART_DEBUG_ENABLED.store(true, Ordering::Release);
            crate::main_runtime::request_uart_wake();
        }
        6 => { crate::shared_ingress_esp::record_pool_drop(); }
        7 => notify_egress_ready(),
        _ => {}
    }
}

unsafe extern "C" fn pool_close(ctx: *mut c_void) {
    let adapter = Box::from_raw(ctx.cast::<PoolAdapter>());
    // C has committed/aborted RX and completed/cancelled TX before close.
    assert!((*adapter.rx.get()).is_none());
    assert!((*adapter.tx.get()).is_none());
    if UART_CLIENT.compare_exchange(ctx, core::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire).is_ok() {
        UART_SEND_BLOCKED.store(false, Ordering::Release);
        UART_STARTED.store(false, Ordering::Release);
    }
}

// Synchronous ownership move, called by C only after reserving its single TX
// slot. A busy/error result never calls this and leaves source unchanged.
unsafe extern "C" fn take_packet(ctx: *mut c_void, source: *mut c_void, out: *mut SerialPacketTx) {
    let submission = (&mut *source.cast::<Option<UartPoolSubmission>>()).take().expect("caller owns packet");
    let bytes = submission.packet().bytes();
    *out = SerialPacketTx { data: bytes.as_ptr(), len: bytes.len(), port: 0xf7,
        owner: ctx, complete: Some(complete_packet) };
    let slot = &mut *(&*ctx.cast::<PoolAdapter>()).tx.get();
    assert!(slot.is_none());
    *slot = Some(submission);
}

unsafe extern "C" fn complete_packet(ctx: *mut c_void, outcome: i32) {
    let submission = (&mut *(&*ctx.cast::<PoolAdapter>()).tx.get()).take().expect("C owns current TX");
    submission.complete(if outcome == 0 { PacketSendOutcome::Sent } else { PacketSendOutcome::Failed }, 0);
}

// The product power owner controls sleep; this adapter acquires no PM locks.
/// Configure the bounded interactive-console interval. Settings ownership is
/// intentionally above this adapter; the policy only receives validated ms.
pub fn configure_active_window(window_ms: u32) {
    unsafe { xip_serial_activity_window(window_ms) };
}

pub fn activate_window() {
    // Incoming bytes and explicit owner activity share the C deadline.
    if UART_DEBUG_ENABLED.load(Ordering::Acquire) {
        unsafe { xip_serial_activate() };
    }
}

pub fn activate_window_for(window_ms: u32) {
    UART_DEBUG_ENABLED.store(true, Ordering::Release);
    unsafe { xip_serial_receive_enabled(true) };
    unsafe { xip_power_activate(window_ms.max(1)) };
}

pub fn is_active() -> bool {
    UART_DEBUG_ENABLED.load(Ordering::Acquire)
        && (UART_ALWAYS_ON.load(Ordering::Acquire) || active_window_open())
}

pub fn interactive_active() -> bool {
    UART_DEBUG_ENABLED.load(Ordering::Acquire)
        && !UART_ALWAYS_ON.load(Ordering::Acquire)
        && active_window_open()
}

/// False when `uart.mode` leaves the port to the module runtime's serial driver (main_runtime::UartMode): the calls below
/// that act on the port or its power locks then do nothing.
static UART_OURS: AtomicBool = AtomicBool::new(true);

pub fn set_ours(ours: bool) {
    UART_OURS.store(ours, Ordering::Release);
}

pub fn set_always_on(enabled: bool) {
    if !UART_OURS.load(Ordering::Acquire) {
        return;
    }
    UART_ALWAYS_ON.store(enabled, Ordering::Release);
    if enabled {
        UART_DEBUG_ENABLED.store(true, Ordering::Release);
        unsafe { xip_serial_receive_enabled(true) };
        // Always-on enables the bearer, without acquiring a PM lock.
    } else {
        unsafe { xip_power_clear_activity() };
    }
}

pub fn set_debug_enabled(enabled: bool) {
    if !UART_OURS.load(Ordering::Acquire) {
        return;
    }
    UART_DEBUG_ENABLED.store(enabled, Ordering::Release);
    unsafe { xip_serial_receive_enabled(enabled) };
    if enabled {
        activate_window();
    } else {
        unsafe { xip_power_clear_activity() };
    }
}

pub fn suspend_for_light_sleep() {
    // No adapter PM locks exist. Driver wake rearming remains paired below.
}

/// Re-arm the physical UART receive wake after a light-sleep return.
///
/// ESP-IDF can leave the receive interrupt disabled after a GPIO wake even
/// though the driver and the shared L2 task remain installed.  This belongs
/// with that driver owner, not with a Main command or battery-policy module.
pub fn rearm_after_wake() {
    if !UART_OURS.load(Ordering::Acquire) {
        return;
    }
    unsafe { xip_serial_rearm() };
    if UART_DEBUG_ENABLED.load(Ordering::Acquire) {
        activate_window();
    }
}

fn active_window_open() -> bool { unsafe { xip_power_active_remaining_ms() != 0 } }

/// Serialize a volatile physical-rate change through the sole writer.
/// Native USB-JTAG is unaffected. The transport response finishes first.
pub(crate) fn request_baud_change(baud: u32) {
    if physical_uart_selected() { unsafe { let _ = xip_serial_request_baud(baud); } }
}

/// Thin QUIC adapter. C admits and owns the one current packet; callbacks
/// translate the typed pool/QUIC token, without framing or a Rust TX queue.
pub struct EspUartBearer;

impl PacketEgress<crate::shared_ingress_esp::SharedPacketLease> for EspUartBearer {
    fn submit(
        &mut self,
        _peer_l2_address: PeerL2Address,
        submission: UartPoolSubmission,
    ) -> Result<(), PacketSubmitError<crate::shared_ingress_esp::SharedPacketLease>> {
        if !physical_bearer_active() {
            return Err(PacketSubmitError::WouldBlock(submission));
        }
        let mut packet = Some(submission);
        let rc = unsafe { xip_serial_submit_packet((&mut packet as *mut Option<UartPoolSubmission>).cast(), take_packet) };
        if rc == 0 {
            debug_assert!(packet.is_none());
            Ok(())
        } else {
            UART_SEND_BLOCKED.store(true, Ordering::Release);
            // Completion may race this busy result. Recheck through a harmless
            // owner notification; the shared node retains and retries its lease.
            if unsafe { xip_serial_tx_available() } { notify_egress_ready(); }
            Err(PacketSubmitError::WouldBlock(packet.expect("busy did not transfer ownership")))
        }
    }
}

/// Install or clear a task-context notification callback for newly accepted
/// UART ingress. The callback must be nonblocking and safe from the dedicated
/// FreeRTOS UART task; it is not an interrupt callback.
pub fn set_ingress_notify(callback: Option<fn()>) {
    let callback = callback.map_or(0, |callback| callback as usize);
    INGRESS_NOTIFY.store(callback, Ordering::Release);
}

/// Install or clear the task-context notification for a QUIC bearer slot
/// released after physical transmission. The callback never runs in an ISR.
pub fn set_egress_notify(callback: Option<fn()>) {
    let callback = callback.map_or(0, |callback| callback as usize);
    EGRESS_NOTIFY.store(callback, Ordering::Release);
}

/// Consume the one-shot QUIC readiness edge recorded after an egress submit
/// returned `WouldBlock`. Successful packet completion alone is not a new
/// readiness event; its independent completion notification still wakes the
/// QUIC owner to release the packet lease.
pub(crate) fn take_send_blocked() -> bool {
    UART_SEND_BLOCKED.swap(false, Ordering::AcqRel)
}

fn notify_ingress() {
    let callback = INGRESS_NOTIFY.load(Ordering::Acquire);
    if callback != 0 {
        // Function pointers are stored only by `set_ingress_notify` above.
        // A function pointer is non-null and fits in usize on supported ESP
        // targets; no callback owns or aliases UART queue memory.
        let callback: fn() = unsafe { core::mem::transmute(callback) };
        callback();
    }
}

fn notify_egress_ready() {
    let callback = EGRESS_NOTIFY.load(Ordering::Acquire);
    if callback != 0 {
        // The matching QUIC submission has completed and its physical bearer
        // slot is free. Packet production remains serialized on shared_ingress_esp.
        let callback: fn() = unsafe { core::mem::transmute(callback) };
        callback();
    }
}

/// Record one diagnostic in the QUIC log history. Hardware ROM and panic output
/// bypass this function and remain visible after a crash.
pub fn send_debug_text(text: &[u8]) -> bool {
    if text.is_empty() || text.len().saturating_add(2) > UART_MAX_PACKET || !text.is_ascii() {
        return false;
    }
    crate::log_history::push(text)
}

/// C/ESP-IDF log bridge. Normal records go to retained `log.watch` history;
/// the physical UART remains available for boot and crash diagnostics only.
#[no_mangle]
pub unsafe extern "C" fn dmesh_uart_log_line(bytes: *const u8, len: usize) -> i32 {
    if bytes.is_null() || len == 0 || len > 255 {
        return -1;
    }
    let line = core::slice::from_raw_parts(bytes, len);
    crate::log_history::push(line);
    len as i32
}

fn physical_uart_selected() -> bool {
    #[cfg(target_arch = "riscv32")]
    {
        !C6_USB_JTAG_SELECTED.load(Ordering::Acquire)
    }
    #[cfg(not(target_arch = "riscv32"))]
    {
        true
    }
}

fn physical_bearer_active() -> bool {
    #[cfg(target_arch = "riscv32")]
    if C6_USB_JTAG_SELECTED.load(Ordering::Acquire) {
        return true;
    }
    is_active()
}

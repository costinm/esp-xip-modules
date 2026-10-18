#![allow(clippy::too_many_arguments)]

use core::ffi::c_void;

pub const ABI_VERSION: u32 = 2;

/// Success.
pub const OK: i32 = 0;
/// Invalid argument.
pub const ERR_ARG: i32 = -1;
/// The host does not implement this call (or the pin or bus is not allowed).
pub const ERR_UNSUPPORTED: i32 = -2;
/// The peripheral failed.
pub const ERR_IO: i32 = -3;
/// A bounded wait expired.
pub const ERR_TIMEOUT: i32 = -4;
/// The resource is in use (pin owned by another module, no free handle, previous request not yet dispatched).
pub const ERR_BUSY: i32 = -5;
/// Out of memory.
pub const ERR_NOMEM: i32 = -6;
/// No such setting, handle or service.
pub const ERR_NOTFOUND: i32 = -7;
/// The buffer is too small (`*len` says how much is needed).
pub const ERR_SIZE: i32 = -8;
/// A module ended holding packets (the host gets them back) or memory (`mem_alloc`: not recoverable, the slot needs a reboot to be replaced) it had not given back. The stop is reported as failed.
pub const ERR_LEAK: i32 = -9;
/// A nonblocking socket operation would block; wait for another readiness event.
pub const ERR_AGAIN: i32 = -10;

// HOST: Methods of component 0, which only the host uses. A module's own components are 1 and up.
/// The host's component number.
pub const HOST_COMPONENT: u32 = 0;
/// Once, after the instance is created. `data` is the start request (may be empty).
pub const HOST_START: u32 = 1;
/// Main asks the module to end. The instance is destroyed after the call; release what is not released by that (a radio left transmitting).
pub const HOST_STOP: u32 = 2;
/// Connectivity changed; `value` is 1 when main has a network path, else 0. Delivered to every running module (a handler that has nothing to do returns at once).
pub const HOST_NETWORK: u32 = 3;
/// A wake window opened. Only if the module asked with an alarm using `ALARM_WINDOW`. `value` is why the window opened: 0 for a window of the application,
/// or, when a `power.sleep` cycle just ended, the wake cause (`esp_sleep_wakeup_cause_t`: 4 timer, 7 GPIO, 8 UART; a soft sleep reports 4). A cause other than the
/// timer is a person or a peer waking the device early: the runtime drops the remaining cycles of that request, and the module decides what to do (stay awake, go active).
pub const HOST_WAKE: u32 = 4;
/// The intended Wi-Fi state or the tuning changed (`wifi` and `wifi-tuning` components); `value` is the new generation. Call `get` to read it again, `state` to see what is true now. Delivered to every running module.
pub const HOST_WIFI: u32 = 5;

// RET: Return values of a handler.
/// Keep the instance.
pub const RET_CONTINUE: u32 = 0;
/// The module is finished; destroy the instance.
pub const RET_DONE: u32 = 1;

// VALUE: Value types of `emit`.
/// No payload.
pub const VALUE_EMPTY: u32 = 0;
/// 8 bytes, little endian.
pub const VALUE_U64: u32 = 1;
/// 8 bytes, little endian.
pub const VALUE_I64: u32 = 2;
/// Opaque bytes.
pub const VALUE_BYTES: u32 = 3;
/// UTF-8 text.
pub const VALUE_TEXT: u32 = 4;
/// One CBOR item.
pub const VALUE_CBOR: u32 = 5;

// GPIO: `gpio_config` modes and pulls.
pub const GPIO_INPUT: u32 = 0;
pub const GPIO_OUTPUT: u32 = 1;
pub const GPIO_INPUT_OUTPUT: u32 = 2;
pub const GPIO_PULL_NONE: u32 = 0;
pub const GPIO_PULL_UP: u32 = 1;
pub const GPIO_PULL_DOWN: u32 = 2;

// IRQ: `irq_register` edges.
pub const IRQ_FALLING: u32 = 1;
pub const IRQ_RISING: u32 = 2;
pub const IRQ_BOTH: u32 = 3;

// ALARM: Flags of `alarm_set`.
/// Fire at the delay, waking the system if it has to.
pub const ALARM_EXACT: u32 = 0;
/// May fire anywhere in `[delay, delay + window]`; the host prefers the next wake window inside that range and fires at the end of the range at the latest.
pub const ALARM_FLEXIBLE: u32 = 1;
/// Do not fire while the network is down (`HOST_NETWORK` 0, `wifi.state`; JobScheduler's network constraint). The alarm becomes due when connectivity returns and fires in the next wake window.
pub const ALARM_CONNECTED: u32 = 2;
/// Also deliver `HOST_WAKE` whenever a wake window opens (a module that batches its work on windows).
pub const ALARM_WINDOW: u32 = 4;

// PKT: Packets: the unit of every message between modules, and between a module and the application. A packet is a fixed buffer from the host's pool with room in front of the payload for whoever frames it next (a network header, an envelope) and behind it for a tag; a module writes only the payload window. The numbers are the contract of every host, whatever its pool (the C pool of this repository, QUIC-lite's pool in a Rust host): see `docs/packets.md`.
/// Bytes in front of the payload window: 80 for link and relay headers, 13 short header, 3 frame header, 36 tagged-CBOR envelope, rounded up to 16.
pub const PKT_HEADROOM: u32 = 144;
/// Largest payload a module can write or send. Larger records are refused with `ERR_SIZE`.
pub const PKT_MAX: u32 = 1000;
/// Smallest pool slot that holds `HEADROOM`, `MAX` and a 60-byte tail (AEAD tag, FCS).
pub const PKT_SLOT_MIN: u32 = 1204;

// ADDR: Where a packet is sent. A module is addressed by its component (the slot tag), which is also the `from` of what it sends.
/// The application (its packet hook). As `from`: the message came from the application, or the sender cannot be answered.
pub const ADDR_MAIN: u32 = 0;
/// The first address of a link (`link` component): `ADDR_LINK + n`. Frames arrive `from` it, and `send` to it transmits. It is below 2^31 so that
/// `link.open` can return an address like a result, positive, and an error stays negative. Module addresses (component numbers) are below it.
pub const ADDR_LINK: u32 = 0x4000_0000;

// SOCKET_MODE: `SOCKET_MODE`
/// Readiness callback; the module calls nonblocking `socket_read` or `socket_write`.
pub const SOCKET_MODE_DIRECT: u32 = 0;
/// The runtime reads directly into pool packets and queues one packet per callback.
pub const SOCKET_MODE_BUFFERED: u32 = 1;

// SOCKET_EVENT: `SOCKET_EVENT`
/// Accepted socket; `call.id` is its handle, `call.pkt` and `call.len` are zero.
pub const SOCKET_EVENT_OPEN: u32 = 1;
/// Direct mode: `socket_read` may make progress.
pub const SOCKET_EVENT_READABLE: u32 = 2;
/// A direct write may progress or a refused packet write may be retried.
pub const SOCKET_EVENT_WRITABLE: u32 = 3;
/// Buffered mode: `call.pkt` holds bytes from one TCP read; TCP message boundaries are not implied.
pub const SOCKET_EVENT_DATA: u32 = 4;
/// Peer closed its write half; distinct from the zero-length OPEN event.
pub const SOCKET_EVENT_EOF: u32 = 5;
/// Socket closed or errored; handle no longer valid after callback.
pub const SOCKET_EVENT_CLOSED: u32 = 6;
/// A previously accepted packet has been fully submitted to the socket.
pub const SOCKET_EVENT_WRITE_DONE: u32 = 7;

// DMOD: Module image header.
/// 'DMOD' little endian.
pub const DMOD_MAGIC: u32 = 0x444f4d44;
pub const DMOD_HEADER_SIZE: u32 = 64;
/// Linked at code_vma/data_vma (Xtensa); otherwise position independent (RISC-V).
pub const DMOD_FLAG_FIXED_VMA: u32 = 1;

/// argument of `spi_open`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SpiConfig {
    /// `sizeof(xip_spi_config)`, set by the caller; lets the struct grow.
    pub size: u32,
    /// SPI controller: 2 or 3 (3 does not exist on ESP32-C6, 2 is used).
    pub bus: i32,
    pub sck: i32,
    pub miso: i32,
    pub mosi: i32,
    pub cs: i32,
    /// Clock; 0 = 1 MHz.
    pub hz: u32,
    /// SPI mode 0..3.
    pub mode: i32,
    /// Optional busy/ready line (SX126x BUSY), -1 = none. The host waits for `ready_level` before and after each transfer.
    pub ready_pin: i32,
    /// Level that means ready (0 for BUSY).
    pub ready_level: i32,
    /// Longest wait for ready; 0 = 100.
    pub ready_timeout_ms: u32,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<SpiConfig>() == 44);

/// what a handler is called with
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Call {
    /// `sizeof(xip_call)`.
    pub size: u32,
    /// Chosen by the creator of the event (0 = the host, see `HOST`).
    pub component: i32,
    /// Chosen by the creator of the event.
    pub method: i32,
    /// IRQ: the pin level. Alarm: how late it fired, in ms (saturated). `HOST_NETWORK`: 1 or 0. Request: 0. A frame of a link: its signal (`link` component).
    pub value: i32,
    /// Bit 0: the call is a request that expects an answer (`respond`, or `send` to `from` with the same `id`). Bit 1 (`CALL_LINK`): a frame of a link, `from` is its address and `id` an address token, not a request. Bit 2 (`CALL_LINK_EVENT`): an event of a link, its kind in bits 8..15.
    pub flags: u32,
    /// Payload, valid during the call only.
    pub data: *const u8,
    /// Length of `data`.
    pub len: usize,
    /// The packet `data` lives in, lent for this call (0: none, as for alarms and interrupts). Keep it with `pkt_keep`, pass it on with `send`; otherwise it goes back to the pool when the handler returns.
    pub pkt: u32,
    /// Who sent it: a component (`ADDR_MAIN` = 0: the application). Answer with `send` to this address.
    pub from: u32,
    /// Frames and events of a link: the low 32 bits of the microsecond clock when the radio received the frame, or how long a
    /// sent frame took (`LINK_EVENT_TX_DONE`). Otherwise zero. Also keeps `id` aligned the same on every target.
    pub time_us: u32,
    /// The sender's correlation number, returned in the answer.
    pub id: u64,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Call>() == 48);

/// first 64 bytes of a module image
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DmodHeader {
    /// `DMOD_MAGIC`.
    pub magic: u32,
    /// `ABI_VERSION` the module was built for.
    pub abi_version: u16,
    /// `DMOD_HEADER_SIZE`.
    pub header_size: u16,
    /// Offset of `xip_handle` from the start of the image (includes the header).
    pub entry_offset: u32,
    /// Header plus code and read-only data.
    pub image_size: u32,
    /// Module tag; must match the slot the image is installed in.
    pub tag: u16,
    /// Zero.
    pub reserved0: u16,
    /// FIXED_VMA: link address of the image start.
    pub code_vma: u32,
    /// FIXED_VMA: data-bus alias of the read-only data.
    pub data_vma: u32,
    /// Stack the deepest handler needs; the worker uses the largest of its modules. 0 = host default.
    pub stack_bytes: u32,
    /// Reserved, zero: it was a set of host feature bits. What a module uses from the host is found at run time with `get_component`.
    pub required_features: u32,
    /// `DMOD_FLAG_*`.
    pub flags: u32,
    /// Size of the zeroed state block passed to every call (at most 4096).
    pub state_bytes: u32,
    /// Offset from the start of the image of the module's manifest (a multiple of 4, after the code); 0: none.
    pub manifest_offset: u32,
    /// Its length.
    pub manifest_size: u32,
    /// Zero.
    pub reserved: [u8; 12],
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<DmodHeader>() == 64);

/// The host table, `xip_host`: every call a module can make, each documented below. The leading `user` argument of every call is `host->user`. Slots are `None` only if a host leaves one empty; slots past `size` do not exist.
#[repr(C)]
pub struct Host {
    pub abi_version: u32,
    pub size: u32,
    pub user: *mut c_void,
    /// Write one line to the host's log.
    ///
    /// TODO: replace with structured logging from dmesh after we add the 'packet pool': get the current out buffer, append CBOR tags and values instead of strings.
    ///
    /// The host truncates the line and adds no newline. A module's log is its only way to explain a failure.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, or `ERR_SIZE` if the line was truncated.
    ///
    /// # Arguments
    ///
    /// * `data` (`const uint8_t *`): Text.
    /// * `len` (`size_t`): Length.
    pub log: Option<unsafe extern "C" fn(user: *mut c_void, data: *const u8, len: usize) -> core::ffi::c_int>,
    /// Monotonic milliseconds since boot.
    ///
    /// # Returns
    ///
    /// `uint64_t` — the time.
    pub now_ms: Option<unsafe extern "C" fn(user: *mut c_void) -> u64>,
    /// Read a persistent setting of this module.
    ///
    /// Own namespace; the key is up to 15 bytes, the value up to 256.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_NOTFOUND` if absent, `ERR_SIZE` if the buffer is small.
    ///
    /// # Arguments
    ///
    /// * `key` (`const uint8_t *`): Name.
    /// * `key_len` (`size_t`): Length.
    /// * `value` (`uint8_t *`): Buffer.
    /// * `capacity` (`size_t`): Its size.
    /// * `value_len` (`size_t *`): Value length.
    pub get_setting: Option<unsafe extern "C" fn(user: *mut c_void, key: *const u8, key_len: usize, value: *mut u8, capacity: usize, value_len: *mut usize) -> core::ffi::c_int>,
    /// Write a persistent setting of this module.
    ///
    /// An empty value erases the setting.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or an error.
    ///
    /// # Arguments
    ///
    /// * `key` (`const uint8_t *`): Name.
    /// * `key_len` (`size_t`): Length.
    /// * `value` (`const uint8_t *`): Value.
    /// * `value_len` (`size_t`): Length.
    pub set_setting: Option<unsafe extern "C" fn(user: *mut c_void, key: *const u8, key_len: usize, value: *const u8, value_len: usize) -> core::ffi::c_int>,
    /// Call me later, once or periodically.
    ///
    /// At `delay_ms` from now the host calls the handler with `(component, method)` and `value` = lateness in
    /// milliseconds. `interval_ms` > 0 makes it periodic (a poll): it is re-armed from its scheduled time, not from
    /// the time it ran. A timeout is a one-shot alarm. With `ALARM_FLEXIBLE` the call may be delayed up to
    /// `window_ms` so that it runs together with other work in the host's next wake window. All alarms of an
    /// instance are cancelled when it is destroyed.
    ///
    /// # Returns
    ///
    /// `int` — an alarm id (>= 0) or a negative error (`ERR_NOMEM` when the module has 8 alarms).
    ///
    /// # Arguments
    ///
    /// * `delay_ms` (`uint32_t`): First fire, from now.
    /// * `window_ms` (`uint32_t`): Flexibility; used with `ALARM_FLEXIBLE`.
    /// * `interval_ms` (`uint32_t`): 0 = once; otherwise the period.
    /// * `flags` (`uint32_t`): `ALARM_*`.
    /// * `component` (`int`): Returned in the call.
    /// * `method` (`int`): Returned in the call.
    pub alarm_set: Option<unsafe extern "C" fn(user: *mut c_void, delay_ms: u32, window_ms: u32, interval_ms: u32, flags: u32, component: core::ffi::c_int, method: core::ffi::c_int) -> core::ffi::c_int>,
    /// Cancel an alarm.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_NOTFOUND`.
    ///
    /// # Arguments
    ///
    /// * `alarm` (`int`): An id from `alarm_set`.
    pub alarm_cancel: Option<unsafe extern "C" fn(user: *mut c_void, alarm: core::ffi::c_int) -> core::ffi::c_int>,
    /// Every module slot.
    ///
    /// One CBOR array with a map per slot, keyed as the `slot` record of `modules/mod_con/API.md`: name, tag, installed, running,
    /// version, last result, runs, duration of the last run, stack high-water mark, result of the last image check.
    ///
    /// # Returns
    ///
    /// `int` — the length of the answer, or a negative error.
    ///
    /// # Arguments
    ///
    /// * `out` (`uint8_t *`): Buffer.
    /// * `capacity` (`size_t`): Its size.
    pub modules_info: Option<unsafe extern "C" fn(user: *mut c_void, out: *mut u8, capacity: usize) -> core::ffi::c_int>,
    /// Start the module installed in a slot.
    ///
    /// `request` is what the module receives with `HOST_START`.
    ///
    /// # Returns
    ///
    /// `int` — `OK` (the module starts in the background), `ERR_NOTFOUND` for an unknown name, or the loader's error.
    ///
    /// # Arguments
    ///
    /// * `name` (`const uint8_t *`): Slot name.
    /// * `name_len` (`size_t`): Length.
    /// * `request` (`const uint8_t *`): Start request, may be empty.
    /// * `request_len` (`size_t`): Length.
    pub module_start: Option<unsafe extern "C" fn(user: *mut c_void, name: *const u8, name_len: usize, request: *const u8, request_len: usize) -> core::ffi::c_int>,
    /// Ask a module to end.
    ///
    /// # Returns
    ///
    /// `int` — `OK` (it ends in the background; poll `modules_info`), `ERR_NOTFOUND`.
    ///
    /// # Arguments
    ///
    /// * `name` (`const uint8_t *`): Slot name.
    /// * `name_len` (`size_t`): Length.
    pub module_stop: Option<unsafe extern "C" fn(user: *mut c_void, name: *const u8, name_len: usize) -> core::ffi::c_int>,
    /// Restart the device.
    ///
    /// # Returns
    ///
    /// `int` — `OK`; the restart follows in 300 ms.
    pub reset: Option<unsafe extern "C" fn(user: *mut c_void) -> core::ffi::c_int>,
    /// Take an empty packet.
    ///
    /// The packet is **held** by this module: it counts against a small fixed quota (`CONFIG_XIP_PKT_HELD`, default 4) and is
    /// returned to the pool when the module sends or frees it, or ends. The handle is an opaque number that is never 0 and
    /// stops being valid when the packet leaves the module (a stale handle gets `ERR_ARG`, not someone else's packet).
    ///
    /// # Returns
    ///
    /// `int` — `OK` and the handle in `*pkt`; `ERR_NOMEM` when the pool is down to the network's reserve; `ERR_BUSY` when the module already holds its quota.
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t *`): The new handle.
    pub pkt_alloc: Option<unsafe extern "C" fn(user: *mut c_void, pkt: *mut u32) -> core::ffi::c_int>,
    /// The payload window of a packet.
    ///
    /// `*ptr` points at the payload window, `*max` is how many bytes may be written (at most `PKT_MAX`). For a packet received
    /// in a call the window is the received payload.
    ///
    /// # Returns
    ///
    /// `int` — `OK`; `ERR_ARG` for a handle that is not valid; `ERR_BUSY` when the packet is shared (a received packet that something else still reads cannot be written).
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t`): The handle.
    /// * `ptr` (`uint8_t **`): The window.
    /// * `max` (`size_t *`): Its size.
    pub pkt_buf: Option<unsafe extern "C" fn(user: *mut c_void, pkt: u32, ptr: *mut *mut u8, max: *mut usize) -> core::ffi::c_int>,
    /// Keep the packet of this call after the handler returns.
    ///
    /// # Returns
    ///
    /// `int` — `OK` (the packet is now held, like one from `pkt_alloc`), `ERR_ARG`, `ERR_BUSY` when the module holds its quota.
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t`): `call.pkt`.
    pub pkt_keep: Option<unsafe extern "C" fn(user: *mut c_void, pkt: u32) -> core::ffi::c_int>,
    /// Give a packet back.
    ///
    /// # Returns
    ///
    /// `int` — `OK` or `ERR_ARG`.
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t`): A held packet.
    pub pkt_free: Option<unsafe extern "C" fn(user: *mut c_void, pkt: u32) -> core::ffi::c_int>,
    /// Hand a packet to a module or to the application.
    ///
    /// The packet moves: **the handle is consumed on success and on failure**, so there is never a doubt who owns it. The first
    /// `len` bytes of the payload window are the message. The receiver gets a call `(component, method)` with `call.pkt`,
    /// `call.from` = this module's component and `call.id` = `id`. A non-zero `id` asks for an answer, and the receiver sees
    /// `flags` bit 0: it answers with a `send` to that `from` with the same `id` (or with `respond`, which the host turns into the
    /// same message). The answer is an ordinary message, and so is every other one the module sends the asker: any number of them,
    /// any time, the same `id` or another. Nobody waits for an answer, and a module with nothing to say says nothing.
    ///
    /// # Returns
    ///
    /// `int` — `OK`; `ERR_NOTFOUND` (no running module has that component), `ERR_BUSY` (its inbox is full), `ERR_SIZE`, `ERR_ARG`.
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t`): A held packet, or the packet of this call.
    /// * `len` (`size_t`): Message length, at most `PKT_MAX`.
    /// * `dst` (`uint32_t`): `ADDR_MAIN` or a module's component.
    /// * `component` (`int`): Returned in the receiver's call.
    /// * `method` (`int`): Returned in the receiver's call.
    /// * `id` (`uint64_t`): Correlation number, returned in the call.
    pub send: Option<unsafe extern "C" fn(user: *mut c_void, pkt: u32, len: usize, dst: u32, component: core::ffi::c_int, method: core::ffi::c_int, id: u64) -> core::ffi::c_int>,
    /// The geometry of this host's packets.
    ///
    /// For a host whose slots are larger than `SLOT_MIN`; `*headroom` is at least `PKT_HEADROOM` and `*max` at most what the
    /// payload window offers.
    ///
    /// # Returns
    ///
    /// `int` — `OK`.
    ///
    /// # Arguments
    ///
    /// * `headroom` (`uint32_t *`): Bytes in front of the window.
    /// * `max` (`uint32_t *`): Largest payload.
    pub pkt_info: Option<unsafe extern "C" fn(user: *mut c_void, headroom: *mut u32, max: *mut u32) -> core::ffi::c_int>,
    /// Memory that outlives a call.
    ///
    /// The heap is managed by balance: the host adds up the bytes a module takes and gives back. A message belongs in a packet
    /// (`pkt_alloc`); this is for what a module keeps between calls and does not fit in its state block (a reassembly buffer, a
    /// queue of frames). **A module frees everything it allocated before it stops.** If the balance is not zero when it ends, the
    /// stop is reported as failed (`ERR_LEAK`, bytes in `xip_stats_t.mem_leaked`) and the module **cannot be replaced live**:
    /// the host does not know which blocks are left, so the slot is marked `needs_reboot` and an install into it is refused until the
    /// device restarts. A leak therefore shows up in the first test rather than as a slow loss of memory in the field. The block is
    /// aligned for any type.
    ///
    /// # Returns
    ///
    /// `void *` — NULL on exhaustion, when an allocation would leave less than 32 KiB of host heap free, or when the optional per-module quota is exceeded. `CONFIG_XIP_MEM_QUOTA=0` (default) uses the available heap; a nonzero value caps outstanding allocations per module. The free-heap margin is checked at allocation time; unrelated firmware allocations can consume it later.
    ///
    /// # Arguments
    ///
    /// * `size` (`size_t`): Bytes.
    pub mem_alloc: Option<unsafe extern "C" fn(user: *mut c_void, size: usize) -> *mut c_void>,
    /// Give a block back.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, or `ERR_ARG` for a pointer this module did not allocate or already freed.
    ///
    /// # Arguments
    ///
    /// * `ptr` (`void *`): From `mem_alloc`.
    pub mem_free: Option<unsafe extern "C" fn(user: *mut c_void, ptr: *mut c_void) -> core::ffi::c_int>,
    /// What an installed module says about itself.
    ///
    /// A module image carries a **manifest** at the offset its header gives (`manifest_offset`, `manifest_size`), read from flash
    /// without starting the module, so it is available for an installed module that is not running and for one installed after the
    /// console was built. It is the module's `API.md` as a compact CBOR catalog, written by `tools/mkmod.py`: the module's name and
    /// component, its methods (id, name, and which request and response field list each uses), the field lists (tag, name, kind
    /// and, for a record, its type) and the record types. It is the equivalent of an MCP `tools.json` and carries what a text or
    /// JSON front end needs to turn `NAME.METHOD key=value` into the module's request without any ID compiled into it: the
    /// component of a module is found by name in `modules_info`, everything below it in the manifest. The module's own code never
    /// reads it. (Format: `tools/mkmod.py`, `manifest_of_api`.)
    ///
    /// Manifest key 7 is optional legacy `flash_slots` metadata emitted by the packer. It is not a host authorization
    /// boundary. Physical flash HAL calls do not interpret manifests; the control plane and upload module own image policy.
    ///
    /// # Returns
    ///
    /// `int` — the length of the manifest (0: the module has none), `ERR_NOTFOUND` for an unknown name, `ERR_SIZE` when the buffer is too small.
    ///
    /// # Arguments
    ///
    /// * `name` (`const uint8_t *`): Slot name.
    /// * `name_len` (`size_t`): Length.
    /// * `out` (`uint8_t *`): Buffer.
    /// * `capacity` (`size_t`): Its size.
    pub module_manifest: Option<unsafe extern "C" fn(user: *mut c_void, name: *const u8, name_len: usize, out: *mut u8, capacity: usize) -> core::ffi::c_int>,
    /// Free memory of the device.
    ///
    /// RAM is the scarce resource on these chips, so the host reports it to any module that asks (the console's `heap`). One CBOR map of
    /// byte counts: `{1: free, 2: largest block, 3: lowest free since boot, 4: total, 5: free internal RAM, 6: largest internal block}`.
    /// "Largest block" is the biggest single allocation that would succeed now; a heap can have plenty free and still refuse a
    /// request (fragmentation). Free memory is **tracked**, not only reported: a build that logs it on a change or at boot has the
    /// history, and the lowest value (3) shows the worst moment since boot, which is the number to size things by.
    ///
    /// # Returns
    ///
    /// `int` — the length of the answer, `ERR_SIZE` when the buffer is too small.
    ///
    /// # Arguments
    ///
    /// * `out` (`uint8_t *`): Buffer.
    /// * `capacity` (`size_t`): Its size.
    pub heap_info: Option<unsafe extern "C" fn(user: *mut c_void, out: *mut u8, capacity: usize) -> core::ffi::c_int>,
    /// A device setting by name.
    ///
    /// that is not set), `ERR_ARG` for a bad value, `ERR_IO` if it cannot be stored, `ERR_SIZE` when `out` is too small, `ERR_UNSUPPORTED`
    /// when the application offers none.
    ///
    /// The settings of the *device*, kept in its NVS by the application and the loader, as text: `wifi.rx_static`, `wifi.rx_dynamic`,
    /// `wifi.tx_dynamic`, `wifi.rx_ba` (the Wi-Fi driver's buffer counts, which are RAM) from the loader, and whatever else the application
    /// lists (DMesh: `uart.mode`, `quic.pool`, ...). The host knows no names: it passes them on, and the application answers for its own. This is for a
    /// console or a tool; a module's own settings are `get_setting` / `set_setting`. With `value` NULL it reads; with a value it writes
    /// (an empty value restores the default). Most settings apply at the next start of the thing they configure, or after a reboot.
    ///
    /// # Returns
    ///
    /// `int` — reading: the length of the text put in `out`; writing: `OK`. `ERR_NOTFOUND` for a name the device does not have (or
    ///
    /// # Arguments
    ///
    /// * `key` (`const uint8_t *`): Setting name.
    /// * `key_len` (`size_t`): Its length.
    /// * `value` (`const uint8_t *`): New value, or NULL to read.
    /// * `value_len` (`size_t`): Its length.
    /// * `out` (`uint8_t *`): Buffer for the value that is read.
    /// * `capacity` (`size_t`): Its size.
    pub nv: Option<unsafe extern "C" fn(user: *mut c_void, key: *const u8, key_len: usize, value: *const u8, value_len: usize, out: *mut u8, capacity: usize) -> core::ffi::c_int>,
    /// Print where the memory is, to the device log.
    ///
    /// `heap_info` says how much is free; this says what is there. The output goes to the **device log** (the port's text output, never
    /// into a reply), because it is long and for a person. `kind`: 0 the heap regions (size, free, largest block, block counts), 1 every block
    /// of the heap (address, size, free or used: the picture of fragmentation), 2 the tasks (name, priority, stack left; needs FreeRTOS
    /// trace facility), 3 start recording who allocates (`CONFIG_HEAP_TRACING_STANDALONE`; slows allocation, for a bench), 4 print what is
    /// still allocated with the call sites (the owners: this is the one that answers "who holds the RAM"), 5 stop recording.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG` for an unknown `kind`, `ERR_UNSUPPORTED` when the build lacks what the kind needs.
    ///
    /// # Arguments
    ///
    /// * `kind` (`int`): What to print (above).
    pub heap_dump: Option<unsafe extern "C" fn(user: *mut c_void, kind: core::ffi::c_int) -> core::ffi::c_int>,
    // Accepting is declarative: the manifest port (key 9) listens on TCP port `PORT_NET + port`, and each accepted connection arrives as a call
    // `(component, 0)` with `value = SOCKET_EVENT_*` and `id` = the socket handle (first `OPEN`), which the module reads and writes below.
    /// Nonblocking read into module memory or a packet span.
    ///
    /// Only for direct-mode accepted sockets. One call may read fewer than `len` bytes. The buffer is borrowed for this call only. A packet span may be passed as `out` while the module owns its mutable lease.
    ///
    /// # Returns
    ///
    /// `int` — bytes read, 0 at EOF, `ERR_AGAIN` if no bytes are ready; `ERR_ARG`, `ERR_IO`.
    ///
    /// # Arguments
    ///
    /// * `socket` (`uint32_t`): Accepted socket handle.
    /// * `out` (`uint8_t *`): Destination.
    /// * `len` (`size_t`): Maximum bytes to read.
    pub socket_read: Option<unsafe extern "C" fn(user: *mut c_void, socket: u32, out: *mut u8, len: usize) -> core::ffi::c_int>,
    /// Nonblocking POSIX-style write.
    ///
    /// The host does not keep `data` after return. A short write leaves the rest with the caller. The module requests another `WRITABLE` event by returning to the worker; it never waits in the handler.
    ///
    /// # Returns
    ///
    /// `int` — bytes written, `ERR_AGAIN` if no bytes were written; `ERR_ARG`, `ERR_IO`.
    ///
    /// # Arguments
    ///
    /// * `socket` (`uint32_t`): Accepted socket handle.
    /// * `data` (`const uint8_t *`): Bytes to write.
    /// * `len` (`size_t`): Byte count.
    pub socket_write: Option<unsafe extern "C" fn(user: *mut c_void, socket: u32, data: *const u8, len: usize) -> core::ffi::c_int>,
    /// Close a listener or accepted socket.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG`.
    ///
    /// # Arguments
    ///
    /// * `socket` (`uint32_t`): Handle to close.
    pub socket_close: Option<unsafe extern "C" fn(user: *mut c_void, socket: u32) -> core::ffi::c_int>,
    /// Peer address metadata.
    ///
    /// The output is an IPv6 address (IPv4 is represented as IPv4-mapped IPv6), port in host byte order, and IPv6 scope. The socket handle is not a raw lwIP descriptor.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG`, `ERR_IO`.
    ///
    /// # Arguments
    ///
    /// * `socket` (`uint32_t`): Accepted socket handle.
    /// * `address` (`uint8_t *`): Exactly 16 writable bytes.
    /// * `port` (`uint16_t *`): Peer port.
    /// * `scope` (`uint32_t *`): IPv6 interface scope.
    pub socket_peer: Option<unsafe extern "C" fn(user: *mut c_void, socket: u32, address: *mut u8, port: *mut u16, scope: *mut u32) -> core::ffi::c_int>,
    /// Full mutable pool slot for framing.
    ///
    /// Returns the full slot and capacity while the packet is uniquely owned. The caller chooses a valid prefix, payload and suffix range; `socket_write_pkt` validates the selected range. Pointers do not survive packet release or transfer.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG`, `ERR_BUSY` when shared.
    ///
    /// # Arguments
    ///
    /// * `pkt` (`uint32_t`): Held packet.
    /// * `base` (`uint8_t **`): Receives the slot base pointer.
    /// * `capacity` (`size_t *`): Receives its byte capacity.
    pub pkt_span: Option<unsafe extern "C" fn(user: *mut c_void, pkt: u32, base: *mut *mut u8, capacity: *mut usize) -> core::ffi::c_int>,
    /// Transfer a pool packet for runtime-buffered output.
    ///
    /// On `OK` the runtime owns the packet until the whole selected range has been submitted, then emits `WRITE_DONE`. On every error the caller retains the exact packet. TCP may write part of it; the runtime retains the remaining range. The runtime admits at most one pending packet per socket. This differs from `send`, which consumes on failure.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_BUSY`, `ERR_ARG`, `ERR_SIZE`, `ERR_IO`.
    ///
    /// # Arguments
    ///
    /// * `socket` (`uint32_t`): Accepted socket handle.
    /// * `pkt` (`uint32_t`): Held packet.
    /// * `start` (`uint32_t`): First byte within the slot.
    /// * `len` (`uint32_t`): Bytes to submit.
    pub socket_write_pkt: Option<unsafe extern "C" fn(user: *mut c_void, socket: u32, pkt: u32, start: u32, len: u32) -> core::ffi::c_int>,
    /// Enqueue a physical flash erase.
    ///
    /// `address` is a physical flash base address; the operation starts at `address + offset`.
    /// The host checks arithmetic overflow, actual device capacity, nonzero length and 4096-byte erase alignment.
    /// It does not enforce partitions, signed manifests, ownership or image policy. These are low-level HAL primitives;
    /// callers are responsible for stopping code mapped from the bytes they replace and coordinating other flash users.
    /// The upload module and its control plane decide which image is acceptable and where it belongs.
    ///
    /// Erase runs on a flash worker, never on the module or poll worker. At most one flash job per caller and one device-wide flash job may be pending. On completion, a still-running caller receives `(component, method)` with `call.id = id`, `call.value = OK` or a negative error, and no payload. A caller stop cancels a queued job. If the worker has started it, stop waits for that job to finish before destroying the instance or reusing its slot; no callback is delivered after stop. The host does not inspect XIP image mappings. A stopped caller cannot submit another job.
    ///
    /// # Returns
    ///
    /// `int` — `OK` when queued; `ERR_BUSY`, `ERR_ARG`, `ERR_IO`, `ERR_UNSUPPORTED`.
    ///
    /// # Arguments
    ///
    /// * `address` (`uint32_t`): Physical flash base address.
    /// * `offset` (`uint32_t`): Byte offset added to address.
    /// * `len` (`uint32_t`): Erase byte count.
    /// * `component` (`int32_t`): Completion component.
    /// * `method` (`int32_t`): Completion method.
    /// * `id` (`uint64_t`): Caller correlation number.
    pub flash_erase: Option<unsafe extern "C" fn(user: *mut c_void, address: u32, offset: u32, len: u32, component: i32, method: i32, id: u64) -> core::ffi::c_int>,
    /// Transfer a pool packet for a physical flash write.
    ///
    /// The same device-capacity and overflow checks as `flash_erase` apply. `start + len` must fit the uniquely held pool slot and `len` must be nonzero; invalid ranges return `ERR_ARG`. The host writes directly from the packet's selected span. On `OK` it takes ownership of the exact packet until the write completes or is canceled, then releases it; it allocates no separate 4096-byte job buffer. On any error the caller retains ownership. The pool slot capacity is the maximum job payload, so larger upload blocks are split by the module. Completion and stop follow `flash_erase`; no callback is delivered after the caller stops. The caller must quiesce any code using the target bytes.
    ///
    /// # Returns
    ///
    /// `int` — `OK` when queued; `ERR_BUSY`, `ERR_ARG`, `ERR_SIZE`, `ERR_IO`, `ERR_UNSUPPORTED`.
    ///
    /// # Arguments
    ///
    /// * `address` (`uint32_t`): Physical flash base address.
    /// * `offset` (`uint32_t`): Byte offset added to address.
    /// * `pkt` (`uint32_t`): Uniquely held pool packet; transferred only on `OK`.
    /// * `start` (`uint32_t`): First byte within the pool slot.
    /// * `len` (`uint32_t`): Byte count, at most the pool slot capacity.
    /// * `component` (`int32_t`): Completion component.
    /// * `method` (`int32_t`): Completion method.
    /// * `id` (`uint64_t`): Caller correlation number.
    pub flash_write: Option<unsafe extern "C" fn(user: *mut c_void, address: u32, offset: u32, pkt: u32, start: u32, len: u32, component: i32, method: i32, id: u64) -> core::ffi::c_int>,
    /// Read physical flash into module memory.
    ///
    /// A read is synchronous and limited to 4096 bytes. The physical range `address + offset .. + len` must fit the device without arithmetic overflow. No partition or image policy is enforced. Split larger reads over callbacks.
    ///
    /// # Returns
    ///
    /// `int` — bytes read; `ERR_ARG`, `ERR_IO`, `ERR_SIZE`, `ERR_UNSUPPORTED`.
    ///
    /// # Arguments
    ///
    /// * `address` (`uint32_t`): Physical flash base address.
    /// * `offset` (`uint32_t`): Byte offset added to address.
    /// * `out` (`uint8_t *`): Destination.
    /// * `len` (`size_t`): At most 4096 bytes.
    pub flash_read: Option<unsafe extern "C" fn(user: *mut c_void, address: u32, offset: u32, out: *mut u8, len: usize) -> core::ffi::c_int>,
    /// Structured device and component status.
    ///
    /// Returns one tagged-CBOR map: `{1: device, 2: wifi, 3: heap, 4: modules, 5: components}`.
    /// Device is `{1: uptime_ms, 2: chip, 3: flash_size, 4: active_partition_label,
    /// 5: active_partition_address, 6: active_partition_size}`. Chip is an ESP chip model number;
    /// partition fields are omitted when the platform cannot identify its running partition.
    /// Wi-Fi, heap and modules reuse the Wi-Fi state, `heap_info` and `modules_info` records;
    /// unavailable Wi-Fi is an empty map and unavailable module enumeration is an empty array.
    /// Components is `{component: {method: {1: updated_ms, 2: status_map}}}`.
    /// Times are milliseconds since boot. Built-in records are read during the call; contributed
    /// records are cached and may be stale. This call never invokes another module handler.
    /// No console module is needed. No partial record is returned on buffer overflow.
    ///
    /// # Returns
    ///
    /// `int` — encoded length, `ERR_ARG`, `ERR_SIZE`, or `ERR_UNSUPPORTED`.
    ///
    /// # Arguments
    ///
    /// * `out` (`uint8_t *`): Caller-owned CBOR destination.
    /// * `capacity` (`size_t`): Destination size.
    pub device_info: Option<unsafe extern "C" fn(user: *mut c_void, out: *mut u8, capacity: usize) -> core::ffi::c_int>,
    /// Replace a cached component status contribution.
    ///
    /// Copies one complete definite-length CBOR map, keyed by component and method, and records
    /// its publication time. The input remains caller-owned. A zero length clears that key.
    /// The latest publication replaces the previous entry. Entries owned by a module instance
    /// are removed when it stops or finishes, including entries published during HOST_STOP.
    /// Host components use the same cache through the native host API. Published data is
    /// status, not a handler request; reading device_info does not call the named method.
    /// Allocation is fallible and keeps the host's 32 KiB heap margin; replaced and cleared
    /// buffers are freed. Component and method are nonnegative integers.
    ///
    /// # Returns
    ///
    /// `int` — `OK`, `ERR_ARG`, `ERR_NOMEM`, or `ERR_UNSUPPORTED`.
    ///
    /// # Arguments
    ///
    /// * `component` (`int32_t`): Component identifier.
    /// * `method` (`int32_t`): Status record identifier within that component.
    /// * `data` (`const uint8_t *`): CBOR map; NULL is valid when len is zero.
    /// * `len` (`size_t`): Map size; zero removes the contribution.
    pub info_publish: Option<unsafe extern "C" fn(user: *mut c_void, component: i32, method: i32, data: *const u8, len: usize) -> core::ffi::c_int>,
    /// Get a live host function table.
    ///
    /// The leading `user` argument is the requesting module's existing runtime context,
    /// used for bookkeeping; it is not added to functions in the returned table.
    /// `name` is a NUL-terminated string. `"wifi"` returns the wanted-state component (`xip_wifi` in `xip.h`: `set`, `get`, `state`; design in
    /// `notes/ai/2026-10-16-wifi-intent-component.md`), where the host has Wi-Fi. `"psa"` returns the host's native PSA
    /// crypto table, defined by the ordinary C header `include/xip_psa.h` and a
    /// hand-written Rust wrapper. Component headers need not be generated from this file.
    /// The PSA table is read-only and valid for the firmware lifetime. Its signatures,
    /// context types and error codes are native SDK ones. Lookup does not initialize
    /// Wi-Fi or invoke another module. Module-provided tables and entrypoint changes
    /// are separate future work.
    ///
    /// # Returns
    ///
    /// `const void *` — table pointer, or NULL if the name is unknown/unavailable.
    ///
    /// # Arguments
    ///
    /// * `name` (`const char *`): NUL-terminated component name; NULL returns NULL.
    pub get_component: Option<unsafe extern "C" fn(user: *mut c_void, name: *const core::ffi::c_char) -> *const c_void>,
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<Host>() == 148);

/// Byte offset of each host call, for `Hw`'s check that an older host has the slot.
pub mod slot {
    use super::Host;
    pub const LOG: usize = core::mem::offset_of!(Host, log);
    pub const NOW_MS: usize = core::mem::offset_of!(Host, now_ms);
    pub const GET_SETTING: usize = core::mem::offset_of!(Host, get_setting);
    pub const SET_SETTING: usize = core::mem::offset_of!(Host, set_setting);
    pub const ALARM_SET: usize = core::mem::offset_of!(Host, alarm_set);
    pub const ALARM_CANCEL: usize = core::mem::offset_of!(Host, alarm_cancel);
    pub const MODULES_INFO: usize = core::mem::offset_of!(Host, modules_info);
    pub const MODULE_START: usize = core::mem::offset_of!(Host, module_start);
    pub const MODULE_STOP: usize = core::mem::offset_of!(Host, module_stop);
    pub const RESET: usize = core::mem::offset_of!(Host, reset);
    pub const PKT_ALLOC: usize = core::mem::offset_of!(Host, pkt_alloc);
    pub const PKT_BUF: usize = core::mem::offset_of!(Host, pkt_buf);
    pub const PKT_KEEP: usize = core::mem::offset_of!(Host, pkt_keep);
    pub const PKT_FREE: usize = core::mem::offset_of!(Host, pkt_free);
    pub const SEND: usize = core::mem::offset_of!(Host, send);
    pub const PKT_INFO: usize = core::mem::offset_of!(Host, pkt_info);
    pub const MEM_ALLOC: usize = core::mem::offset_of!(Host, mem_alloc);
    pub const MEM_FREE: usize = core::mem::offset_of!(Host, mem_free);
    pub const MODULE_MANIFEST: usize = core::mem::offset_of!(Host, module_manifest);
    pub const HEAP_INFO: usize = core::mem::offset_of!(Host, heap_info);
    pub const NV: usize = core::mem::offset_of!(Host, nv);
    pub const HEAP_DUMP: usize = core::mem::offset_of!(Host, heap_dump);
    pub const SOCKET_READ: usize = core::mem::offset_of!(Host, socket_read);
    pub const SOCKET_WRITE: usize = core::mem::offset_of!(Host, socket_write);
    pub const SOCKET_CLOSE: usize = core::mem::offset_of!(Host, socket_close);
    pub const SOCKET_PEER: usize = core::mem::offset_of!(Host, socket_peer);
    pub const PKT_SPAN: usize = core::mem::offset_of!(Host, pkt_span);
    pub const SOCKET_WRITE_PKT: usize = core::mem::offset_of!(Host, socket_write_pkt);
    pub const FLASH_ERASE: usize = core::mem::offset_of!(Host, flash_erase);
    pub const FLASH_WRITE: usize = core::mem::offset_of!(Host, flash_write);
    pub const FLASH_READ: usize = core::mem::offset_of!(Host, flash_read);
    pub const DEVICE_INFO: usize = core::mem::offset_of!(Host, device_info);
    pub const INFO_PUBLISH: usize = core::mem::offset_of!(Host, info_publish);
    pub const GET_COMPONENT: usize = core::mem::offset_of!(Host, get_component);
}

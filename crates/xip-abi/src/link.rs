//! Links: how frames reach modules and leave them. UDP, ESP-NOW, PPP, the console's lines and the radio of a module like
//! `mod_lora` are all the same thing to a module that speaks a protocol (QUIC, a mesh, a console): a place frames come
//! from and go to.
//!
//! A **link** has a name (`"espnow"`, `"udp6"`, `"ppp"`, `"line"`, `"lora"`, ...) and an **address**, a number from
//! [`ADDR_LINK`] up. It is provided either by the runtime (a *native* link: the runtime owns the driver) or by a module (`mod_lora`
//! provides `"lora"`: it owns the radio). Modules subscribe to it. Which side of a link a frame is on does not matter to a
//! protocol module, so QUIC runs unchanged over ESP-NOW, UDP6, a serial line or LoRa.
//!
//! # Frames
//!
//! A frame is a pool packet, never copied between the driver, the runtime and modules. Its **token** is a `u64` that names the
//! far end on that link (a MAC address as [`mac_token`], a node number, a peer handle); the link defines it, 0 on transmit is
//! the link's broadcast. The packet's payload window starts at least `PKT_HEADROOM` bytes into the slot, so whoever sends it next
//! writes its own envelope in front without moving the payload (a relay sends the same packet on another link).
//!
//! * **Receive.** A subscriber gets every frame as a packet call `(component, method)` it chose in [`Link::open`]: `call.from`
//!   is the link's address, `call.data` the frame, `call.pkt` its packet (keep it with `pkt_keep` or pass it on with `send`),
//!   `call.id` the source token, `call.value` the signal ([`signal`]), `call.time_us` the receive time (low 32 bits of the
//!   microsecond clock, taken by the driver, not at delivery). `call.flags` has [`CALL_LINK`], which also says `id` is an
//!   address and not a request to answer.
//! * **Transmit.** `send(pkt, len, dst = link address, component, method, id = destination token)`. The runtime owns the packet
//!   from the moment the link accepts it. If `component` or `method` is not 0 the packet comes **back** to the sender when the
//!   link is done with it, as a call `(component, method)` with `CALL_LINK | CALL_LINK_EVENT` and [`LINK_EVENT_TX_DONE`]
//!   in `call.flags >> 8`: `call.value` is the outcome (0 sent, a negative `ERR_*`), `call.id` the token, `call.time_us` the
//!   microseconds it took, `call.pkt` the same packet (to retransmit, or let go). With both 0 the packet is released when sent.
//!   `ERR_BUSY` means the link has no room: that is the one case where `send` does **not** consume the packet, so the sender can
//!   try another link.
//! * **Events.** Subscribers also get calls with `CALL_LINK | CALL_LINK_EVENT` (`call.flags >> 8` is the kind): [`LINK_EVENT_READY`]
//!   (capacity returned after `ERR_BUSY`) and [`LINK_EVENT_STATE`] (`call.value` the new state, `call.id` the rate in kbit/s).
//! * **Provide.** A module that owns a transport calls [`Link::provide`] once. Frames sent to the link arrive as calls
//!   `(component, method)` it gave (`call.from` the sender, `call.id` the token, `call.pkt` the packet: keep it while the radio
//!   transmits, then [`Link::tx_done`]; a packet it does not keep counts as sent when the call returns). It reports what it
//!   receives with [`Link::rx`], capacity with [`Link::ready`] and its state with [`Link::set_state`].
//!
//! # Forwarding in the runtime
//!
//! Relaying is the purpose of a mesh node and is latency critical, so it does not wake a module. A module that manages the
//! associations tells the runtime where the connection identifier sits in the frames of a link ([`Link::classify`]) and installs
//! a route per identifier ([`Link::route`]): a frame whose identifier matches is sent on to the route's egress link and token
//! straight from the driver's context, before any subscriber sees it. Everything else is delivered as above. The match is bytes at
//! an offset, nothing about QUIC.
//!
//! Frames are best effort: a subscriber whose inbox is full loses the frame and the link counts it ([`LinkInfo::dropped`]).
//!
//! Look the component up once with `get_component("link")` ([`Hw::link`](crate::Hw::link)).
use core::ffi::{c_char, c_int, c_void};

/// Version of the [`Link`] table.
pub const LINK_VERSION: u32 = 1;

/// `call.flags` bit set on the frames of a link (both directions): `id` is an address token and the call is not a request.
pub const CALL_LINK: u32 = 2;
/// `call.flags` bit set on the events of a link; the kind is in `call.flags >> 8`.
pub const CALL_LINK_EVENT: u32 = 4;

/// Event: a frame sent to the link is done (the packet comes back, `call.value` is the outcome).
pub const LINK_EVENT_TX_DONE: u32 = 1;
/// Event: the link has capacity again after answering a send with `ERR_BUSY`.
pub const LINK_EVENT_READY: u32 = 2;
/// Event: the state or the rate of the link changed.
pub const LINK_EVENT_STATE: u32 = 3;

/// The kind of an event call: `call.flags >> 8`.
pub const fn event_kind(flags: u32) -> u32 {
    (flags >> 8) & 0xff
}

/// Names of the links the runtime provides when it can.
pub const LINK_ESPNOW: &core::ffi::CStr = c"espnow";
pub const LINK_UDP6: &core::ffi::CStr = c"udp6";
pub const LINK_PPP: &core::ffi::CStr = c"ppp";
pub const LINK_LINE: &core::ffi::CStr = c"line";

/// Who provides a link ([`LinkInfo::provider`]).
pub const LINK_NONE: u32 = 0;
/// The runtime owns the driver.
pub const LINK_NATIVE: u32 = 1;
/// A module owns the transport.
pub const LINK_MODULE: u32 = 2;

/// [`LinkInfo::state`]: not known or not usable (no provider, radio off, no carrier).
pub const LINK_DOWN: u32 = 0;
/// [`LinkInfo::state`]: frames can be sent.
pub const LINK_UP: u32 = 1;

/// [`LinkInfo::flags`]: the link protects frames itself (a secured Wi-Fi network, an encrypted ESP-NOW peer).
pub const LINK_FLAG_SECURE: u32 = 1;
/// [`LinkInfo::flags`]: a byte stream with no frame boundaries of its own (a serial line): the frames are what the provider cut.
pub const LINK_FLAG_STREAM: u32 = 2;

/// [`Link::route`] flag: also deliver the frame to the subscribers (the default is to forward it only).
pub const ROUTE_COPY: u32 = 1;

/// Pack the signal of a received frame for `call.value`: the RSSI in dBm in the low 16 bits, the SNR in quarter dB in the
/// high 16 bits (0 when the link has none).
pub const fn signal(rssi_dbm: i16, snr_quarter_db: i16) -> i32 {
    ((snr_quarter_db as i32) << 16) | (rssi_dbm as u16 as i32)
}
/// The RSSI in dBm of a packed signal.
pub const fn rssi(signal: i32) -> i16 {
    signal as i16
}
/// The SNR in quarter dB of a packed signal.
pub const fn snr_quarter_db(signal: i32) -> i16 {
    (signal >> 16) as i16
}
/// The token of an Ethernet-style link (ESP-NOW, UDP6 by peer) from a MAC address: the six bytes, first byte lowest.
pub const fn mac_token(mac: [u8; 6]) -> u64 {
    (mac[0] as u64) | (mac[1] as u64) << 8 | (mac[2] as u64) << 16 | (mac[3] as u64) << 24 | (mac[4] as u64) << 32 | (mac[5] as u64) << 40
}

/// What a link is and how it has done, for [`Link::info`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinkInfo {
    /// `sizeof(xip_link_info)`, set by the caller.
    pub size: u32,
    /// Who provides it: `LINK_NONE` (named by a subscriber, nobody yet), `LINK_NATIVE` or `LINK_MODULE`.
    pub provider: u32,
    /// `LINK_UP` or `LINK_DOWN`, as the provider last said.
    pub state: u32,
    /// Largest frame the link carries, bytes (0: unknown).
    pub mtu: u32,
    /// Nominal rate in kbit/s (0: unknown).
    pub rate_kbps: u32,
    /// `LINK_FLAG_*`.
    pub flags: u32,
    /// Subscribers.
    pub subscribers: u32,
    /// Frames received from the provider and delivered or forwarded (a frame to two subscribers counts once).
    pub received: u32,
    /// Frames handed to the provider by `send` or by a route.
    pub sent: u32,
    /// Frames forwarded by a route of this link.
    pub forwarded: u32,
    /// Frames lost: no pool packet, a full inbox, a provider that refused.
    pub dropped: u32,
    /// The link's name, NUL-terminated.
    pub name: [u8; 16],
}
impl Default for LinkInfo {
    fn default() -> Self {
        let mut i: LinkInfo = unsafe { core::mem::zeroed() };
        i.size = core::mem::size_of::<LinkInfo>() as u32;
        i
    }
}
const _: () = assert!(core::mem::size_of::<LinkInfo>() == 60);

/// The `link` component table, from `get_component("link")`. Valid for the firmware lifetime; every function takes `ctx`
/// first (the `user` of the host table) and never blocks. Results are negative `ERR_*`.
#[repr(C)]
pub struct Link {
    /// `LINK_VERSION`.
    pub version: u32,
    /// `sizeof(xip_link)`: members past it do not exist.
    pub size: u32,
    /// Subscribe to the link `name` (created if nobody has named it yet, so the order of start does not matter): its frames
    /// and events arrive as calls `(component, method)` to the calling module. Returns the link's address (>= `ADDR_LINK`, positive) to
    /// `send` to, or `ERR_ARG` / `ERR_NOMEM` (no room for another link or subscriber). Subscribing again with the same pair changes nothing.
    pub open: Option<unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, component: c_int, method: c_int) -> c_int>,
    /// Stop receiving the link at `addr`. Subscriptions also end with the module.
    pub close: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32) -> c_int>,
    /// Be the provider of the link `name`: frames sent to its address arrive as calls `(component, method)` to the calling
    /// module. `mtu` is the largest frame (0: unknown). The link is `LINK_DOWN` until [`Link::set_state`]. Returns the address,
    /// `ERR_BUSY` if another provider has the name, or `ERR_ARG`. The provision ends with the module.
    pub provide: Option<unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, component: c_int, method: c_int, mtu: u32) -> c_int>,
    /// The provider received a frame: the **held** packet `pkt` (see `pkt_alloc`) with `len` bytes at its payload window goes
    /// to the route or to every subscriber, with `source`, `signal` and `time_us` (the low 32 bits of the microsecond clock when
    /// the radio received it; 0: now). The packet is consumed whether this succeeds or not (a link with nobody listening is a
    /// success). `ERR_ARG` for an address this module does not provide.
    pub rx: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, pkt: u32, len: usize, source: u64, signal: i32, time_us: u32) -> c_int>,
    /// The provider is done with a frame it was given (the held packet `pkt`, which it kept): `outcome` is 0 or a negative
    /// `ERR_*`. If the sender asked, the packet goes back to it as a `LINK_EVENT_TX_DONE` call; otherwise it is released.
    pub tx_done: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, pkt: u32, outcome: c_int) -> c_int>,
    /// The provider has capacity again after refusing a frame: every subscriber gets `LINK_EVENT_READY`.
    pub ready: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32) -> c_int>,
    /// The provider's state (`LINK_UP` / `LINK_DOWN`) and nominal rate in kbit/s changed: subscribers get `LINK_EVENT_STATE`.
    pub set_state: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, state: u32, rate_kbps: u32) -> c_int>,
    /// Describe the link at `addr` (set `info.size` first).
    pub info: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, info: *mut LinkInfo) -> c_int>,
    /// Tell the runtime where the connection identifier is in the frames of the link at `addr`: `len` bytes (1..=16) at `offset`
    /// from the start of the frame. A frame too short for it is delivered, never routed. Changing it clears the routes of the link.
    pub classify: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, offset: u32, len: u32) -> c_int>,
    /// Install (or replace) a route on the link at `addr`: a frame whose identifier equals `key[..key_len]` (`key_len` must be
    /// the classified length) is sent to `egress` with destination `token`, from the driver's context, and counted; with
    /// `ROUTE_COPY` the subscribers get it too. `ERR_NOMEM` when the table (16 routes) is full, `ERR_ARG` for a link that is not classified.
    pub route: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, key: *const u8, key_len: usize, egress: u32, token: u64, flags: u32) -> c_int>,
    /// Remove the route for `key` (`ERR_NOTFOUND` if there is none).
    pub unroute: Option<unsafe extern "C" fn(ctx: *mut c_void, addr: u32, key: *const u8, key_len: usize) -> c_int>,
}

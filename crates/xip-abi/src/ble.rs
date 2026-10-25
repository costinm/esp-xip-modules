//! The `ble` component (104): the DMesh companion link over Bluetooth LE, hosted by the module `mod_ble`, from `get_component("ble")`.
//!
//! The types of its API (`modules/mod_ble/API.md` names them as `C type`) are here; the table [`Ble`] (`ble_vtable.rs`) is generated from the API.md
//! by `tools/gen_vtables.py`. The module owns the policy: the service UUIDs, the advertisement and its intervals, the device name, the default PSM, the
//! scan filter and the size and buffer count of the CoC pool, and it drives the runtime's `nimble` component ([`Nimble`](crate::Nimble)), the thin
//! NimBLE subset that holds none of it. The method numbers and field tags are the ones of the DMesh firmware (`ble` component 104: `start` 80 ...
//! `connect` 85).
//!
//! ```c
//! const xip_ble *ble = host->get_component(host->user, "ble");
//! ble->start(host->user, &(xip_ble_start){ .size = sizeof(xip_ble_start), .paired = 0 });   /* advertise the pairing window */
//! ```

/// `Ble::version`.
pub const BLE_VERSION: u32 = 1;
/// The default dynamic LE PSM of the DMesh CoC channel; a PSM argument of 0 means this.
pub const BLE_COC_PSM: u16 = 0x0080;
/// Entries of the scan cache that [`Ble::scan_results`] can return at most (`CONFIG_XIP_BLE_SCAN_CACHE` entries are kept).
pub const BLE_SCAN_RESULT_MAX: usize = 16;
/// Bytes of one scan result record: address (6), address type, RSSI (signed byte), flags (bit 0: the advertisement names the DMesh service).
pub const BLE_SCAN_RECORD: usize = 9;

/// `BleMode::mode`: leave as is.
pub const BLE_MODE_KEEP: u8 = 0;
/// `BleMode::mode`: the service stays advertised while the device sleeps (a light sleep is vetoed while it is live). The default.
pub const BLE_MODE_ALWAYS: u8 = 1;
/// `BleMode::mode`: duty-cycled rendezvous: advertising and scanning stop before a sleep and are re-armed after it; only an established link vetoes the sleep.
pub const BLE_MODE_RENDEZVOUS: u8 = 2;
/// `BleMode::boot`: leave as is.
pub const BLE_BOOT_KEEP: u8 = 0;
/// `BleMode::boot`: BLE does not start by itself at boot.
pub const BLE_BOOT_OFF: u8 = 1;
/// `BleMode::boot`: BLE service starts after the radio is ready at boot.
pub const BLE_BOOT_ON: u8 = 2;

/// Start the DMesh service: the CoC server and the advertisement, for [`Ble::start`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleStart {
    /// `sizeof(xip_ble_start)`, set by the caller.
    pub size: u32,
    /// 1: the device holds a pairing secret, so the advertisement carries the operational UUID; 0: the button-gated pairing UUID.
    pub paired: u8,
    pub reserved: [u8; 3],
}
impl Default for BleStart {
    fn default() -> Self {
        BleStart { size: core::mem::size_of::<BleStart>() as u32, paired: 0, reserved: [0; 3] }
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleStart>() == 8);

/// Scan, for [`Ble::scan`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleScan {
    pub size: u32,
    /// Milliseconds, clamped to 1000..30000; 0 is 10000.
    pub duration_ms: u32,
}
impl Default for BleScan {
    fn default() -> Self {
        BleScan { size: core::mem::size_of::<BleScan>() as u32, duration_ms: 0 }
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleScan>() == 8);

/// Central connect and open the CoC channel, for [`Ble::connect`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleConnect {
    pub size: u32,
    /// The peer, as the stack reports it (little-endian, the order scan results use).
    pub addr: [u8; 6],
    /// 0 public, 1 random.
    pub addr_type: u8,
    pub reserved: u8,
    /// 0x0080..0x00ff; 0 is `BLE_COC_PSM`.
    pub psm: u16,
    pub reserved2: [u8; 2],
}
impl Default for BleConnect {
    fn default() -> Self {
        let mut c: BleConnect = unsafe { core::mem::zeroed() };
        c.size = core::mem::size_of::<BleConnect>() as u32;
        c
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleConnect>() == 16);

/// Listen on a PSM without advertising, for [`Ble::listen`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleListen {
    pub size: u32,
    /// 0x0080..0x00ff; 0 is `BLE_COC_PSM`.
    pub psm: u16,
    pub reserved: [u8; 2],
}
impl Default for BleListen {
    fn default() -> Self {
        BleListen { size: core::mem::size_of::<BleListen>() as u32, psm: 0, reserved: [0; 2] }
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleListen>() == 8);

/// Wake/sleep modes, for [`Ble::mode`]. A field left at 0 is not changed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleMode {
    pub size: u32,
    /// `BLE_MODE_*`.
    pub mode: u8,
    /// `BLE_BOOT_*`.
    pub boot: u8,
    pub reserved: [u8; 2],
}
impl Default for BleMode {
    fn default() -> Self {
        BleMode { size: core::mem::size_of::<BleMode>() as u32, mode: 0, boot: 0, reserved: [0; 2] }
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleMode>() == 8);

/// What the component is doing, for [`Ble::status`]: the fields of the DMesh `ble.status` (tags 1 to 26) and the additions of this component.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleStatus {
    pub size: u32,
    /// The stack is synchronized (the host task runs and the address is known).
    pub ready: u8,
    pub advertising: u8,
    /// A GAP connection exists.
    pub connected: u8,
    /// The CoC channel is open.
    pub coc_connected: u8,
    pub scanning: u8,
    pub bonded: u8,
    pub coc_tx_pending: u8,
    pub last_scan_addr_type: u8,
    pub local_addr_type: u8,
    /// Link encrypted (the CoC channel's connection).
    pub encrypted: u8,
    /// `BLE_MODE_*` in force.
    pub mode: u8,
    /// BLE starts at boot (`BLE_BOOT_ON`).
    pub boot_enabled: u8,
    /// The service is suspended around a sleep (rendezvous mode).
    pub suspended: u8,
    pub reserved: [u8; 3],
    /// Increments for every GAP connection.
    pub generation: u32,
    /// GAP connection handle, 0xffff when none.
    pub handle: u16,
    pub reserved2: [u8; 2],
    /// SDUs given to the receive hook, and rejected before it (bad length, no hook).
    pub coc_rx: u32,
    pub coc_rx_rejected: u32,
    /// Advertisements seen while scanning, and those naming the DMesh service.
    pub scan_reports: u32,
    pub scan_matches: u32,
    pub last_scan_rssi: i32,
    pub last_scan_addr: [u8; 6],
    pub local_addr: [u8; 6],
    /// The consumer's side of the bearer: SDUs accepted for the hook, accepted and rejected by it, transmit attempts, accepted, failed, and refused because a send was pending.
    pub ingress_enqueued: u32,
    pub ingress_accepted: u32,
    pub ingress_rejected: u32,
    pub egress_attempts: u32,
    pub egress_sent: u32,
    pub egress_send_errors: u32,
    pub egress_blocked_disconnected: u32,
    pub egress_blocked_pending: u32,
}
impl Default for BleStatus {
    fn default() -> Self {
        let mut s: BleStatus = unsafe { core::mem::zeroed() };
        s.size = core::mem::size_of::<BleStatus>() as u32;
        s
    }
}
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleStatus>() == 92);

/// The scan cache, for [`Ble::scan_results`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BleScanResults {
    pub size: u32,
    /// Bytes of `results` that are used: a multiple of `BLE_SCAN_RECORD`.
    pub results_len: u8,
    pub reserved: [u8; 3],
    /// Records of `BLE_SCAN_RECORD` bytes each: address, address type, RSSI, flags; the order is first seen.
    pub results: [u8; 144],
}
impl Default for BleScanResults {
    fn default() -> Self {
        let mut s: BleScanResults = unsafe { core::mem::zeroed() };
        s.size = core::mem::size_of::<BleScanResults>() as u32;
        s
    }
}
const _: () = assert!(BLE_SCAN_RESULT_MAX * BLE_SCAN_RECORD == 144);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<BleScanResults>() == 152);

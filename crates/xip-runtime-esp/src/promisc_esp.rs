//! Promiscuous receive (the `wifi` component's `monitor`) is the C implementation of the runtime, shared by both hosts:
//! `components/xip_runtime/src/xip_link_promisc.c`. The receive callback and the early filter (`xip_rx_filter.c`) must be fixed code in IRAM, and
//! one copy of them is enough: this is not initialization, so there is no twin. The callback runs the filter before anything is copied into a pool
//! packet, then delivers the frame on the `promisc` link. The runtime makes no power-save change and does not choose the window: the module does.
use xip_abi::{WifiMonitor, ERR_ARG, ERR_UNSUPPORTED};

extern "C" {
    fn xip_link_promisc_set(monitor: *const WifiMonitor) -> i32;
    fn xip_link_promisc_stop();
}

/// The radio stops: the driver forgets promiscuous mode, so the link goes down.
pub(crate) fn stop() {
    unsafe { xip_link_promisc_stop() };
}

/// `HostExt::wifi_monitor`.
pub(crate) unsafe extern "C" fn abi_monitor(m: *const WifiMonitor) -> i32 {
    if m.is_null() {
        return ERR_ARG;
    }
    if !crate::wifi_esp::radio_up() {
        return ERR_UNSUPPORTED;
    }
    if unsafe { (*m).size } < core::mem::size_of::<WifiMonitor>() as u32 {
        return ERR_ARG;
    }
    unsafe { xip_link_promisc_set(m) }
}

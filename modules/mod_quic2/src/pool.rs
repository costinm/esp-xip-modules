//! Zero-copy `quic_mux::{PacketPool, PoolPacket}` adapter over the runtime packet pool.

use core::ops::Range;
use quic_mux::{PacketPool, PoolError, PoolPacket};
use xip_abi::{Hw, PacketPoolInfo, PKT_HEADROOM, PKT_MAX};

#[derive(Clone, Copy)]
pub(crate) struct XipPool {
    pub hw: Hw,
}

impl XipPool {
    pub const fn new(hw: Hw) -> Self {
        Self { hw }
    }

    pub fn pool_info(&self) -> Option<PacketPoolInfo> {
        self.hw.pkt_pool_info().ok()
    }
}

pub(crate) struct XipPacket {
    hw: Hw,
    handle: u32,
    base: *mut u8,
    cap: usize,
    range: Range<usize>,
}

impl core::fmt::Debug for XipPacket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XipPacket")
            .field("handle", &self.handle)
            .field("range", &self.range)
            .finish()
    }
}

impl XipPacket {
    /// Wrap an owned runtime packet handle; frees `handle` if `pkt_span` or `range` is invalid.
    pub fn from_owned(hw: Hw, handle: u32, range: Range<usize>) -> Result<Self, i32> {
        match hw.pkt_span(handle) {
            Ok((base, cap)) => {
                if range.start > range.end || range.end > cap {
                    hw.pkt_free(handle);
                    return Err(xip_abi::ERR_ARG);
                }
                Ok(Self {
                    hw,
                    handle,
                    base,
                    cap,
                    range,
                })
            }
            Err(rc) => {
                hw.pkt_free(handle);
                Err(rc)
            }
        }
    }

    /// Return the underlying runtime packet handle.
    #[inline]
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Consume the Rust guard without calling `pkt_free`, returning `(handle, base, cap, range)`.
    #[inline]
    pub fn into_raw_parts(self) -> (u32, *mut u8, usize, Range<usize>) {
        let out = (self.handle, self.base, self.cap, self.range.clone());
        core::mem::forget(self);
        out
    }

    /// Reconstruct from parts previously extracted via [`Self::into_raw_parts`] when a hostcall refused ownership.
    #[inline]
    pub unsafe fn from_raw_parts(
        hw: Hw,
        handle: u32,
        base: *mut u8,
        cap: usize,
        range: Range<usize>,
    ) -> Self {
        Self {
            hw,
            handle,
            base,
            cap,
            range,
        }
    }
}

impl Drop for XipPacket {
    fn drop(&mut self) {
        if self.handle != 0 {
            self.hw.pkt_free(self.handle);
        }
    }
}

impl PoolPacket for XipPacket {
    #[inline]
    fn slot_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.base, self.cap) }
    }

    #[inline]
    fn slot_slice_mut(&mut self) -> Result<&mut [u8], PoolError> {
        if self.hw.pkt_buf(self.handle).is_err() {
            // Copy-on-write when another reference to `self.handle` is still live.
            let new_handle = self.hw.pkt_alloc().map_err(|_| PoolError::Exhausted)?;
            let (new_base, new_cap) = match self.hw.pkt_span(new_handle) {
                Ok(pair) => pair,
                Err(_) => {
                    self.hw.pkt_free(new_handle);
                    return Err(PoolError::Exhausted);
                }
            };
            unsafe {
                core::ptr::copy_nonoverlapping(self.base, new_base, self.cap.min(new_cap));
            }
            self.hw.pkt_free(self.handle);
            self.handle = new_handle;
            self.base = new_base;
            self.cap = new_cap;
        }
        Ok(unsafe { core::slice::from_raw_parts_mut(self.base, self.cap) })
    }

    #[inline]
    fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    #[inline]
    fn set_range(&mut self, range: Range<usize>) -> Result<(), PoolError> {
        if range.start > range.end || range.end > self.cap {
            return Err(PoolError::InvalidRange);
        }
        self.range = range;
        Ok(())
    }

    #[inline]
    fn is_unique(&self) -> bool {
        self.hw.pkt_buf(self.handle).is_ok()
    }

    fn try_clone(&self) -> Result<Self, PoolError> {
        if self.hw.pkt_keep(self.handle) != 0 {
            return Err(PoolError::Exhausted);
        }
        Ok(Self {
            hw: self.hw,
            handle: self.handle,
            base: self.base,
            cap: self.cap,
            range: self.range.clone(),
        })
    }
}

impl PacketPool for XipPool {
    type Packet = XipPacket;

    #[inline]
    fn headroom(&self) -> usize {
        self.hw
            .pkt_info()
            .map_or(PKT_HEADROOM as usize, |(h, _)| h as usize)
    }

    #[inline]
    fn max_payload(&self) -> usize {
        self.hw
            .pkt_info()
            .map_or(PKT_MAX as usize, |(_, m)| m as usize)
    }

    fn alloc(&self) -> Result<Self::Packet, PoolError> {
        let handle = self.hw.pkt_alloc().map_err(|_| PoolError::Exhausted)?;
        let h = self.headroom();
        XipPacket::from_owned(self.hw, handle, h..h).map_err(|_| PoolError::Exhausted)
    }

    fn alloc_reserving(&self, reserved: usize) -> Result<Self::Packet, PoolError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let res_u32 = u32::try_from(reserved).map_err(|_| PoolError::Exhausted)?;
            match self.hw.pkt_acquire(res_u32) {
                Ok(handle) => {
                    let h = self.headroom();
                    return XipPacket::from_owned(self.hw, handle, h..h)
                        .map_err(|_| PoolError::Exhausted);
                }
                Err(xip_abi::ERR_UNSUPPORTED) => {}
                Err(_) => return Err(PoolError::Exhausted),
            }
        }
        if let Some(info) = self.pool_info() {
            if (info.available as usize) <= reserved {
                return Err(PoolError::Exhausted);
            }
        }
        self.alloc()
    }

    fn available(&self) -> usize {
        self.pool_info().map_or(16, |i| i.available as usize)
    }
}

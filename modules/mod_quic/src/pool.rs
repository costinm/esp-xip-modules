//! Native leases, not a second packet pool. Only the serialized module owner
//! calls these adapters; no SDK task receives a Rust pointer or vtable.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};
use quic_lite::{OwnedPacket, PacketBuffer, PacketPool, PacketWriter};
use xip_abi::*;
#[derive(Clone, Copy)]
pub(crate) struct Native {
    pub hw: Hw,
    pub table: &'static Quic,
}
// The private adapter is passed only between data structures driven on the one
// module worker. It is never registered as a native/SDK callback context.
unsafe impl Send for Native {}
unsafe impl Sync for Native {}
impl Native {
    pub fn get(hw: Hw) -> Result<Self, i32> {
        let p = hw.get_component(c"quic").cast::<Quic>();
        if p.is_null() {
            return Err(ERR_UNSUPPORTED);
        }
        let table = unsafe { &*p };
        if table.version != QUIC_VERSION || (table.size as usize) < core::mem::size_of::<Quic>() {
            return Err(ERR_UNSUPPORTED);
        }
        Ok(Self { hw, table })
    }
    pub fn info(&self) -> Result<QuicPoolInfo, i32> {
        let mut out = QuicPoolInfo {
            size: core::mem::size_of::<QuicPoolInfo>() as u32,
            ..Default::default()
        };
        let rc = unsafe { self.table.pool_info.ok_or(ERR_UNSUPPORTED)?(self.hw.ctx(), &mut out) };
        if rc < 0 {
            Err(rc)
        } else {
            Ok(out)
        }
    }
}
struct Lease {
    native: Native,
    handle: AtomicU32,
    base: *mut u8,
    len: usize,
}
unsafe impl Send for Lease {}
unsafe impl Sync for Lease {}
impl Drop for Lease {
    fn drop(&mut self) {
        let h = self.handle.swap(0, Ordering::AcqRel);
        if h != 0 {
            self.native.hw.pkt_free(h);
        }
    }
}
#[derive(Clone)]
pub(crate) struct Buffer(Arc<Lease>);
impl Buffer {
    pub fn owned(native: Native, handle: u32) -> Result<Self, i32> {
        match native.hw.pkt_span(handle) {
            Ok((base, len)) => Ok(Self(Arc::new(Lease {
                native,
                handle: AtomicU32::new(handle),
                base,
                len,
            }))),
            Err(rc) => {
                native.hw.pkt_free(handle);
                Err(rc)
            }
        }
    }
    pub fn handle(&self) -> u32 {
        self.0.handle.load(Ordering::Acquire)
    }
    pub fn moved(&self) {
        self.0.handle.store(0, Ordering::Release);
    }
    pub fn restored(&self, handle: u32) -> Result<(), i32> {
        let (base, len) = self.0.native.hw.pkt_span(handle)?;
        if base != self.0.base || len != self.0.len || self.handle() != 0 {
            return Err(ERR_ARG);
        }
        self.0.handle.store(handle, Ordering::Release);
        Ok(())
    }
}
impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        assert_ne!(self.handle(), 0, "physical driver owns this lease");
        unsafe { core::slice::from_raw_parts(self.0.base, self.0.len) }
    }
}
impl PacketBuffer for Buffer {
    fn as_mut_unique(&mut self) -> Option<&mut [u8]> {
        if Arc::strong_count(&self.0) != 1 || self.handle() == 0 {
            return None;
        }
        let (p, n) = self.0.native.hw.pkt_span(self.handle()).ok()?;
        Some(unsafe { core::slice::from_raw_parts_mut(p, n) })
    }
}
pub(crate) struct Pool {
    pub native: Native,
}
pub(crate) struct Writer {
    buffer: Buffer,
    start: usize,
    cap: usize,
}
impl PacketWriter for Writer {
    type Buffer = Buffer;
    fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.as_mut_unique().expect("new native writer")
            [self.start..self.start + self.cap]
    }
    fn commit(self, len: usize) -> Option<OwnedPacket<Buffer>> {
        if len > self.cap {
            return None;
        }
        OwnedPacket::new(self.buffer, self.start..self.start + len).ok()
    }
}
impl PacketPool for Pool {
    type Buffer = Buffer;
    type Writer = Writer;
    fn acquire_writer(&'static self, headroom: usize, reserved: usize) -> Option<Writer> {
        let info = self.native.info().ok()?;
        let start = headroom.max(info.headroom as usize);
        let room =
            (info.slot_bytes as usize).checked_sub(start + quic_lite::PACKET_SUFFIX_RESERVE)?;
        let cap = room.min(info.max_packet as usize);
        let mut handle = 0;
        let rc = unsafe {
            self.native.table.acquire?(
                self.native.hw.ctx(),
                u32::try_from(reserved).ok()?,
                &mut handle,
            )
        };
        if rc < 0 {
            return None;
        }
        let buffer = Buffer::owned(self.native, handle).ok()?;
        Some(Writer { buffer, start, cap })
    }
    fn capacity(&self) -> usize {
        self.native.info().map_or(0, |i| i.capacity as usize)
    }
    fn available(&self) -> usize {
        self.native.info().map_or(0, |i| i.available as usize)
    }
}

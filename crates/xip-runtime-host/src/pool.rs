//! Shared physical slots with allocation generations and reference counts.
//! Modules own their references; managed heap accounting is separate.
use std::alloc::{alloc, dealloc, Layout};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;

/// The shared pool of the runtime (single threaded, like the runtime).
pub struct Pool {
    slots: u16,
    slot_size: u16,
    keep: Cell<u16>,
    free: Cell<u32>,
    buffers: Cell<u32>,
    refs: RefCell<Vec<u32>>,
    mem: RefCell<Vec<Option<Box<[u8]>>>>,
    /// Storage of every slot in one block of memory the pool does not own (the shared memory of wasm modules, `attach_region`).
    region: Cell<*mut u8>,
    generations: RefCell<Vec<u16>>,
    packets: RefCell<Vec<PacketRef>>,
}

impl Pool {
    pub fn new(slots: u16) -> Pool {
        Pool::with_slot_size(slots, xip_abi::PKT_SLOT_MIN as u16)
    }
    pub fn with_slot_size(slots: u16, slot_size: u16) -> Pool {
        assert!(slots > 0 && slot_size > 0);
        Pool {
            generations: RefCell::new(vec![0;slots as usize]),
            packets: RefCell::new(vec![PacketRef {slot:0,start:0,len:0};slots as usize]),
            slots,
            slot_size,
            keep: Cell::new(2),
            free: Cell::new(slots as u32),
            buffers: Cell::new(0),
            refs: RefCell::new(vec![0; slots as usize]),
            mem: RefCell::new((0..slots).map(|_| None).collect()),
            region: Cell::new(std::ptr::null_mut()),
        }
    }
    /// Bytes `attach_region` needs.
    pub fn region_bytes(&self) -> usize {
        self.slots as usize * self.slot_size as usize
    }
    /// Keep the storage of every slot in `ptr` (`region_bytes()` bytes that outlive the pool), so that wasm modules can reach the
    /// packets. Only while no packet is in use; the per-slot allocation and trimming stop.
    pub fn attach_region(&self, ptr: *mut u8) -> Result<(), String> {
        if self.free.get() != self.slots as u32 {
            return Err("the packet pool is in use: load wasm modules before the first packet".into());
        }
        let mut mem = self.mem.borrow_mut();
        for m in mem.iter_mut() {
            *m = None;
        }
        self.buffers.set(0);
        self.region.set(ptr);
        Ok(())
    }
    pub fn slots(&self) -> u16 {
        self.slots
    }
    pub fn slot_size(&self) -> u16 {
        self.slot_size
    }
    /// Idle slots that keep their storage (default 2).
    pub fn set_keep(&self, keep: u16) {
        self.keep.set(keep);
    }
    /// Slots that hold storage now.
    pub fn allocated(&self) -> u16 {
        self.buffers.get() as u16
    }
    /// Give back the storage of idle slots beyond `keep`.
    pub fn trim(&self) {
        let (refs, mut mem) = (self.refs.borrow(), self.mem.borrow_mut());
        for i in 0..self.slots as usize {
            if self.buffers.get() <= self.keep.get() as u32 + (self.slots as u32 - self.free.get()) {
                return;
            }
            if mem[i].is_some() && refs[i] == 0 {
                mem[i] = None;
                self.buffers.set(self.buffers.get() - 1);
            }
        }
    }

    /// A free slot with one reference, or `None` (no more than `reserve` slots free).
    pub fn acquire(&self, reserve: u16) -> Option<i32> {
        if self.free.get() <= reserve as u32 {
            return None;
        }
        self.free.set(self.free.get() - 1);
        let (mut refs, mut mem) = (self.refs.borrow_mut(), self.mem.borrow_mut());
        for pass in 0..2 {
            // slots that already have storage first, then any
            for i in 0..self.slots as usize {
                if (pass == 0 && mem[i].is_none() && self.region.get().is_null()) || refs[i] != 0 {
                    continue;
                }
                refs[i] = 1;
                let mut g=self.generations.borrow_mut();g[i]=g[i].wrapping_add(1);
                if mem[i].is_none() && self.region.get().is_null() {
                    mem[i] = Some(vec![0u8; self.slot_size as usize].into_boxed_slice());
                    self.buffers.set(self.buffers.get() + 1);
                }
                return Some(i as i32);
            }
        }
        self.free.set(self.free.get() + 1);
        None
    }
    pub fn retain(&self, slot: i32) {
        if let Some(r) = self.refs.borrow_mut().get_mut(slot as usize).filter(|_| slot >= 0) {
            *r += 1;
        }
    }
    /// The last release frees the slot (and its storage, if more than `keep` idle slots have some). A release of a slot
    /// that is not in use is ignored.
    pub fn release(&self, slot: i32) {
        if slot < 0 || slot >= self.slots as i32 {
            return;
        }
        let i = slot as usize;
        let (mut refs, mut mem) = (self.refs.borrow_mut(), self.mem.borrow_mut());
        match refs[i] {
            0 => {}
            1 => {
                let in_use = self.slots as u32 - self.free.get();
                if self.buffers.get() + 1 > in_use + self.keep.get() as u32 && mem[i].is_some() {
                    mem[i] = None;
                    self.buffers.set(self.buffers.get() - 1);
                }
                refs[i] = 0;
                self.free.set(self.free.get() + 1);
            }
            _ => refs[i] -= 1,
        }
    }
    pub fn refs(&self, slot: i32) -> u32 {
        if slot >= 0 && slot < self.slots as i32 { self.refs.borrow()[slot as usize] } else { 0 }
    }
    /// The slot's first byte; null if it has no storage. Valid until the slot is released.
    pub fn slot(&self, slot: i32) -> *mut u8 {
        if slot < 0 || slot >= self.slots as i32 {
            return std::ptr::null_mut();
        }
        if !self.region.get().is_null() {
            return unsafe { self.region.get().add(slot as usize * self.slot_size as usize) };
        }
        self.mem.borrow_mut()[slot as usize].as_mut().map_or(std::ptr::null_mut(), |b| b.as_mut_ptr())
    }
    pub fn available(&self) -> u16 {
        self.free.get() as u16
    }
}

#[derive(Clone,Copy,Debug)]
pub struct PacketRef { pub slot:i16, pub start:u16, pub len:u16 }
impl Pool {
    pub fn handle(&self,slot:i32,start:u16,len:u16)->u32 {
        if self.refs(slot)==0 {return 0;}
        self.packets.borrow_mut()[slot as usize]=PacketRef{slot:slot as i16,start,len};
        ((self.generations.borrow()[slot as usize] as u32)<<16)|(slot as u32+1)
    }
    pub fn get(&self,handle:u32)->Option<PacketRef> {
        let i=(handle&0xffff).checked_sub(1)? as usize;
        if i>=self.slots as usize || self.refs(i as i32)==0 || self.generations.borrow()[i] as u32 != handle>>16 {return None;}
        Some(self.packets.borrow()[i])
    }
    pub fn keep(&self,handle:u32)->i32 {
        let Some(p)=self.get(handle) else {return -1;}; self.retain(p.slot as i32);0
    }
}

/// The managed heap of one module: the bytes it took with `mem_alloc` and has not given back, and a quota. A block still
/// held when the module's instance is dropped is freed then (the device cannot, which is why it counts as a leak).
pub struct Mem {
    pub used: u32,
    pub quota: u32,
    blocks: HashMap<usize, Layout>,
    /// A wasm module's heap: blocks are carved from this memory (first fit) instead of malloc, so that the module can address them.
    arena: Option<Arena>,
}

struct Arena {
    ptr: *mut u8,
    len: usize,
    /// Offset to size of the live blocks.
    blocks: std::collections::BTreeMap<usize, usize>,
}

impl Mem {
    pub fn new(quota: u32) -> Box<Mem> {
        Box::new(Mem { used: 0, quota, blocks: HashMap::new(), arena: None })
    }
    /// Blocks live in `len` bytes at `ptr` (inside the shared memory of the wasm modules).
    pub fn with_arena(quota: u32, ptr: *mut u8, len: usize) -> Box<Mem> {
        Box::new(Mem { used: 0, quota, blocks: HashMap::new(), arena: Some(Arena { ptr, len, blocks: Default::default() }) })
    }
    /// Null: over the quota, zero size or no memory. Aligned to 16 for any type.
    pub fn alloc(&mut self, size: usize) -> *mut c_void {
        if size == 0 || size > self.quota as usize || self.used as usize > self.quota as usize - size {
            return std::ptr::null_mut();
        }
        if let Some(a) = self.arena.as_mut() {
            let size = size.next_multiple_of(16);
            let mut at = 0usize;
            for (&p, &s) in &a.blocks {
                if p - at >= size {
                    break;
                }
                at = p + s;
            }
            if at + size > a.len {
                return std::ptr::null_mut();
            }
            a.blocks.insert(at, size);
            self.used += size as u32;
            return unsafe { a.ptr.add(at) }.cast();
        }
        let layout = Layout::from_size_align(size, 16).unwrap();
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            return p.cast();
        }
        self.blocks.insert(p as usize, layout);
        self.used += size as u32;
        p.cast()
    }
    /// `false` for a pointer that is not a live block of this module.
    pub fn free(&mut self, ptr: *mut c_void) -> bool {
        if let Some(a) = self.arena.as_mut() {
            let off = (ptr as usize).wrapping_sub(a.ptr as usize);
            return match a.blocks.remove(&off) {
                Some(s) => {
                    self.used -= s as u32;
                    true
                }
                None => false,
            };
        }
        match self.blocks.remove(&(ptr as usize)) {
            Some(layout) => {
                self.used -= layout.size() as u32;
                unsafe { dealloc(ptr.cast(), layout) };
                true
            }
            None => false,
        }
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        for (p, layout) in self.blocks.drain() {
            unsafe { dealloc(p as *mut u8, layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_references_and_the_reserve() {
        let p = Pool::with_slot_size(8, 1204);
        assert_eq!((p.available(), p.slots(), p.slot_size()), (8, 8, 1204));
        let (a, b) = (p.acquire(0).unwrap(), p.acquire(0).unwrap());
        assert!(a != b && p.available() == 6);
        p.retain(a);
        p.release(a);
        assert!(p.available() == 6 && p.refs(a) == 1);
        p.release(a);
        assert!(p.available() == 7 && p.refs(a) == 0);
        p.release(a); // a double release does nothing
        assert_eq!(p.available(), 7);
        p.release(b);
        assert_eq!(p.available(), 8);

        // a module cannot take the last slots
        let got: Vec<i32> = std::iter::from_fn(|| p.acquire(3)).collect();
        assert!(got.len() == 5 && p.available() == 3);
        let net = p.acquire(0).unwrap(); // the application still can
        assert_eq!(p.available(), 2);
        for g in got {
            p.release(g);
        }
        p.release(net);
        assert_eq!(p.available(), 8);
    }

    #[test]
    fn storage_is_dynamic() {
        let p = Pool::with_slot_size(8, 1204);
        let all: Vec<i32> = std::iter::from_fn(|| p.acquire(0)).collect();
        assert_eq!(all.len(), 8);
        for s in all {
            p.release(s);
        }
        assert_eq!(p.allocated(), 2, "the two kept idle of the eight that were used");
        let tt: Vec<i32> = (0..5).map(|_| p.acquire(0).unwrap()).collect();
        assert!(p.allocated() == 5 && !p.slot(tt[0]).is_null() && !p.slot(tt[4]).is_null() && p.slot(8).is_null());
        for s in tt {
            p.release(s);
        }
        assert!(p.allocated() == 2 && p.available() == 8);
        p.set_keep(0);
        p.trim();
        assert_eq!(p.allocated(), 0, "keep 0: everything goes back");
    }

    #[test]
    fn memory_balance_and_quota() {
        let mut m = Mem::new(1000);
        let (x1, x2) = (m.alloc(400), m.alloc(400));
        assert!(!x1.is_null() && !x2.is_null() && m.used == 800 && x1 as usize & 15 == 0);
        assert!(m.alloc(400).is_null(), "over the quota");
        assert!(m.alloc(0).is_null() && m.alloc(100000).is_null());
        assert!(m.free(x1) && m.used == 400);
        assert!(!m.free(std::ptr::null_mut()));
        let x3 = m.alloc(600); // the freed bytes are usable again
        assert!(!x3.is_null() && m.used == 1000);
        assert!(m.free(x2) && m.free(x3) && m.used == 0);
    }
}

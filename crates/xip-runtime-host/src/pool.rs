//! The packet pool, the table of packets one module holds, and the balance of the memory it took with `mem_alloc`.
//! Pure Rust, with the semantics of the C code of the device runtime (`components/xip_runtime/src/xip_pool.c`): slots with
//! reference counts and a reserve kept for the application, storage allocated when a slot is first used and given back when
//! more than `keep` idle slots hold some, handles that carry a generation, a quota of owned packets, a byte quota.
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
}

impl Pool {
    pub fn new(slots: u16) -> Pool {
        Pool::with_slot_size(slots, xip_abi::PKT_SLOT_MIN as u16)
    }
    pub fn with_slot_size(slots: u16, slot_size: u16) -> Pool {
        assert!(slots > 0 && slot_size > 0);
        Pool {
            slots,
            slot_size,
            keep: Cell::new(2),
            free: Cell::new(slots as u32),
            buffers: Cell::new(0),
            refs: RefCell::new(vec![0; slots as usize]),
            mem: RefCell::new((0..slots).map(|_| None).collect()),
        }
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
                if (pass == 0 && mem[i].is_none()) || refs[i] != 0 {
                    continue;
                }
                refs[i] = 1;
                if mem[i].is_none() {
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
        self.mem.borrow_mut()[slot as usize].as_mut().map_or(std::ptr::null_mut(), |b| b.as_mut_ptr())
    }
    pub fn available(&self) -> u16 {
        self.free.get() as u16
    }
}

pub const HELD_MAX: usize = 8;
pub const HELD_FREE: u8 = 0;
pub const HELD_OWNED: u32 = 1;
pub const HELD_BORROWED: u32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct HeldEntry {
    pub slot: i16,
    /// The payload inside the slot: the range belongs to the holder.
    pub start: u16,
    pub len: u16,
    pub gen: u8,
    pub state: u8,
}

/// What one module holds: owned packets (up to a quota) and the one borrowed for the current call.
pub struct Held {
    pub e: [HeldEntry; HELD_MAX],
    pub quota: u8,
}

impl Held {
    pub fn new(quota: u8) -> Box<Held> {
        Box::new(Held { e: [HeldEntry { slot: 0, start: 0, len: 0, gen: 0, state: HELD_FREE }; HELD_MAX], quota: quota.min(HELD_MAX as u8) })
    }
    fn handle(&self, i: usize) -> u32 {
        (self.e[i].gen as u32) << 8 | (i as u32 + 1)
    }
    pub fn count(&self, state: u32) -> u32 {
        self.e.iter().filter(|e| e.state as u32 == state).count() as u32
    }
    /// A handle (never 0) for a slot the table now owns a reference of; 0 when the table is full or, for an owned packet,
    /// at its quota.
    pub fn add(&mut self, slot: i32, start: u16, len: u16, state: u32) -> u32 {
        if state == HELD_OWNED && self.count(HELD_OWNED) >= self.quota as u32 {
            return 0;
        }
        let Some(i) = self.e.iter().position(|e| e.state == HELD_FREE) else { return 0 };
        let e = &mut self.e[i];
        e.slot = slot as i16;
        e.start = start;
        e.len = len;
        e.state = state as u8;
        e.gen = e.gen.wrapping_add(1);
        if e.gen == 0 {
            e.gen = 1; // 0 would make the handle 0x00NN
        }
        self.handle(i)
    }
    fn index(&self, handle: u32) -> Option<usize> {
        let i = (handle & 0xff) as usize;
        if i == 0 || i > HELD_MAX {
            return None;
        }
        (self.e[i - 1].state != HELD_FREE && handle == self.handle(i - 1)).then_some(i - 1)
    }
    pub fn get(&mut self, handle: u32) -> Option<HeldEntry> {
        self.index(handle).map(|i| self.e[i])
    }
    /// Borrowed -> owned. 0, or -1 for a bad handle, -2 at the quota.
    pub fn keep(&mut self, handle: u32) -> i32 {
        let Some(i) = self.index(handle) else { return -1 };
        if self.e[i].state as u32 == HELD_OWNED {
            return 0;
        }
        if self.count(HELD_OWNED) >= self.quota as u32 {
            return -2;
        }
        self.e[i].state = HELD_OWNED as u8;
        0
    }
    /// The entry leaves the table; the caller releases or hands on the slot reference.
    pub fn take(&mut self, handle: u32) -> Option<HeldEntry> {
        let i = self.index(handle)?;
        let copy = self.e[i];
        self.e[i].state = HELD_FREE; // gen stays: the old handle no longer matches the next use
        Some(copy)
    }
}

/// The managed heap of one module: the bytes it took with `mem_alloc` and has not given back, and a quota. A block still
/// held when the module's instance is dropped is freed then (the device cannot, which is why it counts as a leak).
pub struct Mem {
    pub used: u32,
    pub quota: u32,
    blocks: HashMap<usize, Layout>,
}

impl Mem {
    pub fn new(quota: u32) -> Box<Mem> {
        Box::new(Mem { used: 0, quota, blocks: HashMap::new() })
    }
    /// Null: over the quota, zero size or no memory. Aligned to 16 for any type.
    pub fn alloc(&mut self, size: usize) -> *mut c_void {
        if size == 0 || size > self.quota as usize || self.used as usize > self.quota as usize - size {
            return std::ptr::null_mut();
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
    fn handles_generations_and_quota() {
        let mut h = Held::new(2);
        let x = h.add(3, 144, 10, HELD_OWNED);
        let y = h.add(4, 144, 0, HELD_OWNED);
        assert!(x != 0 && y != 0 && x != y && h.add(5, 144, 0, HELD_OWNED) == 0, "quota 2");
        let c = h.add(6, 144, 5, HELD_BORROWED); // borrowed ones are not counted
        assert!(c != 0 && h.get(c).unwrap().slot == 6 && h.get(x).unwrap().len == 10);
        assert_eq!(h.keep(c), -2, "keeping would pass the quota");
        assert_eq!(h.take(x).unwrap().slot, 3);
        assert!(h.get(x).is_none(), "the handle is gone");
        assert!(h.take(x).is_none(), "taking twice finds nothing");
        assert!(h.keep(c) == 0 && h.get(c).unwrap().state as u32 == HELD_OWNED);
        assert_eq!(h.add(7, 144, 0, HELD_OWNED), 0, "y and c are the two");
        h.take(y);
        let z = h.add(7, 144, 0, HELD_OWNED);
        assert!(z != 0 && z != x && z != y && h.get(y).is_none() && h.get(x).is_none());
        assert!(h.get(0).is_none() && h.get(0xff).is_none() && h.get(0xffffff).is_none());
        assert_eq!(h.count(HELD_OWNED), 2);
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

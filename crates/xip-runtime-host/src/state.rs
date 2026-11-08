//! The state objects of a module (`state_resize`, `state_get`; `Host::state_resize` in xip-abi). Instance objects live in the
//! module's managed heap (`Mem`, counted in its quota) and are freed when the instance ends. Retained objects belong to the module
//! name and version: they are kept for the next instance of that name and version, under a device-wide byte cap and a cap per
//! name, and dropped when an instance with another version starts. Same rules as the C runtime (components/xip_runtime/src/xip_state.c).
use crate::runtime::InstState;
use xip_abi::{ERR_ARG, ERR_NOMEM, ERR_SIZE, ERR_UNSUPPORTED, STATE_INSTANCE, STATE_PSRAM, STATE_RETAINED, STATE_RTC};

/// The byte cap of all retained objects (`CONFIG_XIP_RETAINED_MAX` of the C runtime).
pub const RETAINED_MAX: usize = 8192;
/// Retained objects per module name.
pub const RETAINED_OBJECTS: usize = 8;

/// An instance object. The root (key 0) is not charged to the quota: it is the module's own state. `fixed` marks the image's
/// state block (the header's `state_bytes`), which the instance keeps and which is not resized; other objects are blocks of `Mem`.
pub(crate) struct Obj {
    pub key: u32,
    pub ptr: *mut u8,
    pub size: usize,
    pub charged: bool,
    pub fixed: bool,
}

#[derive(Default)]
pub(crate) struct Retained {
    pub mods: Vec<RetainedMod>,
    /// Bytes of all retained objects.
    pub bytes: usize,
}

pub(crate) struct RetainedMod {
    pub name: String,
    pub version: u32,
    pub objs: Vec<RetObj>,
}

pub(crate) struct RetObj {
    pub key: u32,
    pub data: Box<[u8]>,
}

impl Retained {
    fn module(&self, name: &str, version: u32) -> Option<usize> {
        self.mods.iter().position(|m| m.name == name && m.version == version)
    }
    fn size_of(&self, name: &str, version: u32, key: u32) -> Option<usize> {
        let m = &self.mods[self.module(name, version)?];
        m.objs.iter().find(|o| o.key == key).map(|o| o.data.len())
    }
    /// Drop what a module of `name` retained for another version: a new version starts with none of the old one's objects.
    pub fn forget_other_versions(&mut self, name: &str, version: u32) {
        let mut dropped = 0;
        self.mods.retain(|m| {
            if m.name == name && m.version != version {
                dropped += m.objs.iter().map(|o| o.data.len()).sum::<usize>();
                false
            } else {
                true
            }
        });
        self.bytes -= dropped;
    }
}

/// The pointer and size of object `key`: an instance object, then a retained one of the module.
pub(crate) fn get(st: &InstState, ret: &Retained, name: &str, version: u32, key: u32) -> Option<(*mut u8, usize)> {
    if let Some(o) = st.state_objs.iter().find(|o| o.key == key) {
        return Some((o.ptr, o.size));
    }
    let m = &ret.mods[ret.module(name, version)?];
    m.objs.iter().find(|o| o.key == key).map(|o| (o.data.as_ptr() as *mut u8, o.data.len()))
}

/// `Host::state_resize`: create, resize or free an object; a pointer, or the error code. `can_retain` is false for a wasm
/// module, whose retained objects would have to live in memory that outlives its slot (`ERR_UNSUPPORTED`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resize(st: &mut InstState, ret: &mut Retained, name: &str, version: u32, can_retain: bool, key: u32, class: u32, size: usize) -> Result<*mut u8, i32> {
    if class == STATE_PSRAM || class == STATE_RTC {
        return Err(ERR_UNSUPPORTED);
    }
    if class != STATE_INSTANCE && class != STATE_RETAINED {
        return Err(ERR_ARG);
    }
    let in_inst = st.state_objs.iter().any(|o| o.key == key);
    let in_ret = !in_inst && ret.size_of(name, version, key).is_some();
    if class == STATE_INSTANCE {
        return if in_ret { Err(ERR_ARG) } else { instance(st, key, size) };
    }
    if key == 0 || in_inst {
        return Err(ERR_ARG);   // the root is an instance object; a key of this instance is not retained
    }
    if !can_retain {
        return Err(ERR_UNSUPPORTED);
    }
    retained(ret, name, version, key, size)
}

fn instance(st: &mut InstState, key: u32, size: usize) -> Result<*mut u8, i32> {
    let idx = st.state_objs.iter().position(|o| o.key == key);
    let old = match idx {
        Some(i) if st.state_objs[i].fixed => {
            // the image's state block keeps the size its header gives
            return if size == st.state_objs[i].size { Ok(st.state_objs[i].ptr) } else { Err(ERR_UNSUPPORTED) };
        }
        Some(i) => st.state_objs[i].size,
        None => 0,
    };
    if size == 0 {
        if let Some(i) = idx {
            let o = st.state_objs.remove(i);
            st.mem.free(o.ptr.cast());
        }
        return Ok(std::ptr::null_mut());
    }
    if let Some(i) = idx {
        if size == old {
            return Ok(st.state_objs[i].ptr);
        }
    }
    // the root is the module's own state and is not charged; other objects count against the quota, the new block and not the
    // old one (given back once its contents are copied)
    let charged = key != 0;
    if charged {
        let quota = st.mem.quota as usize;
        if size > quota || st.mem.used as usize - old > quota - size {
            return Err(ERR_SIZE);
        }
        st.mem.used -= old as u32;
    }
    let p = if charged { st.mem.alloc(size) } else { st.mem.alloc_uncharged(size) } as *mut u8;
    if charged {
        st.mem.used += old as u32;
    }
    if p.is_null() {
        return Err(ERR_NOMEM);
    }
    // SAFETY: `p` is a fresh block of `size` bytes; the old block (if any) holds `old` bytes and is still live
    unsafe { std::ptr::write_bytes(p, 0, size) };
    match idx {
        Some(i) => {
            let old_ptr = st.state_objs[i].ptr;
            unsafe { std::ptr::copy_nonoverlapping(old_ptr, p, old.min(size)) };
            st.mem.free(old_ptr.cast());
            st.state_objs[i].ptr = p;
            st.state_objs[i].size = size;
        }
        None => st.state_objs.push(Obj { key, ptr: p, size, charged, fixed: false }),
    }
    Ok(p)
}

fn retained(ret: &mut Retained, name: &str, version: u32, key: u32, size: usize) -> Result<*mut u8, i32> {
    let mi = ret.module(name, version);
    let existing = mi.and_then(|m| ret.mods[m].objs.iter().position(|o| o.key == key));
    let old = existing.map_or(0, |o| ret.mods[mi.unwrap()].objs[o].data.len());
    if size == 0 {
        if let (Some(m), Some(o)) = (mi, existing) {
            ret.mods[m].objs.remove(o);
            ret.bytes -= old;
            if ret.mods[m].objs.is_empty() {
                ret.mods.remove(m);
            }
        }
        return Ok(std::ptr::null_mut());
    }
    if existing.is_some() && size == old {
        return Ok(ret.mods[mi.unwrap()].objs[existing.unwrap()].data.as_ptr() as *mut u8);
    }
    if size > RETAINED_MAX || ret.bytes - old > RETAINED_MAX - size {
        return Err(ERR_SIZE);
    }
    if existing.is_none() && mi.map_or(0, |m| ret.mods[m].objs.len()) >= RETAINED_OBJECTS {
        return Err(ERR_SIZE);
    }
    let mut data = vec![0u8; size].into_boxed_slice();
    if let Some(o) = existing {
        let m = mi.unwrap();
        data[..old.min(size)].copy_from_slice(&ret.mods[m].objs[o].data[..old.min(size)]);
    }
    let p = data.as_mut_ptr();
    match (mi, existing) {
        (Some(m), Some(o)) => ret.mods[m].objs[o].data = data,
        (Some(m), None) => ret.mods[m].objs.push(RetObj { key, data }),
        (None, _) => ret.mods.push(RetainedMod { name: name.to_string(), version, objs: vec![RetObj { key, data }] }),
    }
    ret.bytes = ret.bytes - old + size;
    Ok(p)
}

/// Give back the instance objects when the instance ends (their heap balance leaves the quota before it is read).
pub(crate) fn release(st: &mut InstState) {
    for o in st.state_objs.drain(..) {
        if !o.fixed {
            st.mem.free(o.ptr.cast());
        }
    }
}

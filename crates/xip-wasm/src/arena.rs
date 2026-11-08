//! Prototype of the memory layout where Linux does the mapping (see `modules/mod_wasm3/WASM-PORTING.md`): every module instance gets its own linear
//! memory, `private` bytes of anonymous memory at address 0 (data, stack, heap: nothing else can reach them) and, right above, the same
//! shared pool in every instance at the same address (`private`), a `memfd` mapped `MAP_SHARED`. Packets live in the pool; nothing else is shared,
//! so code that depends on another module's heap faults (an out-of-bounds trap) instead of working by accident.
//!
//! Order matters: wasmi zero-fills a static buffer when it makes the memory, which would erase a pool already in use, so the range is
//! reserved and handed to wasmi first, and the pool is mapped over its tail afterwards (wasmi keeps only a pointer and a length).
use std::io;
use wasmi::{AsContextMut, Memory, MemoryType};

const PAGE: usize = 65536;

/// The shared pool: a `memfd` every instance maps.
pub struct SharedPool {
    fd: libc::c_int,
    len: usize,
}

impl SharedPool {
    pub fn new(bytes: usize) -> io::Result<SharedPool> {
        let len = bytes.next_multiple_of(PAGE);
        // SAFETY: plain syscalls
        let fd = unsafe { libc::memfd_create(c"xip-pool".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(SharedPool { fd, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// The private memory + pool of one instance. The mapping lives as long as the process (wasmi wants a `'static` buffer): unmap with
    /// [`Arena::unmap`] when the instance is dropped.
    pub fn memory(&self, mut cx: impl AsContextMut, private: usize) -> io::Result<Arena> {
        let private = private.next_multiple_of(PAGE);
        let total = private + self.len;
        // SAFETY: a fresh anonymous mapping, then the pool over its tail
        unsafe {
            let p = libc::mmap(std::ptr::null_mut(), total, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let all: &'static mut [u8] = std::slice::from_raw_parts_mut(p as *mut u8, total);
            let pages = (total / PAGE) as u32;
            let memory = Memory::new_static(&mut cx, MemoryType::new(pages, Some(pages)), all).map_err(|e| {
                libc::munmap(p, total);
                io::Error::other(e.to_string())
            })?;
            let at = (p as *mut u8).add(private) as *mut libc::c_void;
            if libc::mmap(at, self.len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, self.fd, 0) == libc::MAP_FAILED {
                let e = io::Error::last_os_error();
                libc::munmap(p, total);
                return Err(e);
            }
            Ok(Arena { memory, base: p as *mut u8, private, total })
        }
    }
}

impl Drop for SharedPool {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor
        unsafe { libc::close(self.fd) };
    }
}

pub struct Arena {
    pub memory: Memory,
    /// Native address of wasm address 0.
    pub base: *mut u8,
    /// Wasm address of the pool.
    pub private: usize,
    pub total: usize,
}

impl Arena {
    /// Release the mapping. The instance must not run again.
    ///
    /// # Safety
    /// Nothing may use the memory afterwards.
    pub unsafe fn unmap(self) {
        libc::munmap(self.base as *mut libc::c_void, self.total);
    }
}

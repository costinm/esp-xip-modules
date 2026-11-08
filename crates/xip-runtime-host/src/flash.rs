//! Optional persistent NOR device, accessed through the same flash ABI as ESP.
use crate::runtime::{QItem, Shared};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use xip_abi::*;

pub struct FlashFile {
    file: File,
    size: u32,
    /// Completed mutations before the next simulated loss of power. None disables it.
    cut_after: Option<u64>,
    pub operations: u64,
    pub reads: u64,
    pub sector_erases: Vec<u64>,
}
impl FlashFile {
    /// Create an erased device, or reopen an existing device without changing its bytes.
    pub fn open(path: impl AsRef<Path>, size: u32) -> io::Result<Self> {
        if size == 0 || size % 4096 != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "flash size"));
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.as_ref())
        {
            Ok(mut f) => {
                let blank = [0xff; 4096];
                for _ in 0..size / 4096 {
                    f.write_all(&blank)?;
                }
                f.sync_data()?;
                f
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                OpenOptions::new().read(true).write(true).open(path)?
            }
            Err(e) => return Err(e),
        };
        if file.metadata()?.len() != u64::from(size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "existing flash size mismatch",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            size,
            cut_after: None,
            operations: 0,
            reads: 0,
            sector_erases: vec![0; (size / 4096) as usize],
        })
    }
    pub fn size(&self) -> u32 {
        self.size
    }
    pub fn cut_after(&mut self, operations: Option<u64>) {
        self.cut_after = operations;
    }
    fn range(&self, at: u32, n: usize) -> bool {
        n > 0 && at <= self.size && n <= (self.size - at) as usize
    }
    pub fn read(&mut self, at: u32, out: &mut [u8]) -> Result<(), i32> {
        self.reads+=1;
        if !self.range(at, out.len()) {
            return Err(ERR_ARG);
        }
        self.file
            .seek(SeekFrom::Start(at as u64))
            .and_then(|_| self.file.read_exact(out))
            .map_err(|_| ERR_IO)
    }
    fn before(&self) -> Result<(), i32> {
        if self.cut_after == Some(0) {
            Err(ERR_IO)
        } else {
            Ok(())
        }
    }
    fn complete(&mut self) -> Result<(), i32> {
        self.file.sync_data().map_err(|_| ERR_IO)?;
        self.operations += 1;
        if let Some(n) = &mut self.cut_after {
            *n -= 1;
        }
        Ok(())
    }
    pub fn program(&mut self, at: u32, data: &[u8]) -> Result<(), i32> {
        if at % 4 != 0 || data.len() % 4 != 0 || !self.range(at, data.len()) {
            return Err(ERR_ARG);
        }
        self.before()?;
        let mut old = [0u8; 4096];
        for (i, chunk) in data.chunks(old.len()).enumerate() {
            self.read(at + (i * 4096) as u32, &mut old[..chunk.len()])?;
            if old[..chunk.len()]
                .iter()
                .zip(chunk)
                .any(|(o, n)| o & n != *n)
            {
                return Err(ERR_ARG);
            }
        }
        self.file
            .seek(SeekFrom::Start(at as u64))
            .and_then(|_| self.file.write_all(data))
            .map_err(|_| ERR_IO)?;
        self.complete()
    }
    pub fn erase(&mut self, at: u32, len: u32) -> Result<(), i32> {
        if at % 4096 != 0 || len % 4096 != 0 || !self.range(at, len as usize) {
            return Err(ERR_ARG);
        }
        // Sector boundaries are independently interruptible, including a multi-sector job.
        for off in (at..at + len).step_by(4096) {
            self.before()?;
            self.file
                .seek(SeekFrom::Start(off as u64))
                .and_then(|_| self.file.write_all(&[0xff; 4096]))
                .map_err(|_| ERR_IO)?;
            self.sector_erases[(off / 4096) as usize] += 1;
            self.complete()?;
        }
        Ok(())
    }
}

pub(crate) struct Job {
    owner: usize,
    address: u32,
    len: u32,
    packet: u32,
    start: u32,
    component: i32,
    method: i32,
    id: u64,
}
fn address(base: u32, off: u32) -> Result<u32, i32> {
    base.checked_add(off).ok_or(ERR_ARG)
}
unsafe extern "C" fn read(u: *mut core::ffi::c_void, b: u32, o: u32, p: *mut u8, n: usize) -> i32 {
    let (sh, _) = crate::hostcalls::ctx(u);
    if p.is_null() || n > 4096 {
        return ERR_ARG;
    }
    let Some(at) = address(b, o).ok() else {
        return ERR_ARG;
    };
    let mut f = sh.flash.borrow_mut();
    let Some(f) = f.as_mut() else {
        return ERR_UNSUPPORTED;
    };
    match f.read(at, core::slice::from_raw_parts_mut(p, n)) {
        Ok(()) => n as i32,
        Err(e) => e,
    }
}
unsafe extern "C" fn erase(
    u: *mut core::ffi::c_void,
    b: u32,
    o: u32,
    n: u32,
    c: i32,
    m: i32,
    id: u64,
) -> i32 {
    submit(u, b, o, n, 0, 0, c, m, id)
}
unsafe extern "C" fn write(
    u: *mut core::ffi::c_void,
    b: u32,
    o: u32,
    p: u32,
    s: u32,
    n: u32,
    c: i32,
    m: i32,
    id: u64,
) -> i32 {
    if p == 0 {
        return ERR_ARG;
    }
    submit(u, b, o, n, p, s, c, m, id)
}
unsafe fn submit(
    u: *mut core::ffi::c_void,
    b: u32,
    o: u32,
    n: u32,
    p: u32,
    s: u32,
    c: i32,
    m: i32,
    id: u64,
) -> i32 {
    let (sh, me) = crate::hostcalls::ctx(u);
    if me.st.borrow().ended {
        return ERR_ARG;
    }
    let Some(at) = address(b, o).ok() else {
        return ERR_ARG;
    };
    let f = sh.flash.borrow();
    let Some(f) = f.as_ref() else {
        return ERR_UNSUPPORTED;
    };
    if !f.range(at, n as usize) {
        return ERR_ARG;
    }
    if p == 0 {
        if at % 4096 != 0 || n % 4096 != 0 {
            return ERR_ARG;
        }
    } else {
        let Some(e) = sh.pool.get(p) else {
            return ERR_ARG;
        };
        if sh.pool.refs(e.slot as i32) != 1
            || at % 4 != 0
            || n % 4 != 0
            || s > sh.pool.slot_size() as u32
            || n > sh.pool.slot_size() as u32 - s
        {
            return ERR_ARG;
        }
    }
    let mut job = sh.flash_job.borrow_mut();
    if job.is_some() {
        return ERR_BUSY;
    }
    *job = Some(Job {
        owner: me.id,
        address: at,
        len: n,
        packet: p,
        start: s,
        component: c,
        method: m,
        id,
    });
    sh.notify.notify_one();
    0
}
pub(crate) fn fill(t: &mut Host) {
    t.flash_read = Some(read);
    t.flash_write = Some(write);
    t.flash_erase = Some(erase);
}
pub(crate) fn cancel(sh: &Shared, owner: usize) {
    let mut j = sh.flash_job.borrow_mut();
    if j.as_ref().is_some_and(|j| j.owner == owner) {
        if let Some(j) = j.take() {
            if let Some(p) = sh.pool.get(j.packet) {
                sh.pool.release(p.slot as i32);
            }
        }
    }
}
pub(crate) fn pump(sh: &Shared) -> bool {
    let Some(j) = sh.flash_job.borrow_mut().take() else {
        return false;
    };
    let me = sh.insts.borrow().get(j.owner).cloned();
    let mut f = sh.flash.borrow_mut();
    let rc = if let Some(f) = f.as_mut() {
        if j.packet == 0 {
            f.erase(j.address, j.len)
        } else {
            let e = sh.pool.get(j.packet).expect("flash owns packet");
            let data = unsafe {
                core::slice::from_raw_parts(
                    sh.pool.slot(e.slot as i32).add(j.start as usize),
                    j.len as usize,
                )
            };
            let rc = f.program(j.address, data);
            sh.pool.release(e.slot as i32);
            rc
        }
    } else {
        Err(ERR_UNSUPPORTED)
    };
    drop(f);
    if let Some(me) = me {
        if !me.st.borrow().ended {
            me.st.borrow_mut().queue.push_back(QItem {
                request: 4,
                component: j.component,
                method: j.method,
                id: j.id,
                value: rc.err().unwrap_or(0),
                ..Default::default()
            });
        }
    }
    true
}

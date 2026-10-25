//! Components hosted by a module (manifest key 11), the same rules as `xip_runtime.c` ("components hosted by a module"): right after `HOST_START`
//! the runtime asks the module for the table of each name (`HOST_TABLE`, answered with `Hw::provide_table`); `get_component(name)` then hands
//! a caller a copy of the table whose functions are thunks that call the module's function with the module's context, and return `ERR_NOTFOUND` when
//! no running module hosts the name. The host-runtime is single threaded, so there is no lock to take.
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::{Rc, Weak};
use xip_abi::*;

use crate::runtime::{Inst, Shared};

/// A component a module hosts: `table` and `ctx` are 0 until the module answered `HOST_TABLE`.
pub(crate) struct Hosted {
    pub name: String,
    pub table: usize,
    pub ctx: usize,
}

const THUNKS: usize = 24;
type ThunkFn = unsafe extern "C" fn(*mut c_void, usize, usize, usize, usize, usize) -> i32;

struct Slot {
    sh: Weak<Shared>,
    name: String,
    entry: usize,
}

thread_local! {
    static SLOTS: RefCell<Vec<Option<Slot>>> = RefCell::new((0..THUNKS).map(|_| None).collect());
}

fn word(bytes: *const u8, at: usize) -> usize {
    let mut w = [0u8; std::mem::size_of::<usize>()];
    unsafe { std::ptr::copy_nonoverlapping(bytes.add(at), w.as_mut_ptr(), w.len()) };
    usize::from_ne_bytes(w)
}

/// The running module that hosts `name`, with its table and context.
fn provider(sh: &Shared, name: &str) -> Option<(usize, usize)> {
    sh.insts.borrow().iter().find_map(|i| {
        let st = i.st.borrow();
        if st.ended {
            return None;
        }
        st.hosted.iter().find(|h| h.name == name && h.table != 0).map(|h| (h.table, h.ctx))
    })
}

fn size_of_table(table: usize) -> usize {
    let mut s = [0u8; 4];
    unsafe { std::ptr::copy_nonoverlapping((table + 4) as *const u8, s.as_mut_ptr(), 4) };
    u32::from_ne_bytes(s) as usize
}

fn run(slot: usize, a: [usize; 5]) -> i32 {
    let found = SLOTS.with(|s| s.borrow()[slot].as_ref().map(|s| (s.sh.clone(), s.name.clone(), s.entry)));
    let Some((sh, name, entry)) = found else { return ERR_NOTFOUND };
    let Some(sh) = sh.upgrade() else { return ERR_NOTFOUND };
    let Some((table, ctx)) = provider(&sh, &name) else { return ERR_NOTFOUND };
    let off = 8 + entry * std::mem::size_of::<usize>();
    if off + std::mem::size_of::<usize>() > size_of_table(table) {
        return ERR_UNSUPPORTED;
    }
    let f = word(table as *const u8, off);
    if f == 0 {
        return ERR_UNSUPPORTED;
    }
    let f: ThunkFn = unsafe { std::mem::transmute(f) };
    unsafe { f(ctx as *mut c_void, a[0], a[1], a[2], a[3], a[4]) }
}

unsafe extern "C" fn thunk<const N: usize>(_ctx: *mut c_void, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    run(N, [a1, a2, a3, a4, a5])
}

macro_rules! thunks {
    ($($n:literal)*) => { [$(thunk::<$n> as ThunkFn),*] };
}
fn thunk_fns() -> [ThunkFn; THUNKS] {
    thunks!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23)
}

/// After `HOST_START`: ask the module for the table of each component it hosts. The answer is the request's response, two native words.
pub(crate) fn publish(sh: &Shared, me: &Rc<Inst>) {
    let n = me.st.borrow().hosted.len();
    for i in 0..n {
        let name = me.st.borrow().hosted[i].name.clone();
        let id = {
            let mut st = sh.st.borrow_mut();
            st.next_request += 1;
            st.next_request
        };
        let call = Call {
            size: std::mem::size_of::<Call>() as u32,
            component: HOST_COMPONENT as i32,
            method: HOST_TABLE as i32,
            value: 0,
            flags: 1,
            data: name.as_ptr(),
            len: name.len(),
            pkt: 0,
            from: ADDR_MAIN,
            time_us: 0,
            id,
            request_id: 0,
            request_has_id: 0,
            request_reserved: 0,
        };
        me.st.borrow_mut().calls += 1;
        unsafe { (me.handler)(&*me.table, me.state_ptr, &call) };
        let answer = {
            let mut st = sh.st.borrow_mut();
            st.main.iter().position(|m| m.id == id).map(|at| st.main.remove(at).data)
        };
        match answer {
            Some(a) if a.len() == 2 * std::mem::size_of::<usize>() => {
                let w = std::mem::size_of::<usize>();
                let table = usize::from_ne_bytes(a[..w].try_into().unwrap());
                let ctx = usize::from_ne_bytes(a[w..].try_into().unwrap());
                if table != 0 && size_of_table(table) >= 8 {
                    let mut st = me.st.borrow_mut();
                    st.hosted[i].table = table;
                    st.hosted[i].ctx = ctx;
                }
            }
            _ => {}
        }
    }
}

/// The table of `name` for a caller: the runtime's copy of the table of the running module that hosts it, or null.
pub(crate) fn table_of(sh: &Rc<Shared>, name: &str) -> *const c_void {
    let mut found = provider(sh, name);
    if found.is_none() {
        let starter = sh.starter.borrow().clone();
        if let Some(inst) = starter.and_then(|f| f(name)) {
            // installed but not running: run its start here, so that the table exists before the caller gets an answer
            crate::runtime::drain(sh, &inst.me);
            found = provider(sh, name);
        }
    }
    let Some((table, _)) = found else { return std::ptr::null() };
    if let Some(t) = sh.thunk_tables.borrow().iter().find(|(n, _)| n == name) {
        return t.1.as_ptr().cast();
    }
    let size = size_of_table(table);
    let w = std::mem::size_of::<usize>();
    if size < 8 || size > 8 + THUNKS * w {
        return std::ptr::null();
    }
    let n = (size - 8) / w;
    let mut copy = vec![0usize; size.div_ceil(w)].into_boxed_slice();
    let bytes = copy.as_mut_ptr().cast::<u8>();
    unsafe { std::ptr::copy_nonoverlapping(table as *const u8, bytes, 8) };
    let fns = thunk_fns();
    let ok = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let wanted: Vec<usize> = (0..n).filter(|e| word(table as *const u8, 8 + e * w) != 0).collect();
        let free: Vec<usize> = (0..THUNKS).filter(|i| slots[*i].as_ref().is_none_or(|s| s.sh.upgrade().is_none())).collect();
        if free.len() < wanted.len() {
            return false;
        }
        for (e, i) in wanted.iter().zip(free) {
            slots[i] = Some(Slot { sh: Rc::downgrade(sh), name: name.to_string(), entry: *e });
            let f = fns[i] as usize;
            unsafe { std::ptr::copy_nonoverlapping(f.to_ne_bytes().as_ptr(), bytes.add(8 + e * w), w) };
        }
        true
    });
    if !ok {
        return std::ptr::null();
    }
    let p = copy.as_ptr().cast();
    sh.thunk_tables.borrow_mut().push((name.to_string(), copy));
    p
}

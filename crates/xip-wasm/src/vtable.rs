//! `xip.component` and `xip.vcall`: the component tables (`wifi`, `link`, `power`, ...) for wasm modules. The module has a
//! static table of shims (`xip-abi/src/vt.rs`); a shim lands here with the component handle, the method (its index in the table) and
//! up to six scalar arguments. Pointer arguments are addresses in the shared memory of pointer-free structs or byte buffers, which are
//! checked against the memory and passed to the native table of the runtime. A component has no number of its own: `component` gives the
//! module a handle for a name (private to this runtime), and a method is its index in the table.
use super::{active, Ctx};
use std::ffi::{c_char, c_void};
use wasmi::{Caller, Linker};
use xip_abi::schema::{self, Arg};
use xip_abi::*;

type R<T> = Result<T, i32>;

/// A pointer to one `T` at `addr`, or an array of `n`.
fn p<T>(c: &Ctx, addr: u64) -> R<*mut T> {
    pn::<T>(c, addr, 1)
}
fn pn<T>(c: &Ctx, addr: u64, n: usize) -> R<*mut T> {
    let len = core::mem::size_of::<T>().checked_mul(n).ok_or(ERR_ARG)?;
    let ptr = c.ptr(u32::try_from(addr).map_err(|_| ERR_ARG)?, len).ok_or(ERR_ARG)?;
    if ptr as usize % core::mem::align_of::<T>() != 0 { return Err(ERR_ARG); }
    Ok(ptr.cast())
}
/// A NUL-terminated name that ends inside the memory.
fn cstr(c: &Ctx, addr: u64) -> R<*const c_char> {
    let a = u32::try_from(addr).map_err(|_| ERR_ARG)?;
    for n in 1..=256usize {
        let s = c.slice(a, n).ok_or(ERR_ARG)?;
        if s[n - 1] == 0 { return Ok(s.as_ptr().cast()); }
    }
    Err(ERR_ARG)
}

fn table(c: &Ctx, name: &str) -> *const c_void {
    let name = std::ffi::CString::new(name).unwrap();
    c.host().get_component(&name)
}

/// One argument of a table function as the native word it is called with.
fn word(c: &Ctx, arg: Arg, a: &[u64; 6], i: usize) -> R<usize> {
    Ok(match arg {
        Arg::Val => a[i] as usize,
        Arg::In(n) | Arg::Out(n) => {
            let p = pn::<u8>(c, a[i], n)? as usize;
            if p % 4 != 0 { return Err(ERR_ARG); }
            p
        }
        Arg::Str => cstr(c, a[i])? as usize,
        Arg::Bytes(len) | Arg::OutBytes(len) => pn::<u8>(c, a[i], a[len as usize] as usize)? as usize,
        Arg::OutArr(size, count) => pn::<u8>(c, a[i], size.checked_mul(a[count as usize] as usize).ok_or(ERR_ARG)?)? as usize,
    })
}

/// Call method `m` of component `comp` on the native table: the function is the `m`-th pointer after the `{version, size}` header, called with
/// the context and one word per argument (every argument of every table function is a word on the platforms the host runs on).
fn dispatch(c: &Ctx, handle: i32, m: i32, a: [u64; 6]) -> R<i32> {
    let name = usize::try_from(handle).ok().and_then(|h| h.checked_sub(1)).and_then(|h| c.components.borrow().get(h).copied()).ok_or(ERR_UNSUPPORTED)?;
    let blob = schema::component(name).ok_or(ERR_UNSUPPORTED)?;
    let m = usize::try_from(m).map_err(|_| ERR_ARG)?;
    let method = schema::method(blob, m).ok_or(ERR_UNSUPPORTED)?;
    if method.nargs() > 6 { return Err(ERR_UNSUPPORTED); }
    let t = table(c, name) as *const u8;
    if t.is_null() { return Err(ERR_UNSUPPORTED); }
    // SAFETY: a table starts with `{u32 version, u32 size}` and has `size` bytes
    let (size, at) = unsafe { (std::ptr::read(t.add(4).cast::<u32>()) as usize, 8 + m * std::mem::size_of::<usize>()) };
    if at + std::mem::size_of::<usize>() > size { return Err(ERR_UNSUPPORTED); }
    let fp = unsafe { std::ptr::read(t.add(at).cast::<usize>()) };
    if fp == 0 { return Err(ERR_UNSUPPORTED); }
    let mut w = [0usize; 7];
    w[0] = c.host().ctx() as usize;
    for i in 0..method.nargs() {
        w[i + 1] = word(c, method.arg(i).ok_or(ERR_ARG)?, &a, i)?;
    }
    type F = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize) -> i32;
    // SAFETY: the pointer is a table function of at most six arguments after the context
    Ok(unsafe { std::mem::transmute::<usize, F>(fp)(w[0], w[1], w[2], w[3], w[4], w[5], w[6]) })
}

/// The handle of the component the module named (0 when this runtime does not provide it): the first time it is asked for it takes the next number.
fn handle(c: &Ctx, name: u32) -> i32 {
    let Ok(name) = cstr(c, name as u64).and_then(|p| unsafe { std::ffi::CStr::from_ptr(p) }.to_str().map_err(|_| ERR_ARG)) else { return 0 };
    let Some((known, _)) = schema::COMPONENTS.iter().find(|k| k.0 == name) else { return 0 };
    if table(c, known).is_null() { return 0 }
    let mut names = c.components.borrow_mut();
    let at = names.iter().position(|n| n == known).unwrap_or_else(|| {
        names.push(known);
        names.len() - 1
    });
    at as i32 + 1
}

pub(crate) fn register(linker: &mut Linker<Ctx>) -> Result<(), String> {
    linker
        .func_wrap("xip", "component", |mut caller: Caller<'_, Ctx>, name: u32| -> i32 { active(&mut caller, |c| handle(c, name)) })
        .map_err(|e| e.to_string())?;
    linker
        .func_wrap(
            "xip",
            "vcall",
            |mut caller: Caller<'_, Ctx>, comp: i32, m: i32, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64| -> i32 {
                active(&mut caller, |c| dispatch(c, comp, m, [a0, a1, a2, a3, a4, a5]).unwrap_or_else(|e| e))
            },
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

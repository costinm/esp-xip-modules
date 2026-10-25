#!/usr/bin/env python3
"""Generate the vtable of a component a module hosts, and the glue between its CBOR API and that vtable, from the module's API.md.

    gen_vtables.py --api modules/mod_ble/API.md --types crates/xip-abi/src/ble.rs \
                   --abi-out crates/xip-abi/src/ble_vtable.rs --module-out modules/mod_ble/src/api_gen.rs

Input
  * the API.md of the component: `# `name` API (number)`, a section per method (`## 80. `start` ...` with `**Request:** [Type](#type)` and
    `**Response:** [Type](#type)`), and the `# Types` with their `**C type:**` and field tables (`| tag | `field` | `type` | text |`);
  * the Rust source that declares those C types (`#[repr(C)] pub struct BleStart { ... }`): the generator reads the field types from it, so
    the layout stays the one place a person writes. A field of an API type is the struct field of the same name; an API `bool` is a `u8`; a
    `bytes` field is a `[u8; N]` (with a `<name>_len` sibling it is variable up to N, without it exactly N bytes).

  * optionally `API.local.md` beside it: methods that only `get_component` callers use ("local-only", with arbitrary parameters up to five words, like
    the psa vtable). Every method of API.md is remote-callable (it has a number and a CBOR form, and is in the catalog); the local ones are in the
    table after them, have no number and are not in `dispatch`, so no envelope can reach them.

Output
  * `--abi-out`: the table `Ble` (`#[repr(C)] { version, size, one Option<fn> per method }`, each function `int f(void *ctx [, const Req *] [, Resp *])`)
    that cbindgen puts in include/xip.h. It is the same table whether the runtime or a module implements the component.
  * `--module-out`: for the module: the trait `Api` (one method per API method), `dispatch` (the CBOR fields to the request struct, the call, the
    response struct to CBOR: the module has no `match` of its own and never sees an envelope), and `table`, the module's vtable whose functions
    enter the module's allocator context (`Ctx`: host and state) and call the trait. Answer `HOST_TABLE` with `provide`.
"""
import argparse
import os
import re
import sys


def camel(name):
    return "".join(p.capitalize() for p in re.split(r"[_\-]", name))


def parse_api(path):
    text = open(path).read()
    m = re.search(r"^# `([\w\-]+)` API \((\d+)\)", text, re.M)
    if not m:
        sys.exit(f"{path}: no title `# `name` API (number)`")
    name, number = m.group(1), int(m.group(2))
    body, _, types_text = text.partition("\n# Types")
    methods = []
    for sec in re.split(r"^## (?=\d+\. `)", body, flags=re.M)[1:]:
        h = re.match(r"(\d+)\. `(\w+)` — ([^\n]*)\n(.*)", sec, re.S)
        if not h:
            sys.exit(f"{path}: bad method heading: {sec[:40]!r}")
        number_m, mname, title, rest = int(h.group(1)), h.group(2), h.group(3).strip(), h.group(4)
        req = re.search(r"\*\*Request:\*\* \[(\w+)\]", rest)
        res = re.search(r"\*\*Response:\*\* \[(\w+)\]", rest)
        para = [p for p in re.split(r"\n\s*\n", rest.strip()) if p.strip() and not p.lstrip().startswith(("**", "###", "|"))]
        doc = " ".join(para[0].split()) if para else title
        doc = re.split(r"(?<=[.])\s", doc, maxsplit=1)[0]          # the first sentence; API.md has the rest
        methods.append(dict(number=number_m, name=mname, title=title, doc=doc, request=req and req.group(1), response=res and res.group(1)))
    types = {}
    for sec in re.split(r"^## `", types_text, flags=re.M)[1:]:
        tname = sec.split("`", 1)[0]
        c = re.search(r"\*\*C type:\*\* (\w+)", sec)
        if not c:
            sys.exit(f"{path}: type {tname} has no **C type:**")
        fields = []
        for row in re.finditer(r"^\|\s*(\d+)\s*\|\s*`(\w+)`\s*\|\s*`([^`]+)`\s*\|", sec, re.M):
            fields.append(dict(tag=int(row.group(1)), name=row.group(2), type=row.group(3)))
        types[tname] = dict(ctype=c.group(1), fields=fields)
    return name, number, methods, types


def parse_local(path):
    """Local-only methods of the vtable, from `API.local.md` beside API.md: `## `name` — title`, then `**C function:** `(a: u32, b: *const u8)``
    (the Rust parameters after `ctx`; the function returns an int). They are in the table for `get_component` callers and nowhere else: no number, no
    CBOR form, no catalog entry, no envelope can reach them. They follow the remote methods in the table."""
    if not os.path.exists(path):
        return []
    out = []
    for sec in re.split(r"^## (?=`)", open(path).read(), flags=re.M)[1:]:
        h = re.match(r"`(\w+)`(?: — ([^\n]*))?\n(.*)", sec, re.S)
        sig = re.search(r"\*\*C function:\*\* `\((.*?)\)`", h.group(3))
        if not sig:
            sys.exit(f"{path}: local method {h.group(1)} has no **C function:**")
        params = [p.strip() for p in sig.group(1).split(",") if p.strip()]
        if len(params) > 5:
            sys.exit(f"{path}: {h.group(1)} takes more than five words of arguments")
        para = [p for p in re.split(r"\n\s*\n", h.group(3).strip()) if p.strip() and not p.lstrip().startswith("**")]
        out.append(dict(number=None, name=h.group(1), title=h.group(2) or "", doc=" ".join(para[0].split()) if para else (h.group(2) or h.group(1)),
                        request=None, response=None, params=params))
    return out


def parse_structs(path):
    text = open(path).read()
    out = {}
    for m in re.finditer(r"#\[repr\(C\)\][^{;]*?pub struct (\w+) \{(.*?)\n\}", text, re.S):
        fields = {}
        for f in re.finditer(r"^\s*pub (\w+): ([^,\n]+),", m.group(2), re.M):
            fields[f.group(1)] = f.group(2).strip()
        out[m.group(1)] = fields
    return out


INTS = {"u8": 8, "u16": 16, "u32": 32, "u64": 64, "i8": 8, "i16": 16, "i32": 32, "i64": 64}


def field_codec(tname, f, struct_fields):
    """(kind, rust type, length) of API field `f` of type `tname` given the C struct's fields."""
    rt = struct_fields.get(f["name"])
    if rt is None:
        sys.exit(f"C type of {tname} has no field {f['name']}")
    api = f["type"]
    arr = re.fullmatch(r"\[u8; (\d+)\]", rt)
    if api == "bytes":
        if not arr:
            sys.exit(f"{tname}.{f['name']}: bytes needs a [u8; N] field, found {rt}")
        return ("bytes", rt, int(arr.group(1)))
    if api == "bool":
        if rt != "u8":
            sys.exit(f"{tname}.{f['name']}: bool needs a u8 field, found {rt}")
        return ("bool", rt, 0)
    if api in INTS and rt == api:
        return ("sint" if api.startswith("i") else "uint", rt, 0)
    sys.exit(f"{tname}.{f['name']}: API type {api} does not fit the field type {rt}")


def abi_text(name, number, methods, types, structs, api_path, types_mod):
    cname = camel(name)
    out = [
        f"//! The `{name}` component's table ({number}), generated by `tools/gen_vtables.py` from `{api_path}`; do not edit.",
        "//!",
        "//! The same table whether the runtime or a module implements the component: `get_component(\"%s\")` returns it, a module that hosts it" % name,
        "//! has the runtime hand callers a copy whose functions are thunks (`HOST_TABLE`). Every function takes `ctx` first and returns `OK` or",
        "//! a negative `ERR_*`; a request is read, a response is filled in (set its `size` first). The order of the members is part of the ABI.",
        "use core::ffi::{c_int, c_void};",
        "",
    ]
    out.append(f"use crate::{types_mod}::{{" + ", ".join(sorted({types[t]['ctype'] for m in methods for t in (m['request'], m['response']) if t})) + "};")
    out += ["", f"/// The `{name}` component table.", "#[repr(C)]", f"pub struct {cname} {{",
            "    /// Table version.", "    pub version: u32,", "    /// `sizeof` this table: members past it do not exist.", "    pub size: u32"]
    out[-1] += ","
    for m in methods:
        args = ["ctx: *mut c_void"]
        if m["request"]:
            args.append(f"request: *const {types[m['request']]['ctype']}")
        if m["response"]:
            args.append(f"response: *mut {types[m['response']]['ctype']}")
        args += m.get("params", [])
        out.append(f"    /// {m['number']}. {m['doc']}" if m["number"] else f"    /// Local only (no CBOR form, no envelope reaches it): {m['doc']}")
        out.append(f"    pub {m['name']}: Option<unsafe extern \"C\" fn({', '.join(args)}) -> c_int>,")
    out.append("}")
    return "\n".join(out) + "\n"


def module_text(name, number, methods, types, structs, api_path):
    cname = camel(name)
    t = []
    w = t.append
    ctypes = sorted({types[x]["ctype"] for m in methods for x in (m["request"], m["response"]) if x})
    w(f"//! The glue of the `{name}` component ({number}) for the module that hosts it, generated by `tools/gen_vtables.py` from `{api_path}`; do not edit.")
    w("//!")
    w("//! [`Api`] is what the module implements. [`dispatch`] serves a request (a method number and the fields map: the runtime has parsed the")
    w("//! envelope) and [`table`] is the module's vtable for other modules, whose functions enter the allocator context of the module (`Ctx`).")
    w("#![allow(clippy::all, dead_code)]")
    w("use core::ffi::{c_int, c_void};")
    w("use xip_abi::cbor::Writer;")
    w("use xip_abi::fields as f;")
    w(f"use xip_abi::{{{cname}, CallView, Hw, Host, ERR_ARG, ERR_SIZE, ERR_UNSUPPORTED, {', '.join(ctypes)}}};")
    w("")
    w(f"pub const COMPONENT: i32 = {number};")
    for m in methods:
        if m["number"]:
            w(f"pub const M_{m['name'].upper()}: i32 = {m['number']};")
    w("")
    w("/// What the module implements. Every method returns `Err(code)` with a negative `ERR_*`.")
    w("pub trait Api {")
    for m in methods:
        args = ["&mut self", "hw: &Hw"]
        if m["request"]:
            args.append(f"request: &{types[m['request']]['ctype']}")
        if m["response"]:
            args.append(f"response: &mut {types[m['response']]['ctype']}")
        args += m.get("params", [])
        if m["number"]:
            w(f"    /// {m['number']}. {m['doc']}")
        else:
            w(f"    /// Local only (reached by `get_component` callers, never by an envelope; the arguments are as given): {m['doc']}")
        w(f"    fn {m['name']}({', '.join(args)}) -> Result<(), i32>;")
    w("}")
    w("")
    # decoders for request types, encoders for response types
    dec, enc = [], []
    for m in methods:
        if m["request"] and m["request"] not in dec:
            dec.append(m["request"])
        if m["response"] and m["response"] not in enc:
            enc.append(m["response"])
    for tn in dec:
        ty = types[tn]
        sf = structs.get(ty["ctype"]) or sys.exit(f"no struct {ty['ctype']} in the types file")
        w(f"fn decode_{tn.lower()}(fields: &[u8], r: &mut {ty['ctype']}) -> Result<(), i32> {{")
        for fl in ty["fields"]:
            kind, rt, n = field_codec(tn, fl, sf)
            tag, fn = fl["tag"], fl["name"]
            if kind == "bool":
                w(f"    if let Some(v) = f::get_bool(fields, {tag})? {{ r.{fn} = v as u8; }}")
            elif kind == "uint":
                w(f"    if let Some(v) = f::get_uint(fields, {tag})? {{ r.{fn} = {rt}::try_from(v).map_err(|_| ERR_ARG)?; }}")
            elif kind == "sint":
                w(f"    if let Some(v) = f::get_int(fields, {tag})? {{ r.{fn} = {rt}::try_from(v).map_err(|_| ERR_ARG)?; }}")
            else:
                lenf = sf.get(fn + "_len")
                if lenf:
                    w(f"    if let Some(v) = f::get_bytes(fields, {tag}, &mut r.{fn})? {{ r.{fn}_len = {lenf}::try_from(v).map_err(|_| ERR_ARG)?; }}")
                else:
                    w(f"    if let Some(v) = f::get_bytes(fields, {tag}, &mut r.{fn})? {{ if v != {n} {{ return Err(ERR_ARG); }} }}")
        w("    Ok(())")
        w("}")
        w("")
    for tn in enc:
        ty = types[tn]
        sf = structs.get(ty["ctype"]) or sys.exit(f"no struct {ty['ctype']} in the types file")
        w(f"fn encode_{tn.lower()}(r: &{ty['ctype']}, out: &mut [u8]) -> Result<usize, i32> {{")
        w("    let mut w = Writer::new(out);")
        w(f"    w.map({len(ty['fields'])});")
        for fl in ty["fields"]:
            kind, rt, n = field_codec(tn, fl, sf)
            tag, fn = fl["tag"], fl["name"]
            if kind == "bool":
                w(f"    w.uint({tag}).boolean(r.{fn} != 0);")
            elif kind == "uint":
                w(f"    w.uint({tag}).uint(r.{fn} as u64);")
            elif kind == "sint":
                w(f"    w.uint({tag}).int(r.{fn} as i64);")
            else:
                lenf = sf.get(fn + "_len")
                if lenf:
                    w(f"    w.uint({tag}).bytes(&r.{fn}[..(r.{fn}_len as usize).min({n})]);")
                else:
                    w(f"    w.uint({tag}).bytes(&r.{fn});")
        w("    w.finish().map(|b| b.len()).ok_or(ERR_SIZE)")
        w("}")
        w("")
    w("/// Serve method `method` with the request `fields`; the response map goes to `out`. Returns its length. An unknown method is `ERR_UNSUPPORTED`.")
    w("pub fn dispatch<T: Api>(t: &mut T, hw: &Hw, method: i32, fields: &[u8], out: &mut [u8]) -> Result<usize, i32> {")
    w("    match method {")
    for m in methods:
        if not m["number"]:
            continue
        w(f"        M_{m['name'].upper()} => {{")
        call = ["hw"]
        if m["request"]:
            ty = types[m["request"]]["ctype"]
            w(f"            let mut request = {ty}::default();")
            w(f"            decode_{m['request'].lower()}(fields, &mut request)?;")
            call.append("&request")
        if m["response"]:
            ty = types[m["response"]]["ctype"]
            w(f"            let mut response = {ty}::default();")
            call.append("&mut response")
        w(f"            t.{m['name']}({', '.join(call)})?;")
        if m["response"]:
            w(f"            encode_{m['response'].lower()}(&response, out)")
        else:
            w("            let mut w = Writer::new(out);")
            w("            w.map(0);")
            w("            w.finish().map(|b| b.len()).ok_or(ERR_SIZE)")
        w("        }")
    w("        _ => Err(ERR_UNSUPPORTED),")
    w("    }")
    w("}")
    w("")
    w("/// The first argument of every function of the module's table: the host table and the module's state, both valid while the module runs.")
    w("#[repr(C)]")
    w("pub struct Ctx {")
    w("    pub host: *const Host,")
    w("    pub state: *mut u8,")
    w("}")
    w("")
    w("/// Run `body` in the allocator context of the module (the runtime's thunk has taken the module's lock).")
    w("unsafe fn within<T: Api>(ctx: *mut c_void, body: impl FnOnce(&mut T, &Hw) -> Result<(), i32>) -> c_int {")
    w("    let ctx = &*(ctx as *const Ctx);")
    w("    let Some(hw) = Hw::new(ctx.host) else { return ERR_ARG };")
    w("    let saved = xip_abi::heap::enter(ctx.host);")
    w("    let rc = match body(&mut *(ctx.state as *mut T), &hw) { Ok(()) => 0, Err(rc) => rc };")
    w("    xip_abi::heap::leave(saved);")
    w("    rc")
    w("}")
    w("")
    for m in methods:
        args = ["ctx: *mut c_void"]
        use = ["t", "hw"]
        if m["request"]:
            args.append(f"request: *const {types[m['request']]['ctype']}")
            use.append("&*request")
        if m["response"]:
            args.append(f"response: *mut {types[m['response']]['ctype']}")
            use.append("&mut *response")
        guard = [a.split(":")[0] for a in args[1:]]
        for prm in m.get("params", []):
            args.append(prm)
            use.append(prm.split(":")[0].strip())
        w(f"unsafe extern \"C\" fn v_{m['name']}<T: Api>({', '.join(args)}) -> c_int {{")
        if guard:
            w(f"    if {' || '.join(g + '.is_null()' for g in guard)} {{ return ERR_ARG; }}")
        w(f"    within::<T>(ctx, |t, hw| t.{m['name']}({', '.join(use[1:])}))")
        w("}")
    w("")
    w(f"/// The module's table of the `{name}` component and the context its functions take: a part of the module's state block. The table is built")
    w("/// when the module answers `HOST_TABLE`, not as a constant: the function addresses of a position-independent image (ESP32-C6) exist only at run time.")
    w("#[repr(C)]")
    w("pub struct Hosted {")
    w("    pub ctx: Ctx,")
    w(f"    pub table: {cname},")
    w("}")
    w("impl Default for Hosted {")
    w("    /// All zero: no table yet (the state block of a module starts zeroed).")
    w("    fn default() -> Self {")
    w("        unsafe { core::mem::zeroed() }")
    w("    }")
    w("}")
    w("")
    w("/// Answer `HOST_TABLE` for this component with the module's table, whose functions call `T` in the module's allocator context, and the context")
    w("/// they take. `hosted` points into the module's state block (it must stay valid while the module runs) and `state` is that block as `T`.")
    w("pub fn provide<T: Api>(hw: &Hw, call: &CallView, hosted: *mut Hosted, state: *mut T) -> i32 {")
    w("    unsafe {")
    w("        (*hosted).ctx = Ctx { host: hw.as_ptr(), state: state.cast() };")
    w(f"        (*hosted).table = {cname} {{")
    w("            version: 1,")
    w(f"            size: core::mem::size_of::<{cname}>() as u32,")
    for m in methods:
        w(f"            {m['name']}: Some(v_{m['name']}::<T>),")
    w("        };")
    w(f"        hw.provide_table(call, core::ptr::addr_of!((*hosted).table).cast(), core::ptr::addr_of_mut!((*hosted).ctx).cast())")
    w("    }")
    w("}")
    return "\n".join(t) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--api", required=True)
    ap.add_argument("--types", required=True, help="the Rust source that declares the C types of the API")
    ap.add_argument("--abi-out", required=True)
    ap.add_argument("--module-out", required=True)
    a = ap.parse_args()
    name, number, methods, types = parse_api(a.api)
    methods += parse_local(os.path.join(os.path.dirname(a.api), "API.local.md"))
    structs = parse_structs(a.types)
    for m in methods:
        for key in ("request", "response"):
            if m[key] and m[key] not in types:
                sys.exit(f"{m['name']}: unknown {key} type {m[key]}")
    with open(a.abi_out, "w") as f:
        f.write(abi_text(name, number, methods, types, structs, a.api, os.path.splitext(os.path.basename(a.types))[0]))
    with open(a.module_out, "w") as f:
        f.write(module_text(name, number, methods, types, structs, a.api))


if __name__ == "__main__":
    main()

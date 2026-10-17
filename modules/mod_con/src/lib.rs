//! `mod_con`: the text interface of a device, in Rust, with the features of `mod_console` (C). The API is the same (`API.md`: component 20,
//! `parse`, `render`, and a line typed on the serial port). It owns the serial port through the host (`serial_open`): every line typed
//! arrives as a packet and what the handler answers is printed. A line is `status`, `start NAME [TEXT]`, `stop NAME`, `events`, `reset`,
//! `heap ...`, `nv ...`, `wifi ...`, `help`, or `component.method key=value ...`: the last turns into the tagged-CBOR request of the module
//! `component` (names and tags from the catalog, generated from the modules' API.md files), is sent as a packet, and what comes back is
//! printed as it arrives. A method of a host component with a native function (`wifi.set`, `power.sleep`, ...) is called through the
//! component's table instead (`tables.rs`: the layouts from `native.h`).
//!
//! The console waits for nothing: the only memory is the next request id.
#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void, CStr};

use xip_abi::cbor::{self, Item, Reader, Writer};
use xip_abi::{CallView, Hw, Module, ADDR_MAIN, ERR_BUSY, ERR_NOTFOUND, ERR_SIZE, ERR_UNSUPPORTED, FEAT_MODULES, FEAT_SERIAL, FEAT_WIFI, HOST_START, HOST_STOP};

mod tables;
use tables::*;

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

const C_CONSOLE: i32 = 20;
const M_PARSE: i32 = 1;
const M_RENDER: i32 = 2;
/// A line typed on the serial port (a packet).
const M_LINE: i32 = 3;
const MAX_TOKENS: usize = 24;
const OUT_MAX: usize = 1000;
const RESP_MAX: usize = 1000;

/// Bytes of state the image asks for (`mkmod` in Cargo.toml).
pub const STATE_BYTES: usize = 16;

/// The only memory of the console: the next request id.
#[derive(Default)]
pub struct State {
    next_id: u32,
}

// ---- catalog lookups ----

fn cname(off: u16) -> &'static [u8] {
    let rest = &CAT_POOL[off as usize..];
    let n = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    &rest[..n]
}

fn digits(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut x: u64 = 0;
    for c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        x = x.wrapping_mul(10).wrapping_add((c - b'0') as u64);
    }
    Some(x)
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A component by name, or by number when `s` is one.
fn find_comp(s: &[u8], by_id: bool) -> Option<&'static CatComp> {
    let id = if by_id { digits(s) } else { None };
    CAT_COMPS[..CAT_COMPS_N].iter().find(|c| (c.name != 0 && cname(c.name) == s) || id == Some(c.id as u64))
}

fn find_method(c: &CatComp, s: &[u8], by_id: bool) -> Option<&'static CatMethod> {
    let id = if by_id { digits(s) } else { None };
    CAT_METHODS[c.first as usize..(c.first + c.count) as usize].iter().find(|m| cname(m.name) == s || id == Some(m.id as u64))
}

fn method_of(comp: u64, method: u64) -> Option<&'static CatMethod> {
    let c = CAT_COMPS[..CAT_COMPS_N].iter().find(|c| c.id as u64 == comp)?;
    CAT_METHODS[c.first as usize..(c.first + c.count) as usize].iter().find(|m| m.id as u64 == method)
}

fn req_fields(m: &CatMethod) -> &'static [CatField] {
    &CAT_FIELDS[m.req as usize..(m.req + m.nreq) as usize]
}

fn resp_fields(m: Option<&CatMethod>) -> &'static [CatField] {
    match m {
        Some(m) => &CAT_FIELDS[m.resp as usize..(m.resp + m.nresp) as usize],
        None => &[],
    }
}

fn record_fields(r: &CatRecord) -> &'static [CatField] {
    &CAT_FIELDS[r.first as usize..(r.first + r.count) as usize]
}

fn field_by_tag(list: &[CatField], tag: u64) -> Option<&CatField> {
    list.iter().find(|f| f.tag as u64 == tag)
}

fn find_field<'a>(list: &'a [CatField], name: &[u8]) -> Option<&'a CatField> {
    list.iter().find(|f| f.name != 0 && cname(f.name) == name)
}

// ---- text output ----

/// Output text, bounded like the C console's buffer (one byte stays free).
struct Out {
    b: Vec<u8>,
    cap: usize,
}

impl Out {
    fn new(cap: usize) -> Out {
        Out { b: Vec::new(), cap }
    }
    fn put(&mut self, s: &[u8]) {
        for &c in s {
            if self.b.len() + 1 < self.cap {
                self.b.push(c);
            }
        }
    }
    fn s(&mut self, s: &str) {
        self.put(s.as_bytes());
    }
    fn u(&mut self, mut v: u64) {
        let mut d = [0u8; 20];
        let mut k = 0;
        loop {
            d[k] = b'0' + (v % 10) as u8;
            k += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        while k > 0 {
            k -= 1;
            self.put(&d[k..k + 1]);
        }
    }
    fn n(&mut self, v: i64) {
        if v < 0 {
            self.s("-");
        }
        self.u(v.unsigned_abs());
    }
    fn hex(&mut self, data: &[u8]) {
        const H: &[u8; 16] = b"0123456789abcdef";
        for c in data {
            self.put(&[H[(c >> 4) as usize], H[(c & 15) as usize]]);
        }
    }
    fn ends_with_newline(&self) -> bool {
        self.b.last() == Some(&b'\n')
    }
}

/// A JSON-like dump of the host's CBOR: numeric tags stay numeric. False for malformed input.
fn info_json(o: &mut Out, r: &mut Reader, depth: u32) -> bool {
    if depth > 32 {
        return false;
    }
    let Some(it) = r.next() else { return false };
    match it {
        Item::Map(n) | Item::Array(n) => {
            let map = matches!(it, Item::Map(_));
            o.s(if map { "{" } else { "[" });
            for i in 0..n {
                if i > 0 {
                    o.s(",");
                }
                if map {
                    if !info_json(o, r, depth + 1) {
                        return false;
                    }
                    o.s(":");
                }
                if !info_json(o, r, depth + 1) {
                    return false;
                }
            }
            o.s(if map { "}" } else { "]" });
        }
        Item::Text(t) | Item::Bytes(t) => {
            let bytes = matches!(it, Item::Bytes(_));
            o.s("\"");
            if bytes {
                o.s("hex:");
            }
            for &c in t {
                if bytes {
                    o.hex(&[c]);
                } else if c == b'"' || c == b'\\' {
                    o.put(&[b'\\', c]);
                } else if c < 32 {
                    o.s("\\u00");
                    o.hex(&[c]);
                } else {
                    o.put(&[c]);
                }
            }
            o.s("\"");
        }
        Item::Uint(v) => o.u(v),
        Item::Neg(v) => {
            o.s("-");
            if v == u64::MAX {
                o.s("18446744073709551616");
            } else {
                o.u(v + 1);
            }
        }
        Item::Bool(b) => {
            if b {
                o.s("true");
            } else {
                o.s("false");
            }
        }
        Item::Null => o.s("null"),
    }
    true
}

fn error_rc(o: &mut Out, what: &str, rc: i32) {
    o.s("error: ");
    o.s(what);
    o.s(" failed: ");
    o.n(rc as i64);
    o.s("\n");
}

/// `status`: the device info as JSON, written to the serial port directly. True when nothing is left to print.
fn cmd_status(hw: &Hw, o: &mut Out) -> bool {
    let mut cap = RESP_MAX;
    let (buf, n) = loop {
        let mut buf = vec![0u8; cap];
        match hw.device_info(&mut buf) {
            Err(e) if e == ERR_SIZE => {
                if cap > usize::MAX / 2 {
                    o.s("error: status too large\n");
                    return false;
                }
                cap *= 2;
            }
            Err(e) if e == ERR_UNSUPPORTED => {
                o.s("error: this host has no device info\n");
                return false;
            }
            Err(_) => {
                o.s("error: device info unavailable\n");
                return false;
            }
            Ok(n) => break (buf, n),
        }
    };
    let mut dump = Out::new(n * 6 + 2);
    let mut r = Reader::new(&buf[..n]);
    if !info_json(&mut dump, &mut r, 0) || !r.rest().is_empty() {
        o.s("error: malformed device info\n");
        return false;
    }
    dump.s("\n");
    if hw.features() & FEAT_SERIAL != 0 {
        hw.serial_write(&dump.b);
    }
    o.b.is_empty()
}

// ---- parse: a line of text to a request ----

/// The words of a line; a double-quoted value may contain spaces and the quotes are removed.
fn tokenize(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut t: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < line.len() {
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        if t.len() == MAX_TOKENS {
            return None;
        }
        let mut w = Vec::new();
        let mut quoted = false;
        while i < line.len() && (quoted || line[i] != b' ') {
            if line[i] == b'"' {
                quoted = !quoted;
            } else {
                w.push(line[i]);
            }
            i += 1;
        }
        t.push(w);
    }
    Some(t)
}

fn word_is(t: &[u8], w: &str) -> bool {
    t == w.as_bytes()
}

/// A line turned into what to do with it.
enum Parsed {
    /// Answered here: `help`, a syntax error, an empty line.
    Local(&'static str),
    /// The module component `comp`, method `meth`, with the request fields as a CBOR map.
    Request { comp: u64, meth: u64, body: Vec<u8> },
}

/// Encode `value` for a field of kind `kind`; false when it does not fit.
fn encode_value(w: &mut Writer, kind: u16, v: &[u8]) -> bool {
    if kind == CAT_K_MAP || kind == CAT_K_ARRAY {
        return false;
    }
    if kind == CAT_K_TEXT {
        w.text_raw(v);
        return true;
    }
    if kind == CAT_K_BOOL || kind == CAT_K_ANY {
        if v == b"true" {
            w.boolean(true);
            return true;
        }
        if v == b"false" {
            w.boolean(false);
            return true;
        }
        if kind == CAT_K_BOOL {
            return false;
        }
    }
    if kind == CAT_K_BYTES || (kind == CAT_K_ANY && v.len() > 4 && v.starts_with(b"hex:")) {
        let v = v.strip_prefix(b"hex:").unwrap_or(v);
        if v.len() % 2 != 0 {
            return false;
        }
        let mut b = Vec::with_capacity(v.len() / 2);
        for p in v.chunks(2) {
            let (Some(hi), Some(lo)) = (hexval(p[0]), hexval(p[1])) else { return false };
            b.push(hi << 4 | lo);
        }
        w.bytes(&b);
        return true;
    }
    if let Some(u) = digits(v) {
        w.uint(u);
        return true;
    }
    if v.len() > 1 && v[0] == b'-' {
        if let Some(u) = digits(&v[1..]) {
            if u <= i64::MAX as u64 {
                w.int(-(u as i64));
                return true;
            }
        }
    }
    if kind == CAT_K_ANY {
        w.text_raw(v);
        return true;
    }
    false
}

struct Arg<'a> {
    tag: u64,
    kind: u16,
    value: &'a [u8],
}

fn parse(line: &[u8]) -> Parsed {
    let Some(t) = tokenize(line) else { return Parsed::Local("error: too many words") };
    if t.is_empty() {
        return Parsed::Local("");
    }
    // component and method
    let mut c: Option<&CatComp> = None;
    let mut m: Option<&CatMethod> = None;
    let mut used = 1;
    let dot = t[0].iter().position(|b| *b == b'.' || *b == b'=').unwrap_or(t[0].len());
    if dot < t[0].len() && t[0][dot] == b'.' {
        c = find_comp(&t[0][..dot], true);
        if let Some(c) = c {
            m = find_method(c, &t[0][dot + 1..], true);
        }
    } else if t.len() > 1 && {
        c = find_comp(&t[0], false);
        c.is_some()
    } {
        m = find_method(c.unwrap(), &t[1], false);
        used = 2;
    } else {
        // a bare method: the first component that has it, main's first
        for comp in &CAT_COMPS[..CAT_COMPS_N] {
            c = Some(comp);
            m = find_method(comp, &t[0], false);
            if m.is_some() {
                break;
            }
        }
    }
    let (Some(c), Some(m)) = (c, m) else { return Parsed::Local("error: unknown method (try help)") };

    // fields: key=value words, and plain words filling the method's remaining declared fields in order
    let req = req_fields(m);
    let mut args: Vec<Arg> = Vec::new();
    let mut positional: Vec<usize> = Vec::new();
    for (i, tok) in t.iter().enumerate().skip(used) {
        let Some(eq) = tok.iter().position(|b| *b == b'=') else {
            positional.push(i);
            continue;
        };
        if eq == 0 {
            return Parsed::Local("error: expected key=value");
        }
        let key = &tok[..eq];
        let (tag, kind);
        if key[0] == b'@' && digits(&key[1..]).is_some() {
            tag = digits(&key[1..]).unwrap_or(0);
            kind = field_by_tag(req, tag).map_or(CAT_K_ANY, |f| f.kind as u16);
        } else if let Some(n) = digits(key) {
            tag = n;
            kind = field_by_tag(req, tag).map_or(CAT_K_ANY, |f| f.kind as u16);
        } else {
            match find_field(req, key) {
                Some(f) => {
                    tag = f.tag as u64;
                    kind = f.kind as u16;
                }
                None => {
                    // a method whose last field is text (`start lora probe spi=2`): the word belongs to that text
                    if m.nreq > 0 && req[m.nreq as usize - 1].kind as u16 == CAT_K_TEXT {
                        positional.push(i);
                        continue;
                    }
                    return Parsed::Local("error: unknown field");
                }
            }
        }
        args.push(Arg { tag, kind, value: &tok[eq + 1..] });
    }
    let mut joined: Vec<u8> = Vec::new();
    let mut late: Vec<Arg> = Vec::new();
    if !positional.is_empty() {
        let mut fi = 0usize;
        let mut pi = 0;
        while pi < positional.len() {
            let i = positional[pi];
            while fi < req.len() && args.iter().any(|a| a.tag == req[fi].tag as u64) {
                fi += 1;
            }
            if fi >= req.len() {
                return Parsed::Local("error: too many words");
            }
            let last = fi + 1 == req.len();
            if req[fi].kind as u16 == CAT_K_TEXT && last {
                // the last text field takes the rest of the line
                for (j, tok) in t.iter().enumerate().skip(i) {
                    if j > i {
                        joined.push(b' ');
                    }
                    joined.extend_from_slice(tok);
                }
                late.push(Arg { tag: req[fi].tag as u64, kind: req[fi].kind as u16, value: &[] });
                break;
            }
            args.push(Arg { tag: req[fi].tag as u64, kind: req[fi].kind as u16, value: &t[i] });
            fi += 1;
            pi += 1;
        }
    }
    let mut buf = vec![0u8; OUT_MAX];
    let mut body = Writer::new(&mut buf);
    body.map(args.len() + late.len());
    for a in &args {
        body.uint(a.tag);
        if !encode_value(&mut body, a.kind, a.value) {
            return Parsed::Local("error: bad value");
        }
    }
    for a in &late {
        body.uint(a.tag);
        if !encode_value(&mut body, a.kind, &joined) {
            return Parsed::Local("error: bad value");
        }
    }
    let Some(done) = body.finish() else { return Parsed::Local("error: request too long") };
    Parsed::Request { comp: c.id as u64, meth: m.id as u64, body: done.to_vec() }
}

/// The CBOR answer of `parse` (method 1): `{12: text}` or `{1: component, 2: method, 5: fields}`. (A `Parsed::Local` built from a literal
/// would be promoted to a static with a pointer in it: a module image has no relocations, so the text is passed on its own.)
fn parsed_cbor(local: Option<&str>, p: &Parsed) -> Vec<u8> {
    let mut buf = vec![0u8; RESP_MAX];
    let mut w = Writer::new(&mut buf);
    if let Some(text) = local {
        w.map(1).uint(12).text(text);
    } else {
        match p {
            Parsed::Local(text) => {
                w.map(1).uint(12).text(text);
            }
            Parsed::Request { comp, meth, body } => {
                w.map(3).uint(1).uint(*comp).uint(2).uint(*meth).uint(5).raw(body);
            }
        }
    }
    w.finish().map(|s| s.to_vec()).unwrap_or_default()
}

// ---- render: an answer to text ----

fn render_scalar(o: &mut Out, it: &Item, quote: bool) {
    match *it {
        Item::Uint(v) => o.u(v),
        Item::Neg(v) => {
            o.s("-");
            o.u(v.wrapping_add(1));
        }
        Item::Bool(b) => o.s(if b { "true" } else { "false" }),
        Item::Null => o.s("null"),
        Item::Text(t) => {
            let space = t.iter().any(|c| *c == b' ' || *c == b'=');
            let q = quote && (space || t.is_empty());
            if q {
                o.s("\"");
            }
            o.put(t);
            if q {
                o.s("\"");
            }
        }
        Item::Bytes(b) => {
            o.s("hex:");
            o.hex(b);
        }
        _ => {}
    }
}

/// Field names that hold a secret: `psk`, `sta_psk`, `ap_psk`, ...
fn is_secret(name: &[u8]) -> bool {
    name.ends_with(b"psk")
}

/// The value of a secret field: only whether it is set. True when it consumed the value.
fn render_secret(o: &mut Out, r: &mut Reader, f: Option<&CatField>) -> bool {
    let Some(f) = f else { return false };
    if f.name == 0 || !is_secret(cname(f.name)) {
        return false;
    }
    match r.next() {
        Some(Item::Text(t)) | Some(Item::Bytes(t)) if !t.is_empty() => o.s("set"),
        _ => o.s("unset"),
    }
    true
}

/// A map as `name=value` words. `list` are the fields of the record the keys belong to, or none.
fn render_pairs(o: &mut Out, r: &mut Reader, pairs: usize, list: &[CatField], depth: u32) {
    for i in 0..pairs {
        let Some(k) = r.next() else { return };
        let f = if let Item::Uint(tag) = k { field_by_tag(list, tag) } else { None };
        if i > 0 {
            o.s(" ");
        }
        if let Some(f) = f {
            if f.name != 0 && cname(f.name) == b"text" && f.kind as u16 == CAT_K_TEXT {
                let Some(v) = r.next() else { return };
                render_scalar(o, &v, false);
                continue;
            }
        }
        match (f, k) {
            (Some(f), _) if f.name != 0 => o.put(cname(f.name)),
            (_, Item::Uint(tag)) => {
                o.s("@");
                o.u(tag);
            }
            _ => o.s("@"),
        }
        o.s("=");
        if render_secret(o, r, f) {
            continue;
        }
        render_item(o, r, f, depth + 1);
    }
}

fn render_item(o: &mut Out, r: &mut Reader, f: Option<&CatField>, depth: u32) {
    let Some(it) = r.next() else { return };
    let rec = f.filter(|f| f.record != 0).map(|f| &CAT_RECORDS[f.record as usize - 1]);
    match it {
        Item::Map(n) => {
            o.s("{");
            if depth < 4 {
                render_pairs(o, r, n, rec.map_or(&[][..], record_fields), depth);
            }
            o.s("}");
        }
        Item::Array(n) => {
            o.s("[");
            let mut i = 0;
            while i < n && depth < 4 {
                if i > 0 {
                    o.s(" ");
                }
                let sub = if f.map_or(false, |f| f.kind as u16 == CAT_K_ARRAY && f.record != 0) { f } else { None };
                render_item(o, r, sub, depth + 1);
                i += 1;
            }
            o.s("]");
        }
        _ => render_scalar(o, &it, true),
    }
}

/// The answer map of a method as text: `name=value` words, a list of records one line each (the field names come from the catalog).
fn render_map(o: &mut Out, resp: &[CatField], res: Reader) {
    let mut r = res;
    match r.next() {
        Some(Item::Map(0)) => o.s("ok"),
        Some(Item::Map(n)) => {
            let mut scalars = 0usize;
            for _ in 0..n {
                let Some(k) = r.next() else { break };
                let f = if let Item::Uint(tag) = k { field_by_tag(resp, tag) } else { None };
                let mut probe = r;
                let Some(v) = probe.next() else { break };
                if let (Item::Array(count), Some(f)) = (v, f) {
                    if f.record != 0 {
                        // a list of records: one line each, after the scalar words
                        if scalars > 0 {
                            o.s("\n");
                            scalars = 0;
                        }
                        r = probe;
                        let rec = &CAT_RECORDS[f.record as usize - 1];
                        for _ in 0..count {
                            let Some(Item::Map(pairs)) = r.next() else { break };
                            render_pairs(o, &mut r, pairs, record_fields(rec), 1);
                            o.s("\n");
                        }
                        continue;
                    }
                }
                if scalars > 0 {
                    o.s(" ");
                }
                scalars += 1;
                if let Some(f) = f {
                    if f.name != 0 && f.kind as u16 == CAT_K_TEXT && cname(f.name) == b"text" {
                        if let Some(v) = r.next() {
                            render_scalar(o, &v, false);
                        }
                        continue;
                    }
                }
                match (f, k) {
                    (Some(f), _) if f.name != 0 => o.put(cname(f.name)),
                    (_, Item::Uint(tag)) => {
                        o.s("@");
                        o.u(tag);
                    }
                    _ => o.s("@"),
                }
                o.s("=");
                if render_secret(o, &mut r, f) {
                    continue;
                }
                render_item(o, &mut r, f, 1);
            }
            if scalars > 0 {
                o.s("\n");
            }
        }
        _ => o.s("error: bad answer"),
    }
    if !o.b.is_empty() && !o.ends_with_newline() {
        o.s("\n");
    }
}

/// The text of the answer `data` = `{1: component, 2: method, 6: result}` or `{7: error}`.
fn render_text(data: &[u8]) -> Out {
    let mut o = Out::new(OUT_MAX);
    let comp = cbor::get_int(data, 1).ok().flatten().unwrap_or(0);
    let meth = cbor::get_int(data, 2).ok().flatten().unwrap_or(0);
    if let Ok(Some(err)) = cbor::get_text(data, 7) {
        o.s("error: ");
        o.put(err);
    } else if let Ok(Some(res)) = cbor::get(data, 6) {
        let m = method_of(comp as u64, meth as u64);
        render_map(&mut o, resp_fields(m), res);
    } else {
        o.s("ok\n");
    }
    if o.b.is_empty() {
        o.s("ok");
    }
    if !o.ends_with_newline() {
        o.s("\n");
    }
    o
}

/// The CBOR answer of `render` (method 2): `{1: text}`.
fn render_cbor(data: &[u8]) -> Vec<u8> {
    let o = render_text(data);
    let mut buf = vec![0u8; RESP_MAX + 16];
    let mut w = Writer::new(&mut buf);
    w.map(1).uint(1).text_raw(&o.b);
    w.finish().map(|s| s.to_vec()).unwrap_or_default()
}

/// A CBOR array of maps (the host's slot and event records): one line of `name=value` words each.
fn render_list(o: &mut Out, data: &[u8], record: &str) {
    let rec = CAT_RECORDS[..CAT_RECORDS_N].iter().find(|r| cname(r.name) == record.as_bytes());
    let mut r = Reader::new(data);
    let (Some(rec), Some(Item::Array(n))) = (rec, r.next()) else {
        o.s("error: bad answer\n");
        return;
    };
    for _ in 0..n {
        let Some(Item::Map(pairs)) = r.next() else { break };
        render_pairs(o, &mut r, pairs, record_fields(rec), 1);
        o.s("\n");
    }
}

/// The answer a module sent, printed as the answer to `(comp, meth)`.
fn render_answer(comp: i64, meth: i64, res: &[u8], err: Option<&str>) -> Out {
    let mut buf = vec![0u8; RESP_MAX];
    let mut w = Writer::new(&mut buf);
    w.map(3).uint(1).uint(comp as u64).uint(2).uint(meth as u64);
    match err {
        Some(e) => {
            w.uint(7).text(e);
        }
        None => {
            w.uint(6);
            if res.is_empty() {
                w.map(0);
            } else {
                w.raw(res);
            }
        }
    }
    match w.finish() {
        Some(tmp) => render_text(tmp),
        None => {
            let mut o = Out::new(OUT_MAX);
            o.s("error: answer too large\n");
            o
        }
    }
}

// ---- the commands ----

/// `heap`: free memory from heap_info (bytes), or a report to the device log.
fn cmd_heap(hw: &Hw, t: &[Vec<u8>], o: &mut Out) {
    if t.len() > 1 {
        let kind = if word_is(&t[1], "regions") {
            0
        } else if word_is(&t[1], "blocks") {
            1
        } else if word_is(&t[1], "tasks") {
            2
        } else if word_is(&t[1], "trace") && t.len() > 2 {
            if word_is(&t[2], "start") {
                3
            } else if word_is(&t[2], "dump") {
                4
            } else if word_is(&t[2], "stop") {
                5
            } else {
                -1
            }
        } else {
            -1
        };
        if kind < 0 {
            o.s("error: heap [regions|blocks|tasks|trace start|dump|stop]\n");
            return;
        }
        let rc = hw.heap_dump(kind);
        if rc != 0 {
            error_rc(o, "heap", rc);
        } else {
            o.s("ok (in the device log)\n");
        }
        return;
    }
    let mut info = [0u8; 64];
    let n = hw.heap_info(&mut info);
    if n < 0 {
        error_rc(o, "heap", n);
        return;
    }
    // (no table of pointers: a module image has no relocations)
    for k in 0..6u64 {
        let v = cbor::get_int(&info[..n as usize], k + 1).ok().flatten().unwrap_or(0);
        // each arm calls: a match that returns the string would become a table of pointers
        match k {
            0 => o.s("free="),
            1 => o.s(" largest="),
            2 => o.s(" min_free="),
            3 => o.s(" total="),
            4 => o.s(" internal_free="),
            _ => o.s(" internal_largest="),
        }
        o.n(v);
    }
    o.s("\n");
}

/// `nv KEY` shows a device setting, `nv KEY=VALUE` sets it, `nv KEY=` restores the default.
fn cmd_nv(hw: &Hw, t: &[Vec<u8>], o: &mut Out) {
    if t.len() != 2 {
        o.s("error: nv KEY | nv KEY=VALUE | nv KEY=\n");
        return;
    }
    let tok = &t[1];
    let eq = tok.iter().position(|b| *b == b'=').unwrap_or(tok.len());
    let write = eq < tok.len();
    let mut val = [0u8; 48];
    let rc = hw.nv(&tok[..eq], if write { Some(&tok[eq + 1..]) } else { None }, &mut val);
    if rc == ERR_UNSUPPORTED {
        o.s("error: this host has no device settings\n");
        return;
    }
    if rc == ERR_NOTFOUND && !write {
        o.put(&tok[..eq]);
        o.s(" is unknown or not set\n");
        return;
    }
    if rc < 0 {
        error_rc(o, "nv", rc);
        return;
    }
    o.put(&tok[..eq]);
    if write {
        if tok.len() == eq + 1 {
            o.s(" restored to the default (applies at the next start)\n");
        } else {
            o.s(" set (applies at the next start)\n");
        }
        return;
    }
    o.s("=");
    o.put(&val[..(rc as usize).min(val.len())]);
    o.s("\n");
}

/// `wifi scan`: one line per network in range, strongest first (the first call starts the scan, a later one has the list).
fn wifi_scan_list(hw: &Hw, o: &mut Out) {
    let mut buf = [0u8; 1024];
    let n = hw.wifi_scan(&mut buf);
    if n == ERR_BUSY {
        o.s("scanning; run `wifi scan` again in a few seconds\n");
        return;
    }
    if n < 0 {
        error_rc(o, "wifi scan", n);
        return;
    }
    let data = &buf[..n as usize];
    let mut r = Reader::new(data);
    let ok = matches!(r.next(), Some(Item::Map(1))) && r.next().is_some();
    let Some(Item::Array(count)) = (if ok { r.next() } else { None }) else {
        o.s("error: bad scan answer\n");
        return;
    };
    for _ in 0..count {
        let entry = r.rest();
        let mut after = r;
        if !after.skip() {
            break;
        }
        let elen = entry.len() - after.rest().len();
        let entry = &entry[..elen];
        let name = match cbor::get(entry, 1) {
            Ok(Some(mut v)) => match v.next() {
                Some(Item::Bytes(b)) | Some(Item::Text(b)) => b,
                _ => &[],
            },
            _ => &[],
        };
        let int = |k| cbor::get_int(entry, k).ok().flatten().unwrap_or(0);
        o.s("ssid=\"");
        o.put(name);
        o.s("\" ch=");
        o.n(int(4));
        o.s(" rssi=");
        o.n(int(3));
        o.s(" auth=");
        o.n(int(5));
        if let Ok(Some(mut v)) = cbor::get(entry, 2) {
            if let Some(Item::Bytes(b)) = v.next() {
                if b.len() == 6 {
                    o.s(" bssid=");
                    for (k, c) in b.iter().enumerate() {
                        o.hex(&[*c]);
                        if k < 5 {
                            o.s(":");
                        }
                    }
                }
            }
        }
        o.s("\n");
        r = after;
    }
}

/// `wifi` shows the state; `wifi ssid=NAME psk=SECRET [ch=N]` stores the network and joins it; `wifi ap=1|0` runs the soft AP;
/// `wifi start|stop` switches the radio; `wifi scan` lists the networks in range. All of it goes through the `wifi` component.
fn cmd_wifi(hw: &Hw, t: &[Vec<u8>], o: &mut Out) {
    use xip_abi::{WifiIntent, WifiState};
    let wifi = if hw.features() & FEAT_WIFI != 0 { hw.wifi() } else { None };
    let Some((get, set, state)) = wifi.and_then(|w| Some((w.get?, w.set?, w.state?))) else {
        o.s("error: this host has no Wi-Fi\n");
        return;
    };
    let ctx = hw.ctx();
    let mut want = WifiIntent::default();
    let mut generation = 0u32;
    let rc = unsafe { get(ctx, &mut want, &mut generation) };
    if rc != 0 {
        error_rc(o, "wifi", rc);
        return;
    }
    let mut changed = false;
    for tok in &t[1..] {
        if word_is(tok, "scan") {
            wifi_scan_list(hw, o);
            return;
        }
        if word_is(tok, "stop") {
            want.radio = 0;
            changed = true;
            continue;
        }
        if word_is(tok, "start") {
            want.radio = 1;
            want.sta = (want.sta_ssid_len != 0) as u8;
            changed = true;
            continue;
        }
        let eq = tok.iter().position(|b| *b == b'=').unwrap_or(tok.len());
        let v: &[u8] = if eq < tok.len() { &tok[eq + 1..] } else { &[] };
        let key = &tok[..eq];
        if key == b"ssid" && v.len() <= want.sta_ssid.len() {
            want.sta_ssid = [0; 32];
            want.sta_ssid[..v.len()].copy_from_slice(v);
            want.sta_ssid_len = v.len() as u8;
            want.radio = 1;
            want.sta = (!v.is_empty()) as u8;
            hw.nv(b"sta_ssid", Some(v), &mut []);
        } else if key == b"psk" && v.len() <= want.sta_psk.len() {
            want.sta_psk = [0; 64];
            want.sta_psk[..v.len()].copy_from_slice(v);
            want.sta_psk_len = v.len() as u8;
            hw.nv(b"sec:sta", Some(v), &mut []);
        } else if key == b"ch" && (1..=2).contains(&v.len()) {
            want.channel = if v.len() == 2 { (v[0].wrapping_sub(b'0')) * 10 + v[1].wrapping_sub(b'0') } else { v[0].wrapping_sub(b'0') };
        } else if key == b"ap" && v.len() == 1 {
            want.ap = (v[0] == b'1') as u8;
            if want.ap != 0 {
                want.radio = 1;
            }
        } else {
            o.s("error: wifi [ssid=NAME] [psk=SECRET] [ch=N] [ap=1|0] [start|stop|scan] (empty value clears)\n");
            return;
        }
        changed = true;
    }
    if changed {
        let rc = unsafe { set(ctx, &want, &mut generation) };
        if rc != 0 {
            error_rc(o, "wifi", rc);
            return;
        }
    }
    let mut st = WifiState { size: core::mem::size_of::<WifiState>() as u32, ..WifiState::default() };
    let rc = unsafe { state(ctx, &mut st) };
    if rc != 0 {
        error_rc(o, "wifi", rc);
        return;
    }
    o.s("state=");
    if st.sta == 2 {
        o.s("up");
    } else if st.sta == 1 {
        o.s("joining");
    } else {
        o.s("off");
    }
    if st.sta == 2 {
        o.s(" ssid=\"");
        o.put(&st.ssid[..(st.ssid_len as usize).min(32)]);
        o.s("\" ip=");
        for k in 0..4 {
            o.n(st.ip4[k] as i64);
            if k < 3 {
                o.s(".");
            }
        }
        o.s(" rssi=");
        o.n(st.rssi as i64);
    }
    if st.ap != 0 {
        o.s(" ap=up");
    }
    if st.channel != 0 {
        o.s(" ch=");
        o.n(st.channel as i64);
    }
    o.s(" wanted_ssid=\"");
    o.put(&want.sta_ssid[..(want.sta_ssid_len as usize).min(32)]);
    if want.sta_psk_len != 0 {
        o.s("\" psk=set\n");
    } else {
        o.s("\" psk=unset\n");
    }
}

fn help_text(o: &mut Out) {
    o.s("status | start NAME [TEXT] | stop NAME | events | reset | heap [regions|blocks|tasks|trace ...] | nv KEY[=VALUE] | wifi [ssid=NAME] [psk=SECRET] [start|stop]\n");
    o.s("component.method key=value ... (values: 12, -3, true, hex:0a0b, text, \"two words\"); wifi.get/set and wifi.tuning/set_tuning call the host's components\n");
    for c in &CAT_COMPS[..CAT_COMPS_N] {
        o.put(cname(c.name));
        o.s(":");
        for m in &CAT_METHODS[c.first as usize..(c.first + c.count) as usize] {
            o.s(" ");
            o.put(cname(m.name));
        }
        o.s("\n");
    }
}

/// The manifest of the installed module is the module's own word on which methods it has; when it has one, a method it does not list is
/// refused here instead of being sent. No manifest, no such slot or a manifest too large: the catalog's word stands.
fn manifest_allows(hw: &Hw, comp: u64, method: u64, o: &mut Out) -> bool {
    let Some(c) = CAT_COMPS[..CAT_COMPS_N].iter().find(|c| c.id as u64 == comp) else { return true };
    let name = cname(c.name);
    if hw.features() & FEAT_MODULES == 0 {
        return true;
    }
    let mut m = vec![0u8; 1024];
    let n = hw.module_manifest(name, &mut m);
    if n <= 0 {
        return true;
    }
    let Ok(Some(mut r)) = cbor::get(&m[..n as usize], 3) else { return true };
    let Some(Item::Array(count)) = r.next() else { return true };
    for _ in 0..count {
        let start = r.rest();
        let mut after = r;
        if !after.skip() {
            return true;
        }
        let item = &start[..start.len() - after.rest().len()];
        if cbor::get_int(item, 1).ok().flatten() == Some(method as i64) {
            return true;
        }
        r = after;
    }
    o.s("error: ");
    o.put(name);
    o.s(" does not list method ");
    o.n(method as i64);
    o.s(" in its manifest\n");
    false
}

// ---- native calls ----
// A method of a host component (`wifi.set`, ...) is called through the component's table, not sent to a module: the tagged fields of the
// line fill the request struct of the C function, the function is called, and the output struct comes back as tagged fields, which the
// catalog turns into text like any other answer. `tables.rs` (generated from native.h) says where the member is and how the structs are laid out.

fn nat_find(comp: u64, meth: u64) -> Option<&'static NatMethod> {
    NAT_METHODS[..NAT_METHODS_N].iter().find(|m| m.comp as u64 == comp && m.method as u64 == meth)
}

fn load_le(p: &[u8]) -> u64 {
    p.iter().take(8).enumerate().fold(0u64, |a, (i, b)| a | (*b as u64) << (8 * i))
}

fn store_le(p: &mut [u8], v: u64) {
    for (i, b) in p.iter_mut().take(8).enumerate() {
        *b = (v >> (8 * i)) as u8;
    }
}

/// Does the CBOR integer (negative: -1 - arg) fit a field of `size` bytes?
fn nat_fits(size: usize, signed: bool, negative: bool, arg: u64) -> bool {
    if negative && !signed {
        return false;
    }
    if size >= 8 {
        return if signed { arg <= i64::MAX as u64 } else { true };
    }
    let limit: u64 = match (size, signed) {
        (1, true) => 0x7f,
        (1, false) => 0xff,
        (2, true) => 0x7fff,
        (2, false) => 0xffff,
        (_, true) => 0x7fff_ffff,
        (_, false) => 0xffff_ffff,
    };
    // for a negative value the CBOR argument is -1 - value, so the same limit holds (-128 is arg 127 of a signed byte)
    arg <= limit
}

fn nat_error(o: &mut Out, cm: Option<&CatMethod>, f: &NatField, what: &str) {
    o.s("error: ");
    match cm.and_then(|m| field_by_tag(req_fields(m), f.tag as u64)) {
        Some(cf) if cf.name != 0 => o.put(cname(cf.name)),
        _ => o.u(f.tag as u64),
    }
    o.s(what);
    o.s("\n");
}

/// The struct bytes from the tagged fields of a request; an absent field stays zero.
fn nat_fill(s: &NatStruct, buf: &mut [u8], map: Option<&[u8]>, cm: Option<&CatMethod>, o: &mut Out) -> bool {
    for f in &NAT_FIELDS[s.first as usize..(s.first + s.count) as usize] {
        let off = f.offset as usize;
        let size = f.size as usize;
        if f.kind as u16 == NAT_K_SIZE {
            store_le(&mut buf[off..off + size], s.size as u64);
            continue;
        }
        let Some(map) = map else { continue };
        let Ok(Some(mut r)) = cbor::get(map, f.tag as u64) else { continue };
        let Some(it) = r.next() else { continue };
        if f.kind as u16 == NAT_K_TEXT || f.kind as u16 == NAT_K_BYTES {
            let (Item::Text(d) | Item::Bytes(d)) = it else {
                nat_error(o, cm, f, " wants text or hex:");
                return false;
            };
            if if f.len_size != 0 { d.len() > size } else { d.len() != size } {
                nat_error(o, cm, f, if f.len_size != 0 { " is too long" } else { " has the wrong length" });
                return false;
            }
            buf[off..off + d.len()].copy_from_slice(d);
            if f.len_size != 0 {
                let lo = f.len_offset as usize;
                store_le(&mut buf[lo..lo + f.len_size as usize], d.len() as u64);
            }
            continue;
        }
        if let (Item::Bool(b), true) = (it, f.kind as u16 == NAT_K_BOOL) {
            store_le(&mut buf[off..off + size], b as u64);
            continue;
        }
        let (negative, arg) = match it {
            Item::Uint(a) => (false, a),
            Item::Neg(a) => (true, a),
            _ => {
                nat_error(o, cm, f, " wants a number");
                return false;
            }
        };
        if !nat_fits(size, f.is_signed != 0, negative, arg) {
            nat_error(o, cm, f, " is out of range");
            return false;
        }
        let v = if negative { !arg } else { arg };
        store_le(&mut buf[off..off + size], v);
    }
    true
}

/// The tagged fields of a struct that has something to say (a text of length 0 or all-zero bytes are left out); the pairs go to `w`
/// without a map header.
fn nat_emit(s: &NatStruct, buf: &[u8], w: &mut Writer) -> usize {
    let mut pairs = 0;
    for f in &NAT_FIELDS[s.first as usize..(s.first + s.count) as usize] {
        let off = f.offset as usize;
        let size = f.size as usize;
        let p = &buf[off..off + size];
        let kind = f.kind as u16;
        if kind == NAT_K_SIZE {
            continue;
        }
        if kind == NAT_K_TEXT || kind == NAT_K_BYTES {
            let n = if f.len_size != 0 {
                let lo = f.len_offset as usize;
                (load_le(&buf[lo..lo + f.len_size as usize]) as usize).min(size)
            } else if p.iter().all(|b| *b == 0) {
                0
            } else {
                size
            };
            if n == 0 {
                continue;
            }
            w.uint(f.tag as u64);
            if kind == NAT_K_TEXT {
                w.text_raw(&p[..n]);
            } else {
                w.bytes(&p[..n]);
            }
        } else {
            let v = load_le(p);
            w.uint(f.tag as u64);
            if kind == NAT_K_BOOL {
                w.boolean(v != 0);
            } else if kind == NAT_K_INT {
                let bits = 8 * size.min(8) as u32;
                let signed = if bits >= 64 { v as i64 } else { ((v << (64 - bits)) as i64) >> (64 - bits) };
                w.int(signed);
            } else {
                w.uint(v);
            }
        }
        pairs += 1;
    }
    pairs
}

fn native_call(hw: &Hw, nm: &NatMethod, cm: Option<&CatMethod>, fields: &[u8], o: &mut Out) {
    let name = &NAT_POOL[nm.table as usize..];
    let Ok(cname_) = CStr::from_bytes_until_nul(name) else { return };
    let table = hw.get_component(cname_) as *const u8;
    if table.is_null() {
        o.s("error: this host has no component ");
        o.put(cname_.to_bytes());
        o.s("\n");
        return;
    }
    let table_size = unsafe { core::ptr::read_unaligned(table.add(4) as *const u32) } as usize;
    let member = 8 + nm.slot as usize * core::mem::size_of::<usize>(); // after {version, size}
    if member + core::mem::size_of::<usize>() > table_size {
        o.s("error: this host's component does not have that member\n");
        return;
    }
    let fp = unsafe { core::ptr::read_unaligned(table.add(member) as *const usize) };
    if fp == 0 || nm.nparams > 4 {
        o.s("error: not available on this host\n");
        return;
    }
    let np = nm.nparams as usize;
    // each argument is a zeroed struct (4-byte aligned) or an 8-byte scalar slot
    let mut store: Vec<Vec<u32>> = Vec::new();
    let mut arg = [0usize; 4];
    let mut outs: [Option<usize>; 4] = [None; 4];
    for i in 0..np {
        let p = &NAT_PARAMS[nm.first_param as usize + i];
        if p.role == NAT_CTX {
            arg[i] = hw.ctx() as usize;
            continue;
        }
        let is_struct = p.what >= 0x100;
        let s = if is_struct { Some(&NAT_STRUCTS[p.what as usize - 0x100]) } else { None };
        let size = s.map_or(8, |s| s.size as usize);
        let mut b = vec![0u32; size.div_ceil(4)];
        if let Some(s) = s {
            let bytes = unsafe { core::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u8, size) };
            if !nat_fill(s, bytes, if p.role == NAT_IN { Some(fields) } else { None }, cm, o) {
                return;
            }
        }
        arg[i] = b.as_ptr() as usize;
        if p.role == NAT_OUT {
            outs[i] = Some(store.len());
        }
        store.push(b);
    }
    let rc: c_int = unsafe {
        match np {
            1 => core::mem::transmute::<usize, extern "C" fn(*mut c_void) -> c_int>(fp)(arg[0] as *mut c_void),
            2 => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void) -> c_int>(fp)(arg[0] as *mut c_void, arg[1] as *mut c_void),
            3 => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> c_int>(fp)(
                arg[0] as *mut c_void,
                arg[1] as *mut c_void,
                arg[2] as *mut c_void,
            ),
            _ => core::mem::transmute::<usize, extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> c_int>(fp)(
                arg[0] as *mut c_void,
                arg[1] as *mut c_void,
                arg[2] as *mut c_void,
                arg[3] as *mut c_void,
            ),
        }
    };
    if rc < 0 {
        error_rc(o, core::str::from_utf8(cname_.to_bytes()).unwrap_or("call"), rc);
        return;
    }
    let mut scratch = vec![0u8; RESP_MAX];
    let mut pw = Writer::new(&mut scratch);
    let mut pairs = 0;
    for i in 0..np {
        let p = &NAT_PARAMS[nm.first_param as usize + i];
        let Some(si) = outs[i] else { continue };
        let bytes = unsafe { core::slice::from_raw_parts(store[si].as_ptr() as *const u8, store[si].len() * 4) };
        if p.what >= 0x100 {
            pairs += nat_emit(&NAT_STRUCTS[p.what as usize - 0x100], bytes, &mut pw);
            continue;
        }
        if p.tag == 0 {
            continue;
        }
        let v = load_le(&bytes[..p.what as usize]);
        pw.uint(p.tag as u64);
        if p.is_signed != 0 && p.what < 8 {
            pw.int(v as u32 as i32 as i64);
        } else {
            pw.uint(v);
        }
        pairs += 1;
    }
    let Some(body) = pw.finish() else {
        o.s("error: answer too long\n");
        return;
    };
    let mut result = vec![0u8; RESP_MAX + 16];
    let mut rw = Writer::new(&mut result);
    rw.map(pairs).raw(body);
    let Some(res) = rw.finish() else {
        o.s("error: answer too long\n");
        return;
    };
    render_map(o, resp_fields(cm), Reader::new(res));
}

/// `NAME.METHOD key=value`: the request payload by the catalog, sent as a packet to the module. True when it was sent (the answer comes
/// later); false when `o` already has the text (a syntax error, or nobody to send to).
fn cmd_call(hw: &Hw, st: &mut State, line: &[u8], o: &mut Out) -> bool {
    let (comp, meth, body) = match parse(line) {
        Parsed::Local(text) => {
            o.s(text);
            return false;
        }
        Parsed::Request { comp, meth, body } => (comp, meth, body),
    };
    // the fields as the bytes of the CBOR map the request carries
    let fields: &[u8] = &body;
    if let Some(nm) = nat_find(comp, meth) {
        // a method of a host component: called, not sent
        native_call(hw, nm, method_of(comp, meth), fields, o);
        return false;
    }
    if NAT_METHODS[..NAT_METHODS_N].iter().any(|m| m.comp as u64 == comp) {
        // the rest of a host component has no function to call from text
        o.s("error: not a call the console can make (see the built-in commands: wifi, wifi scan, status)\n");
        return false;
    }
    if !manifest_allows(hw, comp, meth, o) {
        return false;
    }
    let Ok(pkt) = hw.pkt_alloc() else {
        o.s("error: no packet\n");
        return false;
    };
    let ok = match hw.pkt_buf(pkt) {
        Ok((window, room)) if fields.len() <= room => {
            unsafe { core::ptr::copy_nonoverlapping(fields.as_ptr(), window, fields.len()) };
            true
        }
        _ => false,
    };
    if !ok {
        hw.pkt_free(pkt);
        o.s("error: request too long\n");
        return false;
    }
    st.next_id = st.next_id.wrapping_add(1);
    if st.next_id == 0 {
        st.next_id = 1;
    }
    let rc = hw.send(pkt, fields.len(), comp as u32, comp as i32, meth as i32, st.next_id as u64);
    if rc == ERR_NOTFOUND {
        o.s("error: module not running (start it)\n");
        return false;
    }
    if rc == ERR_BUSY {
        o.s("error: busy (the module has no room for it yet)\n");
        return false;
    }
    if rc != 0 {
        error_rc(o, "send", rc);
        return false;
    }
    true
}

/// One line in; the text goes to `o`. True when the answer is still to come.
fn do_line(hw: &Hw, st: &mut State, line: &[u8], o: &mut Out) -> bool {
    let Some(t) = tokenize(line) else {
        o.s("error: too many words\n");
        return false;
    };
    if t.is_empty() {
        return false;
    }
    let cmd = &t[0];
    if word_is(cmd, "help") {
        help_text(o);
    } else if word_is(cmd, "nv") {
        cmd_nv(hw, &t, o);
    } else if word_is(cmd, "heap") {
        cmd_heap(hw, &t, o);
    } else if word_is(cmd, "wifi") {
        cmd_wifi(hw, &t, o);
    } else if word_is(cmd, "status") {
        return cmd_status(hw, o);
    } else if word_is(cmd, "events") || word_is(cmd, "reset") || word_is(cmd, "start") || word_is(cmd, "stop") {
        if hw.features() & FEAT_MODULES == 0 {
            o.s("error: this host has no module control\n");
            return false;
        }
        if word_is(cmd, "events") {
            let mut buf = vec![0u8; RESP_MAX];
            let len = hw.events_info(&mut buf);
            if len > 0 {
                render_list(o, &buf[..len as usize], "Event");
            }
        } else if word_is(cmd, "reset") {
            let rc = hw.reset();
            if rc != 0 {
                error_rc(o, "reset", rc);
            } else {
                o.s("ok\n");
            }
        } else if t.len() < 2 {
            o.s("error: name is required\n");
        } else {
            let stop = word_is(cmd, "stop");
            let rc = if stop {
                hw.module_stop(&t[1])
            } else {
                // the rest of the line is the start request
                let mut rest = Vec::new();
                for (j, tok) in t.iter().enumerate().skip(2) {
                    if j > 2 {
                        rest.push(b' ');
                    }
                    rest.extend_from_slice(tok);
                }
                hw.module_start(&t[1], &rest)
            };
            if rc == ERR_NOTFOUND {
                o.s("error: unknown module\n");
            } else if rc != 0 {
                error_rc(o, if stop { "stop" } else { "start" }, rc);
            } else {
                o.s("ok\n");
            }
        }
    } else if cmd_call(hw, st, line, o) {
        return true;
    }
    if o.b.is_empty() {
        o.s("ok\n");
    } else if !o.ends_with_newline() {
        o.s("\n");
    }
    false
}

fn say(hw: &Hw, o: &Out) {
    if !o.b.is_empty() && hw.features() & FEAT_SERIAL != 0 {
        hw.serial_write(&o.b);
    }
}

impl Module for State {
    fn handle(&mut self, hw: &Hw, c: &CallView) -> i32 {
        const CONTINUE: i32 = xip_abi::RET_CONTINUE as i32;
        if c.component == xip_abi::HOST_COMPONENT as i32 {
            if hw.features() & FEAT_SERIAL != 0 {
                if c.method == HOST_START as i32 {
                    hw.serial_open(C_CONSOLE, M_LINE);
                } else if c.method == HOST_STOP as i32 {
                    hw.serial_close();
                }
            }
            return CONTINUE;
        }
        // a message from a module: print it as the answer to (component, method) it carries; it may be one of many
        if c.pkt != 0 && c.from != ADDR_MAIN && !(c.component == C_CONSOLE && c.method == M_LINE) {
            let mut o = render_answer(c.component as i64, c.method as i64, c.data, None);
            if o.b.is_empty() {
                o.s("ok");
            }
            if !o.ends_with_newline() {
                o.s("\n");
            }
            say(hw, &o);
            return CONTINUE;
        }
        if c.component != C_CONSOLE {
            return CONTINUE;
        }
        if c.method == M_LINE && c.pkt != 0 {
            let mut o = Out::new(OUT_MAX);
            if do_line(hw, self, c.data, &mut o) {
                return CONTINUE; // what the module says comes as packets
            }
            say(hw, &o);
            return CONTINUE;
        }
        if !c.wants_response() {
            return CONTINUE;
        }
        let out = if c.method == M_PARSE {
            match cbor::get_text(c.data, 1) {
                Ok(Some(line)) => parsed_cbor(None, &parse(line)),
                _ => parsed_cbor(Some("error: no line"), &Parsed::Request { comp: 0, meth: 0, body: Vec::new() }),
            }
        } else if c.method == M_RENDER {
            render_cbor(c.data)
        } else {
            return CONTINUE;
        };
        if !out.is_empty() {
            hw.respond(&out);
        }
        CONTINUE
    }
}

/// Export `xip_handle` for the flash image.
#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(feature = "sim")]
export!();

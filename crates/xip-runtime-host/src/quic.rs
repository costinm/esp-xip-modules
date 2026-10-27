//! The common QUIC receive function of the host-runtime: the same classification, relay table, envelope fields and reply bookkeeping as
//! `components/xip_runtime/src/xip_quic.c` (see `include/xip_quic.h` for the formats). Pure: no pool, no links; the glue is in `link.rs`.
//! C does no QUIC termination: it reads the first byte, the version and the connection IDs and decides who gets the packet.
use std::collections::BTreeMap;

/// 1500 - 48 (IPv6 + UDP) - 4.
pub const MAX_PACKET: usize = 1448;
pub const CID_MAX: usize = 20;
pub const TABLE_MAX: usize = 64;
pub const VERSION_DMESH: u32 = 0x444d_0001;
pub const VERSION_CBOR: u32 = 0x444d_0002;
pub const REPLY_MS: u64 = 5000;
pub const REPLIES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuicClass {
    /// Empty, or the fixed bit (0x40) is clear.
    NotQuic,
    /// Truncated or invalid header.
    Malformed,
    Short,
    Long,
}

/// What the first bytes of a packet say. `body`: for a long packet the offset after the SCID, for a short one after the DCID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuicHeader {
    pub first: u8,
    pub is_long: bool,
    pub version: u32,
    pub dcid: Vec<u8>,
    pub scid: Vec<u8>,
    pub body: usize,
}

/// The length of a quic-lite connection ID from its first byte (bit 7 clear: 1; 100, 101, 110 in bits 7..5: 2, 4, 8), 0 for the reserved class.
pub fn cid_class_len(first: u8) -> usize {
    if first & 0x80 == 0 {
        return 1;
    }
    match (first >> 5) & 3 {
        0 => 2,
        1 => 4,
        2 => 8,
        _ => 0,
    }
}

/// `short_cid_len`: 0 = the quic-lite class rule, else the fixed DCID length of short headers.
pub fn classify(p: &[u8], short_cid_len: usize) -> (QuicClass, QuicHeader) {
    let mut h = QuicHeader::default();
    if p.is_empty() || p[0] & 0x40 == 0 {
        return (QuicClass::NotQuic, h);
    }
    h.first = p[0];
    if p[0] & 0x80 == 0 {
        let n = if short_cid_len != 0 { short_cid_len } else if p.len() > 1 { cid_class_len(p[1]) } else { 0 };
        let pn = (p[0] & 3) as usize + 1;
        if n == 0 || n > CID_MAX || p.len() < 1 + n + pn {
            return (QuicClass::Malformed, h);
        }
        h.dcid = p[1..1 + n].to_vec();
        h.body = 1 + n;
        return (QuicClass::Short, h);
    }
    // RFC 8999 invariants: first byte, version, DCID length + DCID, SCID length + SCID
    if p.len() < 7 {
        return (QuicClass::Malformed, h);
    }
    h.is_long = true;
    h.version = u32::from_be_bytes([p[1], p[2], p[3], p[4]]);
    let dl = p[5] as usize;
    if dl > CID_MAX || p.len() < 6 + dl + 1 {
        return (QuicClass::Malformed, h);
    }
    h.dcid = p[6..6 + dl].to_vec();
    let sl = p[6 + dl] as usize;
    let at = 7 + dl;
    if sl > CID_MAX || p.len() < at + sl {
        return (QuicClass::Malformed, h);
    }
    h.scid = p[at..at + sl].to_vec();
    h.body = at + sl;
    (QuicClass::Long, h)
}

/// A long header (first byte 0xc0, version, DCID, SCID).
pub fn long_build(version: u32, dcid: &[u8], scid: &[u8]) -> Option<Vec<u8>> {
    if dcid.len() > CID_MAX || scid.len() > CID_MAX {
        return None;
    }
    let mut v = vec![0xc0];
    v.extend_from_slice(&version.to_be_bytes());
    v.push(dcid.len() as u8);
    v.extend_from_slice(dcid);
    v.push(scid.len() as u8);
    v.extend_from_slice(scid);
    Some(v)
}

/// The routing fields of the CBOR envelope `{1: component, 2: method, 3: id, 5: fields}`: component and method are required positive
/// integers; the id is optional (None: a notification). There is no packet number in a 0x444d0002 packet, so no other source for the id.
/// The fourth value says the map is an answer (field 6 or 7), never to be dispatched as a request.
pub fn envelope(cbor: &[u8]) -> Option<(i32, i32, Option<u64>, bool)> {
    use xip_abi::cbor::{get, get_int};
    let c = get_int(cbor, 1).ok()??;
    let m = get_int(cbor, 2).ok()??;
    if c <= 0 || c > i32::MAX as i64 || m <= 0 || m > i32::MAX as i64 {
        return None;
    }
    let id = match get_int(cbor, 3) {
        Ok(None) => None,
        Ok(Some(i)) if i >= 0 => Some(i as u64),
        _ => return None,
    };
    let response = get(cbor, 6).ok()?.is_some() || get(cbor, 7).ok()?.is_some();
    Some((c as i32, m as i32, id, response))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuicKind {
    /// Terminated here: short packets go to the callback of the QUIC stack (`Runtime::set_quic_local`).
    Local,
    /// Forwarded: short packets go to the link `egress` and the peer address `token` with the DCID replaced by `out_cid`.
    Relay,
}
#[derive(Clone, Debug)]
pub struct QuicEntry {
    pub last_used_us: u64,
    pub idle_ms: u32,
    pub managed: Option<(usize, xip_abi::RelayRoute, u32)>,
    pub kind: QuicKind,
    pub egress: u32,
    pub token: u64,
    pub out_cid: Vec<u8>,
    pub address: Vec<u8>,
}

/// quic-lite's connection ID for `value`: 1 byte up to 0x7f, 2 bytes (tag 100, 13 bits) up to 0x1fff, 4 bytes (tag 101, 29 bits), 8 bytes (tag 110).
pub fn cid_encode(value: u64) -> Option<Vec<u8>> {
    if value == 0 || value >= 1 << 61 {
        return None;
    }
    let n = if value <= 0x7f { 1 } else if value <= 0x1fff { 2 } else if value <= 0x1fff_ffff { 4 } else { 8 };
    let mut v: Vec<u8> = (0..n).map(|i| (value >> (8 * (n - i - 1))) as u8).collect();
    let tag = match n { 1 => 0, 2 => 0b100, 4 => 0b101, _ => 0b110 } << 5;
    v[0] = (v[0] & if n == 1 { 0x7f } else { 0x1f }) | tag;
    Some(v)
}

/// Replace the DCID of the short packet `p` (classified as `h`) by `ncid`, as quic-lite's `rewrite_dcid_in_place`: the flags and the new CID are
/// written right before the packet number, so the packet starts `shift` bytes later (negative: earlier, into `room` bytes of headroom).
/// Writes through `p` (pointer to the packet start inside its slot). Err when it does not fit.
///
/// # Safety
/// `p` points to `len` readable and writable bytes with `room` writable bytes in front.
pub unsafe fn short_rewrite(p: *mut u8, len: usize, h: &QuicHeader, ncid: &[u8], room: usize) -> Result<isize, i32> {
    if h.is_long || ncid.is_empty() || ncid.len() > CID_MAX {
        return Err(xip_abi::ERR_ARG);
    }
    let d = h.dcid.len() as isize - ncid.len() as isize;
    if (d < 0 && (-d) as usize > room) || (len as isize - d) as usize > MAX_PACKET {
        return Err(xip_abi::ERR_SIZE);
    }
    let q = p.offset(d);
    *q = h.first;
    std::ptr::copy(ncid.as_ptr(), q.add(1), ncid.len());
    Ok(d)
}

/// One counter per way a packet can end (`xip_quic_stats` of the C runtime).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuicStats {
    pub rx: u32,
    pub declined: u32,
    pub malformed: u32,
    pub oversize: u32,
    pub short_local: u32,
    pub short_relay: u32,
    pub short_relay_failed: u32,
    pub short_unknown: u32,
    pub short_no_handler: u32,
    pub long_cbor: u32,
    pub long_cbor_bad: u32,
    pub long_cbor_no_target: u32,
    pub long_cbor_response: u32,
    pub long_other: u32,
    pub long_other_dropped: u32,
}

/// Who sent the packet and where it came from, for the local callback.
#[derive(Clone, Debug)]
pub struct QuicInfo {
    pub link: String,
    pub source: u64,
    pub signal: i32,
    pub header: QuicHeader,
}

/// A pool packet that the local callback owns. Return it with [`crate::Runtime::quic_release`]; a callback that drops it leaks the packet
/// (`Runtime::packets_in_use` shows it).
#[derive(Debug)]
pub struct QuicPacket {
    pub(crate) slot: i32,
    pub(crate) start: u16,
    pub(crate) len: u16,
}

pub(crate) struct Reply {
    pub link: u32,
    pub source: u64,
    pub id: u64,
    pub at_ms: u64,
    pub dcid: Vec<u8>,
    pub scid: Vec<u8>,
}

pub(crate) type QuicLocal = std::rc::Rc<std::cell::RefCell<dyn FnMut(&QuicInfo, QuicPacket)>>;

#[derive(Default)]
pub(crate) struct QuicState {
    pub next_relay_cid: u64,
    pub classifier: bool,
    pub local: Option<QuicLocal>,
    pub table: BTreeMap<Vec<u8>, QuicEntry>,
    pub stats: QuicStats,
    pub short_cid_len: usize,
    pub names: Vec<String>,
    pub replies: Vec<Reply>,
    next_reply: usize,
}

impl QuicState {
    pub fn is_quic_link(&self, name: &str) -> bool {
        name == "relay" || name == "ble" || name == "udp" || name == "espnow" || self.names.iter().any(|n| n == name)
    }
    pub fn note_reply(&mut self, link: u32, source: u64, id: u64, request: &QuicHeader, now_ms: u64) {
        let r = Reply { link, source, id, at_ms: now_ms, dcid: request.scid.clone(), scid: request.dcid.clone() };
        let live = |x: &Reply| now_ms.saturating_sub(x.at_ms) <= REPLY_MS;
        if let Some(at) = self.replies.iter().position(|x| x.link == link && x.source == source && x.id == id) {
            self.replies[at] = r;
        } else if self.replies.len() < REPLIES {
            self.replies.push(r);
        } else if let Some(at) = self.replies.iter().position(|x| !live(x)) {
            self.replies[at] = r;
        } else {
            self.replies[self.next_reply] = r;
            self.next_reply = (self.next_reply + 1) % REPLIES;
        }
    }
    pub fn find_reply(&self, link: u32, source: u64, id: u64, now_ms: u64) -> Option<&Reply> {
        self.replies.iter().find(|r| r.link == link && r.source == source && r.id == id && now_ms.saturating_sub(r.at_ms) <= REPLY_MS)
    }
    pub fn done_reply(&mut self, link: u32, source: u64, id: u64) {
        self.replies.retain(|r| !(r.link == link && r.source == source && r.id == id));
    }
}

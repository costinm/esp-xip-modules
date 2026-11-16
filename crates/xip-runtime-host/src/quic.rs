//! The common QUIC receive function of the host-runtime: delegates classification, the sorted relay table,
//! weighted multi-hop selection, high-range relay CID allocation, in-place DCID rewriting, and reply
//! bookkeeping to [`quic_relay::xip`] (matching `components/xip_runtime/src/xip_quic.c` and
//! `include/xip_quic.h`).

pub use quic_relay::xip::{
    CID_MAX, HOST_REPLIES, HOST_REPLY_MS, HOST_TABLE_MAX, MAX_PACKET, QuicClass, QuicEntry,
    QuicHeader, QuicKind, QuicStats, REPLIES, REPLY_MS, ReplyNote, ReplyTable, TABLE_MAX,
    VERSION_CBOR, VERSION_DMESH, XipRelayConfig, XipRelayTable, cid_class_len, cid_encode,
    classify, long_build, short_rewrite, validate_relay_route,
};

/// Convert an [`xip_abi::RelayRoute`] into a [`quic_relay::xip::RelayRoute`].
pub(crate) fn to_relay_route(r: &xip_abi::RelayRoute) -> quic_relay::xip::RelayRoute {
    // Both types are `#[repr(C)]` with identical 280-byte layout verified by compile-time assertions.
    let mut hops = [quic_relay::xip::RelayHop::default(); 4];
    for (dst, src) in hops.iter_mut().zip(r.hops.iter()) {
        *dst = quic_relay::xip::RelayHop {
            link: src.link,
            token: src.token,
            weight: src.weight,
            cid_len: src.cid_len,
            rewritten_dcid: src.rewritten_dcid,
            address_len: src.address_len,
            address: src.address,
            padding: src.padding,
        };
    }
    quic_relay::xip::RelayRoute {
        dcid: r.dcid,
        dcid_len: r.dcid_len,
        hop_count: r.hop_count,
        endpoint: r.endpoint,
        reserved: r.reserved,
        hops,
    }
}

/// Convert a [`quic_relay::xip::RelayRoute`] into an [`xip_abi::RelayRoute`].
pub(crate) fn from_relay_route(r: &quic_relay::xip::RelayRoute) -> xip_abi::RelayRoute {
    let mut hops = [xip_abi::RelayHop::default(); 4];
    for (dst, src) in hops.iter_mut().zip(r.hops.iter()) {
        *dst = xip_abi::RelayHop {
            link: src.link,
            token: src.token,
            weight: src.weight,
            cid_len: src.cid_len,
            rewritten_dcid: src.rewritten_dcid,
            address_len: src.address_len,
            address: src.address,
            padding: src.padding,
        };
    }
    xip_abi::RelayRoute {
        dcid: r.dcid,
        dcid_len: r.dcid_len,
        hop_count: r.hop_count,
        endpoint: r.endpoint,
        reserved: r.reserved,
        hops,
    }
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

pub(crate) type QuicLocal = std::rc::Rc<std::cell::RefCell<dyn FnMut(&QuicInfo, QuicPacket)>>;

#[derive(Default)]
pub(crate) struct QuicState {
    pub classifier: bool,
    pub local: Option<QuicLocal>,
    pub table: XipRelayTable,
    pub stats: QuicStats,
    pub short_cid_len: usize,
    /// The module that took the endpoint with `Quic::attach`: instance, its component and the method of its received packets.
    pub module: Option<(std::rc::Rc<crate::runtime::Inst>, i32, i32)>,
    pub names: Vec<String>,
    pub replies: ReplyTable,
}

impl QuicState {
    pub fn with_limits(max_entries: usize, max_replies: usize, reply_timeout_ms: u64) -> Self {
        Self {
            classifier: false,
            local: None,
            table: XipRelayTable::with_max_entries(max_entries),
            stats: QuicStats::default(),
            short_cid_len: 0,
            module: None,
            names: Vec::new(),
            replies: ReplyTable::with_limits(max_replies, reply_timeout_ms),
        }
    }

    pub fn is_quic_link(&self, name: &str) -> bool {
        name == "relay"
            || name == "ble"
            || name == "udp"
            || name == "espnow"
            || self.names.iter().any(|n| n == name)
    }
    pub fn note_reply(
        &mut self,
        link: u32,
        source: u64,
        id: u64,
        request: &QuicHeader,
        now_ms: u64,
    ) {
        self.replies.note(link, source, id, request, now_ms);
    }
    pub fn find_reply(&self, link: u32, source: u64, id: u64, now_ms: u64) -> Option<&ReplyNote> {
        self.replies.find(link, source, id, now_ms)
    }
    pub fn done_reply(&mut self, link: u32, source: u64, id: u64) {
        self.replies.done(link, source, id);
    }
}

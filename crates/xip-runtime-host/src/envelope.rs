//! The CBOR envelope `{1: component, 2: method, 3: id, 5: fields}` (answer `{1, 2, 3, 6: result}` or `{.., 7: error}`), parsed and built by the
//! runtime so that no module does. The same rules as `components/xip_runtime/src/xip_envelope.c` (which tests/host/test_envelope.c runs on a PC);
//! `xip_runtime.c` and this host-runtime apply them to every packet that arrives on a module's port or as a 0x444d0002 datagram.
use xip_abi::cbor::{self, Item, Reader, Writer};

pub(crate) const PENDING_MAX: usize = 6;
static EMPTY_MAP: [u8; 1] = [0xa0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Not a CBOR map: a raw frame.
    NotMap,
    /// A map without a positive integer component and method, a bad id or fields that are not a map.
    Invalid,
    Request,
    /// Has a result (6) or an error (7): never dispatched as a request.
    Answer,
}

pub(crate) struct Envelope<'a> {
    pub component: i32,
    pub method: i32,
    pub id: u64,
    pub has_id: bool,
    /// The map of field 5, or an empty map.
    pub fields: &'a [u8],
}

pub(crate) fn parse(data: &[u8]) -> (Kind, Envelope<'_>) {
    let mut e = Envelope { component: 0, method: 0, id: 0, has_id: false, fields: &EMPTY_MAP };
    if data.first().is_none_or(|b| b >> 5 != 5) {
        return (Kind::NotMap, e);
    }
    let answer = matches!(cbor::get(data, 6), Ok(Some(_))) || matches!(cbor::get(data, 7), Ok(Some(_)));
    let (Ok(Some(c)), Ok(Some(m))) = (cbor::get_int(data, 1), cbor::get_int(data, 2)) else {
        return (if answer { Kind::Answer } else { Kind::Invalid }, e);
    };
    if c <= 0 || c > i32::MAX as i64 || m <= 0 || m > i32::MAX as i64 {
        return (Kind::Invalid, e);
    }
    e.component = c as i32;
    e.method = m as i32;
    if answer {
        return (Kind::Answer, e);
    }
    match cbor::get_int(data, 3) {
        Ok(None) => {}
        Ok(Some(i)) if i >= 0 => {
            e.has_id = true;
            e.id = i as u64;
        }
        _ => return (Kind::Invalid, e),
    }
    match cbor::get(data, 5) {
        Err(()) => return (Kind::Invalid, e),
        Ok(None) => {}
        Ok(Some(mut r)) => {
            let whole = r.rest();
            if !matches!(r.next(), Some(Item::Map(_))) {
                return (Kind::Invalid, e);
            }
            let mut span = Reader::new(whole);
            if !span.skip() {
                return (Kind::Invalid, e);
            }
            e.fields = &whole[..whole.len() - span.rest().len()];
        }
    }
    (Kind::Request, e)
}

/// The head of an answer: `{1, 2, 3, 6` (the result map follows) when `error` is 0, else the whole `{1, 2, 3, 7: error}`.
pub(crate) fn head(component: i32, method: i32, id: u64, error: i32) -> Vec<u8> {
    let mut buf = [0u8; 40];
    let mut w = Writer::new(&mut buf);
    w.map(4).uint(1).int(component as i64).uint(2).int(method as i64).uint(3).uint(id);
    if error < 0 {
        w.uint(7).int(error as i64);
    } else {
        w.uint(6);
    }
    w.finish().map(|b| b.to_vec()).unwrap_or_default()
}

/// Is `p` exactly one negative integer (an error code in place of a result map)?
pub(crate) fn is_error(p: &[u8]) -> Option<i32> {
    if p.first().is_none_or(|b| b >> 5 != 1) {
        return None;
    }
    let mut r = Reader::new(p);
    let v = r.next()?.as_int()?;
    (r.rest().is_empty() && v < 0 && v >= i32::MIN as i64).then_some(v as i32)
}

/// A request handed to a module whose answer the runtime still wraps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    pub dst: u32,
    pub token: u64,
    pub id: u64,
    pub component: i32,
    pub method: i32,
}

#[derive(Default)]
pub(crate) struct Table(Vec<Pending>);

impl Table {
    pub fn add(&mut self, p: Pending) {
        if self.0.len() == PENDING_MAX {
            self.0.remove(0);
        }
        self.0.push(p);
    }
    /// The newest request waiting for an answer to (dst, token, component, method).
    pub fn find(&self, dst: u32, token: u64, component: i32, method: i32) -> Option<Pending> {
        self.0.iter().rev().find(|p| p.dst == dst && p.token == token && p.component == component && p.method == method).copied()
    }
    pub fn done(&mut self, p: &Pending) {
        if let Some(at) = self.0.iter().rposition(|q| q == p) {
            self.0.remove(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_answers_and_raw_frames() {
        let full = [0xa4, 0x01, 0x18, 104, 0x02, 0x18, 83, 0x03, 0x07, 0x05, 0xa1, 0x01, 0xf5];
        let (k, e) = parse(&full);
        assert_eq!((k, e.component, e.method, e.has_id, e.id, e.fields), (Kind::Request, 104, 83, true, 7, &full[10..]));
        let (k, e) = parse(&[0xa2, 0x01, 0x14, 0x02, 0x05]);
        assert_eq!((k, e.has_id, e.fields), (Kind::Request, false, &[0xa0][..]));
        assert_eq!(parse(&[0xa4, 0x01, 0x14, 0x02, 0x05, 0x03, 0x01, 0x07, 0x27]).0, Kind::Answer);
        assert_eq!(parse(&[0xa1, 0x01, 0x14]).0, Kind::Invalid);
        assert_eq!(parse(&[0xa2, 0x01, 0x00, 0x02, 0x05]).0, Kind::Invalid);
        assert_eq!(parse(&[0xa3, 0x01, 0x14, 0x02, 0x05, 0x05, 0x01]).0, Kind::Invalid);
        assert_eq!(parse(b"hello").0, Kind::NotMap);
        assert_eq!(parse(&[]).0, Kind::NotMap);
        for n in 1..full.len() {
            assert_eq!(parse(&full[..n]).0, Kind::Invalid, "truncated at {n}");
        }
    }

    #[test]
    fn heads_and_error_shape() {
        assert_eq!(head(104, 83, 7, 0), [0xa4, 0x01, 0x18, 104, 0x02, 0x18, 83, 0x03, 0x07, 0x06]);
        assert_eq!(head(49, 5, 256, -8), [0xa4, 0x01, 0x18, 0x31, 0x02, 0x05, 0x03, 0x19, 0x01, 0x00, 0x07, 0x27]);
        assert_eq!(is_error(&[0x27]), Some(-8));
        assert_eq!(is_error(&[0x39, 0x01, 0x2b]), Some(-300));
        assert_eq!((is_error(&[0x00]), is_error(&[0xa0]), is_error(&[0x27, 0x27])), (None, None, None));
    }

    #[test]
    fn the_newest_request_is_answered_first() {
        let mut t = Table::default();
        let p = |id| Pending { dst: 1, token: 4, id, component: 104, method: 83 };
        t.add(p(1));
        t.add(p(2));
        assert_eq!(t.find(1, 4, 104, 83), Some(p(2)));
        t.done(&p(2));
        assert_eq!(t.find(1, 4, 104, 83), Some(p(1)));
        assert_eq!(t.find(1, 5, 104, 83), None);
        for i in 0..20 {
            t.add(Pending { dst: 9, token: 9, id: i, component: 1, method: 1 });
        }
        assert_eq!(t.0.len(), PENDING_MAX);
        assert_eq!(t.find(1, 4, 104, 83), None);
    }
}

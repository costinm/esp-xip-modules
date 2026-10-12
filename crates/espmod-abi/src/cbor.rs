//! A small CBOR encoder and decoder, the Rust twin of `include/espmod_cbor.h`: integers, byte and text strings,
//! arrays, maps and booleans with definite lengths. No allocation. Floats, tags and indefinite lengths are an error
//! when decoding.

/// Writes into a caller buffer; an overflow is remembered and reported by [`Writer::finish`].
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer { buf, len: 0, overflow: false }
    }

    fn put(&mut self, data: &[u8]) {
        if self.len + data.len() > self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
    }

    fn head(&mut self, major: u8, arg: u64) {
        let m = major << 5;
        if arg < 24 {
            self.put(&[m | arg as u8]);
        } else if arg <= 0xff {
            self.put(&[m | 24, arg as u8]);
        } else if arg <= 0xffff {
            self.put(&[m | 25, (arg >> 8) as u8, arg as u8]);
        } else if arg <= 0xffff_ffff {
            self.put(&[m | 26, (arg >> 24) as u8, (arg >> 16) as u8, (arg >> 8) as u8, arg as u8]);
        } else {
            let mut h = [0u8; 9];
            h[0] = m | 27;
            h[1..].copy_from_slice(&arg.to_be_bytes());
            self.put(&h);
        }
    }

    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.head(0, v);
        self
    }

    pub fn int(&mut self, v: i64) -> &mut Self {
        if v >= 0 { self.head(0, v as u64) } else { self.head(1, (-(v + 1)) as u64) }
        self
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.head(2, v.len() as u64);
        self.put(v);
        self
    }

    pub fn text(&mut self, v: &str) -> &mut Self {
        self.head(3, v.len() as u64);
        self.put(v.as_bytes());
        self
    }

    pub fn array(&mut self, items: usize) -> &mut Self {
        self.head(4, items as u64);
        self
    }

    pub fn map(&mut self, pairs: usize) -> &mut Self {
        self.head(5, pairs as u64);
        self
    }

    pub fn boolean(&mut self, v: bool) -> &mut Self {
        self.put(&[if v { 0xf5 } else { 0xf4 }]);
        self
    }

    pub fn null(&mut self) -> &mut Self {
        self.put(&[0xf6]);
        self
    }

    /// The encoded bytes, or `None` if the buffer was too small.
    pub fn finish(self) -> Option<&'a [u8]> {
        if self.overflow { None } else { Some(&self.buf[..self.len]) }
    }
}

/// One decoded item. For arrays and maps only the head is consumed: read `n` items (`2 * n` for a map) next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    Uint(u64),
    /// The value is `-1 - n`.
    Neg(u64),
    Bytes(&'a [u8]),
    Text(&'a [u8]),
    Array(usize),
    Map(usize),
    Bool(bool),
    Null,
}

impl Item<'_> {
    /// The integer value of `Uint`/`Neg`, if it fits in 64 bits signed.
    pub fn as_int(&self) -> Option<i64> {
        match *self {
            Item::Uint(v) if v <= i64::MAX as u64 => Some(v as i64),
            Item::Neg(v) if v <= i64::MAX as u64 => Some(-1 - v as i64),
            _ => None,
        }
    }

    pub fn as_uint(&self) -> Option<u64> {
        if let Item::Uint(v) = *self { Some(v) } else { None }
    }
}

/// Reads items one after another from a byte slice.
#[derive(Clone, Copy)]
pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data }
    }

    pub fn rest(&self) -> &'a [u8] {
        self.data
    }

    /// The next item, or `None` if the input is malformed or unsupported.
    pub fn next(&mut self) -> Option<Item<'a>> {
        let (&b, rest) = self.data.split_first()?;
        let (major, info) = (b >> 5, b & 31);
        if major == 7 {
            self.data = rest;
            return match info {
                20 => Some(Item::Bool(false)),
                21 => Some(Item::Bool(true)),
                22 | 23 => Some(Item::Null),
                _ => None,
            };
        }
        if major == 6 || info >= 28 {
            return None;
        }
        let (arg, rest) = if info < 24 {
            (info as u64, rest)
        } else {
            let n = 1usize << (info - 24);
            if rest.len() < n { return None; }
            (rest[..n].iter().fold(0u64, |a, &x| a << 8 | x as u64), &rest[n..])
        };
        self.data = rest;
        match major {
            0 => Some(Item::Uint(arg)),
            1 => Some(Item::Neg(arg)),
            2 | 3 => {
                if arg > self.data.len() as u64 { return None; }
                let (body, tail) = self.data.split_at(arg as usize);
                self.data = tail;
                Some(if major == 2 { Item::Bytes(body) } else { Item::Text(body) })
            }
            4 | 5 => {
                if arg > self.data.len() as u64 { return None; }
                Some(if major == 4 { Item::Array(arg as usize) } else { Item::Map(arg as usize) })
            }
            _ => None,
        }
    }

    /// Step over one whole item, nested content included.
    pub fn skip(&mut self) -> bool {
        let mut pending = 1usize;
        while pending > 0 {
            match self.next() {
                Some(Item::Array(n)) => pending += n,
                Some(Item::Map(n)) => pending += 2 * n,
                Some(_) => {}
                None => return false,
            }
            pending -= 1;
        }
        true
    }
}

/// In the CBOR map `data`, the value stored under the unsigned key `key`, positioned for [`Reader::next`].
/// `Ok(None)` when the key is absent, `Err(())` when `data` is not a well-formed map.
pub fn get(data: &[u8], key: u64) -> Result<Option<Reader<'_>>, ()> {
    let mut r = Reader::new(data);
    let Some(Item::Map(pairs)) = r.next() else { return Err(()) };
    for _ in 0..pairs {
        let k = r.next().ok_or(())?;
        if matches!(k, Item::Array(_) | Item::Map(_)) { return Err(()); }
        if k == Item::Uint(key) { return Ok(Some(r)); }
        if !r.skip() { return Err(()); }
    }
    Ok(None)
}

/// Shorthand: the integer under `key` (`Ok(None)` if absent, `Err` if present but not an integer or malformed).
pub fn get_int(data: &[u8], key: u64) -> Result<Option<i64>, ()> {
    match get(data, key)? {
        None => Ok(None),
        Some(mut r) => r.next().and_then(|i| i.as_int()).map(Some).ok_or(()),
    }
}

/// Shorthand: the text under `key`.
pub fn get_text(data: &[u8], key: u64) -> Result<Option<&[u8]>, ()> {
    match get(data, key)? {
        None => Ok(None),
        Some(mut r) => match r.next() {
            Some(Item::Text(t)) => Ok(Some(t)),
            _ => Err(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = [0u8; 48];
        let mut w = Writer::new(&mut buf);
        w.map(4).uint(1).int(-68).uint(2).text("ok").uint(3).bytes(&[1, 2, 3]).uint(300).uint(0x12_3456_789a);
        let out = w.finish().unwrap();
        assert_eq!(&out[..4], &[0xa4, 0x01, 0x38, 67]);
        assert_eq!(get_int(out, 1), Ok(Some(-68)));
        assert_eq!(get_text(out, 2), Ok(Some(&b"ok"[..])));
        let mut r = get(out, 3).unwrap().unwrap();
        assert_eq!(r.next(), Some(Item::Bytes(&[1, 2, 3])));
        assert_eq!(get_int(out, 300), Ok(Some(0x12_3456_789a)));
        assert_eq!(get_int(out, 9), Ok(None));
        assert_eq!(get_int(out, 2), Err(()));
        assert_eq!(get_int(&out[..out.len() - 3], 300), Err(()));
    }

    #[test]
    fn overflow_is_reported() {
        let mut buf = [0u8; 4];
        let mut w = Writer::new(&mut buf);
        w.text("too long for four");
        assert!(w.finish().is_none());
    }

    #[test]
    fn rejects_what_it_does_not_decode() {
        assert_eq!(Reader::new(&[0xf9, 0, 0]).next(), None);      // float
        assert_eq!(Reader::new(&[0xc1, 0]).next(), None);          // tag
        assert_eq!(Reader::new(&[0x5f]).next(), None);             // indefinite
        assert_eq!(Reader::new(&[0x65, b'a']).next(), None);       // truncated text
        assert_eq!(Reader::new(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).next(), None);   // absurd count
    }

    #[test]
    fn skips_nested() {
        let mut buf = [0u8; 32];
        let mut w = Writer::new(&mut buf);
        w.array(2).map(1).uint(1).array(1).boolean(true).uint(7);
        let out = w.finish().unwrap();
        let mut r = Reader::new(out);
        assert!(r.skip());
        assert!(r.rest().is_empty());
    }

    #[test]
    fn matches_the_c_encoder_bytes() {
        let mut buf = [0u8; 16];
        let mut w = Writer::new(&mut buf);
        w.uint(24).uint(256).uint(65536).int(-1);
        assert_eq!(w.finish().unwrap(), &[0x18, 24, 0x19, 1, 0, 0x1a, 0, 1, 0, 0, 0x20]);
    }
}

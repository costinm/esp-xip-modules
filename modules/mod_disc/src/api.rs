//! Module-owned experimental route-discovery wire contract (disc/API.md).
//! These are unauthenticated candidates, not authority to install a forwarding.
use xip_abi::cbor::{Item, Reader, Writer};

// Negative notification methods use the runtime's existing no-auto-activation path.
pub const INGRESS: i32 = -120;
pub const MAX_HOPS: u8 = 8;
pub const RECORD_MAX: usize = 80;
pub const REQUEST: u8 = 1;
pub const REPLY: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub kind: u8,
    pub origin: [u8; 6],
    pub session: u64,
    pub sequence: u32,
    pub target: [u8; 6],
    pub hops: u8,
    pub limit: u8,
}
pub fn unicast(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac[0] & 1 == 0
}
impl Record {
    pub fn valid(&self) -> bool {
        matches!(self.kind, REQUEST | REPLY)
            && unicast(self.origin)
            && unicast(self.target)
            && self.origin != self.target
            && self.session != 0
            && self.sequence != 0
            && (1..=MAX_HOPS).contains(&self.limit)
            && self.hops < self.limit
    }
    pub fn encode<'a>(&self, out: &'a mut [u8]) -> Option<&'a [u8]> {
        if !self.valid() {
            return None;
        }
        let mut w = Writer::new(out);
        // A distinct magic/version prevents treating ordinary announcements as work.
        w.map(9)
            .uint(0)
            .uint(0x485701)
            .uint(1)
            .uint(self.kind as u64)
            .uint(2)
            .bytes(&self.origin)
            .uint(3)
            .uint(self.session)
            .uint(4)
            .uint(self.sequence as u64)
            .uint(5)
            .bytes(&self.target)
            .uint(6)
            .uint(self.hops as u64)
            .uint(7)
            .uint(self.limit as u64)
            .uint(8)
            .uint(0); // reserved flags, must be zero
        w.finish()
    }
    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        if r.next()? != Item::Map(9) {
            return None;
        }
        let mut ints = [0u64; 9];
        let mut macs = [[0; 6]; 2];
        let mut seen = 0u16;
        for _ in 0..9 {
            let Item::Uint(k @ 0..=8) = r.next()? else {
                return None;
            };
            if seen & (1 << k) != 0 {
                return None;
            }
            seen |= 1 << k;
            if k == 2 || k == 5 {
                let Item::Bytes(b) = r.next()? else {
                    return None;
                };
                macs[usize::from(k == 5)] = b.try_into().ok()?;
            } else {
                let Item::Uint(v) = r.next()? else {
                    return None;
                };
                ints[k as usize] = v;
            }
        }
        if !r.rest().is_empty() || ints[0] != 0x485701 || ints[8] != 0 {
            return None;
        }
        let record = Self {
            kind: ints[1].try_into().ok()?,
            origin: macs[0],
            session: ints[3],
            sequence: ints[4].try_into().ok()?,
            target: macs[1],
            hops: ints[6].try_into().ok()?,
            limit: ints[7].try_into().ok()?,
        };
        record.valid().then_some(record)
    }
    pub fn same_request(&self, other: &Self) -> bool {
        (self.origin, self.session, self.sequence) == (other.origin, other.session, other.sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_rejects_trailing_duplicate_reserved_and_out_of_range_fields() {
        let p = Record {
            kind: REQUEST,
            origin: [2; 6],
            session: 7,
            sequence: 1,
            target: [4; 6],
            hops: 0,
            limit: 4,
        };
        let mut out = [0; RECORD_MAX];
        let n = p.encode(&mut out).unwrap().len();
        assert_eq!(Record::decode(&out[..n]), Some(p));
        assert!(Record::decode(&out[..n + 1]).is_none());
        out[n - 1] = 1;
        assert!(Record::decode(&out[..n]).is_none());
        p.encode(&mut out).unwrap();
        out[n - 2] = 7;
        assert!(Record::decode(&out[..n]).is_none());
        assert!(Record { hops: 4, ..p }.encode(&mut out).is_none());
        assert!(Record {
            target: [0xff; 6],
            ..p
        }
        .encode(&mut out)
        .is_none());
    }
}

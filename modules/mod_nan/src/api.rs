//! Module-owned NAN datagram contract, independent of the running implementation.
//! Depend on `esp-mod-nan` with feature `api-only` to compile just this API.
//! This file may also be included directly by consumers using the same toolchain.
use xip_abi::cbor::{Item, Reader, Writer};
pub const COMPONENT: i32 = 46;
pub const SOLICIT: i32 = 17;
pub const SUBSCRIBE: i32 = 18;
pub const STATUS: i32 = 19;
pub const PAYLOAD_MAX: usize = 231;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub mac: [u8; 6],
    pub instance: u8,
}
pub fn unicast(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac[0] & 1 == 0
}
pub struct Solicitation<'a> {
    pub payload: &'a [u8],
    pub peer: Option<Peer>,
    pub delay_ms: u8,
}
impl<'a> Solicitation<'a> {
    pub fn encode<'b>(&self, out: &'b mut [u8]) -> Option<&'b [u8]> {
        if self.payload.is_empty() || self.payload.len() > PAYLOAD_MAX || self.delay_ms > 64 {
            return None;
        }
        let mut w = Writer::new(out);
        w.map((if self.peer.is_some() { 3 } else { 1 }) + usize::from(self.delay_ms != 0))
            .uint(1)
            .bytes(self.payload);
        if let Some(p) = self.peer {
            if !unicast(p.mac) {
                return None;
            }
            w.uint(2).bytes(&p.mac).uint(3).uint(p.instance as u64);
        }
        if self.delay_ms != 0 {
            w.uint(4).uint(self.delay_ms as u64);
        }
        w.finish()
    }
    pub fn decode(data: &'a [u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        let Item::Map(n @ 1..=4) = r.next()? else {
            return None;
        };
        if r.next()? != Item::Uint(1) {
            return None;
        }
        let Item::Bytes(payload) = r.next()? else {
            return None;
        };
        if payload.is_empty() || payload.len() > PAYLOAD_MAX {
            return None;
        }
        let peer = if n >= 3 {
            if r.next()? != Item::Uint(2) {
                return None;
            }
            let Item::Bytes(mac) = r.next()? else {
                return None;
            };
            let mac = mac.try_into().ok()?;
            if !unicast(mac) {
                return None;
            }
            if r.next()? != Item::Uint(3) {
                return None;
            }
            let Item::Uint(instance) = r.next()? else {
                return None;
            };
            Some(Peer {
                mac,
                instance: instance.try_into().ok()?,
            })
        } else {
            None
        };
        let delay_ms = if n % 2 == 0 {
            if r.next()? != Item::Uint(4) {
                return None;
            }
            let Item::Uint(delay @ 0..=64) = r.next()? else {
                return None;
            };
            delay as u8
        } else {
            0
        };
        r.rest().is_empty().then_some(Self {
            payload,
            peer,
            delay_ms,
        })
    }
}
#[derive(Clone, Copy)]
pub struct Received<'a> {
    pub peer: Peer,
    pub now: bool,
    pub channel: u8,
    pub rssi: i8,
    pub received_us: u64,
    pub payload: &'a [u8],
}
impl<'a> Received<'a> {
    pub fn encode<'b>(&self, out: &'b mut [u8]) -> Option<&'b [u8]> {
        let mut w = Writer::new(out);
        w.map(7)
            .uint(1)
            .bytes(&self.peer.mac)
            .uint(2)
            .uint(self.peer.instance as u64)
            .uint(3)
            .boolean(self.now)
            .uint(4)
            .uint(self.channel as u64)
            .uint(5)
            .int(self.rssi as i64)
            .uint(6)
            .uint(self.received_us)
            .uint(7)
            .bytes(self.payload);
        w.finish()
    }
    pub fn decode(data: &'a [u8]) -> Option<Self> {
        let mut r = Reader::new(data);
        if r.next()? != Item::Map(7) {
            return None;
        }
        if r.next()? != Item::Uint(1) {
            return None;
        }
        let Item::Bytes(mac) = r.next()? else {
            return None;
        };
        let mac = mac.try_into().ok()?;
        if r.next()? != Item::Uint(2) {
            return None;
        }
        let Item::Uint(instance) = r.next()? else {
            return None;
        };
        if r.next()? != Item::Uint(3) {
            return None;
        }
        let Item::Bool(now) = r.next()? else {
            return None;
        };
        if r.next()? != Item::Uint(4) {
            return None;
        }
        let Item::Uint(channel) = r.next()? else {
            return None;
        };
        if r.next()? != Item::Uint(5) {
            return None;
        }
        let rssi = r.next()?.as_int()?.try_into().ok()?;
        if r.next()? != Item::Uint(6) {
            return None;
        }
        let Item::Uint(received_us) = r.next()? else {
            return None;
        };
        if r.next()? != Item::Uint(7) {
            return None;
        }
        let Item::Bytes(payload) = r.next()? else {
            return None;
        };
        r.rest().is_empty().then_some(Self {
            peer: Peer {
                mac,
                instance: instance.try_into().ok()?,
            },
            now,
            channel: channel.try_into().ok()?,
            rssi,
            received_us,
            payload,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_preserves_signed_rssi_and_rejects_trailing_data() {
        let input = Received {
            peer: Peer {
                mac: [2; 6],
                instance: 7,
            },
            now: true,
            channel: 6,
            rssi: -91,
            received_us: 42,
            payload: b"opaque",
        };
        let mut b = [0; 128];
        let n = input.encode(&mut b).unwrap().len();
        let decoded = Received::decode(&b[..n]).unwrap();
        assert_eq!(decoded.rssi, -91);
        assert_eq!(decoded.payload, b"opaque");
        assert!(Received::decode(&b[..n + 1]).is_none());
        let s = Solicitation {
            payload: b"opaque",
            peer: Some(input.peer),
            delay_ms: 0,
        };
        let n = s.encode(&mut b).unwrap().len();
        assert_eq!(Solicitation::decode(&b[..n]).unwrap().peer, s.peer);
        assert!(Solicitation::decode(&b[..n + 1]).is_none());
    }
}

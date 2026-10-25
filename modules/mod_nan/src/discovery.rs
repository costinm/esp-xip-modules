//! Bounded request deduplication and asynchronous radio return routes.
use xip_abi::*;

/// Radio transaction facts, separate from application request IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReturnPath {
    pub peer: [u8; 6],
    pub instance: u8,
    pub now: bool,
}
#[derive(Clone, Copy)]
pub struct Pending {
    pub token: u64,
    pub path: ReturnPath,
    pub until: u64,
}
#[derive(Clone, Copy)]
struct Recent {
    path: ReturnPath,
    fingerprint: u32,
    len: usize,
    at: u64,
}
pub struct Discovery {
    pub pending: [Option<Pending>; 4],
    next_token: u64,
    pub sequence: u16,
    recent: [Option<Recent>; 4],
}
impl Discovery {
    pub fn new(now: u64) -> Self {
        Self {
            pending: [None; 4],
            next_token: now << 8,
            sequence: 1,
            recent: [None; 4],
        }
    }
    /// Suppress duplicate callback/Subscribe work within a paired DW, not
    /// application retries in later windows. This is module protocol state.
    pub fn admit_request(&mut self, path: ReturnPath, packet: &[u8], now: u64) -> bool {
        let fingerprint = dmesh_rawnan::fnv1a32(packet);
        if self.recent.iter().flatten().any(|r| {
            r.path == path
                && r.fingerprint == fingerprint
                && r.len == packet.len()
                && now.saturating_sub(r.at) < 2 * crate::protocol::WINDOW_US
        }) {
            return false;
        }
        let at = self
            .recent
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.recent
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, r)| r.unwrap().at)
                    .unwrap()
                    .0
            });
        self.recent[at] = Some(Recent {
            path,
            fingerprint,
            len: packet.len(),
            at: now,
        });
        true
    }
    pub fn reserve(&mut self, path: ReturnPath, now: u64) -> Result<u64, i32> {
        let slot = self
            .pending
            .iter_mut()
            .find(|p| p.is_none_or(|p| p.until <= now))
            .ok_or(ERR_BUSY)?;
        self.next_token = self.next_token.wrapping_add(1).max(1);
        *slot = Some(Pending {
            token: self.next_token,
            path,
            until: now + 2 * crate::protocol::WAKE_PERIOD_US,
        });
        Ok(self.next_token)
    }
    /// A peer waits for the answer to `token`.
    pub fn pending(&self, token: u64) -> bool {
        self.pending.iter().any(|p| p.is_some_and(|p| p.token == token))
    }
    pub fn take(&mut self, token: u64, now: u64) -> Option<ReturnPath> {
        let slot = self
            .pending
            .iter_mut()
            .find(|p| p.is_some_and(|p| p.token == token))?;
        slot.take().filter(|p| p.until > now).map(|p| p.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_delivery_is_suppressed_only_inside_the_same_paired_window() {
        let mut p = Discovery::new(1);
        let path = ReturnPath {
            peer: [1; 6],
            instance: 7,
            now: false,
        };
        assert!(p.admit_request(path, b"request", 100));
        assert!(!p.admit_request(path, b"request", 200));
        assert!(p.admit_request(path, b"other", 200));
        assert!(p.admit_request(ReturnPath { now: true, ..path }, b"request", 200));
        assert!(p.admit_request(path, b"request", 100 + crate::protocol::WAKE_PERIOD_US));
    }
    #[test]
    fn replies_expire_and_old_activation_tokens_do_not_match_replacement() {
        let path = ReturnPath {
            peer: [1; 6],
            instance: 9,
            now: false,
        };
        let mut old = Discovery::new(100);
        let token = old.reserve(path, 100).unwrap();
        let mut replacement = Discovery::new(200);
        replacement.reserve(path, 200).unwrap();
        assert!(replacement.take(token, 201).is_none());
        assert_eq!(old.take(token, 101), Some(path));
        assert!(old.take(token, 102).is_none());
        let token = old.reserve(path, 500).unwrap();
        assert!(old
            .take(token, 500 + 2 * crate::protocol::WAKE_PERIOD_US)
            .is_none());
    }
}

//! NAN state/wire behavior, independent of ESP-IDF and the firmware adapter.
use crate::runtime::diagnostics::Stats;
use alloc::{boxed::Box, vec::Vec};
use dmesh_rawnan::{self as wire, MacAddr, NanState, RxFrame};

pub const PERIOD_US: u64 = 512 * 1024;
pub const WINDOW_US: u64 = 16 * 1024;
// Absolute cluster-TSF slots: DW0 and DW8 in each 16-DW cycle.
pub const WAKE_DW: u64 = 8;
pub const WAKE_PERIOD_US: u64 = WAKE_DW * PERIOD_US;
pub const MISSED_WAKE_LIMIT: u64 = 3;
pub const MASTER_STALE_US: u64 = MISSED_WAKE_LIMIT * WAKE_PERIOD_US;
// NSY1 extension: a NOW follower announces timing without claiming to be its
// originator. Legacy masters/direct NAN bridges have this bit clear. A follower
// must never renew another device's clock-parent lease through an echo.
// TODO: define this alongside shared election/rank fields in the portable ABI.
const NOW_SYNC_FLAG_FOLLOWER: u8 = 0x04;
pub const LEAD_US: u64 = 8_000;
pub const ACQUIRE_US: u64 = 2_000_000;
const PUBLISH_DELAY_US: u64 = 4_000;
#[derive(Debug)]
pub struct Plan {
    pub capture: bool,
    pub public: bool,
    pub sync: bool,
    pub slot: u64,
    pub next_us: u64,
    pub start_us: u64,
    pub end_us: u64,
    pub acquiring: bool,
}
pub const INFO_MAX: usize = 255;
pub struct Engine {
    pub nan: NanState,
    pub stats: Option<Box<Stats>>,
    pub local: [u8; 6],
    pub info: Vec<u8>,
    pub master: Option<[u8; 6]>,
    pub master_is_beacon: bool,
    master_sync_beacon: bool,
    master_seen_us: u64,
    anchor_master: [u8; 6],
    nan_aligned: bool,
    pub started_us: u64,
    pub wake_dw: u64,
    acquire_until: u64,
    acquiring: bool,
}
impl Engine {
    /// HOST_START creates a synchronization engine with an initial acquisition interval.
    pub fn new(local: [u8; 6], now: u64) -> Self {
        Self {
            nan: NanState::new(MASTER_STALE_US),
            stats: None,
            local,
            info: Vec::new(),
            master: None,
            master_is_beacon: false,
            master_sync_beacon: false,
            master_seen_us: 0,
            anchor_master: [0; 6],
            nan_aligned: false,
            started_us: now,
            wake_dw: WAKE_DW,
            acquire_until: now + ACQUIRE_US,
            acquiring: true,
        }
    }
    /// A supplier owns content/signing policy. A bounded copy into the active
    /// allocation is the only retained publication; clearing it frees its storage.
    /// Supplier methods replace the opaque publication used by future transmissions.
    pub fn set_publication(&mut self, record: &[u8]) -> Result<(), i32> {
        if record.len() > INFO_MAX { return Err(xip_abi::ERR_SIZE); }
        let mut info = Vec::new();
        info.try_reserve_exact(record.len()).map_err(|_| xip_abi::ERR_NOMEM)?;
        info.extend_from_slice(record);
        self.info = info;
        Ok(())
    }
    /// Diagnostic startup enables optional timing storage without taxing ordinary operation.
    pub fn enable_stats(&mut self, wait_us: u64) -> bool {
        let ptr =
            unsafe { alloc::alloc::alloc(core::alloc::Layout::new::<Stats>()) }.cast::<Stats>();
        if ptr.is_null() {
            return false;
        }
        unsafe {
            ptr.write(Stats::new(wait_us));
            self.stats = Some(Box::from_raw(ptr));
        }
        true
    }
    /// Protocol and driver paths update counters only when diagnostics exist.
    pub fn with_stats(&mut self, f: impl FnOnce(&mut Stats)) {
        if let Some(s) = self.stats.as_mut() {
            f(s);
        }
    }
    /// Native radio-state refresh keeps a local master aligned with its actual source MAC.
    pub fn local_source_changed(&mut self, source: [u8;6]) {
        if self.nan.is_now_master() { self.anchor_master = source; }
    }
    /// Radio ingress feeds validated NAN/NOW observations into clock selection.
    pub fn receive(&mut self, source: [u8; 6], data: &[u8], at: u64, rssi: i8) {
        if source == self.local {
            return;
        }
        if let Some(sync) = wire::parse_now_sync_body(data) {
            self.with_stats(|s| s.sync_rx += 1);
            self.expire_master(at);
            // Exactly one clock parent. Other peers remain discovery inputs,
            // but cannot retime this device or renew its selected master's lease.
            let same_parent = self.master == Some(source) && !self.master_is_beacon;
            let follower = sync.flags & NOW_SYNC_FLAG_FOLLOWER != 0;
            let direct_nan = sync.is_nan_aligned() && !follower;
            let originator =
                !follower && (direct_nan || (sync.is_master() && sync.anchor_master == source));
            let stronger = direct_nan && !self.nan_aligned;
            let eligible = !self.master_is_beacon
                && (self.master.is_none() || same_parent || stronger)
                && sync.anchor_master != self.local;
            // Temporary bootstrap: use the portable MAC ordering for NOW-only
            // roots. TODO: NAN-style master election/rank/anchor maintenance over
            // NOW actions, in the shared protocol. An action is a beacon-equivalent
            // timing/election message here; inability to TX Wi-Fi beacons is no
            // practical obstacle. Do not put election policy in the C bearer.
            if eligible && originator {
                // A selected bridge can lose NAN and start its own NOW clock.
                // Explicitly replace its old clock epoch, including priority;
                // the portable selector otherwise retains NAN priority.
                if same_parent
                    && (self.anchor_master != sync.anchor_master
                        || self.nan_aligned != sync.is_nan_aligned())
                {
                    self.nan = NanState::new(MASTER_STALE_US);
                }
                let before = self.nan.nan_sync_timing();
                self.nan.observe_now_sync(
                    MacAddr(self.local),
                    sync.cluster_id,
                    sync.tsf_us,
                    sync.interval_tu,
                    sync.is_nan_aligned(),
                    at,
                );
                if self.nan.nan_sync_timing() != before && !self.nan.is_now_master() {
                    self.master = Some(source);
                    self.master_seen_us = at;
                    self.anchor_master = sync.anchor_master;
                    self.nan_aligned = sync.is_nan_aligned();
                }
            }
            return;
        }
        if data.len() < 24 {
            self.with_stats(|s| s.malformed += 1);
            return;
        }
        if wire::is_nan_beacon(data) {
            if wire::beacon_tsf_us(data).is_none_or(|tsf| tsf == 0) {
                self.with_stats(|s| s.malformed += 1);
                return;
            }
            let mut selected = false;
            self.expire_master(at);
            // Acquisition is permissive: discovery beacons also carry NAN
            // timing. Prefer a sync-beacon source when one becomes available;
            // this is an explicit priority handover, not multiple clock parents.
            // Full NAN master-rank election remains a shared-protocol TODO.
            let sync_beacon = wire::beacon_interval_tu(data) == Some(512);
            if !self.master_is_beacon
                || self.master == Some(source)
                || (!self.master_sync_beacon && sync_beacon)
            {
                // Direct NAN is an explicit priority handover from NOW. Once
                // selected, only this beacon transmitter updates our clock.
                if self.master != Some(source) || !self.master_is_beacon {
                    self.nan = NanState::new(MASTER_STALE_US);
                    self.master_sync_beacon = false;
                }
                self.nan.observe(RxFrame {
                    bytes: data,
                    rssi_dbm: rssi,
                    timestamp_us: at,
                });
                if self.nan.nan_sync_timing().is_some() {
                    self.master = Some(source);
                    self.master_is_beacon = true;
                    self.master_sync_beacon |= sync_beacon;
                    self.master_seen_us = at;
                    self.anchor_master = source;
                    self.nan_aligned = true;
                    self.acquiring = false;
                    selected = true;
                }
            }
            self.with_stats(|s| {
                if selected {
                    s.attributes(source, wire::beacon_tsf_us(data).unwrap_or(0), data);
                }
                s.beacon(
                    source,
                    at,
                    wire::beacon_tsf_us(data).unwrap_or(0),
                    wire::beacon_interval_tu(data).unwrap_or(0) as u16,
                    selected,
                )
            });
        }
        let mut matched = false;
        for item in wire::service_descriptors(data) {
            if item.service_id == wire::DMESH_SERVICE_ID {
                matched = true;
            }
        }
        if matched {
            self.with_stats(|s| s.sdf_rx += 1);
        }
    }
    /// Acquisition is measured from capture becoming available, not radio setup.
    /// The capture owner starts bounded clock acquisition after the radio becomes ready.
    pub fn acquire(&mut self, now: u64) {
        self.acquiring = true;
        self.acquire_until = now + ACQUIRE_US;
    }
    fn expire_master(&mut self, now: u64) {
        if self.master.is_some() && now.saturating_sub(self.master_seen_us) >= MASTER_STALE_US {
            self.master = None;
            self.master_is_beacon = false;
            self.master_sync_beacon = false;
            self.anchor_master = [0; 6];
            self.nan_aligned = false;
            self.nan = NanState::new(MASTER_STALE_US);
            self.acquire(now);
        }
    }
    /// The NAN scheduler advances clock expiry and acquisition state.
    pub fn tick(&mut self, now: u64) {
        self.expire_master(now);
        if self.nan.tick(now) == wire::Action::Rediscover {
            self.acquire(now);
        }
        if self.acquiring && now < self.acquire_until {
            return;
        }
        self.acquiring = false;
        if self.nan.cluster().is_none() || self.nan.is_now_master() {
            self.nan.become_now_master(MacAddr(self.local), now, None);
            self.master = None;
            self.master_is_beacon = false;
            self.master_sync_beacon = false;
            self.anchor_master = self.local;
            self.nan_aligned = false;
        }
    }
    /// The capture/sleep scheduler obtains the next TSF-aligned discovery windows.
    pub fn plan(&self, now: u64) -> Plan {
        self.plan_with_cadence(now, self.wake_dw)
    }
    /// Additional bounded solicitations share the existing TSF scheduler.
    pub fn plan_with_cadence(&self, now: u64, cadence: u64) -> Plan {
        if self.acquiring {
            return Plan {
                capture: true,
                public: false,
                sync: false,
                slot: 0,
                next_us: self.acquire_until,
                start_us: 0,
                end_us: self.acquire_until,
                acquiring: true,
            };
        }
        let Some((at, tsf, _)) = self.nan.nan_sync_timing() else {
            return Plan {
                capture: true,
                public: false,
                sync: false,
                slot: 0,
                next_us: now + ACQUIRE_US,
                start_us: 0,
                end_us: now + ACQUIRE_US,
                acquiring: true,
            };
        };
        let wake_period = cadence * PERIOD_US;
        let clock = tsf.saturating_add(now.saturating_sub(at));
        let phase = clock % wake_period;
        let slot = clock / PERIOD_US;
        let lead = self.stats.as_deref().map_or(LEAD_US, |s| s.lead_us);
        let start_us = if phase < 2 * WINDOW_US {
            now.saturating_sub(phase)
        } else {
            now + wake_period - phase
        };
        let (capture, public, sync, delta) = if phase < PUBLISH_DELAY_US {
            (true, false, false, PUBLISH_DELAY_US - phase)
        } else if phase < WINDOW_US {
            (true, true, false, WINDOW_US - phase)
        } else if phase < 2 * WINDOW_US {
            (true, false, true, 2 * WINDOW_US - phase)
        } else if phase < wake_period - lead {
            (false, false, false, wake_period - lead - phase)
        } else {
            // Keep listening across DW0/DW8; never transmit in the lead.
            (
                true,
                false,
                false,
                wake_period - phase + PUBLISH_DELAY_US,
            )
        };
        Plan {
            capture,
            public,
            sync,
            slot,
            next_us: now + delta,
            start_us,
            end_us: start_us + 2 * WINDOW_US,
            acquiring: false,
        }
    }
    /// The sender checks whether this device should emit synchronization announcements.
    pub fn emits_sync(&self) -> bool {
        // Every awake participant announces the selected clock. This is not a
        // claim to be master; receive admission uses originator flags above.
        self.nan.cluster().is_some() && !self.acquiring
    }
    /// The sync sender encodes a NOW announcement using the current publication.
    pub fn now_body(&self, now: u64, out: &mut [u8]) -> Option<usize> {
        self.now_message(now, &self.info, out)
    }
    /// Control/data senders encode a NOW body with the current clock anchor.
    pub fn now_message(&self, now: u64, payload: &[u8], out: &mut [u8]) -> Option<usize> {
        if !self.emits_sync() {
            return None;
        }
        let frame = self.nan.now_sync_outgoing(now)?;
        let n = wire::NOW_SYNC_HEADER_LEN + payload.len();
        if out.len() < n {
            return None;
        }
        out[0] = wire::NOW_SYNC_PREFIX_BYTE;
        out[1..5].copy_from_slice(&wire::NOW_SYNC_MAGIC_VERSION.to_be_bytes());
        out[5..11].copy_from_slice(&frame.cluster_id);
        out[11..17].copy_from_slice(&self.anchor_master);
        out[17..25].copy_from_slice(&frame.tsf_us.to_le_bytes());
        out[25..29].copy_from_slice(&((self.wake_dw * 512) as u32).to_le_bytes());
        out[29] = frame.flags
            | if self.master.is_some() && !self.master_is_beacon {
                NOW_SYNC_FLAG_FOLLOWER
            } else {
                0
            };
        out[30..n].copy_from_slice(payload);
        Some(n)
    }
    /// The publication sender encodes a NAN service frame for the current window.
    pub fn publish(&self, out: &mut [u8]) -> Option<usize> {
        let cluster = self.nan.cluster()?.0;
        let n = 43 + self.info.len();
        if out.len() < n {
            return None;
        }
        out[..24].fill(0);
        out[0] = 0xd0;
        out[4..10].copy_from_slice(&wire::NAN_DISCOVERY_MAC);
        out[10..16].copy_from_slice(&self.local);
        // A real NAN cluster's A3 is preserved. NOW-only clusters have a
        // different namespace: derive a NAN discovery BSSID for their SDFs.
        let bssid = if cluster[..3] == wire::NAN_BSSID_OUI {
            cluster
        } else {
            [0x50, 0x6f, 0x9a, cluster[3], cluster[4], cluster[5]]
        };
        out[16..22].copy_from_slice(&bssid);
        out[24..30].copy_from_slice(&[4, 9, 0x50, 0x6f, 0x9a, 0x13]);
        out[30] = 3;
        out[31..33].copy_from_slice(&((10 + self.info.len()) as u16).to_le_bytes());
        out[33..39].copy_from_slice(&wire::DMESH_SERVICE_ID);
        out[39..43].copy_from_slice(&[1, 0, 0x10, self.info.len() as u8]);
        out[43..n].copy_from_slice(&self.info);
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_publication_bounds_reject_before_mutation_and_clear_reclaims_bytes() {
        let mut e = Engine::new([2;6],1);
        e.set_publication(&[7;INFO_MAX]).unwrap();
        assert_eq!(e.set_publication(&[8;INFO_MAX+1]),Err(xip_abi::ERR_SIZE));
        assert_eq!(e.info.len(),INFO_MAX);assert_eq!(e.info,[7;INFO_MAX]);
        e.set_publication(b"x").unwrap();assert_eq!(e.info[0],b'x');
        assert_eq!(e.info.len(),1);
        e.set_publication(&[]).unwrap();assert_eq!(e.info.len(),0);assert_eq!(e.info.capacity(),0);
        assert!(e.info.iter().all(|b|*b==0));
    }

    fn engine(mac: [u8; 6], now: u64) -> Engine {
        let mut e = Engine::new(mac, now);
        assert!(e.enable_stats(0));
        e
    }
    #[test]
    fn sleepy_now_uses_integer_dw_offsets_and_next_beacon_corrects_drift() {
        let local=[2,0,0,0,0,2]; let parent=[2,0,0,0,0,1];
        let mut e=engine(local,1); e.wake_dw=1;
        let at=1_000_000; let tsf=64*PERIOD_US+123_000;
        let mut beacon=[0;36]; beacon[0]=0x80;
        beacon[16..22].copy_from_slice(&[0x50,0x6f,0x9a,1,2,3]);
        beacon[24..32].copy_from_slice(&tsf.to_le_bytes());
        beacon[32..34].copy_from_slice(&512u16.to_le_bytes());
        e.receive(parent,&beacon,at,-40);
        let now=at+9*PERIOD_US+200_000;
        e.tick(now);
        let before=e.plan(now);
        assert_eq!(before.next_us,at+10*PERIOD_US-123_000-LEAD_US);
        let mut out=[0;512]; let n=e.now_body(now,&mut out).unwrap();
        let sync=wire::parse_now_sync_body(&out[..n]).unwrap();
        assert_eq!(sync.tsf_us,tsf+now-at);
        assert_eq!(sync.interval_tu,512);
        // A new accepted beacon corrects a 700 us oscillator error. This
        // shifts the absolute target; it never adds a period to wake completion.
        let next=now+PERIOD_US;
        beacon[24..32].copy_from_slice(&(tsf+next-at+700).to_le_bytes());
        e.receive(parent,&beacon,next,-40);
        assert_eq!(e.plan(next).next_us,before.next_us+PERIOD_US-700);
    }
    #[test]
    fn normal_engine_has_no_diagnostics_allocation() {
        let mut e = Engine::new([2, 0, 0, 0, 0, 1], 1);
        e.tick(ACQUIRE_US + 1);
        assert!(e.stats.is_none());
        assert!(e.nan.is_now_master());
        e.with_stats(|_| panic!("disabled diagnostics must not run"));
    }
    #[test]
    fn follows_existing_now_wire_without_using_firmware_state() {
        let local = [2, 0, 0, 0, 0, 2];
        let remote = [2, 0, 0, 0, 0, 1];
        let mut e = engine(local, 1);
        let body = wire::build_now_sync_body(
            remote,
            remote,
            1234567,
            512,
            wire::NOW_SYNC_FLAG_MASTER,
            b"peer",
        )
        .unwrap();
        e.receive(remote, &body, 10000, -50);
        assert_eq!(e.nan.cluster(), Some(MacAddr(remote)));
        let mut out = [0; 512];
        e.tick(ACQUIRE_US + 1);
        let n = e.now_body(ACQUIRE_US + 2, &mut out).unwrap();
        let forwarded = wire::parse_now_sync_body(&out[..n]).unwrap();
        assert_eq!(forwarded.anchor_master, remote);
        assert!(!forwarded.is_master());
        let sync = e.nan.now_sync_outgoing(11000).unwrap();
        assert_eq!(sync.tsf_us, 1235567);
        assert!(!e.nan.is_now_master());
    }
    #[test]
    fn follows_remote_clock_then_recovers_when_anchor_disappears() {
        let master_mac = [2, 0, 0, 0, 0, 1];
        let follower_mac = [2, 0, 0, 0, 0, 2];
        let mut master = engine(master_mac, 1);
        master.tick(ACQUIRE_US + 1);
        let mut follower = engine(follower_mac, 1);
        let mut packet = [0; 512];
        let sent_at = 3_000_000;
        let n = master.now_body(sent_at, &mut packet).unwrap();
        // Different local clock origins must produce the same DW phase.
        let received_at = 9_000_000;
        follower.receive(master_mac, &packet[..n], received_at, -45);
        follower.tick(received_at + 1_000);
        assert_eq!(follower.nan.cluster(), Some(MacAddr(master_mac)));
        assert!(!follower.nan.is_now_master());
        let remote_window = master.plan(sent_at + 1_000);
        let local_window = follower.plan(received_at + 1_000);
        assert_eq!(
            local_window.next_us - remote_window.next_us,
            received_at - sent_at
        );
        assert_eq!(local_window.capture, remote_window.capture);
        follower.tick(received_at + MASTER_STALE_US + 1);
        assert_eq!(follower.nan.cluster(), None);
        assert!(follower.plan(received_at + MASTER_STALE_US + 1).capture);
        follower.tick(received_at + MASTER_STALE_US + ACQUIRE_US + 1);
        assert_eq!(follower.nan.cluster(), Some(MacAddr(follower_mac)));
        assert!(follower.nan.is_now_master());
    }

    #[test]
    fn acquisition_prefers_nan_and_times_out_to_now() {
        let mut e = engine([2, 0, 0, 0, 0, 2], 1);
        e.acquire(100_000);
        e.tick(2_099_999);
        assert_eq!(e.nan.cluster(), None);
        assert!(e.plan(2_099_999).capture);
        e.tick(2_100_000);
        assert!(e.nan.is_now_master());
        let mut f = engine([2, 0, 0, 0, 0, 3], 1);
        let mut packet = [0; 512];
        let n = e.now_body(2_100_000, &mut packet).unwrap();
        f.acquire(2_000_000);
        f.receive(e.local, &packet[..n], 2_100_000, -40);
        assert_eq!(f.plan(2_100_000).next_us, 4_000_000);
        assert!(!f.plan(2_100_000).sync);
        let mut beacon = [0u8; 36];
        beacon[0] = 0x80;
        beacon[32..34].copy_from_slice(&512u16.to_le_bytes());
        beacon[16..22].copy_from_slice(&[0x50, 0x6f, 0x9a, 1, 2, 3]);
        beacon[24..32].copy_from_slice(&9_000_000u64.to_le_bytes());
        f.receive([2, 0, 0, 0, 0, 4], &beacon, 2_200_000, -40);
        assert_eq!(f.nan.cluster(), Some(MacAddr([0x50, 0x6f, 0x9a, 1, 2, 3])));
        assert_ne!(f.plan(2_200_000).next_us, 4_000_000);
    }
    #[test]
    fn relays_cannot_move_a_live_beacon_or_feed_back_a_masters_clock() {
        let local = [2, 0, 0, 0, 0, 1];
        let remote = [2, 0, 0, 0, 0, 2];
        let mut e = engine(local, 1);
        e.tick(ACQUIRE_US + 1);
        let anchor = e.nan.nan_sync_timing();
        let echo = wire::build_now_sync_body(local, local, 100, 512, 0, b"").unwrap();
        e.receive(remote, &echo, ACQUIRE_US + 2, -40);
        assert_eq!(e.nan.nan_sync_timing(), anchor);
        let mut beacon = [0u8; 36];
        beacon[0] = 0x80;
        beacon[32..34].copy_from_slice(&512u16.to_le_bytes());
        let cluster = [0x50, 0x6f, 0x9a, 1, 2, 3];
        beacon[16..22].copy_from_slice(&cluster);
        beacon[24..32].copy_from_slice(&9_000_000u64.to_le_bytes());
        e.receive(remote, &beacon, ACQUIRE_US + 3, -40);
        let anchor = e.nan.nan_sync_timing();
        let relay = wire::build_now_sync_body(
            cluster,
            cluster,
            100,
            512,
            wire::NOW_SYNC_FLAG_NAN_ALIGNED,
            b"",
        )
        .unwrap();
        e.receive(remote, &relay, ACQUIRE_US + 4, -40);
        assert_eq!(e.nan.nan_sync_timing(), anchor);
        e.tick(ACQUIRE_US + 3 + MASTER_STALE_US);
        assert!(e.plan(ACQUIRE_US + 3 + MASTER_STALE_US).capture);
        assert!(!e.emits_sync());
    }

    #[test]
    fn follows_one_master_and_other_peers_cannot_extend_its_lease() {
        let local = [2, 0, 0, 0, 0, 8];
        let parent = [2, 0, 0, 0, 0, 1];
        let other = [2, 0, 0, 0, 0, 2];
        let mut e = engine(local, 1);
        let body = wire::build_now_sync_body(
            parent,
            parent,
            100_000,
            512,
            wire::NOW_SYNC_FLAG_MASTER,
            b"parent",
        )
        .unwrap();
        e.receive(parent, &body, 10_000, -40);
        assert_eq!(e.master, Some(parent));
        e.tick(ACQUIRE_US + 1);
        assert!(e.emits_sync());
        let timing = e.nan.nan_sync_timing();
        let wrong = wire::build_now_sync_body(
            parent,
            parent,
            9_000_000,
            512,
            wire::NOW_SYNC_FLAG_MASTER,
            b"other",
        )
        .unwrap();
        e.receive(other, &wrong, 20_000, -40);
        assert_eq!(e.nan.nan_sync_timing(), timing);
        assert_eq!(e.master, Some(parent));
        assert_eq!(e.stats.as_ref().unwrap().sync_rx, 2); // heard, but not admitted as a clock source
        e.tick(10_000 + MASTER_STALE_US);
        assert_eq!(e.master, None);
        assert_eq!(e.nan.cluster(), None);
        let mut beacon = [0u8; 36];
        beacon[0] = 0x80;
        beacon[32..34].copy_from_slice(&512u16.to_le_bytes());
        beacon[16..22].copy_from_slice(&[0x50, 0x6f, 0x9a, 1, 2, 3]);
        beacon[24..32].copy_from_slice(&9_000_000u64.to_le_bytes());
        e.receive(parent, &beacon, MASTER_STALE_US + 20_000, -40);
        let timing = e.nan.nan_sync_timing();
        beacon[24..32].copy_from_slice(&99_000_000u64.to_le_bytes());
        e.receive(other, &beacon, MASTER_STALE_US + 20_100, -40);
        assert_eq!(e.master, Some(parent));
        assert!(e.master_is_beacon);
        assert!(e.emits_sync());
        assert_eq!(e.nan.nan_sync_timing(), timing);
    }

    #[test]
    fn permissive_acquisition_prefers_a_sync_source_without_mixing_parents() {
        let discovery = [2, 0, 0, 0, 0, 1];
        let sync = [2, 0, 0, 0, 0, 3];
        let mut e = engine([2, 0, 0, 0, 0, 2], 1);
        let mut b = [0u8; 36];
        b[0] = 0x80;
        b[16..22].copy_from_slice(&[0x50, 0x6f, 0x9a, 1, 2, 3]);
        b[24..32].copy_from_slice(&9_000_000u64.to_le_bytes());
        b[32..34].copy_from_slice(&176u16.to_le_bytes());
        e.receive(discovery, &b, 10_000, -40);
        assert_eq!(e.master, Some(discovery));
        assert!(e.master_is_beacon);
        assert!(!e.acquiring);
        b[32..34].copy_from_slice(&512u16.to_le_bytes());
        e.receive(sync, &b, 20_000, -40);
        let timing = e.nan.nan_sync_timing();
        assert_eq!(e.master, Some(sync));
        b[32..34].copy_from_slice(&176u16.to_le_bytes());
        b[24..32].copy_from_slice(&99_000_000u64.to_le_bytes());
        e.receive(discovery, &b, 30_000, -40);
        assert_eq!(e.master, Some(sync));
        assert_eq!(e.nan.nan_sync_timing(), timing);
        assert_eq!(e.stats.as_ref().unwrap().selected_beacons, 2);
    }

    #[test]
    fn paired_windows_keep_capture_on_across_boundary_and_do_not_tx_in_lead() {
        let mut e = engine([2, 0, 0, 0, 0, 1], 1);
        let base = 16 * PERIOD_US - 1; // fallback TSF is local + 1
        e.tick(base);
        for (phase, capture, public, sync) in [
            (0, true, false, false),
            (3_999, true, false, false),
            (4_000, true, true, false),
            (WINDOW_US - 1, true, true, false),
            (WINDOW_US, true, false, true),
            (2 * WINDOW_US - 1, true, false, true),
            (2 * WINDOW_US, false, false, false),
            (WAKE_PERIOD_US - LEAD_US - 1, false, false, false),
            (WAKE_PERIOD_US - LEAD_US, true, false, false),
            (WAKE_PERIOD_US - 1, true, false, false),
        ] {
            let p = e.plan(base + phase);
            assert_eq!(
                (p.capture, p.public, p.sync),
                (capture, public, sync),
                "phase {phase}"
            );
            assert!(p.next_us > base + phase);
        }
        let lead = e.plan(base + WAKE_PERIOD_US - LEAD_US);
        assert_eq!(lead.next_us, base + WAKE_PERIOD_US + PUBLISH_DELAY_US);
        let now = e.plan(base + WINDOW_US);
        assert_eq!(now.next_us, base + 2 * WINDOW_US);
    }

    #[test]
    fn absolute_tsf_selects_dw0_and_dw8_across_local_clock_origins() {
        for origin in [1_000, 100_000_000] {
            for dw in 0..32u64 {
                let mut e = engine([2, 0, 0, 0, 0, 9], origin);
                let tsf = dw * PERIOD_US + WINDOW_US;
                let body = wire::build_now_sync_body(
                    [2, 0, 0, 0, 0, 1],
                    [2, 0, 0, 0, 0, 1],
                    tsf,
                    4096,
                    wire::NOW_SYNC_FLAG_MASTER,
                    b"",
                )
                .unwrap();
                e.receive([2, 0, 0, 0, 0, 1], &body, origin + ACQUIRE_US, -40);
                e.tick(origin + ACQUIRE_US);
                let p = e.plan(origin + ACQUIRE_US);
                assert_eq!(p.slot, dw);
                assert_eq!(p.sync, dw % 8 == 0, "DW {dw}, origin {origin}");
                assert_eq!(p.capture, dw % 8 == 0);
                assert!(p.next_us > origin + ACQUIRE_US);
            }
        }
    }

    #[test]
    fn follower_announcements_preserve_clock_but_cannot_create_clock_parents() {
        let root = [2, 0, 0, 0, 0, 1];
        let child = [2, 0, 0, 0, 0, 2];
        let mut master = engine(root, 1);
        master.tick(ACQUIRE_US + 1);
        let mut follower = engine(child, 1);
        let mut packet = [0; 512];
        let n = master.now_body(3_000_000, &mut packet).unwrap();
        follower.receive(root, &packet[..n], 7_000_000, -40);
        follower.tick(7_000_000);
        let n = follower.now_body(7_010_000, &mut packet).unwrap();
        let f = wire::parse_now_sync_body(&packet[..n]).unwrap();
        assert_eq!(f.anchor_master, root);
        assert_eq!(f.interval_tu, 4096);
        assert_eq!(f.flags & NOW_SYNC_FLAG_FOLLOWER, NOW_SYNC_FLAG_FOLLOWER);
        let before = master.nan.nan_sync_timing();
        master.receive(child, &packet[..n], 3_010_000, -40);
        assert_eq!(master.nan.nan_sync_timing(), before);
        let mut third = engine([2, 0, 0, 0, 0, 3], 1);
        third.receive(child, &packet[..n], 7_020_000, -40);
        assert_eq!(third.master, None);
    }

    #[test]
    fn nan_bridge_follower_announces_without_echoing_clock_and_follows_root_loss() {
        let bridge_mac = [2, 0, 0, 0, 0, 1];
        let mut bridge = engine(bridge_mac, 1);
        let mut beacon = [0u8; 36];
        beacon[0] = 0x80;
        beacon[16..22].copy_from_slice(&[0x50, 0x6f, 0x9a, 1, 2, 3]);
        beacon[24..32].copy_from_slice(&9_000_000u64.to_le_bytes());
        beacon[32..34].copy_from_slice(&512u16.to_le_bytes());
        bridge.receive([2, 0, 0, 0, 0, 8], &beacon, 2_000_001, -40);
        let mut follower = engine([2, 0, 0, 0, 0, 2], 1);
        let mut out = [0; 512];
        let n = bridge.now_body(2_001_001, &mut out).unwrap();
        follower.receive(bridge_mac, &out[..n], 8_000_001, -40);
        follower.tick(8_000_001);
        assert_eq!(follower.master, Some(bridge_mac));
        let n = follower.now_body(8_002_001, &mut out).unwrap();
        let echoed = wire::parse_now_sync_body(&out[..n]).unwrap();
        assert!(echoed.is_nan_aligned());
        assert_eq!(echoed.tsf_us, 9_003_000);
        let before = bridge.nan.nan_sync_timing();
        bridge.receive(follower.local, &out[..n], 2_003_001, -40);
        assert_eq!(bridge.nan.nan_sync_timing(), before);
        // Same live parent explicitly switches to its own NOW root.
        let replacement = wire::build_now_sync_body(
            bridge_mac,
            bridge_mac,
            100_000,
            4096,
            wire::NOW_SYNC_FLAG_MASTER,
            b"",
        )
        .unwrap();
        follower.receive(bridge_mac, &replacement, 8_010_001, -40);
        assert_eq!(follower.nan.cluster(), Some(MacAddr(bridge_mac)));
        assert!(!follower.nan_aligned);
        assert_eq!(follower.master, Some(bridge_mac));
    }

    #[test]
    fn idle_cadence_survives_two_missed_wakes_and_refreshes_same_parent() {
        let parent = [2, 0, 0, 0, 0, 1];
        let mut e = engine([2, 0, 0, 0, 0, 2], 1);
        let body = wire::build_now_sync_body(
            parent,
            parent,
            100_000,
            4096,
            wire::NOW_SYNC_FLAG_MASTER,
            b"",
        )
        .unwrap();
        e.receive(parent, &body, 10_000, -40);
        e.tick(10_000 + 2 * WAKE_PERIOD_US);
        assert_eq!(e.master, Some(parent));
        e.receive(parent, &body, 10_000 + 2 * WAKE_PERIOD_US + 1, -40);
        e.tick(10_000 + MASTER_STALE_US);
        assert_eq!(e.master, Some(parent));
        e.tick(10_000 + 2 * WAKE_PERIOD_US + 1 + MASTER_STALE_US);
        assert_eq!(e.master, None);
    }

    #[test]
    fn pool_encoders_match_portable_wire_and_discover_payloads() {
        let local = [2, 0, 0, 0, 0, 1];
        let mut e = engine(local, 1);
        e.tick(ACQUIRE_US + 1);
        e.set_publication(b"abc").unwrap();
        let mut out = [0; 512];
        let n = e.now_body(ACQUIRE_US + 2, &mut out).unwrap();
        let f = e.nan.now_sync_outgoing(ACQUIRE_US + 2).unwrap();
        assert_eq!(
            &out[..n],
            wire::build_now_sync_body(
                f.cluster_id,
                f.anchor_master,
                f.tsf_us,
                (WAKE_DW * 512) as u32,
                f.flags,
                b"abc"
            )
            .unwrap()
        );
        let n = e.publish(&mut out).unwrap();
        let expected = wire::build_nan_publish_sdf(
            wire::NAN_DISCOVERY_MAC,
            local,
            [0x50, 0x6f, 0x9a, 0, 0, 1],
            wire::DMESH_SERVICE_ID,
            1,
            b"abc",
        );
        assert_eq!(&out[..n], expected);
        let mut peer = engine([2, 0, 0, 0, 0, 2], 1);
        peer.receive(local, &out[..n], 20, -42);
        assert_eq!(peer.stats.as_ref().unwrap().sdf_rx, 1);
    }
}

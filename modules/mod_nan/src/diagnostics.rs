//! Optional, activation-owned diagnostics. No counters/ring exist when disabled.
use crate::runtime::protocol::Plan;
use xip_abi::cbor::Writer;
const SAMPLES: usize = 8;
#[derive(Clone, Copy, Default)]
pub struct Capture {
    called: u64,
    enabled: u64,
    expected: u64,
    nominal_end: u64,
    closed: u64,
    first: u64,
    selected: u64,
    tsf: u64,
    interval: u16,
    source: [u8; 6],
    acquiring: bool,
    extended: bool,
    until: u64,
    extension_done: bool,
}
#[derive(Default)]
pub struct Stats {
    pub beacons: u32,
    pub selected_beacons: u32,
    pub max_beacon_gap_us: u64,
    last_selected: u64,
    pub sync_rx: u32,
    pub sdf_rx: u32,
    pub sync_tx: u32,
    pub sdf_tx: u32,
    pub malformed: u32,
    pub tx_errors: u32,
    pub last_error: i32,
    pub wait_us: u64,
    pub lead_us: u64,
    pub last_response_id: Option<u64>,
    pub last_response_peer: [u8; 6],
    pub last_response_now: bool,
    capability: [u8; 9],
    capability_len: u8,
    availability: [u8; 32],
    availability_len: u8,
    attributes_truncated: bool,
    attribute_source: [u8; 6],
    attribute_tsf: u64,
    opens: u32,
    missed: u32,
    recovered: u32,
    timed_out: u32,
    samples: [Capture; SAMPLES],
    count: usize,
    next: usize,
}
impl Stats {
    /// Diagnostic startup allocates optional timing observations only when requested.
    pub fn new(wait_us: u64) -> Self {
        Self {
            wait_us,
            lead_us: crate::runtime::protocol::LEAD_US,
            ..Default::default()
        }
    }
    /// The capture scheduler records the actual receive-window opening delay.
    pub fn open(&mut self, called: u64, enabled: u64, plan: &Plan) {
        self.samples[self.next] = Capture {
            called,
            enabled,
            expected: plan.start_us,
            nominal_end: plan.end_us,
            acquiring: plan.acquiring,
            ..Default::default()
        };
        self.next = (self.next + 1) % SAMPLES;
        self.count = (self.count + 1).min(SAMPLES);
        self.opens += 1;
    }
    fn active(&mut self) -> Option<&mut Capture> {
        if self.count == 0 {
            return None;
        }
        let c = &mut self.samples[(self.next + SAMPLES - 1) % SAMPLES];
        (c.closed == 0).then_some(c)
    }
    /// The capture scheduler records completion of an observed receive window.
    pub fn close(&mut self, now: u64) {
        let missed = if let Some(c) = self.active() {
            c.closed = now;
            !c.acquiring && c.selected == 0 && !c.extended
        } else {
            false
        };
        self.missed += u32::from(missed);
    }
    /// Radio ingress records selected beacon timing for diagnostic queries.
    pub fn beacon(&mut self, source: [u8; 6], at: u64, tsf: u64, interval: u16, selected: bool) {
        self.beacons += 1;
        if selected {
            self.selected_beacons += 1;
            if self.last_selected != 0 {
                self.max_beacon_gap_us = self
                    .max_beacon_gap_us
                    .max(at.saturating_sub(self.last_selected));
            }
            self.last_selected = at;
        }
        if let Some(c) = self.active() {
            if at < c.called {
                return;
            }
            if c.first == 0 {
                c.first = at;
                c.tsf = tsf;
                c.interval = interval;
                c.source = source;
            }
            if selected && c.selected == 0 {
                c.selected = at;
            }
        }
    }
    /// Called on each scheduler/receive turn. A miss extends an existing
    /// capture once; it never toggles the radio while waiting for a late beacon.
    /// The NAN tick asks whether diagnostics require extending the receive window.
    pub fn hold_until(&mut self, now: u64, would_close: bool) -> Option<u64> {
        let wait = self.wait_us;
        let mut miss = false;
        let mut recovered = false;
        let mut timed_out = false;
        let mut hold = None;
        if let Some(c) = self.active() {
            if c.extended && !c.extension_done {
                if c.selected != 0 {
                    recovered = true;
                    c.extension_done = true;
                } else if now >= c.until {
                    timed_out = true;
                    c.extension_done = true;
                } else {
                    hold = Some(c.until);
                }
            } else if would_close && !c.acquiring && c.selected == 0 && !c.extended && wait != 0 {
                c.extended = true;
                c.until = now + wait;
                miss = true;
                hold = Some(c.until);
            }
        }
        self.missed += u32::from(miss);
        self.recovered += u32::from(recovered);
        self.timed_out += u32::from(timed_out);
        hold
    }
    /// Retain advertised capability/availability from the selected beacon.
    /// Missing attributes remain empty; never infer DW0/DW8 from their absence.
    /// Bound malformed IE/TLV walks and expose truncation of large attributes.
    /// Beacon ingress retains bounded capability attributes for troubleshooting.
    pub fn attributes(&mut self, source: [u8; 6], tsf: u64, frame: &[u8]) {
        self.capability_len = 0;
        self.availability_len = 0;
        self.attributes_truncated = false;
        self.attribute_source = source;
        self.attribute_tsf = tsf;
        let Some(mut ies) = frame.get(36..) else {
            return;
        };
        while ies.len() >= 2 {
            let n = ies[1] as usize;
            let Some(body) = ies.get(2..2 + n) else { break };
            if ies[0] == 221 && body.starts_with(&[0x50, 0x6f, 0x9a, 0x13]) {
                let mut attrs = &body[4..];
                while attrs.len() >= 3 {
                    let len = u16::from_le_bytes([attrs[1], attrs[2]]) as usize;
                    let Some(value) = attrs.get(3..3 + len) else {
                        break;
                    };
                    let dest: &mut [u8] = match attrs[0] {
                        0x0f => &mut self.capability,
                        0x12 => &mut self.availability,
                        _ => {
                            attrs = &attrs[3 + len..];
                            continue;
                        }
                    };
                    let copied = value.len().min(dest.len());
                    dest[..copied].copy_from_slice(&value[..copied]);
                    self.attributes_truncated |= copied != value.len();
                    if attrs[0] == 0x0f {
                        self.capability_len = copied as u8;
                    } else {
                        self.availability_len = copied as u8;
                    }
                    attrs = &attrs[3 + len..];
                }
            }
            ies = &ies[2 + n..];
        }
    }
    /// The tracking request handler serializes optional diagnostics for the caller.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let mut w = Writer::new(out);
        w.map(17)
            .uint(1)
            .boolean(true)
            .uint(2)
            .uint(self.opens as u64)
            .uint(3)
            .uint(self.missed as u64)
            .uint(4)
            .uint(self.recovered as u64)
            .uint(5)
            .uint(self.timed_out as u64)
            .uint(6)
            .uint(self.wait_us / 1000)
            .uint(9)
            .uint(self.lead_us / 1000)
            .uint(8)
            .uint(core::mem::size_of::<Self>() as u64)
            .uint(10)
            .bytes(&self.capability[..self.capability_len as usize])
            .uint(11)
            .bytes(&self.availability[..self.availability_len as usize])
            .uint(12)
            .boolean(self.attributes_truncated)
            .uint(13)
            .bytes(&self.attribute_source)
            .uint(14)
            .uint(self.attribute_tsf)
            .uint(16)
            .bytes(&self.last_response_peer)
            .uint(17)
            .boolean(self.last_response_now)
            .uint(15);
        if let Some(id) = self.last_response_id {
            w.uint(id);
        } else {
            w.null();
        }
        w.uint(7).array(self.count);
        for n in 0..self.count {
            let c = &self.samples[(self.next + SAMPLES - self.count + n) % SAMPLES];
            w.map(14)
                .uint(1)
                .uint(c.called)
                .uint(2)
                .uint(c.enabled)
                .uint(3)
                .uint(c.expected)
                .uint(4)
                .uint(c.nominal_end)
                .uint(5)
                .uint(c.closed)
                .uint(6);
            delta(&mut w, c.first, c.enabled);
            w.uint(7);
            delta(&mut w, c.selected, c.enabled);
            w.uint(8);
            delta(&mut w, c.first, c.expected);
            w.uint(9);
            delta(&mut w, c.selected, c.expected);
            w.uint(10)
                .uint(c.interval as u64)
                .uint(11)
                .bytes(&c.source)
                .uint(12)
                .boolean(c.extended)
                .uint(13)
                .boolean(c.acquiring)
                .uint(14)
                .uint(c.tsf);
        }
        w.finish().map_or(0, |b| b.len())
    }
}
fn delta(w: &mut Writer, at: u64, origin: u64) {
    if at == 0 || origin == 0 {
        w.null();
    } else {
        w.int(at as i64 - origin as i64);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> Plan {
        Plan {
            capture: true,
            public: false,
            sync: false,
            slot: 1,
            next_us: 104_000,
            start_us: 100_000,
            end_us: 132_768,
            acquiring: false,
        }
    }
    #[test]
    fn records_actual_enable_delay_and_recovers_late_beacon_without_reopening() {
        let mut s = Stats {
            wait_us: 512_000,
            ..Default::default()
        };
        s.open(92_000, 102_000, &plan()); // enabled 2 ms after predicted TBTT
        assert_eq!(s.hold_until(132_768, true), Some(644_768));
        s.beacon([1; 6], 624_500, 900_000, 512, true);
        assert_eq!(s.hold_until(624_600, false), None);
        s.close(630_000);
        assert_eq!((s.opens, s.missed, s.recovered, s.timed_out), (1, 1, 1, 0));
        assert_eq!(s.samples[0].selected - s.samples[0].enabled, 522_500);
        let mut out = [0; 1400];
        assert!(s.encode(&mut out) > 0);
    }
    #[test]
    fn full_ring_with_large_timestamps_fits_one_packet() {
        let mut s = Stats::default();
        for n in 0..12 {
            let t = 9_000_000_000_000_000 + n * 524_288;
            let mut p = plan();
            p.start_us = t + 8_000;
            p.end_us = t + 40_768;
            s.open(t, t + 300, &p);
            s.beacon([1; 6], t + 8_500, 9_000_000_000_000_000, 512, true);
            s.close(t + 40_900);
        }
        s.capability_len = 9;
        s.availability_len = 32;
        s.attribute_tsf = u64::MAX;
        let mut out = [0; 1400];
        assert!(s.encode(&mut out) > 0);
        assert_eq!(s.count, SAMPLES);
    }
    #[test]
    fn selected_beacon_attributes_are_bounded_and_missing_is_unknown() {
        let mut s = Stats::default();
        let mut frame = alloc::vec![0; 36];
        frame.extend_from_slice(&[
            221, 20, 0x50, 0x6f, 0x9a, 0x13, 0x0f, 9, 0, 0, 4, 0, 4, 0, 0x11, 0, 0, 0, 0x12, 1, 0,
            0x55,
        ]);
        s.attributes([1; 6], 100, &frame);
        assert_eq!(s.capability_len, 9);
        assert_eq!(s.capability[1], 4); // advertised 8-DW interval code
        assert_eq!(s.availability_len, 1);
        assert_eq!(s.availability[0], 0x55);
        s.attributes([2; 6], 200, &[0; 36]);
        assert_eq!((s.capability_len, s.availability_len), (0, 0));
        frame[37] = 255; // invalid IE length must not expose old attributes
        s.attributes([1; 6], 300, &frame);
        assert_eq!((s.capability_len, s.availability_len), (0, 0));
    }
    #[test]
    fn bounded_extension_times_out_once_and_no_wait_keeps_normal_windows() {
        let mut s = Stats::default();
        s.open(92_000, 93_000, &plan());
        assert_eq!(s.hold_until(132_768, true), None);
        s.wait_us = 512_000;
        assert_eq!(s.hold_until(132_768, true), Some(644_768));
        assert_eq!(s.hold_until(644_768, true), None);
        assert_eq!(s.hold_until(644_769, true), None);
        assert_eq!(s.timed_out, 1);
    }
}

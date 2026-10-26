//! One configured AP's policy, not a scan-result cache. Desired STA survives
//! effective NOW-only fallback. Scans and individual attempts are bounded.
use xip_abi::*;
pub struct Station {
    pub desired: WifiIntent,
    pub generation: u32,
    pub next: u64,
    pub attempting: bool,
    pub scanning: bool,
}
impl Station {
    /// NAN station policy creates a bounded association attempt from the current intended state.
    pub fn new(desired: WifiIntent, generation: u32, now: u64) -> Self {
        Self {desired,generation,next:now+10_000_000,attempting:true,scanning:false}
    }
    /// The station scan policy checks whether an observed AP can satisfy the saved intent.
    pub fn matches(&self, ap: &WifiAp) -> bool {
        let i=&self.desired;
        ap.rssi >= -80 && (i.bssid==[0;6] || i.bssid==ap.bssid)
            && (i.sta_ssid_len==0 || (i.sta_ssid_len==ap.ssid_len && i.sta_ssid[..i.sta_ssid_len as usize]==ap.ssid[..ap.ssid_len as usize]))
    }
    /// The NAN tick asks for the next association, fallback or scan transition.
    pub fn poll(&mut self, hw: &Hw, actual: &WifiState, now: u64, capture: bool) -> Result<Option<WifiIntent>,i32> {
        if actual.sta==2 {self.attempting=true;self.scanning=false;self.next=now+10_000_000;return Ok(None)}
        if now<self.next {return Ok(None)}
        if self.attempting {
            self.attempting=false;self.next=now+60_000_000;
            return Ok(Some(WifiIntent {radio:1,sta:0,ap:0,now:1,channel:6,..self.desired}));
        }
        if capture || actual.monitoring!=0 {self.next=now+50_000;return Ok(None)}
        let scan=hw.wifi().and_then(|w|w.scan_channels).ok_or(ERR_UNSUPPORTED)?;
        let mut aps=[WifiAp {ssid:[0;32],ssid_len:0,bssid:[0;6],rssi:0,channel:0,auth:0};16];
        let rc=unsafe {scan(hw.ctx(),aps.as_mut_ptr(),aps.len(),(1<<1)|(1<<6)|(1<<11),40)};
        self.scanning=rc==ERR_BUSY;
        if rc==ERR_BUSY {self.next=now+100_000;return Ok(None)}
        self.next=now+60_000_000;
        if rc<0 {return Err(rc)}
        if let Some(ap)=aps[..(rc as usize).min(aps.len())].iter().filter(|ap|self.matches(ap)).max_by_key(|ap|ap.rssi) {
            self.attempting=true;self.next=now+10_000_000;
            return Ok(Some(WifiIntent {radio:1,sta:1,channel:ap.channel,bssid:ap.bssid,..self.desired}));
        }
        Ok(None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ap_selection_requires_matching_identity_and_usable_signal() {
        let mut i=WifiIntent::default();i.sta=1;i.sta_ssid_len=4;i.sta_ssid[..4].copy_from_slice(b"home");
        let s=Station::new(i,1,0);
        let mut ap=WifiAp {ssid:[0;32],ssid_len:4,bssid:[2;6],rssi:-81,channel:6,auth:3};ap.ssid[..4].copy_from_slice(b"home");
        assert!(!s.matches(&ap));ap.rssi=-79;assert!(s.matches(&ap));ap.ssid[0]=b'x';assert!(!s.matches(&ap));
    }
}

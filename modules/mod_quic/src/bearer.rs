//! TX admission transfers a native read reference; BUSY keeps the prepared packet.
use crate::pool::{Buffer, Native, Pool};
use alloc::sync::Arc;
use quic_lite::*;
use spin::Mutex;
use xip_abi::*;
pub(crate) struct Bridge {
    pub link: u32,
    pub context: Option<BearerContext<Pool>>,
}
pub(crate) struct Bearer {
    pub native: Native,
    pub bridge: Arc<Mutex<Bridge>>,
    pub info: BearerInfo,
}
impl PacketEgress<Buffer> for Bearer {
    fn submit(
        &mut self,
        peer: PeerL2Address,
        submission: EgressSubmission<Buffer>,
    ) -> Result<(), PacketSubmitError<Buffer>> {
        let bridge = self.bridge.lock();
        let (packet,completion)=submission.into_parts();
        let (buffer,range)=packet.into_parts();
        let h=buffer.handle();
        let submission=EgressSubmission::from_parts(OwnedPacket::new(buffer,range.clone()).unwrap_or_else(|_|panic!("range")),completion);
        // The protocol retains its immutable reference. The driver owns one
        // additional reference only until it has finished reading the bytes.
        let Some(tx)=(unsafe {&*self.native.hw.as_ptr()}).pkt_tx else {
            submission.complete(PacketSendOutcome::Failed,0);return Ok(());
        };
        if self.native.hw.pkt_keep(h)<0 {
            submission.complete(PacketSendOutcome::Failed,0);return Ok(());
        }
        let rc=unsafe {tx(self.native.hw.ctx(),h,range.start as u32,(range.end-range.start) as u32,bridge.link,peer.value())};
        if rc==ERR_BUSY {
            self.native.hw.pkt_free(h);
            return Err(PacketSubmitError::WouldBlock(submission));
        }
        // Portable completion accounts for admission, not over-the-air delivery.
        submission.complete(if rc<0 {PacketSendOutcome::Failed} else {PacketSendOutcome::Sent},0);
        Ok(())
    }
}
impl PacketBearer<Pool> for Bearer {
    type AttachError = ();
    fn info(&self) -> BearerInfo {
        self.info
    }
    fn attach(&mut self, context: BearerContext<Pool>) -> Result<(), ()> {
        self.bridge.lock().context = Some(context);
        Ok(())
    }
}

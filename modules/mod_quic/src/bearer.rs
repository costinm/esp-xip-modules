//! A native driver owns accepted frames; completion polling restores the exact
//! backing slot. Refusals never replace the prepared packet or token.
use crate::pool::{Buffer, Native, Pool};
use alloc::{sync::Arc, vec::Vec};
use quic_lite::*;
use spin::Mutex;
use xip_abi::*;
pub(crate) struct Pending {
    pub ticket: u32,
    buffer: Buffer,
    range: core::ops::Range<usize>,
    completion: PacketCompletionToken<Buffer>,
}
pub(crate) struct Bridge {
    pub link: u32,
    pub context: Option<BearerContext<Pool>>,
    pub pending: Vec<Pending>,
}
impl Bridge {
    pub fn poll(&mut self, native: Native, done: QuicTxCompletion) -> Result<(), i32> {
        let Some(i) = self.pending.iter().position(|p| p.ticket == done.ticket) else {
            native.hw.pkt_free(done.packet);
            return Err(ERR_NOTFOUND);
        };
        let p = self.pending.swap_remove(i);
        let (buffer, range) = (p.buffer, p.range);
        if let Err(rc) = buffer.restored(done.packet) {
            native.hw.pkt_free(done.packet);
            return Err(rc);
        }
        let packet = OwnedPacket::new(buffer, range).map_err(|_| ERR_ARG)?;
        p.completion.complete(
            packet,
            if done.outcome == 0 {
                PacketSendOutcome::Sent
            } else {
                PacketSendOutcome::Failed
            },
            done.elapsed_us as u64,
        );
        if let Some(c) = &self.context {
            c.send_ready();
        }
        Ok(())
    }
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
        let mut bridge = self.bridge.lock();
        if bridge.pending.len() >= 4 {
            return Err(PacketSubmitError::WouldBlock(submission));
        }
        // Reserve only metadata before transferring any native ownership.
        if bridge.pending.try_reserve(1).is_err() {
            submission.complete(PacketSendOutcome::Failed, 0);
            return Ok(());
        }
        let (packet, completion) = submission.into_parts();
        let (buffer, range) = packet.into_parts();
        let h = buffer.handle();
        let mut ticket = 0;
        let rc = self
            .native
            .table
            .try_send
            .map_or(ERR_UNSUPPORTED, |f| unsafe {
                f(
                    self.native.hw.ctx(),
                    h,
                    range.start as u32,
                    (range.end - range.start) as u32,
                    bridge.link,
                    peer.value(),
                    &mut ticket,
                )
            });
        if rc < 0 {
            let packet =
                OwnedPacket::new(buffer, range).unwrap_or_else(|_| panic!("native range changed"));
            let submission = EgressSubmission::from_parts(packet, completion);
            if rc == ERR_BUSY {
                return Err(PacketSubmitError::WouldBlock(submission));
            }
            submission.complete(PacketSendOutcome::Failed, 0);
            return Ok(());
        }
        buffer.moved();
        // While physical TX owns it, only this opaque metadata is retained.
        // No bytes are read or cloned until poll_tx gives the exact slot back.
        bridge.pending.push(Pending {
            ticket,
            buffer,
            range,
            completion,
        });
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

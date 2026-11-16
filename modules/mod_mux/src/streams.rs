//! Bridge between `QuicMux` multiplexed streams and the native runtime `Stream` service (`xip_stream`).

use quic_mux::{
    AssociationEgress, AssociationId, MuxSendError, PacketPool, PoolPacket, QuicMux, StreamId,
    TxStreamData,
};
use xip_abi::{Hw, Stream, StreamPacket, ERR_ARG, ERR_BUSY, ERR_IO, ERR_NOTFOUND, ERR_UNSUPPORTED, PKT_HEADROOM};

use crate::pool::{XipPacket, XipPool};

pub(crate) struct StreamSession {
    pub assoc: AssociationId,
    pub stream_id: StreamId,
    pub handle: u32,
    pub needs_resume: bool,
    pub rx_fin: bool,
    pub tx_fin: bool,
    pub output: Option<XipPacket>,
    pub output_fin: bool,
    pub closed: bool,
}

impl StreamSession {
    pub const fn new(assoc: AssociationId, stream_id: StreamId) -> Self {
        Self {
            assoc,
            stream_id,
            handle: 0,
            needs_resume: false,
            rx_fin: false,
            tx_fin: false,
            output: None,
            output_fin: false,
            closed: false,
        }
    }

    pub fn close_native(&mut self, hw: Hw, table: Option<&Stream>) {
        if self.handle != 0 {
            if let Some(close) = table.and_then(|t| t.close) {
                unsafe {
                    close(hw.ctx(), self.handle);
                }
            }
            self.handle = 0;
        }
        self.output = None;
        self.needs_resume = false;
        self.closed = true;
    }

    /// Feed incoming stream bytes and/or `fin` to the native `xip_stream` consumer.
    pub fn feed(
        &mut self,
        hw: Hw,
        table: &Stream,
        bytes: &[u8],
        fin: bool,
    ) -> Result<usize, i32> {
        if self.rx_fin {
            return Err(ERR_ARG);
        }
        if self.handle == 0 {
            let rc = unsafe {
                table.open.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), core::ptr::null(), 0)
            };
            if rc <= 0 {
                return Err(if rc < 0 { rc } else { ERR_IO });
            }
            self.handle = rc as u32;
        }
        let n = if bytes.is_empty() {
            0
        } else {
            let rc = unsafe {
                table.feed.ok_or(ERR_UNSUPPORTED)?(
                    hw.ctx(),
                    self.handle,
                    bytes.as_ptr(),
                    bytes.len(),
                    0,
                )
            };
            if rc < 0 {
                return Err(rc);
            }
            let consumed = rc as usize;
            if consumed > bytes.len() {
                return Err(ERR_ARG);
            }
            consumed
        };
        self.needs_resume = n < bytes.len();
        if !self.needs_resume && fin {
            let rc = unsafe {
                table.feed.ok_or(ERR_UNSUPPORTED)?(
                    hw.ctx(),
                    self.handle,
                    core::ptr::null(),
                    0,
                    1,
                )
            };
            if rc < 0 {
                return Err(rc);
            }
            self.rx_fin = true;
        }
        Ok(n)
    }

    /// Poll `c_stream_take` and drain pending consumer output onto `QuicMux` zero-copy.
    pub fn pump_egress(
        &mut self,
        hw: Hw,
        table: &Stream,
        mux: &mut QuicMux<XipPool>,
        egress: &mut impl AssociationEgress<XipPacket>,
    ) -> Result<(), i32> {
        if self.closed || self.tx_fin {
            return Ok(());
        }
        loop {
            if self.output.is_none() && self.handle != 0 {
                if mux.pool().available() <= 4 {
                    return Ok(());
                }
                let mut sp = StreamPacket::default();
                let rc = unsafe {
                    table.take.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle, &mut sp)
                };
                if rc >= 0 && sp.packet != 0 {
                    let start = PKT_HEADROOM as usize;
                    let pkt = XipPacket::from_owned(hw, sp.packet, start..start + sp.len as usize)?;
                    self.output = Some(pkt);
                    self.output_fin = sp.fin != 0;
                } else if rc == ERR_NOTFOUND {
                    // Consumer closed its stream handle: send FIN to peer.
                    match mux.write_stream(
                        self.assoc,
                        self.stream_id,
                        TxStreamData::Bytes(&[]),
                        true,
                        egress,
                    ) {
                        Ok(_) => {
                            self.tx_fin = true;
                            self.close_native(hw, Some(table));
                            let _ = mux.close_stream(self.assoc, self.stream_id, egress);
                        }
                        Err(MuxSendError::Busy(_) | MuxSendError::FlowControl(_)) => {}
                        Err(_) => {
                            self.close_native(hw, Some(table));
                            let _ = mux.reset_stream(self.assoc, self.stream_id, 1, egress);
                        }
                    }
                    return Ok(());
                } else if rc == 0 || rc == ERR_BUSY {
                    return Ok(());
                } else {
                    self.close_native(hw, Some(table));
                    let _ = mux.reset_stream(self.assoc, self.stream_id, 1, egress);
                    return Err(rc);
                }
            }

            let Some(mut pkt) = self.output.take() else {
                return Ok(());
            };
            let cap = match mux.stream_send_capacity(self.assoc, self.stream_id, egress) {
                Ok(c) => c,
                Err(_) => {
                    self.close_native(hw, Some(table));
                    return Ok(());
                }
            };
            let rem = pkt.bytes().len();
            if rem == 0 {
                let fin = self.output_fin;
                match mux.write_stream(
                    self.assoc,
                    self.stream_id,
                    TxStreamData::Packet(pkt),
                    fin,
                    egress,
                ) {
                    Ok(_) => {
                        unsafe {
                            let _ = table.written.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle);
                        }
                        if fin {
                            self.tx_fin = true;
                            if self.rx_fin {
                                self.close_native(hw, Some(table));
                                let _ = mux.close_stream(self.assoc, self.stream_id, egress);
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    Err(
                        MuxSendError::Busy(ret)
                        | MuxSendError::FlowControl(ret)
                        | MuxSendError::PacketTooLarge { packet: ret, .. },
                    ) => {
                        self.output = ret;
                        return Ok(());
                    }
                    Err(_) => {
                        self.close_native(hw, Some(table));
                        return Err(ERR_IO);
                    }
                }
            }

            if !cap.can_send_now || cap.max_chunk_bytes == 0 {
                self.output = Some(pkt);
                return Ok(());
            }

            if rem <= cap.max_chunk_bytes {
                // Fast zero-copy path: transfer ownership of the consumer's exact `XipPacket`!
                let fin = self.output_fin;
                match mux.write_stream(
                    self.assoc,
                    self.stream_id,
                    TxStreamData::Packet(pkt),
                    fin,
                    egress,
                ) {
                    Ok(_) => {
                        unsafe {
                            let _ = table.written.ok_or(ERR_UNSUPPORTED)?(hw.ctx(), self.handle);
                        }
                        if fin {
                            self.tx_fin = true;
                            if self.rx_fin {
                                self.close_native(hw, Some(table));
                                let _ = mux.close_stream(self.assoc, self.stream_id, egress);
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    Err(MuxSendError::PacketTooLarge {
                        packet: Some(returned),
                        max_payload,
                    }) if max_payload > 0 && max_payload < rem => {
                        pkt = returned;
                        // Fall through to small-prefix + zero-copy-bulk split below using `max_payload`.
                        let prefix_len = (rem - max_payload).min(max_payload);
                        if !self.send_prefix_slice(&mut pkt, prefix_len, mux, egress)? {
                            self.output = Some(pkt);
                            return Ok(());
                        }
                        self.output = Some(pkt);
                        continue;
                    }
                    Err(
                        MuxSendError::Busy(ret)
                        | MuxSendError::FlowControl(ret)
                        | MuxSendError::PacketTooLarge { packet: ret, .. },
                    ) => {
                        self.output = ret;
                        return Ok(());
                    }
                    Err(_) => {
                        self.close_native(hw, Some(table));
                        return Err(ERR_IO);
                    }
                }
            } else {
                // `rem > cap.max_chunk_bytes`: send only the small excess prefix (`rem - max_chunk_bytes`,
                // typically <= 34 bytes for a 1,500-byte packet) via `TxStreamData::Bytes`, then loop to
                // send the remaining ~1,466-byte bulk in-place via `TxStreamData::Packet(pkt)` zero-copy!
                let max_c = cap.max_chunk_bytes;
                let prefix_len = if rem <= max_c.saturating_mul(2) {
                    rem - max_c
                } else {
                    max_c
                };
                if !self.send_prefix_slice(&mut pkt, prefix_len, mux, egress)? {
                    self.output = Some(pkt);
                    return Ok(());
                }
                self.output = Some(pkt);
            }
        }
    }

    fn send_prefix_slice(
        &self,
        pkt: &mut XipPacket,
        prefix_len: usize,
        mux: &mut QuicMux<XipPool>,
        egress: &mut impl AssociationEgress<XipPacket>,
    ) -> Result<bool, i32> {
        let r = pkt.range();
        let slice = &pkt.slot_slice()[r.start..r.start + prefix_len];
        match mux.write_stream(
            self.assoc,
            self.stream_id,
            TxStreamData::Bytes(slice),
            false,
            egress,
        ) {
            Ok(n) => {
                pkt.set_range(r.start + n..r.end).map_err(|_| ERR_ARG)?;
                Ok(true)
            }
            Err(
                MuxSendError::Busy(_)
                | MuxSendError::FlowControl(_)
                | MuxSendError::PacketTooLarge { .. },
            ) => Ok(false),
            Err(_) => Err(ERR_IO),
        }
    }
}

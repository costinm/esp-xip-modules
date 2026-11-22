//! `mod_quic2`: combined `quic-link` (`QuicLinkAssoc`) + `quic-mux` (`QuicMux`) multi-link
//! endpoint over UDP (`hw.quic` Short-Header classifier and datagram port `243`), PPP serial
//! (`--port 243`), and TCP sockets (`--stream 6`, TCP port `5231`).
//!
//! ### Notable implementation constraints
//! - **`#[inline(never)]` on slot/RX/tick methods:** `mod_quic2` runs on an 8 KiB FreeRTOS worker
//!   stack. Keeping large `QuicLinkAssoc`/`QuicMux`/`Initial` locals in separate non-overlapping
//!   frames prevents stack-frame accumulation and `.dmod` code bloat.
//! - **`#[inline(always)]` on `psa_aead_pkt_alg`:** defensive workaround for an LLVM 21.1.3
//!   RISC-V `MachineOutliner` bug that clobbered live register `t1` (`x6`) across an outlined
//!   helper (also disabled globally for `riscv32` in `tools/mkmod.py`; see
//!   `notes/ai/2026-11-20-riscv-machine-outliner-t1-clobber.md`).
//! - **64-byte alignment in `psa_aead_pkt_alg`:** ESP32-C6 AES-DMA requires aligned buffers; the
//!   helper shifts the packet in-place within `XIP_PKT_HEADROOM` and `quic-link` adjusts its
//!   `inner_frame_range`/`mux_range` by the resulting `delta`.

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod links;
mod pool;
mod streams;

use alloc::{boxed::Box, vec::Vec};
use links::{
    serial_link_profile_with_encrypt, tcp_link_profile_with_encrypt, udp_link_profile_with_encrypt,
    PeerRoute, Quic2Drivers, LINK_SERIAL, LINK_TCP_BASE, LINK_UDP, OWNER_ASSOC, QUIC2_SERIAL_PORT,
};
use pool::{XipPacket, XipPool};
use quic_link::{
    is_initial_packet, AssocConfig, AssocCryptoConfig, ConnectionId, CryptoError, CryptoProvider,
    CryptoRole, HandshakeMode, InitialHeader, LinkId, QuicLinkAssoc, ShortHeader,
    TransportParameters, AEAD_TAG_LEN, ASSOCIATION_RANDOM_LEN, FLAG_PROTECTED,
    PUBLIC_KEY_COMPRESSED_LEN, TRAFFIC_IV_LEN, TRAFFIC_KEY_LEN,
};
use quic_mux::{
    AssociationBrokenReason, AssociationId, MuxCallbacks, MuxLimits, PoolPacket, QuicMux, RxMeta,
    RxOutcome, RxStreamChunk, StreamCloseReason, StreamError, StreamId,
};
use streams::StreamSession;
use xip_abi::{
    cbor::{self, Item, Writer},
    pack_aead_pkt_spec, CallView, CryptoClient, Host, Hw, LinkInfo, Module, Packet, Quic, Stream,
    ALARM_EXACT, ALG_CCM, ALG_GCM, CALL_LINK, CALL_LINK_EVENT, CALL_STREAM, ERR_ARG, ERR_AUTH,
    ERR_BUSY, ERR_IO, ERR_NOMEM, ERR_SIZE, ERR_UNSUPPORTED, HOST_START, HOST_STOP, LINK_NATIVE,
    PKT_HEADROOM, QUIC_VERSION, RET_CONTINUE, SOCKET_EVENT_CLOSED, SOCKET_EVENT_EOF,
    SOCKET_EVENT_OPEN, SOCKET_EVENT_READABLE, SOCKET_EVENT_WRITABLE, SOCKET_EVENT_WRITE_DONE,
};

pub const COMPONENT: i32 = 55;
const RX: i32 = 100;
const TICK: i32 = 101;
pub const STATE_BYTES: usize = 8;

/// Intentional single-owner association (`MAX_ASSOCS = 1`) for minimal ESP32 footprint.
const MAX_ASSOCS: usize = 1;
const MAX_SESSIONS: usize = 16;
const ACTIVE_POLL_MS: u32 = 10;
const EPOCH_REINJECT_MIN_QUIET_US: u64 = 20_000;

#[inline]
fn slot_assoc_id(idx: usize) -> AssociationId {
    AssociationId((idx + 1) as u32)
}

fn esp32_transport_params(server_cid: ConnectionId) -> TransportParameters {
    TransportParameters {
        initial_scid: Some(server_cid),
        max_idle_timeout_ms: Some(30_000),
        max_udp_payload_size: Some(1472),
        initial_max_data: Some(64 * 1024),
        initial_max_stream_data_bidi_local: Some(32 * 1024),
        initial_max_stream_data_bidi_remote: Some(32 * 1024),
        initial_max_streams_bidi: Some(8),
        initial_max_streams_uni: Some(8),
        ..Default::default()
    }
}

fn handshake_assoc_config(
    assoc_id: AssociationId,
    server_cid: ConnectionId,
    client_scid: Option<ConnectionId>,
) -> AssocConfig {
    let default_peer_cid = client_scid.unwrap_or_else(|| ConnectionId::new(1).unwrap());
    AssocConfig {
        assoc_id,
        local_cid: Some(server_cid),
        default_peer_cid,
        handshake_mode: HandshakeMode::Server,
        local_params: esp32_transport_params(server_cid),
        ..default_assoc_config()
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn embedded_mux_limits() -> MuxLimits {
    MuxLimits {
        initial_max_data: 64 * 1024,
        initial_max_stream_data: 16 * 1024,
        initial_max_streams_bidi: 64,
        initial_max_streams_uni: 64,
        max_concurrent_streams: 8,
        max_queued_chunks_per_stream: 12,
        max_association_rx_packets: 16,
        max_device_rx_packets: 16,
        gap_timeout_us: 15_000_000,
    }
}

fn default_assoc_config() -> AssocConfig {
    let default_cid = ConnectionId::new(1).unwrap();
    AssocConfig {
        assoc_id: OWNER_ASSOC,
        local_cid: None,
        default_peer_cid: default_cid,
        max_retained_tx_packets: 6,
        ack_eliciting_threshold: 1,
        max_ack_delay_us: 20_000,
        ..AssocConfig::default()
    }
}

struct CallbackEnv<'a> {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    sessions: &'a mut Vec<StreamSession>,
    broken_assocs: &'a mut Vec<AssociationId>,
    errors: &'a mut u64,
    last_error: &'a mut i32,
}

impl MuxCallbacks<XipPacket> for CallbackEnv<'_> {
    fn on_stream_data(
        &mut self,
        assoc: AssociationId,
        stream_id: StreamId,
        _offset: u64,
        mut data: RxStreamChunk<'_, XipPacket>,
        fin: bool,
    ) -> Result<RxOutcome<XipPacket>, StreamError> {
        let Some(table) = self.stream_table else {
            *self.errors += 1;
            *self.last_error = ERR_UNSUPPORTED;
            return Err(StreamError { error_code: 1 });
        };
        let pos = match self
            .sessions
            .iter()
            .position(|s| !s.closed && s.assoc == assoc && s.stream_id == stream_id)
        {
            Some(i) => i,
            None => {
                if self.sessions.len() >= MAX_SESSIONS {
                    *self.errors += 1;
                    *self.last_error = ERR_BUSY;
                    return Err(StreamError { error_code: 2 });
                }
                if self.sessions.try_reserve(1).is_err() {
                    *self.errors += 1;
                    *self.last_error = ERR_NOMEM;
                    return Err(StreamError { error_code: 3 });
                }
                self.sessions.push(StreamSession::new(assoc, stream_id));
                self.sessions.len() - 1
            }
        };

        let len = data.len();
        let consumed = match self.sessions[pos].feed(self.hw, table, data.bytes(), fin) {
            Ok(n) => n,
            Err(rc) => {
                *self.errors += 1;
                *self.last_error = rc;
                self.sessions[pos].close_native(self.hw, Some(table));
                return Err(StreamError {
                    error_code: rc.unsigned_abs() as u64,
                });
            }
        };

        if consumed < len {
            if let Some(pkt) = data.take_packet() {
                return Ok(RxOutcome::Partial {
                    consumed,
                    packet: pkt,
                });
            }
        }
        Ok(RxOutcome::Consumed(consumed))
    }

    fn on_stream_writable(
        &mut self,
        _assoc: AssociationId,
        _stream_id: StreamId,
        _available_bytes: usize,
    ) {
    }

    fn on_association_writable(&mut self, _assoc: AssociationId, _available_bytes: u64) {}

    fn on_device_writable(&mut self) {}

    fn on_stream_closed(
        &mut self,
        assoc: AssociationId,
        stream_id: StreamId,
        _reason: StreamCloseReason,
    ) {
        if let Some(s) = self
            .sessions
            .iter_mut()
            .find(|s| !s.closed && s.assoc == assoc && s.stream_id == stream_id)
        {
            s.close_native(self.hw, self.stream_table);
        }
    }

    fn on_association_broken(&mut self, assoc: AssociationId, _reason: AssociationBrokenReason) {
        if !self.broken_assocs.contains(&assoc) {
            self.broken_assocs.push(assoc);
        }
    }
}

struct AssocSlot {
    assoc_id: AssociationId,
    assoc: Box<QuicLinkAssoc<XipPacket>>,
    active: bool,
    local_cid: Option<ConnectionId>,
    peer_scid: Option<ConnectionId>,
    udp_peer: PeerRoute,
    serial_peer: PeerRoute,
    last_activity_us: u64,
    last_pn1_hash: u64,
}

impl AssocSlot {
    fn new(idx: usize, udp_encrypt: bool, serial_encrypt: bool) -> Self {
        let assoc_id = slot_assoc_id(idx);
        let mut cfg = default_assoc_config();
        cfg.assoc_id = assoc_id;
        Self {
            assoc_id,
            assoc: Self::build_assoc(cfg, udp_encrypt, serial_encrypt, false, &[]),
            active: false,
            local_cid: None,
            peer_scid: None,
            udp_peer: PeerRoute::default(),
            serial_peer: PeerRoute::default(),
            last_activity_us: 0,
            last_pn1_hash: 0,
        }
    }

    #[inline(never)]
    fn build_assoc(
        cfg: AssocConfig,
        udp_encrypt: bool,
        serial_encrypt: bool,
        tcp_encrypt: bool,
        tcp_states: &[(LinkId, bool, bool)],
    ) -> Box<QuicLinkAssoc<XipPacket>> {
        let mut assoc = Box::new(QuicLinkAssoc::new(cfg));
        assoc.upsert_link(LINK_UDP, udp_link_profile_with_encrypt(udp_encrypt));
        assoc.upsert_link(LINK_SERIAL, serial_link_profile_with_encrypt(serial_encrypt));
        if let Some(l) = assoc.link_mut(LINK_UDP) {
            l.enabled = false;
        }
        if let Some(l) = assoc.link_mut(LINK_SERIAL) {
            l.enabled = false;
        }
        for &(link_id, enabled, driver_ready) in tcp_states {
            assoc.upsert_link(link_id, tcp_link_profile_with_encrypt(tcp_encrypt));
            if let Some(l) = assoc.link_mut(link_id) {
                l.enabled = enabled;
                l.driver_ready = driver_ready;
            }
        }
        assoc
    }
}

/// Selectable short-packet AEAD cipher (`0 = AES-128-GCM` default, `1 = AES-128-CCM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AeadCipher {
    Gcm = 0,
    Ccm = 1,
}

struct Context {
    hw: Hw,
    stream_table: Option<&'static Stream>,
    quic_attached: bool,
    mux: Box<QuicMux<XipPool>>,
    slots: [AssocSlot; MAX_ASSOCS],
    next_server_cid: u64,
    udp_encrypt: bool,
    serial_encrypt: bool,
    tcp_encrypt: bool,
    cipher: AeadCipher,
    local_public_key: Option<[u8; PUBLIC_KEY_COMPRESSED_LEN]>,
    node_secret: Option<[u8; 32]>,
    send_api_key: Option<[u8; 32]>,
    drivers: Quic2Drivers,
    sessions: Vec<StreamSession>,
    alarm: i32,
    received_packets: u64,
    errors: u64,
    last_error: i32,
}

fn psa_sha256(ctx: usize, chunks: &[&[u8]], out: &mut [u8; 32]) -> Result<(), CryptoError> {
    let Some(hw) = (unsafe { Hw::new(ctx as *const Host) }) else {
        return Err(CryptoError::Invalid);
    };
    let Some(crypto) = CryptoClient::from_host(hw) else {
        return Err(CryptoError::Invalid);
    };
    let mut h = crypto.sha256_stream().map_err(|_| CryptoError::Invalid)?;
    for chunk in chunks {
        h.update(chunk).map_err(|_| CryptoError::Invalid)?;
    }
    *out = h.finish().map_err(|_| CryptoError::Invalid)?;
    Ok(())
}

fn psa_aead_seal_alg(
    alg: u32,
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
) -> Result<[u8; AEAD_TAG_LEN], CryptoError> {
    let Some(hw) = (unsafe { Hw::new(ctx as *const Host) }) else {
        return Err(CryptoError::Invalid);
    };
    let Some(crypto) = CryptoClient::from_host(hw) else {
        return Err(CryptoError::Invalid);
    };
    crypto.aead_seal(alg, key, nonce, aad, body).map_err(|rc| {
        if rc == ERR_SIZE {
            CryptoError::BufferTooSmall
        } else {
            CryptoError::Invalid
        }
    })
}

fn psa_aead_open_alg(
    alg: u32,
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; AEAD_TAG_LEN],
) -> Result<(), CryptoError> {
    let Some(hw) = (unsafe { Hw::new(ctx as *const Host) }) else {
        return Err(CryptoError::AuthenticationFailed);
    };
    let Some(crypto) = CryptoClient::from_host(hw) else {
        return Err(CryptoError::AuthenticationFailed);
    };
    crypto.aead_open(alg, key, nonce, aad, body, tag).map_err(|rc| {
        if rc == ERR_SIZE {
            CryptoError::BufferTooSmall
        } else if rc == ERR_AUTH {
            CryptoError::AuthenticationFailed
        } else {
            CryptoError::Invalid
        }
    })
}

#[inline(always)]
fn psa_aead_pkt_alg(
    alg: u32,
    ctx: usize,
    open: bool,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    header_len: usize,
    src_handle: u32,
    src_slot: &[u8],
    src_range: core::ops::Range<usize>,
) -> Result<(u32, core::ops::Range<usize>), CryptoError> {
    let Some(hw) = (unsafe { Hw::new(ctx as *const Host) }) else {
        return Err(CryptoError::Invalid);
    };
    let Some(crypto) = CryptoClient::from_host(hw) else {
        return Err(CryptoError::Invalid);
    };
    if pack_aead_pkt_spec(alg, open, key.len(), aad.len()).is_none() || src_range.end > src_slot.len() {
        return Err(CryptoError::Invalid);
    }
    if src_range.len() <= header_len {
        return Err(CryptoError::Invalid);
    }
    let dst_handle = hw.pkt_alloc().map_err(|_| CryptoError::BufferTooSmall)?;
    struct DropHandle(Hw, u32);
    impl Drop for DropHandle {
        fn drop(&mut self) {
            if self.1 != 0 {
                self.0.pkt_free(self.1);
            }
        }
    }
    let mut guard = DropHandle(hw, dst_handle);
    let (dst_base, dst_cap) = hw.pkt_span(dst_handle).map_err(|_| CryptoError::Invalid)?;
    let hdr_meta_len = key.len() + 12 + aad.len();
    let src_body_off = src_range.start + header_len;

    let aligned = ((PKT_HEADROOM as usize).max(hdr_meta_len + header_len) + 63) & !63;
    let (dst_body_off, out_len, text_len) = if !open {
        let plain_len = src_range.len() - header_len;
        let out_len = plain_len + AEAD_TAG_LEN;
        let dst_off = if (src_body_off & 63) == 0
            && src_body_off >= hdr_meta_len + header_len
            && src_body_off + out_len <= dst_cap
        {
            src_body_off
        } else if aligned + out_len <= dst_cap {
            aligned
        } else if src_body_off >= hdr_meta_len + header_len && src_body_off + out_len <= dst_cap {
            src_body_off
        } else {
            return Err(CryptoError::BufferTooSmall);
        };
        (dst_off, out_len, plain_len)
    } else {
        if src_range.len() < header_len + AEAD_TAG_LEN + 1 {
            return Err(CryptoError::Invalid);
        }
        let ct_with_tag = src_range.len() - header_len;
        let plain_len = ct_with_tag - AEAD_TAG_LEN;
        let dst_off = if (src_body_off & 63) == 0
            && src_body_off >= hdr_meta_len + header_len
            && src_body_off + plain_len <= dst_cap
        {
            src_body_off
        } else if aligned + plain_len <= dst_cap {
            aligned
        } else if src_body_off >= hdr_meta_len + header_len && src_body_off + plain_len <= dst_cap {
            src_body_off
        } else {
            return Err(CryptoError::BufferTooSmall);
        };
        (dst_off, plain_len, ct_with_tag)
    };

    let dst_hdr_start = dst_body_off - header_len;
    // SAFETY: `dst_base..dst_base + dst_cap` is the uniquely owned borrowed destination slot,
    // and `0..hdr_meta_len` sits strictly before `dst_hdr_start`.
    unsafe {
        let dst_slot = core::slice::from_raw_parts_mut(dst_base, dst_cap);
        dst_slot[..key.len()].copy_from_slice(key);
        dst_slot[key.len()..key.len() + 12].copy_from_slice(nonce);
        if !aad.is_empty() {
            dst_slot[key.len() + 12..hdr_meta_len].copy_from_slice(aad);
        }
        dst_slot[dst_hdr_start..dst_body_off].copy_from_slice(&src_slot[src_range.start..src_body_off]);
        if !open {
            dst_slot[dst_hdr_start] |= FLAG_PROTECTED;
        }
    }

    let rc = if !open {
        crypto.aead_seal_pkt(
            alg,
            key.len(),
            aad.len(),
            src_handle,
            src_body_off as u32,
            dst_handle,
            dst_body_off as u32,
            text_len as u32,
        )
    } else {
        crypto.aead_open_pkt(
            alg,
            key.len(),
            aad.len(),
            src_handle,
            src_body_off as u32,
            dst_handle,
            dst_body_off as u32,
            text_len as u32,
        )
    };
    match rc {
        Ok(written) if written == out_len => {
            guard.1 = 0;
            Ok((dst_handle, dst_hdr_start..(dst_body_off + out_len)))
        }
        Ok(_) => Err(CryptoError::Invalid),
        Err(ERR_SIZE) => Err(CryptoError::BufferTooSmall),
        Err(ERR_AUTH) => Err(CryptoError::AuthenticationFailed),
        Err(_) => Err(CryptoError::Invalid),
    }
}

fn psa_aead_seal_gcm(
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
) -> Result<[u8; AEAD_TAG_LEN], CryptoError> {
    psa_aead_seal_alg(ALG_GCM, ctx, key, nonce, aad, body)
}

fn psa_aead_open_gcm(
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; AEAD_TAG_LEN],
) -> Result<(), CryptoError> {
    psa_aead_open_alg(ALG_GCM, ctx, key, nonce, aad, body, tag)
}

fn psa_aead_pkt_gcm(
    ctx: usize,
    open: bool,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    header_len: usize,
    src_handle: u32,
    src_slot: &[u8],
    src_range: core::ops::Range<usize>,
) -> Result<(u32, core::ops::Range<usize>), CryptoError> {
    psa_aead_pkt_alg(ALG_GCM, ctx, open, key, nonce, aad, header_len, src_handle, src_slot, src_range)
}

fn psa_aead_seal_ccm(
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
) -> Result<[u8; AEAD_TAG_LEN], CryptoError> {
    psa_aead_seal_alg(ALG_CCM, ctx, key, nonce, aad, body)
}

fn psa_aead_open_ccm(
    ctx: usize,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    body: &mut [u8],
    tag: &[u8; AEAD_TAG_LEN],
) -> Result<(), CryptoError> {
    psa_aead_open_alg(ALG_CCM, ctx, key, nonce, aad, body, tag)
}

fn psa_aead_pkt_ccm(
    ctx: usize,
    open: bool,
    key: &[u8; TRAFFIC_KEY_LEN],
    nonce: &[u8; TRAFFIC_IV_LEN],
    aad: &[u8],
    header_len: usize,
    src_handle: u32,
    src_slot: &[u8],
    src_range: core::ops::Range<usize>,
) -> Result<(u32, core::ops::Range<usize>), CryptoError> {
    psa_aead_pkt_alg(ALG_CCM, ctx, open, key, nonce, aad, header_len, src_handle, src_slot, src_range)
}

pub fn psa_crypto_provider_for(hw: Hw, cipher: AeadCipher) -> CryptoProvider {
    let has_pkt = CryptoClient::from_host(hw).is_some_and(|c| c.supports_aead_pkt());
    match cipher {
        AeadCipher::Gcm => CryptoProvider {
            context: hw.as_ptr() as usize,
            sha256: psa_sha256,
            aead_seal: psa_aead_seal_gcm,
            aead_open: psa_aead_open_gcm,
            aead_packet: has_pkt.then_some(psa_aead_pkt_gcm),
        },
        AeadCipher::Ccm => CryptoProvider {
            context: hw.as_ptr() as usize,
            sha256: psa_sha256,
            aead_seal: psa_aead_seal_ccm,
            aead_open: psa_aead_open_ccm,
            aead_packet: has_pkt.then_some(psa_aead_pkt_ccm),
        },
    }
}

pub fn psa_crypto_provider(hw: Hw) -> CryptoProvider {
    psa_crypto_provider_for(hw, AeadCipher::Gcm)
}

fn read_device_compressed_pubkey(hw: Hw) -> Option<[u8; PUBLIC_KEY_COMPRESSED_LEN]> {
    let crypto = CryptoClient::from_host(hw)?;
    let pub65 = crypto.identity_public_key().ok()?;
    let mut out = [0u8; PUBLIC_KEY_COMPRESSED_LEN];
    out[0] = 2 | (pub65[64] & 1);
    out[1..].copy_from_slice(&pub65[1..33]);
    Some(out)
}

fn fill_random_32(hw: Hw, seq: u64) -> [u8; ASSOCIATION_RANDOM_LEN] {
    let mut out = [0u8; ASSOCIATION_RANDOM_LEN];
    if let Some(crypto) = CryptoClient::from_host(hw) {
        if crypto.random(&mut out).is_ok() {
            return out;
        }
    }
    let now = hw.now_us().unwrap_or_else(|| hw.now_ms().saturating_mul(1000));
    let mut state = fnv1a64(&now.to_le_bytes()) ^ seq.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for chunk in out.chunks_mut(8) {
        state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
        let bytes = state.to_be_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

impl Context {
    #[inline(never)]
    fn new_mux(hw: Hw) -> Box<QuicMux<XipPool>> {
        Box::new(QuicMux::new(XipPool::new(hw), embedded_mux_limits()))
    }

    #[inline(never)]
    fn new(hw: Hw) -> Self {
        let local_public_key = read_device_compressed_pubkey(hw);
        let mut cx = Self {
            hw,
            stream_table: hw.stream(),
            quic_attached: false,
            mux: Self::new_mux(hw),
            slots: [AssocSlot::new(0, true, false)],
            next_server_cid: 2,
            udp_encrypt: true,
            serial_encrypt: false,
            tcp_encrypt: false,
            cipher: AeadCipher::Gcm,
            local_public_key,
            node_secret: None,
            send_api_key: None,
            drivers: Quic2Drivers::new(hw),
            sessions: Vec::new(),
            alarm: -1,
            received_packets: 0,
            errors: 0,
            last_error: 0,
        };
        cx.ensure_quic_attached();
        cx
    }

    fn apply_slot_crypto(&mut self, idx: usize, mode: HandshakeMode) {
        if self.node_secret.is_none() && self.send_api_key.is_none() {
            self.slots[idx].assoc.clear_crypto();
            return;
        }
        let seq = self.next_server_cid;
        self.next_server_cid = self.next_server_cid.wrapping_add(1).max(2);
        let cfg = AssocCryptoConfig {
            provider: psa_crypto_provider_for(self.hw, self.cipher),
            role: CryptoRole::Server,
            local_public_key: if mode == HandshakeMode::None {
                None
            } else {
                self.local_public_key
            },
            local_random: fill_random_32(self.hw, seq),
            send_api_key: self.send_api_key,
            node_secret: self.node_secret,
        };
        if self.slots[idx].assoc.set_crypto_config(cfg).is_err() {
            self.errors += 1;
            self.last_error = ERR_ARG;
        }
    }

    fn set_udp_encryption(&mut self, encrypt: bool) {
        self.udp_encrypt = encrypt;
        for slot in &mut self.slots {
            if let Some(l) = slot.assoc.link_mut(LINK_UDP) {
                l.profile.encrypt = encrypt;
            }
        }
    }

    fn set_serial_encryption(&mut self, encrypt: bool) {
        self.serial_encrypt = encrypt;
        for slot in &mut self.slots {
            if let Some(l) = slot.assoc.link_mut(LINK_SERIAL) {
                l.profile.encrypt = encrypt;
            }
        }
    }

    fn set_tcp_encryption(&mut self, encrypt: bool) {
        self.tcp_encrypt = encrypt;
        for slot in &mut self.slots {
            for idx in 0..16u16 {
                if let Some(l) = slot.assoc.link_mut(LinkId(LINK_TCP_BASE + idx)) {
                    l.profile.encrypt = encrypt;
                }
            }
        }
    }

    fn configure_cbor(&mut self, data: &[u8]) -> Result<(), i32> {
        let mut keys_changed = false;
        if !data.is_empty() {
            if let Some(mut r) = cbor::get(data, 1).map_err(|_| ERR_ARG)? {
                match r.next() {
                    Some(Item::Bool(b)) => self.set_udp_encryption(b),
                    _ => return Err(ERR_ARG),
                }
            }
            if let Some(mut r) = cbor::get(data, 2).map_err(|_| ERR_ARG)? {
                match r.next() {
                    Some(Item::Bytes(b)) if b.is_empty() => {
                        self.node_secret = None;
                        keys_changed = true;
                    }
                    Some(Item::Bytes(b)) if b.len() == 32 => {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(b);
                        self.node_secret = Some(key);
                        keys_changed = true;
                    }
                    _ => return Err(ERR_ARG),
                }
            }
            if let Some(mut r) = cbor::get(data, 3).map_err(|_| ERR_ARG)? {
                match r.next() {
                    Some(Item::Bytes(b)) if b.is_empty() => {
                        self.send_api_key = None;
                        keys_changed = true;
                    }
                    Some(Item::Bytes(b)) if b.len() == 32 => {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(b);
                        self.send_api_key = Some(key);
                        keys_changed = true;
                    }
                    _ => return Err(ERR_ARG),
                }
            }
            if let Some(mut r) = cbor::get(data, 4).map_err(|_| ERR_ARG)? {
                match r.next() {
                    Some(Item::Bool(b)) => self.set_serial_encryption(b),
                    _ => return Err(ERR_ARG),
                }
            }
            if let Some(mut r) = cbor::get(data, 5).map_err(|_| ERR_ARG)? {
                match r.next() {
                    Some(Item::Bool(b)) => self.set_tcp_encryption(b),
                    _ => return Err(ERR_ARG),
                }
            }
            if let Some(mut r) = cbor::get(data, 6).map_err(|_| ERR_ARG)? {
                let cipher = match r.next() {
                    Some(Item::Uint(0)) => AeadCipher::Gcm,
                    Some(Item::Uint(1)) => AeadCipher::Ccm,
                    _ => return Err(ERR_ARG),
                };
                if self.cipher != cipher {
                    self.cipher = cipher;
                    keys_changed = true;
                }
            }
        }
        if keys_changed {
            let mode = self.slots[0].assoc.config().handshake_mode;
            self.apply_slot_crypto(0, mode);
        }
        Ok(())
    }

    fn ensure_quic_attached(&mut self) {
        if self.quic_attached {
            return;
        }
        let rc = Self::try_attach_quic(self.hw);
        if rc == 0 {
            self.quic_attached = true;
            if self.last_error < 0 {
                self.last_error = 0;
            }
        } else {
            if rc == ERR_BUSY {
                let _ = self.hw.module_stop(b"quic");
            }
            self.last_error = rc;
            if self.alarm < 0 {
                self.alarm = self
                    .hw
                    .alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            }
        }
    }

    fn try_attach_quic(hw: Hw) -> i32 {
        let p = hw.get_component(c"quic") as *const Quic;
        if p.is_null() {
            return -101;
        }
        let q = unsafe { &*p };
        if q.version != QUIC_VERSION || (q.size as usize) < core::mem::size_of::<Quic>() {
            return -102;
        }
        let Some(attach) = q.attach else {
            return -103;
        };
        unsafe { attach(hw.ctx(), RX) }
    }

    fn detach_quic(&mut self) {
        if !self.quic_attached {
            return;
        }
        self.quic_attached = false;
        let p = self.hw.get_component(c"quic") as *const Quic;
        if p.is_null() {
            return;
        }
        let q = unsafe { &*p };
        if let Some(detach) = q.detach {
            unsafe {
                detach(self.hw.ctx());
            }
        }
    }

    fn now_us(&self, fallback_us: u32) -> u64 {
        self.hw.now_us().unwrap_or_else(|| {
            let ms = self.hw.now_ms();
            if ms != 0 {
                ms.saturating_mul(1000)
            } else {
                u64::from(fallback_us)
            }
        })
    }

    fn slot_has_active_streams(&self, assoc: AssociationId) -> bool {
        self.sessions
            .iter()
            .any(|s| !s.closed && s.assoc == assoc)
    }

    fn tcp_link_states(&self) -> Vec<(LinkId, bool, bool)> {
        self.drivers
            .tcp
            .iter()
            .map(|t| {
                let (en, rdy) = self.slots[0]
                    .assoc
                    .link(t.link_id)
                    .map_or((true, false), |l| (l.enabled, l.driver_ready));
                (t.link_id, en, rdy)
            })
            .collect()
    }

    fn clear_slot_state(&mut self, idx: usize) {
        let assoc_id = self.slots[idx].assoc_id;
        let mut broken = Vec::new();
        if self.slots[idx].active {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.remove_association(assoc_id, &mut env);
            self.slots[idx].active = false;
        }
        for s in &mut self.sessions {
            if !s.closed && s.assoc == assoc_id {
                s.close_native(self.hw, self.stream_table);
            }
        }
        self.sessions.retain(|s| s.assoc != assoc_id);
        self.slots[idx].local_cid = None;
        self.slots[idx].peer_scid = None;
        self.slots[idx].udp_peer = PeerRoute::default();
        self.slots[idx].serial_peer = PeerRoute::default();
        self.slots[idx].last_activity_us = 0;
        self.slots[idx].last_pn1_hash = 0;
    }

    #[inline(never)]
    fn reinit_slot(&mut self, idx: usize) {
        self.clear_slot_state(idx);
        let tcp_states = if idx == 0 {
            self.tcp_link_states()
        } else {
            Vec::new()
        };
        let mut cfg = default_assoc_config();
        cfg.assoc_id = self.slots[idx].assoc_id;
        self.slots[idx].assoc = AssocSlot::build_assoc(
            cfg,
            self.udp_encrypt,
            self.serial_encrypt,
            self.tcp_encrypt,
            &tcp_states,
        );
        self.apply_slot_crypto(idx, HandshakeMode::None);
    }

    fn ensure_slot_legacy_active(&mut self, idx: usize) -> Result<(), i32> {
        if !self.slots[idx].active {
            self.mux
                .add_association(self.slots[idx].assoc_id, false)
                .map_err(|_| ERR_ARG)?;
            self.slots[idx].active = true;
            self.apply_slot_crypto(idx, HandshakeMode::None);
        }
        Ok(())
    }

    fn alloc_server_cid(&mut self) -> ConnectionId {
        for _ in 0..126 {
            let v = self.next_server_cid;
            self.next_server_cid = if self.next_server_cid >= 0x7f {
                2
            } else {
                self.next_server_cid + 1
            };
            if let Some(cid) = ConnectionId::new(v) {
                let in_use = self
                    .slots
                    .iter()
                    .any(|s| s.active && s.local_cid == Some(cid));
                if !in_use {
                    return cid;
                }
            }
        }
        ConnectionId::new(2).unwrap()
    }

    #[inline(never)]
    fn activate_handshake_slot(
        &mut self,
        idx: usize,
        server_cid: ConnectionId,
        client_scid: Option<ConnectionId>,
    ) -> Result<(), i32> {
        self.clear_slot_state(idx);
        let assoc_id = self.slots[idx].assoc_id;
        let tcp_states = if idx == 0 {
            self.tcp_link_states()
        } else {
            Vec::new()
        };
        self.mux
            .add_association(assoc_id, false)
            .map_err(|_| ERR_ARG)?;
        self.slots[idx].active = true;
        let cfg = handshake_assoc_config(assoc_id, server_cid, client_scid);
        self.slots[idx].assoc = AssocSlot::build_assoc(
            cfg,
            self.udp_encrypt,
            self.serial_encrypt,
            self.tcp_encrypt,
            &tcp_states,
        );
        self.apply_slot_crypto(idx, HandshakeMode::Server);
        self.slots[idx].local_cid = Some(server_cid);
        self.slots[idx].peer_scid = client_scid;
        Ok(())
    }

    fn is_serial_link(&self, link_addr: u32, call_method: i32) -> bool {
        if let Some(link_table) = self.hw.link() {
            if let Some(info_fn) = link_table.info {
                let mut info: LinkInfo = unsafe { core::mem::zeroed() };
                info.size = core::mem::size_of::<LinkInfo>() as u32;
                if unsafe { info_fn(self.hw.ctx(), link_addr, &mut info) } == 0
                    && info.provider == LINK_NATIVE
                {
                    let len = info
                        .name
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(info.name.len());
                    let name = &info.name[..len];
                    if name == b"udp" || name == b"udp6" || name.starts_with(b"udp:") {
                        return false;
                    }
                    if name == b"serial" || name == b"ppp" {
                        return true;
                    }
                }
            }
        }
        call_method != RX
    }

    fn slot_matches_route(
        slot: &AssocSlot,
        link_id: LinkId,
        inbound_route: Option<PeerRoute>,
    ) -> bool {
        let Some(route) = inbound_route else {
            return true;
        };
        let current = match link_id {
            LINK_UDP => slot.udp_peer,
            LINK_SERIAL => slot.serial_peer,
            _ => return true,
        };
        !current.active || (current.link == route.link && current.token == route.token)
    }

    fn slot_is_evictable(&self, idx: usize) -> bool {
        let assoc_id = self.slots[idx].assoc_id;
        if self.slot_has_active_streams(assoc_id) || self.slots[idx].assoc.outbound_queue_len() > 0 {
            return false;
        }
        if idx == 0 && !self.drivers.tcp.iter().all(|t| t.is_tx_idle()) {
            return false;
        }
        true
    }

    fn apply_slot_route(&mut self, idx: usize, link_id: LinkId, inbound_route: Option<PeerRoute>) {
        if let Some(route) = inbound_route {
            if link_id == LINK_SERIAL {
                self.slots[idx].serial_peer = route;
                self.drivers.serial_peer = route;
            } else if link_id == LINK_UDP {
                self.slots[idx].udp_peer = route;
                self.drivers.udp_peer = route;
            }
        }
        if let Some(l) = self.slots[idx].assoc.link_mut(link_id) {
            l.enabled = true;
            l.driver_ready = true;
        }
    }

    #[inline(never)]
    fn receive_link_packet(&mut self, call: &CallView, packet: Packet) -> Result<(), i32> {
        if call.data.is_empty() {
            return Err(ERR_ARG);
        }
        let handle = packet.into_raw();
        let pkt = XipPacket::from_owned(self.hw, handle, 0..0)?;
        let base = pkt.slot_slice().as_ptr() as usize;
        let start = (call.data.as_ptr() as usize)
            .checked_sub(base)
            .ok_or(ERR_ARG)?;
        let mut pkt = pkt;
        pkt.set_range(start..(start + call.data.len()))
            .map_err(|_| ERR_ARG)?;

        let is_serial = self.is_serial_link(call.from, call.method);
        let (link_id, route) = if is_serial {
            let token = if call.id == 0 {
                QUIC2_SERIAL_PORT
            } else {
                call.id
            };
            (
                LINK_SERIAL,
                PeerRoute {
                    active: true,
                    link: call.from,
                    token,
                },
            )
        } else {
            (
                LINK_UDP,
                PeerRoute {
                    active: true,
                    link: call.from,
                    token: call.id,
                },
            )
        };

        let now_us = self.now_us(call.time_us);
        self.ingest_quic2_packet(link_id, Some(route), pkt, now_us)
    }

    fn should_reset_idle_slot(&self, idx: usize, wire: &[u8], now_us: u64) -> bool {
        let slot = &self.slots[idx];
        let Some(largest) = slot.assoc.rx_ack_ranges().largest() else {
            return false;
        };
        if self.slot_has_active_streams(slot.assoc_id)
            || slot.assoc.outbound_queue_len() > 0
            || (idx == 0 && !self.drivers.tcp.iter().all(|t| t.is_tx_idle()))
        {
            return false;
        }
        let Ok((hdr, hdr_len)) = ShortHeader::decode(wire) else {
            return false;
        };
        if hdr.packet_number != 1 || hdr.packet_number_len != 1 || hdr_len >= wire.len() {
            return false;
        }
        let quiet = now_us.saturating_sub(slot.last_activity_us) >= EPOCH_REINJECT_MIN_QUIET_US;
        if largest >= 250 && !quiet {
            return false;
        }
        fnv1a64(wire) != slot.last_pn1_hash || quiet
    }

    #[inline(never)]
    fn reset_idle_slot(&mut self, idx: usize) {
        let udp_state = self.slots[idx]
            .assoc
            .link(LINK_UDP)
            .map(|l| (l.enabled, l.driver_ready))
            .unwrap_or((false, false));
        let serial_state = self.slots[idx]
            .assoc
            .link(LINK_SERIAL)
            .map(|l| (l.enabled, l.driver_ready))
            .unwrap_or((false, false));
        let tcp_states = if idx == 0 {
            self.tcp_link_states()
        } else {
            Vec::new()
        };

        self.clear_slot_state(idx);
        let mut cfg = default_assoc_config();
        cfg.assoc_id = self.slots[idx].assoc_id;
        self.slots[idx].assoc = AssocSlot::build_assoc(
            cfg,
            self.udp_encrypt,
            self.serial_encrypt,
            self.tcp_encrypt,
            &tcp_states,
        );
        self.apply_slot_crypto(idx, HandshakeMode::None);
        if let Some(l) = self.slots[idx].assoc.link_mut(LINK_UDP) {
            l.enabled = udp_state.0;
            l.driver_ready = udp_state.1;
        }
        if let Some(l) = self.slots[idx].assoc.link_mut(LINK_SERIAL) {
            l.enabled = serial_state.0;
            l.driver_ready = serial_state.1;
        }
    }

    #[inline(never)]
    fn select_slot_for_packet(
        &mut self,
        link_id: LinkId,
        inbound_route: Option<PeerRoute>,
        wire: &[u8],
        now_us: u64,
    ) -> Result<usize, i32> {
        if is_initial_packet(wire) {
            let Ok((init_hdr, _)) = InitialHeader::decode(wire) else {
                return Err(ERR_ARG);
            };
            if link_id.0 >= LINK_TCP_BASE {
                let server_cid = self.slots[0]
                    .local_cid
                    .unwrap_or_else(|| self.alloc_server_cid());
                if !self.slots[0].active
                    || self.slots[0].local_cid.is_none()
                    || (init_hdr.scid.is_some() && self.slots[0].peer_scid != init_hdr.scid)
                {
                    self.activate_handshake_slot(0, server_cid, init_hdr.scid)?;
                }
                return Ok(0);
            }

            if let Some(dcid) = init_hdr.dcid {
                if let Some(idx) = self
                    .slots
                    .iter()
                    .position(|s| s.active && s.local_cid == Some(dcid))
                {
                    return Ok(idx);
                }
            }
            if let Some(scid) = init_hdr.scid {
                if let Some(idx) = self.slots.iter().position(|s| {
                    s.active
                        && s.local_cid.is_some()
                        && s.peer_scid == Some(scid)
                        && Self::slot_matches_route(s, link_id, inbound_route)
                }) {
                    return Ok(idx);
                }
            }

            if self.slots[0].active && !self.slot_is_evictable(0) {
                return Err(ERR_BUSY);
            }

            let server_cid = self.alloc_server_cid();
            self.activate_handshake_slot(0, server_cid, init_hdr.scid)?;
            return Ok(0);
        }

        if link_id.0 >= LINK_TCP_BASE {
            self.ensure_slot_legacy_active(0)?;
            return Ok(0);
        }

        let Ok(prefix) = ShortHeader::decode_prefix(wire) else {
            return Err(ERR_ARG);
        };
        if prefix.dcid.value() > 1 {
            let Some(idx) = self
                .slots
                .iter()
                .position(|s| s.active && s.local_cid == Some(prefix.dcid))
            else {
                return Err(ERR_IO);
            };
            return Ok(idx);
        }

        // Legacy fixed `dcid = 1` traffic uses slot 0.
        if self.slots[0].active && self.slots[0].local_cid.is_some() {
            if !self.slot_is_evictable(0) {
                return Err(ERR_BUSY);
            }
            self.reset_idle_slot(0);
        } else if self.should_reset_idle_slot(0, wire, now_us) {
            self.reset_idle_slot(0);
        }
        self.ensure_slot_legacy_active(0)?;
        Ok(0)
    }

    #[inline(never)]
    fn assoc_receive_mux_packet(
        &mut self,
        slot_idx: usize,
        link_id: LinkId,
        pkt: XipPacket,
        now_us: u64,
    ) -> Result<Option<XipPacket>, i32> {
        self.slots[slot_idx]
            .assoc
            .receive_packet(link_id, pkt, now_us)
            .map(|outcome| outcome.mux_packet)
            .map_err(|err| match err {
                quic_link::LinkRecvError::Wire(quic_mux::WireError::BufferTooSmall) => -21,
                quic_link::LinkRecvError::Wire(_) => -22,
                quic_link::LinkRecvError::UnknownConnectionId(_) => -23,
                quic_link::LinkRecvError::DuplicatePacket(_) => -24,
                quic_link::LinkRecvError::DatagramTooLarge(_) => -25,
                quic_link::LinkRecvError::Crypto(CryptoError::Invalid) => -31,
                quic_link::LinkRecvError::Crypto(CryptoError::BufferTooSmall) => -32,
                quic_link::LinkRecvError::Crypto(CryptoError::AuthenticationFailed) => -33,
                quic_link::LinkRecvError::Crypto(CryptoError::SharedPacket) => -34,
            })
    }

    #[inline(never)]
    fn mux_receive_packet(
        &mut self,
        assoc_id: AssociationId,
        mux_pkt: XipPacket,
        now_us: u64,
    ) -> Result<(), i32> {
        let mut broken = Vec::new();
        let res = {
            let mut env = CallbackEnv {
                hw: self.hw,
                stream_table: self.stream_table,
                sessions: &mut self.sessions,
                broken_assocs: &mut broken,
                errors: &mut self.errors,
                last_error: &mut self.last_error,
            };
            self.mux.receive_packet(
                RxMeta {
                    association: assoc_id,
                    received_at_us: now_us,
                },
                mux_pkt,
                &mut *self.slots[0].assoc,
                &mut env,
            )
        };
        for bad in broken {
            if bad == self.slots[0].assoc_id {
                self.reinit_slot(0);
            }
        }
        res.map_err(|err| match err {
            quic_mux::MuxError::UnknownAssociation => -41,
            quic_mux::MuxError::DuplicateAssociation => -42,
            quic_mux::MuxError::InvalidStreamState => -43,
            quic_mux::MuxError::StreamLimitReached => -44,
            quic_mux::MuxError::FlowControlViolation => -45,
            quic_mux::MuxError::Wire(_) => -46,
            _ => ERR_IO,
        })
    }

    #[inline(never)]
    fn ingest_quic2_packet(
        &mut self,
        link_id: LinkId,
        inbound_route: Option<PeerRoute>,
        pkt: XipPacket,
        now_us: u64,
    ) -> Result<(), i32> {
        let is_pn1_wire = ShortHeader::decode(pkt.bytes())
            .is_ok_and(|(hdr, _)| hdr.packet_number == 1 && hdr.packet_number_len == 1);
        let pn1_hash = if is_pn1_wire {
            fnv1a64(pkt.bytes())
        } else {
            0
        };
        let slot_idx = self.select_slot_for_packet(link_id, inbound_route, pkt.bytes(), now_us)?;
        self.apply_slot_route(slot_idx, link_id, inbound_route);

        self.received_packets += 1;
        let mux_pkt = self.assoc_receive_mux_packet(slot_idx, link_id, pkt, now_us)?;
        self.slots[slot_idx].last_activity_us = self.slots[slot_idx].last_activity_us.max(now_us);
        if is_pn1_wire {
            self.slots[slot_idx].last_pn1_hash = pn1_hash;
        }
        let assoc_id = self.slots[slot_idx].assoc_id;

        if let Some(mux_pkt) = mux_pkt {
            self.mux_receive_packet(assoc_id, mux_pkt, now_us)?;
        }
        Ok(())
    }

    fn handle_socket_event(&mut self, socket: u32, event: u32, now_us: u64) {
        match event {
            SOCKET_EVENT_OPEN => {
                if self
                    .drivers
                    .add_tcp_socket(socket, self.tcp_encrypt, &mut *self.slots[0].assoc)
                    .is_some()
                {
                    let _ = self.ensure_slot_legacy_active(0);
                    self.read_tcp_frames(socket, now_us);
                }
            }
            SOCKET_EVENT_READABLE => {
                self.read_tcp_frames(socket, now_us);
            }
            SOCKET_EVENT_WRITABLE => {
                self.drivers
                    .on_tcp_writable(socket, &mut *self.slots[0].assoc, now_us);
                self.notify_assoc_ready();
            }
            SOCKET_EVENT_WRITE_DONE => {
                self.drivers
                    .on_tcp_write_done(socket, &mut *self.slots[0].assoc, now_us);
                self.notify_assoc_ready();
                let should_close = self
                    .drivers
                    .tcp
                    .iter()
                    .find(|t| t.socket == socket)
                    .is_some_and(|t| t.peer_eof && t.is_tx_idle());
                if should_close {
                    self.drivers
                        .remove_tcp_socket(socket, &mut *self.slots[0].assoc, now_us);
                }
            }
            SOCKET_EVENT_EOF | SOCKET_EVENT_CLOSED => {
                self.drivers
                    .remove_tcp_socket(socket, &mut *self.slots[0].assoc, now_us);
            }
            _ => {}
        }
    }

    fn notify_assoc_ready(&mut self) {
        for idx in 0..MAX_ASSOCS {
            if !self.slots[idx].active {
                continue;
            }
            let assoc_id = self.slots[idx].assoc_id;
            let mut broken = Vec::new();
            {
                let mut env = CallbackEnv {
                    hw: self.hw,
                    stream_table: self.stream_table,
                    sessions: &mut self.sessions,
                    broken_assocs: &mut broken,
                    errors: &mut self.errors,
                    last_error: &mut self.last_error,
                };
                self.mux.on_association_ready(
                    assoc_id,
                    &mut *self.slots[0].assoc,
                    &mut env,
                );
            }
            for bad in broken {
                if bad == self.slots[0].assoc_id {
                    self.reinit_slot(0);
                }
            }
        }
    }

    fn read_tcp_frames(&mut self, socket: u32, now_us: u64) {
        let pool = *self.mux.pool();
        let mut packets: Vec<(LinkId, XipPacket)> = Vec::new();
        let res = self.drivers.drain_tcp_reads(&pool, socket, |lid, pkt| {
            packets.push((lid, pkt));
            Ok(())
        });
        for (lid, pkt) in packets {
            if let Err(rc) = self.ingest_quic2_packet(lid, None, pkt, now_us) {
                self.errors += 1;
                self.last_error = rc;
            }
        }
        match res {
            Ok(true) => {
                self.drivers
                    .remove_tcp_socket(socket, &mut *self.slots[0].assoc, now_us);
            }
            Ok(false) => {}
            Err(rc) => {
                self.errors += 1;
                self.last_error = rc;
                self.drivers
                    .remove_tcp_socket(socket, &mut *self.slots[0].assoc, now_us);
            }
        }
    }

    #[inline(never)]
    fn progress(&mut self, now_us: u64) {
        for slot in &mut self.slots {
            if slot.active {
                slot.assoc.set_now_us(now_us);
            }
        }

        // 1. Advance `QuicMux` gap timeouts.
        if self.slots.iter().any(|s| s.active) {
            let mut broken = Vec::new();
            {
                let mut env = CallbackEnv {
                    hw: self.hw,
                    stream_table: self.stream_table,
                    sessions: &mut self.sessions,
                    broken_assocs: &mut broken,
                    errors: &mut self.errors,
                    last_error: &mut self.last_error,
                };
                self.mux
                    .advance_time(now_us, &mut *self.slots[0].assoc, &mut env);
            }
            for bad in broken {
                if bad == self.slots[0].assoc_id {
                    self.reinit_slot(0);
                }
            }
        }

        // 2. Retry streams paused by partial consumer `feed`.
        let paused: Vec<(AssociationId, StreamId)> = self
            .sessions
            .iter()
            .filter(|s| !s.closed && s.needs_resume)
            .map(|s| (s.assoc, s.stream_id))
            .collect();
        for (assoc, sid) in paused {
            let mut broken = Vec::new();
            let res = {
                let mut env = CallbackEnv {
                    hw: self.hw,
                    stream_table: self.stream_table,
                    sessions: &mut self.sessions,
                    broken_assocs: &mut broken,
                    errors: &mut self.errors,
                    last_error: &mut self.last_error,
                };
                self.mux.resume_stream(
                    assoc,
                    sid,
                    now_us,
                    &mut *self.slots[0].assoc,
                    &mut env,
                )
            };
            if res.is_err() {
                self.errors += 1;
                self.last_error = ERR_IO;
            }
            for bad in broken {
                if bad == self.slots[0].assoc_id {
                    self.reinit_slot(0);
                }
            }
        }

        // 3. Pump consumer stream egress (`c_stream_take` -> zero-copy `QuicMux::write_stream` -> `QuicLinkAssoc`).
        if let Some(table) = self.stream_table {
            for s in &mut self.sessions {
                if !s.closed && s.assoc == self.slots[0].assoc_id {
                    if let Err(rc) = s.pump_egress(
                        self.hw,
                        table,
                        &mut *self.mux,
                        &mut *self.slots[0].assoc,
                    ) {
                        self.errors += 1;
                        self.last_error = rc;
                    }
                }
            }
        }

        // 4..7. Flush flow control, poll link timers, evict pressured streams, and emit outbound packets per active slot.
        for idx in 0..MAX_ASSOCS {
            if !self.slots[idx].active {
                continue;
            }
            let assoc_id = self.slots[idx].assoc_id;
            let _ = self
                .mux
                .flush_control(assoc_id, &mut *self.slots[idx].assoc);
            let mut pool = *self.mux.pool();
            let _ = self.slots[idx].assoc.poll_timers(&mut pool, now_us);

            let evicted = self.slots[idx].assoc.take_evicted_streams();
            for (ev_assoc, sid) in evicted {
                let _ = self
                    .mux
                    .reset_stream(ev_assoc, sid, 1, &mut *self.slots[idx].assoc);
                if let Some(s) = self
                    .sessions
                    .iter_mut()
                    .find(|s| !s.closed && s.assoc == ev_assoc && s.stream_id == sid)
                {
                    s.close_native(self.hw, self.stream_table);
                }
            }

            let udp_peer = self.slots[idx].udp_peer;
            let serial_peer = self.slots[idx].serial_peer;
            self.drivers.flush_outbound_for_slot(
                &mut *self.slots[idx].assoc,
                udp_peer,
                serial_peer,
                idx == 0,
                now_us,
            );
        }

        self.sessions.retain(|s| !s.closed);
    }

    fn update_alarm(&mut self, now_us: u64) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        let has_active_sessions = self.sessions.iter().any(|s| !s.closed);
        let mut earliest_dl = self.mux.next_deadline();
        for slot in &self.slots {
            if slot.active {
                if let Some(dl) = slot.assoc.next_deadline_us() {
                    earliest_dl = Some(earliest_dl.map_or(dl, |cur| cur.min(dl)));
                }
            }
        }
        if !self.quic_attached {
            self.alarm = self
                .hw
                .alarm_set(1, 0, 0, ALARM_EXACT, COMPONENT, TICK);
            return;
        }
        if !has_active_sessions && earliest_dl.is_none() {
            return;
        }
        let delay_ms = match (has_active_sessions, earliest_dl) {
            (true, Some(dl)) => {
                let dl_ms = dl.saturating_sub(now_us).div_ceil(1000).max(1) as u32;
                dl_ms.min(ACTIVE_POLL_MS)
            }
            (true, None) => ACTIVE_POLL_MS,
            (false, Some(dl)) => dl.saturating_sub(now_us).div_ceil(1000).max(1) as u32,
            (false, None) => return,
        };
        self.alarm = self
            .hw
            .alarm_set(delay_ms, 0, 0, ALARM_EXACT, COMPONENT, TICK);
    }

    fn tick(&mut self, fallback_us: u32) {
        self.ensure_quic_attached();
        let now_us = self.now_us(fallback_us);
        self.progress(now_us);
        self.update_alarm(now_us);
    }

    fn teardown(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        for idx in 0..MAX_ASSOCS {
            self.reinit_slot(idx);
        }
        self.drivers.close_all();
        self.detach_quic();
    }

    #[inline(never)]
    fn reset(&mut self) {
        if self.alarm >= 0 {
            self.hw.alarm_cancel(self.alarm);
            self.alarm = -1;
        }
        for idx in 0..MAX_ASSOCS {
            self.reinit_slot(idx);
        }
        self.drivers.close_all();
        self.mux = Self::new_mux(self.hw);
        self.slots = [AssocSlot::new(0, self.udp_encrypt, self.serial_encrypt)];
        self.apply_slot_crypto(0, HandshakeMode::None);
        self.next_server_cid = 2;
        self.ensure_quic_attached();
    }
}

#[derive(Default)]
pub struct State {
    context: usize,
}

impl State {
    #[cfg(test)]
    pub(crate) fn configure_crypto(
        &mut self,
        local_public_key: Option<[u8; PUBLIC_KEY_COMPRESSED_LEN]>,
        node_secret: Option<[u8; 32]>,
        send_api_key: Option<[u8; 32]>,
    ) {
        if self.context != 0 {
            let cx = unsafe { &mut *(self.context as *mut Context) };
            cx.local_public_key = local_public_key;
            cx.node_secret = node_secret;
            cx.send_api_key = send_api_key;
            cx.apply_slot_crypto(0, HandshakeMode::None);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_udp_encryption(&mut self, encrypt: bool) {
        if self.context != 0 {
            let cx = unsafe { &mut *(self.context as *mut Context) };
            cx.set_udp_encryption(encrypt);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_serial_encryption(&mut self, encrypt: bool) {
        if self.context != 0 {
            let cx = unsafe { &mut *(self.context as *mut Context) };
            cx.set_serial_encryption(encrypt);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_tcp_encryption(&mut self, encrypt: bool) {
        if self.context != 0 {
            let cx = unsafe { &mut *(self.context as *mut Context) };
            cx.set_tcp_encryption(encrypt);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_cipher(&mut self, cipher: AeadCipher) {
        if self.context != 0 {
            let cx = unsafe { &mut *(self.context as *mut Context) };
            if cx.cipher != cipher {
                cx.cipher = cipher;
                let mode = cx.slots[0].assoc.config().handshake_mode;
                cx.apply_slot_crypto(0, mode);
            }
        }
    }
    fn start(&mut self, hw: &Hw) -> Result<(), i32> {
        if self.context != 0 {
            return Err(ERR_BUSY);
        }
        let ptr = hw
            .mem_alloc(core::mem::size_of::<Context>())
            .cast::<Context>();
        if ptr.is_null() {
            return Err(ERR_NOMEM);
        }
        unsafe {
            ptr.write(Context::new(*hw));
        }
        self.context = ptr as usize;
        Ok(())
    }

    fn stop(&mut self, hw: &Hw) {
        if self.context == 0 {
            return;
        }
        let ptr = self.context as *mut Context;
        self.context = 0;
        unsafe {
            (*ptr).teardown();
            ptr.drop_in_place();
        }
        hw.mem_free(ptr.cast());
    }
}

impl Module for State {
    fn handle_owned(&mut self, hw: &Hw, call: &CallView, packet: Option<Packet>) -> i32 {
        if self.context != 0
            && call.component == COMPONENT
            && (call.method == 0 || call.method == RX)
            && call.flags & CALL_LINK != 0
            && call.flags & CALL_LINK_EVENT == 0
        {
            if let Some(pkt) = packet {
                let cx = unsafe { &mut *(self.context as *mut Context) };
                if let Err(rc) = cx.receive_link_packet(call, pkt) {
                    if rc != -33 {
                        cx.errors += 1;
                        cx.last_error = rc;
                    }
                }
                cx.tick(call.time_us);
                return RET_CONTINUE as i32;
            }
        }
        self.handle(hw, call)
    }

    fn handle(&mut self, hw: &Hw, call: &CallView) -> i32 {
        if call.is_host(HOST_START) {
            return self.start(hw).map_or_else(|e| e, |_| RET_CONTINUE as i32);
        }
        if call.is_host(HOST_STOP) {
            self.stop(hw);
            return RET_CONTINUE as i32;
        }
        if self.context == 0 {
            return RET_CONTINUE as i32;
        }
        let cx = unsafe { &mut *(self.context as *mut Context) };

        if call.flags & CALL_LINK_EVENT != 0 {
            cx.notify_assoc_ready();
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        if call.component == COMPONENT && call.method == TICK && call.flags & CALL_LINK == 0 {
            if cx.alarm >= 0 {
                cx.hw.alarm_cancel(cx.alarm);
                cx.alarm = -1;
            }
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        // Socket readiness / lifecycle events on `--stream 6`.
        if call.component == COMPONENT
            && call.method == 0
            && call.flags & (CALL_LINK | CALL_STREAM) == 0
            && !call.wants_response()
        {
            let now_us = cx.now_us(call.time_us);
            cx.handle_socket_event(call.id as u32, call.value as u32, now_us);
            cx.tick(call.time_us);
            return RET_CONTINUE as i32;
        }

        if call.flags & CALL_LINK != 0 || !call.wants_response() {
            return RET_CONTINUE as i32;
        }

        let mut out = [0u8; 160];
        let mut w = Writer::new(&mut out);
        match call.method {
            1 => {
                cx.ensure_quic_attached();
                let active_assocs = cx.slots.iter().filter(|s| s.active).count() as u64;
                let active_streams = cx.sessions.iter().filter(|s| !s.closed).count() as u64;
                let retained: u64 = cx
                    .slots
                    .iter()
                    .filter(|s| s.active)
                    .map(|s| s.assoc.sack_ledger().retained_packets() as u64)
                    .sum();
                let can_admit = cx.slots[0]
                    .assoc
                    .can_admit_priority(quic_mux::StreamPriority::NORMAL);
                let stream_cap = cx
                    .sessions
                    .iter()
                    .find(|s| !s.closed)
                    .and_then(|s| {
                        cx.mux
                            .stream_send_capacity(s.assoc, s.stream_id, &*cx.slots[0].assoc)
                            .ok()
                    })
                    .map_or(u64::MAX, |c| c.max_chunk_bytes as u64);
                let udp_inflight = cx.slots[0]
                    .assoc
                    .link(LINK_UDP)
                    .map_or(0, |l| l.packets_in_flight as u64);
                w.map(11)
                    .uint(1)
                    .uint(active_assocs)
                    .uint(2)
                    .uint(active_streams)
                    .uint(3)
                    .uint(cx.drivers.tcp_count())
                    .uint(4)
                    .uint(cx.received_packets)
                    .uint(5)
                    .uint(cx.drivers.sent_packets)
                    .uint(6)
                    .uint(retained)
                    .uint(7)
                    .uint(cx.errors)
                    .uint(8)
                    .int(cx.last_error as i64)
                    .uint(9)
                    .boolean(can_admit)
                    .uint(10)
                    .uint(stream_cap)
                    .uint(11)
                    .uint(udp_inflight);
            }
            2 => {
                cx.reset();
                w.map(0);
            }
            3 => {
                if let Err(rc) = cx.configure_cbor(call.data) {
                    w.int(rc as i64);
                } else {
                    let keys_active = cx.node_secret.is_some() || cx.send_api_key.is_some();
                    w.map(5)
                        .uint(1)
                        .boolean(cx.udp_encrypt)
                        .uint(2)
                        .boolean(keys_active)
                        .uint(3)
                        .boolean(cx.serial_encrypt)
                        .uint(4)
                        .boolean(cx.tcp_encrypt)
                        .uint(5)
                        .uint(cx.cipher as u64);
                }
            }
            _ => {
                w.int(ERR_UNSUPPORTED as i64);
            }
        }
        if let Some(reply) = w.finish() {
            hw.reply(call, reply);
        }
        RET_CONTINUE as i32
    }
}

#[macro_export]
macro_rules! export {
    () => {
        xip_abi::export_module!($crate::State, state = $crate::STATE_BYTES);
    };
}

#[cfg(test)]
mod tests;

#[cfg(feature = "sim")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(feature = "sim")]
export!();

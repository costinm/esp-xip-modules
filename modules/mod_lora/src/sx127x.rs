//! SX1276/78 (LoRa mode): register map, probe, RX/TX and CAD.

use crate::hal::{Hal, MAX_PACKET};
use crate::config::LoraConfig;

pub(crate) const SX127X_VERSION: u8 = 0x12;
pub(crate) const REG_VERSION: u8 = 0x42;
pub(crate) const SX127X_REG_BITRATE_MSB: u8 = 0x02;
pub(crate) const SX127X_REG_BITRATE_LSB: u8 = 0x03;
pub(crate) const SX127X_REG_FDEV_MSB: u8 = 0x04;
pub(crate) const SX127X_REG_FDEV_LSB: u8 = 0x05;
pub(crate) const SX127X_REG_PA_CONFIG: u8 = 0x09;
pub(crate) const SX127X_REG_PA_RAMP: u8 = 0x0a;
// SX127x FSK/OOK register bank. These overlap the LoRa bank and are valid
// only after LongRangeMode has been cleared while the chip is asleep.
pub(crate) const SX127X_FSK_REG_RX_CONFIG: u8 = 0x0d;
pub(crate) const SX127X_FSK_REG_RX_BW: u8 = 0x12;
pub(crate) const SX127X_FSK_REG_AFC_BW: u8 = 0x13;
pub(crate) const SX127X_FSK_REG_PREAMBLE_DETECT: u8 = 0x1f;
pub(crate) const SX127X_FSK_REG_PREAMBLE_MSB: u8 = 0x25;
pub(crate) const SX127X_FSK_REG_PREAMBLE_LSB: u8 = 0x26;
pub(crate) const SX127X_REG_SYNC_CONFIG: u8 = 0x27;
pub(crate) const SX127X_REG_SYNC_VALUE: u8 = 0x28;
pub(crate) const SX127X_REG_PACKET_CONFIG1: u8 = 0x30;
pub(crate) const SX127X_REG_PACKET_CONFIG2: u8 = 0x31;
pub(crate) const SX127X_FSK_REG_PAYLOAD_LENGTH: u8 = 0x32;
pub(crate) const SX127X_REG_FIFO_THRESH: u8 = 0x35;
pub(crate) const SX127X_REG_IRQ_FLAGS1: u8 = 0x3e;
pub(crate) const SX127X_REG_DIO_MAPPING1: u8 = 0x40;
pub(crate) const SX127X_REG_IRQ_FLAGS2: u8 = 0x3f;
pub(crate) const SX127X_REG_FIFO: u8 = 0x00;
pub(crate) const SX127X_REG_OP_MODE: u8 = 0x01;
pub(crate) const SX127X_REG_FRF_MSB: u8 = 0x06;
pub(crate) const SX127X_REG_FRF_MID: u8 = 0x07;
pub(crate) const SX127X_REG_FRF_LSB: u8 = 0x08;
pub(crate) const SX127X_REG_FIFO_RX_BASE: u8 = 0x0f;
pub(crate) const SX127X_REG_FIFO_RX_CURRENT: u8 = 0x10;
pub(crate) const SX127X_REG_FIFO_ADDR_PTR: u8 = 0x0d;
pub(crate) const SX127X_REG_IRQ_FLAGS: u8 = 0x12;
pub(crate) const SX127X_REG_RX_BYTES: u8 = 0x13;
pub(crate) const SX127X_REG_PKT_SNR: u8 = 0x19;
pub(crate) const SX127X_REG_PKT_RSSI: u8 = 0x1a;
// In FSK mode RSSI_VALUE is the instantaneous RSSI sample.  Unlike the
// LoRa packet RSSI register it is reported as a positive half-dB magnitude.
pub(crate) const SX127X_REG_FSK_RSSI_VALUE: u8 = 0x24;
pub(crate) const SX127X_REG_MODEM1: u8 = 0x1d;
pub(crate) const SX127X_REG_MODEM2: u8 = 0x1e;
pub(crate) const SX127X_REG_PREAMBLE_MSB: u8 = 0x20;
pub(crate) const SX127X_REG_PREAMBLE_LSB: u8 = 0x21;
pub(crate) const SX127X_REG_MODEM3: u8 = 0x26;
pub(crate) const SX127X_REG_SYNC: u8 = 0x39;
pub(crate) const SX127X_REG_DIO_MAPPING: u8 = 0x40;
pub(crate) const SX127X_LONG_RANGE: u8 = 0x80;
pub(crate) const SX127X_MODE_SLEEP: u8 = 0x00;
pub(crate) const SX127X_MODE_STDBY: u8 = 0x01;
pub(crate) const SX127X_MODE_TX: u8 = 0x03;
pub(crate) const SX127X_MODE_RX_CONTINUOUS: u8 = 0x05;
pub(crate) const SX127X_MODE_RX_SINGLE: u8 = 0x06;
pub(crate) const SX127X_MODE_CAD: u8 = 0x07;
pub(crate) const SX127X_IRQ_RX_DONE: u8 = 0x40;
pub(crate) const SX127X_IRQ_CRC_ERROR: u8 = 0x20;
pub(crate) const SX127X_IRQ_TX_DONE: u8 = 0x08;
pub(crate) const SX127X_IRQ_CAD_DONE: u8 = 0x04;
pub(crate) const SX127X_IRQ_CAD_DETECTED: u8 = 0x01;
pub(crate) const SX127X_FSK_IRQ2_PACKET_SENT: u8 = 0x08;
pub(crate) const SX127X_FSK_IRQ2_PAYLOAD_READY: u8 = 0x04;
pub(crate) const SX127X_FIFO_RX_BASE: u8 = 0;
/// RegDioMapping1 values for DIO0 in LoRa mode: RxDone (0, the default), TxDone, CadDone.
pub(crate) const SX127X_DIO0_TX_DONE: u8 = 0x40;
pub(crate) const SX127X_DIO0_CAD_DONE: u8 = 0x80;

pub(crate) fn probe_sx127x<H: Hal>(host: &mut H) -> i32 {
    let tx = [REG_VERSION & 0x7f, 0];
    let mut rx = [0u8; 2];
    let rc = host.spi(&tx, &mut rx);
    if rc != 0 { return -3; }
    if rx[1] == SX127X_VERSION { 0 } else { -6 }
}

pub(crate) fn sx127x_write<H: Hal>(host: &mut H, reg: u8, value: u8) -> i32 {
    let tx = [reg | 0x80, value];
    let mut rx = [0u8; 2];
    host.spi(&tx, &mut rx)
}

pub(crate) fn sx127x_read<H: Hal>(host: &mut H, reg: u8) -> Result<u8, i32> {
    let tx = [reg & 0x7f, 0];
    let mut rx = [0u8; 2];
    let rc = host.spi(&tx, &mut rx);
    if rc == 0 { Ok(rx[1]) } else { Err(-3) }
}

pub(crate) fn sx127x_burst<H: Hal>(host: &mut H, reg: u8, data: &[u8]) -> i32 {
    if data.len() > MAX_PACKET { return -1; }
    let mut tx = [0u8; MAX_PACKET + 1];
    let mut rx = [0u8; MAX_PACKET + 1];
    tx[0] = reg | 0x80;
    tx[1..data.len() + 1].copy_from_slice(data);
    host.spi(&tx[..data.len() + 1], &mut rx[..data.len() + 1])
}

pub(crate) fn sx127x_read_burst<H: Hal>(host: &mut H, reg: u8, data: &mut [u8]) -> i32 {
    if data.len() > MAX_PACKET { return -1; }
    let mut tx = [0u8; MAX_PACKET + 1];
    let mut rx = [0u8; MAX_PACKET + 1];
    tx[0] = reg & 0x7f;
    let rc = host.spi(&tx[..data.len() + 1], &mut rx[..data.len() + 1]);
    if rc == 0 { data.copy_from_slice(&rx[1..data.len() + 1]); }
    rc
}

pub(crate) fn sx127x_configure_rx<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    if config.chip != 1 { return -2; }
    if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_SLEEP) != 0 { return -3; }
    let frf = ((config.frequency_hz as u64) << 19) / 32_000_000;
    let bw = if config.bandwidth_hz == 7_800 { 0 }
        else if config.bandwidth_hz == 10_400 { 1 }
        else if config.bandwidth_hz == 15_600 { 2 }
        else if config.bandwidth_hz == 20_800 { 3 }
        else if config.bandwidth_hz == 31_250 { 4 }
        else if config.bandwidth_hz == 41_700 { 5 }
        else if config.bandwidth_hz == 62_500 { 6 }
        else if config.bandwidth_hz == 125_000 { 7 }
        else if config.bandwidth_hz == 250_000 { 8 }
        else if config.bandwidth_hz == 500_000 { 9 }
        else { return -1 };
    let cr = config.coding_rate.clamp(4, 8).saturating_sub(4) as u8;
    let sf = config.spreading_factor.clamp(6, 12) as u8;
    let symbol_ms = ((1u64 << sf) * 1000 / config.bandwidth_hz.max(1) as u64) as u32;
    let modem1 = (bw << 4) | (cr << 1); // explicit header
    let modem2 = (sf << 4) | if config.crc != 0 { 0x04 } else { 0 };
    let modem3 = 0x04 | if symbol_ms >= 16 { 0x08 } else { 0 }; // AGC + optional LDRO
    let preamble = config.preamble.clamp(1, 65535) as u16;
    for (reg, value) in [
        (SX127X_REG_FRF_MSB, (frf >> 16) as u8),
        (SX127X_REG_FRF_MID, (frf >> 8) as u8),
        (SX127X_REG_FRF_LSB, frf as u8),
        (SX127X_REG_FIFO_RX_BASE, SX127X_FIFO_RX_BASE),
        (SX127X_REG_MODEM1, modem1),
        (SX127X_REG_MODEM2, modem2),
        (SX127X_REG_MODEM3, modem3),
        (SX127X_REG_PREAMBLE_MSB, (preamble >> 8) as u8),
        (SX127X_REG_PREAMBLE_LSB, preamble as u8),
        (SX127X_REG_SYNC, config.sync_word),
        (SX127X_REG_DIO_MAPPING, 0),
        (SX127X_REG_IRQ_FLAGS, 0xff),
    ] {
        if sx127x_write(host, reg, value) != 0 { return -3; }
    }
    sx127x_write(host, SX127X_REG_OP_MODE,
                 SX127X_LONG_RANGE | if config.cad_rx != 0 {
                     SX127X_MODE_STDBY
                 } else {
                     SX127X_MODE_RX_CONTINUOUS
                 })
}

/// Result of a finished CAD window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CadEnd {
    /// CadDone is not set: the window has not finished (or the read failed).
    NotDone,
    /// The channel is quiet.
    Quiet,
    /// Activity: the chip is (SX126x) or must be put (SX127x) into receive.
    Detected,
    /// SPI failure (the code).
    Error(i32),
}

/// Start one CAD window; CadDone is routed to DIO0.
pub(crate) fn sx127x_cad_begin<H: Hal>(host: &mut H) -> i32 {
    if sx127x_write(host, SX127X_REG_IRQ_FLAGS, 0xff) != 0 { return -3; }
    if sx127x_write(host, SX127X_REG_DIO_MAPPING, SX127X_DIO0_CAD_DONE) != 0 { return -3; }
    sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_CAD)
}

/// Read and clear the CAD flags.
pub(crate) fn sx127x_cad_end<H: Hal>(host: &mut H) -> CadEnd {
    let irq = match sx127x_read(host, SX127X_REG_IRQ_FLAGS) {
        Ok(value) => value,
        Err(error) => return CadEnd::Error(error),
    };
    if sx127x_write(host, SX127X_REG_IRQ_FLAGS, 0xff) != 0 { return CadEnd::Error(-3); }
    if irq & SX127X_IRQ_CAD_DONE == 0 { return CadEnd::NotDone; }
    if irq & SX127X_IRQ_CAD_DETECTED == 0 { return CadEnd::Quiet; }
    CadEnd::Detected
}

/// After a detection: one bounded receive window, RxDone on DIO0.
pub(crate) fn sx127x_rx_single<H: Hal>(host: &mut H) -> i32 {
    if sx127x_write(host, SX127X_REG_DIO_MAPPING, 0) != 0 { return -3; }
    sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_RX_SINGLE)
}

pub(crate) fn sx127x_standby<H: Hal>(host: &mut H) {
    let _ = sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_STDBY);
}

pub(crate) fn sx127x_emit_packet<H: Hal>(host: &mut H) -> i32 {
    let irq = match sx127x_read(host, SX127X_REG_IRQ_FLAGS) { Ok(v) => v, Err(e) => return e };
    if irq & SX127X_IRQ_RX_DONE == 0 { return 1; }
    if sx127x_write(host, SX127X_REG_IRQ_FLAGS, 0xff) != 0 { return -3; }
    if irq & SX127X_IRQ_CRC_ERROR != 0 { return 2; }
    let len = match sx127x_read(host, SX127X_REG_RX_BYTES) { Ok(v) => v as usize, Err(e) => return e };
    if len == 0 || len > MAX_PACKET { return -6; }
    let current = match sx127x_read(host, SX127X_REG_FIFO_RX_CURRENT) { Ok(v) => v, Err(e) => return e };
    // The read pointer goes to the start of the packet (after a transmission it is wherever the FIFO write left it).
    if sx127x_write(host, SX127X_REG_FIFO_ADDR_PTR, current) != 0 { return -3; }
    let mut packet = [0u8; MAX_PACKET];
    if sx127x_read_burst(host, SX127X_REG_FIFO, &mut packet[..len]) != 0 { return -3; }
    let rssi = sx127x_read(host, SX127X_REG_PKT_RSSI).unwrap_or(0) as i16 - 157;
    let snr = sx127x_read(host, SX127X_REG_PKT_SNR).unwrap_or(0) as i8 / 4;
    host.packet(&packet[..len], rssi, snr)
}

/// Load the FIFO and start the transmission; TxDone is routed to DIO0.
pub(crate) fn sx127x_tx_begin<H: Hal>(host: &mut H, packet: &[u8]) -> i32 {
    if packet.is_empty() || packet.len() > MAX_PACKET { return -1; }
    if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_STDBY) != 0 { return -3; }
    if sx127x_write(host, 0x0e, 0) != 0 || sx127x_write(host, 0x0d, 0) != 0 { return -3; }
    if sx127x_burst(host, SX127X_REG_FIFO, packet) != 0 { return -3; }
    if sx127x_write(host, 0x22, packet.len() as u8) != 0 || sx127x_write(host, SX127X_REG_IRQ_FLAGS, 0xff) != 0 { return -3; }
    if sx127x_write(host, SX127X_REG_DIO_MAPPING, SX127X_DIO0_TX_DONE) != 0 { return -3; }
    if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_TX) != 0 { return -3; }
    0
}

/// Has the transmission finished? Clears the flag when it has.
pub(crate) fn sx127x_tx_done<H: Hal>(host: &mut H) -> bool {
    if sx127x_read(host, SX127X_REG_IRQ_FLAGS).unwrap_or(0) & SX127X_IRQ_TX_DONE != 0 {
        let _ = sx127x_write(host, SX127X_REG_IRQ_FLAGS, SX127X_IRQ_TX_DONE);
        let _ = sx127x_write(host, SX127X_REG_DIO_MAPPING, 0);
        return true;
    }
    false
}

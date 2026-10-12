//! SX1261/62 (LoRa mode): command set, probe, RX/TX and CAD.

use crate::hal::{Hal, MAX_PACKET};
use crate::sx127x::CadEnd;
use crate::config::LoraConfig;

pub(crate) const SX126X_GET_STATUS: u8 = 0xC0;
pub(crate) const SX126X_CMD_SET_STANDBY: u8 = 0x80;
pub(crate) const SX126X_CMD_SET_RX: u8 = 0x82;
pub(crate) const SX126X_CMD_SET_CAD: u8 = 0xc5;
pub(crate) const SX126X_CMD_SET_CAD_PARAMS: u8 = 0x88;
pub(crate) const SX126X_CMD_SET_TX: u8 = 0x83;
pub(crate) const SX126X_CMD_SET_FS: u8 = 0xc1;
pub(crate) const SX126X_CMD_SET_PACKET_TYPE: u8 = 0x8a;
pub(crate) const SX126X_CMD_SET_RF_FREQUENCY: u8 = 0x86;
pub(crate) const SX126X_CMD_SET_PA_CONFIG: u8 = 0x95;
pub(crate) const SX126X_CMD_SET_TX_PARAMS: u8 = 0x8e;
pub(crate) const SX126X_CMD_SET_BUFFER_BASE_ADDRESS: u8 = 0x8f;
pub(crate) const SX126X_CMD_SET_MODULATION_PARAMS: u8 = 0x8b;
pub(crate) const SX126X_CMD_SET_PACKET_PARAMS: u8 = 0x8c;
pub(crate) const SX126X_CMD_SET_DIO_IRQ_PARAMS: u8 = 0x08;
pub(crate) const SX126X_CMD_GET_IRQ_STATUS: u8 = 0x12;
pub(crate) const SX126X_CMD_GET_PACKET_TYPE: u8 = 0x11;
pub(crate) const SX126X_CMD_CLEAR_IRQ_STATUS: u8 = 0x02;
pub(crate) const SX126X_CMD_GET_RX_BUFFER_STATUS: u8 = 0x13;
pub(crate) const SX126X_CMD_GET_PACKET_STATUS: u8 = 0x14;
pub(crate) const SX126X_CMD_GET_DEVICE_ERRORS: u8 = 0x17;
pub(crate) const SX126X_CMD_CLEAR_DEVICE_ERRORS: u8 = 0x07;
pub(crate) const SX126X_CMD_WRITE_BUFFER: u8 = 0x0e;
pub(crate) const SX126X_CMD_READ_BUFFER: u8 = 0x1e;
pub(crate) const SX126X_CMD_WRITE_REGISTER: u8 = 0x0d;
pub(crate) const SX126X_CMD_READ_REGISTER: u8 = 0x1d;
pub(crate) const SX126X_CMD_SET_REGULATOR_MODE: u8 = 0x96;
pub(crate) const SX126X_CMD_CALIBRATE: u8 = 0x89;
pub(crate) const SX126X_CMD_CALIBRATE_IMAGE: u8 = 0x98;
pub(crate) const SX126X_CMD_SET_RX_TX_FALLBACK_MODE: u8 = 0x93;
pub(crate) const SX126X_CMD_SET_DIO2_AS_RF_SWITCH: u8 = 0x9d;
pub(crate) const SX126X_CMD_SET_DIO3_AS_TCXO_CTRL: u8 = 0x97;
pub(crate) const SX126X_PACKET_TYPE_GFSK: u8 = 0x00;
pub(crate) const SX126X_PACKET_TYPE_LORA: u8 = 0x01;
pub(crate) const SX126X_IRQ_TX_DONE: u16 = 0x0001;
pub(crate) const SX126X_IRQ_RX_DONE: u16 = 0x0002;
pub(crate) const SX126X_IRQ_CAD_DONE: u16 = 0x0004;
pub(crate) const SX126X_IRQ_CAD_DETECTED: u16 = 0x0008;
pub(crate) const SX126X_IRQ_CRC_ERR: u16 = 0x0040;
pub(crate) const SX126X_IRQ_TIMEOUT: u16 = 0x0200;
pub(crate) const SX126X_REG_SYNC_WORD: u16 = 0x0740;
pub(crate) const SX126X_REG_OCP: u16 = 0x08e7;
pub(crate) const SX126X_REG_RX_GAIN: u16 = 0x08ac;
pub(crate) const SX126X_REG_RX_SENSITIVITY: u16 = 0x08b5;
pub(crate) const SX126X_REG_TX_CLAMP_CONFIG: u16 = 0x08d8;

pub(crate) fn probe_sx126x<H: Hal>(host: &mut H) -> i32 {
    let tx = [SX126X_GET_STATUS, 0];
    let mut rx = [0u8; 2];
    let rc = host.spi(&tx, &mut rx);
    if rc == 0 && rx[1] != 0 { 0 } else if rc == 0 { -6 } else { -3 }
}

pub(crate) fn sx126x_command<H: Hal>(host: &mut H, opcode: u8, data: &[u8]) -> i32 {
    if data.len() > 16 { return -1; }
    let mut tx = [0u8; 18];
    let mut rx = [0u8; 18];
    tx[0] = opcode;
    tx[1..data.len() + 1].copy_from_slice(data);
    host.spi(&tx[..data.len() + 1], &mut rx[..data.len() + 1])
}

pub(crate) fn sx126x_read<H: Hal>(host: &mut H, opcode: u8, args: &[u8], out: &mut [u8]) -> i32 {
    if args.len() > 8 || out.len() > MAX_PACKET { return -1; }
    let mut tx = [0u8; MAX_PACKET + 11];
    let mut rx = [0u8; MAX_PACKET + 11];
    tx[0] = opcode;
    tx[1..args.len() + 1].copy_from_slice(args);
    let total = args.len() + out.len() + 2;
    let rc = host.spi(&tx[..total], &mut rx[..total]);
    if rc == 0 {
        /* GetStatus returns its single status byte immediately after the
         * opcode; the other commands have a leading SPI status byte followed
         * by their response payload. */
        let response_offset = if opcode == SX126X_GET_STATUS { 1 } else { args.len() + 2 };
        out.copy_from_slice(&rx[response_offset..response_offset + out.len()]);
    }
    rc
}

/// First half of the receive configuration, up to the TCXO. With a TCXO the chip needs ~5 ms before
/// [`sx126x_configure_b`].
pub(crate) fn sx126x_configure_a<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    if config.chip != 2 { return -2; }
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_REGULATOR_MODE, &[1]) != 0 { return -3; }
    /* SX1262 reset leaves the analog image and PA path uncalibrated. The
     * production Main driver performs these steps before every radio session;
     * the module must do the same because it owns the complete radio setup. */
    if sx126x_command(host, SX126X_CMD_CALIBRATE, &[0x7f]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CALIBRATE_IMAGE, &[0xe1, 0xe9]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_RX_TX_FALLBACK_MODE, &[0x20]) != 0 { return -3; }
    let mut clamp = [0u8; 1];
    if sx126x_read(host, SX126X_CMD_READ_REGISTER,
                   &[(SX126X_REG_TX_CLAMP_CONFIG >> 8) as u8, SX126X_REG_TX_CLAMP_CONFIG as u8],
                   &mut clamp) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_TX_CLAMP_CONFIG, &[clamp[0] | 0x1e]) != 0 { return -3; }
    if sx126x_set_tcxo(host, config.sx1262_tcxo_mv) != 0 { return -1; }
    0
}

/// Second half: packet engine, modulation, IRQ routing and the start of RX (or of the first CAD window).
pub(crate) fn sx126x_configure_b<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    // XOSC_START_ERR is latched at POR when a TCXO is used and is explicitly
    // documented as expected on the first wake. Clear the stale latch before
    // starting the packet engine so the post-TX diagnostic reflects a new
    // failure, not the boot-time condition.
    if sx126x_command(host, SX126X_CMD_CLEAR_DEVICE_ERRORS, &[0, 0]) != 0 { return -3; }
    if config.sx1262_dio2_rf_switch != 0 &&
        sx126x_command(host, SX126X_CMD_SET_DIO2_AS_RF_SWITCH, &[1]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_PACKET_TYPE, &[SX126X_PACKET_TYPE_LORA]) != 0 { return -3; }
    let rf = ((config.frequency_hz as u64) << 25) / 32_000_000;
    if sx126x_command(host, SX126X_CMD_SET_RF_FREQUENCY, &[(rf >> 24) as u8, (rf >> 16) as u8, (rf >> 8) as u8, rf as u8]) != 0 { return -3; }
    let pa_duty = config.sx1262_pa_duty.clamp(0, 7) as u8;
    let pa_hp = config.sx1262_pa_hp.clamp(0, 7) as u8;
    let pa_device = config.sx1262_pa_device.clamp(0, 1) as u8;
    let pa_lut = config.sx1262_pa_lut.clamp(0, 1) as u8;
    if sx126x_command(host, SX126X_CMD_SET_PA_CONFIG, &[pa_duty, pa_hp, pa_device, pa_lut]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_TX_PARAMS, &[((config.tx_power as i32).clamp(-9, 22) as i8) as u8, 4]) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_OCP, &[0x38]) != 0 { return -3; }
    let sync = config.sx1262_sync_word.clamp(0, 0xffff) as u16;
    if sx126x_write_register(host, SX126X_REG_SYNC_WORD, &[(sync >> 8) as u8, sync as u8]) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_RX_GAIN, &[0x96]) != 0 { return -3; }
    let mut sensitivity = [0u8; 1];
    if sx126x_read(host, SX126X_CMD_READ_REGISTER,
                   &[((SX126X_REG_RX_SENSITIVITY >> 8) & 0xff) as u8, SX126X_REG_RX_SENSITIVITY as u8],
                   &mut sensitivity) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_RX_SENSITIVITY, &[sensitivity[0] | 1]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_BUFFER_BASE_ADDRESS, &[0, 0x80]) != 0 { return -3; }
    // Keep this as a branch chain. The flat Xtensa image is linked without
    // relocations; a large integer `match` becomes an absolute jump table in
    // .rodata, which is invalid after flash-MMU mapping at a different base.
    let bw = if config.bandwidth_hz == 7_800 { 0 }
        else if config.bandwidth_hz == 10_400 { 8 }
        else if config.bandwidth_hz == 15_600 { 1 }
        else if config.bandwidth_hz == 20_800 { 9 }
        else if config.bandwidth_hz == 31_250 { 2 }
        else if config.bandwidth_hz == 41_700 { 10 }
        else if config.bandwidth_hz == 62_500 { 3 }
        else if config.bandwidth_hz == 125_000 { 4 }
        else if config.bandwidth_hz == 250_000 { 5 }
        else if config.bandwidth_hz == 500_000 { 6 }
        else { return -1 };
    let sf = config.spreading_factor.clamp(5, 12) as u8;
    let cr = config.coding_rate.clamp(4, 8).saturating_sub(4) as u8;
    let symbol_ms = ((1u64 << sf) * 1000 / config.bandwidth_hz.max(1) as u64) as u32;
    let ldro = if symbol_ms >= 16 { 1 } else { 0 };
    if sx126x_command(host, SX126X_CMD_SET_MODULATION_PARAMS, &[sf, bw, cr, ldro]) != 0 { return -3; }
    let preamble = config.preamble.clamp(1, 65535) as u16;
    if sx126x_command(host, SX126X_CMD_SET_PACKET_PARAMS,
                      &[(preamble >> 8) as u8, preamble as u8, 0,
                        255, if config.crc != 0 { 1 } else { 0 }, 0]) != 0 { return -3; }
    let mut irq = SX126X_IRQ_TX_DONE | SX126X_IRQ_RX_DONE | SX126X_IRQ_CRC_ERR | SX126X_IRQ_TIMEOUT;
    if config.cad_rx != 0 { irq |= SX126X_IRQ_CAD_DONE | SX126X_IRQ_CAD_DETECTED; }
    if sx126x_command(host, SX126X_CMD_SET_DIO_IRQ_PARAMS, &[(irq >> 8) as u8, irq as u8, (irq >> 8) as u8, irq as u8, 0, 0, 0, 0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]) != 0 { return -3; }
    if config.cad_rx == 0 {
        sx126x_command(host, SX126X_CMD_SET_RX, &[0xff, 0xff, 0xff])
    } else {
        /* Use the modem's CAD state machine instead of SetRxDutyCycle. The
         * latter is not reliable on the deployed SX1262 board (it can leave
         * DIO1 idle even with a full receive window). CAD keeps the radio in
         * its low-power detector, and we explicitly enter a bounded RX window
         * only after a preamble is detected. */
        if sx126x_command(host, SX126X_CMD_SET_CAD_PARAMS,
                          /* 4-symbol CAD, then enter RX immediately when a
                           * preamble is detected. Returning to standby and
                           * issuing SetRx afterwards loses that preamble. */
                          &[0x02, 0x1a, 0x0a, 0x01, 0xff, 0xff, 0xff]) != 0 { return -3; }
        sx126x_command(host, SX126X_CMD_SET_CAD, &[])
    }
}

/// Both halves at once, for a chip without a TCXO and for the tests.
#[cfg(test)]
pub(crate) fn sx126x_configure_rx<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    let rc = sx126x_configure_a(host, config);
    if rc != 0 { return rc; }
    sx126x_configure_b(host, config)
}

/// Start one CAD window (the chip enters RX by itself when it detects a preamble).
pub(crate) fn sx126x_cad_begin<H: Hal>(host: &mut H) -> i32 {
    if sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]) != 0 { return -3; }
    /* CAD exits to STDBY_RC on a quiet window. Re-enter standby explicitly
     * and publish the CAD parameters before every new operation so a timeout
     * or previous RX transition cannot leave the command rejected. */
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_CAD_PARAMS,
                      &[0x02, 0x1a, 0x0a, 0x01, 0xff, 0xff, 0xff]) != 0 { return -3; }
    sx126x_command(host, SX126X_CMD_SET_CAD, &[])
}

/// What a finished (or timed out) CAD window says; the extra value is the diagnostic status of `NotDone`.
pub(crate) fn sx126x_cad_end<H: Hal>(host: &mut H) -> (CadEnd, i32) {
    let mut irq_bytes = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut irq_bytes) != 0 { return (CadEnd::Error(-3), 0); }
    let irq = u16::from_be_bytes(irq_bytes);
    if irq & SX126X_IRQ_CAD_DONE == 0 {
        let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
        let mut status = [0u8; 1];
        let diag = if sx126x_read(host, SX126X_GET_STATUS, &[], &mut status) == 0 { 0x100 | status[0] as i32 } else { 11 };
        return (CadEnd::NotDone, diag);
    }
    if irq & SX126X_IRQ_CAD_DETECTED == 0 {
        let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
        return (CadEnd::Quiet, 0);
    }
    /* CAD_RX has already switched the modem into RX. Clear only the CAD
     * completion bits so the subsequent RxDone remains observable. */
    let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0x00, (SX126X_IRQ_CAD_DONE | SX126X_IRQ_CAD_DETECTED) as u8]);
    (CadEnd::Detected, 0)
}

pub(crate) fn sx126x_emit_packet<H: Hal>(host: &mut H) -> i32 {
    let mut irq_bytes = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut irq_bytes) != 0 { return -3; }
    let irq = u16::from_be_bytes(irq_bytes);
    if irq & SX126X_IRQ_RX_DONE == 0 { return 1; }
    let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
    if irq & SX126X_IRQ_CRC_ERR != 0 { return 2; }
    let mut status = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_RX_BUFFER_STATUS, &[], &mut status) != 0 { return -3; }
    let len = status[0] as usize;
    if len == 0 || len > MAX_PACKET { return -6; }
    let mut packet = [0u8; MAX_PACKET];
    if sx126x_read(host, SX126X_CMD_READ_BUFFER, &[status[1]], &mut packet[..len]) != 0 { return -3; }
    let mut stat = [0u8; 3];
    let _ = sx126x_read(host, SX126X_CMD_GET_PACKET_STATUS, &[], &mut stat);
    let rssi = -(stat[0] as i16) / 2;
    let snr = (stat[1] as i8) / 4;
    host.packet(&packet[..len], rssi, snr)
}

/// What a running transmission is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxPoll {
    Pending,
    Done,
    /// The modem timed out (or the read failed): the code to report, the chip is back in standby.
    Failed(i32),
}

/// Load the buffer and start the transmission (TxDone and Timeout are routed to DIO1).
pub(crate) fn sx126x_tx_begin<H: Hal>(host: &mut H, config: &LoraConfig, packet: &[u8]) -> i32 {
    if packet.is_empty() || packet.len() > MAX_PACKET { return -1; }
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]) != 0 { return -3; }
    let preamble = config.preamble.clamp(1, 65535) as u16;
    if sx126x_command(host, SX126X_CMD_SET_PACKET_PARAMS,
                      &[(preamble >> 8) as u8, preamble as u8, 0,
                        packet.len() as u8, if config.crc != 0 { 1 } else { 0 }, 0]) != 0 { return -3; }
    let mut tx = [0u8; MAX_PACKET + 2];
    let mut rx = [0u8; MAX_PACKET + 2];
    tx[0] = SX126X_CMD_WRITE_BUFFER;
    tx[1] = 0;
    tx[2..packet.len() + 2].copy_from_slice(packet);
    if host.spi(&tx[..packet.len() + 2], &mut rx[..packet.len() + 2]) != 0 { return -3; }
    let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
    if sx126x_command(host, SX126X_CMD_SET_FS, &[]) != 0 { return -3; }
    // SX126x timeout units are 15.625 us; use the device's maximum bounded
    // timeout so a failed LoRa TX cannot leave the modem in an unbounded state.
    if sx126x_command(host, SX126X_CMD_SET_TX, &[0xff, 0xff, 0xff]) != 0 { return -3; }
    0
}

/// Look at the IRQ status of a running transmission.
pub(crate) fn sx126x_tx_poll<H: Hal>(host: &mut H) -> TxPoll {
    let mut bytes = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut bytes) != 0 { return TxPoll::Pending; }
    let irq = u16::from_be_bytes(bytes);
    if irq & SX126X_IRQ_TX_DONE != 0 {
        let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
        return TxPoll::Done;
    }
    if irq & SX126X_IRQ_TIMEOUT != 0 {
        let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
        let _ = sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]);
        return TxPoll::Failed(-1000 - irq as i32);
    }
    TxPoll::Pending
}

/// A transmission that never finished: back to standby with the last IRQ bits as the diagnostic.
pub(crate) fn sx126x_tx_abort<H: Hal>(host: &mut H) -> i32 {
    let mut bytes = [0u8; 2];
    let last = if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut bytes) == 0 { u16::from_be_bytes(bytes) } else { 0 };
    let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
    let _ = sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]);
    -1000 - last as i32
}

pub(crate) fn sx126x_write_register<H: Hal>(host: &mut H, address: u16, value: &[u8]) -> i32 {
    if value.len() > 8 { return -1; }
    let mut tx = [0u8; 11];
    let mut rx = [0u8; 11];
    tx[0] = SX126X_CMD_WRITE_REGISTER;
    tx[1] = (address >> 8) as u8;
    tx[2] = address as u8;
    tx[3..value.len() + 3].copy_from_slice(value);
    host.spi(&tx[..value.len() + 3], &mut rx[..value.len() + 3])
}

pub(crate) fn sx126x_set_tcxo<H: Hal>(host: &mut H, millivolts: i32) -> i32 {
    /* Keep this as a branch chain: the Xtensa flat-image lane must not gain a
     * literal jump table whose addresses depend on the mapping base. */
    let voltage = if millivolts == 0 { return 0 }
        // SX126x TCXO voltage codes use 1.5 V as code 0, then the table
        // advances through 1.6, 1.7, 1.8, 2.2, 2.4, 2.7, and 3.0 V.
        else if millivolts == 1500 { 0 }
        else if millivolts == 1600 { 1 }
        else if millivolts == 1700 { 2 }
        else if millivolts == 1800 { 3 }
        else if millivolts == 2200 { 4 }
        else if millivolts == 2400 { 5 }
        else if millivolts == 2700 { 6 }
        else if millivolts == 3000 { 7 }
        else { return -1 };
    sx126x_command(host, SX126X_CMD_SET_DIO3_AS_TCXO_CTRL,
                   &[voltage, 0x00, 0x03, 0x20])
}


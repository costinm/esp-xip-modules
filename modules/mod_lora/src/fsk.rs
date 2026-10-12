//! The chip-specific FSK packet path (100 kbit/s GFSK), SX127x and SX126x.

use crate::config::LoraConfig;
use crate::hal::{Hal, Next, MAX_PACKET};
use crate::sx126x::*;
use crate::sx127x::*;

/// FSK setup of the SX127x as steps: reset (10 + 20 ms), two mode switches (1 ms each), then the registers.
pub(crate) fn sx127x_fsk_setup_step<H: Hal>(host: &mut H, config: &LoraConfig, step: u8) -> Next {
    let mut step = step;
    loop {
        match step {
            // Main may have left the shared SX127x in LoRa RX or an incomplete previous FSK state. A short
            // module-owned reset makes the modem-bank transition deterministic.
            0 => {
                if config.reset_pin < 0 { step = 3; continue; }
                let _ = host.gpio_write(config.reset_pin as i32, 0);
                return Next::Wait(10, 1);
            }
            1 => {
                let _ = host.gpio_write(config.reset_pin as i32, 1);
                return Next::Wait(20, 3);
            }
            // LongRangeMode can only be changed while asleep. Follow the reference two-step switch and verify the
            // mode before touching FSK registers.
            3 => {
                if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_LONG_RANGE | SX127X_MODE_SLEEP) != 0 { return Next::Done(-310); }
                return Next::Wait(1, 4);
            }
            4 => {
                if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_MODE_SLEEP) != 0 { return Next::Done(-311); }
                return Next::Wait(1, 5);
            }
            _ => return Next::Done(sx127x_fsk_registers(host, config)),
        }
    }
}

fn sx127x_fsk_registers<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    if sx127x_read(host, SX127X_REG_OP_MODE).map(|value| value & SX127X_LONG_RANGE != 0).unwrap_or(true) { return -312; }
    let frf = ((config.frequency_hz as u64) << 19) / 32_000_000;
    for (index, (reg, value)) in [
        (SX127X_REG_FRF_MSB, (frf >> 16) as u8),
        (SX127X_REG_FRF_MID, (frf >> 8) as u8),
        (SX127X_REG_FRF_LSB, frf as u8),
        (SX127X_REG_BITRATE_MSB, 0x01), // 100 kbit/s
        (SX127X_REG_BITRATE_LSB, 0x40),
        (SX127X_REG_FDEV_MSB, 0x00),
        (SX127X_REG_FDEV_LSB, 0x52),
        (SX127X_FSK_REG_RX_BW, 0x01),
        (SX127X_FSK_REG_AFC_BW, 0x01),
        (SX127X_FSK_REG_PREAMBLE_MSB, 0x00),
        (SX127X_FSK_REG_PREAMBLE_LSB, 0x10),
        (SX127X_FSK_REG_PREAMBLE_DETECT, 0xaa),
        (SX127X_REG_SYNC_CONFIG, 0x11), // sync enabled, two sync bytes
        (SX127X_REG_SYNC_VALUE, 0xd3),
        (SX127X_REG_SYNC_VALUE + 1, 0xa5),
        // Variable packet profile: the FIFO begins with the payload length.
        (SX127X_REG_PACKET_CONFIG1, 0x90),
        // PacketConfig2 bit 6 selects the FIFO packet engine. The former
        // 0x00 selected continuous data mode and never framed RX packets.
        (SX127X_REG_PACKET_CONFIG2, 0x40),
        (SX127X_FSK_REG_PAYLOAD_LENGTH, 0xff),
        (SX127X_REG_FIFO_THRESH, 0x8f),
        (SX127X_FSK_REG_RX_CONFIG, 0x1e),
        (SX127X_REG_DIO_MAPPING1, 0x34), // DIO0=FSK ready, DIO1/2 unused
        (SX127X_REG_PA_RAMP, 0x29), // Gaussian BT=0.5, 40 us ramp
        (SX127X_REG_PA_CONFIG, 0x8f), // 17 dBm, same persisted LoRa power
    ].into_iter().enumerate() {
        let tx = [reg | 0x80, value];
        let mut rx = [0u8; 2];
        if host.spi(&tx, &mut rx) != 0 { return -300 - index as i32; }
    }
    // Leave the radio in a valid FSK standby state.  Entering RX/TX directly
    // from Sleep is not accepted reliably by all SX127x revisions.
    if sx127x_write(host, SX127X_REG_OP_MODE, SX127X_MODE_STDBY) != 0 { return -313; }
    0
}

/// Load the FIFO and enter FSTx; after 1 ms [`sx127x_fsk_send_go`] starts the transmission.
pub(crate) fn sx127x_fsk_send_begin<H: Hal>(host: &mut H, packet: &[u8]) -> i32 {
    if packet.is_empty() || packet.len() > MAX_PACKET { return -1; }
    let current_mode = sx127x_read(host, SX127X_REG_OP_MODE).unwrap_or(0);
    // Preserve the SX127x low-frequency oscillator bit, but explicitly clear
    // LongRangeMode and modulation-type bits before entering FSK TX.
    let fsk_base = current_mode & !0xe7;
    if sx127x_write(host, SX127X_REG_OP_MODE, fsk_base | SX127X_MODE_STDBY) != 0 { return -3; }
    // Use the reference byte-at-a-time FIFO sequence. It keeps each FIFO
    // write explicit across the ESP-IDF SPI wrapper and avoids relying on a
    // burst transaction's CS/FIFO auto-increment behavior in FSK mode.
    if sx127x_write(host, SX127X_FSK_REG_PAYLOAD_LENGTH, packet.len() as u8) != 0 { return -3; }
    // Variable-length FSK packets carry the payload length as the first FIFO
    // byte; SX127x removes it before exposing the received payload.
    if sx127x_write(host, SX127X_REG_FIFO, packet.len() as u8) != 0 { return -3; }
    for &byte in packet {
        if sx127x_write(host, SX127X_REG_FIFO, byte) != 0 { return -3; }
    }
    if sx127x_write(host, SX127X_REG_DIO_MAPPING1, 0x34) != 0 { return -3; }
    // IRQ_FLAGS2 status bits are read/clear; the reference FSK TX path leaves
    // them untouched before starting the packet engine.
    // Give the synthesizer an explicit FSTx phase before the packet engine
    // enters Tx; this avoids a stuck FIFO-empty state on some SX127x lots.
    if sx127x_write(host, SX127X_REG_OP_MODE, fsk_base | 0x02) != 0 { return -3; }
    0
}

pub(crate) fn sx127x_fsk_send_go<H: Hal>(host: &mut H) -> i32 {
    let current_mode = sx127x_read(host, SX127X_REG_OP_MODE).unwrap_or(0);
    let fsk_base = current_mode & !0xe7;
    if sx127x_write(host, SX127X_REG_OP_MODE, fsk_base | SX127X_MODE_TX) != 0 { return -3; }
    0
}

/// Has the packet been sent? Back to standby when it has.
pub(crate) fn sx127x_fsk_sent<H: Hal>(host: &mut H) -> bool {
    if let Ok(flags) = sx127x_read(host, SX127X_REG_IRQ_FLAGS2) {
        if flags & SX127X_FSK_IRQ2_PACKET_SENT != 0 {
            let _ = sx127x_write(host, SX127X_REG_OP_MODE, SX127X_MODE_STDBY);
            return true;
        }
    }
    false
}

/// After the setup: DIO0 = PayloadReady, flags cleared, continuous receive.
pub(crate) fn sx127x_fsk_rx_enter<H: Hal>(host: &mut H) -> i32 {
    if sx127x_write(host, SX127X_REG_DIO_MAPPING1, 0x00) != 0 { return -321; }
    if sx127x_write(host, SX127X_REG_IRQ_FLAGS2, 0xff) != 0 { return -322; }
    let current_mode = sx127x_read(host, SX127X_REG_OP_MODE).unwrap_or(0);
    if sx127x_write(host, SX127X_REG_OP_MODE, (current_mode & !0xe7) | SX127X_MODE_RX_CONTINUOUS) != 0 { return -320; }
    0
}

pub(crate) fn sx127x_fsk_emit_packet<H: Hal>(host: &mut H) -> i32 {
    let flags = match sx127x_read(host, SX127X_REG_IRQ_FLAGS2) { Ok(v) => v, Err(e) => return e };
    if flags & SX127X_FSK_IRQ2_PAYLOAD_READY == 0 { return 1; }
    let len = 8;
    let mut packet = [0u8; MAX_PACKET];
    if sx127x_read_burst(host, SX127X_REG_FIFO, &mut packet[..len]) != 0 { return -3; }
    /* IRQ_FLAGS2 is write-one-to-clear on SX127x FSK.  Clear PayloadReady
     * after draining the FIFO; otherwise the polling loop emits the same
     * frame repeatedly and Main counts a storm of duplicate packets. */
    let _ = sx127x_write(host, SX127X_REG_IRQ_FLAGS2, 0xff);
    // FSK has no SNR measurement.  Preserve the measured RSSI in the common
    // callback so Main can put it in the ESP-NOW receive envelope instead of
    // silently reporting zero for every FSK frame.
    let rssi = sx127x_read(host, SX127X_REG_FSK_RSSI_VALUE)
        // SX127x FSK RSSI_VALUE has 0.5 dB units, unlike the LoRa
        // RegPktRssiValue register used by the LoRa path.
        .map(|raw| -((raw as i32) / 2))
        .unwrap_or(0);
    host.packet(&packet[..len], rssi.clamp(i16::MIN as i32, i16::MAX as i32) as i16, 0)
}

/// FSK setup of the SX126x as steps: up to the TCXO, then (after 5 ms with a TCXO) the rest.
pub(crate) fn sx126x_fsk_setup_step<H: Hal>(host: &mut H, config: &LoraConfig, step: u8) -> Next {
    if step == 0 {
        let rc = sx126x_fsk_setup_a(host, config);
        if rc != 0 { return Next::Done(rc); }
        return if config.sx1262_tcxo_mv > 0 { Next::Wait(5, 1) } else { Next::Done(sx126x_fsk_setup_b(host, config)) };
    }
    Next::Done(sx126x_fsk_setup_b(host, config))
}

fn sx126x_fsk_setup_a<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_REGULATOR_MODE, &[1]) != 0 { return -3; }
    // The TCXO must be running before calibration and packet-engine setup.
    // Leave the radio in its normal RC standby; the validated SX1262 FSK
    // sequence does not switch to XOSC standby between these commands.
    if sx126x_set_tcxo(host, config.sx1262_tcxo_mv) != 0 { return -1; }
    0
}

fn sx126x_fsk_setup_b<H: Hal>(host: &mut H, config: &LoraConfig) -> i32 {
    if sx126x_command(host, SX126X_CMD_CALIBRATE, &[0x7f]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CALIBRATE_IMAGE, &[0xe1, 0xe9]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_RX_TX_FALLBACK_MODE, &[0x20]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CLEAR_DEVICE_ERRORS, &[0, 0]) != 0 { return -3; }
    if config.sx1262_dio2_rf_switch != 0 &&
        sx126x_command(host, SX126X_CMD_SET_DIO2_AS_RF_SWITCH, &[1]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_PACKET_TYPE, &[SX126X_PACKET_TYPE_GFSK]) != 0 { return -3; }
    let rf = ((config.frequency_hz as u64) << 25) / 32_000_000;
    if sx126x_command(host, SX126X_CMD_SET_RF_FREQUENCY,
                      &[(rf >> 24) as u8, (rf >> 16) as u8, (rf >> 8) as u8, rf as u8]) != 0 { return -3; }
    let pa_duty = config.sx1262_pa_duty.clamp(0, 7) as u8;
    let pa_hp = config.sx1262_pa_hp.clamp(0, 7) as u8;
    let pa_device = config.sx1262_pa_device.clamp(0, 1) as u8;
    let pa_lut = config.sx1262_pa_lut.clamp(0, 1) as u8;
    if sx126x_command(host, SX126X_CMD_SET_PA_CONFIG, &[pa_duty, pa_hp, pa_device, pa_lut]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_TX_PARAMS,
                      &[((config.tx_power as i32).clamp(-9, 22) as i8) as u8, 4]) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_OCP, &[0x38]) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_RX_GAIN, &[0x96]) != 0 { return -3; }
    let mut sensitivity = [0u8; 1];
    if sx126x_read(host, SX126X_CMD_READ_REGISTER,
                   &[(SX126X_REG_RX_SENSITIVITY >> 8) as u8, SX126X_REG_RX_SENSITIVITY as u8],
                   &mut sensitivity) != 0 { return -3; }
    if sx126x_write_register(host, SX126X_REG_RX_SENSITIVITY, &[sensitivity[0] | 1]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_BUFFER_BASE_ADDRESS, &[0, 0x80]) != 0 { return -3; }
    // Common 100 kbit/s FSK/GFSK profile, 5 kHz deviation, and a wide enough
    // receive filter for both SX127x and SX1262. SX126x encodes bitrate as
    // (32 * 32 MHz) / bitrate, so 100 kbit/s is 0x002800.
    if sx126x_command(host, SX126X_CMD_SET_MODULATION_PARAMS,
                      // No Gaussian shaping for the first interop profile;
                      // this is the same SX127x FSK setting and avoids a
                      // modem-side waveform mismatch while TX is validated.
                      &[0x00, 0x28, 0x00, 0x00, 0x0a, 0x00, 0x14, 0x7b]) != 0 { return -3; }
    // Match the SX127x FSK profile's two-byte sync word and standard
    // two-byte CCITT CRC. SX127x uses polynomial 0x1021, init 0x1d0f, with the
    // complemented output. Program these explicitly because SX1262 leaves
    // the GFSK CRC registers configurable across radio resets.
    if sx126x_write_register(host, 0x06c0, &[0xd3, 0xa5]) != 0 { return -3; }
    if sx126x_write_register(host, 0x06bc, &[0x1d, 0x0f]) != 0 { return -3; }
    if sx126x_write_register(host, 0x06be, &[0x10, 0x21]) != 0 { return -3; }
    // SX1262 encodes GFSK preamble length in bits. Match the SX127x 16-byte
    // preamble and leave the detector ungated for cross-chip interoperability.
    if sx126x_command(host, SX126X_CMD_SET_PACKET_PARAMS,
                      &[0, 128, 0x00, 16, 0, 1, 0xff, 6, 0]) != 0 { return -3; }
    let irq = SX126X_IRQ_TX_DONE | SX126X_IRQ_RX_DONE | SX126X_IRQ_CRC_ERR | SX126X_IRQ_TIMEOUT;
    sx126x_command(host, SX126X_CMD_SET_DIO_IRQ_PARAMS,
                   &[(irq >> 8) as u8, irq as u8, (irq >> 8) as u8, irq as u8, 0, 0, 0, 0])
}

/// After the setup: only RX_DONE and timeout reach the pin (CRC_ERR is read from the status; at this low FSK rate a
/// noisy CRC latch could hold DIO1 high), flags cleared, SetRx.
pub(crate) fn sx126x_fsk_rx_enter<H: Hal>(host: &mut H) -> i32 {
    let irq = SX126X_IRQ_RX_DONE | SX126X_IRQ_TIMEOUT;
    if sx126x_command(host, SX126X_CMD_SET_DIO_IRQ_PARAMS,
                      &[(irq >> 8) as u8, irq as u8, (irq >> 8) as u8, irq as u8, 0, 0, 0, 0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]) != 0 { return -3; }
    let mut cleared_irq = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut cleared_irq) == 0 {
        let value = u16::from_be_bytes(cleared_irq);
        if value != 0 { return -2000 - value as i32; }
    }
    if sx126x_command(host, SX126X_CMD_SET_RX, &[0xff, 0xff, 0xff]) != 0 { return -3; }
    0
}

/* After RX_DONE the SX1262 enters its configured fallback mode. Re-running
 * the complete setup sequence is unnecessarily fragile while the TCXO state
 * is transitioning. Packet parameters and DIO IRQ routing are unchanged, so
 * a persistent receiver only needs XOSC standby, IRQ clearing, and SetRx. */
pub(crate) fn sx126x_fsk_rearm_rx<H: Hal>(host: &mut H) -> i32 {
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[1]) != 0 { return -3; }
    // Restore the interoperable variable-length receive profile after TX.
    if sx126x_command(host, SX126X_CMD_SET_PACKET_PARAMS,
                      &[0, 128, 0x00, 16, 0, 1, 0xff, 6, 0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_SET_RX, &[0xff, 0xff, 0xff]) != 0 { return -3; }
    0
}

/// Load the FIFO and start the transmission of a GFSK packet.
pub(crate) fn sx126x_fsk_tx_begin<H: Hal>(host: &mut H, packet: &[u8]) -> i32 {
    if packet.is_empty() || packet.len() > MAX_PACKET { return -1; }
    if sx126x_command(host, SX126X_CMD_SET_STANDBY, &[0]) != 0 { return -3; }
    let mut packet_type = [0u8; 1];
    if sx126x_read(host, SX126X_CMD_GET_PACKET_TYPE, &[], &mut packet_type) != 0 { return -4; }
    if packet_type[0] != SX126X_PACKET_TYPE_GFSK { return -400 - packet_type[0] as i32; }
    // SX1262 variable-length TX: the first FIFO byte is the payload length.
    if sx126x_command(host, SX126X_CMD_SET_PACKET_PARAMS,
                      &[0, 128, 0x00, 16, 0, 1, packet.len() as u8, 6, 0]) != 0 { return -3; }
    let mut tx = [0u8; MAX_PACKET + 2];
    let mut rx = [0u8; MAX_PACKET + 2];
    tx[0] = SX126X_CMD_WRITE_BUFFER;
    tx[1] = 0;
    tx[2] = packet.len() as u8;
    tx[3..packet.len() + 3].copy_from_slice(packet);
    if host.spi(&tx[..packet.len() + 3], &mut rx[..packet.len() + 3]) != 0 { return -3; }
    let tx_irq = SX126X_IRQ_TX_DONE | SX126X_IRQ_TIMEOUT;
    if sx126x_command(host, SX126X_CMD_SET_DIO_IRQ_PARAMS,
                      &[(tx_irq >> 8) as u8, tx_irq as u8, (tx_irq >> 8) as u8, tx_irq as u8, 0, 0, 0, 0]) != 0 { return -3; }
    if sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]) != 0 { return -3; }
    // SetTx performs the synthesizer transition itself; the maximum bounded timeout (15.625 us units) covers a
    // maximum-size 100 kbit/s frame.
    if sx126x_command(host, SX126X_CMD_SET_TX, &[0xff, 0xff, 0xff]) != 0 { return -3; }
    0
}

pub(crate) fn sx126x_fsk_emit_packet<H: Hal>(host: &mut H) -> i32 {
    let mut irq_bytes = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_IRQ_STATUS, &[], &mut irq_bytes) != 0 { return -3; }
    let irq = u16::from_be_bytes(irq_bytes);
    if irq & SX126X_IRQ_RX_DONE == 0 { return 1; }
    let _ = sx126x_command(host, SX126X_CMD_CLEAR_IRQ_STATUS, &[0xff, 0xff]);
    let mut packet_status = [0u8; 3];
    let _ = sx126x_read(host, SX126X_CMD_GET_PACKET_STATUS, &[], &mut packet_status);
    if irq & SX126X_IRQ_CRC_ERR != 0 {
        // GFSK packet-status byte 0 distinguishes preamble, sync, address,
        // CRC, length, and abort failures. Preserve it for the task's
        // diagnostic event instead of collapsing every failure to `2`.
        return 0x1000 | packet_status[0] as i32;
    }
    let mut status = [0u8; 2];
    if sx126x_read(host, SX126X_CMD_GET_RX_BUFFER_STATUS, &[], &mut status) != 0 { return -3; }
    let len = status[0] as usize;
    if len == 0 || len > MAX_PACKET { return -6; }
    let mut packet = [0u8; MAX_PACKET];
    if sx126x_read(host, SX126X_CMD_READ_BUFFER, &[status[1]], &mut packet[..len]) != 0 { return -3; }
    // SX126x GFSK packet status reports RSSI in half-dB units.  There is no
    // FSK SNR value, so keep SNR at zero while carrying the real RSSI.
    let rssi = -((packet_status[2] as i32) / 2);
    host.packet(&packet[..len], rssi.clamp(i16::MIN as i32, i16::MAX as i32) as i16, 0)
}



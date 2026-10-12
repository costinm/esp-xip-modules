//! Radio and board configuration. It arrives only with a request (start or `reconfigure`); the module keeps
//! the current copy on its stack. Defaults are the TTGO T-Beam/LoRa32 SX127x wiring and US MediumFast.

/// The field id of a text request name (`rx freq=915000000 sf=9`; the ids are those of the CBOR request map and of
/// `API.md`: 1..30 and 32; 0 and 31 are not settings). A chain of comparisons, not a table of strings: a table of
/// pointers is `.data.rel.ro` in a position independent image (ESP32-C6), which the build refuses.
pub fn field_id(name: &[u8]) -> Option<u8> {
    if name == b"chip" { return Some(1); }
    if name == b"freq" { return Some(2); }
    if name == b"bw" { return Some(3); }
    if name == b"sf" { return Some(4); }
    if name == b"spi" { return Some(5); }
    if name == b"sync" { return Some(6); }
    if name == b"power" { return Some(7); }
    if name == b"rst" { return Some(8); }
    if name == b"cs" { return Some(9); }
    if name == b"irq" { return Some(10); }
    if name == b"busy" { return Some(11); }
    if name == b"sck" { return Some(12); }
    if name == b"miso" { return Some(13); }
    if name == b"mosi" { return Some(14); }
    if name == b"pwr" { return Some(15); }
    if name == b"pwrlvl" { return Some(16); }
    if name == b"dio2" { return Some(17); }
    if name == b"tcxo" { return Some(18); }
    if name == b"pa_duty" { return Some(19); }
    if name == b"pa_hp" { return Some(20); }
    if name == b"pa_dev" { return Some(21); }
    if name == b"pa_lut" { return Some(22); }
    if name == b"sx_sync" { return Some(23); }
    if name == b"rx_timeout" { return Some(24); }
    if name == b"cr" { return Some(25); }
    if name == b"preamble" { return Some(26); }
    if name == b"crc" { return Some(27); }
    if name == b"cad" { return Some(28); }
    if name == b"cad_interval" { return Some(29); }
    if name == b"cad_rx_ms" { return Some(30); }
    if name == b"cad_period" { return Some(32); }
    None
}

pub const CHIP_SX127X: u32 = 1;
pub const CHIP_SX126X: u32 = 2;

#[derive(Clone, Copy)]
pub struct LoraConfig {
    pub chip: u32,
    pub frequency_hz: u32,
    pub bandwidth_hz: u32,
    pub spreading_factor: u32,
    /// SPI controller number (2 or 3).
    pub spi_bus: i32,
    pub sync_word: u8,
    pub tx_power: u8,
    pub reset_pin: i8,
    pub cs_pin: i8,
    pub irq_pin: i8,
    pub busy_pin: i8,
    pub sck_pin: i8,
    pub miso_pin: i8,
    pub mosi_pin: i8,
    pub board_power_pin: i32,
    pub board_power_level: i32,
    pub sx1262_dio2_rf_switch: i32,
    pub sx1262_tcxo_mv: i32,
    pub sx1262_pa_duty: i32,
    pub sx1262_pa_hp: i32,
    pub sx1262_pa_device: i32,
    pub sx1262_pa_lut: i32,
    pub sx1262_sync_word: i32,
    pub sx1262_rx_timeout_ms: i32,
    pub coding_rate: i32,
    pub preamble: i32,
    pub crc: i32,
    pub cad_rx: i32,
    pub cad_interval_ms: u32,
    pub cad_rx_ms: u32,
    /// Pause between two CAD windows (the radio sleeps, the CPU is idle), ms.
    pub cad_period_ms: u32,
}

impl LoraConfig {
    pub const DEFAULT: LoraConfig = LoraConfig {
        chip: CHIP_SX127X, frequency_hz: 913_125_000, bandwidth_hz: 250_000, spreading_factor: 10,
        spi_bus: 3, sync_word: 0x2b, tx_power: 17, reset_pin: 14, cs_pin: 18, irq_pin: 26, busy_pin: -1,
        sck_pin: 5, miso_pin: 19, mosi_pin: 27, board_power_pin: -1, board_power_level: 1,
        sx1262_dio2_rf_switch: 0, sx1262_tcxo_mv: 0, sx1262_pa_duty: 4, sx1262_pa_hp: 7, sx1262_pa_device: 0,
        sx1262_pa_lut: 1, sx1262_sync_word: 0x24b4, sx1262_rx_timeout_ms: 0, coding_rate: 5, preamble: 16,
        crc: 1, cad_rx: 0, cad_interval_ms: 2_000, cad_rx_ms: 1_000, cad_period_ms: 25,
    };

    /// Apply one field; `None` for an unknown id or a value that does not fit.
    pub fn set(&mut self, id: u8, value: i64) -> Option<()> {
        let v32 = i32::try_from(value).ok();
        let u32v = u32::try_from(value).ok();
        let pin = i8::try_from(value).ok();
        match id {
            1 => self.chip = u32v?,
            2 => self.frequency_hz = u32v?,
            3 => self.bandwidth_hz = u32v?,
            4 => self.spreading_factor = u32v?,
            5 => self.spi_bus = v32?,
            6 => self.sync_word = u8::try_from(value).ok()?,
            7 => self.tx_power = u8::try_from(value).ok()?,
            8 => self.reset_pin = pin?,
            9 => self.cs_pin = pin?,
            10 => self.irq_pin = pin?,
            11 => self.busy_pin = pin?,
            12 => self.sck_pin = pin?,
            13 => self.miso_pin = pin?,
            14 => self.mosi_pin = pin?,
            15 => self.board_power_pin = v32?,
            16 => self.board_power_level = v32?,
            17 => self.sx1262_dio2_rf_switch = v32?,
            18 => self.sx1262_tcxo_mv = v32?,
            19 => self.sx1262_pa_duty = v32?,
            20 => self.sx1262_pa_hp = v32?,
            21 => self.sx1262_pa_device = v32?,
            22 => self.sx1262_pa_lut = v32?,
            23 => self.sx1262_sync_word = v32?,
            24 => self.sx1262_rx_timeout_ms = v32?,
            25 => self.coding_rate = v32?,
            26 => self.preamble = v32?,
            27 => self.crc = v32?,
            28 => self.cad_rx = v32?,
            29 => self.cad_interval_ms = u32v?,
            30 => self.cad_rx_ms = u32v?,
            32 => self.cad_period_ms = u32v?,
            _ => return None,
        }
        Some(())
    }
}

impl Default for LoraConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

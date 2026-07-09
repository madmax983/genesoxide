//! Serial EEPROM (I2C 24Cxx) cartridge save support.
//!
//! A number of Genesis/Mega Drive cartridges back their saves with a small
//! serial EEPROM (Microchip 24Cxx family) wired to two 68000 address lines
//! (SDA = serial data, SCL = serial clock) instead of parallel battery SRAM.
//! The CPU bit-bangs the I2C protocol by reading/writing individual bits at a
//! mapped cartridge address; the chip auto-increments an internal word address.
//!
//! This module re-implements the I2C protocol as an original Rust state machine
//! and holds the factual per-title hardware tables (chip sizes, mapper line
//! wiring, and the ROM-serial database) needed to drive it. The bus-facing API
//! deliberately mirrors [`crate::api::CartSram`]: [`Eeprom::read`] returns
//! `Option<u8>` (so a miss falls through to ROM) and [`Eeprom::write`] returns
//! whether it consumed the access.

use crate::rom::RomHeader;
use serde::{Deserialize, Serialize};

/// A supported serial EEPROM chip. Each variant carries the chip's addressable
/// size, the number of significant address bits, and the page-write wrap mask.
///
/// The 24Cxx family comes in "mode 1" (7-bit word address, single address
/// phase — the classic X24C01) and "mode 2" (device-address byte then an 8-bit
/// word address — 24C02 and larger) flavours; `address_bits() == 7` selects the
/// mode-1 sequence. Values match the Genesis Plus GX `i2c_specs` table (hardware
/// facts, not copied code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EepromType {
    /// 128 bytes, 7-bit word address (mode 1), 4-byte page.
    X24C01,
    /// 256 bytes, 8-bit word address (mode 2), 4-byte page.
    X24C02,
    /// 512 bytes, 8-bit word address + 1 device-address block bit, 16-byte page.
    X24C04,
    /// 1024 bytes, 8-bit word address + 2 block bits, 16-byte page.
    X24C08,
    /// 2048 bytes, 8-bit word address + 3 block bits, 16-byte page.
    X24C16,
    /// 8192 bytes, 16-bit word address (mode 3), 32-byte page.
    X24C64,
}

impl EepromType {
    /// Total addressable size in bytes.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            EepromType::X24C01 => 128,
            EepromType::X24C02 => 256,
            EepromType::X24C04 => 512,
            EepromType::X24C08 => 1024,
            EepromType::X24C16 => 2048,
            EepromType::X24C64 => 8192,
        }
    }

    /// Address-latch width. 7 selects the mode-1 (X24C01) protocol; 8 the
    /// mode-2 device+word-address protocol; 16 the mode-3 two-byte word address.
    #[must_use]
    pub fn address_bits(self) -> u8 {
        match self {
            EepromType::X24C01 => 7,
            EepromType::X24C02
            | EepromType::X24C04
            | EepromType::X24C08
            | EepromType::X24C16 => 8,
            EepromType::X24C64 => 16,
        }
    }

    /// Mask of in-array address bits (`size - 1`); the word address wraps to
    /// this on sequential reads.
    #[must_use]
    pub fn size_mask(self) -> u16 {
        (self.size() as u16).wrapping_sub(1)
    }

    /// Page-write wrap mask: only these low word-address bits increment during a
    /// write burst; higher bits stay fixed, so a page write wraps within a page.
    #[must_use]
    pub fn pagewrite_mask(self) -> u16 {
        match self {
            EepromType::X24C01 | EepromType::X24C02 => 0x03,
            EepromType::X24C04 | EepromType::X24C08 | EepromType::X24C16 => 0x0F,
            EepromType::X24C64 => 0x1F,
        }
    }
}

/// Cartridge board / mapper that wires the EEPROM to the 68000 bus. The variant
/// selects which addresses and bit lanes carry SDA/SCL (see [`LineConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EepromMapper {
    /// SEGA boards (171-5878 etc). SCL=D1, SDA(in/out)=D0 at odd $200000-$3FFFFF.
    Sega,
    /// EA boards (PWA P10003/P10004). SCL=D6, SDA(in/out)=D7 at odd $200000-$3FFFFF.
    Ea,
    /// Acclaim 16Mbit board (670120). SDA-in=D0/SCL=D1 (any lane), SDA-out=D1 (odd).
    AcclaimOld,
    /// Acclaim 32Mbit board (670125/670127). SDA=D0 on odd, SCL=D0 on even,
    /// SDA-out=D0 odd, window $200000-$2FFFFF.
    AcclaimNew,
    /// Codemasters J-CART boards. Write window $300000-$37FFFF (SDA=D0/SCL=D1),
    /// read window $380000-$3FFFFF (SDA-out=D7, odd).
    Codemasters,
}

/// Which 68000 byte lane a line responds on within its address window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lane {
    /// Any address in the window (word-handler boards that ignore /LWR//UWR).
    Any,
    /// Odd addresses only (low byte / D0-D7 lane).
    Odd,
    /// Even addresses only (high byte lane).
    Even,
}

impl Lane {
    /// Whether `addr` is on this lane.
    fn matches(self, addr: u32) -> bool {
        match self {
            Lane::Any => true,
            Lane::Odd => addr & 1 == 1,
            Lane::Even => addr & 1 == 0,
        }
    }
}

/// Wiring of a single I2C line: the address window it decodes, the byte lane it
/// responds on, and the data-bit position carrying the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinePin {
    /// Inclusive window start (68000 address space).
    pub start: u32,
    /// Inclusive window end.
    pub end: u32,
    /// Byte lane the line responds on.
    pub lane: Lane,
    /// Data bit (0-7) carrying the signal.
    pub bit: u8,
}

impl LinePin {
    /// True if `addr` decodes to this pin.
    fn matches(self, addr: u32) -> bool {
        addr >= self.start && addr <= self.end && self.lane.matches(addr)
    }
}

/// The three I2C lines for a mapper: serial-data in (host→chip), serial-data out
/// (chip→host), and serial clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineConfig {
    /// SDA as driven by the CPU (write path).
    pub sda_in: LinePin,
    /// SDA as read back from the chip (read path).
    pub sda_out: LinePin,
    /// SCL clock (write path).
    pub scl: LinePin,
}

/// One serial-EEPROM database entry: a ROM-serial fragment to match, plus the
/// chip and mapper that title uses. Serial fragments are matched by substring
/// against the header serial (matching Genesis Plus GX's `strstr` behaviour),
/// so leading/trailing framing (e.g. "GM ", trailing region digits) does not
/// defeat the lookup.
struct DbEntry {
    /// ROM-serial fragment (as printed in the header product code).
    serial: &'static str,
    /// EEPROM chip fitted.
    chip: EepromType,
    /// Cartridge mapper wiring.
    mapper: EepromMapper,
}

/// Serial-EEPROM game database. Serials and chip/mapper assignments are
/// hardware facts taken from the Genesis Plus GX `i2c_database` table.
///
/// Note on chip mapping: this core exposes one [`EepromType`] per capacity, so
/// GPGX's mode-1 vs mode-2 24C02 split and its 24C65 both fold onto the nearest
/// capacity variant (24C02 -> `X24C02`, 24C65 -> `X24C64`). This preserves
/// size/address-mode behaviour; only the page-write granularity of those few
/// titles differs slightly from silicon.
const GAME_DB: &[DbEntry] = &[
    // --- EA mapper (X24C01) ---
    DbEntry { serial: "T-50176", chip: EepromType::X24C01, mapper: EepromMapper::Ea }, // Rings of Power
    DbEntry { serial: "T-50396", chip: EepromType::X24C01, mapper: EepromMapper::Ea }, // NHLPA Hockey 93
    DbEntry { serial: "T-50446", chip: EepromType::X24C01, mapper: EepromMapper::Ea }, // John Madden Football 93
    DbEntry { serial: "T-50516", chip: EepromType::X24C01, mapper: EepromMapper::Ea }, // Madden 93 Championship
    DbEntry { serial: "T-50606", chip: EepromType::X24C01, mapper: EepromMapper::Ea }, // Bill Walsh College Football
    // --- SEGA mapper (X24C01) ---
    DbEntry { serial: "T-12046", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Megaman - The Wily Wars
    DbEntry { serial: "T-12053", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Rockman Mega World
    DbEntry { serial: "MK-1215", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Evander Holyfield's Boxing
    DbEntry { serial: "MK-1228", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Greatest Heavyweights (U/E)
    DbEntry { serial: "G-5538", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Greatest Heavyweights (J)
    DbEntry { serial: "PR-1993", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Greatest Heavyweights (Proto)
    DbEntry { serial: "G-4060", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Wonder Boy in Monster World
    DbEntry { serial: "00001211", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Sports Talk Baseball
    DbEntry { serial: "00004076", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Honoo no Toukyuuji Dodge Danpei
    DbEntry { serial: "G-4524", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Ninja Burai Densetsu
    DbEntry { serial: "00054503", chip: EepromType::X24C01, mapper: EepromMapper::Sega }, // Game Toshokan
    // --- Acclaim 16M mapper (X24C02) ---
    DbEntry { serial: "T-81033", chip: EepromType::X24C02, mapper: EepromMapper::AcclaimOld }, // NBA Jam (J)
    DbEntry { serial: "T-081326", chip: EepromType::X24C02, mapper: EepromMapper::AcclaimOld }, // NBA Jam (UE)
    // --- Acclaim 32M mapper ---
    DbEntry { serial: "T-081276", chip: EepromType::X24C02, mapper: EepromMapper::AcclaimNew }, // NFL Quarterback Club (24C02)
    DbEntry { serial: "T-81406", chip: EepromType::X24C04, mapper: EepromMapper::AcclaimNew }, // NBA Jam TE (24C04)
    DbEntry { serial: "T-081586", chip: EepromType::X24C16, mapper: EepromMapper::AcclaimNew }, // NFL QB Club '96 (24C16)
    DbEntry { serial: "T-81476", chip: EepromType::X24C64, mapper: EepromMapper::AcclaimNew }, // Frank Thomas Big Hurt (24C65)
    DbEntry { serial: "T-81576", chip: EepromType::X24C64, mapper: EepromMapper::AcclaimNew }, // College Slam (24C65)
    // --- Codemasters J-CART mapper ---
    DbEntry { serial: "T-120106", chip: EepromType::X24C08, mapper: EepromMapper::Codemasters }, // Brian Lara Cricket (24C08)
    DbEntry { serial: "T-120096", chip: EepromType::X24C16, mapper: EepromMapper::Codemasters }, // Micro Machines 2 (24C16)
    DbEntry { serial: "T-120146", chip: EepromType::X24C64, mapper: EepromMapper::Codemasters }, // Brian Lara Cricket 96 (24C65)
];

impl EepromMapper {
    /// Standard SEGA-style whole-cartridge EEPROM window.
    const CART_LO: u32 = 0x20_0000;
    const CART_HI: u32 = 0x3F_FFFF;

    /// The I2C line wiring for this mapper. Address/bit values are hardware
    /// facts extracted from Genesis Plus GX's mapper init routines.
    #[must_use]
    pub fn line_config(self) -> LineConfig {
        match self {
            // SEGA: SCL->D1, SDA(in/out)->D0, odd lane, $200000-$3FFFFF.
            EepromMapper::Sega => LineConfig {
                sda_in: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 0 },
                sda_out: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 0 },
                scl: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 1 },
            },
            // EA: SCL->D6, SDA(in/out)->D7, odd lane, $200000-$3FFFFF.
            EepromMapper::Ea => LineConfig {
                sda_in: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 7 },
                sda_out: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 7 },
                scl: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 6 },
            },
            // Acclaim 16M: write via word handler (any lane) SDA=D0/SCL=D1,
            // read SDA-out=D1 on odd lane.
            EepromMapper::AcclaimOld => LineConfig {
                sda_in: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Any, bit: 0 },
                sda_out: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Odd, bit: 1 },
                scl: LinePin { start: Self::CART_LO, end: Self::CART_HI, lane: Lane::Any, bit: 1 },
            },
            // Acclaim 32M: /LWR (odd) carries SDA=D0, /UWR (even) carries SCL=D0,
            // read SDA-out=D0 odd. Window $200000-$2FFFFF (ROM bankshift NYI).
            EepromMapper::AcclaimNew => LineConfig {
                sda_in: LinePin { start: Self::CART_LO, end: 0x2F_FFFF, lane: Lane::Odd, bit: 0 },
                sda_out: LinePin { start: Self::CART_LO, end: 0x2F_FFFF, lane: Lane::Odd, bit: 0 },
                scl: LinePin { start: Self::CART_LO, end: 0x2F_FFFF, lane: Lane::Even, bit: 0 },
            },
            // Codemasters J-CART: write window $300000-$37FFFF (any lane)
            // SDA=D0/SCL=D1; read window $380000-$3FFFFF SDA-out=D7 odd.
            EepromMapper::Codemasters => LineConfig {
                sda_in: LinePin { start: 0x30_0000, end: 0x37_FFFF, lane: Lane::Any, bit: 0 },
                sda_out: LinePin { start: 0x38_0000, end: 0x3F_FFFF, lane: Lane::Odd, bit: 7 },
                scl: LinePin { start: 0x30_0000, end: 0x37_FFFF, lane: Lane::Any, bit: 1 },
            },
        }
    }
}

/// I2C transfer phase of the EEPROM state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum I2cState {
    /// Idle; waiting for a START condition.
    Standby,
    /// Transfer finished (e.g. read NACK); waiting for a STOP.
    WaitStop,
    /// Latching the device-address byte (mode 2/3).
    GetDeviceAddr,
    /// Latching the 7-bit word address + R/W (mode 1, X24C01).
    GetWordAddr7,
    /// Latching the high byte of a 16-bit word address (mode 3).
    GetWordAddrHigh,
    /// Latching the low byte of the word address (mode 2/3).
    GetWordAddrLow,
    /// Streaming data out to the host.
    ReadData,
    /// Streaming data in from the host.
    WriteData,
}

/// A cartridge serial EEPROM plus its live I2C bit-bang state.
///
/// Mirrors the `CartSram` bus contract: [`read`](Self::read) returns
/// `Option<u8>` so a non-EEPROM address falls through to ROM, and
/// [`write`](Self::write) returns whether it consumed the access. An absent
/// EEPROM ([`Eeprom::empty`]) never claims any access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eeprom {
    /// Chip fitted, or `None` when the cartridge has no serial EEPROM.
    chip: Option<EepromType>,
    /// Board wiring.
    mapper: EepromMapper,
    /// Backing store (length == chip size, empty when absent).
    mem: Vec<u8>,
    /// Current SDA line level.
    sda: bool,
    /// Current SCL line level.
    scl: bool,
    /// SDA level at the previous I2C tick (for edge detection).
    old_sda: bool,
    /// SCL level at the previous I2C tick.
    old_scl: bool,
    /// Bit/cycle counter within the current byte phase (0-9).
    cycles: u8,
    /// True when the master requested a read (R/W bit == 1).
    rw: bool,
    /// Device-address block bits, pre-shifted into high word-address bits.
    device_address: u16,
    /// Current word (memory) address.
    word_address: u16,
    /// Byte being assembled during a write.
    buffer: u8,
    /// Current transfer phase.
    state: I2cState,
    /// Set whenever `mem` is modified; cleared by the host after a flush.
    dirty: bool,
}

impl Eeprom {
    /// An absent EEPROM (used before a ROM is loaded or for non-EEPROM carts).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            chip: None,
            mapper: EepromMapper::Sega,
            mem: Vec::new(),
            sda: true,
            scl: true,
            old_sda: true,
            old_scl: true,
            cycles: 0,
            rw: false,
            device_address: 0,
            word_address: 0,
            buffer: 0,
            state: I2cState::Standby,
            dirty: false,
        }
    }

    /// Builds EEPROM state for a freshly loaded ROM by matching its header
    /// serial against the game database. Returns [`Eeprom::empty`] on no match.
    #[must_use]
    pub fn for_rom(header: &RomHeader) -> Self {
        match Self::lookup(&header.serial) {
            Some((chip, mapper)) => Self::new(chip, mapper),
            None => Self::empty(),
        }
    }

    /// Constructs a present EEPROM of the given chip and mapper (zero-filled).
    #[must_use]
    pub fn new(chip: EepromType, mapper: EepromMapper) -> Self {
        Self {
            chip: Some(chip),
            mapper,
            mem: vec![0; chip.size()],
            ..Self::empty()
        }
    }

    /// Tolerant database lookup: succeeds if any database serial fragment is a
    /// substring of the (whitespace/NUL-trimmed) header serial, mirroring the
    /// reference emulator's substring match.
    fn lookup(serial: &str) -> Option<(EepromType, EepromMapper)> {
        let needle = serial.trim().trim_matches('\0').trim();
        GAME_DB
            .iter()
            .find(|e| needle.contains(e.serial))
            .map(|e| (e.chip, e.mapper))
    }

    /// True if a serial EEPROM is fitted.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.chip.is_some()
    }

    /// Effective backing index for the current device/word address.
    fn mem_index(&self) -> usize {
        let addr = (self.device_address | self.word_address) as usize;
        // All chip sizes are powers of two, so mask to stay in range.
        addr & (self.mem.len().wrapping_sub(1))
    }

    /// Returns the bit the chip currently drives on SDA (mirrors the reference
    /// `eeprom_i2c_out`).
    fn sda_out(&self) -> u8 {
        if self.state == I2cState::ReadData {
            if self.cycles < 9 {
                let byte = self.mem[self.mem_index()];
                return (byte >> (8 - self.cycles)) & 1;
            }
        } else if self.cycles == 9 {
            // ACK cycle: chip pulls SDA low.
            return 0;
        }
        u8::from(self.sda)
    }

    /// Reads a byte if `addr` decodes to this EEPROM's SDA-out line; otherwise
    /// `None` so the bus falls through to ROM.
    #[must_use]
    pub fn read(&self, addr: u32) -> Option<u8> {
        let chip = self.chip?;
        let cfg = self.mapper.line_config();
        let _ = chip;
        if cfg.sda_out.matches(addr) {
            Some(self.sda_out() << cfg.sda_out.bit)
        } else {
            None
        }
    }

    /// Decodes SCL/SDA from a byte write and advances the I2C state machine.
    /// Returns `true` if the write targeted an EEPROM line (and was consumed).
    pub fn write(&mut self, addr: u32, val: u8) -> bool {
        if self.chip.is_none() {
            return false;
        }
        let cfg = self.mapper.line_config();
        let mut hit = false;
        if cfg.scl.matches(addr) {
            self.scl = (val >> cfg.scl.bit) & 1 != 0;
            hit = true;
        }
        if cfg.sda_in.matches(addr) {
            self.sda = (val >> cfg.sda_in.bit) & 1 != 0;
            hit = true;
        }
        if hit {
            self.update();
        }
        hit
    }

    /// The chip's backing bytes (empty when absent).
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.mem
    }

    /// Loads persisted bytes into the EEPROM. Copies up to the chip size; does
    /// not mark dirty (the on-disk copy is current).
    pub fn load(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.mem.len());
        self.mem[..n].copy_from_slice(&bytes[..n]);
        self.dirty = false;
    }

    /// True if the EEPROM has been written since the last [`clear_dirty`](Self::clear_dirty).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag (call after flushing the save to disk).
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// True if this EEPROM holds data worth persisting (i.e. it is present).
    #[must_use]
    pub fn worth_saving(&self) -> bool {
        self.chip.is_some()
    }

    /// Detects an I2C START: SDA high→low while SCL stays high.
    fn detect_start(&mut self) -> bool {
        if self.old_scl && self.scl && self.old_sda && !self.sda {
            self.cycles = 0;
            if self.address_bits() == 7 {
                self.word_address = 0;
                self.state = I2cState::GetWordAddr7;
            } else {
                self.device_address = 0;
                self.state = I2cState::GetDeviceAddr;
            }
            true
        } else {
            false
        }
    }

    /// Detects an I2C STOP: SDA low→high while SCL stays high.
    fn detect_stop(&mut self) -> bool {
        if self.old_scl && self.scl && !self.old_sda && self.sda {
            self.state = I2cState::Standby;
            true
        } else {
            false
        }
    }

    /// Address width of the fitted chip (0 if absent).
    fn address_bits(&self) -> u8 {
        self.chip.map_or(0, EepromType::address_bits)
    }

    fn size_mask(&self) -> u16 {
        self.chip.map_or(0, EepromType::size_mask)
    }

    fn pagewrite_mask(&self) -> u16 {
        self.chip.map_or(0, EepromType::pagewrite_mask)
    }

    /// True on an SCL rising edge (data sampled here).
    fn scl_rising(&self) -> bool {
        !self.old_scl && self.scl
    }

    /// True on an SCL falling edge (phase advance).
    fn scl_falling(&self) -> bool {
        self.old_scl && !self.scl
    }

    /// Advances the I2C state machine by one bus tick, then latches the new
    /// SCL/SDA levels as the previous state for the next edge comparison.
    fn update(&mut self) {
        match self.state {
            I2cState::Standby => {
                self.detect_start();
            }
            I2cState::WaitStop => {
                self.detect_stop();
            }
            I2cState::GetWordAddr7 => self.step_word_addr7(),
            I2cState::GetDeviceAddr => self.step_device_addr(),
            I2cState::GetWordAddrHigh => self.step_word_addr_high(),
            I2cState::GetWordAddrLow => self.step_word_addr_low(),
            I2cState::ReadData => self.step_read(),
            I2cState::WriteData => self.step_write(),
        }
        self.old_scl = self.scl;
        self.old_sda = self.sda;
    }

    /// Mode-1 (X24C01): 7 word-address bits + R/W bit in a single byte.
    fn step_word_addr7(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.cycles = 1;
                self.state = if self.rw { I2cState::ReadData } else { I2cState::WriteData };
                self.buffer = 0;
            }
        } else if self.scl_rising() {
            if self.cycles < 8 {
                self.word_address |= u16::from(self.sda) << (7 - self.cycles);
            } else if self.cycles == 8 {
                self.rw = self.sda;
            }
        }
    }

    /// Mode-2/3: device-address byte (1010 + block bits + R/W).
    fn step_device_addr(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.device_address <<= self.address_bits();
                self.cycles = 1;
                if self.rw {
                    self.state = I2cState::ReadData;
                } else {
                    self.word_address = 0;
                    self.state = if self.address_bits() == 16 {
                        I2cState::GetWordAddrHigh
                    } else {
                        I2cState::GetWordAddrLow
                    };
                }
            }
        } else if self.scl_rising() {
            if self.cycles > 4 && self.cycles < 8 {
                // Block-select bits become the high word-address bits.
                self.device_address |= u16::from(self.sda) << (7 - self.cycles);
            } else if self.cycles == 8 {
                self.rw = self.sda;
            }
        }
    }

    /// Mode-3: high byte of a 16-bit word address.
    fn step_word_addr_high(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.cycles = 1;
                self.state = I2cState::GetWordAddrLow;
            }
        } else if self.scl_rising() && self.cycles < 9 {
            if self.size_mask() < (1u16 << (16 - self.cycles)) {
                self.device_address >>= 1;
            } else {
                self.word_address |= u16::from(self.sda) << (16 - self.cycles);
            }
        }
    }

    /// Mode-2/3: low byte of the word address (7 bits for X24C01-style, 8 else).
    fn step_word_addr_low(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.cycles = 1;
                self.state = I2cState::WriteData;
                self.buffer = 0;
            }
        } else if self.scl_rising() && self.cycles < 9 {
            if self.size_mask() < (1u16 << (8 - self.cycles)) {
                self.device_address >>= 1;
            } else {
                self.word_address |= u16::from(self.sda) << (8 - self.cycles);
            }
        }
    }

    /// Read burst: on ACK the master either continues (auto-increment) or NACKs
    /// to end the transfer.
    fn step_read(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.cycles = 1;
            }
        } else if self.scl_rising() && self.cycles == 9 {
            if self.sda {
                // NACK: master ends the read.
                self.state = I2cState::WaitStop;
            } else {
                // ACK: auto-increment (wraps at the whole array).
                self.word_address = (self.word_address + 1) & self.size_mask();
            }
        }
    }

    /// Write burst: assemble a byte, commit on the 9th (ACK) cycle, then advance
    /// the word address within the current page.
    fn step_write(&mut self) {
        if self.detect_start() || self.detect_stop() {
            return;
        }
        if self.scl_falling() {
            if self.cycles < 9 {
                self.cycles += 1;
            } else {
                self.cycles = 1;
            }
        } else if self.scl_rising() {
            if self.cycles < 9 {
                self.buffer |= u8::from(self.sda) << (8 - self.cycles);
            } else {
                let idx = self.mem_index();
                self.mem[idx] = self.buffer;
                self.dirty = true;
                self.buffer = 0;
                // Increment only the in-page low bits (page-write wrap).
                let page = self.pagewrite_mask();
                self.word_address =
                    (self.word_address & !page) | ((self.word_address + 1) & page);
            }
        }
    }
}

impl Default for Eeprom {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Odd byte of the SEGA-mapper EEPROM window; SCL=D1, SDA=D0.
    const SEGA_ADDR: u32 = 0x20_0001;

    /// Drives one SEGA-mapper bus write with the given SCL/SDA levels.
    fn wr(e: &mut Eeprom, scl: u8, sda: u8, addr: u32) {
        e.write(addr, (scl << 1) | sda);
    }

    /// I2C START on the SEGA mapper. Leaves SCL low, ready to clock.
    fn start(e: &mut Eeprom) {
        wr(e, 1, 1, SEGA_ADDR);
        wr(e, 1, 0, SEGA_ADDR); // SDA high->low while SCL high
        wr(e, 0, 0, SEGA_ADDR); // drop SCL: enter address phase (cycles=1)
    }

    /// I2C STOP on the SEGA mapper.
    fn stop(e: &mut Eeprom) {
        wr(e, 0, 0, SEGA_ADDR);
        wr(e, 1, 0, SEGA_ADDR);
        wr(e, 1, 1, SEGA_ADDR); // SDA low->high while SCL high
    }

    /// Clocks one bit into the chip (set while SCL low, sample on rising edge).
    fn send_bit(e: &mut Eeprom, b: u8) {
        wr(e, 0, b, SEGA_ADDR);
        wr(e, 1, b, SEGA_ADDR); // rising edge: chip latches
        wr(e, 0, b, SEGA_ADDR); // falling edge: advance cycle
    }

    /// Mode-1 addressing: 7 address bits (MSB first), R/W bit, then the ACK
    /// clock that transitions into the data phase.
    fn send_addr7(e: &mut Eeprom, addr: u8, rw: u8) {
        for i in 0..7 {
            send_bit(e, (addr >> (6 - i)) & 1);
        }
        send_bit(e, rw); // R/W bit
        send_bit(e, 1); // ACK clock -> data phase
    }

    /// Clocks a data byte into the chip (write burst) plus the commit clock.
    fn send_data(e: &mut Eeprom, d: u8) {
        for i in 0..8 {
            send_bit(e, (d >> (7 - i)) & 1);
        }
        send_bit(e, 1); // 9th clock: chip commits the byte
    }

    /// Reads one data byte during a read burst; `cont` selects ACK (continue)
    /// vs NACK (end) on the 9th clock.
    fn read_byte(e: &mut Eeprom, cont: bool) -> u8 {
        let mut v = 0u8;
        for _ in 0..8 {
            let bit = e.read(SEGA_ADDR).unwrap() & 1;
            v = (v << 1) | bit;
            wr(e, 1, 1, SEGA_ADDR); // rising (data held high by master)
            wr(e, 0, 1, SEGA_ADDR); // falling: advance to next output bit
        }
        let a = if cont { 0 } else { 1 };
        wr(e, 0, a, SEGA_ADDR);
        wr(e, 1, a, SEGA_ADDR); // rising @ cycle 9: chip samples ACK/NACK
        wr(e, 0, a, SEGA_ADDR);
        v
    }

    /// Writes `byte` to `word` on a mode-1 (X24C01) SEGA-mapper chip.
    fn i2c_write(e: &mut Eeprom, word: u8, byte: u8) {
        start(e);
        send_addr7(e, word, 0);
        send_data(e, byte);
        stop(e);
    }

    /// Reads one byte from `word` (single-byte read).
    fn i2c_read1(e: &mut Eeprom, word: u8) -> u8 {
        start(e);
        send_addr7(e, word, 1);
        let v = read_byte(e, false);
        stop(e);
        v
    }

    #[test]
    fn absent_eeprom_claims_nothing() {
        let mut e = Eeprom::empty();
        assert!(!e.is_present());
        assert_eq!(e.read(0x20_0001), None);
        assert!(!e.write(0x20_0001, 0xFF));
    }

    #[test]
    fn single_byte_write_read_back() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        i2c_write(&mut e, 0x12, 0xA5);
        assert_eq!(e.data()[0x12], 0xA5, "committed byte lands in backing store");
        assert!(e.is_dirty());
        assert_eq!(i2c_read1(&mut e, 0x12), 0xA5, "byte reads back over the bus");
    }

    #[test]
    fn read_addresses_fall_through_to_none_off_lane() {
        let e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        // Even lane is not the SDA-out line for the SEGA mapper.
        assert_eq!(e.read(0x20_0000), None);
        // In-window odd address is claimed.
        assert!(e.read(0x20_0001).is_some());
    }

    #[test]
    fn sequential_read_auto_increments() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        i2c_write(&mut e, 0x00, 0x11);
        i2c_write(&mut e, 0x01, 0x22);
        i2c_write(&mut e, 0x02, 0x33);
        // Address 0x00, then read three bytes with ACK-continue between them.
        start(&mut e);
        send_addr7(&mut e, 0x00, 1);
        assert_eq!(read_byte(&mut e, true), 0x11);
        assert_eq!(read_byte(&mut e, true), 0x22);
        assert_eq!(read_byte(&mut e, false), 0x33);
        stop(&mut e);
    }

    #[test]
    fn sequential_read_wraps_at_array_size() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        // X24C01 is 128 bytes; size_mask 0x7F. Write to top and bottom.
        i2c_write(&mut e, 0x7F, 0xEE);
        i2c_write(&mut e, 0x00, 0xDD);
        start(&mut e);
        send_addr7(&mut e, 0x7F, 1);
        assert_eq!(read_byte(&mut e, true), 0xEE); // 0x7F
        assert_eq!(read_byte(&mut e, false), 0xDD); // wrapped to 0x00
        stop(&mut e);
    }

    #[test]
    fn page_write_wraps_within_page() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        // X24C01 page mask is 0x03 (4-byte page). Start at 0x02 and write 3
        // bytes in one burst; the third must wrap to 0x00 within the page, not
        // advance to 0x05.
        start(&mut e);
        send_addr7(&mut e, 0x02, 0);
        send_data(&mut e, 0xA0); // -> 0x02
        send_data(&mut e, 0xA1); // -> 0x03
        send_data(&mut e, 0xA2); // -> wraps to 0x00
        stop(&mut e);
        assert_eq!(e.data()[0x02], 0xA0);
        assert_eq!(e.data()[0x03], 0xA1);
        assert_eq!(e.data()[0x00], 0xA2, "page write wrapped to page base");
        assert_eq!(e.data()[0x04], 0x00, "did not spill past the page");
    }

    #[test]
    fn start_resets_partial_transfer() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        start(&mut e);
        send_addr7(&mut e, 0x40, 0);
        // A repeated START abandons the in-progress write and re-addresses.
        start(&mut e);
        send_addr7(&mut e, 0x05, 0);
        send_data(&mut e, 0x77);
        stop(&mut e);
        assert_eq!(e.data()[0x05], 0x77);
        assert_eq!(e.data()[0x40], 0x00, "abandoned address was never written");
    }

    #[test]
    fn dirty_flag_lifecycle() {
        let mut e = Eeprom::new(EepromType::X24C01, EepromMapper::Sega);
        assert!(!e.is_dirty());
        i2c_write(&mut e, 0x00, 0x01);
        assert!(e.is_dirty());
        e.clear_dirty();
        assert!(!e.is_dirty());
    }

    #[test]
    fn load_populates_without_dirtying() {
        let mut e = Eeprom::new(EepromType::X24C02, EepromMapper::AcclaimOld);
        let saved: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        e.load(&saved);
        assert_eq!(e.data(), &saved[..]);
        assert!(!e.is_dirty());
    }

    fn header_with_serial(serial: &str) -> RomHeader {
        RomHeader {
            system_type: "SEGA GENESIS".to_string(),
            copyright: String::new(),
            title_domestic: String::new(),
            title_overseas: String::new(),
            serial: serial.to_string(),
            rom_start: 0,
            rom_end: 0,
            ram_start: 0,
            ram_end: 0,
            checksum: 0,
            region: "JUE".to_string(),
            has_sram: false,
            sram_start: 0,
            sram_end: 0,
            sram_type: 0,
            sram_layout: crate::rom::SramLayout::Both,
        }
    }

    #[test]
    fn for_rom_matches_known_serials() {
        // Wonder Boy in Monster World (SEGA / X24C01).
        let e = Eeprom::for_rom(&header_with_serial("GM G-4060 -00"));
        assert!(e.is_present());
        assert_eq!(e.chip, Some(EepromType::X24C01));
        assert_eq!(e.mapper, EepromMapper::Sega);

        // NBA Jam (Acclaim 16M / X24C02).
        let e = Eeprom::for_rom(&header_with_serial("T-081326-00"));
        assert_eq!(e.chip, Some(EepromType::X24C02));
        assert_eq!(e.mapper, EepromMapper::AcclaimOld);

        // NBA Jam TE (Acclaim 32M / 24C04).
        let e = Eeprom::for_rom(&header_with_serial("T-81406"));
        assert_eq!(e.chip, Some(EepromType::X24C04));
        assert_eq!(e.mapper, EepromMapper::AcclaimNew);

        // Brian Lara Cricket (Codemasters J-CART / 24C08).
        let e = Eeprom::for_rom(&header_with_serial("T-120106"));
        assert_eq!(e.chip, Some(EepromType::X24C08));
        assert_eq!(e.mapper, EepromMapper::Codemasters);
    }

    #[test]
    fn for_rom_unknown_serial_is_absent() {
        let e = Eeprom::for_rom(&header_with_serial("T-99999"));
        assert!(!e.is_present());
    }

    #[test]
    fn chip_specs_match_reference() {
        assert_eq!(EepromType::X24C01.size(), 128);
        assert_eq!(EepromType::X24C01.address_bits(), 7);
        assert_eq!(EepromType::X24C01.size_mask(), 0x7F);
        assert_eq!(EepromType::X24C64.size(), 8192);
        assert_eq!(EepromType::X24C64.address_bits(), 16);
        assert_eq!(EepromType::X24C64.size_mask(), 0x1FFF);
        assert_eq!(EepromType::X24C16.pagewrite_mask(), 0x0F);
    }
}

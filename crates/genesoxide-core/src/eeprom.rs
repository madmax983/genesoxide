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

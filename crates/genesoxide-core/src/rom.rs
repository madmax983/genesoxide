//! Genesis ROM header parsing.
//!
//! Genesis cartridge ROMs have a header at offset 0x100-0x1FF containing
//! metadata: system type, copyright, game title, ROM/RAM addresses, region.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Byte-lane layout of cartridge backup RAM (SRAM).
///
/// Genesis 8-bit backup RAM is wired to only one half of the 16-bit data bus.
/// `Even` maps SRAM bytes onto even 68000 addresses (the high byte of a word),
/// `Odd` onto odd addresses (the low byte). `Both` is used for word-wide backup
/// RAM (and for the header-less default) where every byte address is backing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SramLayout {
    /// SRAM backs every byte address in the window (word-wide / 16-bit).
    Both,
    /// SRAM only responds on even addresses (high byte of each word).
    EvenOnly,
    /// SRAM only responds on odd addresses (low byte of each word).
    OddOnly,
}

/// Parsed Genesis ROM header.
#[derive(Debug, Clone)]
pub struct RomHeader {
    /// System type (e.g., "SEGA GENESIS" or "SEGA MEGA DRIVE").
    pub system_type: String,
    /// Copyright / release date.
    pub copyright: String,
    /// Domestic (Japanese) game title.
    pub title_domestic: String,
    /// Overseas game title.
    pub title_overseas: String,
    /// Serial number.
    pub serial: String,
    /// ROM start address.
    pub rom_start: u32,
    /// ROM end address.
    pub rom_end: u32,
    /// RAM start address.
    pub ram_start: u32,
    /// RAM end address.
    pub ram_end: u32,
    /// Checksum from header.
    pub checksum: u16,
    /// Region codes string (e.g., "JUE").
    pub region: String,
    /// True if the header declares battery-backed cartridge SRAM
    /// (the 'RA' marker at 0x1B0 with the backup-RAM type pattern at 0x1B2).
    pub has_sram: bool,
    /// SRAM window start address (68000 address space), from 0x1B4.
    pub sram_start: u32,
    /// SRAM window end address (68000 address space), from 0x1B8.
    pub sram_end: u32,
    /// Raw SRAM type/flags byte at 0x1B2 (odd/even/word layout selection).
    pub sram_type: u8,
    /// Decoded byte-lane layout of the backup RAM.
    pub sram_layout: SramLayout,
}

/// Errors that can occur when parsing a ROM.
#[derive(Debug)]
pub enum RomError {
    /// ROM is too small to contain a valid header.
    TooSmall { size: usize },
    /// ROM header has an invalid system type.
    InvalidSystemType(String),
}

impl fmt::Display for RomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { size } => {
                write!(f, "ROM too small ({size} bytes, need at least 512)")
            }
            Self::InvalidSystemType(s) => write!(f, "invalid system type: {s:?}"),
        }
    }
}

impl std::error::Error for RomError {}

/// Reads a trimmed ASCII string from a byte slice.
fn read_ascii(data: &[u8], start: usize, len: usize) -> String {
    let end = (start + len).min(data.len());
    let bytes = &data[start..end];
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                ' '
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Reads a big-endian u16 from a byte slice.
fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}

/// Reads a big-endian u32 from a byte slice.
fn read_u32_be(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Parses a Genesis ROM header from raw bytes.
///
/// # Errors
///
/// Returns [`RomError::TooSmall`] if the data is less than 512 bytes.
/// Returns [`RomError::InvalidSystemType`] if the header doesn't contain
/// a recognized system identifier.
pub fn parse_header(data: &[u8]) -> Result<RomHeader, RomError> {
    if data.len() < 512 {
        return Err(RomError::TooSmall { size: data.len() });
    }

    let system_type = read_ascii(data, 0x100, 16);

    // Validate system type — must contain SEGA
    if !system_type.contains("SEGA") {
        return Err(RomError::InvalidSystemType(system_type));
    }

    // Cartridge backup-RAM (SRAM) descriptor at 0x1B0-0x1BB.
    //   0x1B0: 'RA' marker (0x5241 big-endian) when external RAM is present.
    //   0x1B2: type/flags byte. Bits 6-5 == 0b10 (the 0xA0 nibble pattern)
    //          indicates backup RAM is present. Bit 3 = even-byte-only,
    //          bit 4 = odd-byte-only; neither set => word/both.
    //   0x1B3: reserved flags byte (parsed but unused here).
    //   0x1B4: SRAM start address (u32 big-endian).
    //   0x1B8: SRAM end address (u32 big-endian).
    let sram_marker = read_u16_be(data, 0x1B0);
    let sram_type = data[0x1B2];
    // The canonical "backup RAM present" type value is 0xA0 (bits 7 and 5 set,
    // the "0xA0 nibble pattern"); odd/even layout bits live in bits 3/4.
    let backup_present = (sram_type & 0xA0) == 0xA0;
    let has_sram = sram_marker == 0x5241 && backup_present;
    let sram_layout = if sram_type & 0x08 != 0 {
        SramLayout::EvenOnly
    } else if sram_type & 0x10 != 0 {
        SramLayout::OddOnly
    } else {
        SramLayout::Both
    };
    let sram_start = read_u32_be(data, 0x1B4);
    let sram_end = read_u32_be(data, 0x1B8);

    Ok(RomHeader {
        system_type,
        copyright: read_ascii(data, 0x110, 16),
        title_domestic: read_ascii(data, 0x120, 48),
        title_overseas: read_ascii(data, 0x150, 48),
        serial: read_ascii(data, 0x180, 14),
        checksum: read_u16_be(data, 0x18E),
        rom_start: read_u32_be(data, 0x1A0),
        rom_end: read_u32_be(data, 0x1A4),
        ram_start: read_u32_be(data, 0x1A8),
        ram_end: read_u32_be(data, 0x1AC),
        region: read_ascii(data, 0x1F0, 3),
        has_sram,
        sram_start,
        sram_end,
        sram_type,
        sram_layout,
    })
}

/// Computes the ROM checksum (sum of all u16 words from 0x200 onward).
#[must_use]
pub fn compute_checksum(data: &[u8]) -> u16 {
    let mut sum: u16 = 0;
    let mut i = 0x200;
    while i + 1 < data.len() {
        sum = sum.wrapping_add(read_u16_be(data, i));
        i += 2;
    }
    sum
}

/// Verifies the ROM checksum matches the header value.
#[must_use]
pub fn verify_checksum(data: &[u8]) -> bool {
    if data.len() < 512 {
        return false;
    }
    let header_checksum = read_u16_be(data, 0x18E);
    let computed = compute_checksum(data);
    header_checksum == computed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_minimal_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 1024];
        // Write "SEGA GENESIS    " at 0x100
        let sys = b"SEGA GENESIS    ";
        rom[0x100..0x110].copy_from_slice(sys);
        // ROM end address
        let end = (rom.len() as u32 - 1).to_be_bytes();
        rom[0x1A4..0x1A8].copy_from_slice(&end);
        // Region "JUE"
        rom[0x1F0..0x1F3].copy_from_slice(b"JUE");
        rom
    }

    #[test]
    fn parse_valid_header() {
        let rom = make_minimal_rom();
        let header = parse_header(&rom).unwrap();
        assert_eq!(header.system_type, "SEGA GENESIS");
        assert_eq!(header.region, "JUE");
    }

    #[test]
    fn reject_too_small() {
        let rom = vec![0u8; 100];
        assert!(matches!(parse_header(&rom), Err(RomError::TooSmall { .. })));
    }

    #[test]
    fn reject_invalid_system_type() {
        let mut rom = vec![0u8; 512];
        rom[0x100..0x110].copy_from_slice(b"NOT A GENESIS   ");
        assert!(matches!(
            parse_header(&rom),
            Err(RomError::InvalidSystemType(_))
        ));
    }

    #[test]
    fn checksum_roundtrip() {
        let mut rom = make_minimal_rom();
        let checksum = compute_checksum(&rom);
        rom[0x18E..0x190].copy_from_slice(&checksum.to_be_bytes());
        assert!(verify_checksum(&rom));
    }

    /// Writes an SRAM descriptor into a ROM buffer at 0x1B0-0x1BB.
    fn write_sram_descriptor(rom: &mut [u8], type_byte: u8, start: u32, end: u32) {
        rom[0x1B0..0x1B2].copy_from_slice(&0x5241u16.to_be_bytes()); // 'RA'
        rom[0x1B2] = type_byte;
        rom[0x1B3] = 0x20;
        rom[0x1B4..0x1B8].copy_from_slice(&start.to_be_bytes());
        rom[0x1B8..0x1BC].copy_from_slice(&end.to_be_bytes());
    }

    #[test]
    fn no_sram_marker_means_no_sram() {
        let rom = make_minimal_rom();
        let header = parse_header(&rom).unwrap();
        assert!(!header.has_sram);
    }

    #[test]
    fn sram_layout_both() {
        let mut rom = make_minimal_rom();
        // 0xA0 = bits 6,5 == 0b10, no odd/even bit => Both.
        write_sram_descriptor(&mut rom, 0xA0, 0x200000, 0x20FFFF);
        let header = parse_header(&rom).unwrap();
        assert!(header.has_sram);
        assert_eq!(header.sram_layout, SramLayout::Both);
        assert_eq!(header.sram_start, 0x200000);
        assert_eq!(header.sram_end, 0x20FFFF);
    }

    #[test]
    fn sram_layout_even_only() {
        let mut rom = make_minimal_rom();
        // 0xA8 = 0xA0 | bit3 (even-only).
        write_sram_descriptor(&mut rom, 0xA8, 0x200000, 0x20FFFF);
        let header = parse_header(&rom).unwrap();
        assert!(header.has_sram);
        assert_eq!(header.sram_layout, SramLayout::EvenOnly);
    }

    #[test]
    fn sram_layout_odd_only() {
        let mut rom = make_minimal_rom();
        // 0xB0 = 0xA0 | bit4 (odd-only).
        write_sram_descriptor(&mut rom, 0xB0, 0x200001, 0x20FFFF);
        let header = parse_header(&rom).unwrap();
        assert!(header.has_sram);
        assert_eq!(header.sram_layout, SramLayout::OddOnly);
        assert_eq!(header.sram_start, 0x200001);
    }

    #[test]
    fn sram_marker_without_backup_pattern_ignored() {
        let mut rom = make_minimal_rom();
        // 'RA' present but type bits 6,5 not 0b10 (0x00) => not backup RAM.
        write_sram_descriptor(&mut rom, 0x00, 0x200000, 0x20FFFF);
        let header = parse_header(&rom).unwrap();
        assert!(!header.has_sram);
    }
}

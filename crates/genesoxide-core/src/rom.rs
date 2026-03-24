//! Genesis ROM header parsing.
//!
//! Genesis cartridge ROMs have a header at offset 0x100-0x1FF containing
//! metadata: system type, copyright, game title, ROM/RAM addresses, region.

use std::fmt;

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
}

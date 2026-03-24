//! Memory bus routing for the 68000's 24-bit address space.
//!
//! The Genesis has a 24-bit address space (0x000000 to 0xFFFFFF). This module
//! defines the distinct regions and maps addresses to their hardware targets.

/// Distinct regions of the Genesis 68000 memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusRegion {
    /// Cartridge ROM (0x000000..=0x3FFFFF). Up to 4MB.
    CartridgeRom,
    /// Extended cartridge space (0x400000..=0x7FFFFF). Used by some mappers.
    CartridgeExtended,
    /// Z80 address space window (0xA00000..=0xA0FFFF). 64KB banked.
    Z80Area,
    /// I/O registers (0xA10000..=0xA1001F). Controllers, region, TMSS.
    IoRegisters,
    /// Control registers (0xA11100..=0xA11201). Z80 bus request/reset.
    ControlRegisters,
    /// VDP ports (0xC00000..=0xC0001F). Data, control, HV counter.
    Vdp,
    /// 68K work RAM (0xFF0000..=0xFFFFFF). 64KB, mirrored.
    WorkRam,
    /// Unmapped address. Reads return open bus, writes are ignored.
    Unmapped,
}

/// Maps a 24-bit 68000 address to its corresponding [`BusRegion`].
///
/// # Examples
///
/// ```
/// use genesoxide_core::bus::{map_region, BusRegion};
///
/// assert_eq!(map_region(0x000000), BusRegion::CartridgeRom);
/// assert_eq!(map_region(0xC00000), BusRegion::Vdp);
/// assert_eq!(map_region(0xFF0000), BusRegion::WorkRam);
/// ```
#[must_use]
pub fn map_region(addr: u32) -> BusRegion {
    // Mask to 24 bits
    let addr = addr & 0x00FF_FFFF;

    match addr {
        0x000000..=0x3FFFFF => BusRegion::CartridgeRom,
        0x400000..=0x7FFFFF => BusRegion::CartridgeExtended,
        0xA00000..=0xA0FFFF => BusRegion::Z80Area,
        0xA10000..=0xA1001F => BusRegion::IoRegisters,
        0xA11100..=0xA11201 => BusRegion::ControlRegisters,
        0xC00000..=0xC0001F => BusRegion::Vdp,
        0xFF0000..=0xFFFFFF => BusRegion::WorkRam,
        _ => BusRegion::Unmapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartridge_rom_region() {
        assert_eq!(map_region(0x000000), BusRegion::CartridgeRom);
        assert_eq!(map_region(0x000100), BusRegion::CartridgeRom);
        assert_eq!(map_region(0x3FFFFF), BusRegion::CartridgeRom);
    }

    #[test]
    fn work_ram_region() {
        assert_eq!(map_region(0xFF0000), BusRegion::WorkRam);
        assert_eq!(map_region(0xFFFFFF), BusRegion::WorkRam);
    }

    #[test]
    fn vdp_region() {
        assert_eq!(map_region(0xC00000), BusRegion::Vdp);
        assert_eq!(map_region(0xC00004), BusRegion::Vdp);
        assert_eq!(map_region(0xC00008), BusRegion::Vdp);
    }

    #[test]
    fn io_region() {
        assert_eq!(map_region(0xA10000), BusRegion::IoRegisters);
        assert_eq!(map_region(0xA1001F), BusRegion::IoRegisters);
    }

    #[test]
    fn unmapped_returns_unmapped() {
        assert_eq!(map_region(0x800000), BusRegion::Unmapped);
        assert_eq!(map_region(0xE00000), BusRegion::Unmapped);
    }

    #[test]
    fn address_masked_to_24_bits() {
        // Bit 24+ should be ignored
        assert_eq!(map_region(0xFF_FF0000), BusRegion::WorkRam);
    }
}

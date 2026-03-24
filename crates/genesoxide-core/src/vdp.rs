//! Sega Genesis VDP (Video Display Processor) — Yamaha YM7101.
//!
//! The VDP handles all video output: two scrollable tile planes (A and B),
//! a window plane, up to 80 sprites, and a DMA engine for bulk memory
//! transfers. Output resolution is 320x224 (NTSC H40 mode).
//!
//! Communication with the 68000 happens through two 16-bit ports:
//! - Data port (0xC00000): read/write VRAM, CRAM, VSRAM
//! - Control port (0xC00004): register writes, DMA setup, address setup
//!
//! The control port uses a two-word command sequence to set up the
//! destination and access type. This state machine is one of the
//! trickiest parts of Genesis emulation.

use serde::{Deserialize, Serialize};

use crate::api::FRAME_RGBA_BYTES;

/// VRAM size in bytes.
pub const VRAM_SIZE: usize = 0x10000; // 64KB
/// Number of CRAM entries (9-bit RGB color values stored as u16).
pub const CRAM_ENTRIES: usize = 64; // 4 palettes x 16 colors
/// Number of VSRAM entries (vertical scroll values).
pub const VSRAM_ENTRIES: usize = 40;
/// Number of VDP registers.
pub const VDP_REGISTER_COUNT: usize = 24;

/// VDP access type set by the control port command words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessType {
    VramRead,
    VramWrite,
    CramRead,
    CramWrite,
    VsramRead,
    VsramWrite,
}

/// State of the control port command word parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlState {
    /// Waiting for the first command word.
    Idle,
    /// Received first word, waiting for second.
    PendingSecond(u16),
}

/// DMA mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DmaMode {
    /// 68K to VRAM/CRAM/VSRAM transfer.
    MemoryToVram,
    /// VRAM fill.
    Fill,
    /// VRAM to VRAM copy.
    Copy,
}

/// Serializable VDP snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VdpSnapshot {
    pub vram: Vec<u8>,
    pub cram: Vec<u16>,
    pub vsram: Vec<u16>,
    pub registers: Vec<u8>,
    pub control_state: ControlState,
    pub address: u16,
    pub scanline: u16,
    pub dot: u16,
}

/// The VDP.
pub struct Vdp {
    /// 64KB Video RAM.
    vram: [u8; VRAM_SIZE],
    /// Color RAM — 64 entries of 9-bit RGB (stored in lower 9 bits of u16).
    cram: [u16; CRAM_ENTRIES],
    /// Vertical scroll RAM — per-column vertical scroll offsets.
    vsram: [u16; VSRAM_ENTRIES],
    /// VDP registers (24 registers, 8-bit each).
    registers: [u8; VDP_REGISTER_COUNT],
    /// Control port state machine.
    control_state: ControlState,
    /// Current VRAM/CRAM/VSRAM address for data port access.
    address: u16,
    /// Auto-increment value (register 0x0F).
    auto_increment: u16,
    /// Current access type.
    access_type: Option<AccessType>,
    /// Current scanline (0-261 NTSC).
    scanline: u16,
    /// Current dot within scanline.
    dot: u16,
    /// RGBA framebuffer.
    framebuffer: Box<[u8; FRAME_RGBA_BYTES]>,
    /// V-blank flag.
    in_vblank: bool,
    /// H-blank flag.
    in_hblank: bool,
}

impl Vdp {
    /// Creates a new VDP in its power-on state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vram: [0; VRAM_SIZE],
            cram: [0; CRAM_ENTRIES],
            vsram: [0; VSRAM_ENTRIES],
            registers: [0; VDP_REGISTER_COUNT],
            control_state: ControlState::Idle,
            address: 0,
            auto_increment: 0,
            access_type: None,
            scanline: 0,
            dot: 0,
            framebuffer: Box::new([0; FRAME_RGBA_BYTES]),
            in_vblank: false,
            in_hblank: false,
        }
    }

    /// Returns a reference to the RGBA framebuffer.
    #[must_use]
    pub fn framebuffer(&self) -> &[u8; FRAME_RGBA_BYTES] {
        &self.framebuffer
    }

    /// Returns the current scanline.
    #[must_use]
    pub fn scanline(&self) -> u16 {
        self.scanline
    }

    /// Returns true if in vertical blanking interval.
    #[must_use]
    pub fn in_vblank(&self) -> bool {
        self.in_vblank
    }

    /// Writes to the VDP control port.
    ///
    /// Handles the two-word command sequence and register writes.
    pub fn write_control(&mut self, value: u16) {
        match self.control_state {
            ControlState::Idle => {
                // Check if this is a register write: 100x xxxx xxxx xxxx
                if value & 0xC000 == 0x8000 {
                    let reg = ((value >> 8) & 0x1F) as usize;
                    let data = (value & 0xFF) as u8;
                    if reg < VDP_REGISTER_COUNT {
                        self.registers[reg] = data;
                        if reg == 0x0F {
                            self.auto_increment = u16::from(data);
                        }
                    }
                    // Register writes don't change control state
                } else {
                    // First word of a two-word command
                    self.control_state = ControlState::PendingSecond(value);
                }
            }
            ControlState::PendingSecond(first) => {
                // Combine first and second words to get access type + address
                let cd = ((first >> 14) & 0x03) | ((value >> 2) & 0x3C);
                let addr = (first & 0x3FFF) | ((value & 0x03) << 14);

                self.address = addr;
                self.access_type = match cd & 0x0F {
                    0b0000 => Some(AccessType::VramRead),
                    0b0001 => Some(AccessType::VramWrite),
                    0b1000 => Some(AccessType::CramRead),
                    0b0011 => Some(AccessType::CramWrite),
                    0b0100 => Some(AccessType::VsramRead),
                    0b0101 => Some(AccessType::VsramWrite),
                    _ => None,
                };
                self.control_state = ControlState::Idle;
            }
        }
    }

    /// Writes to the VDP data port.
    pub fn write_data(&mut self, value: u16) {
        // Reset control state on data port access
        self.control_state = ControlState::Idle;

        match self.access_type {
            Some(AccessType::VramWrite) => {
                let addr = self.address as usize;
                if addr < VRAM_SIZE - 1 {
                    self.vram[addr] = (value >> 8) as u8;
                    self.vram[addr + 1] = value as u8;
                }
            }
            Some(AccessType::CramWrite) => {
                let index = (self.address >> 1) as usize;
                if index < CRAM_ENTRIES {
                    self.cram[index] = value & 0x0EEE; // 9-bit: 0BBB0GGG0RRR
                }
            }
            Some(AccessType::VsramWrite) => {
                let index = (self.address >> 1) as usize;
                if index < VSRAM_ENTRIES {
                    self.vsram[index] = value & 0x07FF; // 11-bit scroll value
                }
            }
            _ => {}
        }
        self.address = self.address.wrapping_add(self.auto_increment);
    }

    /// Reads from the VDP data port.
    #[must_use]
    pub fn read_data(&mut self) -> u16 {
        self.control_state = ControlState::Idle;

        let result = match self.access_type {
            Some(AccessType::VramRead) => {
                let addr = self.address as usize;
                if addr < VRAM_SIZE - 1 {
                    u16::from(self.vram[addr]) << 8 | u16::from(self.vram[addr + 1])
                } else {
                    0
                }
            }
            Some(AccessType::CramRead) => {
                let index = (self.address >> 1) as usize;
                if index < CRAM_ENTRIES {
                    self.cram[index]
                } else {
                    0
                }
            }
            Some(AccessType::VsramRead) => {
                let index = (self.address >> 1) as usize;
                if index < VSRAM_ENTRIES {
                    self.vsram[index]
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.address = self.address.wrapping_add(self.auto_increment);
        result
    }

    /// Reads the VDP status register.
    #[must_use]
    pub fn read_status(&self) -> u16 {
        let mut status: u16 = 0x3400; // Always set bits
        if self.in_vblank {
            status |= 0x0008;
        }
        if self.in_hblank {
            status |= 0x0004;
        }
        status
    }

    /// Converts a 9-bit Genesis color (0BBB0GGG0RRR) to RGBA.
    #[must_use]
    fn color_to_rgba(color: u16) -> [u8; 4] {
        let r = ((color & 0x00E) >> 1) as u8;
        let g = ((color & 0x0E0) >> 5) as u8;
        let b = ((color & 0xE00) >> 9) as u8;
        // Scale 3-bit (0-7) to 8-bit (0-255)
        [r * 36 + r / 2, g * 36 + g / 2, b * 36 + b / 2, 0xFF]
    }

    /// Returns the background color (palette 0, color 0).
    #[must_use]
    pub fn background_color(&self) -> [u8; 4] {
        let bg_index = (self.registers[7] & 0x3F) as usize;
        if bg_index < CRAM_ENTRIES {
            Self::color_to_rgba(self.cram[bg_index])
        } else {
            [0, 0, 0, 0xFF]
        }
    }

    /// Snapshot for save states.
    #[must_use]
    pub fn snapshot(&self) -> VdpSnapshot {
        VdpSnapshot {
            vram: self.vram.to_vec(),
            cram: self.cram.to_vec(),
            vsram: self.vsram.to_vec(),
            registers: self.registers.to_vec(),
            control_state: self.control_state,
            address: self.address,
            scanline: self.scanline,
            dot: self.dot,
        }
    }
}

impl Default for Vdp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_write() {
        let mut vdp = Vdp::new();
        // Write 0x42 to register 5: command = 0x8542
        vdp.write_control(0x8542);
        assert_eq!(vdp.registers[5], 0x42);
    }

    #[test]
    fn auto_increment_register() {
        let mut vdp = Vdp::new();
        // Set auto-increment (reg 0x0F) to 2
        vdp.write_control(0x8F02);
        assert_eq!(vdp.auto_increment, 2);
    }

    #[test]
    fn vram_write_and_read() {
        let mut vdp = Vdp::new();
        // Set auto-increment to 2
        vdp.write_control(0x8F02);
        // Set up VRAM write to address 0x0000
        // First word:  CD1-0=01 (VRAM write), A13-0=0x0000 -> 0x4000
        // Second word: CD5-2=0000, A15-14=00 -> 0x0000
        vdp.write_control(0x4000);
        vdp.write_control(0x0000);
        // Write data
        vdp.write_data(0xABCD);

        // Set up VRAM read from address 0x0000
        vdp.write_control(0x0000);
        vdp.write_control(0x0000);
        let val = vdp.read_data();
        assert_eq!(val, 0xABCD);
    }

    #[test]
    fn cram_write_masks_to_9bit() {
        let mut vdp = Vdp::new();
        vdp.write_control(0x8F02);
        // CRAM write: CD=0011 -> first word has CD1-0=11, addr=0
        vdp.write_control(0xC000);
        vdp.write_control(0x0000);
        vdp.write_data(0xFFFF);
        // Should be masked to 0x0EEE
        vdp.write_control(0x0000); // CRAM read setup
        vdp.write_control(0x0020); // CD5-2=1000
        // Direct check via snapshot
        let snap = vdp.snapshot();
        assert_eq!(snap.cram[0], 0x0EEE);
    }

    #[test]
    fn color_conversion() {
        // Full white: 0x0EEE -> (7,7,7) -> (255,255,255) approximately
        let rgba = Vdp::color_to_rgba(0x0EEE);
        assert_eq!(rgba[0], 255); // R
        assert_eq!(rgba[1], 255); // G
        assert_eq!(rgba[2], 255); // B

        // Pure red: 0x000E
        let rgba = Vdp::color_to_rgba(0x000E);
        assert_eq!(rgba[0], 255);
        assert_eq!(rgba[1], 0);
        assert_eq!(rgba[2], 0);

        // Black: 0x0000
        let rgba = Vdp::color_to_rgba(0x0000);
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[1], 0);
        assert_eq!(rgba[2], 0);
    }

    #[test]
    fn status_register_vblank_flag() {
        let mut vdp = Vdp::new();
        assert_eq!(vdp.read_status() & 0x0008, 0);
        vdp.in_vblank = true;
        assert_ne!(vdp.read_status() & 0x0008, 0);
    }
}

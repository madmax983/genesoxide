//! Motorola 68000 CPU emulation.
//!
//! The 68000 is a 16/32-bit CISC processor with:
//! - 8 data registers (D0-D7)
//! - 8 address registers (A0-A7, where A7 is the stack pointer)
//! - 24-bit program counter
//! - 16-bit status register (CCR + supervisor bits)
//! - Big-endian byte order
//!
//! In the Genesis, it runs at ~7.67 MHz (master clock / 7).

mod decode;
mod engine;

pub use decode::{AddressingMode, Instruction, InstructionSize, decode};
pub use engine::Cpu;

use serde::{Deserialize, Serialize};

/// Snapshot of CPU state for save states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuSnapshot {
    /// Data registers D0-D7.
    pub d: [u32; 8],
    /// Address registers A0-A7.
    pub a: [u32; 8],
    /// Program counter (24-bit).
    pub pc: u32,
    /// Status register.
    pub sr: u16,
    /// Supervisor stack pointer.
    pub ssp: u32,
    /// User stack pointer.
    pub usp: u32,
    /// Total cycles elapsed.
    pub cycles: u64,
    /// Whether the CPU is halted.
    pub halted: bool,
    /// Whether the CPU is stopped (STOP instruction).
    pub stopped: bool,
}

/// 68000 status register flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRegister(pub u16);

impl StatusRegister {
    // Condition Code Register (CCR) bits — lower byte
    /// Carry flag.
    pub const C: u16 = 1 << 0;
    /// Overflow flag.
    pub const V: u16 = 1 << 1;
    /// Zero flag.
    pub const Z: u16 = 1 << 2;
    /// Negative flag.
    pub const N: u16 = 1 << 3;
    /// Extend flag.
    pub const X: u16 = 1 << 4;

    // System byte — upper byte
    /// Interrupt priority mask (bits 8-10).
    pub const IPM_MASK: u16 = 0x0700;
    /// Supervisor mode.
    pub const S: u16 = 1 << 13;
    /// Trace mode.
    pub const T: u16 = 1 << 15;

    /// Creates a new status register with the given raw value.
    #[must_use]
    pub fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns true if the given flag is set.
    #[must_use]
    pub fn flag(self, mask: u16) -> bool {
        self.0 & mask != 0
    }

    /// Sets or clears a flag.
    pub fn set_flag(&mut self, mask: u16, value: bool) {
        if value {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }

    /// Returns the interrupt priority mask (0-7).
    #[must_use]
    pub fn interrupt_mask(self) -> u8 {
        ((self.0 & Self::IPM_MASK) >> 8) as u8
    }

    /// Sets the interrupt priority mask (0-7).
    pub fn set_interrupt_mask(&mut self, level: u8) {
        self.0 = (self.0 & !Self::IPM_MASK) | (u16::from(level & 7) << 8);
    }

    /// Returns true if in supervisor mode.
    #[must_use]
    pub fn supervisor(self) -> bool {
        self.flag(Self::S)
    }

    /// Returns the CCR (lower byte).
    #[must_use]
    pub fn ccr(self) -> u8 {
        (self.0 & 0x1F) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_register_flags() {
        let mut sr = StatusRegister::new(0);
        assert!(!sr.flag(StatusRegister::C));

        sr.set_flag(StatusRegister::C, true);
        assert!(sr.flag(StatusRegister::C));

        sr.set_flag(StatusRegister::Z, true);
        sr.set_flag(StatusRegister::N, true);
        assert!(sr.flag(StatusRegister::Z));
        assert!(sr.flag(StatusRegister::N));
        assert!(!sr.flag(StatusRegister::V));
    }

    #[test]
    fn interrupt_mask() {
        let mut sr = StatusRegister::new(0);
        sr.set_interrupt_mask(5);
        assert_eq!(sr.interrupt_mask(), 5);
        sr.set_interrupt_mask(7);
        assert_eq!(sr.interrupt_mask(), 7);
    }

    #[test]
    fn supervisor_mode() {
        let sr = StatusRegister::new(StatusRegister::S);
        assert!(sr.supervisor());

        let sr = StatusRegister::new(0);
        assert!(!sr.supervisor());
    }
}

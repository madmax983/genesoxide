//! Z80 instruction executor.
//!
//! Fetches, decodes, and executes one instruction, returning the number
//! of T-states consumed. The executor communicates with memory and I/O
//! through the [`Bus`] trait.

use super::Z80;

// ── Bus trait ────────────────────────────────────────────────────────────

/// Memory and I/O bus interface for the Z80.
///
/// The Z80 has a 16-bit address space (64KB) and a separate 16-bit I/O
/// port space. Implementors map these to the Genesis sound subsystem:
/// Z80 RAM, YM2612 registers, SN76489, and bank-switched 68000 ROM.
pub trait Bus {
    /// Reads a byte from the given memory address.
    fn read_byte(&mut self, addr: u16) -> u8;
    /// Writes a byte to the given memory address.
    fn write_byte(&mut self, addr: u16, val: u8);
    /// Reads a byte from the given I/O port.
    fn read_port(&mut self, port: u16) -> u8;
    /// Writes a byte to the given I/O port.
    fn write_port(&mut self, port: u16, val: u8);
}

// ── Executor ────────────────────────────────────────────────────────────

/// Fetches and executes one Z80 instruction, returning T-states consumed.
///
/// The refresh register (R) is incremented on each opcode fetch: the
/// lower 7 bits count while bit 7 is preserved (as on real hardware).
pub fn execute_instruction(cpu: &mut Z80, bus: &mut dyn Bus) -> u8 {
    let opcode = bus.read_byte(cpu.pc);
    cpu.pc = cpu.pc.wrapping_add(1);

    // Increment R: lower 7 bits wrap, bit 7 is preserved.
    cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);

    match opcode {
        0x00 => 4, // NOP: 4 T-states
        _ => 4,    // Unknown — treat as NOP timing
    }
}

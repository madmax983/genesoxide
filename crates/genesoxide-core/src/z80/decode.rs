//! Z80 instruction decoder.
//!
//! Decodes the first opcode byte into an [`Instruction`] variant.
//! Extended prefixes (CB, DD, ED, FD) will be added as instructions
//! are implemented.

/// A decoded Z80 instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    /// NOP — no operation.
    Nop,
    /// Unrecognized opcode (treated as NOP for now).
    Unknown(u8),
}

/// Decodes a single opcode byte into an [`Instruction`].
#[must_use]
pub fn decode(opcode: u8) -> Instruction {
    match opcode {
        0x00 => Instruction::Nop,
        _ => Instruction::Unknown(opcode),
    }
}

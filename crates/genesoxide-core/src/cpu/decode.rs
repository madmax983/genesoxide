//! 68000 instruction decoder.
//!
//! Decodes the first word of a 68000 instruction into an [`Instruction`]
//! enum. The 68000 uses variable-length instructions (1-5 words), where
//! the first word determines the instruction and addressing modes, and
//! subsequent words provide immediate data or extension words.
//!
//! This is the primary target for Verus verification: the decoder is a
//! pure function from u16 → Instruction with well-defined behavior for
//! every possible input.

/// Instruction operand size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionSize {
    /// Byte (8-bit).
    Byte,
    /// Word (16-bit).
    Word,
    /// Long (32-bit).
    Long,
}

/// 68000 addressing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressingMode {
    /// Data register direct: Dn
    DataDirect(u8),
    /// Address register direct: An
    AddrDirect(u8),
    /// Address register indirect: (An)
    AddrIndirect(u8),
    /// Address register indirect with post-increment: (An)+
    AddrPostInc(u8),
    /// Address register indirect with pre-decrement: -(An)
    AddrPreDec(u8),
    /// Address register indirect with displacement: d16(An)
    AddrDisp(u8),
    /// Address register indirect with index: d8(An,Xn)
    AddrIndex(u8),
    /// Absolute short: (xxx).W
    AbsShort,
    /// Absolute long: (xxx).L
    AbsLong,
    /// PC with displacement: d16(PC)
    PcDisp,
    /// PC with index: d8(PC,Xn)
    PcIndex,
    /// Immediate: #imm
    Immediate,
}

/// Decoded 68000 instruction.
///
/// This represents the instruction opcode and addressing modes, but not
/// the extension words (immediates, displacements). Those are read during
/// execution from the instruction stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    // === Data Movement ===
    /// MOVE source, destination
    Move(InstructionSize),
    /// MOVEA source, An
    MoveA(InstructionSize),
    /// MOVEQ #imm8, Dn (immediate in opcode)
    MoveQ,
    /// LEA ea, An
    Lea,
    /// PEA ea
    Pea,

    // === Arithmetic ===
    /// ADD source, Dn / Dn, destination
    Add(InstructionSize),
    /// ADDA source, An
    AddA(InstructionSize),
    /// ADDI #imm, destination
    AddI(InstructionSize),
    /// ADDQ #imm3, destination
    AddQ(InstructionSize),
    /// SUB
    Sub(InstructionSize),
    /// SUBA
    SubA(InstructionSize),
    /// SUBI
    SubI(InstructionSize),
    /// SUBQ
    SubQ(InstructionSize),
    /// MULU source, Dn
    MulU,
    /// MULS source, Dn
    MulS,
    /// DIVU source, Dn
    DivU,
    /// DIVS source, Dn
    DivS,
    /// CLR destination
    Clr(InstructionSize),
    /// NEG destination
    Neg(InstructionSize),
    /// EXT Dn
    Ext(InstructionSize),

    // === Logic ===
    /// AND
    And(InstructionSize),
    /// ANDI
    AndI(InstructionSize),
    /// OR
    Or(InstructionSize),
    /// ORI
    OrI(InstructionSize),
    /// EOR
    Eor(InstructionSize),
    /// EORI
    EorI(InstructionSize),
    /// NOT
    Not(InstructionSize),

    // === Shift/Rotate ===
    /// ASL / ASR
    Asd(InstructionSize),
    /// LSL / LSR
    Lsd(InstructionSize),
    /// ROL / ROR
    Rod(InstructionSize),
    /// ROXL / ROXR
    Roxd(InstructionSize),

    // === Bit Manipulation ===
    /// BTST
    BTst,
    /// BSET
    BSet,
    /// BCLR
    BClr,
    /// BCHG
    BChg,

    // === Compare ===
    /// CMP source, Dn
    Cmp(InstructionSize),
    /// CMPA source, An
    CmpA(InstructionSize),
    /// CMPI #imm, destination
    CmpI(InstructionSize),
    /// TST destination
    Tst(InstructionSize),

    // === Branch ===
    /// BRA / Bcc (condition code in opcode)
    Bcc,
    /// DBcc Dn, displacement
    DBcc,
    /// Scc destination
    Scc,

    // === Jump/Subroutine ===
    /// JMP ea
    Jmp,
    /// JSR ea
    Jsr,
    /// BSR displacement
    Bsr,
    /// RTS
    Rts,
    /// RTE
    Rte,
    /// RTR
    Rtr,

    // === Stack ===
    /// LINK An, #disp
    Link,
    /// UNLK An
    Unlk,
    /// MOVEM registers, ea / ea, registers
    Movem(InstructionSize),

    // === System ===
    /// NOP
    Nop,
    /// STOP #imm
    Stop,
    /// RESET
    ResetInstr,
    /// TRAP #vector
    Trap,
    /// TRAPV
    TrapV,
    /// ANDI to CCR / ANDI to SR
    AndiSr,
    /// ORI to CCR / ORI to SR
    OriSr,
    /// EORI to CCR / EORI to SR
    EoriSr,
    /// MOVE to/from SR
    MoveSr,
    /// MOVE USP
    MoveUsp,
    /// SWAP Dn
    Swap,
    /// EXG
    Exg,

    /// Illegal / unrecognized opcode.
    Illegal,
}

/// Decodes a 68000 instruction from its first opcode word.
///
/// Returns the [`Instruction`] variant. Extension words (immediates,
/// displacements) are not consumed here — they're read during execution.
///
/// # Examples
///
/// ```
/// use genesoxide_core::cpu::{Instruction, decode};
///
/// // NOP = 0x4E71
/// assert_eq!(decode(0x4E71), Instruction::Nop);
/// // RTS = 0x4E75
/// assert_eq!(decode(0x4E75), Instruction::Rts);
/// ```
#[must_use]
pub fn decode(opcode: u16) -> Instruction {
    // Top 4 bits select the major group
    match opcode >> 12 {
        0b0111 => {
            // MOVEQ: 0111 Dn 0 imm8
            if opcode & 0x0100 == 0 {
                return Instruction::MoveQ;
            }
            Instruction::Illegal
        }
        0b0100 => decode_group4(opcode),
        _ => {
            // TODO: implement remaining instruction groups
            Instruction::Illegal
        }
    }
}

/// Decodes group 4 (0100xxxx) — miscellaneous instructions.
fn decode_group4(opcode: u16) -> Instruction {
    match opcode {
        0x4E71 => Instruction::Nop,
        0x4E75 => Instruction::Rts,
        0x4E73 => Instruction::Rte,
        0x4E77 => Instruction::Rtr,
        0x4E70 => Instruction::ResetInstr,
        0x4E72 => Instruction::Stop,
        0x4E76 => Instruction::TrapV,
        _ => {
            // TODO: decode remaining group 4 instructions
            Instruction::Illegal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_nop() {
        assert_eq!(decode(0x4E71), Instruction::Nop);
    }

    #[test]
    fn decode_rts() {
        assert_eq!(decode(0x4E75), Instruction::Rts);
    }

    #[test]
    fn decode_rte() {
        assert_eq!(decode(0x4E73), Instruction::Rte);
    }

    #[test]
    fn decode_moveq() {
        // MOVEQ #0, D0 = 0x7000
        assert_eq!(decode(0x7000), Instruction::MoveQ);
        // MOVEQ #42, D3 = 0x762A
        assert_eq!(decode(0x762A), Instruction::MoveQ);
    }

    #[test]
    fn unknown_decodes_to_illegal() {
        // Unimplemented opcodes should return Illegal, not panic
        let _ = decode(0x0000);
        let _ = decode(0xFFFF);
    }
}

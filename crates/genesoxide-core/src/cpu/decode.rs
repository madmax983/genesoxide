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
    /// ADDX
    AddX(InstructionSize),
    /// SUB
    Sub(InstructionSize),
    /// SUBA
    SubA(InstructionSize),
    /// SUBI
    SubI(InstructionSize),
    /// SUBQ
    SubQ(InstructionSize),
    /// SUBX
    SubX(InstructionSize),
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
    /// NEGX destination
    NegX(InstructionSize),
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
    /// Memory shift (word, count=1): ASd/LSd/ROXd/ROd <ea>
    ShiftMem,

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
    /// CMPM (An)+, (Am)+
    CmpM(InstructionSize),
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
    /// ANDI to CCR
    AndiCcr,
    /// ANDI to SR
    AndiSr,
    /// ORI to CCR
    OriCcr,
    /// ORI to SR
    OriSr,
    /// EORI to CCR
    EoriCcr,
    /// EORI to SR
    EoriSr,
    /// MOVE to SR
    MoveToSr,
    /// MOVE from SR
    MoveFromSr,
    /// MOVE to CCR
    MoveToCcr,
    /// MOVE USP
    MoveUsp,
    /// SWAP Dn
    Swap,
    /// EXG
    Exg,
    /// MOVEP
    MoveP(InstructionSize),

    /// Line A trap (unassigned opcode range 0xAxxx).
    LineA,
    /// Line F trap (unassigned opcode range 0xFxxx).
    LineF,
    /// Illegal / unrecognized opcode.
    Illegal,
}

// ── Helpers for extracting fields from opcode ──────────────────────────

/// Decode the standard 6-bit effective address field (mode:3, reg:3).
#[must_use]
pub fn decode_ea(mode: u8, reg: u8) -> AddressingMode {
    match mode {
        0 => AddressingMode::DataDirect(reg),
        1 => AddressingMode::AddrDirect(reg),
        2 => AddressingMode::AddrIndirect(reg),
        3 => AddressingMode::AddrPostInc(reg),
        4 => AddressingMode::AddrPreDec(reg),
        5 => AddressingMode::AddrDisp(reg),
        6 => AddressingMode::AddrIndex(reg),
        7 => match reg {
            0 => AddressingMode::AbsShort,
            1 => AddressingMode::AbsLong,
            2 => AddressingMode::PcDisp,
            3 => AddressingMode::PcIndex,
            4 => AddressingMode::Immediate,
            _ => AddressingMode::DataDirect(0), // invalid, will be caught elsewhere
        },
        _ => AddressingMode::DataDirect(0),
    }
}

/// Decode a 2-bit size field (00=byte, 01=word, 10=long).
#[must_use]
pub fn decode_size(bits: u8) -> Option<InstructionSize> {
    match bits {
        0 => Some(InstructionSize::Byte),
        1 => Some(InstructionSize::Word),
        2 => Some(InstructionSize::Long),
        _ => None,
    }
}

/// Decode a 2-bit size field for MOVE (01=byte, 11=word, 10=long).
#[must_use]
fn decode_move_size(bits: u8) -> Option<InstructionSize> {
    match bits {
        1 => Some(InstructionSize::Byte),
        3 => Some(InstructionSize::Word),
        2 => Some(InstructionSize::Long),
        _ => None,
    }
}

// ── Main decoder ───────────────────────────────────────────────────────

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
/// // MOVEQ #0, D0 = 0x7000
/// assert_eq!(decode(0x7000), Instruction::MoveQ);
/// ```
#[must_use]
pub fn decode(opcode: u16) -> Instruction {
    match opcode >> 12 {
        0b0000 => decode_group0(opcode),
        0b0001..=0b0011 => decode_move(opcode),
        0b0100 => decode_group4(opcode),
        0b0101 => decode_group5(opcode),
        0b0110 => decode_group6(opcode),
        0b0111 => {
            if opcode & 0x0100 == 0 {
                Instruction::MoveQ
            } else {
                Instruction::Illegal
            }
        }
        0b1000 => decode_group8(opcode),
        0b1001 => decode_group9(opcode),
        0b1010 => Instruction::LineA,
        0b1011 => decode_group_b(opcode),
        0b1100 => decode_group_c(opcode),
        0b1101 => decode_group_d(opcode),
        0b1110 => decode_group_e(opcode),
        0b1111 => Instruction::LineF,
        _ => Instruction::Illegal,
    }
}

/// Group 0 (0000): Bit manipulation / MOVEP / Immediate operations.
fn decode_group0(opcode: u16) -> Instruction {
    let hi_reg = ((opcode >> 9) & 7) as u8;

    // If bit 8 is set, this is a dynamic bit operation or MOVEP
    if opcode & 0x0100 != 0 {
        let mode = ((opcode >> 3) & 7) as u8;
        let _ea_reg = (opcode & 7) as u8;

        // MOVEP: mode == 001 (address register direct)
        if mode == 1 {
            let size = if opcode & 0x0040 != 0 {
                InstructionSize::Long
            } else {
                InstructionSize::Word
            };
            return Instruction::MoveP(size);
        }

        // Dynamic bit operations (bit number in register)
        return match (opcode >> 6) & 3 {
            0 => Instruction::BTst,
            1 => Instruction::BChg,
            2 => Instruction::BClr,
            3 => Instruction::BSet,
            _ => Instruction::Illegal,
        };
    }

    // Bit 8 is clear: immediate operations or static bit operations
    match hi_reg {
        0 => {
            // ORI
            let _mode = ((opcode >> 3) & 7) as u8;
            let _ea_reg = (opcode & 7) as u8;
            let size_bits = ((opcode >> 6) & 3) as u8;
            // ORI to CCR: 0000 0000 0011 1100
            if opcode == 0x003C {
                return Instruction::OriCcr;
            }
            // ORI to SR: 0000 0000 0111 1100
            if opcode == 0x007C {
                return Instruction::OriSr;
            }
            match decode_size(size_bits) {
                Some(size) => Instruction::OrI(size),
                None => Instruction::Illegal,
            }
        }
        1 => {
            // ANDI
            if opcode == 0x023C {
                return Instruction::AndiCcr;
            }
            if opcode == 0x027C {
                return Instruction::AndiSr;
            }
            let size_bits = ((opcode >> 6) & 3) as u8;
            match decode_size(size_bits) {
                Some(size) => Instruction::AndI(size),
                None => Instruction::Illegal,
            }
        }
        2 => {
            // SUBI
            let size_bits = ((opcode >> 6) & 3) as u8;
            match decode_size(size_bits) {
                Some(size) => Instruction::SubI(size),
                None => Instruction::Illegal,
            }
        }
        3 => {
            // ADDI
            let size_bits = ((opcode >> 6) & 3) as u8;
            match decode_size(size_bits) {
                Some(size) => Instruction::AddI(size),
                None => Instruction::Illegal,
            }
        }
        4 => {
            // Static bit operations (bit number is immediate)
            match (opcode >> 6) & 3 {
                0 => Instruction::BTst,
                1 => Instruction::BChg,
                2 => Instruction::BClr,
                3 => Instruction::BSet,
                _ => Instruction::Illegal,
            }
        }
        5 => {
            // EORI
            if opcode == 0x0A3C {
                return Instruction::EoriCcr;
            }
            if opcode == 0x0A7C {
                return Instruction::EoriSr;
            }
            let size_bits = ((opcode >> 6) & 3) as u8;
            match decode_size(size_bits) {
                Some(size) => Instruction::EorI(size),
                None => Instruction::Illegal,
            }
        }
        6 => {
            // CMPI
            let size_bits = ((opcode >> 6) & 3) as u8;
            match decode_size(size_bits) {
                Some(size) => Instruction::CmpI(size),
                None => Instruction::Illegal,
            }
        }
        _ => Instruction::Illegal,
    }
}

/// Groups 1-3 (0001, 0010, 0011): MOVE / MOVEA.
fn decode_move(opcode: u16) -> Instruction {
    let size_bits = ((opcode >> 12) & 3) as u8;
    let size = match decode_move_size(size_bits) {
        Some(s) => s,
        None => return Instruction::Illegal,
    };

    // Destination mode is bits 8-6
    let dst_mode = ((opcode >> 6) & 7) as u8;

    // MOVEA: destination mode == 001 (address register direct)
    // Only valid for word and long
    if dst_mode == 1 {
        match size {
            InstructionSize::Byte => return Instruction::Illegal,
            _ => return Instruction::MoveA(size),
        }
    }

    Instruction::Move(size)
}

/// Group 4 (0100): Miscellaneous.
fn decode_group4(opcode: u16) -> Instruction {
    // Check fixed opcodes first
    match opcode {
        0x4E70 => return Instruction::ResetInstr,
        0x4E71 => return Instruction::Nop,
        0x4E72 => return Instruction::Stop,
        0x4E73 => return Instruction::Rte,
        0x4E75 => return Instruction::Rts,
        0x4E76 => return Instruction::TrapV,
        0x4E77 => return Instruction::Rtr,
        _ => {}
    }

    let _bits_11_8 = (opcode >> 8) & 0xF;
    let bits_7_6 = (opcode >> 6) & 3;
    let ea_mode = ((opcode >> 3) & 7) as u8;
    let _ea_reg = (opcode & 7) as u8;

    // TRAP: 0100 1110 0100 vvvv
    if opcode & 0xFFF0 == 0x4E40 {
        return Instruction::Trap;
    }

    // LINK: 0100 1110 0101 0rrr
    if opcode & 0xFFF8 == 0x4E50 {
        return Instruction::Link;
    }

    // UNLK: 0100 1110 0101 1rrr
    if opcode & 0xFFF8 == 0x4E58 {
        return Instruction::Unlk;
    }

    // MOVE USP: 0100 1110 0110 drrr
    if opcode & 0xFFF0 == 0x4E60 {
        return Instruction::MoveUsp;
    }

    // SWAP: 0100 1000 0100 0rrr
    if opcode & 0xFFF8 == 0x4840 {
        return Instruction::Swap;
    }

    // PEA: 0100 1000 01mm mrrr (mode != 000)
    if opcode & 0xFFC0 == 0x4840 && ea_mode != 0 {
        return Instruction::Pea;
    }

    // EXT.W: 0100 1000 1000 0rrr
    if opcode & 0xFFF8 == 0x4880 {
        return Instruction::Ext(InstructionSize::Word);
    }

    // EXT.L: 0100 1000 1100 0rrr
    if opcode & 0xFFF8 == 0x48C0 {
        return Instruction::Ext(InstructionSize::Long);
    }

    // MOVEM: 0100 1d00 1smm mrrr
    if opcode & 0xFB80 == 0x4880 {
        let size = if opcode & 0x0040 != 0 {
            InstructionSize::Long
        } else {
            InstructionSize::Word
        };
        return Instruction::Movem(size);
    }

    // LEA: 0100 rrr1 11mm mrrr
    if opcode & 0xF1C0 == 0x41C0 {
        return Instruction::Lea;
    }

    // JSR: 0100 1110 10mm mrrr
    if opcode & 0xFFC0 == 0x4E80 {
        return Instruction::Jsr;
    }

    // JMP: 0100 1110 11mm mrrr
    if opcode & 0xFFC0 == 0x4EC0 {
        return Instruction::Jmp;
    }

    // MOVE from SR: 0100 0000 11mm mrrr
    if opcode & 0xFFC0 == 0x40C0 {
        return Instruction::MoveFromSr;
    }

    // MOVE to CCR: 0100 0100 11mm mrrr
    if opcode & 0xFFC0 == 0x44C0 {
        return Instruction::MoveToCcr;
    }

    // MOVE to SR: 0100 0110 11mm mrrr
    if opcode & 0xFFC0 == 0x46C0 {
        return Instruction::MoveToSr;
    }

    // NEG: 0100 0100 ssmm mrrr
    if opcode & 0xFF00 == 0x4400 && bits_7_6 != 3 {
        let size_bits = ((opcode >> 6) & 3) as u8;
        return match decode_size(size_bits) {
            Some(size) => Instruction::Neg(size),
            None => Instruction::Illegal,
        };
    }

    // NEGX: 0100 0000 ssmm mrrr
    if opcode & 0xFF00 == 0x4000 && bits_7_6 != 3 {
        let size_bits = ((opcode >> 6) & 3) as u8;
        return match decode_size(size_bits) {
            Some(size) => Instruction::NegX(size),
            None => Instruction::Illegal,
        };
    }

    // CLR: 0100 0010 ssmm mrrr
    if opcode & 0xFF00 == 0x4200 && bits_7_6 != 3 {
        let size_bits = ((opcode >> 6) & 3) as u8;
        return match decode_size(size_bits) {
            Some(size) => Instruction::Clr(size),
            None => Instruction::Illegal,
        };
    }

    // NOT: 0100 0110 ssmm mrrr
    if opcode & 0xFF00 == 0x4600 && bits_7_6 != 3 {
        let size_bits = ((opcode >> 6) & 3) as u8;
        return match decode_size(size_bits) {
            Some(size) => Instruction::Not(size),
            None => Instruction::Illegal,
        };
    }

    // TST: 0100 1010 ssmm mrrr
    if opcode & 0xFF00 == 0x4A00 {
        let size_bits = ((opcode >> 6) & 3) as u8;
        return match decode_size(size_bits) {
            Some(size) => Instruction::Tst(size),
            None => Instruction::Illegal,
        };
    }

    Instruction::Illegal
}

/// Group 5 (0101): ADDQ / SUBQ / Scc / DBcc.
fn decode_group5(opcode: u16) -> Instruction {
    let size_bits = ((opcode >> 6) & 3) as u8;

    // Scc / DBcc: size field = 11
    if size_bits == 3 {
        let ea_mode = ((opcode >> 3) & 7) as u8;
        let _ea_reg = (opcode & 7) as u8;
        // DBcc: mode = 001 (data register, but encoded specially)
        if ea_mode == 1 {
            return Instruction::DBcc;
        }
        return Instruction::Scc;
    }

    let size = match decode_size(size_bits) {
        Some(s) => s,
        None => return Instruction::Illegal,
    };

    // Bit 8: 0 = ADDQ, 1 = SUBQ
    if opcode & 0x0100 == 0 {
        Instruction::AddQ(size)
    } else {
        Instruction::SubQ(size)
    }
}

/// Group 6 (0110): Bcc / BRA / BSR.
fn decode_group6(opcode: u16) -> Instruction {
    let condition = ((opcode >> 8) & 0xF) as u8;
    match condition {
        0 => Instruction::Bcc, // BRA (condition = 0000 = True)
        1 => Instruction::Bsr, // BSR (condition = 0001)
        _ => Instruction::Bcc, // All other Bcc
    }
}

/// Group 8 (1000): OR / DIVU / DIVS / SBCD.
fn decode_group8(opcode: u16) -> Instruction {
    let opmode = ((opcode >> 6) & 7) as u8;

    match opmode {
        // OR <ea>, Dn (byte/word/long)
        0 => Instruction::Or(InstructionSize::Byte),
        1 => Instruction::Or(InstructionSize::Word),
        2 => Instruction::Or(InstructionSize::Long),
        // DIVU: 1000 rrr0 11mm mrrr
        3 => Instruction::DivU,
        // OR Dn, <ea> (byte/word)
        4 => Instruction::Or(InstructionSize::Byte),
        5 => Instruction::Or(InstructionSize::Word),
        6 => Instruction::Or(InstructionSize::Long),
        // DIVS: 1000 rrr1 11mm mrrr
        7 => Instruction::DivS,
        _ => Instruction::Illegal,
    }
}

/// Group 9 (1001): SUB / SUBA / SUBX.
fn decode_group9(opcode: u16) -> Instruction {
    let opmode = ((opcode >> 6) & 7) as u8;
    let ea_mode = ((opcode >> 3) & 7) as u8;

    match opmode {
        // SUB <ea>, Dn
        0 => Instruction::Sub(InstructionSize::Byte),
        1 => Instruction::Sub(InstructionSize::Word),
        2 => Instruction::Sub(InstructionSize::Long),
        // SUBA.W
        3 => Instruction::SubA(InstructionSize::Word),
        // SUB Dn, <ea> / SUBX
        4 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::SubX(InstructionSize::Byte)
            } else {
                Instruction::Sub(InstructionSize::Byte)
            }
        }
        5 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::SubX(InstructionSize::Word)
            } else {
                Instruction::Sub(InstructionSize::Word)
            }
        }
        6 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::SubX(InstructionSize::Long)
            } else {
                Instruction::Sub(InstructionSize::Long)
            }
        }
        // SUBA.L
        7 => Instruction::SubA(InstructionSize::Long),
        _ => Instruction::Illegal,
    }
}

/// Group B (1011): CMP / CMPA / EOR / CMPM.
fn decode_group_b(opcode: u16) -> Instruction {
    let opmode = ((opcode >> 6) & 7) as u8;
    let ea_mode = ((opcode >> 3) & 7) as u8;

    match opmode {
        // CMP <ea>, Dn
        0 => Instruction::Cmp(InstructionSize::Byte),
        1 => Instruction::Cmp(InstructionSize::Word),
        2 => Instruction::Cmp(InstructionSize::Long),
        // CMPA.W
        3 => Instruction::CmpA(InstructionSize::Word),
        // EOR Dn, <ea> / CMPM
        4 => {
            if ea_mode == 1 {
                Instruction::CmpM(InstructionSize::Byte)
            } else {
                Instruction::Eor(InstructionSize::Byte)
            }
        }
        5 => {
            if ea_mode == 1 {
                Instruction::CmpM(InstructionSize::Word)
            } else {
                Instruction::Eor(InstructionSize::Word)
            }
        }
        6 => {
            if ea_mode == 1 {
                Instruction::CmpM(InstructionSize::Long)
            } else {
                Instruction::Eor(InstructionSize::Long)
            }
        }
        // CMPA.L
        7 => Instruction::CmpA(InstructionSize::Long),
        _ => Instruction::Illegal,
    }
}

/// Group C (1100): AND / MULU / MULS / EXG / ABCD.
fn decode_group_c(opcode: u16) -> Instruction {
    let opmode = ((opcode >> 6) & 7) as u8;
    let ea_mode = ((opcode >> 3) & 7) as u8;

    match opmode {
        // AND <ea>, Dn
        0 => Instruction::And(InstructionSize::Byte),
        1 => Instruction::And(InstructionSize::Word),
        2 => Instruction::And(InstructionSize::Long),
        // MULU
        3 => Instruction::MulU,
        // AND Dn, <ea> / ABCD / EXG
        4 => {
            if ea_mode == 0 {
                // ABCD Dy, Dx — skip for now
                Instruction::Illegal
            } else {
                Instruction::And(InstructionSize::Byte)
            }
        }
        5 => {
            if ea_mode == 0 || ea_mode == 1 {
                // EXG
                Instruction::Exg
            } else {
                Instruction::And(InstructionSize::Word)
            }
        }
        6 => {
            if ea_mode == 1 {
                // EXG Dn, An
                Instruction::Exg
            } else {
                Instruction::And(InstructionSize::Long)
            }
        }
        // MULS
        7 => Instruction::MulS,
        _ => Instruction::Illegal,
    }
}

/// Group D (1101): ADD / ADDA / ADDX.
fn decode_group_d(opcode: u16) -> Instruction {
    let opmode = ((opcode >> 6) & 7) as u8;
    let ea_mode = ((opcode >> 3) & 7) as u8;

    match opmode {
        // ADD <ea>, Dn
        0 => Instruction::Add(InstructionSize::Byte),
        1 => Instruction::Add(InstructionSize::Word),
        2 => Instruction::Add(InstructionSize::Long),
        // ADDA.W
        3 => Instruction::AddA(InstructionSize::Word),
        // ADD Dn, <ea> / ADDX
        4 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::AddX(InstructionSize::Byte)
            } else {
                Instruction::Add(InstructionSize::Byte)
            }
        }
        5 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::AddX(InstructionSize::Word)
            } else {
                Instruction::Add(InstructionSize::Word)
            }
        }
        6 => {
            if ea_mode == 0 || ea_mode == 1 {
                Instruction::AddX(InstructionSize::Long)
            } else {
                Instruction::Add(InstructionSize::Long)
            }
        }
        // ADDA.L
        7 => Instruction::AddA(InstructionSize::Long),
        _ => Instruction::Illegal,
    }
}

/// Group E (1110): Shift / Rotate.
fn decode_group_e(opcode: u16) -> Instruction {
    let size_bits = ((opcode >> 6) & 3) as u8;

    // Memory shifts: size == 11, single word ea
    if size_bits == 3 {
        return Instruction::ShiftMem;
    }

    let size = match decode_size(size_bits) {
        Some(s) => s,
        None => return Instruction::Illegal,
    };

    // Register shifts: type is bits 4-3
    let shift_type = ((opcode >> 3) & 3) as u8;
    match shift_type {
        0 => Instruction::Asd(size),
        1 => Instruction::Lsd(size),
        2 => Instruction::Roxd(size),
        3 => Instruction::Rod(size),
        _ => Instruction::Illegal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Fixed opcodes ===
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
    fn decode_rtr() {
        assert_eq!(decode(0x4E77), Instruction::Rtr);
    }
    #[test]
    fn decode_reset() {
        assert_eq!(decode(0x4E70), Instruction::ResetInstr);
    }
    #[test]
    fn decode_stop() {
        assert_eq!(decode(0x4E72), Instruction::Stop);
    }
    #[test]
    fn decode_trapv() {
        assert_eq!(decode(0x4E76), Instruction::TrapV);
    }

    // === MOVEQ ===
    #[test]
    fn decode_moveq() {
        assert_eq!(decode(0x7000), Instruction::MoveQ); // MOVEQ #0, D0
        assert_eq!(decode(0x762A), Instruction::MoveQ); // MOVEQ #42, D3
        assert_eq!(decode(0x70FF), Instruction::MoveQ); // MOVEQ #-1, D0
    }

    // === MOVE ===
    #[test]
    fn decode_move_byte() {
        // MOVE.B D0, D1 = 0001 001 000 000 000 = 0x1200
        assert_eq!(decode(0x1200), Instruction::Move(InstructionSize::Byte));
    }
    #[test]
    fn decode_move_word() {
        // MOVE.W D0, D1 = 0011 001 000 000 000 = 0x3200
        assert_eq!(decode(0x3200), Instruction::Move(InstructionSize::Word));
    }
    #[test]
    fn decode_move_long() {
        // MOVE.L D0, D1 = 0010 001 000 000 000 = 0x2200
        assert_eq!(decode(0x2200), Instruction::Move(InstructionSize::Long));
    }
    #[test]
    fn decode_movea() {
        // MOVEA.L D0, A0 = 0010 000 001 000 000 = 0x2040
        assert_eq!(decode(0x2040), Instruction::MoveA(InstructionSize::Long));
        // MOVEA.W D0, A0 = 0011 000 001 000 000 = 0x3040
        assert_eq!(decode(0x3040), Instruction::MoveA(InstructionSize::Word));
    }

    // === LEA ===
    #[test]
    fn decode_lea() {
        // LEA (A0), A1 = 0100 001 111 010 000 = 0x43D0
        assert_eq!(decode(0x43D0), Instruction::Lea);
    }

    // === Branches ===
    #[test]
    fn decode_bra() {
        // BRA.S $+4 = 0110 0000 0000 0010 = 0x6002
        assert_eq!(decode(0x6002), Instruction::Bcc);
    }
    #[test]
    fn decode_bsr() {
        // BSR.S $+4 = 0110 0001 0000 0010 = 0x6102
        assert_eq!(decode(0x6102), Instruction::Bsr);
    }
    #[test]
    fn decode_beq() {
        // BEQ.S $+4 = 0110 0111 0000 0010 = 0x6702
        assert_eq!(decode(0x6702), Instruction::Bcc);
    }
    #[test]
    fn decode_bne() {
        // BNE.S $+4 = 0110 0110 0000 0010 = 0x6602
        assert_eq!(decode(0x6602), Instruction::Bcc);
    }

    // === Arithmetic ===
    #[test]
    fn decode_add() {
        // ADD.W D0, D1 = 1101 001 001 000 000 = 0xD240
        assert_eq!(decode(0xD240), Instruction::Add(InstructionSize::Word));
    }
    #[test]
    fn decode_adda() {
        // ADDA.L D0, A0 = 1101 000 111 000 000 = 0xD1C0
        assert_eq!(decode(0xD1C0), Instruction::AddA(InstructionSize::Long));
    }
    #[test]
    fn decode_addi() {
        // ADDI.W #imm, D0 = 0000 0110 0100 0000 = 0x0640
        assert_eq!(decode(0x0640), Instruction::AddI(InstructionSize::Word));
    }
    #[test]
    fn decode_addq() {
        // ADDQ.L #1, A0 = 0101 001 0 10 001 000 = 0x5288
        assert_eq!(decode(0x5288), Instruction::AddQ(InstructionSize::Long));
    }
    #[test]
    fn decode_sub() {
        // SUB.L D0, D1 = 1001 001 010 000 000 = 0x9280
        assert_eq!(decode(0x9280), Instruction::Sub(InstructionSize::Long));
    }
    #[test]
    fn decode_subi() {
        // SUBI.L #imm, D0 = 0000 0100 1000 0000 = 0x0480
        assert_eq!(decode(0x0480), Instruction::SubI(InstructionSize::Long));
    }
    #[test]
    fn decode_subq() {
        // SUBQ.W #1, D0 = 0101 001 1 01 000 000 = 0x5340
        assert_eq!(decode(0x5340), Instruction::SubQ(InstructionSize::Word));
    }

    // === Logic ===
    #[test]
    fn decode_and() {
        // AND.L D0, D1 = 1100 001 010 000 000 = 0xC280
        assert_eq!(decode(0xC280), Instruction::And(InstructionSize::Long));
    }
    #[test]
    fn decode_andi() {
        // ANDI.B #imm, D0 = 0000 0010 0000 0000 = 0x0200
        assert_eq!(decode(0x0200), Instruction::AndI(InstructionSize::Byte));
    }
    #[test]
    fn decode_or() {
        // OR.W (A0), D0 = 1000 000 001 010 000 = 0x8050
        assert_eq!(decode(0x8050), Instruction::Or(InstructionSize::Word));
    }
    #[test]
    fn decode_ori() {
        // ORI.W #imm, D0 = 0000 0000 0100 0000 = 0x0040
        assert_eq!(decode(0x0040), Instruction::OrI(InstructionSize::Word));
    }
    #[test]
    fn decode_eor() {
        // EOR.L D0, D1 = 1011 000 110 000 001 = 0xB181
        assert_eq!(decode(0xB181), Instruction::Eor(InstructionSize::Long));
    }
    #[test]
    fn decode_not() {
        // NOT.L D0 = 0100 0110 1000 0000 = 0x4680
        assert_eq!(decode(0x4680), Instruction::Not(InstructionSize::Long));
    }

    // === Compare ===
    #[test]
    fn decode_cmp() {
        // CMP.L D0, D1 = 1011 001 010 000 000 = 0xB280
        assert_eq!(decode(0xB280), Instruction::Cmp(InstructionSize::Long));
    }
    #[test]
    fn decode_cmpi() {
        // CMPI.W #imm, D0 = 0000 1100 0100 0000 = 0x0C40
        assert_eq!(decode(0x0C40), Instruction::CmpI(InstructionSize::Word));
    }
    #[test]
    fn decode_tst() {
        // TST.L D0 = 0100 1010 1000 0000 = 0x4A80
        assert_eq!(decode(0x4A80), Instruction::Tst(InstructionSize::Long));
    }

    // === CLR / NEG / SWAP / EXT ===
    #[test]
    fn decode_clr() {
        // CLR.W D0 = 0100 0010 0100 0000 = 0x4240
        assert_eq!(decode(0x4240), Instruction::Clr(InstructionSize::Word));
    }
    #[test]
    fn decode_neg() {
        // NEG.L D0 = 0100 0100 1000 0000 = 0x4480
        assert_eq!(decode(0x4480), Instruction::Neg(InstructionSize::Long));
    }
    #[test]
    fn decode_swap() {
        // SWAP D0 = 0100 1000 0100 0000 = 0x4840
        assert_eq!(decode(0x4840), Instruction::Swap);
    }
    #[test]
    fn decode_ext_word() {
        // EXT.W D0 = 0100 1000 1000 0000 = 0x4880
        assert_eq!(decode(0x4880), Instruction::Ext(InstructionSize::Word));
    }
    #[test]
    fn decode_ext_long() {
        // EXT.L D0 = 0100 1000 1100 0000 = 0x48C0
        assert_eq!(decode(0x48C0), Instruction::Ext(InstructionSize::Long));
    }

    // === Shifts ===
    #[test]
    fn decode_lsr() {
        // LSR.W #1, D0 = 1110 001 0 01 0 01 000 = 0xE248
        assert_eq!(decode(0xE248), Instruction::Lsd(InstructionSize::Word));
    }
    #[test]
    fn decode_asl() {
        // ASL.L D1, D0 = 1110 001 1 10 1 00 000 = 0xE3A0
        assert_eq!(decode(0xE3A0), Instruction::Asd(InstructionSize::Long));
    }

    // === Bit operations ===
    #[test]
    fn decode_btst_dynamic() {
        // BTST D0, D1 = 0000 000 1 00 000 001 = 0x0101
        assert_eq!(decode(0x0101), Instruction::BTst);
    }
    #[test]
    fn decode_btst_static() {
        // BTST #imm, D0 = 0000 1000 0000 0000 = 0x0800
        assert_eq!(decode(0x0800), Instruction::BTst);
    }

    // === Multiply / Divide ===
    #[test]
    fn decode_mulu() {
        // MULU D0, D1 = 1100 001 011 000 000 = 0xC2C0
        assert_eq!(decode(0xC2C0), Instruction::MulU);
    }
    #[test]
    fn decode_divu() {
        // DIVU D0, D1 = 1000 001 011 000 000 = 0x82C0
        assert_eq!(decode(0x82C0), Instruction::DivU);
    }

    // === Misc ===
    #[test]
    fn decode_jsr() {
        // JSR (A0) = 0100 1110 1001 0000 = 0x4E90
        assert_eq!(decode(0x4E90), Instruction::Jsr);
    }
    #[test]
    fn decode_jmp() {
        // JMP (A0) = 0100 1110 1101 0000 = 0x4ED0
        assert_eq!(decode(0x4ED0), Instruction::Jmp);
    }
    #[test]
    fn decode_dbcc() {
        // DBRA D0, disp = 0101 0001 1100 1000 = 0x51C8
        assert_eq!(decode(0x51C8), Instruction::DBcc);
    }
    #[test]
    fn decode_trap() {
        // TRAP #0 = 0100 1110 0100 0000 = 0x4E40
        assert_eq!(decode(0x4E40), Instruction::Trap);
    }
    #[test]
    fn decode_link() {
        // LINK A6, #disp = 0100 1110 0101 0110 = 0x4E56
        assert_eq!(decode(0x4E56), Instruction::Link);
    }
    #[test]
    fn decode_unlk() {
        // UNLK A6 = 0100 1110 0101 1110 = 0x4E5E
        assert_eq!(decode(0x4E5E), Instruction::Unlk);
    }
    #[test]
    fn decode_movem() {
        // MOVEM.L <list>, -(A7) = 0100 1000 1110 0111 = 0x48E7 (followed by register mask)
        assert_eq!(decode(0x48E7), Instruction::Movem(InstructionSize::Long));
    }
    #[test]
    fn decode_scc() {
        // ST D0 (Scc with True condition) = 0101 0000 1100 0000 = 0x50C0
        assert_eq!(decode(0x50C0), Instruction::Scc);
    }

    // === SR/CCR operations ===
    #[test]
    fn decode_ori_ccr() {
        assert_eq!(decode(0x003C), Instruction::OriCcr);
    }
    #[test]
    fn decode_ori_sr() {
        assert_eq!(decode(0x007C), Instruction::OriSr);
    }
    #[test]
    fn decode_andi_ccr() {
        assert_eq!(decode(0x023C), Instruction::AndiCcr);
    }
    #[test]
    fn decode_andi_sr() {
        assert_eq!(decode(0x027C), Instruction::AndiSr);
    }

    // === Line traps ===
    #[test]
    fn decode_line_a() {
        assert_eq!(decode(0xA000), Instruction::LineA);
    }
    #[test]
    fn decode_line_f() {
        assert_eq!(decode(0xF000), Instruction::LineF);
    }

    // === Exhaustiveness: every opcode decodes without panic ===
    #[test]
    fn all_opcodes_decode_without_panic() {
        for opcode in 0u16..=0xFFFF {
            let _ = decode(opcode);
        }
    }
}

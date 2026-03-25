//! 68000 instruction executor.
//!
//! Takes a decoded [`Instruction`], resolves addressing modes, reads/writes
//! operands, performs the ALU operation, sets condition codes, and returns
//! the number of CPU cycles consumed.
//!
//! The decoder produces an `Instruction` variant from the first opcode word.
//! The executor reads any additional extension words (immediates,
//! displacements) directly from the instruction stream via the bus.

use super::StatusRegister;
use super::decode::{self, AddressingMode, Instruction, InstructionSize};
use super::engine::Cpu;

// ── Bus trait ────────────────────────────────────────────────────────────

/// Memory bus interface for the 68000.
///
/// Implementors provide byte and word read/write access. Long (32-bit)
/// reads/writes are synthesized from two word operations.
pub trait Bus {
    /// Reads a byte from the given address.
    fn read_byte(&mut self, addr: u32) -> u8;
    /// Reads a big-endian 16-bit word from the given address.
    fn read_word(&mut self, addr: u32) -> u16;
    /// Writes a byte to the given address.
    fn write_byte(&mut self, addr: u32, val: u8);
    /// Writes a big-endian 16-bit word to the given address.
    fn write_word(&mut self, addr: u32, val: u16);
}

// ── Size helpers ─────────────────────────────────────────────────────────

/// Returns the byte count for an instruction size (1, 2, or 4).
#[must_use]
const fn size_bytes(size: InstructionSize) -> u32 {
    match size {
        InstructionSize::Byte => 1,
        InstructionSize::Word => 2,
        InstructionSize::Long => 4,
    }
}

/// Returns the bit count for an instruction size (8, 16, or 32).
#[must_use]
const fn size_bits(size: InstructionSize) -> u32 {
    match size {
        InstructionSize::Byte => 8,
        InstructionSize::Word => 16,
        InstructionSize::Long => 32,
    }
}

/// Returns the MSB mask for a given size.
#[must_use]
const fn msb_mask(size: InstructionSize) -> u32 {
    match size {
        InstructionSize::Byte => 0x80,
        InstructionSize::Word => 0x8000,
        InstructionSize::Long => 0x8000_0000,
    }
}

/// Masks a value to the given size.
#[must_use]
const fn mask_value(val: u32, size: InstructionSize) -> u32 {
    match size {
        InstructionSize::Byte => val & 0xFF,
        InstructionSize::Word => val & 0xFFFF,
        InstructionSize::Long => val,
    }
}

/// Sign-extends a value of the given size to 32 bits.
#[must_use]
fn sign_extend(val: u32, size: InstructionSize) -> u32 {
    match size {
        InstructionSize::Byte => (val as u8) as i8 as i32 as u32,
        InstructionSize::Word => (val as u16) as i16 as i32 as u32,
        InstructionSize::Long => val,
    }
}

// ── PC fetch helpers ─────────────────────────────────────────────────────

/// Fetches the next word from the instruction stream and advances PC by 2.
fn fetch_word(cpu: &mut Cpu, bus: &mut dyn Bus) -> u16 {
    let addr = cpu.pc & 0x00FF_FFFF;
    let val = bus.read_word(addr);
    cpu.pc = cpu.pc.wrapping_add(2);
    val
}

/// Fetches a 32-bit long from the instruction stream (two words, big-endian).
fn fetch_long(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let hi = u32::from(fetch_word(cpu, bus));
    let lo = u32::from(fetch_word(cpu, bus));
    (hi << 16) | lo
}

/// Reads a big-endian u32 from the bus at the given address.
fn read_long(bus: &mut dyn Bus, addr: u32) -> u32 {
    let hi = u32::from(bus.read_word(addr & 0x00FF_FFFF));
    let lo = u32::from(bus.read_word(addr.wrapping_add(2) & 0x00FF_FFFF));
    (hi << 16) | lo
}

/// Writes a big-endian u32 to the bus at the given address.
fn write_long(bus: &mut dyn Bus, addr: u32, val: u32) {
    bus.write_word(addr & 0x00FF_FFFF, (val >> 16) as u16);
    bus.write_word(addr.wrapping_add(2) & 0x00FF_FFFF, val as u16);
}

// ── Addressing mode resolution ───────────────────────────────────────────

/// Decodes the 6-bit EA from the opcode (source position: bits 5-3 = mode, bits 2-0 = reg).
#[must_use]
fn src_ea(opcode: u16) -> AddressingMode {
    let mode = ((opcode >> 3) & 7) as u8;
    let reg = (opcode & 7) as u8;
    decode::decode_ea(mode, reg)
}

/// Decodes the destination EA for MOVE instructions (bits 11-9 = reg, bits 8-6 = mode).
#[must_use]
fn dst_ea_move(opcode: u16) -> AddressingMode {
    let mode = ((opcode >> 6) & 7) as u8;
    let reg = ((opcode >> 9) & 7) as u8;
    decode::decode_ea(mode, reg)
}

/// Computes the effective address for modes that have a memory address.
/// Returns the address in the 24-bit space. For register-direct modes,
/// this is meaningless -- callers should use `read_ea` / `write_ea` instead.
fn resolve_ea(cpu: &mut Cpu, ea: AddressingMode, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    match ea {
        AddressingMode::DataDirect(_) | AddressingMode::AddrDirect(_) => {
            // No memory address for register-direct modes.
            0
        }
        AddressingMode::AddrIndirect(reg) => cpu.read_a(reg) & 0x00FF_FFFF,
        AddressingMode::AddrPostInc(reg) => {
            let addr = cpu.read_a(reg);
            // A7 always increments by at least 2 for byte ops (keep SP word-aligned).
            let inc = if reg == 7 && size == InstructionSize::Byte {
                2
            } else {
                size_bytes(size)
            };
            cpu.write_a(reg, addr.wrapping_add(inc));
            addr & 0x00FF_FFFF
        }
        AddressingMode::AddrPreDec(reg) => {
            let dec = if reg == 7 && size == InstructionSize::Byte {
                2
            } else {
                size_bytes(size)
            };
            let addr = cpu.read_a(reg).wrapping_sub(dec);
            cpu.write_a(reg, addr);
            addr & 0x00FF_FFFF
        }
        AddressingMode::AddrDisp(reg) => {
            let disp = fetch_word(cpu, bus) as i16 as i32;
            (cpu.read_a(reg) as i32).wrapping_add(disp) as u32 & 0x00FF_FFFF
        }
        AddressingMode::AddrIndex(reg) => {
            let ext = fetch_word(cpu, bus);
            let base = cpu.read_a(reg);
            compute_index_ea(cpu, base, ext)
        }
        AddressingMode::AbsShort => {
            let addr = fetch_word(cpu, bus) as i16 as i32 as u32;
            addr & 0x00FF_FFFF
        }
        AddressingMode::AbsLong => {
            let addr = fetch_long(cpu, bus);
            addr & 0x00FF_FFFF
        }
        AddressingMode::PcDisp => {
            let pc = cpu.pc; // PC of the extension word
            let disp = fetch_word(cpu, bus) as i16 as i32;
            (pc as i32).wrapping_add(disp) as u32 & 0x00FF_FFFF
        }
        AddressingMode::PcIndex => {
            let pc = cpu.pc; // PC of the extension word
            let ext = fetch_word(cpu, bus);
            compute_index_ea(cpu, pc, ext)
        }
        AddressingMode::Immediate => {
            // Immediate doesn't have an "address" in memory.
            // We return the PC pointing at the immediate data (will be read by read_ea).
            0
        }
    }
}

/// Computes a brief extension word index EA.
/// Extension word format: D/A | Reg(3) | W/L | 0 | disp(8)
fn compute_index_ea(cpu: &Cpu, base: u32, ext: u16) -> u32 {
    let disp = (ext & 0xFF) as i8 as i32;
    let idx_reg = ((ext >> 12) & 7) as u8;
    let idx_val = if ext & 0x8000 != 0 {
        // Address register
        cpu.read_a(idx_reg)
    } else {
        // Data register
        cpu.d[idx_reg as usize]
    };
    let idx_val = if ext & 0x0800 != 0 {
        // Long index
        idx_val as i32
    } else {
        // Word index (sign-extended)
        idx_val as i16 as i32
    };
    (base as i32).wrapping_add(disp).wrapping_add(idx_val) as u32 & 0x00FF_FFFF
}

/// Reads a value from an effective address.
fn read_ea(cpu: &mut Cpu, ea: AddressingMode, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    match ea {
        AddressingMode::DataDirect(reg) => mask_value(cpu.d[reg as usize], size),
        AddressingMode::AddrDirect(reg) => {
            // Address register reads always return full 32-bit value
            cpu.read_a(reg)
        }
        AddressingMode::Immediate => match size {
            InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
            InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
            InstructionSize::Long => fetch_long(cpu, bus),
        },
        _ => {
            let addr = resolve_ea(cpu, ea, size, bus);
            read_from_addr(bus, addr, size)
        }
    }
}

/// Reads a value from a memory address with the given size.
fn read_from_addr(bus: &mut dyn Bus, addr: u32, size: InstructionSize) -> u32 {
    let addr = addr & 0x00FF_FFFF;
    match size {
        InstructionSize::Byte => u32::from(bus.read_byte(addr)),
        InstructionSize::Word => u32::from(bus.read_word(addr & !1)),
        InstructionSize::Long => read_long(bus, addr & !1),
    }
}

/// Writes a value to an effective address.
fn write_ea(cpu: &mut Cpu, ea: AddressingMode, size: InstructionSize, bus: &mut dyn Bus, val: u32) {
    match ea {
        AddressingMode::DataDirect(reg) => {
            let r = &mut cpu.d[reg as usize];
            match size {
                InstructionSize::Byte => *r = (*r & !0xFF) | (val & 0xFF),
                InstructionSize::Word => *r = (*r & !0xFFFF) | (val & 0xFFFF),
                InstructionSize::Long => *r = val,
            }
        }
        AddressingMode::AddrDirect(reg) => {
            // Address register writes are always long (sign-extended for word)
            cpu.write_a(reg, val);
        }
        AddressingMode::Immediate => {
            // Can't write to immediate -- silently ignore
        }
        _ => {
            let addr = resolve_ea(cpu, ea, size, bus);
            write_to_addr(bus, addr, size, val);
        }
    }
}

/// Writes a value to a memory address with the given size.
fn write_to_addr(bus: &mut dyn Bus, addr: u32, size: InstructionSize, val: u32) {
    let addr = addr & 0x00FF_FFFF;
    match size {
        InstructionSize::Byte => bus.write_byte(addr, val as u8),
        InstructionSize::Word => bus.write_word(addr & !1, val as u16),
        InstructionSize::Long => write_long(bus, addr & !1, val),
    }
}

/// Reads EA and returns both the value and the resolved address.
/// Used for read-modify-write instructions where we need the address to write back.
fn read_ea_with_addr(
    cpu: &mut Cpu,
    ea: AddressingMode,
    size: InstructionSize,
    bus: &mut dyn Bus,
) -> (u32, u32) {
    match ea {
        AddressingMode::DataDirect(reg) => (mask_value(cpu.d[reg as usize], size), 0),
        AddressingMode::AddrDirect(reg) => (cpu.read_a(reg), 0),
        AddressingMode::Immediate => {
            let val = match size {
                InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
                InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
                InstructionSize::Long => fetch_long(cpu, bus),
            };
            (val, 0)
        }
        _ => {
            let addr = resolve_ea(cpu, ea, size, bus);
            (read_from_addr(bus, addr, size), addr)
        }
    }
}

// ── Flag computation ─────────────────────────────────────────────────────

/// Sets N and Z flags based on a result value. Clears V and C.
fn set_flags_nz(sr: &mut StatusRegister, result: u32, size: InstructionSize) {
    let masked = mask_value(result, size);
    sr.set_flag(StatusRegister::N, masked & msb_mask(size) != 0);
    sr.set_flag(StatusRegister::Z, masked == 0);
    sr.set_flag(StatusRegister::V, false);
    sr.set_flag(StatusRegister::C, false);
}

/// Sets all flags (X, N, Z, V, C) for an ADD operation.
fn set_flags_add(sr: &mut StatusRegister, src: u32, dst: u32, result: u32, size: InstructionSize) {
    let s = mask_value(src, size);
    let d = mask_value(dst, size);
    let r = mask_value(result, size);
    let msb = msb_mask(size);

    sr.set_flag(StatusRegister::N, r & msb != 0);
    sr.set_flag(StatusRegister::Z, r == 0);

    // Overflow: both operands same sign, result different sign
    let overflow = (s & msb == d & msb) && (r & msb != s & msb);
    sr.set_flag(StatusRegister::V, overflow);

    // Carry: unsigned overflow
    let carry = match size {
        InstructionSize::Byte => (u32::from(s as u8) + u32::from(d as u8)) > 0xFF,
        InstructionSize::Word => (u32::from(s as u16) + u32::from(d as u16)) > 0xFFFF,
        InstructionSize::Long => (s as u64 + d as u64) > 0xFFFF_FFFF,
    };
    sr.set_flag(StatusRegister::C, carry);
    sr.set_flag(StatusRegister::X, carry);
}

/// Sets all flags (X, N, Z, V, C) for a SUB operation (dst - src).
fn set_flags_sub(sr: &mut StatusRegister, src: u32, dst: u32, result: u32, size: InstructionSize) {
    let s = mask_value(src, size);
    let d = mask_value(dst, size);
    let r = mask_value(result, size);
    let msb = msb_mask(size);

    sr.set_flag(StatusRegister::N, r & msb != 0);
    sr.set_flag(StatusRegister::Z, r == 0);

    // Overflow: operands different sign and result sign differs from dest
    let overflow = (s & msb != d & msb) && (r & msb != d & msb);
    sr.set_flag(StatusRegister::V, overflow);

    // Borrow (carry)
    let borrow = s > d;
    sr.set_flag(StatusRegister::C, borrow);
    sr.set_flag(StatusRegister::X, borrow);
}

/// Sets NZVC flags for a CMP operation (like SUB but no X flag change).
fn set_flags_cmp(sr: &mut StatusRegister, src: u32, dst: u32, result: u32, size: InstructionSize) {
    let s = mask_value(src, size);
    let d = mask_value(dst, size);
    let r = mask_value(result, size);
    let msb = msb_mask(size);

    sr.set_flag(StatusRegister::N, r & msb != 0);
    sr.set_flag(StatusRegister::Z, r == 0);

    let overflow = (s & msb != d & msb) && (r & msb != d & msb);
    sr.set_flag(StatusRegister::V, overflow);

    let borrow = s > d;
    sr.set_flag(StatusRegister::C, borrow);
}

// ── Condition code evaluation ────────────────────────────────────────────

/// Evaluates a 68000 condition code (4-bit field from Bcc/DBcc/Scc).
#[must_use]
fn evaluate_condition(sr: &StatusRegister, condition: u8) -> bool {
    let c = sr.flag(StatusRegister::C);
    let v = sr.flag(StatusRegister::V);
    let z = sr.flag(StatusRegister::Z);
    let n = sr.flag(StatusRegister::N);

    match condition & 0xF {
        0x0 => true,           // T (true/always)
        0x1 => false,          // F (false/never)
        0x2 => !c && !z,       // HI (higher)
        0x3 => c || z,         // LS (lower or same)
        0x4 => !c,             // CC (carry clear)
        0x5 => c,              // CS (carry set)
        0x6 => !z,             // NE (not equal)
        0x7 => z,              // EQ (equal)
        0x8 => !v,             // VC (overflow clear)
        0x9 => v,              // VS (overflow set)
        0xA => !n,             // PL (plus)
        0xB => n,              // MI (minus)
        0xC => n == v,         // GE (greater or equal)
        0xD => n != v,         // LT (less than)
        0xE => !z && (n == v), // GT (greater than)
        0xF => z || (n != v),  // LE (less or equal)
        _ => false,
    }
}

// ── Stack operations ─────────────────────────────────────────────────────

/// Pushes a 32-bit long onto the stack (pre-decrement A7 by 4).
fn push_long(cpu: &mut Cpu, bus: &mut dyn Bus, val: u32) {
    let sp = cpu.sp().wrapping_sub(4);
    cpu.set_sp(sp);
    write_long(bus, sp & 0x00FF_FFFF, val);
}

/// Pops a 32-bit long from the stack (post-increment A7 by 4).
fn pop_long(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let sp = cpu.sp();
    cpu.set_sp(sp.wrapping_add(4));
    read_long(bus, sp & 0x00FF_FFFF)
}

/// Pushes a 16-bit word onto the stack (pre-decrement A7 by 2).
fn push_word(cpu: &mut Cpu, bus: &mut dyn Bus, val: u16) {
    let sp = cpu.sp().wrapping_sub(2);
    cpu.set_sp(sp);
    bus.write_word(sp & 0x00FF_FFFF, val);
}

/// Pops a 16-bit word from the stack (post-increment A7 by 2).
fn pop_word(cpu: &mut Cpu, bus: &mut dyn Bus) -> u16 {
    let sp = cpu.sp();
    cpu.set_sp(sp.wrapping_add(2));
    bus.read_word(sp & 0x00FF_FFFF)
}

// ── Interrupt delivery ──────────────────────────────────────────────────

/// Delivers an interrupt to the CPU at the given level (1-7).
///
/// The 68000 interrupt sequence:
/// 1. Push SR (with current flags and interrupt mask)
/// 2. Push PC (return address)
/// 3. Enter supervisor mode
/// 4. Set interrupt mask to the accepted level
/// 5. Clear trace flag
/// 6. Read the vector address from the exception vector table
/// 7. Jump to the interrupt handler
///
/// Auto-vectors are at addresses 0x64 + (level * 4).
/// Level 6 = V-blank (vector at 0x78).
/// Level 4 = H-blank (vector at 0x70).
///
/// Returns the number of cycles consumed (44 for interrupt processing).
pub fn deliver_interrupt(cpu: &mut Cpu, bus: &mut dyn Bus, level: u8) -> u32 {
    // Only deliver if the interrupt level exceeds the current mask
    // (level 7 is non-maskable)
    let mask = cpu.sr.interrupt_mask();
    if level < 7 && level <= mask {
        return 0;
    }

    // If CPU is stopped (STOP instruction), wake it up
    cpu.stopped = false;

    // Save current state
    let old_sr = cpu.sr.0;
    let old_pc = cpu.pc;

    // Enter supervisor mode, set interrupt mask, clear trace
    cpu.sr.set_flag(StatusRegister::S, true);
    cpu.sr.set_interrupt_mask(level);
    cpu.sr.set_flag(StatusRegister::T, false);

    // Push SR and PC onto supervisor stack
    push_long(cpu, bus, old_pc);
    push_word(cpu, bus, old_sr);

    // Read vector from auto-vector table
    let vector_addr = 0x60 + u32::from(level) * 4;
    let handler = read_long(bus, vector_addr);
    cpu.pc = handler & 0x00FF_FFFF;

    44 // interrupt processing takes ~44 cycles
}

// ── Main executor ────────────────────────────────────────────────────────

/// Executes one decoded instruction.
///
/// The opcode word has already been fetched and PC advanced past it.
/// This function reads any extension words from the instruction stream,
/// resolves effective addresses, performs the operation, sets condition
/// codes, and returns the number of CPU cycles consumed.
pub fn execute_instruction(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let instr = decode::decode(opcode);

    match instr {
        // ── Data Movement ────────────────────────────────────────
        Instruction::Move(size) => exec_move(cpu, opcode, size, bus),
        Instruction::MoveA(size) => exec_movea(cpu, opcode, size, bus),
        Instruction::MoveQ => exec_moveq(cpu, opcode),
        Instruction::Lea => exec_lea(cpu, opcode, bus),
        Instruction::Pea => exec_pea(cpu, opcode, bus),
        Instruction::Movem(size) => exec_movem(cpu, opcode, size, bus),
        Instruction::MoveP(size) => exec_movep(cpu, opcode, size, bus),

        // ── Arithmetic ───────────────────────────────────────────
        Instruction::Add(size) => exec_add(cpu, opcode, size, bus),
        Instruction::AddA(size) => exec_adda(cpu, opcode, size, bus),
        Instruction::AddI(size) => exec_addi(cpu, opcode, size, bus),
        Instruction::AddQ(size) => exec_addq(cpu, opcode, size, bus),
        Instruction::AddX(size) => exec_addx(cpu, opcode, size, bus),
        Instruction::Sub(size) => exec_sub(cpu, opcode, size, bus),
        Instruction::SubA(size) => exec_suba(cpu, opcode, size, bus),
        Instruction::SubI(size) => exec_subi(cpu, opcode, size, bus),
        Instruction::SubQ(size) => exec_subq(cpu, opcode, size, bus),
        Instruction::SubX(size) => exec_subx(cpu, opcode, size, bus),
        Instruction::MulU => exec_mulu(cpu, opcode, bus),
        Instruction::MulS => exec_muls(cpu, opcode, bus),
        Instruction::DivU => exec_divu(cpu, opcode, bus),
        Instruction::DivS => exec_divs(cpu, opcode, bus),
        Instruction::Clr(size) => exec_clr(cpu, opcode, size, bus),
        Instruction::Neg(size) => exec_neg(cpu, opcode, size, bus),
        Instruction::NegX(size) => exec_negx(cpu, opcode, size, bus),
        Instruction::Ext(size) => exec_ext(cpu, opcode, size),

        // ── Logic ────────────────────────────────────────────────
        Instruction::And(size) => exec_and(cpu, opcode, size, bus),
        Instruction::AndI(size) => exec_andi(cpu, opcode, size, bus),
        Instruction::Or(size) => exec_or(cpu, opcode, size, bus),
        Instruction::OrI(size) => exec_ori(cpu, opcode, size, bus),
        Instruction::Eor(size) => exec_eor(cpu, opcode, size, bus),
        Instruction::EorI(size) => exec_eori(cpu, opcode, size, bus),
        Instruction::Not(size) => exec_not(cpu, opcode, size, bus),

        // ── Shift/Rotate ─────────────────────────────────────────
        Instruction::Asd(size) => exec_asd(cpu, opcode, size),
        Instruction::Lsd(size) => exec_lsd(cpu, opcode, size),
        Instruction::Rod(size) => exec_rod(cpu, opcode, size),
        Instruction::Roxd(size) => exec_roxd(cpu, opcode, size),
        Instruction::ShiftMem => exec_shift_mem(cpu, opcode, bus),

        // ── Bit Manipulation ─────────────────────────────────────
        Instruction::BTst => exec_btst(cpu, opcode, bus),
        Instruction::BSet => exec_bset(cpu, opcode, bus),
        Instruction::BClr => exec_bclr(cpu, opcode, bus),
        Instruction::BChg => exec_bchg(cpu, opcode, bus),

        // ── Compare ──────────────────────────────────────────────
        Instruction::Cmp(size) => exec_cmp(cpu, opcode, size, bus),
        Instruction::CmpA(size) => exec_cmpa(cpu, opcode, size, bus),
        Instruction::CmpI(size) => exec_cmpi(cpu, opcode, size, bus),
        Instruction::CmpM(size) => exec_cmpm(cpu, opcode, size, bus),
        Instruction::Tst(size) => exec_tst(cpu, opcode, size, bus),

        // ── Branch ───────────────────────────────────────────────
        Instruction::Bcc => exec_bcc(cpu, opcode, bus),
        Instruction::Bsr => exec_bsr(cpu, opcode, bus),
        Instruction::DBcc => exec_dbcc(cpu, opcode, bus),
        Instruction::Scc => exec_scc(cpu, opcode, bus),

        // ── Jump / Subroutine ────────────────────────────────────
        Instruction::Jmp => exec_jmp(cpu, opcode, bus),
        Instruction::Jsr => exec_jsr(cpu, opcode, bus),
        Instruction::Rts => exec_rts(cpu, bus),
        Instruction::Rte => exec_rte(cpu, bus),
        Instruction::Rtr => exec_rtr(cpu, bus),

        // ── Stack ────────────────────────────────────────────────
        Instruction::Link => exec_link(cpu, opcode, bus),
        Instruction::Unlk => exec_unlk(cpu, opcode, bus),

        // ── System ───────────────────────────────────────────────
        Instruction::Nop => 4,
        Instruction::Stop => exec_stop(cpu, bus),
        Instruction::ResetInstr => 132, // RESET line asserted for 124 clocks + 8
        Instruction::Trap => exec_trap(cpu, opcode, bus),
        Instruction::TrapV => exec_trapv(cpu, bus),
        Instruction::Swap => exec_swap(cpu, opcode),
        Instruction::Exg => exec_exg(cpu, opcode),

        // ── SR / CCR operations ──────────────────────────────────
        Instruction::AndiCcr => exec_andi_ccr(cpu, bus),
        Instruction::AndiSr => exec_andi_sr(cpu, bus),
        Instruction::OriCcr => exec_ori_ccr(cpu, bus),
        Instruction::OriSr => exec_ori_sr(cpu, bus),
        Instruction::EoriCcr => exec_eori_ccr(cpu, bus),
        Instruction::EoriSr => exec_eori_sr(cpu, bus),
        Instruction::MoveToSr => exec_move_to_sr(cpu, opcode, bus),
        Instruction::MoveFromSr => exec_move_from_sr(cpu, opcode, bus),
        Instruction::MoveToCcr => exec_move_to_ccr(cpu, opcode, bus),
        Instruction::MoveUsp => exec_move_usp(cpu, opcode),

        // ── Traps / Illegal ──────────────────────────────────────
        Instruction::LineA | Instruction::LineF | Instruction::Illegal => {
            // Trigger illegal instruction exception (vector 4)
            exec_exception(cpu, bus, 4);
            34
        }
    }
}

// ── Exception processing ─────────────────────────────────────────────────

/// Processes a 68000 exception: pushes PC and SR, reads vector, jumps.
fn exec_exception(cpu: &mut Cpu, bus: &mut dyn Bus, vector: u8) {
    let sr_save = cpu.sr.0;
    // Enter supervisor mode
    if !cpu.sr.supervisor() {
        cpu.usp = cpu.sp();
        cpu.sr.set_flag(StatusRegister::S, true);
    }
    // Disable trace
    cpu.sr.set_flag(StatusRegister::T, false);

    push_long(cpu, bus, cpu.pc);
    push_word(cpu, bus, sr_save);

    // Read vector
    let vector_addr = u32::from(vector) * 4;
    cpu.pc = read_long(bus, vector_addr);
}

// ══════════════════════════════════════════════════════════════════════════
//  Instruction implementations
// ══════════════════════════════════════════════════════════════════════════

// ── MOVE ─────────────────────────────────────────────────────────────────

fn exec_move(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let src = src_ea(opcode);
    let dst = dst_ea_move(opcode);

    let val = read_ea(cpu, src, size, bus);
    set_flags_nz(&mut cpu.sr, val, size);
    write_ea(cpu, dst, size, bus, val);
    4
}

fn exec_movea(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let src = src_ea(opcode);
    let dst_reg = ((opcode >> 9) & 7) as u8;

    let val = read_ea(cpu, src, size, bus);
    // MOVEA sign-extends word to long, no flags affected
    let val = sign_extend(val, size);
    cpu.write_a(dst_reg, val);
    4
}

fn exec_moveq(cpu: &mut Cpu, opcode: u16) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let data = (opcode & 0xFF) as i8 as i32 as u32;
    cpu.d[reg] = data;
    set_flags_nz(&mut cpu.sr, data, InstructionSize::Long);
    4
}

fn exec_lea(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let dst_reg = ((opcode >> 9) & 7) as u8;
    let addr = resolve_ea(cpu, ea, InstructionSize::Long, bus);
    cpu.write_a(dst_reg, addr);
    4
}

fn exec_pea(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let addr = resolve_ea(cpu, ea, InstructionSize::Long, bus);
    push_long(cpu, bus, addr);
    12
}

fn exec_movem(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let mask = fetch_word(cpu, bus);
    let ea = src_ea(opcode);
    let direction = (opcode >> 10) & 1; // 0 = register to memory, 1 = memory to register
    let step: u32 = if size == InstructionSize::Long { 4 } else { 2 };
    let mut cycles: u32 = 8;

    if direction == 0 {
        // Register to memory
        match ea {
            AddressingMode::AddrPreDec(reg) => {
                // Predecrement mode: registers stored in reverse order (A7..A0, D7..D0)
                let mut addr = cpu.read_a(reg);
                for i in (0..16).rev() {
                    if mask & (1 << i) != 0 {
                        addr = addr.wrapping_sub(step);
                        let val = if i < 8 {
                            cpu.d[i as usize]
                        } else {
                            cpu.read_a((i - 8) as u8)
                        };
                        if size == InstructionSize::Long {
                            write_long(bus, addr & 0x00FF_FFFF, val);
                        } else {
                            bus.write_word(addr & 0x00FF_FFFF, val as u16);
                        }
                        cycles += step;
                    }
                }
                cpu.write_a(reg, addr);
            }
            _ => {
                // Normal modes: registers stored D0..D7, A0..A7
                let mut addr = resolve_ea(cpu, ea, size, bus);
                for i in 0..16u16 {
                    if mask & (1 << i) != 0 {
                        let val = if i < 8 {
                            cpu.d[i as usize]
                        } else {
                            cpu.read_a((i - 8) as u8)
                        };
                        if size == InstructionSize::Long {
                            write_long(bus, addr & 0x00FF_FFFF, val);
                        } else {
                            bus.write_word(addr & 0x00FF_FFFF, val as u16);
                        }
                        addr = addr.wrapping_add(step);
                        cycles += step;
                    }
                }
            }
        }
    } else {
        // Memory to register
        match ea {
            AddressingMode::AddrPostInc(reg) => {
                let mut addr = cpu.read_a(reg);
                for i in 0..16u16 {
                    if mask & (1 << i) != 0 {
                        let val = if size == InstructionSize::Long {
                            read_long(bus, addr & 0x00FF_FFFF)
                        } else {
                            let w = bus.read_word(addr & 0x00FF_FFFF);
                            // Sign-extend word to long
                            w as i16 as i32 as u32
                        };
                        if i < 8 {
                            cpu.d[i as usize] = val;
                        } else {
                            cpu.write_a((i - 8) as u8, val);
                        }
                        addr = addr.wrapping_add(step);
                        cycles += step;
                    }
                }
                cpu.write_a(reg, addr);
            }
            _ => {
                let mut addr = resolve_ea(cpu, ea, size, bus);
                for i in 0..16u16 {
                    if mask & (1 << i) != 0 {
                        let val = if size == InstructionSize::Long {
                            read_long(bus, addr & 0x00FF_FFFF)
                        } else {
                            let w = bus.read_word(addr & 0x00FF_FFFF);
                            w as i16 as i32 as u32
                        };
                        if i < 8 {
                            cpu.d[i as usize] = val;
                        } else {
                            cpu.write_a((i - 8) as u8, val);
                        }
                        addr = addr.wrapping_add(step);
                        cycles += step;
                    }
                }
            }
        }
    }

    cycles
}

fn exec_movep(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let data_reg = ((opcode >> 9) & 7) as usize;
    let addr_reg = (opcode & 7) as u8;
    let disp = fetch_word(cpu, bus) as i16 as i32;
    let base = (cpu.read_a(addr_reg) as i32).wrapping_add(disp) as u32 & 0x00FF_FFFF;
    let direction = (opcode >> 7) & 1; // 0 = memory-to-register, 1 = register-to-memory

    if direction == 0 {
        // Memory to register
        if size == InstructionSize::Long {
            let b3 = u32::from(bus.read_byte(base));
            let b2 = u32::from(bus.read_byte(base.wrapping_add(2)));
            let b1 = u32::from(bus.read_byte(base.wrapping_add(4)));
            let b0 = u32::from(bus.read_byte(base.wrapping_add(6)));
            cpu.d[data_reg] = (b3 << 24) | (b2 << 16) | (b1 << 8) | b0;
        } else {
            let b1 = u32::from(bus.read_byte(base));
            let b0 = u32::from(bus.read_byte(base.wrapping_add(2)));
            cpu.d[data_reg] = (cpu.d[data_reg] & 0xFFFF_0000) | (b1 << 8) | b0;
        }
    } else {
        // Register to memory
        let val = cpu.d[data_reg];
        if size == InstructionSize::Long {
            bus.write_byte(base, (val >> 24) as u8);
            bus.write_byte(base.wrapping_add(2), (val >> 16) as u8);
            bus.write_byte(base.wrapping_add(4), (val >> 8) as u8);
            bus.write_byte(base.wrapping_add(6), val as u8);
        } else {
            bus.write_byte(base, (val >> 8) as u8);
            bus.write_byte(base.wrapping_add(2), val as u8);
        }
    }

    if size == InstructionSize::Long {
        24
    } else {
        16
    }
}

// ── ADD / SUB ────────────────────────────────────────────────────────────

fn exec_add(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let opmode = (opcode >> 6) & 7;
    let ea = src_ea(opcode);

    if opmode < 3 {
        // <ea> + Dn -> Dn
        let src = read_ea(cpu, ea, size, bus);
        let dst = mask_value(cpu.d[reg], size);
        let result = dst.wrapping_add(src);
        set_flags_add(&mut cpu.sr, src, dst, result, size);
        match size {
            InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | (result & 0xFF),
            InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | (result & 0xFFFF),
            InstructionSize::Long => cpu.d[reg] = result,
        }
    } else {
        // Dn + <ea> -> <ea>
        let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
        let src = mask_value(cpu.d[reg], size);
        let result = dst.wrapping_add(src);
        set_flags_add(&mut cpu.sr, src, dst, result, size);
        match ea {
            AddressingMode::DataDirect(r) => match size {
                InstructionSize::Byte => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[r as usize] = result,
            },
            _ => write_to_addr(bus, addr, size, result),
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_adda(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as u8;
    let ea = src_ea(opcode);
    let src = sign_extend(read_ea(cpu, ea, size, bus), size);
    let dst = cpu.read_a(reg);
    cpu.write_a(reg, dst.wrapping_add(src));
    // ADDA does not affect flags
    if size == InstructionSize::Long { 8 } else { 8 }
}

fn exec_addi(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = dst.wrapping_add(imm);
    set_flags_add(&mut cpu.sr, imm, dst, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 16 } else { 8 }
}

fn exec_addq(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let mut imm = ((opcode >> 9) & 7) as u32;
    if imm == 0 {
        imm = 8;
    }
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::AddrDirect(reg) => {
            // ADDQ to address register: no flags, always long
            let val = cpu.read_a(reg);
            cpu.write_a(reg, val.wrapping_add(imm));
        }
        AddressingMode::DataDirect(reg) => {
            let dst = mask_value(cpu.d[reg as usize], size);
            let result = dst.wrapping_add(imm);
            set_flags_add(&mut cpu.sr, imm, dst, result, size);
            match size {
                InstructionSize::Byte => {
                    cpu.d[reg as usize] = (cpu.d[reg as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[reg as usize] = (cpu.d[reg as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[reg as usize] = result,
            }
        }
        _ => {
            let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
            let result = dst.wrapping_add(imm);
            set_flags_add(&mut cpu.sr, imm, dst, result, size);
            write_to_addr(bus, addr, size, result);
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_addx(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let rx = ((opcode >> 9) & 7) as u8;
    let ry = (opcode & 7) as u8;
    let rm = (opcode >> 3) & 1; // 0 = data reg, 1 = -(Ay), -(Ax)
    let x_bit: u32 = if cpu.sr.flag(StatusRegister::X) { 1 } else { 0 };

    if rm == 0 {
        // Data register
        let src = mask_value(cpu.d[ry as usize], size);
        let dst = mask_value(cpu.d[rx as usize], size);
        let result = dst.wrapping_add(src).wrapping_add(x_bit);
        set_flags_add(&mut cpu.sr, src.wrapping_add(x_bit), dst, result, size);
        // ADDX: Z flag is only cleared, never set (sticky zero)
        if mask_value(result, size) != 0 {
            cpu.sr.set_flag(StatusRegister::Z, false);
        }
        match size {
            InstructionSize::Byte => {
                cpu.d[rx as usize] = (cpu.d[rx as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[rx as usize] = (cpu.d[rx as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[rx as usize] = result,
        }
    } else {
        // Memory with predecrement
        let src_ea = AddressingMode::AddrPreDec(ry);
        let dst_ea = AddressingMode::AddrPreDec(rx);
        let src = read_ea(cpu, src_ea, size, bus);
        let (dst, addr) = read_ea_with_addr(cpu, dst_ea, size, bus);
        let result = dst.wrapping_add(src).wrapping_add(x_bit);
        set_flags_add(&mut cpu.sr, src.wrapping_add(x_bit), dst, result, size);
        if mask_value(result, size) != 0 {
            cpu.sr.set_flag(StatusRegister::Z, false);
        }
        write_to_addr(bus, addr, size, result);
    }

    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_sub(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let opmode = (opcode >> 6) & 7;
    let ea = src_ea(opcode);

    if opmode < 3 {
        // Dn - <ea> (actually: Dn := Dn - <ea>)
        let src = read_ea(cpu, ea, size, bus);
        let dst = mask_value(cpu.d[reg], size);
        let result = dst.wrapping_sub(src);
        set_flags_sub(&mut cpu.sr, src, dst, result, size);
        match size {
            InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | (result & 0xFF),
            InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | (result & 0xFFFF),
            InstructionSize::Long => cpu.d[reg] = result,
        }
    } else {
        // <ea> - Dn -> <ea> (actually: <ea> := <ea> - Dn)
        let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
        let src = mask_value(cpu.d[reg], size);
        let result = dst.wrapping_sub(src);
        set_flags_sub(&mut cpu.sr, src, dst, result, size);
        match ea {
            AddressingMode::DataDirect(r) => match size {
                InstructionSize::Byte => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[r as usize] = result,
            },
            _ => write_to_addr(bus, addr, size, result),
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_suba(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as u8;
    let ea = src_ea(opcode);
    let src = sign_extend(read_ea(cpu, ea, size, bus), size);
    let dst = cpu.read_a(reg);
    cpu.write_a(reg, dst.wrapping_sub(src));
    // SUBA does not affect flags
    8
}

fn exec_subi(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = dst.wrapping_sub(imm);
    set_flags_sub(&mut cpu.sr, imm, dst, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 16 } else { 8 }
}

fn exec_subq(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let mut imm = ((opcode >> 9) & 7) as u32;
    if imm == 0 {
        imm = 8;
    }
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::AddrDirect(reg) => {
            // SUBQ to address register: no flags, always long
            let val = cpu.read_a(reg);
            cpu.write_a(reg, val.wrapping_sub(imm));
        }
        AddressingMode::DataDirect(reg) => {
            let dst = mask_value(cpu.d[reg as usize], size);
            let result = dst.wrapping_sub(imm);
            set_flags_sub(&mut cpu.sr, imm, dst, result, size);
            match size {
                InstructionSize::Byte => {
                    cpu.d[reg as usize] = (cpu.d[reg as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[reg as usize] = (cpu.d[reg as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[reg as usize] = result,
            }
        }
        _ => {
            let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
            let result = dst.wrapping_sub(imm);
            set_flags_sub(&mut cpu.sr, imm, dst, result, size);
            write_to_addr(bus, addr, size, result);
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_subx(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let rx = ((opcode >> 9) & 7) as u8;
    let ry = (opcode & 7) as u8;
    let rm = (opcode >> 3) & 1;
    let x_bit: u32 = if cpu.sr.flag(StatusRegister::X) { 1 } else { 0 };

    if rm == 0 {
        let src = mask_value(cpu.d[ry as usize], size);
        let dst = mask_value(cpu.d[rx as usize], size);
        let result = dst.wrapping_sub(src).wrapping_sub(x_bit);
        set_flags_sub(&mut cpu.sr, src.wrapping_add(x_bit), dst, result, size);
        if mask_value(result, size) != 0 {
            cpu.sr.set_flag(StatusRegister::Z, false);
        }
        match size {
            InstructionSize::Byte => {
                cpu.d[rx as usize] = (cpu.d[rx as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[rx as usize] = (cpu.d[rx as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[rx as usize] = result,
        }
    } else {
        let src_ea = AddressingMode::AddrPreDec(ry);
        let dst_ea = AddressingMode::AddrPreDec(rx);
        let src = read_ea(cpu, src_ea, size, bus);
        let (dst, addr) = read_ea_with_addr(cpu, dst_ea, size, bus);
        let result = dst.wrapping_sub(src).wrapping_sub(x_bit);
        set_flags_sub(&mut cpu.sr, src.wrapping_add(x_bit), dst, result, size);
        if mask_value(result, size) != 0 {
            cpu.sr.set_flag(StatusRegister::Z, false);
        }
        write_to_addr(bus, addr, size, result);
    }

    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_mulu(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    let src = read_ea(cpu, ea, InstructionSize::Word, bus) & 0xFFFF;
    let dst = cpu.d[reg] & 0xFFFF;
    let result = src * dst;
    cpu.d[reg] = result;

    cpu.sr
        .set_flag(StatusRegister::N, result & 0x8000_0000 != 0);
    cpu.sr.set_flag(StatusRegister::Z, result == 0);
    cpu.sr.set_flag(StatusRegister::V, false);
    cpu.sr.set_flag(StatusRegister::C, false);
    70
}

fn exec_muls(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    let src = read_ea(cpu, ea, InstructionSize::Word, bus) as i16 as i32;
    let dst = cpu.d[reg] as i16 as i32;
    let result = (src * dst) as u32;
    cpu.d[reg] = result;

    cpu.sr
        .set_flag(StatusRegister::N, result & 0x8000_0000 != 0);
    cpu.sr.set_flag(StatusRegister::Z, result == 0);
    cpu.sr.set_flag(StatusRegister::V, false);
    cpu.sr.set_flag(StatusRegister::C, false);
    70
}

fn exec_divu(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    let divisor = read_ea(cpu, ea, InstructionSize::Word, bus) & 0xFFFF;
    let dividend = cpu.d[reg];

    if divisor == 0 {
        // Division by zero exception
        exec_exception(cpu, bus, 5);
        return 38;
    }

    let quotient = dividend / divisor;
    let remainder = dividend % divisor;

    if quotient > 0xFFFF {
        // Overflow
        cpu.sr.set_flag(StatusRegister::V, true);
        cpu.sr.set_flag(StatusRegister::C, false);
        // N and Z are undefined on overflow, but register is unchanged
    } else {
        cpu.d[reg] = (remainder << 16) | (quotient & 0xFFFF);
        cpu.sr.set_flag(StatusRegister::N, quotient & 0x8000 != 0);
        cpu.sr.set_flag(StatusRegister::Z, quotient == 0);
        cpu.sr.set_flag(StatusRegister::V, false);
        cpu.sr.set_flag(StatusRegister::C, false);
    }
    140
}

fn exec_divs(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    let divisor = read_ea(cpu, ea, InstructionSize::Word, bus) as i16 as i32;
    let dividend = cpu.d[reg] as i32;

    if divisor == 0 {
        exec_exception(cpu, bus, 5);
        return 38;
    }

    let quotient = dividend / divisor;
    let remainder = dividend % divisor;

    if !(-0x8000..=0x7FFF).contains(&quotient) {
        // Overflow
        cpu.sr.set_flag(StatusRegister::V, true);
        cpu.sr.set_flag(StatusRegister::C, false);
    } else {
        let q16 = (quotient as u32) & 0xFFFF;
        let r16 = (remainder as u32) & 0xFFFF;
        cpu.d[reg] = (r16 << 16) | q16;
        cpu.sr.set_flag(StatusRegister::N, q16 & 0x8000 != 0);
        cpu.sr.set_flag(StatusRegister::Z, q16 == 0);
        cpu.sr.set_flag(StatusRegister::V, false);
        cpu.sr.set_flag(StatusRegister::C, false);
    }
    158
}

fn exec_clr(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    write_ea(cpu, ea, size, bus, 0);
    cpu.sr.set_flag(StatusRegister::N, false);
    cpu.sr.set_flag(StatusRegister::Z, true);
    cpu.sr.set_flag(StatusRegister::V, false);
    cpu.sr.set_flag(StatusRegister::C, false);
    match ea {
        AddressingMode::DataDirect(_) => 4,
        _ => {
            if size == InstructionSize::Long {
                12
            } else {
                8
            }
        }
    }
}

fn exec_neg(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let (val, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = 0u32.wrapping_sub(val);
    set_flags_sub(&mut cpu.sr, val, 0, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    match ea {
        AddressingMode::DataDirect(_) => {
            if size == InstructionSize::Long {
                6
            } else {
                4
            }
        }
        _ => {
            if size == InstructionSize::Long {
                12
            } else {
                8
            }
        }
    }
}

fn exec_negx(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let (val, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let x_bit: u32 = if cpu.sr.flag(StatusRegister::X) { 1 } else { 0 };
    let result = 0u32.wrapping_sub(val).wrapping_sub(x_bit);
    set_flags_sub(&mut cpu.sr, val.wrapping_add(x_bit), 0, result, size);
    // NEGX: Z is only cleared, never set (sticky)
    if mask_value(result, size) != 0 {
        cpu.sr.set_flag(StatusRegister::Z, false);
    }
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    match ea {
        AddressingMode::DataDirect(_) => {
            if size == InstructionSize::Long {
                6
            } else {
                4
            }
        }
        _ => {
            if size == InstructionSize::Long {
                12
            } else {
                8
            }
        }
    }
}

fn exec_ext(cpu: &mut Cpu, opcode: u16, size: InstructionSize) -> u32 {
    let reg = (opcode & 7) as usize;
    match size {
        InstructionSize::Word => {
            // Byte -> Word
            let val = (cpu.d[reg] as u8) as i8 as i16 as u16;
            cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | u32::from(val);
            set_flags_nz(&mut cpu.sr, u32::from(val), InstructionSize::Word);
        }
        InstructionSize::Long => {
            // Word -> Long
            let val = (cpu.d[reg] as u16) as i16 as i32 as u32;
            cpu.d[reg] = val;
            set_flags_nz(&mut cpu.sr, val, InstructionSize::Long);
        }
        InstructionSize::Byte => {
            // EXT.B doesn't exist on 68000 -- treat as NOP
        }
    }
    4
}

// ── Logic ────────────────────────────────────────────────────────────────

fn exec_and(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let opmode = (opcode >> 6) & 7;
    let ea = src_ea(opcode);

    if opmode < 3 {
        // <ea> AND Dn -> Dn
        let src = read_ea(cpu, ea, size, bus);
        let dst = mask_value(cpu.d[reg], size);
        let result = src & dst;
        set_flags_nz(&mut cpu.sr, result, size);
        match size {
            InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | (result & 0xFF),
            InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | (result & 0xFFFF),
            InstructionSize::Long => cpu.d[reg] = result,
        }
    } else {
        // Dn AND <ea> -> <ea>
        let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
        let src = mask_value(cpu.d[reg], size);
        let result = src & dst;
        set_flags_nz(&mut cpu.sr, result, size);
        match ea {
            AddressingMode::DataDirect(r) => match size {
                InstructionSize::Byte => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[r as usize] = result,
            },
            _ => write_to_addr(bus, addr, size, result),
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_andi(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = dst & imm;
    set_flags_nz(&mut cpu.sr, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 16 } else { 8 }
}

fn exec_or(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let opmode = (opcode >> 6) & 7;
    let ea = src_ea(opcode);

    if opmode < 3 {
        let src = read_ea(cpu, ea, size, bus);
        let dst = mask_value(cpu.d[reg], size);
        let result = src | dst;
        set_flags_nz(&mut cpu.sr, result, size);
        match size {
            InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | (result & 0xFF),
            InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | (result & 0xFFFF),
            InstructionSize::Long => cpu.d[reg] = result,
        }
    } else {
        let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
        let src = mask_value(cpu.d[reg], size);
        let result = src | dst;
        set_flags_nz(&mut cpu.sr, result, size);
        match ea {
            AddressingMode::DataDirect(r) => match size {
                InstructionSize::Byte => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
                }
                InstructionSize::Word => {
                    cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
                }
                InstructionSize::Long => cpu.d[r as usize] = result,
            },
            _ => write_to_addr(bus, addr, size, result),
        }
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_ori(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = dst | imm;
    set_flags_nz(&mut cpu.sr, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 16 } else { 8 }
}

fn exec_eor(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    // EOR is always Dn XOR <ea> -> <ea>
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let src = mask_value(cpu.d[reg], size);
    let result = src ^ dst;
    set_flags_nz(&mut cpu.sr, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 8 } else { 4 }
}

fn exec_eori(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let (dst, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = dst ^ imm;
    set_flags_nz(&mut cpu.sr, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    if size == InstructionSize::Long { 16 } else { 8 }
}

fn exec_not(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let (val, addr) = read_ea_with_addr(cpu, ea, size, bus);
    let result = !val;
    set_flags_nz(&mut cpu.sr, result, size);
    match ea {
        AddressingMode::DataDirect(r) => match size {
            InstructionSize::Byte => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFF) | (result & 0xFF)
            }
            InstructionSize::Word => {
                cpu.d[r as usize] = (cpu.d[r as usize] & !0xFFFF) | (result & 0xFFFF)
            }
            InstructionSize::Long => cpu.d[r as usize] = result,
        },
        _ => write_to_addr(bus, addr, size, result),
    }
    match ea {
        AddressingMode::DataDirect(_) => {
            if size == InstructionSize::Long {
                6
            } else {
                4
            }
        }
        _ => {
            if size == InstructionSize::Long {
                12
            } else {
                8
            }
        }
    }
}

// ── Shift / Rotate ───────────────────────────────────────────────────────

fn exec_asd(cpu: &mut Cpu, opcode: u16, size: InstructionSize) -> u32 {
    let reg = (opcode & 7) as usize;
    let direction = (opcode >> 8) & 1; // 0 = right, 1 = left
    let ir = (opcode >> 5) & 1; // 0 = count in bits 11-9, 1 = count in register
    let count_field = ((opcode >> 9) & 7) as u32;

    let count = if ir == 0 {
        if count_field == 0 { 8 } else { count_field }
    } else {
        cpu.d[count_field as usize] % 64
    };

    let msb = msb_mask(size);
    let mut val = mask_value(cpu.d[reg], size);

    if count == 0 {
        set_flags_nz(&mut cpu.sr, val, size);
        cpu.sr.set_flag(StatusRegister::C, false);
        cpu.sr.set_flag(StatusRegister::V, false);
        return 6 + 2 * count;
    }

    let mut carry = false;
    let mut overflow = false;

    if direction == 1 {
        // ASL (left)
        for _ in 0..count {
            carry = val & msb != 0;
            let old_msb = val & msb;
            val = mask_value(val << 1, size);
            if val & msb != old_msb {
                overflow = true;
            }
        }
    } else {
        // ASR (right) - sign extension
        let sign = val & msb;
        for _ in 0..count {
            carry = val & 1 != 0;
            val = mask_value((val >> 1) | sign, size);
        }
        // ASR never overflows
    }

    cpu.sr.set_flag(StatusRegister::C, carry);
    cpu.sr.set_flag(StatusRegister::X, carry);
    cpu.sr.set_flag(StatusRegister::V, overflow);
    set_flags_nz_only(&mut cpu.sr, val, size);

    match size {
        InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | val,
        InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | val,
        InstructionSize::Long => cpu.d[reg] = val,
    }

    6 + 2 * count
}

fn exec_lsd(cpu: &mut Cpu, opcode: u16, size: InstructionSize) -> u32 {
    let reg = (opcode & 7) as usize;
    let direction = (opcode >> 8) & 1;
    let ir = (opcode >> 5) & 1;
    let count_field = ((opcode >> 9) & 7) as u32;

    let count = if ir == 0 {
        if count_field == 0 { 8 } else { count_field }
    } else {
        cpu.d[count_field as usize] % 64
    };

    let msb = msb_mask(size);
    let mut val = mask_value(cpu.d[reg], size);

    if count == 0 {
        set_flags_nz(&mut cpu.sr, val, size);
        cpu.sr.set_flag(StatusRegister::C, false);
        return 6 + 2 * count;
    }

    let mut carry = false;

    if direction == 1 {
        // LSL
        for _ in 0..count {
            carry = val & msb != 0;
            val = mask_value(val << 1, size);
        }
    } else {
        // LSR
        for _ in 0..count {
            carry = val & 1 != 0;
            val >>= 1;
        }
        val = mask_value(val, size);
    }

    cpu.sr.set_flag(StatusRegister::C, carry);
    cpu.sr.set_flag(StatusRegister::X, carry);
    cpu.sr.set_flag(StatusRegister::V, false);
    set_flags_nz_only(&mut cpu.sr, val, size);

    match size {
        InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | val,
        InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | val,
        InstructionSize::Long => cpu.d[reg] = val,
    }

    6 + 2 * count
}

fn exec_rod(cpu: &mut Cpu, opcode: u16, size: InstructionSize) -> u32 {
    let reg = (opcode & 7) as usize;
    let direction = (opcode >> 8) & 1;
    let ir = (opcode >> 5) & 1;
    let count_field = ((opcode >> 9) & 7) as u32;

    let count = if ir == 0 {
        if count_field == 0 { 8 } else { count_field }
    } else {
        cpu.d[count_field as usize] % 64
    };

    let bits = size_bits(size);
    let msb = msb_mask(size);
    let mut val = mask_value(cpu.d[reg], size);

    if count == 0 {
        set_flags_nz(&mut cpu.sr, val, size);
        cpu.sr.set_flag(StatusRegister::C, false);
        return 6 + 2 * count;
    }

    let mut carry = false;
    let effective_count = count % bits;

    if direction == 1 {
        // ROL
        for _ in 0..effective_count {
            carry = val & msb != 0;
            val = mask_value((val << 1) | (if carry { 1 } else { 0 }), size);
        }
    } else {
        // ROR
        for _ in 0..effective_count {
            carry = val & 1 != 0;
            val = mask_value((val >> 1) | (if carry { msb } else { 0 }), size);
        }
    }

    cpu.sr.set_flag(StatusRegister::C, carry);
    cpu.sr.set_flag(StatusRegister::V, false);
    set_flags_nz_only(&mut cpu.sr, val, size);

    match size {
        InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | val,
        InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | val,
        InstructionSize::Long => cpu.d[reg] = val,
    }

    6 + 2 * count
}

fn exec_roxd(cpu: &mut Cpu, opcode: u16, size: InstructionSize) -> u32 {
    let reg = (opcode & 7) as usize;
    let direction = (opcode >> 8) & 1;
    let ir = (opcode >> 5) & 1;
    let count_field = ((opcode >> 9) & 7) as u32;

    let count = if ir == 0 {
        if count_field == 0 { 8 } else { count_field }
    } else {
        cpu.d[count_field as usize] % 64
    };

    let msb = msb_mask(size);
    let mut val = mask_value(cpu.d[reg], size);
    let mut x = cpu.sr.flag(StatusRegister::X);

    if count == 0 {
        set_flags_nz(&mut cpu.sr, val, size);
        cpu.sr.set_flag(StatusRegister::C, x);
        return 6 + 2 * count;
    }

    for _ in 0..count {
        if direction == 1 {
            // ROXL
            let new_x = val & msb != 0;
            val = mask_value((val << 1) | (if x { 1 } else { 0 }), size);
            x = new_x;
        } else {
            // ROXR
            let new_x = val & 1 != 0;
            val = mask_value((val >> 1) | (if x { msb } else { 0 }), size);
            x = new_x;
        }
    }

    cpu.sr.set_flag(StatusRegister::C, x);
    cpu.sr.set_flag(StatusRegister::X, x);
    cpu.sr.set_flag(StatusRegister::V, false);
    set_flags_nz_only(&mut cpu.sr, val, size);

    match size {
        InstructionSize::Byte => cpu.d[reg] = (cpu.d[reg] & !0xFF) | val,
        InstructionSize::Word => cpu.d[reg] = (cpu.d[reg] & !0xFFFF) | val,
        InstructionSize::Long => cpu.d[reg] = val,
    }

    6 + 2 * count
}

fn exec_shift_mem(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    // Memory shift: always word size, count=1
    // Bits 10-9 = shift type, bit 8 = direction
    let shift_type = (opcode >> 9) & 3;
    let direction = (opcode >> 8) & 1;
    let ea = src_ea(opcode);
    let (val, addr) = read_ea_with_addr(cpu, ea, InstructionSize::Word, bus);

    let msb: u32 = 0x8000;
    let mut result = val & 0xFFFF;
    let carry;

    match shift_type {
        0 => {
            // ASd
            if direction == 1 {
                carry = result & msb != 0;
                let old_msb = result & msb;
                result = (result << 1) & 0xFFFF;
                cpu.sr
                    .set_flag(StatusRegister::V, (result & msb) != old_msb);
            } else {
                carry = result & 1 != 0;
                let sign = result & msb;
                result = ((result >> 1) | sign) & 0xFFFF;
                cpu.sr.set_flag(StatusRegister::V, false);
            }
        }
        1 => {
            // LSd
            if direction == 1 {
                carry = result & msb != 0;
                result = (result << 1) & 0xFFFF;
            } else {
                carry = result & 1 != 0;
                result >>= 1;
            }
            cpu.sr.set_flag(StatusRegister::V, false);
        }
        2 => {
            // ROXd
            let x = cpu.sr.flag(StatusRegister::X);
            if direction == 1 {
                carry = result & msb != 0;
                result = ((result << 1) | (if x { 1 } else { 0 })) & 0xFFFF;
            } else {
                carry = result & 1 != 0;
                result = ((result >> 1) | (if x { msb } else { 0 })) & 0xFFFF;
            }
            cpu.sr.set_flag(StatusRegister::X, carry);
            cpu.sr.set_flag(StatusRegister::V, false);
        }
        3 | _ => {
            // ROd
            if direction == 1 {
                carry = result & msb != 0;
                result = ((result << 1) | (if carry { 1 } else { 0 })) & 0xFFFF;
            } else {
                carry = result & 1 != 0;
                result = ((result >> 1) | (if carry { msb } else { 0 })) & 0xFFFF;
            }
            cpu.sr.set_flag(StatusRegister::V, false);
        }
    }

    cpu.sr.set_flag(StatusRegister::C, carry);
    if shift_type < 2 {
        cpu.sr.set_flag(StatusRegister::X, carry);
    }
    set_flags_nz_only(&mut cpu.sr, result, InstructionSize::Word);

    match ea {
        AddressingMode::DataDirect(_) => { /* shouldn't happen for memory shift */ }
        _ => write_to_addr(bus, addr, InstructionSize::Word, result),
    }

    8
}

/// Sets just N and Z without touching V and C (used by shift/rotate).
fn set_flags_nz_only(sr: &mut StatusRegister, result: u32, size: InstructionSize) {
    let masked = mask_value(result, size);
    sr.set_flag(StatusRegister::N, masked & msb_mask(size) != 0);
    sr.set_flag(StatusRegister::Z, masked == 0);
}

// ── Bit Manipulation ─────────────────────────────────────────────────────

/// Gets the bit number for bit instructions. Static (immediate) or dynamic (register).
fn get_bit_number(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    if opcode & 0x0100 != 0 {
        // Dynamic: bit number in register Dn (bits 11-9)
        let reg = ((opcode >> 9) & 7) as usize;
        cpu.d[reg]
    } else {
        // Static: bit number is immediate (next word)
        u32::from(fetch_word(cpu, bus))
    }
}

fn exec_btst(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let bit_num = get_bit_number(cpu, opcode, bus);
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::DataDirect(reg) => {
            // Long register: bit mod 32
            let bit = bit_num % 32;
            let val = cpu.d[reg as usize];
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            4
        }
        _ => {
            // Byte memory: bit mod 8
            let bit = bit_num % 8;
            let val = read_ea(cpu, ea, InstructionSize::Byte, bus);
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            8
        }
    }
}

fn exec_bset(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let bit_num = get_bit_number(cpu, opcode, bus);
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::DataDirect(reg) => {
            let bit = bit_num % 32;
            let val = cpu.d[reg as usize];
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            cpu.d[reg as usize] = val | (1 << bit);
            8
        }
        _ => {
            let (val, addr) = read_ea_with_addr(cpu, ea, InstructionSize::Byte, bus);
            let bit = bit_num % 8;
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            write_to_addr(bus, addr, InstructionSize::Byte, val | (1 << bit));
            12
        }
    }
}

fn exec_bclr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let bit_num = get_bit_number(cpu, opcode, bus);
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::DataDirect(reg) => {
            let bit = bit_num % 32;
            let val = cpu.d[reg as usize];
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            cpu.d[reg as usize] = val & !(1 << bit);
            10
        }
        _ => {
            let (val, addr) = read_ea_with_addr(cpu, ea, InstructionSize::Byte, bus);
            let bit = bit_num % 8;
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            write_to_addr(bus, addr, InstructionSize::Byte, val & !(1 << bit));
            12
        }
    }
}

fn exec_bchg(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let bit_num = get_bit_number(cpu, opcode, bus);
    let ea = src_ea(opcode);

    match ea {
        AddressingMode::DataDirect(reg) => {
            let bit = bit_num % 32;
            let val = cpu.d[reg as usize];
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            cpu.d[reg as usize] = val ^ (1 << bit);
            8
        }
        _ => {
            let (val, addr) = read_ea_with_addr(cpu, ea, InstructionSize::Byte, bus);
            let bit = bit_num % 8;
            cpu.sr.set_flag(StatusRegister::Z, val & (1 << bit) == 0);
            write_to_addr(bus, addr, InstructionSize::Byte, val ^ (1 << bit));
            12
        }
    }
}

// ── Compare ──────────────────────────────────────────────────────────────

fn exec_cmp(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as usize;
    let ea = src_ea(opcode);
    let src = read_ea(cpu, ea, size, bus);
    let dst = mask_value(cpu.d[reg], size);
    let result = dst.wrapping_sub(src);
    set_flags_cmp(&mut cpu.sr, src, dst, result, size);
    if size == InstructionSize::Long { 6 } else { 4 }
}

fn exec_cmpa(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let reg = ((opcode >> 9) & 7) as u8;
    let ea = src_ea(opcode);
    let src = sign_extend(read_ea(cpu, ea, size, bus), size);
    let dst = cpu.read_a(reg);
    let result = dst.wrapping_sub(src);
    set_flags_cmp(&mut cpu.sr, src, dst, result, InstructionSize::Long);
    6
}

fn exec_cmpi(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let imm = match size {
        InstructionSize::Byte => u32::from(fetch_word(cpu, bus)) & 0xFF,
        InstructionSize::Word => u32::from(fetch_word(cpu, bus)),
        InstructionSize::Long => fetch_long(cpu, bus),
    };
    let ea = src_ea(opcode);
    let dst = read_ea(cpu, ea, size, bus);
    let result = dst.wrapping_sub(imm);
    set_flags_cmp(&mut cpu.sr, imm, dst, result, size);
    if size == InstructionSize::Long { 12 } else { 8 }
}

fn exec_cmpm(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ay = (opcode & 7) as u8;
    let ax = ((opcode >> 9) & 7) as u8;
    let src = read_ea(cpu, AddressingMode::AddrPostInc(ay), size, bus);
    let dst = read_ea(cpu, AddressingMode::AddrPostInc(ax), size, bus);
    let result = dst.wrapping_sub(src);
    set_flags_cmp(&mut cpu.sr, src, dst, result, size);
    if size == InstructionSize::Long {
        20
    } else {
        12
    }
}

fn exec_tst(cpu: &mut Cpu, opcode: u16, size: InstructionSize, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let val = read_ea(cpu, ea, size, bus);
    set_flags_nz(&mut cpu.sr, val, size);
    4
}

// ── Branch ───────────────────────────────────────────────────────────────

fn exec_bcc(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let condition = ((opcode >> 8) & 0xF) as u8;
    let disp8 = (opcode & 0xFF) as i8;

    // PC currently points past the opcode word. For 8-bit displacement,
    // the base is PC after the opcode. For 16/32-bit, it's also after the opcode.
    let base_pc = cpu.pc; // PC after fetching the opcode word (already advanced by caller)

    let displacement = if disp8 == 0 {
        // 16-bit displacement follows
        let w = fetch_word(cpu, bus);
        w as i16 as i32
    } else if disp8 == -1 {
        // 32-bit displacement follows (68020+, but handle gracefully)
        let l = fetch_long(cpu, bus);
        l as i32
    } else {
        disp8 as i32
    };

    if evaluate_condition(&cpu.sr, condition) {
        // Branch taken: base is PC of the extension word (= PC after opcode)
        // Wait -- the 68000 computes Bcc target as (PC of opcode + 2) + displacement.
        // Our `base_pc` is already (PC of opcode + 2) since the caller advanced PC.
        // But if we fetched a 16-bit displacement, PC moved further. We need base_pc
        // which was captured before the displacement fetch.
        cpu.pc = (base_pc as i32).wrapping_add(displacement) as u32 & 0x00FF_FFFF;
        10
    } else {
        8
    }
}

fn exec_bsr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let disp8 = (opcode & 0xFF) as i8;
    let base_pc = cpu.pc;

    let displacement = if disp8 == 0 {
        let w = fetch_word(cpu, bus);
        w as i16 as i32
    } else {
        disp8 as i32
    };

    // Push return address (PC after the entire BSR instruction including displacement word)
    push_long(cpu, bus, cpu.pc);
    cpu.pc = (base_pc as i32).wrapping_add(displacement) as u32 & 0x00FF_FFFF;
    18
}

fn exec_dbcc(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let condition = ((opcode >> 8) & 0xF) as u8;
    let reg = (opcode & 7) as usize;
    let base_pc = cpu.pc;
    let displacement = fetch_word(cpu, bus) as i16 as i32;

    if evaluate_condition(&cpu.sr, condition) {
        // Condition true: no branch, no decrement
        return 12;
    }

    // Decrement low word of Dn
    let counter = (cpu.d[reg] as u16).wrapping_sub(1);
    cpu.d[reg] = (cpu.d[reg] & 0xFFFF_0000) | u32::from(counter);

    if counter != 0xFFFF {
        // Counter not exhausted: branch
        cpu.pc = (base_pc as i32).wrapping_add(displacement) as u32 & 0x00FF_FFFF;
        10
    } else {
        // Counter exhausted: fall through
        14
    }
}

fn exec_scc(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let condition = ((opcode >> 8) & 0xF) as u8;
    let ea = src_ea(opcode);
    let val: u32 = if evaluate_condition(&cpu.sr, condition) {
        0xFF
    } else {
        0x00
    };
    write_ea(cpu, ea, InstructionSize::Byte, bus, val);
    match ea {
        AddressingMode::DataDirect(_) => {
            if val == 0xFF {
                6
            } else {
                4
            }
        }
        _ => 8,
    }
}

// ── Jump / Subroutine ────────────────────────────────────────────────────

fn exec_jmp(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let addr = resolve_ea(cpu, ea, InstructionSize::Long, bus);
    cpu.pc = addr & 0x00FF_FFFF;
    match ea {
        AddressingMode::AddrIndirect(_) => 8,
        AddressingMode::AddrDisp(_) => 10,
        AddressingMode::AddrIndex(_) => 14,
        AddressingMode::AbsShort => 10,
        AddressingMode::AbsLong => 12,
        AddressingMode::PcDisp => 10,
        AddressingMode::PcIndex => 14,
        _ => 8,
    }
}

fn exec_jsr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let addr = resolve_ea(cpu, ea, InstructionSize::Long, bus);
    push_long(cpu, bus, cpu.pc);
    cpu.pc = addr & 0x00FF_FFFF;
    match ea {
        AddressingMode::AddrIndirect(_) => 16,
        AddressingMode::AddrDisp(_) => 18,
        AddressingMode::AddrIndex(_) => 22,
        AddressingMode::AbsShort => 18,
        AddressingMode::AbsLong => 20,
        AddressingMode::PcDisp => 18,
        AddressingMode::PcIndex => 22,
        _ => 16,
    }
}

fn exec_rts(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    cpu.pc = pop_long(cpu, bus) & 0x00FF_FFFF;
    16
}

fn exec_rte(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let new_sr = pop_word(cpu, bus);
    let new_pc = pop_long(cpu, bus);

    // If switching from supervisor to user mode, swap stack pointers
    let was_super = cpu.sr.supervisor();
    cpu.sr = StatusRegister::new(new_sr);
    if was_super && !cpu.sr.supervisor() {
        // Save SSP, restore USP
        cpu.ssp = cpu.sp();
        // The SP we just used was the SSP; the new mode is user, so set_sp will set USP
    }
    cpu.pc = new_pc & 0x00FF_FFFF;
    20
}

fn exec_rtr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let ccr = pop_word(cpu, bus);
    let new_pc = pop_long(cpu, bus);
    // RTR restores only the CCR (lower byte), not the system byte
    cpu.sr.0 = (cpu.sr.0 & 0xFF00) | (ccr & 0x001F);
    cpu.pc = new_pc & 0x00FF_FFFF;
    20
}

// ── Stack ────────────────────────────────────────────────────────────────

fn exec_link(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = (opcode & 7) as u8;
    let displacement = fetch_word(cpu, bus) as i16 as i32;
    push_long(cpu, bus, cpu.read_a(reg));
    cpu.write_a(reg, cpu.sp());
    let new_sp = (cpu.sp() as i32).wrapping_add(displacement) as u32;
    cpu.set_sp(new_sp);
    16
}

fn exec_unlk(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let reg = (opcode & 7) as u8;
    cpu.set_sp(cpu.read_a(reg));
    let val = pop_long(cpu, bus);
    cpu.write_a(reg, val);
    12
}

// ── System / Miscellaneous ───────────────────────────────────────────────

fn exec_stop(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr = StatusRegister::new(imm);
    cpu.stopped = true;
    4
}

fn exec_trap(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let vector = (opcode & 0xF) as u8 + 32; // TRAP vectors are 32-47
    exec_exception(cpu, bus, vector);
    34
}

fn exec_trapv(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    if cpu.sr.flag(StatusRegister::V) {
        exec_exception(cpu, bus, 7); // TRAPV vector
        34
    } else {
        4
    }
}

fn exec_swap(cpu: &mut Cpu, opcode: u16) -> u32 {
    let reg = (opcode & 7) as usize;
    let val = cpu.d[reg];
    cpu.d[reg] = val.rotate_left(16);
    set_flags_nz(&mut cpu.sr, cpu.d[reg], InstructionSize::Long);
    4
}

fn exec_exg(cpu: &mut Cpu, opcode: u16) -> u32 {
    let rx = ((opcode >> 9) & 7) as u8;
    let ry = (opcode & 7) as u8;
    let opmode = (opcode >> 3) & 0x1F;

    match opmode {
        0b01000 => {
            // EXG Dx, Dy
            cpu.d.swap(rx as usize, ry as usize);
        }
        0b01001 => {
            // EXG Ax, Ay
            let tmp = cpu.read_a(rx);
            cpu.write_a(rx, cpu.read_a(ry));
            cpu.write_a(ry, tmp);
        }
        0b10001 => {
            // EXG Dx, Ay
            let tmp = cpu.d[rx as usize];
            cpu.d[rx as usize] = cpu.read_a(ry);
            cpu.write_a(ry, tmp);
        }
        _ => {}
    }
    6
}

// ── SR / CCR operations ──────────────────────────────────────────────────

fn exec_andi_ccr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 = (cpu.sr.0 & 0xFF00) | (cpu.sr.0 & imm & 0x001F);
    20
}

fn exec_andi_sr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 &= imm;
    20
}

fn exec_ori_ccr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 |= imm & 0x001F;
    20
}

fn exec_ori_sr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 |= imm;
    20
}

fn exec_eori_ccr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 ^= imm & 0x001F;
    20
}

fn exec_eori_sr(cpu: &mut Cpu, bus: &mut dyn Bus) -> u32 {
    let imm = fetch_word(cpu, bus);
    cpu.sr.0 ^= imm;
    20
}

fn exec_move_to_sr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let val = read_ea(cpu, ea, InstructionSize::Word, bus) as u16;
    let was_super = cpu.sr.supervisor();
    cpu.sr = StatusRegister::new(val);
    if was_super && !cpu.sr.supervisor() {
        // Switched to user mode
        cpu.ssp = cpu.sp();
    } else if !was_super && cpu.sr.supervisor() {
        // Switched to supervisor mode
        cpu.usp = cpu.sp();
    }
    12
}

fn exec_move_from_sr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let val = u32::from(cpu.sr.0);
    write_ea(cpu, ea, InstructionSize::Word, bus, val);
    match ea {
        AddressingMode::DataDirect(_) => 6,
        _ => 8,
    }
}

fn exec_move_to_ccr(cpu: &mut Cpu, opcode: u16, bus: &mut dyn Bus) -> u32 {
    let ea = src_ea(opcode);
    let val = read_ea(cpu, ea, InstructionSize::Word, bus);
    cpu.sr.0 = (cpu.sr.0 & 0xFF00) | (val as u16 & 0x001F);
    12
}

fn exec_move_usp(cpu: &mut Cpu, opcode: u16) -> u32 {
    let reg = (opcode & 7) as u8;
    let direction = (opcode >> 3) & 1; // 0 = An -> USP, 1 = USP -> An

    if direction == 0 {
        cpu.usp = cpu.read_a(reg);
    } else {
        cpu.write_a(reg, cpu.usp);
    }
    4
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple RAM-backed bus for testing.
    struct TestBus {
        ram: [u8; 0x10000],
    }

    impl TestBus {
        fn new() -> Self {
            Self { ram: [0; 0x10000] }
        }

        /// Write a big-endian u16 at the given address.
        fn poke_word(&mut self, addr: u32, val: u16) {
            let a = (addr & 0xFFFF) as usize;
            self.ram[a] = (val >> 8) as u8;
            self.ram[a + 1] = val as u8;
        }

        /// Write a big-endian u32 at the given address.
        #[allow(dead_code)]
        fn poke_long(&mut self, addr: u32, val: u32) {
            self.poke_word(addr, (val >> 16) as u16);
            self.poke_word(addr + 2, val as u16);
        }
    }

    impl Bus for TestBus {
        fn read_byte(&mut self, addr: u32) -> u8 {
            self.ram[(addr & 0xFFFF) as usize]
        }

        fn read_word(&mut self, addr: u32) -> u16 {
            let a = (addr & 0xFFFF) as usize;
            u16::from(self.ram[a]) << 8 | u16::from(self.ram[a + 1])
        }

        fn write_byte(&mut self, addr: u32, val: u8) {
            self.ram[(addr & 0xFFFF) as usize] = val;
        }

        fn write_word(&mut self, addr: u32, val: u16) {
            let a = (addr & 0xFFFF) as usize;
            self.ram[a] = (val >> 8) as u8;
            self.ram[a + 1] = val as u8;
        }
    }

    fn make_cpu() -> Cpu {
        let mut cpu = Cpu::new();
        cpu.pc = 0x1000;
        cpu.ssp = 0xFFFE;
        cpu
    }

    // ── MOVEQ ────────────────────────────────────────────────────────

    #[test]
    fn moveq_positive() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        // MOVEQ #42, D3 = 0x762A
        execute_instruction(&mut cpu, 0x762A, &mut bus);
        assert_eq!(cpu.d[3], 42);
        assert!(!cpu.sr.flag(StatusRegister::N));
        assert!(!cpu.sr.flag(StatusRegister::Z));
    }

    #[test]
    fn moveq_negative() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        // MOVEQ #-1, D0 = 0x70FF
        execute_instruction(&mut cpu, 0x70FF, &mut bus);
        assert_eq!(cpu.d[0], 0xFFFF_FFFF);
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    #[test]
    fn moveq_zero() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        // MOVEQ #0, D0 = 0x7000
        execute_instruction(&mut cpu, 0x7000, &mut bus);
        assert_eq!(cpu.d[0], 0);
        assert!(cpu.sr.flag(StatusRegister::Z));
    }

    // ── MOVE ─────────────────────────────────────────────────────────

    #[test]
    fn move_word_reg_to_reg() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x1234;
        // MOVE.W D0, D1 = 0x3200
        execute_instruction(&mut cpu, 0x3200, &mut bus);
        assert_eq!(cpu.d[1] & 0xFFFF, 0x1234);
    }

    #[test]
    fn move_long_reg_to_reg() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0xDEAD_BEEF;
        // MOVE.L D0, D1 = 0x2200
        execute_instruction(&mut cpu, 0x2200, &mut bus);
        assert_eq!(cpu.d[1], 0xDEAD_BEEF);
    }

    // ── ADD ──────────────────────────────────────────────────────────

    #[test]
    fn add_word_ea_to_dn() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 10;
        cpu.d[1] = 20;
        // ADD.W D0, D1 = 0xD240
        execute_instruction(&mut cpu, 0xD240, &mut bus);
        assert_eq!(cpu.d[1] & 0xFFFF, 30);
    }

    #[test]
    fn add_overflow_sets_flags() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x7FFF;
        cpu.d[1] = 1;
        // ADD.W D1, D0 -- opcode: 1101 000 001 000 001 = 0xD041
        execute_instruction(&mut cpu, 0xD041, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0x8000);
        assert!(cpu.sr.flag(StatusRegister::V));
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    // ── SUB ──────────────────────────────────────────────────────────

    #[test]
    fn sub_word() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 5;
        cpu.d[1] = 10;
        // SUB.W D0, D1 = 0x9240 (Dn - <ea>: D1 := D1 - D0)
        execute_instruction(&mut cpu, 0x9240, &mut bus);
        assert_eq!(cpu.d[1] & 0xFFFF, 5);
    }

    // ── CMP ──────────────────────────────────────────────────────────

    #[test]
    fn cmp_equal_sets_zero() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 42;
        cpu.d[1] = 42;
        // CMP.L D0, D1 = 0xB280
        execute_instruction(&mut cpu, 0xB280, &mut bus);
        assert!(cpu.sr.flag(StatusRegister::Z));
        assert!(!cpu.sr.flag(StatusRegister::N));
    }

    #[test]
    fn cmp_less_sets_negative() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 100;
        cpu.d[1] = 50;
        // CMP.L D0, D1 = 0xB280
        execute_instruction(&mut cpu, 0xB280, &mut bus);
        assert!(!cpu.sr.flag(StatusRegister::Z));
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    // ── Branch ───────────────────────────────────────────────────────

    #[test]
    fn bra_short_displacement() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        // BRA.S $+6 = 0x6004 (displacement 4, but relative to PC+2 which is already done)
        // PC is 0x1000 before execution, after opcode fetch it's 0x1002
        // But execute_instruction is called after the opcode is fetched, so PC = 0x1000
        // actually the caller advances PC by 2 before calling us... let me set up properly
        cpu.pc = 0x1002; // After opcode fetch
        execute_instruction(&mut cpu, 0x6004, &mut bus);
        assert_eq!(cpu.pc, 0x1006); // 0x1002 + 4
    }

    #[test]
    fn beq_not_taken() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.sr.set_flag(StatusRegister::Z, false);
        cpu.pc = 0x1002;
        // BEQ.S $+4 = 0x6702
        execute_instruction(&mut cpu, 0x6702, &mut bus);
        // Not taken, PC should not change (stays at 0x1002)
        assert_eq!(cpu.pc, 0x1002);
    }

    #[test]
    fn beq_taken() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.sr.set_flag(StatusRegister::Z, true);
        cpu.pc = 0x1002;
        // BEQ.S $+4 = 0x6702
        execute_instruction(&mut cpu, 0x6702, &mut bus);
        assert_eq!(cpu.pc, 0x1004);
    }

    // ── JSR / RTS ────────────────────────────────────────────────────

    #[test]
    fn jsr_rts_roundtrip() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.pc = 0x1002;
        cpu.write_a(0, 0x2000);
        // JSR (A0) = 0x4E90
        execute_instruction(&mut cpu, 0x4E90, &mut bus);
        assert_eq!(cpu.pc, 0x2000);
        // Return address should be on stack
        let ret = read_long(&mut bus, cpu.sp() & 0x00FF_FFFF);
        assert_eq!(ret, 0x1002);

        // RTS = 0x4E75
        execute_instruction(&mut cpu, 0x4E75, &mut bus);
        assert_eq!(cpu.pc, 0x1002);
    }

    // ── SWAP ─────────────────────────────────────────────────────────

    #[test]
    fn swap_register() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[3] = 0xAABB_CCDD;
        // SWAP D3 = 0x4843
        execute_instruction(&mut cpu, 0x4843, &mut bus);
        assert_eq!(cpu.d[3], 0xCCDD_AABB);
    }

    // ── CLR ──────────────────────────────────────────────────────────

    #[test]
    fn clr_long_register() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0xDEAD_BEEF;
        // CLR.L D0 = 0x4280
        execute_instruction(&mut cpu, 0x4280, &mut bus);
        assert_eq!(cpu.d[0], 0);
        assert!(cpu.sr.flag(StatusRegister::Z));
    }

    // ── Logic ────────────────────────────────────────────────────────

    #[test]
    fn and_word() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0xFF0F;
        cpu.d[1] = 0x0FF0;
        // AND.W D0, D1 -- opcode: AND <ea>, Dn: 1100 001 001 000 000 = 0xC240
        execute_instruction(&mut cpu, 0xC240, &mut bus);
        assert_eq!(cpu.d[1] & 0xFFFF, 0x0F00);
    }

    #[test]
    fn or_long() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x00FF_0000;
        cpu.d[1] = 0x0000_FF00;
        // OR.L D0, D1 = 0x8280 -- opcode: OR <ea>, Dn byte... wait
        // OR.L D0, D1: 1000 001 010 000 000 = 0x8280
        execute_instruction(&mut cpu, 0x8280, &mut bus);
        assert_eq!(cpu.d[1], 0x00FF_FF00);
    }

    // ── EXT ──────────────────────────────────────────────────────────

    #[test]
    fn ext_byte_to_word() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x00FF_0080; // low byte = 0x80 (-128)
        // EXT.W D0 = 0x4880
        execute_instruction(&mut cpu, 0x4880, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0xFF80);
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    #[test]
    fn ext_word_to_long() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x0000_FFFE; // low word = 0xFFFE (-2)
        // EXT.L D0 = 0x48C0
        execute_instruction(&mut cpu, 0x48C0, &mut bus);
        assert_eq!(cpu.d[0], 0xFFFF_FFFE);
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    // ── LSR / LSL ────────────────────────────────────────────────────

    #[test]
    fn lsr_word() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x0080;
        // LSR.W #1, D0 = 0xE248
        execute_instruction(&mut cpu, 0xE248, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0x0040);
    }

    #[test]
    fn lsl_carry() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x8000;
        // LSL.W #1, D0 = 0xE348
        execute_instruction(&mut cpu, 0xE348, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0x0000);
        assert!(cpu.sr.flag(StatusRegister::C));
        assert!(cpu.sr.flag(StatusRegister::X));
        assert!(cpu.sr.flag(StatusRegister::Z));
    }

    // ── BTST ─────────────────────────────────────────────────────────

    #[test]
    fn btst_register_bit_set() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 4; // test bit 4
        cpu.d[1] = 0x10; // bit 4 is set
        // BTST D0, D1 = 0x0101
        execute_instruction(&mut cpu, 0x0101, &mut bus);
        assert!(!cpu.sr.flag(StatusRegister::Z));
    }

    #[test]
    fn btst_register_bit_clear() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 3; // test bit 3
        cpu.d[1] = 0x10; // bit 3 is clear
        // BTST D0, D1 = 0x0101
        execute_instruction(&mut cpu, 0x0101, &mut bus);
        assert!(cpu.sr.flag(StatusRegister::Z));
    }

    // ── MULU / DIVU ──────────────────────────────────────────────────

    #[test]
    fn mulu_basic() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 100;
        cpu.d[1] = 200;
        // MULU D0, D1 = 0xC2C0
        execute_instruction(&mut cpu, 0xC2C0, &mut bus);
        assert_eq!(cpu.d[1], 20000);
    }

    #[test]
    fn divu_basic() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 7;
        cpu.d[1] = 100;
        // DIVU D0, D1 = 0x82C0
        execute_instruction(&mut cpu, 0x82C0, &mut bus);
        let quotient = cpu.d[1] & 0xFFFF;
        let remainder = cpu.d[1] >> 16;
        assert_eq!(quotient, 14);
        assert_eq!(remainder, 2);
    }

    // ── LINK / UNLK ──────────────────────────────────────────────────

    #[test]
    fn link_unlk_roundtrip() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.ssp = 0x8000;
        cpu.write_a(6, 0x1234);
        cpu.pc = 0x100;
        // LINK A6, #-8: opcode = 0x4E56, extension = 0xFFF8
        bus.poke_word(0x100, 0xFFF8);
        execute_instruction(&mut cpu, 0x4E56, &mut bus);

        let saved_a6 = read_long(&mut bus, 0x7FFC); // old A6 on stack
        assert_eq!(saved_a6, 0x1234);
        let frame_ptr = cpu.read_a(6);
        assert_eq!(frame_ptr, 0x7FFC); // A6 = SP after push
        assert_eq!(cpu.sp(), 0x7FFC - 8); // SP -= 8

        // UNLK A6 = 0x4E5E
        execute_instruction(&mut cpu, 0x4E5E, &mut bus);
        assert_eq!(cpu.read_a(6), 0x1234); // A6 restored
    }

    // ── DBcc ─────────────────────────────────────────────────────────

    #[test]
    fn dbra_loop() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 2; // loop counter
        cpu.pc = 0x100;
        // DBRA D0, disp(-4): opcode 0x51C8, extension 0xFFFC
        bus.poke_word(0x100, 0xFFFC);

        // First iteration: counter 2 -> 1, branch taken
        execute_instruction(&mut cpu, 0x51C8, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 1);
        assert_eq!(cpu.pc, 0xFC); // 0x100 + (-4) = 0xFC

        // Second iteration: counter 1 -> 0, branch taken
        cpu.pc = 0x100;
        bus.poke_word(0x100, 0xFFFC);
        execute_instruction(&mut cpu, 0x51C8, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0);
        assert_eq!(cpu.pc, 0xFC);

        // Third iteration: counter 0 -> 0xFFFF, fall through
        cpu.pc = 0x100;
        bus.poke_word(0x100, 0xFFFC);
        execute_instruction(&mut cpu, 0x51C8, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 0xFFFF);
        assert_eq!(cpu.pc, 0x102); // fell through past displacement word
    }

    // ── ADDQ / SUBQ ─────────────────────────────────────────────────

    #[test]
    fn addq_to_address_register() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.write_a(0, 0x1000);
        // ADDQ.L #4, A0: 0101 100 0 10 001 000 = 0x5088 -- actually
        // ADDQ.L #1, A0 = 0x5288
        // bits 11-9 = 001 => imm=1, size=10 (long), ea=001 000 (AddrDirect A0)
        execute_instruction(&mut cpu, 0x5288, &mut bus);
        assert_eq!(cpu.read_a(0), 0x1001);
    }

    #[test]
    fn subq_from_data_register() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 10;
        // SUBQ.W #1, D0 = 0x5340
        execute_instruction(&mut cpu, 0x5340, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 9);
    }

    // ── NOP ──────────────────────────────────────────────────────────

    #[test]
    fn nop_returns_4_cycles() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        let cycles = execute_instruction(&mut cpu, 0x4E71, &mut bus);
        assert_eq!(cycles, 4);
    }

    // ── Condition codes ──────────────────────────────────────────────

    #[test]
    fn condition_codes_basic() {
        let sr = StatusRegister::new(0);
        assert!(evaluate_condition(&sr, 0x0)); // T
        assert!(!evaluate_condition(&sr, 0x1)); // F
        assert!(evaluate_condition(&sr, 0x4)); // CC (C=0)
        assert!(evaluate_condition(&sr, 0x6)); // NE (Z=0)
        assert!(evaluate_condition(&sr, 0xA)); // PL (N=0)
    }

    #[test]
    fn condition_codes_with_flags() {
        let mut sr = StatusRegister::new(0);
        sr.set_flag(StatusRegister::Z, true);
        assert!(evaluate_condition(&sr, 0x7)); // EQ
        assert!(!evaluate_condition(&sr, 0x6)); // NE

        sr = StatusRegister::new(0);
        sr.set_flag(StatusRegister::C, true);
        assert!(evaluate_condition(&sr, 0x5)); // CS
        assert!(!evaluate_condition(&sr, 0x4)); // CC
    }

    // ── LEA ──────────────────────────────────────────────────────────

    #[test]
    fn lea_addr_indirect() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.write_a(0, 0x2000);
        // LEA (A0), A1 = 0x43D0
        execute_instruction(&mut cpu, 0x43D0, &mut bus);
        assert_eq!(cpu.read_a(1), 0x2000);
    }

    // ── NEG ──────────────────────────────────────────────────────────

    #[test]
    fn neg_long() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 1;
        // NEG.L D0 = 0x4480
        execute_instruction(&mut cpu, 0x4480, &mut bus);
        assert_eq!(cpu.d[0], 0xFFFF_FFFF); // -1 in two's complement
        assert!(cpu.sr.flag(StatusRegister::N));
    }

    // ── NOT ──────────────────────────────────────────────────────────

    #[test]
    fn not_long() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0;
        // NOT.L D0 = 0x4680
        execute_instruction(&mut cpu, 0x4680, &mut bus);
        assert_eq!(cpu.d[0], 0xFFFF_FFFF);
        assert!(cpu.sr.flag(StatusRegister::N));
        assert!(!cpu.sr.flag(StatusRegister::Z));
    }

    // ── ADDI / SUBI / CMPI with immediate ────────────────────────────

    #[test]
    fn addi_word() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 10;
        cpu.pc = 0x100;
        // ADDI.W #5, D0 = 0x0640 followed by 0x0005
        bus.poke_word(0x100, 0x0005);
        execute_instruction(&mut cpu, 0x0640, &mut bus);
        assert_eq!(cpu.d[0] & 0xFFFF, 15);
    }

    #[test]
    fn cmpi_word_equal() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 42;
        cpu.pc = 0x100;
        // CMPI.W #42, D0 = 0x0C40, followed by 0x002A
        bus.poke_word(0x100, 0x002A);
        execute_instruction(&mut cpu, 0x0C40, &mut bus);
        assert!(cpu.sr.flag(StatusRegister::Z));
    }

    // ── EXG ──────────────────────────────────────────────────────────

    #[test]
    fn exg_data_registers() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0xAAAA;
        cpu.d[1] = 0xBBBB;
        // EXG D0, D1: 1100 000 1 01000 001 = 0xC141
        execute_instruction(&mut cpu, 0xC141, &mut bus);
        assert_eq!(cpu.d[0], 0xBBBB);
        assert_eq!(cpu.d[1], 0xAAAA);
    }

    // ── Scc ──────────────────────────────────────────────────────────

    #[test]
    fn st_sets_byte() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        // ST D0 = 0x50C0 (condition = True)
        execute_instruction(&mut cpu, 0x50C0, &mut bus);
        assert_eq!(cpu.d[0] & 0xFF, 0xFF);
    }

    #[test]
    fn sf_clears_byte() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0xFF;
        // SF D0 = 0x51C0 (condition = False)
        execute_instruction(&mut cpu, 0x51C0, &mut bus);
        assert_eq!(cpu.d[0] & 0xFF, 0x00);
    }

    // ── TST ──────────────────────────────────────────────────────────

    #[test]
    fn tst_zero() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0;
        // TST.L D0 = 0x4A80
        execute_instruction(&mut cpu, 0x4A80, &mut bus);
        assert!(cpu.sr.flag(StatusRegister::Z));
        assert!(!cpu.sr.flag(StatusRegister::N));
    }

    #[test]
    fn tst_negative() {
        let mut cpu = make_cpu();
        let mut bus = TestBus::new();
        cpu.d[0] = 0x8000_0000;
        // TST.L D0 = 0x4A80
        execute_instruction(&mut cpu, 0x4A80, &mut bus);
        assert!(!cpu.sr.flag(StatusRegister::Z));
        assert!(cpu.sr.flag(StatusRegister::N));
    }
}

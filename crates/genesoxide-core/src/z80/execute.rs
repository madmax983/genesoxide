//! Z80 instruction executor.
//!
//! Fetches, decodes, and executes one instruction, returning the number
//! of T-states consumed. The executor communicates with memory and I/O
//! through the [`Bus`] trait.

use super::{FLAG_C, FLAG_H, FLAG_N, FLAG_PV, FLAG_S, FLAG_X, FLAG_Y, FLAG_Z, Z80};

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

// ── Helpers ──────────────────────────────────────────────────────────────

/// Returns true if `val` has even parity.
#[inline]
fn parity(val: u8) -> bool {
    val.count_ones().is_multiple_of(2)
}

/// Fetches one byte at PC and advances PC.
#[inline]
fn fetch_byte(cpu: &mut Z80, bus: &mut dyn Bus) -> u8 {
    let val = bus.read_byte(cpu.pc);
    cpu.pc = cpu.pc.wrapping_add(1);
    val
}

/// Fetches a 16-bit little-endian word at PC and advances PC by 2.
#[inline]
fn fetch_word(cpu: &mut Z80, bus: &mut dyn Bus) -> u16 {
    let lo = bus.read_byte(cpu.pc) as u16;
    cpu.pc = cpu.pc.wrapping_add(1);
    let hi = bus.read_byte(cpu.pc) as u16;
    cpu.pc = cpu.pc.wrapping_add(1);
    (hi << 8) | lo
}

/// Reads an 8-bit register by 3-bit index (B=0, C=1, D=2, E=3, H=4, L=5, (HL)=6, A=7).
#[inline]
fn read_reg8(cpu: &Z80, bus: &mut dyn Bus, reg: u8) -> u8 {
    match reg {
        0 => cpu.b,
        1 => cpu.c,
        2 => cpu.d,
        3 => cpu.e,
        4 => cpu.h,
        5 => cpu.l,
        6 => bus.read_byte(cpu.hl()),
        7 => cpu.a,
        _ => unreachable!(),
    }
}

/// Writes an 8-bit register by 3-bit index.
#[inline]
fn write_reg8(cpu: &mut Z80, bus: &mut dyn Bus, reg: u8, val: u8) {
    match reg {
        0 => cpu.b = val,
        1 => cpu.c = val,
        2 => cpu.d = val,
        3 => cpu.e = val,
        4 => cpu.h = val,
        5 => cpu.l = val,
        6 => bus.write_byte(cpu.hl(), val),
        7 => cpu.a = val,
        _ => unreachable!(),
    }
}

/// Reads a 16-bit register pair by 2-bit index (BC=0, DE=1, HL=2, SP=3).
#[inline]
fn read_reg16(cpu: &Z80, pair: u8) -> u16 {
    match pair {
        0 => cpu.bc(),
        1 => cpu.de(),
        2 => cpu.hl(),
        3 => cpu.sp,
        _ => unreachable!(),
    }
}

/// Writes a 16-bit register pair by 2-bit index.
#[inline]
fn write_reg16(cpu: &mut Z80, pair: u8, val: u16) {
    match pair {
        0 => cpu.set_bc(val),
        1 => cpu.set_de(val),
        2 => cpu.set_hl(val),
        3 => cpu.sp = val,
        _ => unreachable!(),
    }
}

/// Tests a condition code (NZ=0, Z=1, NC=2, C=3, PO=4, PE=5, P=6, M=7).
#[inline]
fn condition_met(cpu: &Z80, cc: u8) -> bool {
    match cc {
        0 => !cpu.flag(FLAG_Z),  // NZ
        1 => cpu.flag(FLAG_Z),   // Z
        2 => !cpu.flag(FLAG_C),  // NC
        3 => cpu.flag(FLAG_C),   // C
        4 => !cpu.flag(FLAG_PV), // PO
        5 => cpu.flag(FLAG_PV),  // PE
        6 => !cpu.flag(FLAG_S),  // P (positive)
        7 => cpu.flag(FLAG_S),   // M (minus)
        _ => unreachable!(),
    }
}

/// Pushes a 16-bit value onto the stack (SP decrements first, high byte at higher address).
#[inline]
fn push(cpu: &mut Z80, bus: &mut dyn Bus, val: u16) {
    cpu.sp = cpu.sp.wrapping_sub(1);
    bus.write_byte(cpu.sp, (val >> 8) as u8);
    cpu.sp = cpu.sp.wrapping_sub(1);
    bus.write_byte(cpu.sp, val as u8);
}

/// Pops a 16-bit value from the stack (low byte at SP, high byte at SP+1).
#[inline]
fn pop(cpu: &mut Z80, bus: &mut dyn Bus) -> u16 {
    let lo = bus.read_byte(cpu.sp) as u16;
    cpu.sp = cpu.sp.wrapping_add(1);
    let hi = bus.read_byte(cpu.sp) as u16;
    cpu.sp = cpu.sp.wrapping_add(1);
    (hi << 8) | lo
}

// ── ALU helpers ─────────────────────────────────────────────────────────

/// Sets S, Z, X, Y flags from an 8-bit result. Does NOT touch other flags.
#[inline]
fn set_sz_xy(cpu: &mut Z80, result: u8) {
    cpu.set_flag(FLAG_S, result & 0x80 != 0);
    cpu.set_flag(FLAG_Z, result == 0);
    cpu.set_flag(FLAG_X, result & FLAG_X != 0);
    cpu.set_flag(FLAG_Y, result & FLAG_Y != 0);
}

/// ADD A,val — sets all flags.
#[inline]
fn alu_add(cpu: &mut Z80, val: u8) {
    let a = cpu.a;
    let result16 = a as u16 + val as u16;
    let result = result16 as u8;
    cpu.f = 0;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, (a & 0x0F) + (val & 0x0F) > 0x0F);
    // Overflow: both operands same sign, result different sign
    cpu.set_flag(
        FLAG_PV,
        ((a ^ val) & 0x80 == 0) && ((a ^ result) & 0x80 != 0),
    );
    cpu.set_flag(FLAG_C, result16 > 0xFF);
    // N = 0 (already cleared)
    cpu.a = result;
}

/// ADC A,val — add with carry.
#[inline]
fn alu_adc(cpu: &mut Z80, val: u8) {
    let a = cpu.a;
    let carry = if cpu.flag(FLAG_C) { 1u16 } else { 0 };
    let result16 = a as u16 + val as u16 + carry;
    let result = result16 as u8;
    cpu.f = 0;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, (a & 0x0F) + (val & 0x0F) + carry as u8 > 0x0F);
    cpu.set_flag(
        FLAG_PV,
        ((a ^ val) & 0x80 == 0) && ((a ^ result) & 0x80 != 0),
    );
    cpu.set_flag(FLAG_C, result16 > 0xFF);
    cpu.a = result;
}

/// SUB val — subtract from A.
#[inline]
fn alu_sub(cpu: &mut Z80, val: u8) {
    let a = cpu.a;
    let result = a.wrapping_sub(val);
    cpu.f = FLAG_N;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, (a & 0x0F) < (val & 0x0F));
    // Overflow: operands different sign, result sign differs from A
    cpu.set_flag(
        FLAG_PV,
        ((a ^ val) & 0x80 != 0) && ((a ^ result) & 0x80 != 0),
    );
    cpu.set_flag(FLAG_C, (a as u16) < (val as u16));
    cpu.a = result;
}

/// SBC A,val — subtract with carry.
#[inline]
fn alu_sbc(cpu: &mut Z80, val: u8) {
    let a = cpu.a;
    let carry = if cpu.flag(FLAG_C) { 1u16 } else { 0 };
    let result16 = (a as u16).wrapping_sub(val as u16).wrapping_sub(carry);
    let result = result16 as u8;
    cpu.f = FLAG_N;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, (a & 0x0F) < (val & 0x0F) + carry as u8);
    cpu.set_flag(
        FLAG_PV,
        ((a ^ val) & 0x80 != 0) && ((a ^ result) & 0x80 != 0),
    );
    cpu.set_flag(FLAG_C, (a as u16) < (val as u16) + carry);
    cpu.a = result;
}

/// AND val.
#[inline]
fn alu_and(cpu: &mut Z80, val: u8) {
    cpu.a &= val;
    let result = cpu.a;
    cpu.f = FLAG_H; // H = 1, N = 0, C = 0
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_PV, parity(result));
}

/// XOR val.
#[inline]
fn alu_xor(cpu: &mut Z80, val: u8) {
    cpu.a ^= val;
    let result = cpu.a;
    cpu.f = 0; // H = 0, N = 0, C = 0
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_PV, parity(result));
}

/// OR val.
#[inline]
fn alu_or(cpu: &mut Z80, val: u8) {
    cpu.a |= val;
    let result = cpu.a;
    cpu.f = 0; // H = 0, N = 0, C = 0
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_PV, parity(result));
}

/// CP val — compare (SUB without storing result; X/Y from operand).
#[inline]
fn alu_cp(cpu: &mut Z80, val: u8) {
    let a = cpu.a;
    let result = a.wrapping_sub(val);
    cpu.f = FLAG_N;
    cpu.set_flag(FLAG_S, result & 0x80 != 0);
    cpu.set_flag(FLAG_Z, result == 0);
    // X, Y come from the OPERAND for CP, not the result
    cpu.set_flag(FLAG_X, val & FLAG_X != 0);
    cpu.set_flag(FLAG_Y, val & FLAG_Y != 0);
    cpu.set_flag(FLAG_H, (a & 0x0F) < (val & 0x0F));
    cpu.set_flag(
        FLAG_PV,
        ((a ^ val) & 0x80 != 0) && ((a ^ result) & 0x80 != 0),
    );
    cpu.set_flag(FLAG_C, (a as u16) < (val as u16));
}

/// INC val — returns new value, sets flags (C unchanged).
#[inline]
fn alu_inc(cpu: &mut Z80, val: u8) -> u8 {
    let result = val.wrapping_add(1);
    // Preserve carry flag
    let old_c = cpu.flag(FLAG_C);
    cpu.f = 0;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, (val & 0x0F) + 1 > 0x0F);
    cpu.set_flag(FLAG_PV, val == 0x7F);
    // N = 0 (already cleared)
    cpu.set_flag(FLAG_C, old_c);
    result
}

/// DEC val — returns new value, sets flags (C unchanged).
#[inline]
fn alu_dec(cpu: &mut Z80, val: u8) -> u8 {
    let result = val.wrapping_sub(1);
    let old_c = cpu.flag(FLAG_C);
    cpu.f = FLAG_N;
    set_sz_xy(cpu, result);
    cpu.set_flag(FLAG_H, val & 0x0F == 0);
    cpu.set_flag(FLAG_PV, val == 0x80);
    cpu.set_flag(FLAG_C, old_c);
    result
}

// ── Executor ────────────────────────────────────────────────────────────

/// Fetches and executes one Z80 instruction, returning T-states consumed.
///
/// The refresh register (R) is incremented on each opcode fetch: the
/// lower 7 bits count while bit 7 is preserved (as on real hardware).
pub fn execute_instruction(cpu: &mut Z80, bus: &mut dyn Bus) -> u8 {
    // Handle delayed EI: clear the pending flag (interrupts were already
    // enabled by the EI instruction itself in the jsmoo model).
    if cpu.ei_pending {
        cpu.ei_pending = false;
    }

    let opcode = bus.read_byte(cpu.pc);
    cpu.pc = cpu.pc.wrapping_add(1);

    // Increment R: lower 7 bits wrap, bit 7 is preserved.
    cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);

    match opcode {
        // ── NOP ────────────────────────────────────────────────────
        0x00 => 4,

        // ── LD rr,nn (16-bit immediate loads) ─────────────────────
        // 0x01: LD BC,nn  0x11: LD DE,nn  0x21: LD HL,nn  0x31: LD SP,nn
        0x01 | 0x11 | 0x21 | 0x31 => {
            let val = fetch_word(cpu, bus);
            let pair = (opcode >> 4) & 0x03;
            write_reg16(cpu, pair, val);
            10
        }

        // ── LD (BC),A ─────────────────────────────────────────────
        0x02 => {
            bus.write_byte(cpu.bc(), cpu.a);
            7
        }

        // ── INC rr (16-bit increment, no flags) ──────────────────
        // 0x03: INC BC  0x13: INC DE  0x23: INC HL  0x33: INC SP
        0x03 | 0x13 | 0x23 | 0x33 => {
            let pair = (opcode >> 4) & 0x03;
            let val = read_reg16(cpu, pair).wrapping_add(1);
            write_reg16(cpu, pair, val);
            6
        }

        // ── INC r (8-bit) ─────────────────────────────────────────
        // 0x04: INC B  0x0C: INC C  0x14: INC D  0x1C: INC E
        // 0x24: INC H  0x2C: INC L  0x34: INC (HL)  0x3C: INC A
        0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
            let reg = (opcode >> 3) & 0x07;
            let val = read_reg8(cpu, bus, reg);
            let result = alu_inc(cpu, val);
            write_reg8(cpu, bus, reg, result);
            if reg == 6 { 11 } else { 4 }
        }

        // ── DEC r (8-bit) ─────────────────────────────────────────
        // 0x05: DEC B  0x0D: DEC C  0x15: DEC D  0x1D: DEC E
        // 0x25: DEC H  0x2D: DEC L  0x35: DEC (HL)  0x3D: DEC A
        0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
            let reg = (opcode >> 3) & 0x07;
            let val = read_reg8(cpu, bus, reg);
            let result = alu_dec(cpu, val);
            write_reg8(cpu, bus, reg, result);
            if reg == 6 { 11 } else { 4 }
        }

        // ── LD r,n (8-bit immediate loads) ────────────────────────
        // 0x06: LD B,n  0x0E: LD C,n  0x16: LD D,n  0x1E: LD E,n
        // 0x26: LD H,n  0x2E: LD L,n  0x36: LD (HL),n  0x3E: LD A,n
        0x06 | 0x0E | 0x16 | 0x1E | 0x26 | 0x2E | 0x36 | 0x3E => {
            let val = fetch_byte(cpu, bus);
            let reg = (opcode >> 3) & 0x07;
            write_reg8(cpu, bus, reg, val);
            if reg == 6 { 10 } else { 7 }
        }

        // ── RLCA ──────────────────────────────────────────────────
        0x07 => {
            let bit7 = (cpu.a >> 7) & 1;
            cpu.a = (cpu.a << 1) | bit7;
            // Preserve S, Z, PV; clear H, N; set C from old bit 7; X, Y from A
            cpu.set_flag(FLAG_C, bit7 != 0);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── EX AF,AF' ─────────────────────────────────────────────
        0x08 => {
            std::mem::swap(&mut cpu.a, &mut cpu.a_prime);
            std::mem::swap(&mut cpu.f, &mut cpu.f_prime);
            4
        }

        // ── ADD HL,rr ─────────────────────────────────────────────
        // 0x09: ADD HL,BC  0x19: ADD HL,DE  0x29: ADD HL,HL  0x39: ADD HL,SP
        0x09 | 0x19 | 0x29 | 0x39 => {
            let pair = (opcode >> 4) & 0x03;
            let hl = cpu.hl() as u32;
            let rr = read_reg16(cpu, pair) as u32;
            let result = hl + rr;
            cpu.set_hl(result as u16);
            // Preserve S, Z, PV
            cpu.set_flag(FLAG_H, ((hl ^ rr ^ result) >> 8) & 0x10 != 0);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_C, result > 0xFFFF);
            // X, Y from high byte of result
            let high = (result >> 8) as u8;
            cpu.set_flag(FLAG_X, high & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, high & FLAG_Y != 0);
            11
        }

        // ── LD A,(BC) ─────────────────────────────────────────────
        0x0A => {
            cpu.a = bus.read_byte(cpu.bc());
            7
        }

        // ── DEC rr (16-bit decrement, no flags) ──────────────────
        // 0x0B: DEC BC  0x1B: DEC DE  0x2B: DEC HL  0x3B: DEC SP
        0x0B | 0x1B | 0x2B | 0x3B => {
            let pair = (opcode >> 4) & 0x03;
            let val = read_reg16(cpu, pair).wrapping_sub(1);
            write_reg16(cpu, pair, val);
            6
        }

        // ── RRCA ──────────────────────────────────────────────────
        0x0F => {
            let bit0 = cpu.a & 1;
            cpu.a = (cpu.a >> 1) | (bit0 << 7);
            cpu.set_flag(FLAG_C, bit0 != 0);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── DJNZ e ────────────────────────────────────────────────
        0x10 => {
            let offset = fetch_byte(cpu, bus) as i8;
            cpu.b = cpu.b.wrapping_sub(1);
            if cpu.b != 0 {
                cpu.pc = cpu.pc.wrapping_add(offset as u16);
                13
            } else {
                8
            }
        }

        // ── LD (DE),A ─────────────────────────────────────────────
        0x12 => {
            bus.write_byte(cpu.de(), cpu.a);
            7
        }

        // ── RLA ───────────────────────────────────────────────────
        0x17 => {
            let old_carry = if cpu.flag(FLAG_C) { 1u8 } else { 0 };
            let bit7 = (cpu.a >> 7) & 1;
            cpu.a = (cpu.a << 1) | old_carry;
            cpu.set_flag(FLAG_C, bit7 != 0);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── JR e ──────────────────────────────────────────────────
        0x18 => {
            let offset = fetch_byte(cpu, bus) as i8;
            cpu.pc = cpu.pc.wrapping_add(offset as u16);
            12
        }

        // ── LD A,(DE) ─────────────────────────────────────────────
        0x1A => {
            cpu.a = bus.read_byte(cpu.de());
            7
        }

        // ── RRA ───────────────────────────────────────────────────
        0x1F => {
            let old_carry = if cpu.flag(FLAG_C) { 0x80u8 } else { 0 };
            let bit0 = cpu.a & 1;
            cpu.a = (cpu.a >> 1) | old_carry;
            cpu.set_flag(FLAG_C, bit0 != 0);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── JR cc,e (conditional relative jumps) ──────────────────
        // 0x20: JR NZ,e  0x28: JR Z,e  0x30: JR NC,e  0x38: JR C,e
        0x20 | 0x28 | 0x30 | 0x38 => {
            let offset = fetch_byte(cpu, bus) as i8;
            let cc = (opcode >> 3) & 0x03;
            let taken = match cc {
                0 => !cpu.flag(FLAG_Z), // NZ
                1 => cpu.flag(FLAG_Z),  // Z
                2 => !cpu.flag(FLAG_C), // NC
                3 => cpu.flag(FLAG_C),  // C
                _ => unreachable!(),
            };
            if taken {
                cpu.pc = cpu.pc.wrapping_add(offset as u16);
                12
            } else {
                7
            }
        }

        // ── LD (nn),HL ────────────────────────────────────────────
        0x22 => {
            let addr = fetch_word(cpu, bus);
            let hl = cpu.hl();
            bus.write_byte(addr, hl as u8);
            bus.write_byte(addr.wrapping_add(1), (hl >> 8) as u8);
            16
        }

        // ── DAA ───────────────────────────────────────────────────
        0x27 => {
            let mut correction = 0u8;
            let mut carry = cpu.flag(FLAG_C);
            let n_flag = cpu.flag(FLAG_N);
            let h_flag = cpu.flag(FLAG_H);
            let old_a = cpu.a;

            // Lower nibble check: always applies regardless of N flag.
            if h_flag || (cpu.a & 0x0F) > 9 {
                correction |= 0x06;
            }
            // Upper nibble check: always applies regardless of N flag.
            if carry || cpu.a > 0x99 {
                correction |= 0x60;
                carry = true;
            }

            if n_flag {
                cpu.a = cpu.a.wrapping_sub(correction);
            } else {
                cpu.a = cpu.a.wrapping_add(correction);
            }

            let result = cpu.a;
            cpu.set_flag(FLAG_S, result & 0x80 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, (old_a ^ result) & 0x10 != 0);
            cpu.set_flag(FLAG_PV, parity(result));
            cpu.set_flag(FLAG_C, carry);
            // N unchanged
            cpu.set_flag(FLAG_X, result & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, result & FLAG_Y != 0);
            4
        }

        // ── LD HL,(nn) ────────────────────────────────────────────
        0x2A => {
            let addr = fetch_word(cpu, bus);
            let lo = bus.read_byte(addr);
            let hi = bus.read_byte(addr.wrapping_add(1));
            cpu.set_hl((u16::from(hi) << 8) | u16::from(lo));
            16
        }

        // ── CPL ───────────────────────────────────────────────────
        0x2F => {
            cpu.a = !cpu.a;
            cpu.set_flag(FLAG_H, true);
            cpu.set_flag(FLAG_N, true);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── LD (nn),A ─────────────────────────────────────────────
        0x32 => {
            let addr = fetch_word(cpu, bus);
            bus.write_byte(addr, cpu.a);
            13
        }

        // ── SCF ───────────────────────────────────────────────────
        0x37 => {
            cpu.set_flag(FLAG_C, true);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── LD A,(nn) ─────────────────────────────────────────────
        0x3A => {
            let addr = fetch_word(cpu, bus);
            cpu.a = bus.read_byte(addr);
            13
        }

        // ── CCF ───────────────────────────────────────────────────
        0x3F => {
            let old_c = cpu.flag(FLAG_C);
            cpu.set_flag(FLAG_H, old_c);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_C, !old_c);
            cpu.set_flag(FLAG_X, cpu.a & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, cpu.a & FLAG_Y != 0);
            4
        }

        // ── LD r,r' (0x40-0x7F except 0x76) ──────────────────────
        // This covers all register-to-register and register-(HL) loads.
        0x40..=0x75 | 0x77..=0x7F => {
            let dst = (opcode >> 3) & 0x07;
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            write_reg8(cpu, bus, dst, val);
            // Timing: 4 normally, 7 if (HL) is involved
            if src == 6 || dst == 6 { 7 } else { 4 }
        }

        // ── HALT ──────────────────────────────────────────────────
        0x76 => {
            cpu.halted = true;
            4
        }

        // ── ALU A,r (0x80-0xBF) ──────────────────────────────────
        // ADD A,r (0x80-0x87)
        0x80..=0x87 => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_add(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // ADC A,r (0x88-0x8F)
        0x88..=0x8F => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_adc(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // SUB r (0x90-0x97)
        0x90..=0x97 => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_sub(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // SBC A,r (0x98-0x9F)
        0x98..=0x9F => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_sbc(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // AND r (0xA0-0xA7)
        0xA0..=0xA7 => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_and(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // XOR r (0xA8-0xAF)
        0xA8..=0xAF => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_xor(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // OR r (0xB0-0xB7)
        0xB0..=0xB7 => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_or(cpu, val);
            if src == 6 { 7 } else { 4 }
        }
        // CP r (0xB8-0xBF)
        0xB8..=0xBF => {
            let src = opcode & 0x07;
            let val = read_reg8(cpu, bus, src);
            alu_cp(cpu, val);
            if src == 6 { 7 } else { 4 }
        }

        // ── RET cc (conditional return) ───────────────────────────
        // 0xC0: RET NZ  0xC8: RET Z  0xD0: RET NC  0xD8: RET C
        // 0xE0: RET PO  0xE8: RET PE  0xF0: RET P   0xF8: RET M
        0xC0 | 0xC8 | 0xD0 | 0xD8 | 0xE0 | 0xE8 | 0xF0 | 0xF8 => {
            let cc = (opcode >> 3) & 0x07;
            if condition_met(cpu, cc) {
                cpu.pc = pop(cpu, bus);
                11
            } else {
                5
            }
        }

        // ── POP rr ────────────────────────────────────────────────
        // 0xC1: POP BC  0xD1: POP DE  0xE1: POP HL  0xF1: POP AF
        0xC1 | 0xD1 | 0xE1 | 0xF1 => {
            let val = pop(cpu, bus);
            let pair = (opcode >> 4) & 0x03;
            match pair {
                0 => cpu.set_bc(val),
                1 => cpu.set_de(val),
                2 => cpu.set_hl(val),
                3 => cpu.set_af(val),
                _ => unreachable!(),
            }
            10
        }

        // ── JP cc,nn (conditional absolute jump) ──────────────────
        // 0xC2: JP NZ,nn  0xCA: JP Z,nn  0xD2: JP NC,nn  0xDA: JP C,nn
        // 0xE2: JP PO,nn  0xEA: JP PE,nn  0xF2: JP P,nn  0xFA: JP M,nn
        0xC2 | 0xCA | 0xD2 | 0xDA | 0xE2 | 0xEA | 0xF2 | 0xFA => {
            let addr = fetch_word(cpu, bus);
            let cc = (opcode >> 3) & 0x07;
            if condition_met(cpu, cc) {
                cpu.pc = addr;
            }
            10
        }

        // ── JP nn ─────────────────────────────────────────────────
        0xC3 => {
            cpu.pc = fetch_word(cpu, bus);
            10
        }

        // ── CALL cc,nn (conditional call) ─────────────────────────
        // 0xC4: CALL NZ  0xCC: CALL Z  0xD4: CALL NC  0xDC: CALL C
        // 0xE4: CALL PO  0xEC: CALL PE  0xF4: CALL P   0xFC: CALL M
        0xC4 | 0xCC | 0xD4 | 0xDC | 0xE4 | 0xEC | 0xF4 | 0xFC => {
            let addr = fetch_word(cpu, bus);
            let cc = (opcode >> 3) & 0x07;
            if condition_met(cpu, cc) {
                push(cpu, bus, cpu.pc);
                cpu.pc = addr;
                17
            } else {
                10
            }
        }

        // ── PUSH rr ──────────────────────────────────────────────
        // 0xC5: PUSH BC  0xD5: PUSH DE  0xE5: PUSH HL  0xF5: PUSH AF
        0xC5 | 0xD5 | 0xE5 | 0xF5 => {
            let pair = (opcode >> 4) & 0x03;
            let val = match pair {
                0 => cpu.bc(),
                1 => cpu.de(),
                2 => cpu.hl(),
                3 => cpu.af(),
                _ => unreachable!(),
            };
            push(cpu, bus, val);
            11
        }

        // ── ALU A,n (immediate ALU operations) ───────────────────
        0xC6 => {
            let n = fetch_byte(cpu, bus);
            alu_add(cpu, n);
            7
        }
        0xCE => {
            let n = fetch_byte(cpu, bus);
            alu_adc(cpu, n);
            7
        }
        0xD6 => {
            let n = fetch_byte(cpu, bus);
            alu_sub(cpu, n);
            7
        }
        0xDE => {
            let n = fetch_byte(cpu, bus);
            alu_sbc(cpu, n);
            7
        }
        0xE6 => {
            let n = fetch_byte(cpu, bus);
            alu_and(cpu, n);
            7
        }
        0xEE => {
            let n = fetch_byte(cpu, bus);
            alu_xor(cpu, n);
            7
        }
        0xF6 => {
            let n = fetch_byte(cpu, bus);
            alu_or(cpu, n);
            7
        }
        0xFE => {
            let n = fetch_byte(cpu, bus);
            alu_cp(cpu, n);
            7
        }

        // ── RST n ─────────────────────────────────────────────────
        // 0xC7: RST 00  0xCF: RST 08  0xD7: RST 10  0xDF: RST 18
        // 0xE7: RST 20  0xEF: RST 28  0xF7: RST 30  0xFF: RST 38
        0xC7 | 0xCF | 0xD7 | 0xDF | 0xE7 | 0xEF | 0xF7 | 0xFF => {
            push(cpu, bus, cpu.pc);
            cpu.pc = u16::from(opcode & 0x38);
            11
        }

        // ── RET ───────────────────────────────────────────────────
        0xC9 => {
            cpu.pc = pop(cpu, bus);
            10
        }

        // ── CB prefix (bit operations) ────────────────────────────
        0xCB => {
            // Placeholder: consume the sub-opcode byte, return 4 T-states.
            let _sub = fetch_byte(cpu, bus);
            // Increment R for the second fetch.
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            4
        }

        // ── CALL nn ───────────────────────────────────────────────
        0xCD => {
            let addr = fetch_word(cpu, bus);
            push(cpu, bus, cpu.pc);
            cpu.pc = addr;
            17
        }

        // ── OUT (n),A ─────────────────────────────────────────────
        0xD3 => {
            let port_lo = fetch_byte(cpu, bus);
            let port = (u16::from(cpu.a) << 8) | u16::from(port_lo);
            bus.write_port(port, cpu.a);
            11
        }

        // ── EXX ───────────────────────────────────────────────────
        0xD9 => {
            std::mem::swap(&mut cpu.b, &mut cpu.b_prime);
            std::mem::swap(&mut cpu.c, &mut cpu.c_prime);
            std::mem::swap(&mut cpu.d, &mut cpu.d_prime);
            std::mem::swap(&mut cpu.e, &mut cpu.e_prime);
            std::mem::swap(&mut cpu.h, &mut cpu.h_prime);
            std::mem::swap(&mut cpu.l, &mut cpu.l_prime);
            4
        }

        // ── IN A,(n) ──────────────────────────────────────────────
        0xDB => {
            let port_lo = fetch_byte(cpu, bus);
            let port = (u16::from(cpu.a) << 8) | u16::from(port_lo);
            cpu.a = bus.read_port(port);
            11
        }

        // ── DD prefix (IX operations) ─────────────────────────────
        0xDD => {
            let _sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            4
        }

        // ── EX (SP),HL ────────────────────────────────────────────
        0xE3 => {
            let lo = bus.read_byte(cpu.sp);
            let hi = bus.read_byte(cpu.sp.wrapping_add(1));
            let old_hl = cpu.hl();
            cpu.set_hl((u16::from(hi) << 8) | u16::from(lo));
            bus.write_byte(cpu.sp, old_hl as u8);
            bus.write_byte(cpu.sp.wrapping_add(1), (old_hl >> 8) as u8);
            19
        }

        // ── JP (HL) ──────────────────────────────────────────────
        0xE9 => {
            cpu.pc = cpu.hl();
            4
        }

        // ── EX DE,HL ──────────────────────────────────────────────
        0xEB => {
            std::mem::swap(&mut cpu.d, &mut cpu.h);
            std::mem::swap(&mut cpu.e, &mut cpu.l);
            4
        }

        // ── ED prefix (extended operations) ───────────────────────
        0xED => {
            let _sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            4
        }

        // ── DI ────────────────────────────────────────────────────
        0xF3 => {
            cpu.iff1 = false;
            cpu.iff2 = false;
            cpu.ei_pending = false;
            4
        }

        // ── LD SP,HL ──────────────────────────────────────────────
        0xF9 => {
            cpu.sp = cpu.hl();
            6
        }

        // ── EI ────────────────────────────────────────────────────
        0xFB => {
            cpu.iff1 = true;
            cpu.iff2 = true;
            cpu.ei_pending = true;
            4
        }

        // ── FD prefix (IY operations) ─────────────────────────────
        0xFD => {
            let _sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            4
        }

        // Catch-all for any remaining opcodes — should not be reached
        // since we cover 0x00-0xFF above.
        #[allow(unreachable_patterns)]
        _ => 4,
    }
}

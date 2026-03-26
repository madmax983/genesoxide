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
            let sub = fetch_byte(cpu, bus);
            // Increment R for the second fetch.
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            execute_cb(cpu, bus, sub)
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
            let sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            execute_ddfd(cpu, bus, sub, true)
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
            let sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            execute_ed(cpu, bus, sub)
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
            let sub = fetch_byte(cpu, bus);
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_add(1)) & 0x7F);
            execute_ddfd(cpu, bus, sub, false)
        }

        // Catch-all for any remaining opcodes — should not be reached
        // since we cover 0x00-0xFF above.
        #[allow(unreachable_patterns)]
        _ => 4,
    }
}

// ── CB prefix handler ──────────────────────────────────────────────────

/// Executes a CB-prefixed opcode. Returns T-states consumed (excluding the
/// 4 T-states for fetching the CB byte itself — caller adds those implicitly
/// via the main fetch). Total is 8 for register ops, 15 for (HL) modify, 12 for BIT (HL).
fn execute_cb(cpu: &mut Z80, bus: &mut dyn Bus, sub: u8) -> u8 {
    let reg = sub & 0x07;
    let op = sub >> 6;
    let bit = (sub >> 3) & 0x07;

    match op {
        0 => {
            // Rotate/shift operations (0x00-0x3F)
            let val = read_reg8(cpu, bus, reg);
            let result = match bit {
                0 => {
                    // RLC
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | bit7;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                1 => {
                    // RRC
                    let bit0 = val & 1;
                    let r = (val >> 1) | (bit0 << 7);
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                2 => {
                    // RL
                    let old_c = if cpu.flag(FLAG_C) { 1u8 } else { 0 };
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | old_c;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                3 => {
                    // RR
                    let old_c = if cpu.flag(FLAG_C) { 0x80u8 } else { 0 };
                    let bit0 = val & 1;
                    let r = (val >> 1) | old_c;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                4 => {
                    // SLA
                    let bit7 = (val >> 7) & 1;
                    let r = val << 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                5 => {
                    // SRA
                    let bit0 = val & 1;
                    let r = (val >> 1) | (val & 0x80); // preserve sign bit
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                6 => {
                    // SLL (undocumented) — shift left, bit 0 = 1
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                7 => {
                    // SRL
                    let bit0 = val & 1;
                    let r = val >> 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                _ => unreachable!(),
            };
            write_reg8(cpu, bus, reg, result);
            if reg == 6 { 15 } else { 8 }
        }
        1 => {
            // BIT b,r (0x40-0x7F)
            let val = read_reg8(cpu, bus, reg);
            let tested = val & (1 << bit);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            cpu.set_flag(FLAG_Z, tested == 0);
            cpu.set_flag(FLAG_H, true);
            cpu.set_flag(FLAG_S, bit == 7 && tested != 0);
            cpu.set_flag(FLAG_PV, tested == 0); // same as Z
            cpu.set_flag(FLAG_C, old_c);
            // N = 0 already
            if reg == 6 {
                // BIT b,(HL): X and Y from high byte of WZ (internal MEMPTR)
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                12
            } else {
                // BIT b,r: X and Y from the value tested
                cpu.set_flag(FLAG_X, val & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, val & FLAG_Y != 0);
                8
            }
        }
        2 => {
            // RES b,r (0x80-0xBF)
            let val = read_reg8(cpu, bus, reg);
            let result = val & !(1 << bit);
            write_reg8(cpu, bus, reg, result);
            if reg == 6 { 15 } else { 8 }
        }
        3 => {
            // SET b,r (0xC0-0xFF)
            let val = read_reg8(cpu, bus, reg);
            let result = val | (1 << bit);
            write_reg8(cpu, bus, reg, result);
            if reg == 6 { 15 } else { 8 }
        }
        _ => unreachable!(),
    }
}

// ── ED prefix handler ──────────────────────────────────────────────────

/// Reads a 16-bit register pair for ED instructions (BC=0, DE=1, HL=2, SP=3).
#[inline]
fn read_reg16_ed(cpu: &Z80, pair: u8) -> u16 {
    match pair {
        0 => cpu.bc(),
        1 => cpu.de(),
        2 => cpu.hl(),
        3 => cpu.sp,
        _ => unreachable!(),
    }
}

/// Writes a 16-bit register pair for ED instructions.
#[inline]
fn write_reg16_ed(cpu: &mut Z80, pair: u8, val: u16) {
    match pair {
        0 => cpu.set_bc(val),
        1 => cpu.set_de(val),
        2 => cpu.set_hl(val),
        3 => cpu.sp = val,
        _ => unreachable!(),
    }
}

/// Executes an ED-prefixed opcode. Returns T-states.
fn execute_ed(cpu: &mut Z80, bus: &mut dyn Bus, sub: u8) -> u8 {
    match sub {
        // ── IN r,(C) — 0x40,0x48,0x50,0x58,0x60,0x68,0x70,0x78
        0x40 | 0x48 | 0x50 | 0x58 | 0x60 | 0x68 | 0x70 | 0x78 => {
            let port = cpu.bc();
            let val = bus.read_port(port);
            let reg = (sub >> 3) & 0x07;
            if reg != 6 {
                // reg 6 = "IN (C)" / "IN F,(C)" — reads but doesn't store (flags only)
                write_reg8(cpu, bus, reg, val);
            }
            // Set flags: S, Z, H=0, PV=parity, N=0, C unchanged
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            set_sz_xy(cpu, val);
            cpu.set_flag(FLAG_PV, parity(val));
            cpu.set_flag(FLAG_C, old_c);
            // H = 0, N = 0 already
            12
        }

        // ── OUT (C),r — 0x41,0x49,0x51,0x59,0x61,0x69,0x71,0x79
        0x41 | 0x49 | 0x51 | 0x59 | 0x61 | 0x69 | 0x71 | 0x79 => {
            let port = cpu.bc();
            let reg = (sub >> 3) & 0x07;
            let val = if reg == 6 {
                0
            } else {
                read_reg8(cpu, bus, reg)
            };
            bus.write_port(port, val);
            12
        }

        // ── SBC HL,rr — 0x42,0x52,0x62,0x72
        0x42 | 0x52 | 0x62 | 0x72 => {
            let pair = (sub >> 4) & 0x03;
            let hl = cpu.hl();
            let rr = read_reg16_ed(cpu, pair);
            let carry = if cpu.flag(FLAG_C) { 1u32 } else { 0 };
            let result32 = (hl as u32).wrapping_sub(rr as u32).wrapping_sub(carry);
            let result = result32 as u16;
            cpu.set_hl(result);

            cpu.f = FLAG_N;
            cpu.set_flag(FLAG_S, result & 0x8000 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, ((hl ^ rr ^ result) >> 8) & 0x10 != 0);
            // Overflow: operands different sign, result sign differs from HL
            let hl_s = hl as i16;
            let rr_s = rr as i16;
            let c_s = carry as i16;
            let expected = (hl_s as i32) - (rr_s as i32) - (c_s as i32);
            cpu.set_flag(FLAG_PV, !(-32768..=32767).contains(&expected));
            cpu.set_flag(FLAG_C, (hl as u32) < (rr as u32) + carry);
            let high = (result >> 8) as u8;
            cpu.set_flag(FLAG_X, high & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, high & FLAG_Y != 0);
            15
        }

        // ── ADC HL,rr — 0x4A,0x5A,0x6A,0x7A
        0x4A | 0x5A | 0x6A | 0x7A => {
            let pair = (sub >> 4) & 0x03;
            let hl = cpu.hl();
            let rr = read_reg16_ed(cpu, pair);
            let carry = if cpu.flag(FLAG_C) { 1u32 } else { 0 };
            let result32 = (hl as u32) + (rr as u32) + carry;
            let result = result32 as u16;
            cpu.set_hl(result);

            cpu.f = 0; // N = 0
            cpu.set_flag(FLAG_S, result & 0x8000 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, ((hl ^ rr ^ result) >> 8) & 0x10 != 0);
            let hl_s = hl as i16;
            let rr_s = rr as i16;
            let c_s = carry as i16;
            let expected = (hl_s as i32) + (rr_s as i32) + (c_s as i32);
            cpu.set_flag(FLAG_PV, !(-32768..=32767).contains(&expected));
            cpu.set_flag(FLAG_C, result32 > 0xFFFF);
            let high = (result >> 8) as u8;
            cpu.set_flag(FLAG_X, high & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, high & FLAG_Y != 0);
            15
        }

        // ── LD (nn),rr — 0x43,0x53,0x63,0x73
        0x43 | 0x53 | 0x63 | 0x73 => {
            let addr = fetch_word(cpu, bus);
            let pair = (sub >> 4) & 0x03;
            let val = read_reg16_ed(cpu, pair);
            bus.write_byte(addr, val as u8);
            bus.write_byte(addr.wrapping_add(1), (val >> 8) as u8);
            20
        }

        // ── LD rr,(nn) — 0x4B,0x5B,0x6B,0x7B
        0x4B | 0x5B | 0x6B | 0x7B => {
            let addr = fetch_word(cpu, bus);
            let pair = (sub >> 4) & 0x03;
            let lo = bus.read_byte(addr);
            let hi = bus.read_byte(addr.wrapping_add(1));
            let val = (u16::from(hi) << 8) | u16::from(lo);
            write_reg16_ed(cpu, pair, val);
            20
        }

        // ── NEG — 0x44 and mirrors 0x4C,0x54,0x5C,0x64,0x6C,0x74,0x7C
        0x44 | 0x4C | 0x54 | 0x5C | 0x64 | 0x6C | 0x74 | 0x7C => {
            let old_a = cpu.a;
            cpu.a = 0u8.wrapping_sub(old_a);
            let result = cpu.a;
            cpu.f = FLAG_N;
            set_sz_xy(cpu, result);
            cpu.set_flag(FLAG_H, (old_a & 0x0F) != 0);
            cpu.set_flag(FLAG_PV, old_a == 0x80);
            cpu.set_flag(FLAG_C, old_a != 0);
            8
        }

        // ── RETN — 0x45 and mirrors 0x55,0x65,0x75
        0x45 | 0x55 | 0x65 | 0x75 => {
            cpu.pc = pop(cpu, bus);
            cpu.iff1 = cpu.iff2;
            14
        }

        // ── RETI — 0x4D and mirrors 0x5D,0x6D,0x7D
        0x4D | 0x5D | 0x6D | 0x7D => {
            cpu.pc = pop(cpu, bus);
            cpu.iff1 = cpu.iff2;
            14
        }

        // ── IM 0 — 0x46, 0x4E, 0x66, 0x6E
        0x46 | 0x4E | 0x66 | 0x6E => {
            cpu.im = 0;
            8
        }

        // ── IM 1 — 0x56, 0x76
        0x56 | 0x76 => {
            cpu.im = 1;
            8
        }

        // ── IM 2 — 0x5E, 0x7E
        0x5E | 0x7E => {
            cpu.im = 2;
            8
        }

        // ── LD I,A — 0x47
        0x47 => {
            cpu.i = cpu.a;
            9
        }

        // ── LD R,A — 0x4F
        0x4F => {
            cpu.r = cpu.a;
            9
        }

        // ── LD A,I — 0x57
        0x57 => {
            cpu.a = cpu.i;
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            set_sz_xy(cpu, cpu.a);
            cpu.set_flag(FLAG_PV, cpu.iff2);
            cpu.set_flag(FLAG_C, old_c);
            // H = 0, N = 0 already
            9
        }

        // ── LD A,R — 0x5F
        0x5F => {
            cpu.a = cpu.r;
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            set_sz_xy(cpu, cpu.a);
            cpu.set_flag(FLAG_PV, cpu.iff2);
            cpu.set_flag(FLAG_C, old_c);
            9
        }

        // ── RRD — 0x67
        0x67 => {
            let addr = cpu.hl();
            let mem = bus.read_byte(addr);
            let old_a = cpu.a;
            cpu.a = (old_a & 0xF0) | (mem & 0x0F);
            let new_mem = ((old_a & 0x0F) << 4) | ((mem >> 4) & 0x0F);
            bus.write_byte(addr, new_mem);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            set_sz_xy(cpu, cpu.a);
            cpu.set_flag(FLAG_PV, parity(cpu.a));
            cpu.set_flag(FLAG_C, old_c);
            18
        }

        // ── RLD — 0x6F
        0x6F => {
            let addr = cpu.hl();
            let mem = bus.read_byte(addr);
            let old_a = cpu.a;
            cpu.a = (old_a & 0xF0) | ((mem >> 4) & 0x0F);
            let new_mem = ((mem & 0x0F) << 4) | (old_a & 0x0F);
            bus.write_byte(addr, new_mem);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            set_sz_xy(cpu, cpu.a);
            cpu.set_flag(FLAG_PV, parity(cpu.a));
            cpu.set_flag(FLAG_C, old_c);
            18
        }

        // ── LDI — 0xA0
        0xA0 => {
            let val = bus.read_byte(cpu.hl());
            bus.write_byte(cpu.de(), val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            cpu.set_de(cpu.de().wrapping_add(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let n = cpu.a.wrapping_add(val);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_X, n & 0x08 != 0); // bit 3
            cpu.set_flag(FLAG_Y, n & 0x02 != 0); // bit 1
            16
        }

        // ── LDD — 0xA8
        0xA8 => {
            let val = bus.read_byte(cpu.hl());
            bus.write_byte(cpu.de(), val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            cpu.set_de(cpu.de().wrapping_sub(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let n = cpu.a.wrapping_add(val);
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_X, n & 0x08 != 0);
            cpu.set_flag(FLAG_Y, n & 0x02 != 0);
            16
        }

        // ── CPI — 0xA1
        0xA1 => {
            let val = bus.read_byte(cpu.hl());
            let result = cpu.a.wrapping_sub(val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let hf = (cpu.a & 0x0F) < (val & 0x0F);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = FLAG_N;
            cpu.set_flag(FLAG_S, result & 0x80 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, hf);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_C, old_c);
            let n = result.wrapping_sub(if hf { 1 } else { 0 });
            cpu.set_flag(FLAG_X, n & 0x08 != 0); // bit 3
            cpu.set_flag(FLAG_Y, n & 0x02 != 0); // bit 1
            16
        }

        // ── CPD — 0xA9
        0xA9 => {
            let val = bus.read_byte(cpu.hl());
            let result = cpu.a.wrapping_sub(val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let hf = (cpu.a & 0x0F) < (val & 0x0F);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = FLAG_N;
            cpu.set_flag(FLAG_S, result & 0x80 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, hf);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_C, old_c);
            let n = result.wrapping_sub(if hf { 1 } else { 0 });
            cpu.set_flag(FLAG_X, n & 0x08 != 0);
            cpu.set_flag(FLAG_Y, n & 0x02 != 0);
            16
        }

        // ── INI — 0xA2
        0xA2 => {
            let port = cpu.bc();
            let val = bus.read_port(port);
            bus.write_byte(cpu.hl(), val);
            cpu.b = cpu.b.wrapping_sub(1);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            cpu.set_flag(FLAG_Z, cpu.b == 0);
            cpu.set_flag(FLAG_N, true);
            // S, H, PV, C are "undefined" per Zilog but jsmoo expects specific values
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.c.wrapping_add(1) as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            16
        }

        // ── IND — 0xAA
        0xAA => {
            let port = cpu.bc();
            let val = bus.read_port(port);
            bus.write_byte(cpu.hl(), val);
            cpu.b = cpu.b.wrapping_sub(1);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.c.wrapping_sub(1) as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            16
        }

        // ── OUTI — 0xA3
        0xA3 => {
            let val = bus.read_byte(cpu.hl());
            cpu.b = cpu.b.wrapping_sub(1);
            let port = cpu.bc();
            bus.write_port(port, val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.l as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            16
        }

        // ── OUTD — 0xAB
        0xAB => {
            let val = bus.read_byte(cpu.hl());
            cpu.b = cpu.b.wrapping_sub(1);
            let port = cpu.bc();
            bus.write_port(port, val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.l as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            16
        }

        // ── LDIR — 0xB0
        0xB0 => {
            let val = bus.read_byte(cpu.hl());
            bus.write_byte(cpu.de(), val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            cpu.set_de(cpu.de().wrapping_add(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            if cpu.bc() != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                cpu.set_flag(FLAG_PV, true);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                cpu.set_flag(FLAG_PV, false);
                let n = cpu.a.wrapping_add(val);
                cpu.set_flag(FLAG_X, n & 0x08 != 0);
                cpu.set_flag(FLAG_Y, n & 0x02 != 0);
                16
            }
        }

        // ── LDDR — 0xB8
        0xB8 => {
            let val = bus.read_byte(cpu.hl());
            bus.write_byte(cpu.de(), val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            cpu.set_de(cpu.de().wrapping_sub(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            cpu.set_flag(FLAG_H, false);
            cpu.set_flag(FLAG_N, false);
            if cpu.bc() != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                cpu.set_flag(FLAG_PV, true);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                cpu.set_flag(FLAG_PV, false);
                let n = cpu.a.wrapping_add(val);
                cpu.set_flag(FLAG_X, n & 0x08 != 0);
                cpu.set_flag(FLAG_Y, n & 0x02 != 0);
                16
            }
        }

        // ── CPIR — 0xB1
        0xB1 => {
            let val = bus.read_byte(cpu.hl());
            let result = cpu.a.wrapping_sub(val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let hf = (cpu.a & 0x0F) < (val & 0x0F);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = FLAG_N;
            cpu.set_flag(FLAG_S, result & 0x80 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, hf);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_C, old_c);
            if cpu.bc() != 0 && result != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                let n = result.wrapping_sub(if hf { 1 } else { 0 });
                cpu.set_flag(FLAG_X, n & 0x08 != 0);
                cpu.set_flag(FLAG_Y, n & 0x02 != 0);
                16
            }
        }

        // ── CPDR — 0xB9
        0xB9 => {
            let val = bus.read_byte(cpu.hl());
            let result = cpu.a.wrapping_sub(val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            cpu.set_bc(cpu.bc().wrapping_sub(1));
            let hf = (cpu.a & 0x0F) < (val & 0x0F);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = FLAG_N;
            cpu.set_flag(FLAG_S, result & 0x80 != 0);
            cpu.set_flag(FLAG_Z, result == 0);
            cpu.set_flag(FLAG_H, hf);
            cpu.set_flag(FLAG_PV, cpu.bc() != 0);
            cpu.set_flag(FLAG_C, old_c);
            if cpu.bc() != 0 && result != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                let n = result.wrapping_sub(if hf { 1 } else { 0 });
                cpu.set_flag(FLAG_X, n & 0x08 != 0);
                cpu.set_flag(FLAG_Y, n & 0x02 != 0);
                16
            }
        }

        // ── INIR — 0xB2
        0xB2 => {
            let port = cpu.bc();
            let val = bus.read_port(port);
            bus.write_byte(cpu.hl(), val);
            cpu.b = cpu.b.wrapping_sub(1);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.c.wrapping_add(1) as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            if cpu.b != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                16
            }
        }

        // ── INDR — 0xBA
        0xBA => {
            let port = cpu.bc();
            let val = bus.read_port(port);
            bus.write_byte(cpu.hl(), val);
            cpu.b = cpu.b.wrapping_sub(1);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.c.wrapping_sub(1) as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            if cpu.b != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                16
            }
        }

        // ── OTIR — 0xB3
        0xB3 => {
            let val = bus.read_byte(cpu.hl());
            cpu.b = cpu.b.wrapping_sub(1);
            let port = cpu.bc();
            bus.write_port(port, val);
            cpu.set_hl(cpu.hl().wrapping_add(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.l as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            if cpu.b != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                16
            }
        }

        // ── OTDR — 0xBB
        0xBB => {
            let val = bus.read_byte(cpu.hl());
            cpu.b = cpu.b.wrapping_sub(1);
            let port = cpu.bc();
            bus.write_port(port, val);
            cpu.set_hl(cpu.hl().wrapping_sub(1));
            set_sz_xy(cpu, cpu.b);
            cpu.set_flag(FLAG_N, val & 0x80 != 0);
            let k = val as u16 + cpu.l as u16;
            cpu.set_flag(FLAG_H, k > 255);
            cpu.set_flag(FLAG_C, k > 255);
            cpu.set_flag(FLAG_PV, parity(((k & 7) as u8) ^ cpu.b));
            if cpu.b != 0 {
                cpu.pc = cpu.pc.wrapping_sub(2);
                cpu.wz = cpu.pc.wrapping_add(1);
                let wz_hi = (cpu.wz >> 8) as u8;
                cpu.set_flag(FLAG_X, wz_hi & FLAG_X != 0);
                cpu.set_flag(FLAG_Y, wz_hi & FLAG_Y != 0);
                21
            } else {
                16
            }
        }

        // ── Undefined ED opcodes → NOP-like (8 T-states total)
        _ => 8,
    }
}

// ── DD/FD prefix handler ───────────────────────────────────────────────

/// Reads the high byte of IX or IY.
#[inline]
fn index_high(cpu: &Z80, is_ix: bool) -> u8 {
    if is_ix {
        (cpu.ix >> 8) as u8
    } else {
        (cpu.iy >> 8) as u8
    }
}

/// Reads the low byte of IX or IY.
#[inline]
fn index_low(cpu: &Z80, is_ix: bool) -> u8 {
    if is_ix { cpu.ix as u8 } else { cpu.iy as u8 }
}

/// Reads the full IX or IY register.
#[inline]
fn index_reg(cpu: &Z80, is_ix: bool) -> u16 {
    if is_ix { cpu.ix } else { cpu.iy }
}

/// Sets the full IX or IY register.
#[inline]
fn set_index_reg(cpu: &mut Z80, is_ix: bool, val: u16) {
    if is_ix {
        cpu.ix = val;
    } else {
        cpu.iy = val;
    }
}

/// Sets the high byte of IX or IY.
#[inline]
fn set_index_high(cpu: &mut Z80, is_ix: bool, val: u8) {
    if is_ix {
        cpu.ix = (cpu.ix & 0x00FF) | (u16::from(val) << 8);
    } else {
        cpu.iy = (cpu.iy & 0x00FF) | (u16::from(val) << 8);
    }
}

/// Sets the low byte of IX or IY.
#[inline]
fn set_index_low(cpu: &mut Z80, is_ix: bool, val: u8) {
    if is_ix {
        cpu.ix = (cpu.ix & 0xFF00) | u16::from(val);
    } else {
        cpu.iy = (cpu.iy & 0xFF00) | u16::from(val);
    }
}

/// Reads an 8-bit register for DD/FD prefixed ops.
/// H maps to IXH/IYH, L maps to IXL/IYL (undocumented).
/// (HL) maps to (IX+d)/(IY+d) — caller must handle that case separately.
#[inline]
fn read_reg8_indexed(cpu: &Z80, bus: &mut dyn Bus, reg: u8, is_ix: bool) -> u8 {
    match reg {
        0 => cpu.b,
        1 => cpu.c,
        2 => cpu.d,
        3 => cpu.e,
        4 => index_high(cpu, is_ix),
        5 => index_low(cpu, is_ix),
        6 => bus.read_byte(cpu.hl()), // shouldn't be called for (HL)
        7 => cpu.a,
        _ => unreachable!(),
    }
}

/// Writes an 8-bit register for DD/FD prefixed ops.
#[inline]
fn write_reg8_indexed(cpu: &mut Z80, bus: &mut dyn Bus, reg: u8, val: u8, is_ix: bool) {
    match reg {
        0 => cpu.b = val,
        1 => cpu.c = val,
        2 => cpu.d = val,
        3 => cpu.e = val,
        4 => set_index_high(cpu, is_ix, val),
        5 => set_index_low(cpu, is_ix, val),
        6 => bus.write_byte(cpu.hl(), val), // shouldn't be called for (HL)
        7 => cpu.a = val,
        _ => unreachable!(),
    }
}

/// Reads a 16-bit register pair for DD/FD ADD instructions,
/// where pair 2 = IX/IY instead of HL.
#[inline]
fn read_reg16_indexed(cpu: &Z80, pair: u8, is_ix: bool) -> u16 {
    match pair {
        0 => cpu.bc(),
        1 => cpu.de(),
        2 => index_reg(cpu, is_ix),
        3 => cpu.sp,
        _ => unreachable!(),
    }
}

/// Executes a DD/FD-prefixed opcode. is_ix = true for DD (IX), false for FD (IY).
/// Returns total T-states for the prefixed instruction.
fn execute_ddfd(cpu: &mut Z80, bus: &mut dyn Bus, sub: u8, is_ix: bool) -> u8 {
    match sub {
        // DD/FD followed by another prefix: treat current as NOP, let main loop
        // re-process. We already consumed the sub-opcode byte and incremented R.
        // The next prefix byte will be re-fetched by the main loop.
        // Actually, we need to "put back" the PC so the next instruction can be
        // fetched. We decrement PC by 1 to re-fetch the prefix byte.
        0xDD | 0xFD | 0xED => {
            // Treat current prefix as NOP (4 T-states), re-process next byte
            cpu.pc = cpu.pc.wrapping_sub(1);
            // Undo the R increment for the sub-opcode (we already incremented for
            // the prefix and the sub-opcode, but we want only the prefix increment)
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_sub(1)) & 0x7F);
            4
        }

        // ── DD/FD CB — indexed bit operations
        0xCB => {
            // DD CB dd op / FD CB dd op
            // The displacement byte comes BEFORE the opcode byte
            let d = fetch_byte(cpu, bus) as i8;
            let op = fetch_byte(cpu, bus);
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            execute_ddfd_cb(cpu, bus, op, addr)
        }

        // ── LD IX/IY,nn — 0x21
        0x21 => {
            let val = fetch_word(cpu, bus);
            set_index_reg(cpu, is_ix, val);
            14
        }

        // ── LD (nn),IX/IY — 0x22
        0x22 => {
            let addr = fetch_word(cpu, bus);
            let val = index_reg(cpu, is_ix);
            bus.write_byte(addr, val as u8);
            bus.write_byte(addr.wrapping_add(1), (val >> 8) as u8);
            20
        }

        // ── INC IX/IY — 0x23
        0x23 => {
            let val = index_reg(cpu, is_ix).wrapping_add(1);
            set_index_reg(cpu, is_ix, val);
            10
        }

        // ── INC IXH/IYH — 0x24
        0x24 => {
            let val = index_high(cpu, is_ix);
            let result = alu_inc(cpu, val);
            set_index_high(cpu, is_ix, result);
            8
        }

        // ── DEC IXH/IYH — 0x25
        0x25 => {
            let val = index_high(cpu, is_ix);
            let result = alu_dec(cpu, val);
            set_index_high(cpu, is_ix, result);
            8
        }

        // ── LD IXH/IYH,n — 0x26
        0x26 => {
            let val = fetch_byte(cpu, bus);
            set_index_high(cpu, is_ix, val);
            11
        }

        // ── LD IX/IY,(nn) — 0x2A
        0x2A => {
            let addr = fetch_word(cpu, bus);
            let lo = bus.read_byte(addr);
            let hi = bus.read_byte(addr.wrapping_add(1));
            set_index_reg(cpu, is_ix, (u16::from(hi) << 8) | u16::from(lo));
            20
        }

        // ── DEC IX/IY — 0x2B
        0x2B => {
            let val = index_reg(cpu, is_ix).wrapping_sub(1);
            set_index_reg(cpu, is_ix, val);
            10
        }

        // ── INC IXL/IYL — 0x2C
        0x2C => {
            let val = index_low(cpu, is_ix);
            let result = alu_inc(cpu, val);
            set_index_low(cpu, is_ix, result);
            8
        }

        // ── DEC IXL/IYL — 0x2D
        0x2D => {
            let val = index_low(cpu, is_ix);
            let result = alu_dec(cpu, val);
            set_index_low(cpu, is_ix, result);
            8
        }

        // ── LD IXL/IYL,n — 0x2E
        0x2E => {
            let val = fetch_byte(cpu, bus);
            set_index_low(cpu, is_ix, val);
            11
        }

        // ── ADD IX/IY,rr — 0x09,0x19,0x29,0x39
        0x09 | 0x19 | 0x29 | 0x39 => {
            let pair = (sub >> 4) & 0x03;
            let idx = index_reg(cpu, is_ix) as u32;
            let rr = read_reg16_indexed(cpu, pair, is_ix) as u32;
            let result = idx + rr;
            set_index_reg(cpu, is_ix, result as u16);
            // Preserve S, Z, PV
            cpu.set_flag(FLAG_H, ((idx ^ rr ^ result) >> 8) & 0x10 != 0);
            cpu.set_flag(FLAG_N, false);
            cpu.set_flag(FLAG_C, result > 0xFFFF);
            let high = (result >> 8) as u8;
            cpu.set_flag(FLAG_X, high & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, high & FLAG_Y != 0);
            15
        }

        // ── INC (IX/IY+d) — 0x34
        0x34 => {
            let d = fetch_byte(cpu, bus) as i8;
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            let val = bus.read_byte(addr);
            let result = alu_inc(cpu, val);
            bus.write_byte(addr, result);
            23
        }

        // ── DEC (IX/IY+d) — 0x35
        0x35 => {
            let d = fetch_byte(cpu, bus) as i8;
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            let val = bus.read_byte(addr);
            let result = alu_dec(cpu, val);
            bus.write_byte(addr, result);
            23
        }

        // ── LD (IX/IY+d),n — 0x36
        0x36 => {
            let d = fetch_byte(cpu, bus) as i8;
            let n = fetch_byte(cpu, bus);
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            bus.write_byte(addr, n);
            19
        }

        // ── LD r,(IX/IY+d) — reg in bits 5-3 of sub-opcode
        // 0x46: LD B,(IX+d)  0x4E: LD C,(IX+d)
        // 0x56: LD D,(IX+d)  0x5E: LD E,(IX+d)
        // 0x66: LD H,(IX+d)  0x6E: LD L,(IX+d)  0x7E: LD A,(IX+d)
        0x46 | 0x4E | 0x56 | 0x5E | 0x66 | 0x6E | 0x7E => {
            let d = fetch_byte(cpu, bus) as i8;
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            let val = bus.read_byte(addr);
            let dst = (sub >> 3) & 0x07;
            // Write to actual register (not indexed version) for dst=4,5
            write_reg8(cpu, bus, dst, val);
            19
        }

        // ── LD (IX/IY+d),r — reg in bits 2-0 of sub-opcode
        // 0x70: LD (IX+d),B  0x71: LD (IX+d),C  0x72: LD (IX+d),D
        // 0x73: LD (IX+d),E  0x74: LD (IX+d),H  0x75: LD (IX+d),L
        // 0x77: LD (IX+d),A
        0x70 | 0x71 | 0x72 | 0x73 | 0x74 | 0x75 | 0x77 => {
            let d = fetch_byte(cpu, bus) as i8;
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            let src = sub & 0x07;
            // Read from actual register (not indexed version) for src=4,5
            let val = read_reg8(cpu, bus, src);
            bus.write_byte(addr, val);
            19
        }

        // ── LD r,r' with IXH/IXL substitution (undocumented)
        // These are register-to-register loads where H→IXH, L→IXL
        // Covers 0x40-0x6F range (excluding (HL) cases handled above)
        // and 0x78-0x7F (excluding 0x7E handled above)
        0x40..=0x45
        | 0x47..=0x4D
        | 0x4F..=0x55
        | 0x57..=0x5D
        | 0x5F
        | 0x60..=0x65
        | 0x67..=0x6D
        | 0x6F
        | 0x78..=0x7D
        | 0x7F => {
            let dst = (sub >> 3) & 0x07;
            let src = sub & 0x07;
            let val = read_reg8_indexed(cpu, bus, src, is_ix);
            write_reg8_indexed(cpu, bus, dst, val, is_ix);
            8
        }

        // ── ALU A,(IX/IY+d)
        // ADD: 0x86  ADC: 0x8E  SUB: 0x96  SBC: 0x9E
        // AND: 0xA6  XOR: 0xAE  OR:  0xB6  CP:  0xBE
        0x86 | 0x8E | 0x96 | 0x9E | 0xA6 | 0xAE | 0xB6 | 0xBE => {
            let d = fetch_byte(cpu, bus) as i8;
            let addr = index_reg(cpu, is_ix).wrapping_add(d as u16);
            let val = bus.read_byte(addr);
            let alu_op = (sub >> 3) & 0x07;
            match alu_op {
                0 => alu_add(cpu, val),
                1 => alu_adc(cpu, val),
                2 => alu_sub(cpu, val),
                3 => alu_sbc(cpu, val),
                4 => alu_and(cpu, val),
                5 => alu_xor(cpu, val),
                6 => alu_or(cpu, val),
                7 => alu_cp(cpu, val),
                _ => unreachable!(),
            }
            19
        }

        // ── ALU A,r with IXH/IXL substitution (undocumented)
        // ADD A,IXH/IXL, ADC, SUB, SBC, AND, XOR, OR, CP
        0x80..=0x85
        | 0x87..=0x8D
        | 0x8F..=0x95
        | 0x97..=0x9D
        | 0x9F
        | 0xA0..=0xA5
        | 0xA7..=0xAD
        | 0xAF..=0xB5
        | 0xB7..=0xBD
        | 0xBF => {
            let src = sub & 0x07;
            let val = read_reg8_indexed(cpu, bus, src, is_ix);
            let alu_op = (sub >> 3) & 0x07;
            match alu_op {
                0 => alu_add(cpu, val),
                1 => alu_adc(cpu, val),
                2 => alu_sub(cpu, val),
                3 => alu_sbc(cpu, val),
                4 => alu_and(cpu, val),
                5 => alu_xor(cpu, val),
                6 => alu_or(cpu, val),
                7 => alu_cp(cpu, val),
                _ => unreachable!(),
            }
            8
        }

        // ── POP IX/IY — 0xE1
        0xE1 => {
            let val = pop(cpu, bus);
            set_index_reg(cpu, is_ix, val);
            14
        }

        // ── EX (SP),IX/IY — 0xE3
        0xE3 => {
            let lo = bus.read_byte(cpu.sp);
            let hi = bus.read_byte(cpu.sp.wrapping_add(1));
            let old_idx = index_reg(cpu, is_ix);
            set_index_reg(cpu, is_ix, (u16::from(hi) << 8) | u16::from(lo));
            bus.write_byte(cpu.sp, old_idx as u8);
            bus.write_byte(cpu.sp.wrapping_add(1), (old_idx >> 8) as u8);
            23
        }

        // ── PUSH IX/IY — 0xE5
        0xE5 => {
            let val = index_reg(cpu, is_ix);
            push(cpu, bus, val);
            15
        }

        // ── JP (IX/IY) — 0xE9
        0xE9 => {
            cpu.pc = index_reg(cpu, is_ix);
            8
        }

        // ── LD SP,IX/IY — 0xF9
        0xF9 => {
            cpu.sp = index_reg(cpu, is_ix);
            10
        }

        // Any other opcode under DD/FD is executed as normal (no prefix effect).
        // We need to re-execute the sub-opcode as a normal instruction.
        // Since we already consumed it, we put it back and re-execute.
        _ => {
            // Undo: put the sub-opcode byte back by rewinding PC
            cpu.pc = cpu.pc.wrapping_sub(1);
            // Undo the R increment for the sub-opcode
            cpu.r = (cpu.r & 0x80) | ((cpu.r.wrapping_sub(1)) & 0x7F);
            // Execute as normal (the main execute will re-fetch and re-increment R)
            // But we already consumed 4 T-states for the prefix fetch.
            // The re-execute will add the normal instruction cycles.
            // We return 4 for the prefix NOP and let the caller handle it...
            // Actually, for jsmoo tests, these are executed as one instruction.
            // We need to just execute the normal instruction and add 4.
            // Re-fetch and execute:
            let cycles = execute_instruction(cpu, bus);
            // The execute_instruction already incremented R for the sub-opcode,
            // which is what we want (total R increments = 2 for prefix + sub).
            // But we un-decremented R above, so the net is correct.
            4 + cycles
        }
    }
}

// ── DD CB / FD CB handler ──────────────────────────────────────────────

/// Executes a DD CB dd op / FD CB dd op instruction.
/// `addr` is the pre-computed IX/IY + d address.
/// `op` is the CB-style operation byte.
/// Returns T-states for the indexed bit operation portion.
fn execute_ddfd_cb(cpu: &mut Z80, bus: &mut dyn Bus, op: u8, addr: u16) -> u8 {
    let reg = op & 0x07;
    let bit = (op >> 3) & 0x07;
    let operation = op >> 6;

    match operation {
        0 => {
            // Rotate/shift at (IX/IY+d) with undocumented copy to register
            let val = bus.read_byte(addr);
            let result = match bit {
                0 => {
                    // RLC
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | bit7;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                1 => {
                    // RRC
                    let bit0 = val & 1;
                    let r = (val >> 1) | (bit0 << 7);
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                2 => {
                    // RL
                    let old_c = if cpu.flag(FLAG_C) { 1u8 } else { 0 };
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | old_c;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                3 => {
                    // RR
                    let old_c = if cpu.flag(FLAG_C) { 0x80u8 } else { 0 };
                    let bit0 = val & 1;
                    let r = (val >> 1) | old_c;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                4 => {
                    // SLA
                    let bit7 = (val >> 7) & 1;
                    let r = val << 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                5 => {
                    // SRA
                    let bit0 = val & 1;
                    let r = (val >> 1) | (val & 0x80);
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                6 => {
                    // SLL (undocumented)
                    let bit7 = (val >> 7) & 1;
                    let r = (val << 1) | 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit7 != 0);
                    r
                }
                7 => {
                    // SRL
                    let bit0 = val & 1;
                    let r = val >> 1;
                    cpu.f = 0;
                    set_sz_xy(cpu, r);
                    cpu.set_flag(FLAG_PV, parity(r));
                    cpu.set_flag(FLAG_C, bit0 != 0);
                    r
                }
                _ => unreachable!(),
            };
            bus.write_byte(addr, result);
            // Undocumented: also store result in register (unless reg==6, memory only)
            if reg != 6 {
                write_reg8(cpu, bus, reg, result);
            }
            23
        }
        1 => {
            // BIT b,(IX/IY+d)
            let val = bus.read_byte(addr);
            let tested = val & (1 << bit);
            let old_c = cpu.flag(FLAG_C);
            cpu.f = 0;
            cpu.set_flag(FLAG_Z, tested == 0);
            cpu.set_flag(FLAG_H, true);
            cpu.set_flag(FLAG_S, bit == 7 && tested != 0);
            cpu.set_flag(FLAG_PV, tested == 0);
            cpu.set_flag(FLAG_C, old_c);
            // X and Y from high byte of computed address
            let addr_hi = (addr >> 8) as u8;
            cpu.set_flag(FLAG_X, addr_hi & FLAG_X != 0);
            cpu.set_flag(FLAG_Y, addr_hi & FLAG_Y != 0);
            20
        }
        2 => {
            // RES b,(IX/IY+d) with undocumented copy
            let val = bus.read_byte(addr);
            let result = val & !(1 << bit);
            bus.write_byte(addr, result);
            if reg != 6 {
                write_reg8(cpu, bus, reg, result);
            }
            23
        }
        3 => {
            // SET b,(IX/IY+d) with undocumented copy
            let val = bus.read_byte(addr);
            let result = val | (1 << bit);
            bus.write_byte(addr, result);
            if reg != 6 {
                write_reg8(cpu, bus, reg, result);
            }
            23
        }
        _ => unreachable!(),
    }
}

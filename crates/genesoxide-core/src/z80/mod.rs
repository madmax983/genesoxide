//! Zilog Z80 CPU emulation.
//!
//! The Z80 is an 8-bit processor with:
//! - Main register set: A, F, B, C, D, E, H, L
//! - Shadow register set: A', F', B', C', D', E', H', L'
//! - Index registers: IX, IY (16-bit)
//! - Stack pointer (SP), program counter (PC)
//! - Interrupt vector register (I), refresh register (R)
//! - Two interrupt flip-flops (IFF1, IFF2) and interrupt mode (IM)
//!
//! In the Genesis, it runs at ~3.58 MHz (master clock / 15) and drives
//! the YM2612 FM synthesizer and SN76489 PSG sound chips.

pub mod decode;
pub mod execute;

pub use decode::{Instruction, decode};
pub use execute::{Bus, execute_instruction};

use serde::{Deserialize, Serialize};

// ── Flag constants ──────────────────────────────────────────────────────

/// Carry flag (bit 0).
pub const FLAG_C: u8 = 0x01;
/// Add/Subtract flag (bit 1).
pub const FLAG_N: u8 = 0x02;
/// Parity/Overflow flag (bit 2).
pub const FLAG_PV: u8 = 0x04;
/// Undocumented bit 3 (X/F3).
pub const FLAG_X: u8 = 0x08;
/// Half-carry flag (bit 4).
pub const FLAG_H: u8 = 0x10;
/// Undocumented bit 5 (Y/F5).
pub const FLAG_Y: u8 = 0x20;
/// Zero flag (bit 6).
pub const FLAG_Z: u8 = 0x40;
/// Sign flag (bit 7).
pub const FLAG_S: u8 = 0x80;

// ── CPU state ───────────────────────────────────────────────────────────

/// Zilog Z80 CPU state.
#[derive(Debug, Clone)]
pub struct Z80 {
    // ── Main register set ───────────────────────────────────────────
    /// Accumulator.
    pub a: u8,
    /// Flags register.
    pub f: u8,
    /// General-purpose register B.
    pub b: u8,
    /// General-purpose register C.
    pub c: u8,
    /// General-purpose register D.
    pub d: u8,
    /// General-purpose register E.
    pub e: u8,
    /// General-purpose register H.
    pub h: u8,
    /// General-purpose register L.
    pub l: u8,

    // ── Shadow register set ─────────────────────────────────────────
    /// Shadow accumulator (A').
    pub a_prime: u8,
    /// Shadow flags (F').
    pub f_prime: u8,
    /// Shadow B'.
    pub b_prime: u8,
    /// Shadow C'.
    pub c_prime: u8,
    /// Shadow D'.
    pub d_prime: u8,
    /// Shadow E'.
    pub e_prime: u8,
    /// Shadow H'.
    pub h_prime: u8,
    /// Shadow L'.
    pub l_prime: u8,

    // ── Index registers ─────────────────────────────────────────────
    /// Index register IX.
    pub ix: u16,
    /// Index register IY.
    pub iy: u16,

    // ── Control registers ───────────────────────────────────────────
    /// Stack pointer.
    pub sp: u16,
    /// Program counter.
    pub pc: u16,
    /// Interrupt vector register.
    pub i: u8,
    /// Memory refresh register.
    pub r: u8,
    /// Interrupt flip-flop 1.
    pub iff1: bool,
    /// Interrupt flip-flop 2.
    pub iff2: bool,
    /// Interrupt mode (0, 1, or 2).
    pub im: u8,
    /// Whether the CPU is halted (HALT instruction).
    pub halted: bool,
    /// Total T-states elapsed.
    pub cycles: u64,

    // ── Internal state ──────────────────────────────────────────────
    /// EI delays enabling interrupts by one instruction.
    pub ei_pending: bool,
    /// Internal WZ/MEMPTR register (used for undocumented flag behavior).
    pub wz: u16,
    /// Internal Q register: the value written to F by the last instruction
    /// that modified the flags, or 0 if the last instruction left F alone.
    /// Consumed by SCF/CCF to reconstruct their undocumented X/Y flags.
    pub q: u8,
    /// Level-triggered maskable interrupt line (directly connected to VDP
    /// V-blank on the Genesis). When asserted (`true`) and IFF1 is set,
    /// the Z80 will service the interrupt before the next instruction.
    pub int_line: bool,
}

impl Z80 {
    /// Creates a new Z80 with all registers zeroed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            a: 0,
            f: 0,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
            h: 0,
            l: 0,
            a_prime: 0,
            f_prime: 0,
            b_prime: 0,
            c_prime: 0,
            d_prime: 0,
            e_prime: 0,
            h_prime: 0,
            l_prime: 0,
            ix: 0,
            iy: 0,
            sp: 0,
            pc: 0,
            i: 0,
            r: 0,
            iff1: false,
            iff2: false,
            im: 0,
            halted: false,
            cycles: 0,
            ei_pending: false,
            wz: 0,
            q: 0,
            int_line: false,
        }
    }

    /// Reset the Z80 to its power-on state, as if the /RESET pin was asserted.
    ///
    /// Clears PC, I, R, interrupt flip-flops, and sets interrupt mode to 0.
    /// The cycle counter is preserved. General-purpose registers are technically
    /// undefined after reset on real hardware, but we zero them for determinism.
    /// Hardware reset: only PC, I, R, IM, and interrupt flip-flops are
    /// affected.  All data registers (A/F, BC, DE, HL, IX, IY, SP, shadows)
    /// retain their previous values — the SMPS sound driver relies on HL
    /// surviving reset so that `JP (HL)` at address 0 can re-enter the idle
    /// loop.
    pub fn reset(&mut self) {
        self.pc = 0;
        self.i = 0;
        self.r = 0;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.halted = false;
        self.ei_pending = false;
        self.int_line = false;
        // cycles intentionally preserved
    }

    // ── Register pair accessors ─────────────────────────────────────

    /// Returns the AF register pair (A = high, F = low).
    #[must_use]
    pub fn af(&self) -> u16 {
        (u16::from(self.a) << 8) | u16::from(self.f)
    }

    /// Sets the AF register pair (A = high, F = low).
    pub fn set_af(&mut self, val: u16) {
        self.a = (val >> 8) as u8;
        self.f = val as u8;
    }

    /// Returns the BC register pair (B = high, C = low).
    #[must_use]
    pub fn bc(&self) -> u16 {
        (u16::from(self.b) << 8) | u16::from(self.c)
    }

    /// Sets the BC register pair (B = high, C = low).
    pub fn set_bc(&mut self, val: u16) {
        self.b = (val >> 8) as u8;
        self.c = val as u8;
    }

    /// Returns the DE register pair (D = high, E = low).
    #[must_use]
    pub fn de(&self) -> u16 {
        (u16::from(self.d) << 8) | u16::from(self.e)
    }

    /// Sets the DE register pair (D = high, E = low).
    pub fn set_de(&mut self, val: u16) {
        self.d = (val >> 8) as u8;
        self.e = val as u8;
    }

    /// Returns the HL register pair (H = high, L = low).
    #[must_use]
    pub fn hl(&self) -> u16 {
        (u16::from(self.h) << 8) | u16::from(self.l)
    }

    /// Sets the HL register pair (H = high, L = low).
    pub fn set_hl(&mut self, val: u16) {
        self.h = (val >> 8) as u8;
        self.l = val as u8;
    }

    // ── Flag helpers ────────────────────────────────────────────────

    /// Returns true if the given flag bit(s) are set.
    #[must_use]
    pub fn flag(&self, mask: u8) -> bool {
        self.f & mask != 0
    }

    /// Sets or clears a flag bit.
    pub fn set_flag(&mut self, mask: u8, value: bool) {
        if value {
            self.f |= mask;
        } else {
            self.f &= !mask;
        }
    }

    // ── Snapshot ────────────────────────────────────────────────────

    /// Captures a serializable snapshot of the CPU state.
    #[must_use]
    pub fn snapshot(&self) -> Z80Snapshot {
        Z80Snapshot {
            a: self.a,
            f: self.f,
            b: self.b,
            c: self.c,
            d: self.d,
            e: self.e,
            h: self.h,
            l: self.l,
            a_prime: self.a_prime,
            f_prime: self.f_prime,
            b_prime: self.b_prime,
            c_prime: self.c_prime,
            d_prime: self.d_prime,
            e_prime: self.e_prime,
            h_prime: self.h_prime,
            l_prime: self.l_prime,
            ix: self.ix,
            iy: self.iy,
            sp: self.sp,
            pc: self.pc,
            i: self.i,
            r: self.r,
            iff1: self.iff1,
            iff2: self.iff2,
            im: self.im,
            halted: self.halted,
            cycles: self.cycles,
            ei_pending: self.ei_pending,
            wz: self.wz,
            q: self.q,
            int_line: self.int_line,
        }
    }

    /// Restores CPU state from a snapshot.
    pub fn restore(&mut self, snap: &Z80Snapshot) {
        self.a = snap.a;
        self.f = snap.f;
        self.b = snap.b;
        self.c = snap.c;
        self.d = snap.d;
        self.e = snap.e;
        self.h = snap.h;
        self.l = snap.l;
        self.a_prime = snap.a_prime;
        self.f_prime = snap.f_prime;
        self.b_prime = snap.b_prime;
        self.c_prime = snap.c_prime;
        self.d_prime = snap.d_prime;
        self.e_prime = snap.e_prime;
        self.h_prime = snap.h_prime;
        self.l_prime = snap.l_prime;
        self.ix = snap.ix;
        self.iy = snap.iy;
        self.sp = snap.sp;
        self.pc = snap.pc;
        self.i = snap.i;
        self.r = snap.r;
        self.iff1 = snap.iff1;
        self.iff2 = snap.iff2;
        self.im = snap.im;
        self.halted = snap.halted;
        self.cycles = snap.cycles;
        self.ei_pending = snap.ei_pending;
        self.wz = snap.wz;
        self.q = snap.q;
        self.int_line = snap.int_line;
    }
}

impl Default for Z80 {
    fn default() -> Self {
        Self::new()
    }
}

// ── Snapshot ────────────────────────────────────────────────────────────

/// Serializable snapshot of Z80 CPU state for save states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Z80Snapshot {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub a_prime: u8,
    pub f_prime: u8,
    pub b_prime: u8,
    pub c_prime: u8,
    pub d_prime: u8,
    pub e_prime: u8,
    pub h_prime: u8,
    pub l_prime: u8,
    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,
    pub i: u8,
    pub r: u8,
    pub iff1: bool,
    pub iff2: bool,
    pub im: u8,
    pub halted: bool,
    pub cycles: u64,
    pub ei_pending: bool,
    pub wz: u16,
    /// Internal Q register (see [`Z80::q`]); must round-trip so SCF/CCF
    /// undocumented flags stay deterministic across save-state/rewind.
    pub q: u8,
    pub int_line: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_z80_is_zeroed() {
        let z80 = Z80::new();
        assert_eq!(z80.pc, 0);
        assert_eq!(z80.sp, 0);
        assert_eq!(z80.a, 0);
        assert!(!z80.halted);
    }

    #[test]
    fn register_pair_accessors() {
        let mut z80 = Z80::new();
        z80.b = 0x12;
        z80.c = 0x34;
        assert_eq!(z80.bc(), 0x1234);
        z80.set_hl(0xABCD);
        assert_eq!(z80.h, 0xAB);
        assert_eq!(z80.l, 0xCD);
    }

    #[test]
    fn flag_set_and_test() {
        let mut z80 = Z80::new();
        z80.set_flag(FLAG_Z, true);
        assert!(z80.flag(FLAG_Z));
        z80.set_flag(FLAG_Z, false);
        assert!(!z80.flag(FLAG_Z));
    }
}

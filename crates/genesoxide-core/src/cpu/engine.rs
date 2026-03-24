//! 68000 CPU execution engine.
//!
//! Owns the register file and executes instructions by reading from and
//! writing to a memory bus provided by the caller.

use super::{CpuSnapshot, StatusRegister};

/// The Motorola 68000 CPU.
#[derive(Debug, Clone)]
pub struct Cpu {
    /// Data registers D0-D7.
    pub d: [u32; 8],
    /// Address registers A0-A6. A7 is handled via usp/ssp.
    pub a: [u32; 7],
    /// Program counter (24-bit, upper byte ignored).
    pub pc: u32,
    /// Status register (CCR + system byte).
    pub sr: StatusRegister,
    /// Supervisor stack pointer.
    pub ssp: u32,
    /// User stack pointer.
    pub usp: u32,
    /// Total CPU cycles elapsed.
    pub cycles: u64,
    /// CPU is halted (double bus fault).
    pub halted: bool,
    /// CPU is stopped (STOP instruction, waiting for interrupt).
    pub stopped: bool,
}

impl Cpu {
    /// Creates a new CPU in its power-on state.
    ///
    /// After power-on, the CPU reads the initial SSP from address 0x000000
    /// and the initial PC from address 0x000004. This must be done by the
    /// caller after construction (requires bus access).
    #[must_use]
    pub fn new() -> Self {
        Self {
            d: [0; 8],
            a: [0; 7],
            pc: 0,
            sr: StatusRegister::new(StatusRegister::S), // supervisor mode on reset
            ssp: 0,
            usp: 0,
            cycles: 0,
            halted: false,
            stopped: false,
        }
    }

    /// Returns the active stack pointer (SSP in supervisor mode, USP otherwise).
    #[must_use]
    pub fn sp(&self) -> u32 {
        if self.sr.supervisor() {
            self.ssp
        } else {
            self.usp
        }
    }

    /// Sets the active stack pointer.
    pub fn set_sp(&mut self, value: u32) {
        if self.sr.supervisor() {
            self.ssp = value;
        } else {
            self.usp = value;
        }
    }

    /// Returns address register by index (0-7, where 7 = active SP).
    #[must_use]
    pub fn read_a(&self, reg: u8) -> u32 {
        if reg == 7 {
            self.sp()
        } else {
            self.a[reg as usize]
        }
    }

    /// Sets address register by index (0-7, where 7 = active SP).
    pub fn write_a(&mut self, reg: u8, value: u32) {
        if reg == 7 {
            self.set_sp(value);
        } else {
            self.a[reg as usize] = value;
        }
    }

    /// Masks PC to 24 bits.
    #[must_use]
    pub fn masked_pc(&self) -> u32 {
        self.pc & 0x00FF_FFFF
    }

    /// Snapshot for save states.
    #[must_use]
    pub fn snapshot(&self) -> CpuSnapshot {
        let mut a = [0u32; 8];
        a[..7].copy_from_slice(&self.a);
        a[7] = self.sp();
        CpuSnapshot {
            d: self.d,
            a,
            pc: self.pc,
            sr: self.sr.0,
            ssp: self.ssp,
            usp: self.usp,
            cycles: self.cycles,
            halted: self.halted,
            stopped: self.stopped,
        }
    }

    /// Restore from a snapshot.
    pub fn restore(&mut self, snap: &CpuSnapshot) {
        self.d = snap.d;
        self.a.copy_from_slice(&snap.a[..7]);
        self.pc = snap.pc;
        self.sr = StatusRegister::new(snap.sr);
        self.ssp = snap.ssp;
        self.usp = snap.usp;
        self.cycles = snap.cycles;
        self.halted = snap.halted;
        self.stopped = snap.stopped;
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_on_state() {
        let cpu = Cpu::new();
        assert_eq!(cpu.d, [0; 8]);
        assert!(cpu.sr.supervisor());
        assert!(!cpu.halted);
        assert!(!cpu.stopped);
    }

    #[test]
    fn stack_pointer_switches_on_mode() {
        let mut cpu = Cpu::new();
        // Supervisor mode — sp() returns ssp
        cpu.ssp = 0x1000;
        cpu.usp = 0x2000;
        assert_eq!(cpu.sp(), 0x1000);

        // Switch to user mode
        cpu.sr.set_flag(StatusRegister::S, false);
        assert_eq!(cpu.sp(), 0x2000);
    }

    #[test]
    fn address_register_7_is_sp() {
        let mut cpu = Cpu::new();
        cpu.ssp = 0xABCD;
        assert_eq!(cpu.read_a(7), 0xABCD);

        cpu.write_a(7, 0x1234);
        assert_eq!(cpu.ssp, 0x1234);
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut cpu = Cpu::new();
        cpu.d[3] = 0xDEAD;
        cpu.a[2] = 0xBEEF;
        cpu.pc = 0x000200;
        cpu.ssp = 0xFFFE;
        cpu.cycles = 42;

        let snap = cpu.snapshot();
        let mut restored = Cpu::new();
        restored.restore(&snap);

        assert_eq!(restored.d[3], 0xDEAD);
        assert_eq!(restored.a[2], 0xBEEF);
        assert_eq!(restored.pc, 0x000200);
        assert_eq!(restored.ssp, 0xFFFE);
        assert_eq!(restored.cycles, 42);
    }
}

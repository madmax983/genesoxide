//! Global hardware cycle counters.
//!
//! Tracks aggregate 68000/VDP cycle counts. Same pattern as NES scheduler:
//! deterministic counters for stepping logic, timing, and snapshots.
//!
//! Genesis timing: 68000 @ 7.670453 MHz, VDP @ ~13.423294 MHz.
//! Ratio: roughly 1 CPU cycle = 7/4 VDP cycles (1.75), but the exact
//! relationship depends on the master clock divider. We use integer
//! master clock ticks to avoid floating point.
//!
//! Master clock (NTSC): 53.693175 MHz
//! - 68000 = master / 7 = 7.670 MHz
//! - VDP   = master / 4 = 13.423 MHz

use serde::{Deserialize, Serialize};

/// NTSC Genesis master clock frequency in Hz.
pub const MASTER_CLOCK_NTSC: u64 = 53_693_175;
/// Master clock ticks per 68000 CPU cycle.
pub const MASTER_PER_CPU: u64 = 7;
/// Master clock ticks per VDP cycle.
pub const MASTER_PER_VDP: u64 = 4;
/// Master clock ticks per Z80 cycle.
pub const MASTER_PER_Z80: u64 = 15;

/// Serializable scheduler snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    /// Total master clock ticks elapsed.
    pub master_ticks: u64,
    /// Total 68000 CPU cycles elapsed.
    pub cpu_cycles: u64,
    /// Total VDP cycles elapsed.
    pub vdp_cycles: u64,
}

/// Live cycle counters for the running core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheduler {
    master_ticks: u64,
    cpu_cycles: u64,
    vdp_cycles: u64,
}

impl Scheduler {
    /// Creates a zeroed scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            master_ticks: 0,
            cpu_cycles: 0,
            vdp_cycles: 0,
        }
    }

    /// Advances by one 68000 CPU cycle (7 master ticks).
    pub fn step_cpu_cycle(&mut self) {
        self.cpu_cycles = self.cpu_cycles.wrapping_add(1);
        self.master_ticks = self.master_ticks.wrapping_add(MASTER_PER_CPU);
    }

    /// Records one VDP cycle executed. Does not advance master clock —
    /// master time is driven only by the CPU. The VDP catches up to it.
    pub fn step_vdp_cycle(&mut self) {
        self.vdp_cycles = self.vdp_cycles.wrapping_add(1);
    }

    /// Batch-advances by `n` CPU cycles, updating master ticks accordingly.
    #[inline(always)]
    pub fn advance_cpu(&mut self, n: u64) {
        self.cpu_cycles = self.cpu_cycles.wrapping_add(n);
        self.master_ticks = self.master_ticks.wrapping_add(n * MASTER_PER_CPU);
    }

    /// Returns the number of VDP cycles that should have elapsed given
    /// the current master tick count.
    #[must_use]
    pub fn expected_vdp_cycles(&self) -> u64 {
        self.master_ticks / MASTER_PER_VDP
    }

    /// Returns how many VDP cycles the VDP needs to catch up.
    #[must_use]
    pub fn vdp_catchup_cycles(&self) -> u64 {
        self.expected_vdp_cycles().saturating_sub(self.vdp_cycles)
    }

    /// Snapshot for serialization.
    #[must_use]
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            master_ticks: self.master_ticks,
            cpu_cycles: self.cpu_cycles,
            vdp_cycles: self.vdp_cycles,
        }
    }

    /// Restore from a snapshot.
    pub fn restore(&mut self, snap: &SchedulerSnapshot) {
        self.master_ticks = snap.master_ticks;
        self.cpu_cycles = snap.cpu_cycles;
        self.vdp_cycles = snap.vdp_cycles;
    }

    /// Total CPU cycles elapsed.
    #[must_use]
    pub fn cpu_cycles(&self) -> u64 {
        self.cpu_cycles
    }

    /// Total VDP cycles elapsed.
    #[must_use]
    pub fn vdp_cycles(&self) -> u64 {
        self.vdp_cycles
    }

    /// Total master clock ticks.
    #[must_use]
    pub fn master_ticks(&self) -> u64 {
        self.master_ticks
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_cpu_advances_master() {
        let mut sched = Scheduler::new();
        sched.step_cpu_cycle();
        assert_eq!(sched.cpu_cycles(), 1);
        assert_eq!(sched.master_ticks(), MASTER_PER_CPU);
    }

    #[test]
    fn vdp_catchup() {
        let mut sched = Scheduler::new();
        // 4 CPU cycles = 28 master ticks = 7 VDP cycles expected
        sched.advance_cpu(4);
        assert_eq!(sched.expected_vdp_cycles(), 28 / MASTER_PER_VDP);
        assert_eq!(sched.vdp_catchup_cycles(), 7);

        // Simulate VDP catching up
        for _ in 0..7 {
            sched.step_vdp_cycle();
        }
        assert_eq!(sched.vdp_catchup_cycles(), 0);
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut sched = Scheduler::new();
        sched.advance_cpu(100);
        let snap = sched.snapshot();

        let mut restored = Scheduler::new();
        restored.restore(&snap);
        assert_eq!(sched, restored);
    }
}

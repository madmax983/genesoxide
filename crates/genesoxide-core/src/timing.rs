//! Central line-timing constants for the Genesis/Mega Drive.
//!
//! This module is the single source of truth for the *derived* line/frame
//! timing the CPU-stepping, audio-synthesis, and DMA-pacing paths share, so the
//! emulator's notion of "how long a scanline is" never diverges between them.
//!
//! The primitive master-clock / divider constants and the region model
//! (NTSC vs PAL line counts, master clock, [`crate::scheduler::Region`]) live in
//! [`crate::scheduler`]; this module re-exports the ones the timing math needs
//! and layers the NTSC frame-total and DMA-budget derivations on top. Region-
//! generic code should prefer the [`crate::scheduler::Region`] accessors
//! (`total_scanlines()`, `master_clock()`) and the region-invariant
//! [`MASTER_TICKS_PER_LINE_H40`]; the `*_NTSC` constants below are the nominal
//! NTSC values kept for tests and NTSC-specific assertions.
//!
//! # Hardware reference (NTSC)
//!
//! The NTSC console runs off a single 53.693175 MHz master oscillator. Every
//! other clock is an integer division of it:
//!
//! - 68000 CPU: master / 7  = 7.670453 MHz
//! - VDP:       master / 4  = 13.423294 MHz (H40 pixel dot clock derivation)
//! - Z80:       master / 15 = 3.579545 MHz
//!
//! One H40 scanline is exactly 3420 master-clock ticks, and an NTSC frame is
//! 262 scanlines, giving 3420 × 262 = 896_040 master ticks per frame and a
//! refresh of 53_693_175 / 896_040 ≈ 59.9227 Hz. PAL uses the same 3420
//! ticks/line but 313 lines (see [`crate::scheduler::Region::total_scanlines`]).

// The primitive divider/clock constants and the region model are owned by
// `crate::scheduler`. Re-export the ones the timing math and downstream modules
// reference so there is exactly one definition of each value.
pub use crate::scheduler::{MASTER_CLOCK_NTSC, MASTER_PER_CPU, MASTER_PER_VDP, MASTER_PER_Z80};

/// Master-clock ticks per scanline in H40 (40-cell) timing.
///
/// The exact hardware figure: one H40 line is 3420 master ticks. This is
/// region-invariant (NTSC and PAL only differ in master-clock rate and lines
/// per frame), and is the authoritative line length used to advance the
/// CPU/scheduler timeline and the audio-synthesis timeline in lockstep so they
/// never drift apart. Aliased to [`crate::scheduler::H40_LINE_MASTER_TICKS`].
pub const MASTER_TICKS_PER_LINE_H40: u64 = crate::scheduler::H40_LINE_MASTER_TICKS;

/// Total scanlines per NTSC frame: 224 active + 38 blanking = 262.
///
/// Aliased to [`crate::scheduler::TOTAL_SCANLINES_NTSC`]. Region-generic code
/// should use [`crate::scheduler::Region::total_scanlines`] instead (313 for
/// PAL); this constant is the nominal NTSC value used by NTSC-specific timing.
pub const LINES_PER_FRAME_NTSC: u64 = crate::scheduler::TOTAL_SCANLINES_NTSC as u64;

/// Active (visible) scanlines per NTSC frame (V28).
///
/// PAL V30 has 240 active lines; region-aware code should query
/// `Vdp::active_height()` rather than this constant. Kept as the NTSC nominal
/// value for tests and NTSC assertions.
pub const ACTIVE_SCANLINES: u16 = 224;

/// Master-clock ticks per full NTSC frame.
///
/// `MASTER_TICKS_PER_LINE_H40 * LINES_PER_FRAME_NTSC` = 3420 × 262 = 896_040.
/// A whole-frame advance of the master clock must equal this value on the
/// long-run average; the resulting refresh is `MASTER_CLOCK_NTSC /
/// MASTER_TICKS_PER_FRAME_NTSC` ≈ 59.9227 Hz.
pub const MASTER_TICKS_PER_FRAME_NTSC: u64 =
    MASTER_TICKS_PER_LINE_H40 * LINES_PER_FRAME_NTSC;

/// Integer 68000-cycle approximation of one H40 scanline, used only for DMA
/// pacing (cycle-stealing budget).
///
/// `3420 / 7 = 488.57`, truncated to 488. This is *not* used to bound the
/// per-scanline CPU run (that is driven precisely by [`MASTER_TICKS_PER_LINE_H40`]
/// on the master-tick timeline); it is only the coarse per-line word-transfer
/// budget the VDP charges a DMA against, matching the Software Manual DMA
/// slot-rate tables which are themselves quoted per scanline.
pub const CPU_CYCLES_PER_LINE_H40: u32 = 488;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_length_is_exact_hardware_value() {
        assert_eq!(MASTER_TICKS_PER_FRAME_NTSC, 896_040);
        assert_eq!(
            MASTER_TICKS_PER_FRAME_NTSC,
            MASTER_TICKS_PER_LINE_H40 * LINES_PER_FRAME_NTSC
        );
    }

    #[test]
    fn refresh_rate_is_about_59_92_hz() {
        let hz = MASTER_CLOCK_NTSC as f64 / MASTER_TICKS_PER_FRAME_NTSC as f64;
        assert!(
            (hz - 59.92).abs() < 0.01,
            "NTSC refresh should be ~59.92 Hz, got {hz}"
        );
    }
}

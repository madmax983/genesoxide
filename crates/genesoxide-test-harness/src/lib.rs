//! Test harness for ROM-based integration testing.
//!
//! Provides utilities for loading test ROMs, running them for a fixed
//! number of frames, and comparing output against golden references.
//!
//! Also includes the m68000-tests runner for cycle-accurate CPU validation
//! against MAME-generated test vectors.

pub mod m68k_tests;
pub mod rom_builder;
pub mod vgm;
pub mod ymfm_reference;
pub mod z80_tests;

use genesoxide_core::{Command, FRAME_RGBA_BYTES, GenesisCore};

/// Loads a ROM and runs it for the given number of frames.
/// Returns the final framebuffer as a Vec<u8>.
#[must_use]
pub fn run_rom_frames(rom_data: Vec<u8>, frames: u64) -> Vec<u8> {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data));

    for _ in 0..frames {
        core.execute(Command::StepFrame);
    }

    core.framebuffer_rgba().to_vec()
}

/// Compares two framebuffers pixel-by-pixel.
/// Returns the number of differing pixels.
#[must_use]
pub fn compare_framebuffers(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), FRAME_RGBA_BYTES);
    assert_eq!(b.len(), FRAME_RGBA_BYTES);

    a.chunks(4)
        .zip(b.chunks(4))
        .filter(|(pa, pb)| pa != pb)
        .count()
}

/// Loads a ROM from a file path, if it exists.
///
/// Returns `None` if the file doesn't exist (test ROMs may not be present
/// in CI or on all machines).
pub fn load_test_rom(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rom_produces_framebuffer() {
        let fb = run_rom_frames(vec![0; 1024], 1);
        assert_eq!(fb.len(), FRAME_RGBA_BYTES);
    }

    #[test]
    fn identical_framebuffers_have_zero_diff() {
        let fb = vec![0u8; FRAME_RGBA_BYTES];
        assert_eq!(compare_framebuffers(&fb, &fb), 0);
    }

    #[test]
    fn different_framebuffers_count_pixels() {
        let a = vec![0u8; FRAME_RGBA_BYTES];
        let mut b = vec![0u8; FRAME_RGBA_BYTES];
        // Change first pixel
        b[0] = 0xFF;
        assert_eq!(compare_framebuffers(&a, &b), 1);
    }

    #[test]
    fn rewind_reproduces_reference_run() {
        // End-to-end rewind exercise on a synthetic ROM: run forward, rewind,
        // replay, and confirm the reconstructed run matches a reference core
        // that never rewound.
        const F: u64 = 90;
        const R: u32 = 30;
        let rom = vec![0u8; 0x8000];

        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom.clone()));
        let mut reference = GenesisCore::new();
        reference.execute(Command::LoadRom(rom));

        for _ in 0..F {
            core.execute(Command::StepFrame);
            reference.execute(Command::StepFrame);
        }
        let reference_fb = reference.framebuffer_rgba().to_vec();

        // Rewind buffer should hold history and report non-zero memory.
        assert!(core.rewind_frames_available() > 0);
        assert!(core.rewind_memory_used() > 0);

        core.execute(Command::Rewind { frames: R });
        assert_eq!(core.frame_count(), F - u64::from(R));
        for _ in 0..R {
            core.execute(Command::StepFrame);
        }
        assert_eq!(core.frame_count(), F);

        assert_eq!(
            compare_framebuffers(core.framebuffer_rgba(), &reference_fb),
            0,
            "rewind+replay framebuffer differs from reference"
        );
    }

    #[test]
    fn step_back_walks_backward() {
        let rom = vec![0u8; 0x8000];
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom));
        for _ in 0..30 {
            core.execute(Command::StepFrame);
        }
        assert_eq!(core.frame_count(), 30);
        core.execute(Command::StepBack);
        assert_eq!(core.frame_count(), 29);
        core.execute(Command::StepBack);
        assert_eq!(core.frame_count(), 28);
    }
}

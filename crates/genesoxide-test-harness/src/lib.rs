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

use genesoxide_core::{Command, GenesisCore, Region};

/// Loads a ROM and runs it for the given number of frames.
/// Returns the final framebuffer as a Vec<u8>.
#[must_use]
pub fn run_rom_frames(rom_data: Vec<u8>, frames: u64) -> Vec<u8> {
    run_rom_frames_region(rom_data, frames, None)
}

/// Like [`run_rom_frames`], but forces a console region (`None` = auto-detect).
///
/// Used by the PAL/V30 golden scene, which must run at 313-line PAL timing with
/// V30 (240-line) mode enabled regardless of the synthetic ROM's header.
#[must_use]
pub fn run_rom_frames_region(rom_data: Vec<u8>, frames: u64, region: Option<Region>) -> Vec<u8> {
    let mut core = GenesisCore::new();
    core.execute(Command::SetRegionOverride(region));
    core.execute(Command::LoadRom(rom_data));

    for _ in 0..frames {
        core.execute(Command::StepFrame);
    }

    core.framebuffer_rgba().to_vec()
}

/// Compares two framebuffers pixel-by-pixel.
/// Returns the number of differing pixels.
///
/// Both framebuffers must be the same non-empty length that is a whole number of
/// 4-byte RGBA pixels. The length encodes BOTH the active width (256 in H32, 320
/// in H40) and the active height (224 in V28, 240 in PAL V30), so this compares
/// exactly for any width/height combination — the existing 320×224 goldens still
/// match exactly, while 256-wide H32 and 240-line PAL/V30 goldens are allowed.
#[must_use]
pub fn compare_framebuffers(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len(), "framebuffer lengths differ");
    assert!(
        !a.is_empty() && a.len() % 4 == 0,
        "framebuffer must be a non-empty multiple of 4"
    );

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

// ── Test coverage self-report ──────────────────────────────────────────────
//
// The honesty mechanism for the harness: a single place that inspects the
// environment at runtime and reports, for every suite, whether it is
// ALWAYS-RUN, ACTIVE (committed data present), OPT-IN (needs a fetched corpus),
// or IGNORED (needs a commercial ROM / reference data). A green harness run is
// then self-documenting — `coverage_summary` prints this and also *asserts* the
// invariants that must hold on any clean checkout, so a stripped-down tree fails
// loudly instead of quietly reporting "ok" with everything skipped.

use std::path::{Path, PathBuf};

/// Minimum number of committed m68k vendored opcode files a clean checkout MUST
/// contain. Fewer than this means the committed data was stripped — a hard fail.
/// The committed set is 33 opcodes (including the now-fixed BTST/BSET/LINK/DIVU);
/// this floor keeps a small margin so a partial strip still fails loudly.
pub const M68K_VENDORED_MIN: usize = 29;
/// Minimum number of committed z80 vendored opcode files a clean checkout MUST
/// contain.
pub const Z80_VENDORED_MIN: usize = 25;

/// Resolve a path relative to this crate's manifest directory
/// (`crates/genesoxide-test-harness`). Mirrors the path helpers in the suite
/// integration tests so the report and the tests agree on where data lives.
fn harness_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn count_files_ending(dir: &Path, suffix: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
                .count()
        })
        .unwrap_or(0)
}

/// Count of committed m68k vendored `.json.bin` opcode files.
#[must_use]
pub fn m68k_vendored_count() -> usize {
    count_files_ending(&harness_path("../../tests/m68000-tests/v1"), ".json.bin")
}

/// Count of committed z80 vendored `.json` opcode files.
#[must_use]
pub fn z80_vendored_count() -> usize {
    count_files_ending(
        &harness_path("../../tests/z80-tests-vendored/v1/v1"),
        ".json",
    )
}

/// Count of opt-in full-corpus m68k opcode files (gitignored, fetched on demand).
#[must_use]
pub fn m68k_full_corpus_count() -> usize {
    count_files_ending(
        &harness_path("../../tests/m68000-tests-full/v1"),
        ".json.bin",
    )
}

/// Count of opt-in full-corpus z80 opcode files (gitignored, fetched on demand).
#[must_use]
pub fn z80_full_corpus_count() -> usize {
    count_files_ending(&harness_path("../../tests/z80-tests/v1/v1"), ".json")
}

/// Count of committed video golden `.rgba` reference frames.
#[must_use]
pub fn video_golden_count() -> usize {
    count_files_ending(&harness_path("tests/goldens"), ".rgba")
}

/// Whether a commercial Sonic ROM is reachable via `GENESOXIDE_SONIC_ROM`
/// (used by the `sonic_boot` suite).
#[must_use]
pub fn sonic_rom_available() -> bool {
    std::env::var("GENESOXIDE_SONIC_ROM")
        .ok()
        .is_some_and(|p| Path::new(&p).exists())
}

/// Whether the (gitignored) GHZ audio reference directory holds any reference
/// recordings for the `audio_golden` diagnostics.
#[must_use]
pub fn audio_reference_available() -> bool {
    let dir = harness_path("tests/reference_audio");
    std::fs::read_dir(&dir)
        .map(|mut rd| rd.next().is_some())
        .unwrap_or(false)
}

/// Produce a multi-line, CI-log-readable report of every suite's coverage state.
///
/// Each line is tagged ALWAYS-RUN / ACTIVE / OPT-IN / IGNORED / MISSING so a
/// reader can tell at a glance what a green run actually exercised versus what
/// was gated or absent. This is intentionally dependency-light: it only reads
/// the filesystem and environment, it does not run any emulation.
#[must_use]
pub fn coverage_report() -> String {
    let mut out = String::new();

    let m68k_vendored = m68k_vendored_count();
    let z80_vendored = z80_vendored_count();
    let m68k_full = m68k_full_corpus_count();
    let z80_full = z80_full_corpus_count();
    let goldens = video_golden_count();
    let sonic = sonic_rom_available();
    let audio_ref = audio_reference_available();

    out.push_str("=== genesoxide test coverage ===\n");
    out.push_str("  legend: ALWAYS-RUN=committed, ACTIVE=vendored data present,\n");
    out.push_str("          OPT-IN=fetch full corpus, IGNORED=needs commercial ROM/refs,\n");
    out.push_str("          MISSING=committed data absent (hard fail)\n\n");

    // Aligned columns: area (36) | state (18) | detail.
    let mut row = |area: &str, state: &str, detail: &str| {
        out.push_str(&format!("  {area:<36}{state:<18}{detail}\n"));
    };

    row(
        "F1 SGDK boot proof",
        "ALWAYS-RUN",
        "committed ROM, no external data (f1_boot_proof)",
    );

    let m68k_state = if m68k_vendored >= M68K_VENDORED_MIN {
        "ACTIVE"
    } else {
        "MISSING"
    };
    row(
        "m68k vendored SST subset",
        m68k_state,
        &format!(
            "{m68k_vendored} .json.bin files (min {M68K_VENDORED_MIN}){}",
            if m68k_vendored >= M68K_VENDORED_MIN {
                " — runs by default"
            } else {
                " — HARD FAIL: committed data stripped"
            }
        ),
    );
    row(
        "m68k full corpus",
        if m68k_full > 0 {
            "OPT-IN(ready)"
        } else {
            "OPT-IN"
        },
        &format!(
            "{m68k_full} files; fetch: scripts/fetch-sst-corpus.sh --full then \
             --test m68k_suite full_suite -- --ignored"
        ),
    );

    let z80_state = if z80_vendored >= Z80_VENDORED_MIN {
        "ACTIVE"
    } else {
        "MISSING"
    };
    row(
        "z80 vendored SST subset",
        z80_state,
        &format!(
            "{z80_vendored} .json files (min {Z80_VENDORED_MIN}){}",
            if z80_vendored >= Z80_VENDORED_MIN {
                " — runs by default"
            } else {
                " — HARD FAIL: committed data stripped"
            }
        ),
    );
    row(
        "z80 full corpus",
        if z80_full > 0 {
            "OPT-IN(ready)"
        } else {
            "OPT-IN"
        },
        &format!(
            "{z80_full} files; fetch: scripts/fetch-sst-corpus.sh --full then \
             --test z80_suite full_suite -- --ignored"
        ),
    );

    row(
        "video goldens",
        "ALWAYS-RUN",
        &format!("{goldens} committed .rgba scenes (video_golden)"),
    );

    row(
        "sonic_boot (commercial)",
        if sonic { "ACTIVE" } else { "IGNORED-needs-ROM" },
        if sonic {
            "GENESOXIDE_SONIC_ROM set — run with -- --ignored"
        } else {
            "set GENESOXIDE_SONIC_ROM=/path/to/sonic.md; 11 #[ignore] tests"
        },
    );

    row(
        "audio_golden diagnostics",
        "IGNORED-needs-ROM",
        &format!(
            "~100 opt-in #[ignore] tuning diagnostics; needs local Sonic ROM + \
             reference audio (present: {})",
            if audio_ref { "yes" } else { "no" }
        ),
    );
    row(
        "vgm / ymfm diagnostics",
        "IGNORED-opt-in",
        "vgm_file_playback (needs tests/vgm_files/) + ymfm dump; 10 always-run VGM unit tests",
    );

    out.push('\n');
    out.push_str(&format!(
        "  summary: m68k_vendored={m68k_vendored} z80_vendored={z80_vendored} \
         goldens={goldens} sonic_rom={} audio_ref={}\n",
        if sonic { "yes" } else { "no" },
        if audio_ref { "yes" } else { "no" },
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use genesoxide_core::FRAME_RGBA_BYTES;

    #[test]
    fn empty_rom_produces_framebuffer() {
        let fb = run_rom_frames(vec![0; 1024], 1);
        // Native-width framebuffer: an all-zero ROM leaves reg 0x0C = 0, which
        // is H32 (256px), so the packed frame is 256x224x4. The buffer is always
        // a whole number of 224-row RGBA lines regardless of horizontal mode.
        assert!(!fb.is_empty());
        assert!(fb.len() == 256 * 224 * 4 || fb.len() == FRAME_RGBA_BYTES);
        assert_eq!(fb.len() % (224 * 4), 0);
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

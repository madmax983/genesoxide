//! ALWAYS-RUN coverage honesty guardrail.
//!
//! This binary has no external-data dependency and therefore runs on every
//! `cargo test -p genesoxide-test-harness` invocation. It does two things:
//!
//! 1. Prints [`coverage_report`] to stderr (visible with `--nocapture`) so a
//!    green run is self-documenting: the log spells out which suites actually
//!    ran versus which were gated/absent.
//! 2. Asserts the *honesty invariants* that MUST hold on any clean checkout —
//!    the committed vendored SingleStepTests subsets are present, and the
//!    F1 boot proof is compiled in. This is a second guardrail beyond the
//!    per-opcode panics in `m68k_suite` / `z80_suite`: if the committed data
//!    is stripped, this fails loudly instead of the suites quietly skipping.
//!
//! Run with: `cargo test -p genesoxide-test-harness --test coverage_summary -- --nocapture`

use genesoxide_test_harness::{
    M68K_VENDORED_MIN, Z80_VENDORED_MIN, coverage_report, m68k_vendored_count, video_golden_count,
    z80_vendored_count,
};

#[test]
fn coverage_report_is_honest_on_clean_checkout() {
    // Always print the report so CI logs document the run.
    let report = coverage_report();
    eprintln!("\n{report}");

    // Invariant 1: committed m68k vendored subset present.
    let m68k = m68k_vendored_count();
    assert!(
        m68k >= M68K_VENDORED_MIN,
        "committed m68k vendored SST subset missing or stripped: found {m68k} .json.bin files, \
         expected >= {M68K_VENDORED_MIN}. See tests/m68000-tests/README.md — a clean checkout \
         must include these; the m68k_suite would otherwise silently under-run."
    );

    // Invariant 2: committed z80 vendored subset present.
    let z80 = z80_vendored_count();
    assert!(
        z80 >= Z80_VENDORED_MIN,
        "committed z80 vendored SST subset missing or stripped: found {z80} .json files, \
         expected >= {Z80_VENDORED_MIN}. See tests/z80-tests-vendored/README.md."
    );

    // Invariant 3: the always-run F1 boot proof is compiled into this harness.
    // (Its test binary is built alongside this one; a missing f1_boot_proof.rs
    // would fail the whole crate build, so reaching here proves it is present.)
    let goldens = video_golden_count();
    assert!(
        goldens > 0,
        "expected committed video golden scenes, found none — the always-run video_golden \
         suite would have nothing to compare against."
    );
}

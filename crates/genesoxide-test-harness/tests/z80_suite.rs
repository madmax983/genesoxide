//! Integration tests for the jsmoo Z80 test vectors.
//!
//! Runs jsmoo-generated test vectors against our Z80 executor.
//! Each JSON file contains ~1000 cases per opcode.

use std::path::{Path, PathBuf};

use genesoxide_test_harness::z80_tests;

/// Returns the path to the z80-tests/v1/ directory.
fn test_data_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/genesoxide-test-harness
    // test data = ../../tests/z80-tests/v1/v1/
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../../tests/z80-tests/v1/v1")
}

/// Helper: run all test cases from a single .json file.
/// Panics with a summary if any tests fail.
fn run_opcode_tests(filename: &str) {
    let path = test_data_dir().join(filename);
    if !path.exists() {
        eprintln!("Skipping {filename}: file not found at {}", path.display());
        return;
    }

    let (passed, failed, failures) = z80_tests::run_test_file(&path, 10);
    let total = passed + failed;

    if failed > 0 {
        let mut msg = format!("\n{filename}: {failed}/{total} FAILED\n");
        for f in &failures {
            msg.push_str(&format!("\n  Test: {}\n", f.test_name));
            for m in &f.mismatches {
                msg.push_str(&format!(
                    "    {}: expected {}, got {}\n",
                    m.field, m.expected, m.actual
                ));
            }
        }
        if failed > failures.len() {
            msg.push_str(&format!(
                "\n  ... and {} more failures\n",
                failed - failures.len()
            ));
        }
        panic!("{msg}");
    }

    eprintln!("{filename}: {passed}/{total} passed");
}

// ── Smoke test — NOP (opcode 0x00) ────────────────────────────────────

#[test]
fn nop() {
    run_opcode_tests("00.json");
}

// ── Full suite runner ───────────────────────────────────────────────────

/// Runs every .json file in the test directory and reports a summary.
/// Use: cargo test -p genesoxide-test-harness --test z80_suite full_suite -- --nocapture
#[test]
#[ignore] // Run explicitly — takes a while with ~1.6M tests
fn full_suite() {
    let dir = test_data_dir();
    if !dir.exists() {
        eprintln!("Skipping Z80 full suite: directory not found at {}", dir.display());
        return;
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("Cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "json")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    let mut failed_opcodes = Vec::new();

    for entry in &entries {
        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        let path = entry.path();

        let (passed, failed, failures) = z80_tests::run_test_file(&path, 3);
        total_passed += passed;
        total_failed += failed;

        if failed > 0 {
            eprintln!("FAIL  {filename}: {failed}/{} failed", passed + failed);
            for f in &failures {
                eprintln!(
                    "      {} — {:?}",
                    f.test_name,
                    f.mismatches.iter().map(|m| &m.field).collect::<Vec<_>>()
                );
            }
            failed_opcodes.push(filename.to_string());
        } else {
            eprintln!("PASS  {filename}: {passed}/{passed} passed");
        }
    }

    eprintln!("\n════════════════════════════════════════════");
    eprintln!(
        "Total: {} passed, {} failed ({} opcode files tested)",
        total_passed,
        total_failed,
        entries.len()
    );
    if !failed_opcodes.is_empty() {
        eprintln!("Failed opcodes: {}", failed_opcodes.join(", "));
    }
    eprintln!("════════════════════════════════════════════");

    assert_eq!(total_failed, 0, "{total_failed} tests failed across the full suite");
}

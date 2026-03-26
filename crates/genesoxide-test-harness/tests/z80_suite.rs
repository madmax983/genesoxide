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

// ── 16-bit loads ──────────────────────────────────────────────────────

#[test]
fn ld_bc_nn() {
    run_opcode_tests("01.json");
}

// ── 8-bit immediate loads ─────────────────────────────────────────────

#[test]
fn ld_b_n() {
    run_opcode_tests("06.json");
}

#[test]
fn ld_a_n() {
    run_opcode_tests("3e.json");
}

// ── ADD HL,rr ─────────────────────────────────────────────────────────

#[test]
fn add_hl_bc() {
    run_opcode_tests("09.json");
}

// ── 8-bit register loads ──────────────────────────────────────────────

#[test]
fn ld_b_c() {
    run_opcode_tests("41.json");
}

// ── HALT ──────────────────────────────────────────────────────────────

#[test]
fn halt() {
    run_opcode_tests("76.json");
}

// ── ALU operations ────────────────────────────────────────────────────

#[test]
fn add_a_b() {
    run_opcode_tests("80.json");
}

#[test]
fn sub_b() {
    run_opcode_tests("90.json");
}

#[test]
fn and_b() {
    run_opcode_tests("a0.json");
}

#[test]
fn xor_b() {
    run_opcode_tests("a8.json");
}

#[test]
fn or_b() {
    run_opcode_tests("b0.json");
}

#[test]
fn cp_b() {
    run_opcode_tests("b8.json");
}

// ── Jumps ─────────────────────────────────────────────────────────────

#[test]
fn jp_nn() {
    run_opcode_tests("c3.json");
}

#[test]
fn jr_e() {
    run_opcode_tests("18.json");
}

// ── DJNZ ──────────────────────────────────────────────────────────────

#[test]
fn djnz() {
    run_opcode_tests("10.json");
}

// ── Calls & Returns ───────────────────────────────────────────────────

#[test]
fn call_nn() {
    run_opcode_tests("cd.json");
}

#[test]
fn ret() {
    run_opcode_tests("c9.json");
}

// ── Stack ─────────────────────────────────────────────────────────────

#[test]
fn push_bc() {
    run_opcode_tests("c5.json");
}

#[test]
fn pop_bc() {
    run_opcode_tests("c1.json");
}

// ── DAA ───────────────────────────────────────────────────────────────

#[test]
fn daa() {
    run_opcode_tests("27.json");
}

// ── CB prefix (bit operations) ──────────────────────────────────────────

#[test]
fn cb_rlc_b() {
    run_opcode_tests("cb 00.json");
}

#[test]
fn cb_rrc_b() {
    run_opcode_tests("cb 08.json");
}

#[test]
fn cb_rl_b() {
    run_opcode_tests("cb 10.json");
}

#[test]
fn cb_rr_b() {
    run_opcode_tests("cb 18.json");
}

#[test]
fn cb_sla_b() {
    run_opcode_tests("cb 20.json");
}

#[test]
fn cb_sra_b() {
    run_opcode_tests("cb 28.json");
}

#[test]
fn cb_sll_b() {
    run_opcode_tests("cb 30.json");
}

#[test]
fn cb_srl_b() {
    run_opcode_tests("cb 38.json");
}

#[test]
fn cb_rlc_hl() {
    run_opcode_tests("cb 06.json");
}

#[test]
fn cb_bit_0_b() {
    run_opcode_tests("cb 40.json");
}

#[test]
fn cb_bit_7_a() {
    run_opcode_tests("cb 7f.json");
}

#[test]
fn cb_bit_0_hl() {
    run_opcode_tests("cb 46.json");
}

#[test]
fn cb_res_0_b() {
    run_opcode_tests("cb 80.json");
}

#[test]
fn cb_res_0_hl() {
    run_opcode_tests("cb 86.json");
}

#[test]
fn cb_set_0_b() {
    run_opcode_tests("cb c0.json");
}

#[test]
fn cb_set_0_hl() {
    run_opcode_tests("cb c6.json");
}

// ── ED prefix (extended operations) ────────────────────────────────────

#[test]
fn ed_in_b_c() {
    run_opcode_tests("ed 40.json");
}

#[test]
fn ed_out_c_b() {
    run_opcode_tests("ed 41.json");
}

#[test]
fn ed_sbc_hl_bc() {
    run_opcode_tests("ed 42.json");
}

#[test]
fn ed_adc_hl_bc() {
    run_opcode_tests("ed 4a.json");
}

#[test]
fn ed_ld_nn_bc() {
    run_opcode_tests("ed 43.json");
}

#[test]
fn ed_ld_bc_nn() {
    run_opcode_tests("ed 4b.json");
}

#[test]
fn ed_neg() {
    run_opcode_tests("ed 44.json");
}

#[test]
fn ed_retn() {
    run_opcode_tests("ed 45.json");
}

#[test]
fn ed_reti() {
    run_opcode_tests("ed 4d.json");
}

#[test]
fn ed_im_0() {
    run_opcode_tests("ed 46.json");
}

#[test]
fn ed_im_1() {
    run_opcode_tests("ed 56.json");
}

#[test]
fn ed_im_2() {
    run_opcode_tests("ed 5e.json");
}

#[test]
fn ed_ld_i_a() {
    run_opcode_tests("ed 47.json");
}

#[test]
fn ed_ld_r_a() {
    run_opcode_tests("ed 4f.json");
}

#[test]
fn ed_ld_a_i() {
    run_opcode_tests("ed 57.json");
}

#[test]
fn ed_ld_a_r() {
    run_opcode_tests("ed 5f.json");
}

#[test]
fn ed_rrd() {
    run_opcode_tests("ed 67.json");
}

#[test]
fn ed_rld() {
    run_opcode_tests("ed 6f.json");
}

#[test]
fn ed_ldi() {
    run_opcode_tests("ed a0.json");
}

#[test]
fn ed_ldd() {
    run_opcode_tests("ed a8.json");
}

#[test]
fn ed_cpi() {
    run_opcode_tests("ed a1.json");
}

#[test]
fn ed_cpd() {
    run_opcode_tests("ed a9.json");
}

#[test]
fn ed_ldir() {
    run_opcode_tests("ed b0.json");
}

#[test]
fn ed_lddr() {
    run_opcode_tests("ed b8.json");
}

#[test]
fn ed_cpir() {
    run_opcode_tests("ed b1.json");
}

#[test]
fn ed_cpdr() {
    run_opcode_tests("ed b9.json");
}

#[test]
fn ed_ini() {
    run_opcode_tests("ed a2.json");
}

#[test]
fn ed_ind() {
    run_opcode_tests("ed aa.json");
}

#[test]
fn ed_outi() {
    run_opcode_tests("ed a3.json");
}

#[test]
fn ed_outd() {
    run_opcode_tests("ed ab.json");
}

#[test]
fn ed_inir() {
    run_opcode_tests("ed b2.json");
}

#[test]
fn ed_indr() {
    run_opcode_tests("ed ba.json");
}

#[test]
fn ed_otir() {
    run_opcode_tests("ed b3.json");
}

#[test]
fn ed_otdr() {
    run_opcode_tests("ed bb.json");
}

// ── DD prefix (IX operations) ──────────────────────────────────────────

#[test]
fn dd_ld_ix_nn() {
    run_opcode_tests("dd 21.json");
}

#[test]
fn dd_add_ix_bc() {
    run_opcode_tests("dd 09.json");
}

#[test]
fn dd_ld_b_ix_d() {
    run_opcode_tests("dd 46.json");
}

#[test]
fn dd_ld_ix_d_b() {
    run_opcode_tests("dd 70.json");
}

#[test]
fn dd_inc_ix_d() {
    run_opcode_tests("dd 34.json");
}

#[test]
fn dd_dec_ix_d() {
    run_opcode_tests("dd 35.json");
}

#[test]
fn dd_add_a_ix_d() {
    run_opcode_tests("dd 86.json");
}

#[test]
fn dd_pop_ix() {
    run_opcode_tests("dd e1.json");
}

#[test]
fn dd_push_ix() {
    run_opcode_tests("dd e5.json");
}

#[test]
fn dd_ld_b_ixh() {
    run_opcode_tests("dd 44.json");
}

#[test]
fn dd_ld_b_ixl() {
    run_opcode_tests("dd 45.json");
}

#[test]
fn dd_jp_ix() {
    run_opcode_tests("dd e9.json");
}

#[test]
fn dd_ld_sp_ix() {
    run_opcode_tests("dd f9.json");
}

// ── FD prefix (IY operations) ──────────────────────────────────────────

#[test]
fn fd_ld_iy_nn() {
    run_opcode_tests("fd 21.json");
}

#[test]
fn fd_add_iy_bc() {
    run_opcode_tests("fd 09.json");
}

#[test]
fn fd_ld_b_iy_d() {
    run_opcode_tests("fd 46.json");
}

#[test]
fn fd_push_iy() {
    run_opcode_tests("fd e5.json");
}

// ── DD CB prefix (indexed bit operations) ──────────────────────────────

#[test]
fn ddcb_rlc_ix_d() {
    run_opcode_tests("dd cb __ 06.json");
}

#[test]
fn ddcb_bit_0_ix_d() {
    run_opcode_tests("dd cb __ 46.json");
}

#[test]
fn ddcb_res_0_ix_d() {
    run_opcode_tests("dd cb __ 86.json");
}

#[test]
fn ddcb_set_0_ix_d() {
    run_opcode_tests("dd cb __ c6.json");
}

#[test]
fn ddcb_rlc_ix_d_b() {
    run_opcode_tests("dd cb __ 00.json");
}

// ── FD CB prefix (indexed bit operations) ──────────────────────────────

#[test]
fn fdcb_rlc_iy_d() {
    run_opcode_tests("fd cb __ 06.json");
}

#[test]
fn fdcb_bit_0_iy_d() {
    run_opcode_tests("fd cb __ 46.json");
}

#[test]
fn fdcb_res_0_iy_d() {
    run_opcode_tests("fd cb __ 86.json");
}

#[test]
fn fdcb_set_0_iy_d() {
    run_opcode_tests("fd cb __ c6.json");
}

// ── Full suite runner ───────────────────────────────────────────────────

/// Runs every .json file in the test directory and reports a summary.
/// Use: cargo test -p genesoxide-test-harness --test z80_suite full_suite -- --nocapture
#[test]
#[ignore] // Run explicitly — takes a while with ~1.6M tests
fn full_suite() {
    let dir = test_data_dir();
    if !dir.exists() {
        eprintln!(
            "Skipping Z80 full suite: directory not found at {}",
            dir.display()
        );
        return;
    }

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("Cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
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

    assert_eq!(
        total_failed, 0,
        "{total_failed} tests failed across the full suite"
    );
}

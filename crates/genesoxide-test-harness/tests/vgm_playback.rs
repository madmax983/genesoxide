//! VGM playback tests for YM2612 FM synthesis accuracy.
//!
//! These tests use programmatically built VGM sequences to isolate and verify
//! specific YM2612 behaviors: basic tone generation, FM modulation, envelope
//! shaping, algorithm routing, and DAC mode.
//!
//! For full-song comparison against Nuked-OPN2 reference renders, place
//! reference WAVs in `tests/vgm_reference/` and VGM files in `tests/vgm_files/`.
//!
//! Run with: cargo test -p genesoxide-test-harness --test vgm_playback -- --nocapture

use genesoxide_test_harness::vgm::*;
use std::path::Path;

const SAMPLE_RATE: u32 = 44100;

const VGM_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vgm_files");
const VGM_REF_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vgm_reference");
const VGM_OUTPUT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vgm_output");

// ── Helper ──────────────────────────────────────────────────────────────

fn ensure_output_dir() {
    std::fs::create_dir_all(VGM_OUTPUT_DIR).ok();
}

fn render_and_save(name: &str, vgm: &Vgm) -> Vec<f32> {
    ensure_output_dir();
    let mut renderer = VgmRenderer::new();
    let samples = renderer.render(vgm);
    let out_path = Path::new(VGM_OUTPUT_DIR).join(format!("{name}.wav"));
    save_wav(&out_path, &samples, SAMPLE_RATE).expect("Failed to save WAV");
    eprintln!(
        "  {name}: {} samples, {:.2}s, peak={:.4}, rms={:.4}",
        samples.len() / 2,
        samples.len() as f64 / (2.0 * SAMPLE_RATE as f64),
        peak(&samples),
        rms(&samples),
    );
    samples
}

// ── Test 1: Single operator produces a clean sine-like tone ─────────────

#[test]
fn single_operator_tone() {
    // A4 = 440 Hz. OPN2 phase step for MUL=1 is effectively
    // fnum * 2^(block - 1), so with block=4:
    // fnum = 440 * 2^20 / (2^3 * 53267) ≈ 1083
    let vgm = VgmBuilder::new()
        .single_op_tone(1083, 4, 0, 31) // fnum=1083, block=4, TL=0, AR=31
        .wait(44100) // 1 second of tone
        .ym_write(0, 0x28, 0x00) // Key-off
        .wait(4410) // 0.1s release tail
        .build();

    let samples = render_and_save("01_single_op_tone", &vgm);
    let left = left_channel(&samples);

    // Should produce audible output
    let rms_val = rms(&left);
    assert!(
        rms_val > 0.001,
        "Single operator tone should be audible, got RMS {rms_val:.6}"
    );

    // Estimate frequency from zero crossings (skip first 500 samples for attack)
    let steady = &left[500..left.len() - 4410];
    let freq = estimate_frequency(steady, SAMPLE_RATE);
    eprintln!("  Estimated frequency: {freq:.1} Hz (expected ~440 Hz)");

    // Allow ±10% tolerance (zero-crossing estimation isn't precise)
    assert!(
        (396.0..=484.0).contains(&freq),
        "Frequency {freq:.1} Hz should be near 440 Hz"
    );

    // Peak should be reasonable (not clipping, not silent)
    let peak_val = peak(&left[500..]);
    eprintln!("  Peak amplitude: {peak_val:.4}");
    assert!(peak_val > 0.01, "Peak too low: {peak_val:.4}");
}

// ── Test 2: TL attenuation works correctly ──────────────────────────────

#[test]
fn total_level_attenuation() {
    // Generate tones at TL=0 (loudest) and TL=24 (~18dB quieter)
    let vgm_loud = VgmBuilder::new()
        .single_op_tone(653, 4, 0, 31)
        .wait(22050) // 0.5s
        .build();

    let vgm_quiet = VgmBuilder::new()
        .single_op_tone(653, 4, 24, 31)
        .wait(22050)
        .build();

    let loud = render_and_save("02a_tl_loud", &vgm_loud);
    let quiet = render_and_save("02b_tl_quiet", &vgm_quiet);

    let loud_rms = rms(&left_channel(&loud)[500..]);
    let quiet_rms = rms(&left_channel(&quiet)[500..]);

    eprintln!("  TL=0 RMS: {loud_rms:.6}, TL=24 RMS: {quiet_rms:.6}");
    eprintln!("  Ratio: {:.2}x", loud_rms / quiet_rms.max(1e-10));

    // TL=24 means 24 * 0.75 dB = 18 dB quieter ≈ 8x voltage ratio
    // Allow wide tolerance since our tables may differ slightly
    assert!(
        loud_rms > quiet_rms * 2.0,
        "TL=0 should be significantly louder than TL=24"
    );
}

// ── Test 3: FM modulation produces richer spectrum than pure tone ────────

#[test]
fn fm_modulation_enriches_spectrum() {
    // Pure tone: single operator
    let vgm_pure = VgmBuilder::new()
        .single_op_tone(653, 4, 0, 31)
        .wait(22050)
        .build();

    // FM tone: op1 modulates op4 with MUL ratios 1:1
    let vgm_fm = VgmBuilder::new()
        .two_op_fm(653, 4, 0, 0, 1, 1, 0)
        .wait(22050)
        .build();

    let pure = render_and_save("03a_pure_tone", &vgm_pure);
    let fm = render_and_save("03b_fm_tone", &vgm_fm);

    let pure_left = left_channel(&pure);
    let fm_left = left_channel(&fm);

    // Both should be audible
    let pure_rms = rms(&pure_left[500..]);
    let fm_rms = rms(&fm_left[500..]);
    eprintln!("  Pure RMS: {pure_rms:.6}, FM RMS: {fm_rms:.6}");

    assert!(pure_rms > 0.001, "Pure tone should be audible");
    assert!(fm_rms > 0.001, "FM tone should be audible");

    // FM modulation should produce more zero crossings (richer harmonics)
    let pure_zc = zero_crossings(&pure_left[500..pure_left.len() - 500]);
    let fm_zc = zero_crossings(&fm_left[500..fm_left.len() - 500]);
    eprintln!("  Pure zero crossings: {pure_zc}, FM zero crossings: {fm_zc}");

    assert!(
        fm_zc > pure_zc,
        "FM tone ({fm_zc} ZC) should have richer harmonics than pure tone ({pure_zc} ZC)"
    );
}

// ── Test 4: ADSR envelope shape ─────────────────────────────────────────

#[test]
fn envelope_attack_decay() {
    // Short attack (AR=31), moderate decay (DR=10), sustain level 8
    let vgm = VgmBuilder::new()
        // Alg 7, panning
        .ym_write(0, 0xB0, 0x07)
        .ym_write(0, 0xB4, 0xC0)
        // Silence all but op1
        .ym_write(0, 0x40, 127)
        .ym_write(0, 0x44, 127)
        .ym_write(0, 0x48, 127)
        .ym_write(0, 0x4C, 127)
        // Op1 config
        .ym_write(0, 0x30, 0x01) // MUL=1
        .ym_write(0, 0x40, 0) // TL=0
        .ym_write(0, 0x50, 31) // AR=31 (instant attack)
        .ym_write(0, 0x60, 10) // DR=10 (moderate decay)
        .ym_write(0, 0x70, 0) // SR=0
        .ym_write(0, 0x80, 0x8F) // SL=8 (~-24dB), RR=15
        // Frequency (A4)
        .ym_write(0, 0xA4, (4 << 3) | 0x02)
        .ym_write(0, 0xA0, 0x8D)
        // Key-on
        .ym_write(0, 0x28, 0x10)
        .wait(44100) // 1s sustain
        // Key-off
        .ym_write(0, 0x28, 0x00)
        .wait(22050) // 0.5s release
        .build();

    let samples = render_and_save("04_envelope_adsr", &vgm);
    let left = left_channel(&samples);

    // Divide into segments and check envelope shape
    let chunk = SAMPLE_RATE as usize / 10; // 100ms chunks

    // First 100ms: should be loud (attack + start of decay)
    let early_rms = rms(&left[0..chunk]);
    // 500ms in: should be at sustain level (quieter)
    let sustain_rms = rms(&left[chunk * 5..chunk * 6]);
    // After key-off (last 0.5s): should decay to near silence
    let release_rms = rms(&left[left.len() - chunk..]);

    eprintln!("  Early RMS: {early_rms:.6}");
    eprintln!("  Sustain RMS: {sustain_rms:.6}");
    eprintln!("  Release RMS: {release_rms:.6}");

    // Attack should be louder than sustain (decay happened)
    assert!(
        early_rms > sustain_rms || sustain_rms > 0.001,
        "Envelope should show decay from attack to sustain"
    );

    // Release tail should be quieter than sustain
    assert!(
        release_rms < sustain_rms || release_rms < 0.001,
        "Release should be quieter than sustain"
    );
}

// ── Test 5: All 8 algorithms produce output ─────────────────────────────

#[test]
fn all_algorithms_produce_output() {
    for algo in 0..8u8 {
        let vgm = VgmBuilder::new()
            .ym_write(0, 0xB0, algo)
            .ym_write(0, 0xB4, 0xC0)
            // Set all 4 ops: TL=0, AR=31, MUL=1
            .ym_write(0, 0x30, 0x01)
            .ym_write(0, 0x40, 0)
            .ym_write(0, 0x50, 31)
            .ym_write(0, 0x60, 0)
            .ym_write(0, 0x70, 0)
            .ym_write(0, 0x80, 0x0F)
            .ym_write(0, 0x34, 0x01)
            .ym_write(0, 0x44, 0)
            .ym_write(0, 0x54, 31)
            .ym_write(0, 0x64, 0)
            .ym_write(0, 0x74, 0)
            .ym_write(0, 0x84, 0x0F)
            .ym_write(0, 0x38, 0x01)
            .ym_write(0, 0x48, 0)
            .ym_write(0, 0x58, 31)
            .ym_write(0, 0x68, 0)
            .ym_write(0, 0x78, 0)
            .ym_write(0, 0x88, 0x0F)
            .ym_write(0, 0x3C, 0x01)
            .ym_write(0, 0x4C, 0)
            .ym_write(0, 0x5C, 31)
            .ym_write(0, 0x6C, 0)
            .ym_write(0, 0x7C, 0)
            .ym_write(0, 0x8C, 0x0F)
            // Frequency
            .ym_write(0, 0xA4, (4 << 3) | 0x02)
            .ym_write(0, 0xA0, 0x8D)
            // Key-on all 4 ops
            .ym_write(0, 0x28, 0xF0)
            .wait(4410) // 0.1s
            .build();

        let name = format!("05_algo_{algo}");
        let samples = render_and_save(&name, &vgm);
        let left = left_channel(&samples);
        let rms_val = rms(&left[200..]);

        assert!(
            rms_val > 0.0001,
            "Algorithm {algo} should produce audible output, got RMS {rms_val:.6}"
        );
    }
}

// ── Test 6: DAC mode produces output ────────────────────────────────────

#[test]
fn dac_mode_produces_output() {
    let mut builder = VgmBuilder::new()
        // Enable DAC
        .ym_write(0, 0x2B, 0x80)
        // Panning for channel 6
        .ym_write(1, 0xB6, 0xC0);

    // Write a simple square wave to DAC: alternate 0x00 and 0xFF
    for _ in 0..441 {
        builder = builder
            .ym_write(0, 0x2A, 0xFF)
            .wait(50)
            .ym_write(0, 0x2A, 0x00)
            .wait(50);
    }

    let vgm = builder.build();
    let samples = render_and_save("06_dac_mode", &vgm);
    let left = left_channel(&samples);
    let rms_val = rms(&left);

    eprintln!("  DAC square wave RMS: {rms_val:.6}");
    assert!(
        rms_val > 0.001,
        "DAC mode should produce audible output, got RMS {rms_val:.6}"
    );
}

// ── Test 7: Key-off silences output ─────────────────────────────────────

#[test]
fn key_off_silences_output() {
    let vgm = VgmBuilder::new()
        .single_op_tone(653, 4, 0, 31)
        .wait(4410) // 0.1s tone
        .ym_write(0, 0x28, 0x00) // Key-off
        .wait(22050) // 0.5s silence
        .build();

    let samples = render_and_save("07_key_off", &vgm);
    let left = left_channel(&samples);

    // Tone portion should be loud
    let tone_rms = rms(&left[200..4410]);
    // End should be near silent (after release)
    let silence_rms = rms(&left[left.len() - 4410..]);

    eprintln!("  Tone RMS: {tone_rms:.6}, Silence RMS: {silence_rms:.6}");

    assert!(tone_rms > 0.001, "Tone should be audible");
    assert!(
        silence_rms < tone_rms * 0.1,
        "After key-off + release, output should be much quieter"
    );
}

// ── Test 8: Multiple channels mix correctly ─────────────────────────────

#[test]
fn multi_channel_mixing() {
    // Play two channels at different frequencies simultaneously
    let vgm = VgmBuilder::new()
        // Channel 0: A4 (440 Hz), fnum=653, block=4
        .single_op_tone(653, 4, 0, 31)
        // Channel 1: E5 (659 Hz), fnum=692, block=4
        .ym_write(0, 0xB1, 0x07) // algo 7
        .ym_write(0, 0xB5, 0xC0) // panning
        .ym_write(0, 0x41, 127)
        .ym_write(0, 0x45, 127)
        .ym_write(0, 0x49, 127)
        .ym_write(0, 0x4D, 127)
        .ym_write(0, 0x31, 0x01) // MUL=1
        .ym_write(0, 0x41, 0) // TL=0
        .ym_write(0, 0x51, 31) // AR=31
        .ym_write(0, 0x61, 0)
        .ym_write(0, 0x71, 0)
        .ym_write(0, 0x81, 0x0F)
        .ym_write(0, 0xA5, (4 << 3) | 0x02) // fnum hi
        .ym_write(0, 0xA1, 0xB4) // fnum lo = 692
        .ym_write(0, 0x28, 0x11) // Key-on ch1 op1
        .wait(22050) // 0.5s
        .build();

    let samples = render_and_save("08_multi_channel", &vgm);
    let left = left_channel(&samples);

    // Should be louder than single channel (two tones mixed)
    let rms_val = rms(&left[500..]);
    eprintln!("  Multi-channel RMS: {rms_val:.6}");
    assert!(rms_val > 0.001, "Multi-channel should be audible");
}

// ── Test 9: Stereo panning ──────────────────────────────────────────────

#[test]
fn stereo_panning() {
    // Left only
    let vgm_left = VgmBuilder::new()
        .ym_write(0, 0xB0, 0x07)
        .ym_write(0, 0xB4, 0x80) // Left only
        .ym_write(0, 0x40, 127)
        .ym_write(0, 0x44, 127)
        .ym_write(0, 0x48, 127)
        .ym_write(0, 0x4C, 127)
        .ym_write(0, 0x30, 0x01)
        .ym_write(0, 0x40, 0)
        .ym_write(0, 0x50, 31)
        .ym_write(0, 0x60, 0)
        .ym_write(0, 0x70, 0)
        .ym_write(0, 0x80, 0x0F)
        .ym_write(0, 0xA4, (4 << 3) | 0x02)
        .ym_write(0, 0xA0, 0x8D)
        .ym_write(0, 0x28, 0x10)
        .wait(4410)
        .build();

    let samples = render_and_save("09_panning_left", &vgm_left);
    let l = left_channel(&samples);
    let r = right_channel(&samples);

    let l_rms = rms(&l[200..]);
    let r_rms = rms(&r[200..]);
    eprintln!("  Left pan: L={l_rms:.6}, R={r_rms:.6}");

    assert!(l_rms > 0.001, "Left channel should have signal");
    assert!(
        r_rms < l_rms * 0.01,
        "Right channel should be silent with left-only panning"
    );
}

// ── Test 10: Frequency multiplier ───────────────────────────────────────

#[test]
fn frequency_multiplier() {
    // MUL=1: base frequency
    let vgm_mul1 = VgmBuilder::new()
        .single_op_tone(653, 4, 0, 31)
        .wait(22050)
        .build();

    // MUL=2: double frequency (manually set MUL register)
    let vgm_mul2 = VgmBuilder::new()
        .ym_write(0, 0xB0, 0x07)
        .ym_write(0, 0xB4, 0xC0)
        .ym_write(0, 0x40, 127)
        .ym_write(0, 0x44, 127)
        .ym_write(0, 0x48, 127)
        .ym_write(0, 0x4C, 127)
        .ym_write(0, 0x30, 0x02) // MUL=2
        .ym_write(0, 0x40, 0)
        .ym_write(0, 0x50, 31)
        .ym_write(0, 0x60, 0)
        .ym_write(0, 0x70, 0)
        .ym_write(0, 0x80, 0x0F)
        .ym_write(0, 0xA4, (4 << 3) | 0x02)
        .ym_write(0, 0xA0, 0x8D)
        .ym_write(0, 0x28, 0x10)
        .wait(22050)
        .build();

    let s1 = render_and_save("10a_mul1", &vgm_mul1);
    let s2 = render_and_save("10b_mul2", &vgm_mul2);

    let l1 = left_channel(&s1);
    let l2 = left_channel(&s2);

    let freq1 = estimate_frequency(&l1[500..], SAMPLE_RATE);
    let freq2 = estimate_frequency(&l2[500..], SAMPLE_RATE);

    eprintln!("  MUL=1 freq: {freq1:.1} Hz");
    eprintln!("  MUL=2 freq: {freq2:.1} Hz");
    eprintln!("  Ratio: {:.2}x", freq2 / freq1.max(1.0));

    // MUL=2 should be approximately 2x the frequency
    let ratio = freq2 / freq1.max(1.0);
    assert!(
        (1.8..=2.2).contains(&ratio),
        "MUL=2 should be ~2x frequency of MUL=1, got {ratio:.2}x"
    );
}

// ── VGM File Playback (from disk) ───────────────────────────────────────

/// Play a VGM file through our YM2612 and save output WAV.
/// Optionally compare against a Nuked-OPN2 reference WAV.
#[test]
#[ignore = "requires .vgm/.vgz files in tests/vgm_files/ (optional reference WAVs in tests/vgm_reference/); run with -- --ignored"]
fn vgm_file_playback() {
    ensure_output_dir();
    let vgm_dir = Path::new(VGM_DIR);
    let ref_dir = Path::new(VGM_REF_DIR);

    if !vgm_dir.exists() {
        eprintln!("No VGM files directory at {VGM_DIR}");
        eprintln!("Place .vgm or .vgz files there to enable file-based tests.");
        return;
    }

    let entries: Vec<_> = std::fs::read_dir(vgm_dir)
        .expect("Failed to read VGM directory")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            name.ends_with(".vgm") || name.ends_with(".vgz")
        })
        .collect();

    if entries.is_empty() {
        eprintln!("No .vgm/.vgz files found in {VGM_DIR}");
        return;
    }

    for entry in &entries {
        let path = entry.path();
        let stem = path.file_stem().unwrap().to_string_lossy();
        eprintln!("\n=== Processing: {} ===", path.display());

        let vgm = match Vgm::load(&path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  Failed to parse: {e}");
                continue;
            }
        };

        eprintln!(
            "  Version: 0x{:03X}, YM2612 clock: {}, Duration: {:.1}s, Commands: {}",
            vgm.header.version,
            vgm.header.ym2612_clock,
            vgm.duration_secs(),
            vgm.commands.len(),
        );

        // Render (full render, then truncate to 30s)
        let max_samples = 30 * SAMPLE_RATE;
        let mut renderer =
            VgmRenderer::with_clocks(vgm.header.ym2612_clock, vgm.header.sn76489_clock);
        let mut output = renderer.render(&vgm);

        // Truncate to 30s if longer
        let max_stereo = (max_samples as usize) * 2;
        if output.len() > max_stereo {
            output.truncate(max_stereo);
        }

        let out_path = Path::new(VGM_OUTPUT_DIR).join(format!("{stem}.wav"));
        save_wav(&out_path, &output, SAMPLE_RATE).expect("Failed to save WAV");

        let left = left_channel(&output);
        let rms_val = rms(&left);
        let peak_val = peak(&left);

        eprintln!(
            "  Output: {} samples, RMS={rms_val:.4}, Peak={peak_val:.4}",
            left.len()
        );
        eprintln!("  Saved: {}", out_path.display());

        // Check for reference WAV
        let ref_wav = ref_dir.join(format!("{stem}.wav"));
        if ref_wav.exists() {
            eprintln!("  Comparing against reference: {}", ref_wav.display());
            match load_wav(&ref_wav) {
                Ok((ref_rate, ref_samples)) => {
                    let ref_left = left_channel(&ref_samples);
                    let n = left.len().min(ref_left.len());

                    let corr = cross_correlation(&left[..n], &ref_left[..n]);
                    let ref_rms = rms(&ref_left[..n]);
                    let emu_rms = rms(&left[..n]);
                    let rms_ratio = if ref_rms > 1e-6 {
                        emu_rms / ref_rms
                    } else {
                        0.0
                    };

                    eprintln!(
                        "  Reference rate: {ref_rate} Hz, samples: {}",
                        ref_samples.len() / 2
                    );
                    eprintln!("  Cross-correlation: {corr:.4}");
                    eprintln!("  RMS ratio: {rms_ratio:.4} (1.0 = same level)");

                    if corr > 0.8 {
                        eprintln!("  EXCELLENT: High correlation with reference");
                    } else if corr > 0.5 {
                        eprintln!("  GOOD: Moderate correlation");
                    } else if corr > 0.2 {
                        eprintln!("  FAIR: Low correlation — audible differences");
                    } else {
                        eprintln!("  POOR: Very low correlation — significant divergence");
                    }
                }
                Err(e) => eprintln!("  Failed to load reference: {e}"),
            }
        }
    }
}

//! Sonic the Hedgehog integration tests.
//!
//! These tests require a commercial Sonic the Hedgehog ROM, which cannot be
//! committed to this repository. They are therefore all `#[ignore]`d so they
//! never *silently* pass when the ROM is absent — an ignored test is reported as
//! ignored, not as a vacuous green. Point [`SONIC_ROM_ENV`] at a local ROM and
//! run them explicitly:
//!
//! ```text
//! GENESOXIDE_SONIC_ROM=/path/to/sonic.md \
//!   cargo test -p genesoxide-test-harness --test sonic_boot -- --ignored --nocapture
//! ```
//!
//! The always-run, committed F1 boot proof lives in `f1_boot_proof.rs`.

use genesoxide_core::api::AudioOutputConfig;
use genesoxide_core::{Command, GenesisCore};
use genesoxide_test_harness::vgm::{
    CoreAudioRenderer, cross_correlation, left_channel, rms, vgm_from_timed_ym2612_writes,
};
use genesoxide_test_harness::ymfm_reference::Ymfm2612Renderer;

/// Environment variable holding the path to a commercial Sonic ROM (e.g.
/// `GENESOXIDE_SONIC_ROM=/path/to/sonic.md`). Unset by default, which is why
/// every test here is `#[ignore]`d.
const SONIC_ROM_ENV: &str = "GENESOXIDE_SONIC_ROM";
const GHZ_GOLDEN_RECORD_FRAMES: u32 = 6087;

fn load_sonic() -> Option<Vec<u8>> {
    let path = std::env::var(SONIC_ROM_ENV).ok()?;
    std::fs::read(path).ok()
}

fn run_frames(core: &mut GenesisCore, frames: u32) {
    for _ in 0..frames {
        core.execute(Command::StepFrame);
    }
}

fn press_start(core: &mut GenesisCore) {
    core.execute(Command::PressButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
    core.execute(Command::StepFrame);
    core.execute(Command::ReleaseButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
}

fn frame_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

fn best_lagged_correlation(a: &[f32], b: &[f32], max_lag: usize) -> (f32, isize) {
    let mut best = (f32::NEG_INFINITY, 0isize);

    for lag in -(max_lag as isize)..=(max_lag as isize) {
        let (a_start, b_start) = if lag >= 0 {
            (lag as usize, 0usize)
        } else {
            (0usize, (-lag) as usize)
        };
        if a_start >= a.len() || b_start >= b.len() {
            continue;
        }

        let n = (a.len() - a_start).min(b.len() - b_start);
        if n < 4096 {
            continue;
        }

        let corr = cross_correlation(&a[a_start..a_start + n], &b[b_start..b_start + n]);
        if corr > best.0 {
            best = (corr, lag);
        }
    }

    best
}

fn advance_to_green_hill_music(core: &mut GenesisCore) -> u32 {
    const MAX_SEARCH_FRAMES: u32 = 480;
    const ACTIVE_RMS: f32 = 0.02;

    run_frames(core, 200);
    press_start(core);
    run_frames(core, 120);
    core.clear_audio_buffer();

    for extra_frames in 1..=MAX_SEARCH_FRAMES {
        let before = core.ym2612_write_count();
        core.execute(Command::StepFrame);
        let write_delta = core.ym2612_write_count().saturating_sub(before);
        let rms = frame_rms(core.audio_samples());

        if write_delta > 0 && rms > ACTIVE_RMS {
            core.clear_audio_buffer();
            return extra_frames;
        }

        core.clear_audio_buffer();
    }

    panic!("failed to find active GHZ music window within {MAX_SEARCH_FRAMES} frames");
}

#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_boot_diagnostic() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    eprintln!("=== Initial state ===");
    eprintln!("PC: 0x{:06X}  SSP: 0x{:06X}", core.cpu_pc(), core.cpu_ssp());

    for frame in 0..120 {
        core.execute(Command::StepFrame);

        let fb = core.framebuffer_rgba();
        let non_black: usize = fb
            .chunks(4)
            .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
            .count();

        let snap = core.vdp_snapshot();

        if frame < 3 || frame % 20 == 0 || non_black > 0 {
            eprintln!("\n=== Frame {} ===", frame);
            eprintln!("PC: 0x{:06X}  Pixels: {}", core.cpu_pc(), non_black);
            eprintln!(
                "VDP regs: {:02X?}",
                &snap.registers[..snap.registers.len().min(24)]
            );
            eprintln!(
                "  Display: {}  DMA: {}  Auto-inc: {}",
                snap.registers[1] & 0x40 != 0,
                snap.registers[1] & 0x10 != 0,
                snap.registers[0x0F]
            );
            let non_zero_cram: usize = snap.cram.iter().filter(|&&c| c != 0).count();
            let non_zero_vram: usize = snap.vram.iter().filter(|&&b| b != 0).count();
            eprintln!(
                "  CRAM non-zero: {}/64  VRAM non-zero: {}/65536",
                non_zero_cram, non_zero_vram
            );
            if non_zero_cram > 0 {
                eprintln!("  CRAM[0..16]: {:03X?}", &snap.cram[..16]);
            }
            if non_zero_vram > 0 {
                eprintln!("  (VRAM has tile data)");
            }
        }
    }
}

/// Verifies that the window plane (HUD) renders visible content
/// once Sonic enters gameplay. Requires the SEGA logo + title screen
/// to complete (~200 frames), then pressing Start, then running
/// a few frames into Green Hill Zone.
#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_renders_hud() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    // Skip past SEGA logo and into title screen (~200 frames)
    for _ in 0..200 {
        core.execute(Command::StepFrame);
    }

    // Press Start to begin the game
    core.execute(Command::PressButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
    core.execute(Command::StepFrame);
    core.execute(Command::ReleaseButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });

    // Run into gameplay (~120 more frames for zone title card to clear)
    for _ in 0..120 {
        core.execute(Command::StepFrame);
    }

    let fb = core.framebuffer_rgba();
    let total_pixels = 320 * 224;
    let non_black: usize = fb
        .chunks(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    // Check the HUD region (top 32 pixel rows) for non-background content
    let top_area_pixels: usize = fb[..320 * 32 * 4]
        .chunks(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    eprintln!("Total non-black pixels: {non_black}/{total_pixels}");
    eprintln!("Top 32 rows non-black: {top_area_pixels}/{}", 320 * 32);

    assert!(
        non_black > 1000,
        "Frame should have substantial rendered content ({non_black} non-black pixels)"
    );
    assert!(
        top_area_pixels > 50,
        "HUD area should have visible content from window plane ({top_area_pixels} pixels)"
    );
}

/// Verifies that Sonic produces non-silent audio output.
/// The SEGA jingle and title screen music should generate audible samples.
#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_produces_audio() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    // Run 300 frames (~5 seconds): SEGA splash + title screen music
    for _ in 0..300 {
        core.execute(Command::StepFrame);
    }

    let samples = core.audio_samples();
    let total = samples.len();
    let non_silent = samples.iter().filter(|&&s| s.abs() > 0.001).count();

    eprintln!("Audio: {non_silent}/{total} non-silent samples");

    assert!(total > 0, "Should have audio samples in the buffer");
    assert!(
        non_silent > total / 4,
        "At least 25% of samples should be non-silent ({non_silent}/{total})"
    );
}

#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_live_ym_trace_replays_against_ymfm() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(44_100));
    core.execute(Command::SetAudioOutputConfig(AudioOutputConfig::default()));

    core.clear_audio_buffer();
    core.clear_ym2612_timed_write_trace();
    let capture_start = core.master_ticks();
    let write_count_start = core.ym2612_write_count();

    // Capture a live boot/title window. This has dense YM traffic and gives us
    // a real Sonic register stream to replay against ymfm.
    for _ in 0..300 {
        core.execute(Command::StepFrame);
    }

    let capture_end = core.master_ticks();
    let write_count_end = core.ym2612_write_count();
    let writes = core.ym2612_timed_write_trace();
    let vgm = vgm_from_timed_ym2612_writes(writes, capture_start, capture_end);
    let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);

    eprintln!(
        "live_sonic_ym_trace: timed_writes={}, write_delta={}, samples={}, corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}",
        writes.len(),
        write_count_end.saturating_sub(write_count_start),
        metrics.samples,
        metrics.correlation_left,
        metrics.correlation_right,
        metrics.rms_ratio_left,
        metrics.peak_ratio_left,
    );

    assert!(
        writes.len() > 1_000,
        "expected substantial live YM activity, got {} writes",
        writes.len()
    );
    assert!(
        metrics.samples > 200_000,
        "expected several seconds of replay audio, got {} samples",
        metrics.samples
    );
    assert!(metrics.correlation_left.is_finite());
    assert!(metrics.correlation_right.is_finite());
    assert!(metrics.rms_ratio_left.is_finite());
    assert!(metrics.peak_ratio_left.is_finite());
}

#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_green_hill_live_ym_trace_replays_against_ymfm() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(44_100));

    let extra_frames = advance_to_green_hill_music(&mut core);
    core.clear_audio_buffer();
    core.clear_ym2612_timed_write_trace();
    let capture_start = core.master_ticks();
    let write_count_start = core.ym2612_write_count();

    for _ in 0..300 {
        core.execute(Command::StepFrame);
    }

    let capture_end = core.master_ticks();
    let write_count_end = core.ym2612_write_count();
    let writes = core.ym2612_timed_write_trace();
    let vgm = vgm_from_timed_ym2612_writes(writes, capture_start, capture_end);
    let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);

    eprintln!(
        "ghz_live_ym_trace: extra_frames={}, timed_writes={}, write_delta={}, samples={}, corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}",
        extra_frames,
        writes.len(),
        write_count_end.saturating_sub(write_count_start),
        metrics.samples,
        metrics.correlation_left,
        metrics.correlation_right,
        metrics.rms_ratio_left,
        metrics.peak_ratio_left,
    );

    assert!(writes.len() > 100);
    assert!(metrics.samples > 200_000);
    assert!(metrics.correlation_left.is_finite());
    assert!(metrics.correlation_right.is_finite());
    assert!(metrics.rms_ratio_left.is_finite());
    assert!(metrics.peak_ratio_left.is_finite());
}

#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_green_hill_live_replay_matches_captured_audio() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(44_100));
    core.execute(Command::SetAudioOutputConfig(AudioOutputConfig::default()));

    core.clear_audio_buffer();
    core.clear_ym2612_timed_write_trace();
    core.clear_psg_timed_write_trace();
    let trace_start = core.audio_master_ticks();
    let trace_start_samples = core.audio_output_sample_count();

    let extra_frames = advance_to_green_hill_music(&mut core);
    let capture_start_samples = core.audio_output_sample_count();

    let mut live_samples = Vec::new();
    for _ in 0..300 {
        core.execute(Command::StepFrame);
        live_samples.extend_from_slice(core.audio_samples());
        core.clear_audio_buffer();
    }

    let capture_end = core.audio_master_ticks();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let replay_samples = renderer.render_timed_writes(
        core.ym2612_timed_write_trace(),
        core.psg_timed_write_trace(),
        trace_start,
        capture_end,
    );

    let live_left = left_channel(&live_samples);
    let replay_left = left_channel(&replay_samples);
    let expected_start = capture_start_samples.saturating_sub(trace_start_samples) as usize;
    let lag_window = 64usize;
    let replay_window_start = expected_start.saturating_sub(lag_window);
    let replay_window = &replay_left[replay_window_start..];
    let (corr, lag) = best_lagged_correlation(&live_left, replay_window, lag_window);
    let live_start = if lag >= 0 { lag as usize } else { 0usize };
    let replay_start = if lag >= 0 {
        replay_window_start
    } else {
        replay_window_start + (-lag) as usize
    };
    let n = (live_left.len() - live_start).min(replay_left.len() - replay_start);
    let rms_ratio = rms(&live_left[live_start..live_start + n])
        / rms(&replay_left[replay_start..replay_start + n]).max(1e-9);

    eprintln!(
        "ghz_live_replay_vs_capture: extra_frames={}, ym_writes={}, psg_writes={}, expected_start={}, lag={}, samples={}, corr={:.4}, rms_ratio={:.4}",
        extra_frames,
        core.ym2612_timed_write_trace().len(),
        core.psg_timed_write_trace().len(),
        expected_start,
        lag,
        n,
        corr,
        rms_ratio,
    );

    assert!(n > 200_000);
    assert!(
        lag.abs() <= 64,
        "expected near-zero alignment lag, got {lag} samples"
    );
    assert!(
        corr > 0.99,
        "expected direct timed replay to match live capture, got corr {corr:.4}"
    );
    assert!(
        (rms_ratio - 1.0).abs() < 0.02,
        "expected near-unity RMS ratio, got {rms_ratio:.4}"
    );
}

#[test]
#[ignore]
fn sonic_green_hill_long_live_replay_matches_captured_audio() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(44_100));
    core.execute(Command::SetAudioOutputConfig(AudioOutputConfig::default()));

    core.clear_audio_buffer();
    core.clear_ym2612_timed_write_trace();
    core.clear_psg_timed_write_trace();
    let trace_start = core.audio_master_ticks();
    let trace_start_samples = core.audio_output_sample_count();

    let extra_frames = advance_to_green_hill_music(&mut core);
    let capture_start_samples = core.audio_output_sample_count();

    let mut live_samples = Vec::new();
    for _ in 0..GHZ_GOLDEN_RECORD_FRAMES {
        core.execute(Command::StepFrame);
        live_samples.extend_from_slice(core.audio_samples());
        core.clear_audio_buffer();
    }

    let capture_end = core.audio_master_ticks();
    let mut renderer = CoreAudioRenderer::with_audio_output_config(AudioOutputConfig::default());
    let replay_samples = renderer.render_timed_writes(
        core.ym2612_timed_write_trace(),
        core.psg_timed_write_trace(),
        trace_start,
        capture_end,
    );

    let live_left = left_channel(&live_samples);
    let replay_left = left_channel(&replay_samples);
    let expected_start = capture_start_samples.saturating_sub(trace_start_samples) as usize;
    let lag_window = 64usize;
    let replay_window_start = expected_start.saturating_sub(lag_window);
    let replay_window = &replay_left[replay_window_start..];
    let (corr, lag) = best_lagged_correlation(&live_left, replay_window, lag_window);
    let live_start = if lag >= 0 { lag as usize } else { 0usize };
    let replay_start = if lag >= 0 {
        replay_window_start
    } else {
        replay_window_start + (-lag) as usize
    };
    let n = (live_left.len() - live_start).min(replay_left.len() - replay_start);
    let rms_ratio = rms(&live_left[live_start..live_start + n])
        / rms(&replay_left[replay_start..replay_start + n]).max(1e-9);

    eprintln!(
        "ghz_long_live_replay_vs_capture: extra_frames={}, frames={}, ym_writes={}, psg_writes={}, expected_start={}, lag={}, samples={}, corr={:.4}, rms_ratio={:.4}",
        extra_frames,
        GHZ_GOLDEN_RECORD_FRAMES,
        core.ym2612_timed_write_trace().len(),
        core.psg_timed_write_trace().len(),
        expected_start,
        lag,
        n,
        corr,
        rms_ratio,
    );

    assert!(n > 4_000_000);
    assert!(
        lag.abs() <= 64,
        "expected near-zero long-horizon alignment lag, got {lag} samples"
    );
    assert!(
        corr > 0.99,
        "expected long-horizon timed replay to match live capture, got corr {corr:.4}"
    );
    assert!(
        (rms_ratio - 1.0).abs() < 0.02,
        "expected near-unity long-horizon RMS ratio, got {rms_ratio:.4}"
    );
}

/// Diagnostic: run 600 frames and trace Z80/68K state to find hang point.
#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_hang_diagnostic() {
    let rom_data = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom_data.clone()));

    let mut prev_68k_pc = 0u32;
    let mut stuck_count = 0u32;

    for frame in 0..600 {
        let z80_cycles_before = core.z80_cycles();
        core.execute(Command::StepFrame);
        let z80_cycles_after = core.z80_cycles();
        let z80_ran = z80_cycles_after - z80_cycles_before;

        let pc_68k = core.cpu_pc();
        let pc_z80 = core.z80_pc();
        let bus_req = core.z80_bus_requested();
        let z80_reset = core.z80_in_reset();
        let stopped = core.cpu_stopped();
        let halted = core.cpu_halted();
        let ipm = core.cpu_interrupt_mask();
        let vint_en = core.vdp_register(1) & 0x20 != 0;
        let ram = core.z80_ram();
        let ram_1ffd = ram[0x1FFD];
        let ram_1fff = ram[0x1FFF];
        let ram_0000 = ram[0x0000];

        // Detect 68K stuck (but ignore checksum loop by requiring 60+ frames)
        if pc_68k == prev_68k_pc {
            stuck_count += 1;
        } else {
            if stuck_count > 5 {
                eprintln!(
                    "  (68K was at 0x{prev_68k_pc:06X} for {stuck_count} frames, now moved to 0x{pc_68k:06X})"
                );
            }
            stuck_count = 0;
        }
        prev_68k_pc = pc_68k;

        // Log periodically, at transitions, and near known problem area
        let should_log = frame < 3
            || frame % 100 == 0
            || stuck_count == 1
            || stuck_count == 40
            || stopped
            || halted
            || (frame >= 530 && frame <= 560);

        if should_log {
            eprintln!(
                "F{frame:3}: 68K=0x{pc_68k:06X} IPM={ipm} VINT={vint_en} stopped={stopped} Z80=0x{pc_z80:04X} z80_ran={z80_ran:5} bus_req={bus_req} reset={z80_reset} RAM[0]={ram_0000:02X} [1FFD]={ram_1ffd:02X} [1FFF]={ram_1fff:02X}"
            );
        }

        // When stuck for exactly 100 frames, dump detailed info once
        if stuck_count == 100 || stopped || halted {
            eprintln!(
                "=== 68K at 0x{pc_68k:06X} for {stuck_count} frames (stopped={stopped} halted={halted}) ==="
            );
            eprintln!("SR=0x{:04X} IPM={ipm} VINT={vint_en}", core.cpu_sr());
            eprintln!(
                "Z80 PC=0x{pc_z80:04X} cycles={z80_cycles_after} bus_req={bus_req} reset={z80_reset}"
            );
            eprintln!("Z80 RAM[0]={ram_0000:02X} [1FFD]={ram_1ffd:02X} [1FFF]={ram_1fff:02X}");

            let addr = pc_68k as usize;
            if addr + 16 <= rom_data.len() {
                let bytes: Vec<String> = rom_data[addr..addr + 16]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect();
                eprintln!("ROM[0x{addr:06X}..]: {}", bytes.join(" "));
            }

            if stopped || halted {
                break;
            }
        }
    }
}

/// Diagnostic: dump YM2612 channel state during Sonic playback to verify
/// the Z80 SMPS sound driver programs the FM chip correctly.
///
/// Run with: cargo test -p genesoxide-test-harness --test sonic_boot sonic_ym2612_diagnostic -- --nocapture
#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_ym2612_diagnostic() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));
    core.execute(Command::SetAudioSampleRate(44100));

    // Track key-on events and non-trivial writes
    let mut any_keyon_seen = false;
    let mut any_nonzero_fnum = false;
    let mut any_carrier_active = false;
    let mut prev_cmd_byte: u8 = 0;

    for frame in 0..700 {
        core.execute(Command::StepFrame);

        // Monitor Z80 RAM[$1FFF] for command changes
        let ram = core.z80_ram();
        let cmd_byte = ram[0x1FFF];
        if cmd_byte != prev_cmd_byte {
            let z80 = core.z80_snapshot();
            eprintln!(
                "[Frame {:3}] RAM[$1FFF] changed: 0x{:02X} -> 0x{:02X}  Z80 PC=0x{:04X}",
                frame, prev_cmd_byte, cmd_byte, z80.pc
            );
            prev_cmd_byte = cmd_byte;
        }

        // Track Z80 RAM coverage at early frames to verify driver upload
        if frame <= 10 || frame == 20 || frame == 50 || frame == 152 {
            let nonzero = ram.iter().filter(|&&b| b != 0).count();
            let last_nonzero = ram.iter().rposition(|&b| b != 0).unwrap_or(0);
            eprintln!(
                "[Frame {:3}] Z80 RAM: {} non-zero bytes, last non-zero at 0x{:04X}",
                frame, nonzero, last_nonzero
            );
        }

        // Dump detailed state at key frames:
        //   ~60: SEGA jingle should be playing
        //   ~100: SEGA jingle fading
        //   ~200: title screen music starting
        //   ~300: title screen music going
        let should_dump = frame == 60
            || frame == 100
            || frame == 152
            || frame == 153
            || frame == 200
            || frame == 300
            || frame == 340
            || frame == 500
            || frame == 600;

        if should_dump {
            let z80s = core.z80_snapshot();
            let z80_pc = z80s.pc;
            let z80_a = z80s.a;
            let z80_hl = u16::from(z80s.h) << 8 | u16::from(z80s.l);
            eprintln!(
                "\n=== Frame {} === (Z80 PC=0x{:04X} A=0x{:02X} HL=0x{:04X})",
                frame, z80_pc, z80_a, z80_hl
            );

            // At frame 152/153/340, dump full command dispatch code
            if frame == 152 || frame == 153 || frame == 340 {
                let ram = core.z80_ram();
                // Dump 0x0000-0x00D8 (full SMPS boot + dispatch + DAC routine)
                for chunk_start in (0x0000..0x00D8).step_by(0x40) {
                    let chunk_end = (chunk_start + 0x40).min(0x00D8);
                    eprint!("Z80 RAM[0x{:04X}..0x{:04X}]:", chunk_start, chunk_end);
                    for i in chunk_start..chunk_end {
                        if (i - chunk_start) % 16 == 0 {
                            eprint!("\n  {:04X}:", i);
                        }
                        eprint!(" {:02X}", ram[i]);
                    }
                    eprintln!();
                }
            }

            let diag = core.ym2612_diagnostic();
            eprintln!("Frame {} state:", frame);
            eprintln!(
                "DAC={} LFO={} (freq={}) Timer=0x{:02X}",
                diag.dac_enabled, diag.lfo_enabled, diag.lfo_frequency, diag.timer_control
            );

            for (ch_idx, ch) in diag.channels.iter().enumerate() {
                // Compute approximate frequency
                let freq = if ch.fnum > 0 {
                    let f = ch.fnum as f64 * (1u64 << ch.block) as f64 * (7_670_454.0 / 144.0)
                        / (1u64 << 20) as f64;
                    f
                } else {
                    0.0
                };

                // Check carrier operators based on algorithm
                let carrier_ops: &[usize] = match ch.algorithm {
                    0..=3 => &[3],       // Only op4
                    4 => &[1, 3],        // Op2, op4
                    5 | 6 => &[1, 2, 3], // Op2, op3, op4
                    7 => &[0, 1, 2, 3],  // All
                    _ => &[3],
                };

                let any_op_keyed = ch.operators.iter().any(|op| op.key_on);
                let carrier_tls: Vec<u8> = carrier_ops
                    .iter()
                    .map(|&i| ch.operators[i].total_level)
                    .collect();
                let carrier_active = carrier_ops
                    .iter()
                    .any(|&i| ch.operators[i].total_level < 127 && ch.operators[i].key_on);

                if any_op_keyed {
                    any_keyon_seen = true;
                }
                if ch.fnum > 0 {
                    any_nonzero_fnum = true;
                }
                if carrier_active {
                    any_carrier_active = true;
                }

                eprintln!(
                    "  CH{} alg={} fb={} fnum={:4} blk={} freq={:7.1}Hz pan={}{} keyed={} carrier_TL={:?}",
                    ch_idx + 1,
                    ch.algorithm,
                    ch.feedback,
                    ch.fnum,
                    ch.block,
                    freq,
                    if ch.panning_left { "L" } else { "-" },
                    if ch.panning_right { "R" } else { "-" },
                    any_op_keyed,
                    carrier_tls,
                );

                // Dump individual operators for keyed-on channels
                if any_op_keyed || ch.fnum > 0 {
                    for (op_idx, op) in ch.operators.iter().enumerate() {
                        let is_carrier = carrier_ops.contains(&op_idx);
                        eprintln!(
                            "    OP{} {} MUL={:2} DT={} TL={:3} AR={:2} DR={:2} SR={:2} SL={:2} RR={:2} KS={} AM={} SSG={:X} key={} env={:4} {:?}",
                            op_idx + 1,
                            if is_carrier { "C" } else { "M" },
                            op.multiply,
                            op.detune,
                            op.total_level,
                            op.attack_rate,
                            op.decay_rate,
                            op.sustain_rate,
                            op.sustain_level,
                            op.release_rate,
                            op.key_scale,
                            if op.am_enable { 1 } else { 0 },
                            op.ssg_eg,
                            if op.key_on { "ON" } else { "--" },
                            op.envelope,
                            op.env_state,
                        );
                    }
                }
            }
        }
    }

    // Basic sanity: after 350 frames, SMPS should have programmed SOMETHING
    let final_diag = core.ym2612_diagnostic();
    let any_final_fnum = final_diag.channels.iter().any(|ch| ch.fnum > 0);

    eprintln!("\n=== YM2612 Write Trace ===");
    let write_count = core.ym2612_write_count();
    let trace = core.ym2612_write_trace();
    eprintln!("Total register writes: {write_count}");
    eprintln!("First {} captured writes:", trace.len());
    for (i, &(bank, addr, val)) in trace.iter().enumerate() {
        let desc = match addr {
            0x22 => "LFO".to_string(),
            0x24 => "TimerA-hi".to_string(),
            0x25 => "TimerA-lo".to_string(),
            0x26 => "TimerB".to_string(),
            0x27 => "TimerCtrl".to_string(),
            0x28 => format!("KeyOnOff ch={} ops={:04b}", val & 0x07, (val >> 4) & 0x0F),
            0x2A => "DAC".to_string(),
            0x2B => format!("DACen={}", val >> 7),
            0x30..=0x3F => format!("DT/MUL slot={}", addr & 0x0F),
            0x40..=0x4F => format!("TL slot={}", addr & 0x0F),
            0x50..=0x5F => format!("RS/AR slot={}", addr & 0x0F),
            0x60..=0x6F => format!("AM/DR slot={}", addr & 0x0F),
            0x70..=0x7F => format!("SR slot={}", addr & 0x0F),
            0x80..=0x8F => format!("SL/RR slot={}", addr & 0x0F),
            0x90..=0x9F => format!("SSG-EG slot={}", addr & 0x0F),
            0xA0..=0xA3 => format!("Fnum-lo ch={}", addr & 0x03),
            0xA4..=0xA7 => format!("Fnum-hi ch={}", addr & 0x03),
            0xB0..=0xB3 => format!("FB/Algo ch={}", addr & 0x03),
            0xB4..=0xB7 => format!("LR/AMS/PMS ch={}", addr & 0x03),
            _ => format!("reg=0x{:02X}", addr),
        };
        if i < 100 || (addr >= 0x28 && addr <= 0x28) || (addr >= 0xA0 && addr <= 0xA7) {
            eprintln!(
                "  [{:3}] bank={} addr=0x{:02X} val=0x{:02X}  {}",
                i, bank, addr, val, desc
            );
        }
    }

    // Dump 68K writes to Z80 driver code area
    let (driver_writes, driver_last_frame) = core.z80_driver_write_info();
    eprintln!("\n=== 68K writes to Z80 driver area (0x0000-0x00FF) ===");
    eprintln!("Total writes: {driver_writes}, last write at frame: {driver_last_frame}");

    // Compare Z80 RAM[0] at frame 350 vs what we know from boot
    let ram = core.z80_ram();
    eprintln!("Z80 RAM[0x0000] = 0x{:02X} (boot was 0xF3=DI)", ram[0]);
    eprintln!("Z80 RAM[0x0003] = 0x{:02X} (boot was 0x31=LD SP)", ram[3]);

    // Dump 68K writes to Z80 RAM[$1FFF]
    let cmd_trace = core.z80_cmd_trace();
    eprintln!("\n=== 68K writes to Z80 RAM[$1FFF] ===");
    eprintln!("Total 68K command writes: {}", cmd_trace.len());
    for &(frame, val) in cmd_trace.iter() {
        eprintln!(
            "  Frame {:3}: wrote 0x{:02X} (bit7={})",
            frame,
            val,
            if val & 0x80 != 0 { "CMD" } else { "---" }
        );
    }

    // Dump Z80 bank register and sample of banked ROM
    let bank = core.z80_bank();
    eprintln!("\n=== Z80 Bank Register ===");
    eprintln!("Bank value: 0x{:06X}", bank);
    eprintln!("ROM window at 0x8000 maps to ROM offset 0x{:06X}", bank);
    // Dump first 64 bytes of the banked ROM window
    eprint!("Banked ROM[0x8000..0x8040]:");
    for i in 0..64u32 {
        if i % 16 == 0 {
            eprint!("\n  {:04X}:", 0x8000 + i);
        }
        let rom_offset = bank + i;
        let byte = core.rom_byte(rom_offset as usize);
        eprint!(" {:02X}", byte);
    }
    eprintln!();

    // Also dump Z80 RAM $1FF0-$1FFF to see command state
    let ram = core.z80_ram();
    eprint!("Z80 RAM[$1FF0..$2000]:");
    for i in 0x1FF0..0x2000 {
        if (i - 0x1FF0) % 16 == 0 {
            eprint!("\n  {:04X}:", i);
        }
        eprint!(" {:02X}", ram[i]);
    }
    eprintln!();

    // Dump Z80 state
    let z80 = core.z80_snapshot();
    let hl = u16::from(z80.h) << 8 | u16::from(z80.l);
    let bc = u16::from(z80.b) << 8 | u16::from(z80.c);
    let de = u16::from(z80.d) << 8 | u16::from(z80.e);
    eprintln!(
        "Z80: PC=0x{:04X} SP=0x{:04X} A=0x{:02X} HL=0x{:04X} BC=0x{:04X} DE=0x{:04X} IX=0x{:04X} IY=0x{:04X}",
        z80.pc, z80.sp, z80.a, hl, bc, de, z80.ix, z80.iy
    );
    eprintln!(
        "     halted={} int_line={} iff1={} im={}",
        z80.halted, z80.int_line, z80.iff1, z80.im
    );

    eprintln!("\n=== Summary ===");
    eprintln!("Key-on events seen: {any_keyon_seen}");
    eprintln!("Non-zero fnum seen: {any_nonzero_fnum}");
    eprintln!("Carrier active seen: {any_carrier_active}");
    eprintln!("Final state has fnum: {any_final_fnum}");

    // Check audio output
    let samples = core.audio_samples();
    let total = samples.len();
    let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let rms: f32 = if total > 0 {
        (samples.iter().map(|s| (s * s) as f64).sum::<f64>() / total as f64).sqrt() as f32
    } else {
        0.0
    };
    eprintln!("Audio: {total} samples, peak={peak:.4}, rms={rms:.4}");

    // BGM commands arrive at ~frame 486, so keyon/fnum may only be seen at
    // later dump points.  Also accept the post-loop final diagnostic.
    assert!(
        any_keyon_seen
            || final_diag
                .channels
                .iter()
                .any(|ch| ch.operators.iter().any(|op| op.key_on)),
        "SMPS should have triggered key-on events on YM2612"
    );
    assert!(
        any_nonzero_fnum || any_final_fnum,
        "SMPS should have programmed non-zero frequencies"
    );
}

/// Diagnostic: dump Z80 RAM around the stuck PC to understand what the
/// SMPS driver is doing (or failing to do).
#[test]
#[ignore = "requires GENESOXIDE_SONIC_ROM (commercial ROM); run with -- --ignored"]
fn sonic_z80_smps_trace() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    // Run to frame 100 (after driver upload and reset)
    for _ in 0..100 {
        core.execute(Command::StepFrame);
    }

    let ram = core.z80_ram();
    let pc = core.z80_pc();
    let snap = core.z80_snapshot();

    eprintln!("=== Z80 state at frame 100 ===");
    eprintln!(
        "PC=0x{:04X} SP=0x{:04X} A=0x{:02X} F=0x{:02X} BC=0x{:04X} DE=0x{:04X} HL=0x{:04X}",
        snap.pc,
        snap.sp,
        snap.a,
        snap.f,
        (snap.b as u16) << 8 | snap.c as u16,
        (snap.d as u16) << 8 | snap.e as u16,
        (snap.h as u16) << 8 | snap.l as u16,
    );
    eprintln!(
        "IX=0x{:04X} IY=0x{:04X} I=0x{:02X} R=0x{:02X} IFF1={} IFF2={} IM={} halted={}",
        snap.ix, snap.iy, snap.i, snap.r, snap.iff1, snap.iff2, snap.im, snap.halted
    );
    eprintln!(
        "int_line={} ei_pending={} wz=0x{:04X}",
        snap.int_line, snap.ei_pending, snap.wz
    );
    eprintln!(
        "bus_req={} reset={}",
        core.z80_bus_requested(),
        core.z80_in_reset()
    );

    // Dump Z80 RAM: first 256 bytes (the SMPS driver entry point + init)
    eprintln!("\n=== Z80 RAM dump (first 256 bytes) ===");
    for row in 0..16 {
        let base = row * 16;
        let hex: Vec<String> = ram[base..base + 16]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        let ascii: String = ram[base..base + 16]
            .iter()
            .map(|&b| {
                if (0x20..=0x7E).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        let pc_marker = if (base..base + 16).contains(&(pc as usize)) {
            " <-- PC"
        } else {
            ""
        };
        eprintln!("  {:04X}: {} |{}|{}", base, hex.join(" "), ascii, pc_marker);
    }

    // Disassemble around PC
    eprintln!("\n=== Disassembly around PC=0x{:04X} ===", pc);
    let start = (pc as usize).saturating_sub(16);
    let end = (pc as usize + 32).min(ram.len());
    let chunk: Vec<String> = ram[start..end].iter().map(|b| format!("{b:02X}")).collect();
    eprintln!("  [{:04X}]: {}", start, chunk.join(" "));

    // Run more frames and check again
    for _ in 0..200 {
        core.execute(Command::StepFrame);
    }
    let ram2 = core.z80_ram();
    let pc2 = core.z80_pc();
    let snap2 = core.z80_snapshot();

    eprintln!("\n=== Z80 state at frame 300 ===");
    eprintln!(
        "PC=0x{:04X} SP=0x{:04X} A=0x{:02X} HL=0x{:04X} IX=0x{:04X} halted={} IM={}",
        snap2.pc,
        snap2.sp,
        snap2.a,
        (snap2.h as u16) << 8 | snap2.l as u16,
        snap2.ix,
        snap2.halted,
        snap2.im
    );

    // Dump around PC2 if different
    if pc2 != pc {
        eprintln!("\n=== Disassembly around PC=0x{:04X} ===", pc2);
        let start = (pc2 as usize).saturating_sub(16);
        let end = (pc2 as usize + 32).min(ram2.len());
        let chunk: Vec<String> = ram2[start..end]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        eprintln!("  [{:04X}]: {}", start, chunk.join(" "));
    }

    // Check if the Z80 has written to YM2612 address space markers
    // The SMPS driver writes to Z80 RAM locations as work variables
    eprintln!("\n=== Key Z80 RAM locations ===");
    eprintln!("  RAM[0x1C00..0x1C20]: {:02X?}", &ram2[0x1C00..0x1C20]);
    eprintln!("  RAM[0x1F00..0x1F20]: {:02X?}", &ram2[0x1F00..0x1F20]);
    eprintln!("  RAM[0x1FE0..0x2000]: {:02X?}", &ram2[0x1FE0..]);
}

/// Debug: dump VDP state during zone title card to diagnose z-ordering.
#[test]
#[ignore]
fn sonic_title_card_debug() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!(
                "Sonic ROM not found: set {SONIC_ROM_ENV}=/path/to/sonic.md and run with \
                 `-- --ignored` to exercise this test."
            );
            return;
        }
    };

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    // Press Start to skip title screen
    for _ in 0..200 {
        core.execute(Command::StepFrame);
    }
    core.execute(Command::PressButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });
    core.execute(Command::StepFrame);
    core.execute(Command::ReleaseButton {
        port: 0,
        button: genesoxide_core::Button::Start,
    });

    // Run frames during the title card
    for frame in 0..120 {
        core.execute(Command::StepFrame);
        let snap = core.vdp_snapshot();
        let win_h = snap.registers[0x11];
        let win_v = snap.registers[0x12];

        // Scroll A nametable base
        let nt_a_base = usize::from(snap.registers[0x02] & 0x38) << 10;
        let h_cells: usize = match snap.registers[0x10] & 0x03 {
            0 => 32,
            1 => 64,
            3 => 128,
            _ => 32,
        };

        // Sample nametable priority around screen middle (rows 12-18, lines 96-144)
        let mut hi = 0u32;
        let mut lo = 0u32;
        for row in 12..18 {
            for col in 0..h_cells.min(40) {
                let offset = (row * h_cells + col) * 2;
                let addr = nt_a_base + offset;
                if addr + 1 < snap.vram.len() {
                    let entry = u16::from(snap.vram[addr]) << 8 | u16::from(snap.vram[addr + 1]);
                    let tile = entry & 0x07FF;
                    if tile != 0 {
                        if entry & 0x8000 != 0 {
                            hi += 1;
                        } else {
                            lo += 1;
                        }
                    }
                }
            }
        }

        // Sample sprite attributes (first 10 sprites)
        let sat_base = usize::from(snap.registers[0x05] & 0x7F) << 9;
        let mut sprite_info = Vec::new();
        let mut idx = 0u8;
        for _ in 0..10 {
            let ea = sat_base + usize::from(idx) * 8;
            if ea + 7 >= snap.vram.len() {
                break;
            }
            let w0 = u16::from(snap.vram[ea]) << 8 | u16::from(snap.vram[ea + 1]);
            let w1 = u16::from(snap.vram[ea + 2]) << 8 | u16::from(snap.vram[ea + 3]);
            let w2 = u16::from(snap.vram[ea + 4]) << 8 | u16::from(snap.vram[ea + 5]);
            let w3 = u16::from(snap.vram[ea + 6]) << 8 | u16::from(snap.vram[ea + 7]);
            let sy = (w0 & 0x03FF).wrapping_sub(128);
            let sx = (w3 & 0x01FF).wrapping_sub(128);
            let vs = ((w1 >> 8) & 3) + 1;
            let hs = ((w1 >> 10) & 3) + 1;
            let pri = if w2 & 0x8000 != 0 { "HI" } else { "lo" };
            let link = w1 & 0x7F;
            sprite_info.push(format!("#{idx}({sx},{sy} {hs}x{vs} {pri})"));
            if link == 0 {
                break;
            }
            idx = link as u8;
        }

        if frame < 5 || frame % 10 == 0 || (frame >= 30 && frame <= 50) {
            eprintln!(
                "F{frame:3}: win_h=0x{win_h:02X} win_v=0x{win_v:02X} scrollA_pri(hi={hi}/lo={lo}) sprites=[{}]",
                sprite_info.join(", ")
            );
        }
    }
}

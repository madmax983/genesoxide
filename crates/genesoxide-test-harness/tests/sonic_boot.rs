//! Sonic the Hedgehog integration tests.
//! Run with: cargo test -p genesoxide-test-harness --test sonic_boot -- --nocapture

use genesoxide_core::{Command, GenesisCore};

const SONIC_ROM_PATH: &str =
    r"C:\Users\markm\AppData\Local\Temp\sonic_test\Sonic The Hedgehog (USA, Europe).md";

fn load_sonic() -> Option<Vec<u8>> {
    std::fs::read(SONIC_ROM_PATH).ok()
}

#[test]
fn sonic_boot_diagnostic() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
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
fn sonic_renders_hud() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
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
fn sonic_produces_audio() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
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

/// Debug: dump VDP state during zone title card to diagnose z-ordering.
#[test]
#[ignore]
fn sonic_title_card_debug() {
    let rom = match load_sonic() {
        Some(r) => r,
        None => {
            eprintln!("Sonic ROM not found, skipping");
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

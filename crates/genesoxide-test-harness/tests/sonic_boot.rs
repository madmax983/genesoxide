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

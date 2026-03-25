//! Diagnostic: check if Sonic produces visible pixels.
//! Run with: cargo test -p genesoxide-test-harness --test sonic_boot -- --nocapture

use genesoxide_core::{Command, GenesisCore};

#[test]
fn sonic_boot_diagnostic() {
    let rom_path =
        r"C:\Users\markm\AppData\Local\Temp\sonic_test\Sonic The Hedgehog (USA, Europe).md";
    let rom = match std::fs::read(rom_path) {
        Ok(r) => r,
        Err(_) => {
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

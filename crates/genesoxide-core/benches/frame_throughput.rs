use divan::Bencher;
use genesoxide_core::{Command, GenesisCore};

fn main() {
    divan::main();
}

#[divan::bench]
fn step_frame_empty_rom(bencher: Bencher) {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(vec![0; 1024]));

    bencher.bench_local(|| {
        core.execute(Command::StepFrame);
    });
}

/// Builds a tiny bootable 68000 ROM whose mainline runs an infinite loop that
/// rewrites the backdrop register (reg 0x07) as the beam sweeps, so EVERY active
/// scanline records mid-line events and renders via the VDP span path. This is
/// the worst case for the mid-line raster feature and measures the cost of the
/// span path (vs. the byte-identical fast path exercised by `step_frame_empty_rom`,
/// where no line has events). Mirrors the `midline_backdrop_split` golden ROM.
fn build_midline_backdrop_rom() -> Vec<u8> {
    // VDP control port = 0xC00004, data port = 0xC00000. Reset vectors put SSP at
    // 0xFF0000 and PC at 0x000200; code lives at ROM offset 0x200.
    let mut words: Vec<u16> = Vec::new();
    // Prologue: a0 = control port, a1 = data port.
    words.extend_from_slice(&[0x207C, 0x00C0, 0x0004]); // movea.l #0xC00004, a0
    words.extend_from_slice(&[0x227C, 0x00C0, 0x0000]); // movea.l #0xC00000, a1
    // Minimal registers for a visible H40 display, backdrop index 0.
    words.extend_from_slice(&[0x30BC, 0x8004]); // reg 0x00 = 0x04 (no H-int)
    words.extend_from_slice(&[0x30BC, 0x8140]); // reg 0x01 = 0x40 (display on)
    words.extend_from_slice(&[0x30BC, 0x8700]); // reg 0x07 = 0x00 (backdrop idx 0)
    words.extend_from_slice(&[0x30BC, 0x8C81]); // reg 0x0C = 0x81 (H40)
    words.extend_from_slice(&[0x30BC, 0x8F02]); // reg 0x0F = 0x02 (autoinc 2)
    // Preload CRAM 0..7 with distinct colors (set CRAM addr 0, then 8 data words).
    words.extend_from_slice(&[0x30BC, 0xC000]); // CRAM write addr, high word
    words.extend_from_slice(&[0x30BC, 0x0000]); // ... low word (index 0)
    for &color in &[
        0x000Eu16, 0x00E0, 0x0E00, 0x00EE, 0x0E0E, 0x0EE0, 0x0EEE, 0x0080,
    ] {
        words.extend_from_slice(&[0x32BC, color]); // move.w #color, (a1)
    }
    // Seed d0 with the reg-0x07 write command for backdrop index 0.
    words.extend_from_slice(&[0x303C, 0x8700]); // move.w #0x8700, d0
    // Infinite backdrop-write loop:
    //   move.w d0,(a0) ; addq.w #1,d0 ; andi.w #0x8707,d0 ; bra.s loop
    words.extend_from_slice(&[0x3080, 0x5240, 0x0240, 0x8707]);
    words.push(0x60F6); // bra.s -10 -> back to move.w d0,(a0)

    // Serialize big-endian starting at 0x200; set reset vectors; pad to 0x400.
    let code_start = 0x200usize;
    let mut rom = vec![0u8; code_start + words.len() * 2];
    for (i, w) in words.iter().enumerate() {
        let off = code_start + i * 2;
        rom[off] = (w >> 8) as u8;
        rom[off + 1] = (*w & 0xFF) as u8;
    }
    // Reset vectors: SSP = 0x00FF0000, PC = 0x00000200.
    rom[0..4].copy_from_slice(&0x00FF_0000u32.to_be_bytes());
    rom[4..8].copy_from_slice(&0x0000_0200u32.to_be_bytes());
    let min_len = 0x400usize.max(rom.len());
    rom.resize(min_len.next_power_of_two(), 0);
    rom
}

#[divan::bench]
fn step_frame_midline_span_path(bencher: Bencher) {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(build_midline_backdrop_rom()));
    // Warm up so the mid-line write loop is running across active lines.
    for _ in 0..4 {
        core.execute(Command::StepFrame);
    }

    bencher.bench_local(|| {
        core.execute(Command::StepFrame);
    });
}

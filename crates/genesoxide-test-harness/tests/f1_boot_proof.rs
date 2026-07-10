//! Audit item **F1** — SGDK black-screen fix — end-to-end boot proof.
//!
//! This is the committed, freely-licensed replacement for the old F1 coverage
//! that depended on a hardcoded commercial Sonic ROM (see `sonic_boot.rs`, now
//! env-gated). It builds a purpose-built, SGDK-style boot ROM entirely from the
//! [`RomBuilder`] (all emitted 68000 code is original and check-in-able), runs
//! it through the REAL core (CPU → bus → VDP), and asserts the same behaviour
//! the F1 fix restored:
//!
//! 1. **Z80 BUSREQ poll** — at reset the ROM performs SGDK's bus request/grant/
//!    release handshake against 0xA11100, including the *release-wait* loop that
//!    hung forever under the historical BUSREQ-read mask bug. Interrupts are
//!    masked (SR mask 7) across the whole boot setup, so if that loop hangs the
//!    mask never drops, the latched V-blank interrupt is never delivered, the
//!    queued DMA never flushes, and the screen stays black — i.e. the whole test
//!    fails, exactly reproducing the original bug's symptom.
//! 2. **V-blank DMA-queue flush** — the visible tilemap is written to VRAM
//!    ONLY by a 68K→VRAM DMA issued from inside the V-blank interrupt handler
//!    (the "DMA queue flushed in V-blank" path). A populated plane-A nametable
//!    therefore proves the V-interrupt was latched (reg 0x01 bit 5) while the
//!    68000 held mask 7 during setup, then delivered once the mask dropped, and
//!    that the handler's DMA ran.
//!
//! The ROM also loads a non-black palette (CRAM) and a solid tile, so a correct
//! boot yields a fully rendered, non-black framebuffer. This test is
//! ALWAYS-RUN: it needs no external data and must pass against the current
//! (fixed) core.
//!
//! Run with:
//! `cargo test -p genesoxide-test-harness --test f1_boot_proof -- --nocapture`

use genesoxide_core::{Command, GenesisCore};
use genesoxide_test_harness::rom_builder::{RomBuilder, VINT_VECTOR, vblank_dma_handler};

/// Plane-A nametable base in VRAM. reg 0x02 = 0x30 → (0x30 & 0x38) << 10 = 0xC000.
const PLANE_A_NT: u16 = 0xC000;
/// Work-RAM base used to stage the DMA source buffer (64KB work RAM @ 0xFF0000).
const DMA_SRC: u32 = 0x00FF_0000;
/// Visible tilemap dimensions (32×28 cells covers the H40 320×224 display).
const NT_COLS: u16 = 32;
const NT_ROWS: u16 = 28;
/// Number of nametable entries (words) staged and DMA'd into plane A.
const NT_ENTRIES: u16 = NT_COLS * NT_ROWS;
/// Nametable entry: tile 1, palette line 0, low priority.
const NT_ENTRY: u16 = 0x0001;
/// Solid tile color index (within palette line 0).
const TILE_COLOR: u8 = 1;
/// CRAM color for index 1 — white (0x0EEE), so the tile renders bright non-black.
const PALETTE_COLOR: u16 = 0x0EEE;

/// Builds the SGDK-style F1 boot-proof ROM.
///
/// Boot sequence (after the builder prologue loads a0 = VDP control port,
/// a1 = VDP data port):
///
/// 1. mask all interrupts (SR mask 7) for the duration of setup;
/// 2. SGDK Z80 BUSREQ request/grant/release/wait-for-release handshake;
/// 3. stage the nametable DMA source buffer in work RAM;
/// 4. program the VDP registers (H40, plane bases, autoinc), display + VInt +
///    DMA enabled;
/// 5. load the palette (CRAM) and the solid tile graphics;
/// 6. install a V-blank handler that flushes a queued 68K→VRAM DMA of the staged
///    nametable into plane A;
/// 7. drop the interrupt mask to 0 so the latched V-blank interrupt is delivered;
/// 8. spin in the builder's trailing `bra.s *`.
fn build_f1_boot_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();

    // (1) Hold interrupts off through boot setup (SGDK does this). This makes the
    //     BUSREQ handshake *load-bearing*: only after it completes and we drop the
    //     mask can the latched V-blank interrupt be delivered.
    b.mask_all_interrupts();

    // (2) SGDK-style Z80 BUSREQ handshake. The release-wait loop hangs under the
    //     historical BUSREQ-read mask bug; if it hangs, nothing below ever runs.
    b.z80_busreq_poll();

    // (3) Stage the DMA source: NT_ENTRIES copies of `NT_ENTRY` in work RAM.
    b.fill_work_ram(DMA_SRC, NT_ENTRY, NT_ENTRIES);

    // (4) VDP register setup.
    b.set_register(0x00, 0x04); // Mode 1: no H-int
    b.set_register(0x02, 0x30); // Plane A nametable = 0xC000
    b.set_register(0x03, 0x28); // Window nametable   = 0xA000
    b.set_register(0x04, 0x07); // Plane B nametable   = 0xE000
    b.set_register(0x05, 0x6C); // Sprite attr table   = 0xD800
    b.set_register(0x07, 0x00); // Backdrop color index 0
    b.set_register(0x0B, 0x00); // Scroll modes: full-screen
    b.set_register(0x0C, 0x81); // Mode 4: H40 (RS0|RS1) → 320px
    b.set_register(0x0D, 0x37); // H-scroll table = 0xDC00
    b.set_register(0x0F, 0x02); // Auto-increment = 2
    b.set_register(0x10, 0x00); // Scroll size 32×32

    // (5) Palette + tile graphics (written directly; the nametable is NOT).
    b.set_cram_color(1, PALETTE_COLOR);
    b.write_solid_tile(1, TILE_COLOR);

    // (6a) Enable display + V-blank interrupt + DMA (reg 0x01 = 0x74), last.
    b.enable_display_vint_dma();

    // (6b) Install the V-blank handler that flushes the queued 68K→VRAM DMA.
    let handler = vblank_dma_handler(PLANE_A_NT, DMA_SRC, NT_ENTRIES);
    b.set_handler(VINT_VECTOR, &handler);

    // (7) Drop the interrupt mask so the latched V-blank interrupt fires.
    b.lower_interrupt_mask();

    b.finish()
}

/// Full F1 boot proof: the SGDK-style ROM must boot through the BUSREQ poll,
/// flush its queued DMA in V-blank, and render a non-black framebuffer.
#[test]
fn f1_sgdk_boot_renders_via_vblank_dma() {
    let rom = build_f1_boot_rom();

    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(rom));

    // Frame 0 runs boot setup and takes the first V-blank interrupt (which flushes
    // the DMA); subsequent frames render the now-populated plane. Run a healthy
    // margin of frames.
    const FRAMES: u32 = 20;
    for _ in 0..FRAMES {
        core.execute(Command::StepFrame);
    }

    let snap = core.vdp_snapshot();

    // (a) Plane-A nametable populated — written ONLY by the V-blank handler's DMA,
    //     so any non-zero entry proves the VInt-latch → delivery → DMA-flush path
    //     executed. Base derives from reg 0x02 exactly as the renderer computes it.
    let nt_base = usize::from(snap.registers[0x02] & 0x38) << 10;
    let nt_entries_nonzero = (0..NT_ENTRIES as usize)
        .filter(|&i| {
            let a = nt_base + i * 2;
            snap.vram[a] != 0 || snap.vram[a + 1] != 0
        })
        .count();

    // (b) CRAM populated (non-black palette).
    let cram_nonzero = snap.cram.iter().filter(|&&c| c != 0).count();

    // (c) Framebuffer non-black.
    let fb = core.framebuffer_rgba();
    let total_px = fb.len() / 4;
    let non_black = fb
        .chunks(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    eprintln!(
        "f1_boot_proof: nt_base=0x{nt_base:04X} nt_entries_nonzero={nt_entries_nonzero}/{NT_ENTRIES} \
         cram_nonzero={cram_nonzero}/{} non_black_px={non_black}/{total_px}",
        snap.cram.len()
    );

    // The V-blank DMA path actually ran: the plane it targets is populated.
    assert!(
        nt_entries_nonzero > 0,
        "plane-A nametable must be populated by the V-blank DMA — a zero nametable \
         means the VInt was never delivered or the DMA queue never flushed \
         (the SGDK black-screen regression)"
    );
    // We staged the entire visible plane, so the flush should have copied all of it.
    assert_eq!(
        nt_entries_nonzero, NT_ENTRIES as usize,
        "the V-blank DMA should have copied all {NT_ENTRIES} staged nametable entries"
    );

    // Non-black palette present.
    assert!(
        cram_nonzero > 0,
        "CRAM must hold a non-black palette (got {cram_nonzero} non-zero entries)"
    );

    // Substantial rendered content, mirroring `sonic_renders_hud`'s threshold.
    assert!(
        non_black > 1000,
        "framebuffer must have substantial rendered content (got {non_black} non-black pixels)"
    );
}

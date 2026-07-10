//! Headless golden-frame VIDEO test harness.
//!
//! Each test in this file programmatically builds a tiny, freely-licensed 68000
//! test ROM (see `genesoxide_test_harness::rom_builder`) that programs the VDP to
//! render a specific scene, runs it through the REAL emulator
//! (`run_rom_frames`, i.e. CPU -> bus -> VDP), and then:
//!
//! 1. Asserts specific known pixels in the rendered 320x224 RGBA framebuffer.
//!    These assertions encode the *expected* rendering derived from the VDP rules
//!    (shadow/highlight intensities, window boundary position, normal
//!    compositing) — they are the real proof of correctness.
//! 2. Compares the whole framebuffer against a checked-in raw `.rgba` golden via
//!    `compare_framebuffers` (exact, 0 differing pixels).
//!
//! # Scenes
//!
//! * `shadow_highlight_operators` — S/H enabled. Side-by-side plane regions plus
//!   operator sprites exercise every operator case:
//!   (a) high-priority plane -> Normal, (b) low-priority plane -> Shadow,
//!   (c) shadow-operator sprite over a Normal region -> Shadow,
//!   (d) highlight-operator sprite over a Normal region -> Highlight,
//!   (e) highlight-operator sprite over a Shadowed region -> Normal,
//!   and the backdrop -> Shadow.
//! * `window_plane_positioning` — window plane with a left split at 10 units.
//!   Because WHP (reg 0x11) is in 16px (2-cell) units, the window/Scroll-A
//!   boundary must land at pixel 160; asserted at x=159 (window) vs x=160 (plane).
//! * `normal_render_lock` — S/H disabled. A high-priority sprite over a
//!   low-priority plane locks current correct normal compositing against
//!   regressions.
//!
//! # Regenerating goldens
//!
//! Goldens live in `tests/goldens/<scene>.rgba` (raw RGBA, 286720 bytes each).
//! To regenerate after an intentional rendering change:
//!
//! ```text
//! GENESOXIDE_BLESS=1 cargo test -p genesoxide-test-harness --test video_golden
//! ```
//!
//! A normal run (without the env var) loads the goldens and asserts an exact
//! match. The programmatic pixel assertions run in BOTH modes, so a blessed
//! golden can never encode output that violates the VDP rules.

use genesoxide_core::Region;
use genesoxide_test_harness::rom_builder::{HINT_VECTOR, RomBuilder, VINT_VECTOR};
use genesoxide_test_harness::{compare_framebuffers, run_rom_frames, run_rom_frames_region};

const FRAME_W: usize = 320;
const FRAME_H: usize = 224;

/// Number of frames to run: frame 0 completes the CPU setup program (which then
/// spins in an infinite loop), so a later frame renders the fully-built VRAM.
const FRAMES: u64 = 4;

/// Gray used for the S/H scene. CRAM 0x0888 -> Normal RGBA [146,146,146].
const SH_GRAY: u16 = 0x0888;

/// Returns the RGBA pixel at (x, y) in a 320x224 (H40) framebuffer.
fn pixel(fb: &[u8], x: usize, y: usize) -> [u8; 4] {
    assert!(x < FRAME_W && y < FRAME_H, "pixel ({x},{y}) out of bounds");
    let o = (y * FRAME_W + x) * 4;
    [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
}

/// Returns the RGBA pixel at (x, y) in a framebuffer packed at the given native
/// row `width` (256 for H32, 320 for H40).
fn pixel_w(fb: &[u8], x: usize, y: usize, width: usize) -> [u8; 4] {
    assert!(x < width && y < FRAME_H, "pixel ({x},{y}) out of bounds");
    let o = (y * width + x) * 4;
    [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
}

/// Normal RGBA for a CRAM color, mirroring `Vdp::color_to_rgba`.
fn normal_rgba(color: u16) -> [u8; 4] {
    let r = ((color & 0x00E) >> 1) as u8;
    let g = ((color & 0x0E0) >> 5) as u8;
    let b = ((color & 0xE00) >> 9) as u8;
    [r * 36 + r / 2, g * 36 + g / 2, b * 36 + b / 2, 0xFF]
}

fn shadow_of(c: [u8; 4]) -> [u8; 4] {
    [c[0] >> 1, c[1] >> 1, c[2] >> 1, c[3]]
}

fn highlight_of(c: [u8; 4]) -> [u8; 4] {
    [128 + (c[0] >> 1), 128 + (c[1] >> 1), 128 + (c[2] >> 1), c[3]]
}

/// Loads or (when GENESOXIDE_BLESS is set) regenerates a raw RGBA golden, then
/// asserts the rendered framebuffer matches it exactly.
fn check_golden(scene: &str, rendered: &[u8]) {
    let path = format!(
        "{}/tests/goldens/{}.rgba",
        env!("CARGO_MANIFEST_DIR"),
        scene
    );

    if std::env::var("GENESOXIDE_BLESS").is_ok() {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent).expect("create goldens dir");
        }
        std::fs::write(&path, rendered).expect("write golden");
        eprintln!("blessed golden: {path} ({} bytes)", rendered.len());
        return;
    }

    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {path}: {e}. Regenerate with \
             GENESOXIDE_BLESS=1 cargo test -p genesoxide-test-harness --test video_golden"
        )
    });
    let diff = compare_framebuffers(rendered, &golden);
    assert_eq!(diff, 0, "scene `{scene}`: {diff} pixels differ from golden");
}

/// Common minimal register setup for a visible frame. `reg0c` selects the mode-4
/// register: H40 (320px) needs BOTH RS0|RS1 (0x81), H32 (256px) uses 0x00; add
/// S/H bit3 = 0x08 when desired. Does not enable the VBlank interrupt (no handler
/// is present in these tiny ROMs).
fn base_registers(b: &mut RomBuilder, reg0c: u8, backdrop_index: u8) {
    b.set_register(0x00, 0x04); // Mode 1: no H-int
    b.set_register(0x01, 0x40); // Mode 2: display on, no VInt, no DMA
    b.set_register(0x02, 0x30); // Scroll A nametable = 0xC000
    b.set_register(0x03, 0x28); // Window nametable = 0xA000
    b.set_register(0x04, 0x07); // Scroll B nametable = 0xE000
    b.set_register(0x05, 0x6C); // Sprite attribute table = 0xD800
    b.set_register(0x07, backdrop_index); // Backdrop color index
    b.set_register(0x0A, 0xFF); // H-int counter (unused)
    b.set_register(0x0B, 0x00); // Scroll modes: full-screen
    b.set_register(0x0C, reg0c); // Mode 4: horizontal mode (+ optional S/H)
    b.set_register(0x0D, 0x37); // H-scroll table = 0xDC00
    b.set_register(0x0F, 0x02); // Auto-increment = 2
    b.set_register(0x10, 0x00); // Scroll size 32x32
    b.set_register(0x11, 0x00); // Window H position
    b.set_register(0x12, 0x00); // Window V position
}

const SCROLL_A_NT: u16 = 0xC000;
const WINDOW_NT: u16 = 0xA000;
const SPRITE_TABLE: u16 = 0xD800;

/// Fills a 32x32-cell scroll nametable at `base` so every row is identical to
/// `row_pattern` (32 entries). Written as one contiguous auto-incrementing
/// stream so all scanlines render the same horizontal bands.
fn fill_nametable_32(b: &mut RomBuilder, base: u16, row_pattern: &[u16; 32]) {
    let mut entries = Vec::with_capacity(32 * 32);
    for _ in 0..32 {
        entries.extend_from_slice(row_pattern);
    }
    b.write_vram(base, &entries);
}

/// Writes one sprite entry (4 words) at slot `slot` in the sprite table.
fn write_sprite(
    b: &mut RomBuilder,
    slot: u16,
    y_raw: u16,
    v_size: u8,
    h_size: u8,
    link: u8,
    attr: u16,
    x_raw: u16,
) {
    let addr = SPRITE_TABLE + slot * 8;
    let word1 = (u16::from(h_size & 3) << 10) | (u16::from(v_size & 3) << 8) | u16::from(link & 0x7F);
    b.write_vram(addr, &[y_raw, word1, attr, x_raw]);
}

// ---------------------------------------------------------------------------
// Scene 1: Shadow / Highlight — all operator cases
// ---------------------------------------------------------------------------

/// Builds the shadow/highlight scene ROM.
///
/// Scroll A is a gray plane (CRAM 0x0888). Cell columns carry a priority pattern
/// and operator sprites cover a horizontal band (y = 100..107):
///
/// | cols  | x range | plane pri | sprite band (y~103)        |
/// |-------|---------|-----------|----------------------------|
/// | 0-3   | 0-31    | high      | (none) Normal              |
/// | 4-7   | 32-63   | low       | (none) Shadow              |
/// | 8-11  | 64-95   | high      | shadow op   -> Shadow      |
/// | 12-15 | 96-127  | high      | highlight op-> Highlight   |
/// | 16-19 | 128-159 | low       | highlight op-> Normal      |
/// | 20-31 | 160-255 | (transp)  | backdrop -> Shadow         |
fn build_shadow_highlight_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 (0x81 = RS0|RS1) + S/H (0x08); backdrop = palette 0 color 15.
    base_registers(&mut b, 0x89, 0x0F);

    // Palette entries: gray at index 1 and at backdrop index 15.
    b.set_cram_color(1, SH_GRAY);
    b.set_cram_color(15, SH_GRAY);

    // Tile 1: solid gray (color index 1). Tiles 2-5: shadow operator (color 15).
    // Tiles 6-9: highlight operator (color 14).
    b.write_solid_tile(1, 1);
    for t in 2..=5 {
        b.write_solid_tile(t, 15);
    }
    for t in 6..=9 {
        b.write_solid_tile(t, 14);
    }

    // Scroll A nametable row pattern (palette 0, gray tile 1).
    const HI: u16 = 0x8001; // high priority, tile 1
    const LO: u16 = 0x0001; // low priority, tile 1
    let mut row = [0u16; 32];
    for c in 0..4 {
        row[c] = HI;
    } // A: high
    for c in 4..8 {
        row[c] = LO;
    } // B: low
    for c in 8..12 {
        row[c] = HI;
    } // C: high (shadow op)
    for c in 12..16 {
        row[c] = HI;
    } // D: high (highlight op)
    for c in 16..20 {
        row[c] = LO;
    } // E: low (highlight op)
    // cols 20..31 stay 0 -> transparent -> backdrop.
    fill_nametable_32(&mut b, SCROLL_A_NT, &row);

    // Operator sprites, 4 cells wide x 1 cell tall (32x8), band y=100..107.
    // Palette 3. word2 = palette(3<<13) | base_tile.
    let y_raw = 100 + 128; // 0x0E4
    // Sprite 0 (C): shadow operator, tiles 2..5, x = 64.
    write_sprite(&mut b, 0, y_raw, 0, 3, 1, 0x6000 | 2, 64 + 128);
    // Sprite 1 (D): highlight operator, tiles 6..9, x = 96.
    write_sprite(&mut b, 1, y_raw, 0, 3, 2, 0x6000 | 6, 96 + 128);
    // Sprite 2 (E): highlight operator, tiles 6..9, x = 128, end of list.
    write_sprite(&mut b, 2, y_raw, 0, 3, 0, 0x6000 | 6, 128 + 128);

    b.finish()
}

#[test]
fn shadow_highlight_operators() {
    let rom = build_shadow_highlight_rom();
    let fb = run_rom_frames(rom, FRAMES);

    let normal = normal_rgba(SH_GRAY); // [146,146,146,255]
    let shadow = shadow_of(normal); // [73,73,73,255]
    let highlight = highlight_of(normal); // [201,201,201,255]

    // Sanity on the intensity math.
    assert_eq!(normal, [146, 146, 146, 255]);
    assert_eq!(shadow, [73, 73, 73, 255]);
    assert_eq!(highlight, [201, 201, 201, 255]);

    // Row above the sprite band (y=8): plane-only priority behavior.
    assert_eq!(pixel(&fb, 16, 8), normal, "(a) high-priority plane -> Normal");
    assert_eq!(pixel(&fb, 48, 8), shadow, "(b) low-priority plane -> Shadow");
    assert_eq!(
        pixel(&fb, 200, 8),
        shadow,
        "backdrop (priority 0) -> Shadow"
    );
    // Without an operator, the high-priority C/D columns are Normal here.
    assert_eq!(pixel(&fb, 80, 8), normal, "col C high plane (no op) -> Normal");
    assert_eq!(pixel(&fb, 112, 8), normal, "col D high plane (no op) -> Normal");

    // Inside the sprite band (y=103): operator effects.
    assert_eq!(
        pixel(&fb, 80, 103),
        shadow,
        "(c) shadow operator over Normal -> Shadow"
    );
    assert_eq!(
        pixel(&fb, 112, 103),
        highlight,
        "(d) highlight operator over Normal -> Highlight"
    );
    assert_eq!(
        pixel(&fb, 144, 103),
        normal,
        "(e) highlight operator over Shadow -> Normal"
    );
    // Region A still Normal, region B still Shadow within the band (no sprite).
    assert_eq!(pixel(&fb, 16, 103), normal, "region A within band -> Normal");
    assert_eq!(pixel(&fb, 48, 103), shadow, "region B within band -> Shadow");

    check_golden("shadow_highlight_operators", &fb);
}

// ---------------------------------------------------------------------------
// Scene 2: Window plane positioning (16px/unit boundary)
// ---------------------------------------------------------------------------

fn build_window_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 only (0x81 = RS0|RS1, S/H off). Backdrop black.
    base_registers(&mut b, 0x81, 0x00);

    // Colors: red (Scroll A) at index 1, green (window) at index 2.
    b.set_cram_color(1, 0x000E); // red
    b.set_cram_color(2, 0x00E0); // green

    b.write_solid_tile(1, 1); // red
    b.write_solid_tile(2, 2); // green

    // Scroll A: solid red across every row (low priority, tile 1).
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // Window nametable is 64 cells wide in H40. Fill 28 rows (covers 224 lines)
    // with green tile 2 as one contiguous stream.
    let mut win = Vec::with_capacity(64 * 28);
    for _ in 0..64 * 28 {
        win.push(0x0002u16);
    }
    b.write_vram(WINDOW_NT, &win);

    // Window: left side, 10 units. WHP is in 16px units -> boundary at 160px.
    b.set_register(0x11, 0x0A);
    // Window vertical: full screen (31 cells -> 248 lines, covers 224).
    b.set_register(0x12, 0x1F);

    b.finish()
}

#[test]
fn window_plane_positioning() {
    let rom = build_window_rom();
    let fb = run_rom_frames(rom, FRAMES);

    let red = normal_rgba(0x000E); // [255,0,0,255]
    let green = normal_rgba(0x00E0); // [0,255,0,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(green, [0, 255, 0, 255]);

    let y = 100;
    // Inside window (x < 160): green.
    assert_eq!(pixel(&fb, 0, y), green, "x=0 is window (green)");
    assert_eq!(pixel(&fb, 8, y), green, "x=8 is window (green)");
    // The boundary: 10 units * 16px/unit = 160. x=159 window, x=160 Scroll A.
    assert_eq!(
        pixel(&fb, 159, y),
        green,
        "x=159 just left of the 16px boundary is window (green)"
    );
    assert_eq!(
        pixel(&fb, 160, y),
        red,
        "x=160 at the 16px boundary is Scroll A (red)"
    );
    assert_eq!(pixel(&fb, 200, y), red, "x=200 is Scroll A (red)");

    check_golden("window_plane_positioning", &fb);
}

// ---------------------------------------------------------------------------
// Scene 3: Normal-render lock (S/H disabled) — sprite over plane
// ---------------------------------------------------------------------------

fn build_normal_lock_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 only (0x81 = RS0|RS1, S/H off). Backdrop black.
    base_registers(&mut b, 0x81, 0x00);

    // Colors: blue plane at index 1, green sprite at index 2.
    b.set_cram_color(1, 0x0E00); // blue
    b.set_cram_color(2, 0x00E0); // green

    b.write_solid_tile(1, 1); // blue plane tile
    // Sprite is 2x2 tiles (16x16); column-major tiles base..base+3 = 2..5.
    for t in 2..=5 {
        b.write_solid_tile(t, 2); // green sprite (color index 2)
    }

    // Scroll A: solid blue, low priority, every row.
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // One high-priority green sprite (palette 0, tile 2) at screen (64,64),
    // 16x16 px. word2 = priority(0x8000) | tile 2.
    write_sprite(&mut b, 0, 64 + 128, 1, 1, 0, 0x8000 | 2, 64 + 128);

    b.finish()
}

#[test]
fn normal_render_lock() {
    let rom = build_normal_lock_rom();
    let fb = run_rom_frames(rom, FRAMES);

    let blue = normal_rgba(0x0E00); // [0,0,255,255]
    let green = normal_rgba(0x00E0); // [0,255,0,255]
    assert_eq!(blue, [0, 0, 255, 255]);
    assert_eq!(green, [0, 255, 0, 255]);

    // Plane outside the sprite: blue.
    assert_eq!(pixel(&fb, 8, 8), blue, "plane background is blue");
    assert_eq!(pixel(&fb, 200, 200), blue, "plane background is blue");
    // Sprite region (64..79, 64..79): green over the plane.
    assert_eq!(pixel(&fb, 70, 70), green, "sprite draws green over plane");
    assert_eq!(pixel(&fb, 64, 64), green, "sprite top-left corner is green");
    assert_eq!(pixel(&fb, 79, 79), green, "sprite bottom-right corner is green");
    // Just outside the sprite: back to blue.
    assert_eq!(pixel(&fb, 80, 70), blue, "just right of sprite is blue");
    assert_eq!(pixel(&fb, 70, 80), blue, "just below sprite is blue");

    check_golden("normal_render_lock", &fb);
}

// ---------------------------------------------------------------------------
// Scene 4: H32 (256px) native-width framebuffer
// ---------------------------------------------------------------------------

/// H32 display width in pixels.
const H32_W: usize = 256;

fn build_h32_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H32 (0x00: neither RS0 nor RS1). Backdrop black.
    base_registers(&mut b, 0x00, 0x00);

    // Colors: red plane at index 1, green sprites at index 2.
    b.set_cram_color(1, 0x000E); // red
    b.set_cram_color(2, 0x00E0); // green

    b.write_solid_tile(1, 1); // red plane tile
    // 16x16 green sprite: 2x2 tiles column-major (tiles 2..5).
    for t in 2..=5 {
        b.write_solid_tile(t, 2);
    }

    // Scroll A: solid red across every one of the 32 cells (= full 256px width),
    // low priority, every row. This makes the ENTIRE 256-wide frame red, so the
    // rightmost columns carry content — there is no 64px black bar and no
    // 320-wide buffer.
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // Two green sprites at y=64: one at the left (x=16) and one hard against the
    // right edge (x=232 -> spans 232..247, inside 256). The right one exercises
    // sprite compositing in the far-right region of an H32 frame.
    write_sprite(&mut b, 0, 64 + 128, 1, 1, 1, 0x8000 | 2, 16 + 128);
    write_sprite(&mut b, 1, 64 + 128, 1, 1, 0, 0x8000 | 2, 232 + 128);

    b.finish()
}

#[test]
fn h32_centered() {
    let rom = build_h32_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // Native-width framebuffer: H32 is 256x224x4, NOT 320-wide.
    assert_eq!(fb.len(), H32_W * FRAME_H * 4, "H32 frame must be 256x224 RGBA");

    let red = normal_rgba(0x000E); // [255,0,0,255]
    let green = normal_rgba(0x00E0); // [0,255,0,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(green, [0, 255, 0, 255]);

    // Plane content is present across the FULL 256 width, including the
    // rightmost columns 248..255 — proving there is no left-jammed content with
    // a black bar and the buffer really is 256 wide (indexing 255 is valid).
    assert_eq!(pixel_w(&fb, 0, 100, H32_W), red, "leftmost column is plane red");
    assert_eq!(pixel_w(&fb, 128, 100, H32_W), red, "center column is plane red");
    assert_eq!(pixel_w(&fb, 250, 100, H32_W), red, "col 250 is plane red");
    assert_eq!(
        pixel_w(&fb, 255, 100, H32_W),
        red,
        "rightmost column (255) is plane red — no 64px black bar"
    );

    // Sprites render at both the left and the far right of the H32 frame.
    assert_eq!(pixel_w(&fb, 20, 68, H32_W), green, "left sprite is green");
    assert_eq!(pixel_w(&fb, 240, 68, H32_W), green, "right-edge sprite is green");

    check_golden("h32_centered", &fb);
}

// ---------------------------------------------------------------------------
// Scene 5: Runtime H40 -> H32 mode switch (no panic / no corruption)
// ---------------------------------------------------------------------------

fn build_mode_switch_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // Start in H40 (0x81), build content, THEN switch to H32 (0x00). Because the
    // setup stream runs within the first frame and the switch is applied at the
    // next frame boundary (frame_width is latched per frame), the captured final
    // frame renders entirely in H32.
    base_registers(&mut b, 0x81, 0x00);

    b.set_cram_color(1, 0x000E); // red
    b.write_solid_tile(1, 1);

    // Scroll A: solid red every row.
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // Switch the horizontal mode to H32 at the end of the setup stream.
    b.set_register(0x0C, 0x00);

    b.finish()
}

#[test]
fn mode_switch_sequence() {
    let rom = build_mode_switch_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // The runtime H40 -> H32 switch must be applied cleanly at a frame boundary:
    // the final frame is a coherent 256x224 buffer, not a crash or a ragged mix.
    assert_eq!(
        fb.len(),
        H32_W * FRAME_H * 4,
        "after switching to H32 the frame must be 256x224 RGBA"
    );

    let red = normal_rgba(0x000E);
    // Content is intact across the full native width (no corruption).
    assert_eq!(pixel_w(&fb, 0, 100, H32_W), red, "plane red at left");
    assert_eq!(pixel_w(&fb, 255, 100, H32_W), red, "plane red at right edge");

    check_golden("mode_switch_h32", &fb);
}

// ---------------------------------------------------------------------------
// Scene 6: PAL V30 (240-line) vertical mode
// ---------------------------------------------------------------------------

/// Builds a PAL V30 scene: a solid red Scroll A plane covering the full frame,
/// with reg 0x01 bit 3 (V30) set so the active area is 240 lines. Rendered under
/// a forced PAL region, the framebuffer is 320×240 and the plane extends past
/// the 224-line V28 boundary into the V30-only band (lines 224..239).
fn build_pal_v30_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 (RS0|RS1 = 0x81), S/H off. Backdrop black. Both resolution
    // bits are required for the 320px H40 width; only RS0 (0x01) would select
    // H32 (256px).
    base_registers(&mut b, 0x81, 0x00);
    // Enable V30: reg 0x01 = display on (0x40) + M2/V30 (0x08).
    b.set_register(0x01, 0x48);

    // Red plane at index 1.
    b.set_cram_color(1, 0x000E); // red
    b.write_solid_tile(1, 1);
    // Scroll A: solid red, low priority, every row (32 rows = 256px covers 240).
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    b.finish()
}

#[test]
fn pal_v30_240_lines() {
    let rom = build_pal_v30_rom();
    let fb = run_rom_frames_region(rom, FRAMES, Some(Region::Pal));

    // The active area is 320×240 in PAL V30.
    const V30_H: usize = 240;
    assert_eq!(fb.len(), FRAME_W * V30_H * 4, "PAL V30 framebuffer is 320x240");

    let red = normal_rgba(0x000E); // [255,0,0,255]
    assert_eq!(red, [255, 0, 0, 255]);

    let px = |x: usize, y: usize| -> [u8; 4] {
        let o = (y * FRAME_W + x) * 4;
        [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
    };
    // Red plane fills the visible area, including the V30-only band beyond 224.
    assert_eq!(px(10, 10), red, "top of frame is the red plane");
    assert_eq!(px(160, 200), red, "mid frame is the red plane");
    assert_eq!(px(10, 230), red, "line 230 (V30-only band) renders the plane");
    assert_eq!(px(300, 239), red, "last V30 line renders the plane");

    check_golden("pal_v30", &fb);
}

// ---------------------------------------------------------------------------
// Scene 7: HINT raster split — per-scanline backdrop color bands
// ---------------------------------------------------------------------------
//
// This scene demonstrates a per-scanline raster split driven by the VDP's
// H-interrupt (HINT). The 68000 program:
//
//   * enables H-interrupts (reg 0x00 bit 4) and sets the HINT counter
//     (reg 0x0A) to 0 so a HINT fires on every active scanline;
//   * enables V-interrupts (reg 0x01 bit 5) for the frame-start reset;
//   * lowers the CPU interrupt mask so levels 4 (HINT) and 6 (VINT) are taken;
//   * installs a HINT handler that, once per line, bumps a line counter in
//     work RAM and rewrites the backdrop palette index (reg 0x07) to
//     `line / 16`, selecting a different preloaded CRAM color every 16 lines;
//   * installs a VINT handler that, once per frame, resets the line counter
//     and the backdrop index to 0 so every frame reproduces the same bands.
//
// With no tiles or sprites drawn (all nametable entries are transparent tile
// 0), the whole screen shows the backdrop, so the result is a stack of 14
// solid horizontal color bands, each 16 scanlines tall.
//
// GRANULARITY NOTE: HINT delivery in this core is line-granular (documented in
// the VDP HINT-delivery comment in `api.rs` / Commit 3 — the handler's register
// writes land in the hblank *between* rendered lines, not mid-line). A true
// MID-LINE split (two colors within a single scanline) is therefore NOT
// representable at the current documented granularity and is deliberately out
// of scope for this scene; the bands here are strictly per-scanline.

/// Backdrop colors preloaded into CRAM entries 0..13. The HINT handler selects
/// entry `scanline / 16`, so band `b` (lines `16b..16b+15`) shows `BAND_COLORS[b]`.
const BAND_COLORS: [u16; 14] = [
    0x0000, // 0  black
    0x000E, // 1  red
    0x00E0, // 2  green
    0x0E00, // 3  blue
    0x00EE, // 4  yellow
    0x0E0E, // 5  magenta
    0x0EE0, // 6  cyan
    0x0EEE, // 7  white
    0x0008, // 8  dim red
    0x0080, // 9  dim green
    0x0800, // 10 dim blue
    0x0088, // 11 dim yellow
    0x0808, // 12 dim magenta
    0x0880, // 13 dim cyan
];

/// Height of each color band, in scanlines (= the HINT handler's `line / 16`).
const BAND_HEIGHT: usize = 16;

fn build_hint_raster_bands_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 only (0x81 = RS0|RS1, S/H off), backdrop index 0 initially.
    base_registers(&mut b, 0x81, 0x00);

    // Preload the band palette (CRAM entries 0..13). No tiles or sprites are
    // written, so every plane pixel is transparent and the backdrop fills the
    // screen.
    for (i, &color) in BAND_COLORS.iter().enumerate() {
        b.set_cram_color(i as u16, color);
    }

    // HINT handler (auto-vector 0x70). Runs once per active scanline; a0 still
    // holds the VDP control port (0xC00004) from the prologue and is never
    // modified by the spinning main loop. The line counter lives in work RAM at
    // short-absolute 0xF000 (sign-extends to 0xFFFFF000 -> masked to 0xFFF000).
    //
    //   addq.w #1, (0xF000).w   ; line counter++
    //   move.w (0xF000).w, d0   ; d0 = counter
    //   lsr.w  #4, d0           ; d0 = counter / 16  (band index)
    //   andi.w #0x000F, d0      ; safety clamp to 0..15
    //   ori.w  #0x8700, d0      ; VDP reg 0x07 (backdrop) write command
    //   move.w d0, (a0)         ; set backdrop palette index for this line
    //   rte
    b.set_handler(
        HINT_VECTOR,
        &[
            0x5278, 0xF000, // addq.w #1, (0xF000).w
            0x3038, 0xF000, // move.w (0xF000).w, d0
            0xE848, // lsr.w #4, d0
            0x0240, 0x000F, // andi.w #0x000F, d0
            0x0040, 0x8700, // ori.w #0x8700, d0
            0x3080, // move.w d0, (a0)
            0x4E73, // rte
        ],
    );

    // VINT handler (auto-vector 0x78). Runs once per frame at the top of
    // V-blank; resets the line counter and the backdrop index so each frame
    // renders identical bands.
    //
    //   clr.w  (0xF000).w         ; line counter = 0
    //   move.w #0x8700, (a0)      ; reg 0x07 = 0 (backdrop index 0)
    //   rte
    b.set_handler(
        VINT_VECTOR,
        &[
            0x4278, 0xF000, // clr.w (0xF000).w
            0x30BC, 0x8700, // move.w #0x8700, (a0)
            0x4E73, // rte
        ],
    );

    // Enable interrupts last, after all VRAM/CRAM setup is complete.
    b.set_register(0x00, 0x14); // Mode 1: H-int enable (bit 4) + bit 2
    b.set_register(0x0A, 0x00); // HINT counter = 0 -> fire every line
    b.set_register(0x01, 0x60); // Mode 2: display on (bit 6) + V-int enable (bit 5)

    // Lower the 68000 interrupt mask to 0 so levels 4 and 6 are delivered
    // (supervisor mode is retained). move.w #0x2000, sr.
    b.emit(&[0x46FC, 0x2000]);

    b.finish()
}

#[test]
fn hint_raster_bands() {
    let rom = build_hint_raster_bands_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // Expected backdrop color for scanline `y`.
    //
    // HINT delivery is line-granular: the handler for scanline L runs during
    // L's hblank and its reg 0x07 write takes effect on the NEXT rendered line.
    // The net result is a one-line lag — band 0 covers lines 0..=16 (line 0 has
    // no HINT, then the first 16 HINTs still select index 0), and band `b`
    // (b >= 1) covers lines `16b+1..=16b+16`. So the band index is
    // `(y - 1) / 16` for y >= 1, and 0 at y = 0.
    let band_of = |y: usize| if y == 0 { 0 } else { (y - 1) / BAND_HEIGHT };
    let expected_at = |y: usize| normal_rgba(BAND_COLORS[band_of(y)]);

    // Sanity on a couple of band colors.
    assert_eq!(normal_rgba(0x0000), [0, 0, 0, 255]);
    assert_eq!(normal_rgba(0x000E), [255, 0, 0, 255]);

    // Every band renders its preloaded backdrop color across the full width.
    // Sample two scanlines well inside each of the first several bands, at two
    // x positions each, proving (a) the band color is correct and (b) pixels
    // WITHIN a band (same scanline, different x) match.
    for band in 0..8usize {
        let y = band * BAND_HEIGHT + 8; // 8 lines into the band
        let want = expected_at(y);
        assert_eq!(
            pixel(&fb, 10, y),
            want,
            "band {band} (y={y}) left edge is its backdrop color"
        );
        assert_eq!(
            pixel(&fb, 310, y),
            want,
            "band {band} (y={y}) right edge matches left (within-band pixels match)"
        );
    }

    // Adjacent bands must DIFFER: a pixel in the top band vs one a band lower.
    assert_ne!(
        pixel(&fb, 160, 8),
        pixel(&fb, 160, 24),
        "band 0 (y=8) and band 1 (y=24) are different colors"
    );
    // Two well-separated bands also differ.
    assert_ne!(
        pixel(&fb, 160, 40),
        pixel(&fb, 160, 200),
        "band 2 (y=40) and band 12 (y=200) are different colors"
    );

    // The band-0/band-1 boundary lands exactly at the 16-line HINT step (with
    // the one-line delivery lag): the last line of band 0 (y=16) still shows
    // band 0's color, the first line of band 1 (y=17) shows band 1's color.
    assert_eq!(band_of(16), 0, "model: y=16 is band 0");
    assert_eq!(band_of(17), 1, "model: y=17 is band 1");
    assert_eq!(pixel(&fb, 160, 16), expected_at(16), "y=16 is still band 0");
    assert_eq!(pixel(&fb, 160, 17), expected_at(17), "y=17 is band 1");
    assert_ne!(
        pixel(&fb, 160, 16),
        pixel(&fb, 160, 17),
        "the band boundary falls between y=16 and y=17"
    );

    check_golden("hint_raster_bands", &fb);
}

// ---------------------------------------------------------------------------
// Scenes 8-10: TRUE MID-LINE raster splits (within a single scanline)
// ---------------------------------------------------------------------------
//
// Unlike `hint_raster_bands` (strictly per-scanline bands driven from a HINT
// handler), these scenes drive a color/scroll change from MAINLINE 68000 code
// running at interrupt mask 0 (< 4). A VDP write issued mid-active-line by the
// mainline is recorded by the VDP at the beam dot where it landed, so the line
// is rendered in spans and a single scanline shows two-or-more states. This is
// the capability that per-line HINT delivery cannot express.
//
// Why mainline and not a HINT handler: mid-line register/CRAM/VSRAM writes are
// recorded ONLY when the 68000 interrupt mask is < 4. Writes at mask >= 4
// (inside a level-4 HINT / level-6 VINT handler) are treated as whole-line by
// design, so that `hint_raster_bands` stays byte-identical. The 68000 resets
// with the mask at 7 (0x2700), so — exactly like a real game before it relies
// on mainline raster timing, and like `hint_raster_bands` — each ROM lowers the
// mask to 0 (`move.w #0x2000, sr`) in its prologue. Its write loop then runs at
// mask 0 throughout, so every write carries a real beam dot.
//
// Each ROM ends in an INFINITE write loop (not a `bra.s *` spin): the loop runs
// across every scanline of every frame, so the captured frame (FRAMES-1) shows
// the split on its active lines. The loop is cycle-counted only implicitly (by
// instruction timing); the exact split dots depend on that timing, so the
// assertions never hard-code a split x — they scan an active line for an
// INTERIOR color transition (a boundary strictly inside the display, not at
// x=0 or x=width-1) and require the line to carry >= 2 distinct colors. The
// blessed golden then locks the exact pixels.

/// Appends `body` as an infinite loop: the body words are emitted verbatim and
/// followed by a `bra.s` back to the first body word. `body` must not itself
/// transfer control out of the loop. The branch displacement is measured from
/// the PC after the branch word (`addr + 2`) back to the loop top, i.e.
/// `-2 * (body.len() + 1)`, and must fit a signed byte (body < 63 words).
fn emit_loop(b: &mut RomBuilder, body: &[u16]) {
    b.emit(body);
    let total_words = body.len() + 1; // include the bra.s word itself
    let disp = -(2 * total_words as isize);
    let d8 = i8::try_from(disp).expect("mid-line loop body too long for bra.s");
    b.emit(&[0x6000 | u16::from(d8 as u8)]);
}

/// Scans row `y` (native `width`) for color transitions whose boundary lies
/// strictly inside the display — a transition between pixel `x-1` and `x` with
/// `2 <= x <= width-2`, so neither adjacent pixel is the extreme left/right
/// edge. Returns the interior boundary x positions. A non-empty result proves a
/// WITHIN-line split (the effect changed part-way across the scanline), which a
/// strictly per-line effect can never produce on an otherwise-uniform line.
fn interior_transitions(fb: &[u8], y: usize, width: usize) -> Vec<usize> {
    let mut xs = Vec::new();
    for x in 2..width - 1 {
        if pixel_w(fb, x, y, width) != pixel_w(fb, x - 1, y, width) {
            xs.push(x);
        }
    }
    xs
}

/// Counts the distinct RGBA colors present on row `y`.
fn distinct_colors_on_row(fb: &[u8], y: usize, width: usize) -> usize {
    let mut seen: Vec<[u8; 4]> = Vec::new();
    for x in 0..width {
        let p = pixel_w(fb, x, y, width);
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen.len()
}

/// Asserts that active line `y` carries a genuine within-line split: at least
/// one interior color transition and at least two distinct colors.
fn assert_midline_split(fb: &[u8], y: usize, width: usize, scene: &str) {
    let transitions = interior_transitions(fb, y, width);
    assert!(
        !transitions.is_empty(),
        "{scene}: no interior color transition on line y={y} — the split did not \
         land mid-line (a per-line effect would leave the line uniform). \
         distinct colors = {}",
        distinct_colors_on_row(fb, y, width)
    );
    let distinct = distinct_colors_on_row(fb, y, width);
    assert!(
        distinct >= 2,
        "{scene}: line y={y} has only {distinct} color(s); a mid-line split must \
         show >= 2"
    );
    // The interior transition definitionally means the pixel left of the split
    // differs from the pixel right of it — the essence of "mid-line".
    let x = transitions[0];
    assert_ne!(
        pixel_w(fb, x - 1, y, width),
        pixel_w(fb, x, y, width),
        "{scene}: interior boundary at x={x} must separate two colors"
    );
}

// ---------------------------------------------------------------------------
// Scene 8: mid-line BACKDROP split (reg 0x07 rewritten as the beam sweeps)
// ---------------------------------------------------------------------------

/// Distinct backdrop colors preloaded into CRAM 0..7. The mainline loop cycles
/// reg 0x07 through indices 0..7 as the beam advances, so a single scanline
/// shows a horizontal sweep through these colors (a "barber pole" that shears
/// diagonally frame-wide because the color counter carries across lines).
const BACKDROP_COLORS: [u16; 8] = [
    0x000E, // red
    0x00E0, // green
    0x0E00, // blue
    0x00EE, // yellow
    0x0E0E, // magenta
    0x0EE0, // cyan
    0x0EEE, // white
    0x0080, // dim green
];

fn build_midline_backdrop_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // H40, backdrop starts at index 0. Display on, no interrupts.
    base_registers(&mut b, 0x81, 0x00);
    // Lower the CPU interrupt mask to 0 (reset leaves it at 7) so the mainline
    // write loop's VDP writes are recorded dot-accurately. move.w #0x2000, sr.
    b.emit(&[0x46FC, 0x2000]);

    // Preload the backdrop palette (CRAM 0..7). No tiles/sprites are written, so
    // every plane pixel is transparent and the backdrop fills the whole screen —
    // reg 0x07 is exactly what is visible.
    for (i, &color) in BACKDROP_COLORS.iter().enumerate() {
        b.set_cram_color(i as u16, color);
    }

    // Seed d0 with the reg-0x07 write command for backdrop index 0 (0x8700).
    b.emit(&[0x303C, 0x8700]); // move.w #0x8700, d0

    // Infinite mainline loop (mask 0): write reg 0x07 = (d0 & 7), then advance
    // the index, wrapping 0..7. Each iteration lands at a later beam dot, so the
    // backdrop color changes several times within every active scanline. a0 is
    // still the VDP control port from the prologue.
    //
    //   move.w d0, (a0)        ; reg 0x07 = current backdrop index
    //   addq.w #1, d0          ; next index
    //   andi.w #0x8707, d0     ; keep it a 0x8700..0x8707 reg-write command
    //   bra.s  loop
    emit_loop(
        &mut b,
        &[
            0x3080, // move.w d0, (a0)
            0x5240, // addq.w #1, d0
            0x0240, 0x8707, // andi.w #0x8707, d0
        ],
    );

    b.finish()
}

#[test]
fn midline_backdrop_split() {
    let rom = build_midline_backdrop_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // The frame is H40 -> 320 wide.
    assert_eq!(fb.len(), FRAME_W * FRAME_H * 4, "H40 backdrop scene is 320x224");

    // Prove a genuine within-line backdrop split on several active lines: each
    // must show an interior transition and multiple backdrop colors. (A per-line
    // effect could only ever paint each line one solid backdrop color.)
    for &y in &[40usize, 100, 160] {
        assert_midline_split(&fb, y, FRAME_W, "midline_backdrop_split");
    }
    // Every color on the line must be one of the preloaded backdrop entries
    // (nothing else is drawn), confirming reg 0x07 is what split.
    let allowed: Vec<[u8; 4]> = BACKDROP_COLORS.iter().map(|&c| normal_rgba(c)).collect();
    for x in 0..FRAME_W {
        let p = pixel(&fb, x, 100);
        assert!(
            allowed.contains(&p),
            "x={x},y=100 backdrop color {p:?} is not a preloaded entry"
        );
    }

    check_golden("midline_backdrop_split", &fb);
}

// ---------------------------------------------------------------------------
// Scene 9: mid-line CRAM palette swap (a plane tile changes color mid-scanline)
// ---------------------------------------------------------------------------

/// Two colors the CRAM entry is toggled between: red and blue.
const CRAM_SWAP_A: u16 = 0x000E; // red
const CRAM_SWAP_B: u16 = 0x0E00; // blue

fn build_midline_cram_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // H40, backdrop black. Display on, no interrupts.
    base_registers(&mut b, 0x81, 0x00);
    // Lower the CPU interrupt mask to 0 (reset leaves it at 7) so the mainline
    // write loop's VDP writes are recorded dot-accurately. move.w #0x2000, sr.
    b.emit(&[0x46FC, 0x2000]);

    // CRAM index 1 starts red; the loop rewrites it live. Tile 1 is solid color
    // index 1, and Scroll A is filled with tile 1 everywhere, so the whole plane
    // shows CRAM[1] — whatever the beam sees it as at each dot.
    b.set_cram_color(1, CRAM_SWAP_A);
    b.write_solid_tile(1, 1);
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // Seed d0 = red; the loop writes it to CRAM[1] then toggles red<->blue.
    b.emit(&[0x303C, CRAM_SWAP_A]); // move.w #CRAM_SWAP_A, d0

    // Infinite mainline loop (mask 0). Each iteration reprograms the CRAM write
    // address to entry 1 (byte 2), writes the current color via the data port,
    // then toggles the color. Because the color changes as the beam advances,
    // the solid plane tile shows red on the left of each write dot and blue on
    // the right (and back), i.e. a within-line palette swap. a0 = control port,
    // a1 = data port (from the prologue).
    //
    //   move.w #0xC002, (a0)   ; CRAM write addr, high command word (index 1)
    //   move.w #0x0000, (a0)   ; ... low command word
    //   move.w d0, (a1)        ; CRAM[1] = current color
    //   eori.w #0x0E0E, d0     ; toggle red (0x000E) <-> blue (0x0E00)
    //   bra.s  loop
    emit_loop(
        &mut b,
        &[
            0x30BC, 0xC002, // move.w #0xC002, (a0)
            0x30BC, 0x0000, // move.w #0x0000, (a0)
            0x3280, // move.w d0, (a1)
            0x0A40, 0x0E0E, // eori.w #0x0E0E, d0
        ],
    );

    b.finish()
}

#[test]
fn midline_cram_swap() {
    let rom = build_midline_cram_rom();
    let fb = run_rom_frames(rom, FRAMES);

    let red = normal_rgba(CRAM_SWAP_A); // [255,0,0,255]
    let blue = normal_rgba(CRAM_SWAP_B); // [0,0,255,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(blue, [0, 0, 255, 255]);

    // The plane tile row must show BOTH colors split at interior x on active
    // lines: a mid-line CRAM rewrite changed the palette entry the solid tile
    // resolves through, part-way across the scanline.
    for &y in &[40usize, 100, 160] {
        assert_midline_split(&fb, y, FRAME_W, "midline_cram_swap");
    }
    // Only the two toggled colors appear (the plane is a single solid tile).
    for x in 0..FRAME_W {
        let p = pixel(&fb, x, 100);
        assert!(
            p == red || p == blue,
            "x={x},y=100 color {p:?} must be red or blue (the toggled CRAM entry)"
        );
    }
    // Both colors are actually present on the line.
    assert!(
        (0..FRAME_W).any(|x| pixel(&fb, x, 100) == red),
        "line y=100 must contain red"
    );
    assert!(
        (0..FRAME_W).any(|x| pixel(&fb, x, 100) == blue),
        "line y=100 must contain blue"
    );

    check_golden("midline_cram_swap", &fb);
}

// ---------------------------------------------------------------------------
// Scene 10: mid-line VERTICAL-SCROLL split (VSRAM rewritten mid-scanline)
// ---------------------------------------------------------------------------
//
// MECHANISM CHOICE: this is a scroll split driven by a mid-line VSRAM write
// (recorded as `LineChange::Vsram`), not an HSCROLL-table write. Stage 1 does
// record a VRAM write that lands on the current line's HSCROLL slot, but an
// HSCROLL split on a horizontally-striped plane produces stripe transitions
// everywhere, which cannot be distinguished from the split itself. A VERTICAL
// scroll change instead re-selects the horizontal band the beam samples: on a
// plane of 8px horizontal color bands, each span between VSRAM writes is a
// single uniform color, so the split shows as ONE clean interior transition
// with uniform regions on either side — an unambiguous, programmatically
// verifiable within-line scroll split. VSRAM writes are recorded cleanly for
// any full-screen-scroll ROM, so this needs no HSCROLL-slot address matching.

/// Colors for the two alternating horizontal bands of the scroll plane.
const VSCROLL_BAND_A: u16 = 0x000E; // red  (even tile rows)
const VSCROLL_BAND_B: u16 = 0x0E00; // blue (odd tile rows)

fn build_midline_vscroll_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // H40, backdrop black. reg 0x0B = 0x00 -> full-screen vertical scroll (a
    // single VSRAM[0] value drives all of Scroll A). Display on, no interrupts.
    base_registers(&mut b, 0x81, 0x00);
    // Lower the CPU interrupt mask to 0 (reset leaves it at 7) so the mainline
    // write loop's VDP writes are recorded dot-accurately. move.w #0x2000, sr.
    b.emit(&[0x46FC, 0x2000]);

    b.set_cram_color(1, VSCROLL_BAND_A); // red
    b.set_cram_color(2, VSCROLL_BAND_B); // blue
    b.write_solid_tile(1, 1); // red tile
    b.write_solid_tile(2, 2); // blue tile

    // Scroll A nametable: 8px horizontal bands — even tile rows red (tile 1),
    // odd tile rows blue (tile 2). A vertical-scroll change of one tile (8px)
    // flips the band a given screen line samples.
    let mut entries = Vec::with_capacity(32 * 32);
    for row in 0..32u16 {
        let tile = if row % 2 == 0 { 0x0001u16 } else { 0x0002u16 };
        for _ in 0..32 {
            entries.push(tile);
        }
    }
    b.write_vram(SCROLL_A_NT, &entries);

    // Seed d0 = 0 (vertical scroll of 0 lines).
    b.emit(&[0x303C, 0x0000]); // move.w #0, d0

    // Infinite mainline loop (mask 0). Each iteration reprograms the VSRAM write
    // address to entry 0, writes the current vertical scroll, then toggles it by
    // 8 lines (one band). Columns the beam has not yet rendered pick up the new
    // scroll, so the sampled band — and thus the color — flips part-way across
    // the scanline. a0 = control port, a1 = data port.
    //
    //   move.w #0x4000, (a0)   ; VSRAM write addr, high command word (index 0)
    //   move.w #0x0010, (a0)   ; ... low command word
    //   move.w d0, (a1)        ; VSRAM[0] = current vertical scroll
    //   eori.w #0x0008, d0     ; toggle scroll 0 <-> 8 (one 8px band)
    //   bra.s  loop
    emit_loop(
        &mut b,
        &[
            0x30BC, 0x4000, // move.w #0x4000, (a0)
            0x30BC, 0x0010, // move.w #0x0010, (a0)
            0x3280, // move.w d0, (a1)
            0x0A40, 0x0008, // eori.w #0x0008, d0
        ],
    );

    b.finish()
}

#[test]
fn midline_vscroll_split() {
    let rom = build_midline_vscroll_rom();
    let fb = run_rom_frames(rom, FRAMES);

    let red = normal_rgba(VSCROLL_BAND_A); // [255,0,0,255]
    let blue = normal_rgba(VSCROLL_BAND_B); // [0,0,255,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(blue, [0, 0, 255, 255]);

    // A mid-line vertical-scroll change re-selects the horizontal band sampled,
    // so the scanline splits into uniform red/blue regions at interior x.
    for &y in &[40usize, 100, 160] {
        assert_midline_split(&fb, y, FRAME_W, "midline_vscroll_split");
    }
    // Only the two band colors appear, and both are present on the line.
    for x in 0..FRAME_W {
        let p = pixel(&fb, x, 100);
        assert!(
            p == red || p == blue,
            "x={x},y=100 color {p:?} must be a band color (red or blue)"
        );
    }
    assert!(
        (0..FRAME_W).any(|x| pixel(&fb, x, 100) == red),
        "line y=100 must contain the red band"
    );
    assert!(
        (0..FRAME_W).any(|x| pixel(&fb, x, 100) == blue),
        "line y=100 must contain the blue band"
    );

    check_golden("midline_vscroll_split", &fb);
}

// ---------------------------------------------------------------------------
// Scene 8: Interlace mode 1 (LSM=01) — same-resolution render path
// ---------------------------------------------------------------------------
//
// Interlace mode 1 (reg 0x0C LSM1:LSM0 = 0b01, i.e. bit 1 set) does NOT change
// the framebuffer geometry: the active area is still H40 320×224 and every tile
// is an 8px/32-byte cell. It only affects the status field flag, the HV counter
// V field bit, and (for double-res mode 2) sprite Y interpretation. This scene
// therefore renders an ordinary plane+sprite scene with LSM=01 set and locks in
// that the render path is byte-for-byte a normal frame — the framebuffer stays
// single-height (320×224), not doubled.

fn build_interlace_mode1_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 (0x81 = RS0|RS1) | LSM0 (0x02) => interlace mode 1.
    base_registers(&mut b, 0x81 | 0x02, 0x00);

    // Colors: blue plane at index 1, green sprite at index 2.
    b.set_cram_color(1, 0x0E00); // blue
    b.set_cram_color(2, 0x00E0); // green

    b.write_solid_tile(1, 1); // blue plane tile (normal 8px cell)
    // 16x16 green sprite: 2x2 tiles column-major (tiles 2..5), normal 8px cells.
    for t in 2..=5 {
        b.write_solid_tile(t, 2);
    }

    // Scroll A: solid blue, low priority, every row.
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // One high-priority green sprite (palette 0, tile 2) at screen (64,64),
    // 16x16 px. In mode 1 the sprite Y offset is the classic -128.
    write_sprite(&mut b, 0, 64 + 128, 1, 1, 0, 0x8000 | 2, 64 + 128);

    b.finish()
}

#[test]
fn interlace_mode1_render() {
    let rom = build_interlace_mode1_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // Mode 1 keeps normal resolution: NOT doubled. A doubled (mode-2) weave
    // would make this 320*448*4 = 573440 bytes; asserting the single height
    // proves LSM=01 did not reshape the framebuffer.
    assert_eq!(
        fb.len(),
        FRAME_W * FRAME_H * 4,
        "interlace mode 1 keeps single-height 320x224 RGBA (not doubled)"
    );

    let blue = normal_rgba(0x0E00); // [0,0,255,255]
    let green = normal_rgba(0x00E0); // [0,255,0,255]
    assert_eq!(blue, [0, 0, 255, 255]);
    assert_eq!(green, [0, 255, 0, 255]);

    // Plane and sprite render exactly as a normal frame.
    assert_eq!(pixel(&fb, 8, 8), blue, "plane background is blue");
    assert_eq!(pixel(&fb, 200, 200), blue, "plane background is blue");
    assert_eq!(pixel(&fb, 70, 70), green, "sprite draws green over plane");
    assert_eq!(pixel(&fb, 80, 70), blue, "just right of sprite is blue");

    check_golden("interlace_mode1", &fb);
}

// ---------------------------------------------------------------------------
// Scene 9: Interlace mode 2 (LSM=11) — double-res field weave
// ---------------------------------------------------------------------------
//
// Interlace mode 2 (reg 0x0C LSM1:LSM0 = 0b11, i.e. bits 2 AND 1 set) weaves
// BOTH fields into one framebuffer at DOUBLE vertical resolution: NTSC V28
// (224 lines) becomes 448 physical rows (out row 2*line = field 0, 2*line+1 =
// field 1). Planes use 16px-tall cells backed by 64-byte (16-row) tiles; the
// tile row is `out_y >> 4` and the row-in-cell is `out_y & 15`.
//
// This scene uses H40 (320 wide) so the framebuffer is 320×448. Scroll A is
// filled with a distinctive two-tone 16px cell: rows 0..7 red, rows 8..15
// green. Under the doubled addressing that produces horizontal bands that
// repeat every 16 output rows (8 red, 8 green), which is only visible if the
// 16px-cell weave is correct. A high-priority blue sprite (mode-2 Y = SAT_Y -
// 0x100, 16px cell, 64-byte tile) sits entirely in the LOWER weave half
// (out_y 228..244) to prove content beyond the single-field 224-line height.

/// H40 display width for the mode-2 scene.
const M2_W: usize = 320;
/// Doubled active height for NTSC V28 in interlace mode 2 (224 * 2).
const M2_H: usize = 448;

fn build_interlace_mode2_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // reg 0x0C = H40 (0x81) | LSM1 (0x04) | LSM0 (0x02) = 0x87 => interlace mode 2.
    base_registers(&mut b, 0x81 | 0x06, 0x00);

    // Palette: red (1), green (2), blue (3).
    b.set_cram_color(1, 0x000E); // red
    b.set_cram_color(2, 0x00E0); // green
    b.set_cram_color(3, 0x0E00); // blue

    // Plane tile 1 as a 16-row / 64-byte interlace-mode-2 cell: rows 0..7 solid
    // color 1 (red), rows 8..15 solid color 2 (green). Mode-2 tile stride is 64
    // bytes, so tile index 1 lives at VRAM byte 1*64 = 0x40. Each row is two
    // words; a solid color C row is the word 0xCCCC repeated.
    let mut plane_tile = Vec::with_capacity(32);
    for r in 0..16u16 {
        let w: u16 = if r < 8 { 0x1111 } else { 0x2222 };
        plane_tile.push(w);
        plane_tile.push(w);
    }
    b.write_vram(1 * 64, &plane_tile);

    // Sprite tile 4 as a 16-row / 64-byte solid color 3 (blue) cell at VRAM byte
    // 4*64 = 0x100.
    let sprite_tile = vec![0x3333u16; 32]; // 16 rows * 2 words
    b.write_vram(4 * 64, &sprite_tile);

    // Scroll A nametable: tile 1 everywhere, low priority, palette 0.
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // One high-priority blue sprite (1x1 cell = 8x16px in the doubled space),
    // palette 0, tile 4. Mode-2 sprite Y = (SAT_Y & 0x3FF) - 0x100, so SAT_Y =
    // 228 + 256 = 484 puts its top at out_y 228 (lower weave half); it spans
    // out_y 228..244. x = 100 => x_raw = 100 + 128 = 228.
    write_sprite(&mut b, 0, 228 + 256, 0, 0, 0, 0x8000 | 4, 100 + 128);

    b.finish()
}

#[test]
fn interlace_mode2_double_res() {
    let rom = build_interlace_mode2_rom();
    let fb = run_rom_frames(rom, FRAMES);

    // Mode 2 weaves both fields: the framebuffer is DOUBLE height (320x448).
    assert_eq!(
        fb.len(),
        M2_W * M2_H * 4,
        "interlace mode 2 weaves both fields into a 320x448 RGBA buffer (573440 bytes)"
    );

    // Local pixel accessor for the doubled 320x448 buffer (the shared `pixel`
    // helper caps y < 224, which the lower weave half exceeds).
    let px = |x: usize, y: usize| -> [u8; 4] {
        assert!(x < M2_W && y < M2_H, "px ({x},{y}) out of bounds");
        let o = (y * M2_W + x) * 4;
        [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
    };

    let red = normal_rgba(0x000E); // [255,0,0,255]
    let green = normal_rgba(0x00E0); // [0,255,0,255]
    let blue = normal_rgba(0x0E00); // [0,0,255,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(green, [0, 255, 0, 255]);
    assert_eq!(blue, [0, 0, 255, 255]);

    // UPPER weave half (y < 224): the 16px cell shows red on rows 0..7 and green
    // on rows 8..15, repeating every 16 output rows. y=4 -> red, y=12 -> green.
    assert_eq!(px(8, 4), red, "upper half y=4 (row-in-cell 4) is red");
    assert_eq!(px(8, 12), green, "upper half y=12 (row-in-cell 12) is green");
    // Full H40 width is covered (x=300 wraps the 256px plane back onto tile 1).
    assert_eq!(px(300, 4), red, "upper half far-right column (x=300) is red");

    // LOWER weave half (y >= 224): proves the second field produced real content
    // beyond the single-field 224-line height. y%16 selects the band.
    assert_eq!(px(8, 228), red, "lower half y=228 (228%16=4) is red");
    assert_eq!(px(8, 236), green, "lower half y=236 (236%16=12) is green");
    assert_eq!(px(8, 440), green, "lower half y=440 (440%16=8) is green");
    // The lower half is NOT blank/backdrop (backdrop is black) — a broken weave
    // that only rendered field 0 would leave these rows black.
    assert_ne!(px(8, 228), [0, 0, 0, 255], "lower half must not be black backdrop");
    assert_ne!(px(8, 440), [0, 0, 0, 255], "last woven row must not be black backdrop");

    // Sprite in the LOWER weave half (out_y 228..244, x 100..107): mode-2 sprite
    // addressing (Y - 0x100, 16px cell, 64-byte tile) draws high-priority blue
    // over the green plane band.
    assert_eq!(px(103, 232), blue, "mode-2 sprite draws blue in the lower weave half");
    // Just outside the sprite (same row, different x) is the plane band, not blue.
    assert_eq!(px(8, 232), green, "outside the sprite the lower-half plane band shows");

    check_golden("interlace_mode2", &fb);
}

// ---------------------------------------------------------------------------
// Scene 10: Interlace mode switch — dimension change at the frame boundary
// ---------------------------------------------------------------------------
//
// A reg 0x0C LSM change is latched at scanline 0 of the NEXT frame (like the
// H40/H32 width latch), so switching interlace mode reshapes the framebuffer at
// the frame boundary. This scene proves the framebuffer length tracks the mode:
//
//   * a pure mode-2 ROM yields a doubled 320×448 buffer;
//   * a ROM that starts in mode 2 then writes reg 0x0C = non-interlace (0x81)
//     at the end of setup yields a single-height 320×224 buffer for the final
//     captured frame.
//
// Asserting the two lengths differ (and equal their expected mode geometry)
// locks in that the switch actually changed the framebuffer dimensions.

fn build_interlace_switch_rom() -> Vec<u8> {
    let mut b = RomBuilder::new();
    // Start in interlace mode 2 (H40 | LSM11 = 0x87).
    base_registers(&mut b, 0x81 | 0x06, 0x00);

    // Red plane using a NORMAL 8px/32-byte tile so the FINAL (non-interlace)
    // frame renders a visible red plane. (tile 1 at VRAM byte 1*32 = 0x20.)
    b.set_cram_color(1, 0x000E); // red
    b.write_solid_tile(1, 1);
    let row_a = [0x0001u16; 32];
    fill_nametable_32(&mut b, SCROLL_A_NT, &row_a);

    // Switch OFF interlace at the end of the setup stream: reg 0x0C = H40 only
    // (0x81, LSM=00). Latched at the next frame boundary, so the captured frame
    // is a single-height non-interlaced 320x224 frame.
    b.set_register(0x0C, 0x81);

    b.finish()
}

#[test]
fn interlace_mode_switch() {
    // Reference: pure mode-2 ROM produces a doubled framebuffer.
    let doubled = run_rom_frames(build_interlace_mode2_rom(), FRAMES);
    assert_eq!(
        doubled.len(),
        M2_W * M2_H * 4,
        "pure mode-2 frame is doubled height (320x448)"
    );

    // Switched: mode 2 -> non-interlace collapses to single height for the final
    // captured frame.
    let switched = run_rom_frames(build_interlace_switch_rom(), FRAMES);
    assert_eq!(
        switched.len(),
        FRAME_W * FRAME_H * 4,
        "after switching OFF interlace the final frame is single-height 320x224"
    );

    // The switch actually changed the framebuffer dimensions at the boundary.
    assert_ne!(
        doubled.len(),
        switched.len(),
        "the interlace mode switch must change the framebuffer length (448 vs 224 rows)"
    );

    // The final non-interlaced frame renders the red plane (8px tile path).
    let red = normal_rgba(0x000E); // [255,0,0,255]
    assert_eq!(red, [255, 0, 0, 255]);
    assert_eq!(pixel(&switched, 8, 8), red, "final frame plane is red at left");
    assert_eq!(
        pixel(&switched, 300, 200),
        red,
        "final frame plane is red across the H40 width"
    );

    check_golden("interlace_mode_switch", &switched);
}

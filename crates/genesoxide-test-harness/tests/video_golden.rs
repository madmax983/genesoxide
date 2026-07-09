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
use genesoxide_test_harness::rom_builder::RomBuilder;
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
    // reg 0x0C = H40 only (S/H off). Backdrop black.
    base_registers(&mut b, 0x01, 0x00);
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

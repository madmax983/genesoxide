# Playable Sonic: Milestones 2 & 3 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make Sonic the Hedgehog playable by a human at correct speed with visible HUD, correct visual effects, and proper hardware emulation.

**Architecture:** Fix the two critical blockers (frame rate limiting + window plane), then layer on H-interrupt delivery, HV counter, I/O register fixes, and VDP status completeness. Each task is independently testable.

**Tech Stack:** Rust, winit, pixels, std::time for frame pacing, existing genesoxide-core VDP/CPU infrastructure.

---

## Milestone 2: Human-Playable at Correct Speed

### Task 1: Frame Rate Limiter (59.92 Hz)

The game currently runs at unlimited speed because `about_to_wait()` requests redraws continuously with no pacing. This is THE critical blocker — without it, input is meaningless.

**Files:**
- Modify: `crates/genesoxide-desktop/src/main.rs`

**Step 1: Write a frame timing test (manual verification)**

We can't unit-test vsync in a windowed app, but we can add a frame time diagnostic. Add a field to `App`:

```rust
struct App {
    core: GenesisCore,
    scale: u32,
    window: Option<Window>,
    pixels: Option<Pixels<'static>>,
    last_frame_time: Option<std::time::Instant>,
    frame_duration: std::time::Duration,
}
```

Initialize in `cmd_run`:
```rust
let mut app = App {
    core,
    scale,
    window: None,
    pixels: None,
    last_frame_time: None,
    frame_duration: std::time::Duration::from_nanos(16_686_116), // 1/59.92 Hz
};
```

**Step 2: Implement frame pacing in `about_to_wait`**

Replace the unconditional redraw request with sleep-based pacing:

```rust
fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
    if let Some(window) = &self.window {
        let now = std::time::Instant::now();
        if let Some(last) = self.last_frame_time {
            let elapsed = now.duration_since(last);
            if elapsed < self.frame_duration {
                std::thread::sleep(self.frame_duration - elapsed);
            }
        }
        window.request_redraw();
    }
}
```

**Step 3: Update `RedrawRequested` to record frame time**

In the `WindowEvent::RedrawRequested` handler, after stepping the frame:

```rust
WindowEvent::RedrawRequested => {
    self.core.execute(Command::StepFrame);
    self.last_frame_time = Some(std::time::Instant::now());
    // ... rest of render code ...
}
```

**Step 4: Run the emulator and verify frame timing**

Run: `cargo run -p genesoxide-desktop -- run <sonic_rom> --scale 3`
Expected: Game runs at approximately real-time speed. Sonic's demo should take ~30 seconds, not 1-2 seconds.

**Step 5: Commit**

```bash
git add crates/genesoxide-desktop/src/main.rs
git commit -m "feat(desktop): add 59.92 Hz frame rate limiter"
```

---

### Task 2: Window Plane Rendering

Sonic uses the window plane for the HUD (score, rings, time, lives). Without it, the player has no feedback. The VDP already has `window_nametable_addr()` but `render_scanline()` never calls it.

**Files:**
- Modify: `crates/genesoxide-core/src/vdp.rs`

**Step 1: Write a failing test for window plane rendering**

Add to vdp.rs tests:

```rust
#[test]
fn window_plane_renders_over_scroll_a() {
    let mut vdp = setup_vdp_for_rendering();

    vdp.cram[1] = 0x000E; // red (scroll A)
    vdp.cram[2] = 0x00E0; // green (window)

    // Tile 1: solid red (for Scroll A)
    write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);
    // Tile 2: solid green (for window)
    write_tile_pattern(&mut vdp, 2, &[[2u8; 8]; 8]);

    // Scroll A: tile 1 everywhere
    let nt_a = vdp.scroll_a_nametable_addr();
    vram_write_word(&mut vdp, nt_a, 0x0001);

    // Window nametable at a different address
    // Register 0x03: window nametable. Bits 5-1 * 0x800.
    // Use 0x28 -> bits 5-1 = 0b10100 = 20 * 0x800 = 0xA000
    vdp.registers[0x03] = 0x28;
    let wnd_base = (0x28 & 0x3E) as usize * 0x400; // 0xA000

    // Window: tile 2 at position (0,0)
    vram_write_word(&mut vdp, wnd_base, 0x0002);

    // Window covers right side: register 0x11 bit 7 = 1 (right),
    // bits 4-0 = 1 (from cell 1 onwards, i.e. pixel 8+)
    // Actually for full coverage: set window to cover from column 0
    // Register 0x11 = 0x00 means window not active (0 cells from left)
    // Register 0x11 = 0x80 | 0x00 means window from right, 0 cells = no window
    // For simplicity: window down from row 0
    // Register 0x12: vertical window position
    // Bit 7 = 0: window is above the split line
    // Bits 4-0 = cell row (window fills from top to this row)
    // Set to 0x1F (31) = window fills entire screen vertically
    vdp.registers[0x12] = 0x1F;
    // Register 0x11: horizontal window position
    // Bit 7 = 0: window on left side, bits 4-0 = 20 (cells) = 160 pixels
    // This means: left 160 pixels = window, right 160 pixels = scroll A
    vdp.registers[0x11] = 0x14; // 20 cells from left

    vdp.render_scanline(0);

    // Pixel 0 should be green (window)
    let green = Vdp::color_to_rgba(0x00E0);
    assert_eq!(&vdp.framebuffer[0..4], &green, "window pixel at x=0");

    // Pixel 160 should be red (scroll A, outside window)
    let red = Vdp::color_to_rgba(0x000E);
    let offset = 160 * 4;
    assert_eq!(&vdp.framebuffer[offset..offset + 4], &red, "scroll A pixel at x=160");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p genesoxide-core -- window_plane_renders`
Expected: FAIL — window plane not rendered, pixel 0 will be red (scroll A) not green.

**Step 3: Implement window plane rendering**

In `render_scanline()`, between the Scroll A loop (Step 3) and sprites (Step 4), add window plane logic. The window plane replaces Scroll A in its active region:

```rust
// Step 3b: Window plane (replaces Scroll A where active)
let wnd_base = self.window_nametable_addr();
let (wnd_left, wnd_right) = self.window_h_range(width);
let (wnd_top, wnd_bottom) = self.window_v_range();

if line >= wnd_top && line < wnd_bottom {
    // Window layout uses a fixed 32 or 64-cell wide nametable
    let wnd_cells_wide: u16 = if width == 320 { 64 } else { 32 };
    let row_in_window = line - wnd_top;
    let tile_row = row_in_window / 8;
    let pixel_row_in_tile = (row_in_window % 8) as u8;

    for x in wnd_left..wnd_right {
        let tile_col = x / 8;
        let nt_offset = (tile_row * wnd_cells_wide + tile_col) as usize * 2;
        let nt_addr = wnd_base + nt_offset;
        let entry = self.vram_read_word(nt_addr);

        let priority = entry & 0x8000 != 0;
        let palette = ((entry >> 13) & 0x03) as u8;
        let vflip = entry & 0x1000 != 0;
        let hflip = entry & 0x0800 != 0;
        let tile_index = entry & 0x07FF;

        let col_in_tile = (x % 8) as u8;
        let color_index = self.tile_pixel(tile_index, pixel_row_in_tile, col_in_tile, hflip, vflip);

        let xi = x as usize;
        if color_index == 0 {
            // Transparent window pixel — DON'T show scroll A underneath.
            // Window transparent pixels show the backdrop or scroll B.
            // (Already handled by previous layers)
            continue;
        }
        let pri_level = if priority { 2 } else { 1 };
        if pri_level >= pixel_priority[xi] {
            pixel_color[xi] = self.resolve_color(palette, color_index);
            pixel_priority[xi] = pri_level;
        }
    }
}
```

Add helper methods:

```rust
/// Returns the horizontal pixel range where the window plane is active.
/// Returns (left_pixel, right_pixel) — the window fills this range.
#[must_use]
fn window_h_range(&self, screen_width: u16) -> (u16, u16) {
    let reg = self.registers[0x11];
    let cells = u16::from(reg & 0x1F);
    let pixels = cells * 8; // could exceed screen_width, will be clamped
    if reg & 0x80 != 0 {
        // Window on the right side, from `pixels` to screen edge
        (pixels.min(screen_width), screen_width)
    } else {
        // Window on the left side, from 0 to `pixels`
        (0, pixels.min(screen_width))
    }
}

/// Returns the vertical scanline range where the window plane is active.
/// Returns (top_line, bottom_line).
#[must_use]
fn window_v_range(&self) -> (u16, u16) {
    let reg = self.registers[0x12];
    let cells = u16::from(reg & 0x1F);
    let lines = cells * 8;
    if reg & 0x80 != 0 {
        // Window below the split line
        (lines, 224)
    } else {
        // Window above the split line
        (0, lines)
    }
}
```

Also need to modify the Scroll A loop to skip pixels inside the window region, since the window replaces Scroll A:

```rust
// Step 3: Scroll A (skip window region)
for x in 0..width {
    // Skip if this pixel is in the window region
    if line >= wnd_top && line < wnd_bottom && x >= wnd_left && x < wnd_right {
        continue;
    }
    // ... existing scroll A code ...
}
```

This requires computing wnd_left/wnd_right/wnd_top/wnd_bottom before the Scroll A loop.

**Step 4: Run test to verify it passes**

Run: `cargo test -p genesoxide-core -- window_plane_renders`
Expected: PASS

**Step 5: Add edge case test — window disabled**

```rust
#[test]
fn window_plane_disabled_shows_scroll_a() {
    let mut vdp = setup_vdp_for_rendering();

    vdp.cram[1] = 0x000E; // red
    write_tile_pattern(&mut vdp, 1, &[[1u8; 8]; 8]);

    let nt_a = vdp.scroll_a_nametable_addr();
    vram_write_word(&mut vdp, nt_a, 0x0001);

    // Window registers at 0 = no window coverage
    vdp.registers[0x11] = 0x00;
    vdp.registers[0x12] = 0x00;

    vdp.render_scanline(0);

    let red = Vdp::color_to_rgba(0x000E);
    assert_eq!(&vdp.framebuffer[0..4], &red);
}
```

Run: `cargo test -p genesoxide-core -- window_plane_disabled`
Expected: PASS (window is 0 cells, so scroll A renders everywhere)

**Step 6: Commit**

```bash
git add crates/genesoxide-core/src/vdp.rs
git commit -m "feat(vdp): implement window plane rendering with H/V split"
```

---

### Task 3: I/O Version Register Fix

Reading 0xA10001 currently returns controller data instead of the hardware version/region byte. This affects region detection.

**Files:**
- Modify: `crates/genesoxide-core/src/api.rs`
- Modify: `crates/genesoxide-core/src/io.rs`

**Step 1: Write a failing test**

In api.rs tests:

```rust
#[test]
fn io_version_register_returns_region() {
    let core = GenesisCore::new();
    // 0xA10001 should return version/region, not controller data
    // Default: overseas NTSC = 0x80 (bit 7 = overseas)
    let val = core.read_byte(0xA10001);
    // Should NOT be 0x7F (all buttons released) — it should be version info
    assert_ne!(val, 0x7F, "0xA10001 should return version register, not controller data");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p genesoxide-core -- io_version_register`
Expected: FAIL

**Step 3: Fix the I/O register mapping**

The Genesis I/O register layout (0xA10000-0xA1001F) uses odd addresses for reads:
- 0xA10001: Version register (hardware model, NTSC/PAL, overseas/domestic)
- 0xA10003: Port 1 data
- 0xA10005: Port 2 data
- 0xA10007: EXP port data
- 0xA10009: Port 1 control
- 0xA1000B: Port 2 control

Current code maps reg 0x01 to port1.read_data(), but the real mapping is different. Fix the byte read:

```rust
bus::BusRegion::IoRegisters => {
    let reg = (addr & 0x1F) as u8;
    match reg {
        0x00 | 0x01 => {
            // Version register: bit 7 = overseas, bit 6 = PAL, bits 3-0 = revision
            // Default: overseas NTSC revision 0 = 0xA0
            0xA0
        }
        0x02 | 0x03 => self.port1.read_data(),
        0x04 | 0x05 => self.port2.read_data(),
        0x08 | 0x09 => self.port1.read_ctrl(),
        0x0A | 0x0B => self.port2.read_ctrl(),
        _ => 0,
    }
}
```

Do the same fix in `CoreBus::read_byte()` and `CoreBus::read_word()`.

**Step 4: Run test to verify it passes**

Run: `cargo test -p genesoxide-core -- io_version_register`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/genesoxide-core/src/api.rs
git commit -m "fix(io): return version register from 0xA10001 instead of controller data"
```

---

### Task 4: Z80 Bus Request Stub

Games write to 0xA11100 to request the Z80 bus, then poll until granted. Without a response, Sonic may hang during initialization.

**Files:**
- Modify: `crates/genesoxide-core/src/api.rs`

**Step 1: Write a failing test**

```rust
#[test]
fn z80_bus_request_grants_immediately() {
    let mut core = GenesisCore::new();
    core.execute(Command::LoadRom(vec![0; 1024]));

    // Write bus request
    let mut bus = CoreBus { /* ... */ };
    // We need to test via the public API. Write 0x0100 to 0xA11100.
    // Then read 0xA11100 — bit 0 should be 1 (bus granted).

    // For now, test that reading ControlRegisters returns bus-granted
    let val = core.read_byte(0xA11100);
    assert_eq!(val & 0x01, 0x01, "Z80 bus should be granted (Z80 not present)");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p genesoxide-core -- z80_bus_request`
Expected: FAIL — currently returns 0

**Step 3: Handle ControlRegisters in bus reads/writes**

Add handling for the ControlRegisters region in both `GenesisCore::read_byte` and `CoreBus`:

```rust
bus::BusRegion::ControlRegisters => {
    let reg_offset = addr & 0x01FF;
    match reg_offset {
        0x0000..=0x0001 => {
            // Z80 bus request: always grant (Z80 not emulated)
            // Bit 0 = 1 means bus is available to 68K
            0x01
        }
        0x0100..=0x0101 => {
            // Z80 reset: acknowledge
            0x00
        }
        _ => 0,
    }
}
```

For writes, just absorb them silently:

```rust
bus::BusRegion::ControlRegisters => {
    // Z80 bus request/reset — absorbed (Z80 not emulated)
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p genesoxide-core -- z80_bus_request`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/genesoxide-core/src/api.rs
git commit -m "fix(bus): stub Z80 bus request/reset to prevent hangs"
```

---

## Milestone 3: Visual Correctness

### Task 5: H-Interrupt Delivery

Sonic uses level 4 H-interrupts for palette swaps (water effects in Labyrinth Zone) and animation timing. The VDP tracks the counter but never fires the interrupt.

**Files:**
- Modify: `crates/genesoxide-core/src/vdp.rs`
- Modify: `crates/genesoxide-core/src/api.rs`

**Step 1: Write a test for H-interrupt counter behavior**

```rust
#[test]
fn h_interrupt_counter_fires_at_zero() {
    let mut vdp = Vdp::new();
    // H-interrupt register: fire every 4 scanlines
    vdp.registers[0x0A] = 0x03; // counter value 3 -> fires on 4th line
    // Enable H-interrupt: register 0, bit 4
    vdp.registers[0] = 0x10;

    // Simulate scanlines
    vdp.begin_scanline(0); // counter loaded with 3
    assert!(!vdp.h_interrupt_pending());

    vdp.begin_scanline(1); // counter = 2
    assert!(!vdp.h_interrupt_pending());

    vdp.begin_scanline(2); // counter = 1
    assert!(!vdp.h_interrupt_pending());

    vdp.begin_scanline(3); // counter = 0 -> fire!
    assert!(vdp.h_interrupt_pending());
    vdp.clear_h_interrupt(); // CPU acknowledges

    vdp.begin_scanline(4); // counter reloaded with 3
    assert!(!vdp.h_interrupt_pending());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p genesoxide-core -- h_interrupt_counter_fires`
Expected: FAIL — `h_interrupt_pending()` doesn't exist yet

**Step 3: Implement H-interrupt counter logic in VDP**

Add field and methods to Vdp:

```rust
// In Vdp struct:
h_interrupt_pending: bool,

// Methods:
#[must_use]
pub fn h_interrupt_pending(&self) -> bool {
    self.h_interrupt_pending
}

pub fn clear_h_interrupt(&mut self) {
    self.h_interrupt_pending = false;
}
```

Update `begin_scanline()`:

```rust
pub fn begin_scanline(&mut self, line: u16) {
    self.scanline = line;
    self.in_hblank = true;

    if line == 0 {
        self.h_interrupt_counter = i16::from(self.registers[0x0A]);
    } else if line < 224 {
        self.h_interrupt_counter -= 1;
        if self.h_interrupt_counter < 0 {
            self.h_interrupt_counter = i16::from(self.registers[0x0A]);
            // Fire H-interrupt if enabled (register 0, bit 4)
            if self.registers[0] & 0x10 != 0 {
                self.h_interrupt_pending = true;
            }
        }
    }
}
```

**Step 4: Deliver H-interrupt in step_frame**

In `api.rs`, after `step_scanline()` and before `render_scanline()`:

```rust
// Check for H-interrupt
if self.vdp.h_interrupt_pending() {
    self.vdp.clear_h_interrupt();
    let mut bus = CoreBus { /* ... */ };
    let cycles = cpu::deliver_interrupt(&mut self.cpu, &mut bus, 4);
    self.cpu.cycles += u64::from(cycles);
    self.scheduler.advance_cpu(u64::from(cycles));
}
```

**Step 5: Run test to verify it passes**

Run: `cargo test -p genesoxide-core -- h_interrupt_counter_fires`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/genesoxide-core/src/vdp.rs crates/genesoxide-core/src/api.rs
git commit -m "feat(vdp): implement H-interrupt (level 4) delivery"
```

---

### Task 6: HV Counter

Games read the HV counter at 0xC00008 for timing. Currently returns 0. The H counter is less critical but the V counter (current scanline) is important.

**Files:**
- Modify: `crates/genesoxide-core/src/vdp.rs`
- Modify: `crates/genesoxide-core/src/api.rs`

**Step 1: Write a failing test**

```rust
#[test]
fn hv_counter_reflects_scanline() {
    let mut vdp = Vdp::new();
    vdp.begin_scanline(42);
    let hv = vdp.read_hv_counter();
    // V counter should be 42 in the high byte
    assert_eq!((hv >> 8) as u8, 42);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p genesoxide-core -- hv_counter_reflects`
Expected: FAIL

**Step 3: Implement HV counter**

Add to VDP:

```rust
/// Returns the current HV counter value.
/// High byte = V counter (scanline number, 0-261 for NTSC).
/// Low byte = H counter (horizontal position, approximated).
#[must_use]
pub fn read_hv_counter(&self) -> u16 {
    // V counter: scanline number, wraps at specific points for NTSC
    let v = if self.scanline <= 0xEA {
        self.scanline as u8
    } else {
        // NTSC V counter jumps from 0xEA to 0xE5 in the blanking area
        (self.scanline.wrapping_sub(6)) as u8
    };

    // H counter: we approximate based on hblank state
    // During H-blank, counter is near end of line (~0xE4-0xFF)
    // During active, it's proportional to dot position
    let h: u8 = if self.in_hblank { 0xE4 } else { 0x08 };

    (u16::from(v) << 8) | u16::from(h)
}
```

Update CoreBus::read_word for VDP:

```rust
0x08 | 0x0A | 0x0C | 0x0E => {
    self.vdp.read_hv_counter()
}
```

Note: `read_hv_counter` needs `&self` only, but CoreBus holds `&mut self.vdp`. This is fine — we just call the method.

**Step 4: Run test to verify it passes**

Run: `cargo test -p genesoxide-core -- hv_counter_reflects`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/genesoxide-core/src/vdp.rs crates/genesoxide-core/src/api.rs
git commit -m "feat(vdp): implement HV counter reads"
```

---

### Task 7: VDP Status Register Completeness

Add missing flags: FIFO empty (always set for now), sprite overflow, odd frame.

**Files:**
- Modify: `crates/genesoxide-core/src/vdp.rs`

**Step 1: Write a failing test**

```rust
#[test]
fn status_register_fifo_empty_set() {
    let vdp = Vdp::new();
    // Bit 9 should be set (FIFO empty)
    assert_ne!(vdp.read_status() & 0x0200, 0, "FIFO empty bit should be set");
}

#[test]
fn status_register_odd_frame_toggles() {
    let mut vdp = Vdp::new();
    let status1 = vdp.read_status();
    vdp.end_frame();
    let status2 = vdp.read_status();
    // Bit 4 should toggle between frames
    assert_ne!(status1 & 0x0010, status2 & 0x0010, "odd frame bit should toggle");
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p genesoxide-core -- status_register_fifo status_register_odd`
Expected: FAIL

**Step 3: Implement missing status bits**

Add field: `odd_frame: bool` to Vdp struct.

Update `read_status()`:

```rust
pub fn read_status(&self) -> u16 {
    let mut status: u16 = 0x3400;
    // Bit 9: FIFO empty (always set — no FIFO emulation yet)
    status |= 0x0200;
    // Bit 3: V-blank
    if self.in_vblank { status |= 0x0008; }
    // Bit 4: Odd frame
    if self.odd_frame { status |= 0x0010; }
    // Bit 2: H-blank
    if self.in_hblank { status |= 0x0004; }
    status
}
```

Toggle odd_frame in `end_frame()`:

```rust
pub fn end_frame(&mut self) {
    self.scanline = 0;
    self.in_hblank = false;
    self.odd_frame = !self.odd_frame;
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p genesoxide-core -- status_register_fifo status_register_odd`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/genesoxide-core/src/vdp.rs
git commit -m "feat(vdp): add FIFO empty and odd frame bits to status register"
```

---

### Task 8: Verify Sonic Gameplay

End-to-end verification that a human can play Green Hill Zone.

**Files:**
- Modify: `crates/genesoxide-test-harness/tests/sonic_boot.rs`

**Step 1: Add a multi-frame diagnostic test**

```rust
#[test]
fn sonic_renders_hud() {
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

    // Skip past SEGA logo (~180 frames) and title screen (~300 frames)
    // Press Start to begin game
    for _ in 0..200 {
        core.execute(Command::StepFrame);
    }
    core.execute(Command::PressButton { port: 0, button: genesoxide_core::Button::Start });
    core.execute(Command::StepFrame);
    core.execute(Command::ReleaseButton { port: 0, button: genesoxide_core::Button::Start });

    // Run a few more frames into the game
    for _ in 0..60 {
        core.execute(Command::StepFrame);
    }

    let fb = core.framebuffer_rgba();
    let non_black: usize = fb.chunks(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    // The HUD should have rendered something in the top-left area
    // Check the window region (top 32 pixels) for non-background pixels
    let top_area_pixels: usize = fb[..320 * 32 * 4]
        .chunks(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    eprintln!("Total non-black pixels: {non_black}");
    eprintln!("Top 32 rows non-black: {top_area_pixels}");

    assert!(non_black > 1000, "Frame should have substantial rendered content");
    assert!(top_area_pixels > 100, "HUD area should have visible content (window plane)");
}
```

**Step 2: Run the test**

Run: `cargo test -p genesoxide-test-harness --test sonic_boot -- sonic_renders_hud --nocapture`
Expected: PASS with visible HUD content

**Step 3: Manual play test**

Run: `cargo run -p genesoxide-desktop -- run <sonic_rom> --scale 3`
Verify:
- Game runs at real-time speed (~60fps)
- SEGA logo appears, fades, title screen shows
- Press Enter to start, game enters Green Hill Zone
- Arrow keys move Sonic, Z = jump
- HUD shows score, time, rings at top of screen
- Sonic responds to input at appropriate speed

**Step 4: Commit**

```bash
git add crates/genesoxide-test-harness/tests/sonic_boot.rs
git commit -m "test: add Sonic HUD rendering verification test"
```

---

## Summary

| Task | Milestone | Priority | Description |
|------|-----------|----------|-------------|
| 1 | M2 | CRITICAL | Frame rate limiter (59.92 Hz) |
| 2 | M2 | CRITICAL | Window plane rendering |
| 3 | M2 | MEDIUM | I/O version register fix |
| 4 | M2 | LOW | Z80 bus request stub |
| 5 | M3 | HIGH | H-interrupt delivery |
| 6 | M3 | MEDIUM | HV counter implementation |
| 7 | M3 | MEDIUM | VDP status register completeness |
| 8 | M3 | VERIFY | End-to-end Sonic gameplay test |

# genesoxide-test-harness

ROM-based integration tests for the genesoxide Genesis / Mega Drive core:
CPU/Z80 instruction suites, audio (VGM) checks, and headless golden-frame
**video** tests.

## Video golden-frame harness

The video harness proves the emulator renders known VDP scenes correctly, then
locks that output against regressions. It has three pieces:

| Piece | Location | Role |
|-------|----------|------|
| ROM builder | `src/rom_builder.rs` | Emits tiny, freely-licensed 68000 test ROMs (reset vectors + a flat stream of VDP register/VRAM/CRAM writes + an infinite self-loop). |
| Golden tests | `tests/video_golden.rs` | Build each scene ROM, run it through the real emulator, assert known pixels, and compare the whole framebuffer against a golden. |
| Goldens | `tests/goldens/*.rgba` | Raw RGBA framebuffers, 286720 bytes each (320 x 224 x 4). |

### How it works

Each scene test:

1. Builds a ROM with `RomBuilder` (VDP register setup, tile patterns, palette
   entries, nametable fills, sprite table entries).
2. Runs it with `run_rom_frames(rom, N)`. Frame 0 executes the setup program
   (which then spins forever); a later frame renders the fully-built VRAM.
3. **Asserts specific pixels** against values derived from the VDP rules
   (shadow/highlight intensities, the 16px/unit window boundary, normal
   compositing). These assertions are the real proof of correctness and run in
   both normal and BLESS mode — a blessed golden can never encode output that
   violates them.
4. Calls `compare_framebuffers(rendered, golden)` and asserts 0 differing pixels.

The ROM builder emits only standard `MOVEA.L` / `MOVE.W` / `BRA.S` opcodes and
programs the VDP exactly as real hardware does (control port `0xC00004`, data
port `0xC00000`, two-word address commands). No assembler or external ROM is
required, and all emitted code is original and freely licensed.

### Scenes

* **`shadow_highlight_operators`** — S/H enabled (reg 0x0C bit 3). A gray Scroll A
  plane with a per-column priority pattern plus operator sprites (palette 3,
  color 15 = shadow op, color 14 = highlight op) over a horizontal band exercise
  every operator case in one frame:
  * (a) high-priority plane -> **Normal** (146)
  * (b) low-priority plane -> **Shadow** (73)
  * (c) shadow-operator sprite over a Normal region -> **Shadow** (73)
  * (d) highlight-operator sprite over a Normal region -> **Highlight** (201)
  * (e) highlight-operator sprite over a Shadowed region -> **Normal** (146)
  * backdrop (priority 0) -> **Shadow** (73)
* **`window_plane_positioning`** — window plane, left split at 10 units. Because
  WHP (reg 0x11) is in 16px (2-cell) units, the window/Scroll-A boundary must
  land at pixel 160; the test asserts x=159 is window (green) and x=160 is
  Scroll A (red).
* **`normal_render_lock`** — S/H disabled. A high-priority sprite over a
  low-priority plane locks current correct normal compositing.

### Running

```sh
# Compare against checked-in goldens (normal CI run):
cargo test -p genesoxide-test-harness --test video_golden

# Just the video tests plus the ROM-builder unit tests:
cargo test -p genesoxide-test-harness --test video_golden
cargo test -p genesoxide-test-harness --lib rom_builder
```

### Regenerating goldens (BLESS)

After an **intentional** rendering change, regenerate the goldens by setting the
`GENESOXIDE_BLESS` environment variable. In bless mode each test writes its
rendered framebuffer to `tests/goldens/<scene>.rgba` instead of comparing:

```sh
GENESOXIDE_BLESS=1 cargo test -p genesoxide-test-harness --test video_golden
```

The pixel assertions still run while blessing, so goldens can only be written if
the render already satisfies the VDP rules. Review the resulting `.rgba` diff
before committing. Goldens are raw RGBA (no PNG dependency); to eyeball one,
load it as 320x224 RGBA in any image tool.

# Genesoxide Parity & Accuracy Audit — July 2026

**Scope:** Compatibility/accuracy audit of the genesoxide Sega Genesis / Mega Drive emulator against mature references (BlastEm, Genesis Plus GX, ares) and documented hardware behavior. This is a **diagnosis**, not a fix — it produces a ranked, evidence-backed defect list to drive the next phase of parity work.

**Audited commit:** `f8c9edd` (trunk) — after PRs #2/#3/#4 (rewind, Z80 fixes, audio fixes).
**Method:** Code inspection (every claim quotes `file:line`), synthetic test programs run headlessly through the core API (`GenesisCore::new()` → `LoadRom` → `StepFrame` → `framebuffer_rgba()`), a master-clock timing measurement, and execution of one real freely-available SGDK homebrew test ROM plus a 7-program synthetic VDP suite. Hardware references: Sega Genesis Software Manual, Charles MacDonald's `genvdp.txt`/`gen-hw.txt`, Nemesis's timing research (SpritesMind), Mask of Destiny's VDPFIFOTesting notes, plutiedev.com, Eke-Eke's Genesis Plus GX notes, the Motorola 68000 User's Manual.

## Executive summary

The core is structurally sound: the 68000 and Z80 pass per-opcode state tests, the basic VDP render path (backdrop, scroll planes at any base, sprites, 68K→VRAM/CRAM/VSRAM DMA, VRAM fill) is correct, and audio is generated. The parity gaps are concentrated in three areas:

1. **Timing fidelity** — the 68000 has no effective-address/size cycle timing (MOVE returns a flat 4 cycles regardless of operands), DMA is instantaneous and never stalls the CPU, and H-interrupts are whole-line-granular so mid-line raster splits are impossible. The emulated frame runs ~0.29% long (measured 898,660 mclk/frame vs hardware 896,040; 59.748 Hz vs 59.92 Hz).
2. **Missing VDP features** — shadow/highlight is entirely unimplemented, H32 (256-px) mode renders left-aligned into a 320-wide buffer, there is no true interlace, and several status-register bits (V-int-pending, sprite overflow/collision) are never set.
3. **Missing system capabilities** — no cartridge SRAM/EEPROM saving, no SSF2/>4MB mapper, no PAL/50 Hz mode, a hardwired non-configurable region register, and 6-button-controller extras (X/Y/Z/Mode) are never decoded.

The single most alarming *observed* symptom: a real SGDK homebrew ROM boots to a **100% black screen** — its tiles and palette upload correctly but the plane nametables never populate, so nothing composites. This points at a defect in the VBlank-interrupt-driven tilemap/DMA-queue path that likely affects a class of SGDK-built homebrew (and possibly commercial titles using the same staging pattern).

## Test-infrastructure inventory

| Component | State |
|---|---|
| `genesoxide-test-harness` z80_suite (98 tests) | jsmoo Z80 vectors — validates regs+RAM per opcode. Vectors are an un-checked-out git submodule; self-skips when absent. |
| m68k_suite (129 tests) | MAME 68000 vectors — validates **final state only**; the returned cycle count is discarded (`let _cycles = execute_instruction(...)`, `m68k_tests.rs:427`). Cycle timing is **not** validated. |
| sonic_boot / audio_golden | Reference a **hard-coded local Windows path** to a commercial Sonic ROM; skip when absent. Comparison is audio (vs bundled `ymfm` C++), not video. |
| vgm_playback (11) | Programmatic VGM sequences exercising YM2612. |
| Golden **frame** images | **None checked in.** `compare_framebuffers()` exists but no reference images are tracked. |
| In-repo test ROMs | **None.** Tests synthesize `vec![0u8; …]` or read the local Sonic path. |

**Gap:** there is no video golden-frame regression suite and no in-repo test ROM. Adding both is a prerequisite for defending parity fixes.

## Test-ROM ledger (this audit)

| ROM | Source | License | Result |
|---|---|---|---|
| gentest (`game.bin`, 3.5 MB, sha `a929fc80…`) | `git clone github.com/clbr/gentest` | Unclear (no LICENSE) → **not vendored**, fetch-at-test-time | Ran 600 frames → **100% black** (see F1) |
| 240pTestSuite (Genesis) | `git clone ArtemioUrbina/240pTestSuite` | GPLv2 | Cloned; **could not run** — no prebuilt `.bin`, needs SGDK + `m68k-elf-gcc` (absent) |
| VDPFIFOTesting | retrodev.com / bitbucket | Free | **Could not fetch** a prebuilt binary via the proxy. Manual: download, run through the harness runner |
| sik / ComradeOj / SGDK samples | GitHub | Mostly MIT | Source-only; **could not run** without a m68k toolchain |
| 7 synthetic VDP ROMs | Built in-runner (ours) | — | Ran; 5 PASS (backdrop, plane at 0xC000/0xE000, sprite, DMA from ROM/RAM), 1 FAIL (shadow/highlight), 1 PARTIAL (H32) |

**Environment note for future runs:** GitHub `raw`/`codeload` returns HTTP 403 through the proxy, but `git clone` works — clone repos rather than fetching raw blobs. A standalone headless runner (dependency-free PNG encoder + inline 68k assembler) was built in the audit scratch area and can be reused.

## Ranked top-10 defects (severity × breadth)

| # | Defect | Sev | Breadth | Subsystem | Fix |
|---|---|---|---|---|---|
| 1 | **SGDK homebrew boots to black — plane tilemaps never populate** despite tiles+palette uploaded | Critical | Class of SGDK homebrew; possibly commercial titles using VInt DMA-queue staging | VDP VRAM-write / VInt DMA-queue path | M |
| 2 | **Shadow/Highlight mode entirely unimplemented** (reg 0x0C bit 3 ignored) | High | Dozens of marquee titles: Sonic 2/3/3D water & translucency, Toy Story, Comix Zone, Vectorman, Gunstar Heroes, Ristar | VDP compositing | L |
| 3 | **No cartridge SRAM / battery save** — writes dropped, nothing persists | High | Hundreds: Phantasy Star II–IV, Sonic 3&K, Shining Force I/II, sports/RPG saves | Mapper/SRAM | M |
| 4 | **68000 has no EA/size cycle timing** — MOVE/ALU/LEA/CLR return flat constants; test suite discards cycles | High | All titles; breaks every cycle-timed effect | Timing/68000 | L |
| 5 | **DMA is instantaneous** — steals zero bus cycles, never asserts DMA-busy | High | Nearly all titles do VBlank DMA; timing-sensitive ones tear/mistime | Timing/DMA | M |
| 6 | **H-interrupt is whole-line only** — no mid-line raster splits | High | Raster-effect titles: Sonic, Golden Axe, Ristar, Comix Zone, demos | Timing/HInt | L |
| 7 | **6-button pad extras (X/Y/Z/Mode) never decoded** — reads as a 3-button pad | High | All 6-button titles: SF2:SCE, Super SF2, Comix Zone, MK block, Virtua Fighter 2 | IO | S |
| 8 | **H32 (256-px) mode renders left-aligned** into 320-wide buffer with black right bar | High | Large class of H32 RPGs/menus: Phantasy Star, Shining Force, Story of Thor | VDP/render | M |
| 9 | **No PAL / 50 Hz mode** — NTSC-only, 262/224 hardwired, V30 (240-line) ignored | Med | All European releases run at wrong speed; V30 games truncated | System/Timing | L |
| 10 | **No SSF2 / >4MB mapper** (0xA130xx ignored) — upper banks unreachable | Med | Super Street Fighter II + >4 MB homebrew/repro | Mapper | M |

## Full findings

Format: **Sev** = Critical/High/Medium/Low · **Fix** = S/M/L.

### VDP — rendering & modes

**F1 · SGDK ROM boots to 100% black — tilemaps never populate · Critical · Fix M · STATUS (re-audit 2026-07-09): UNVERIFIABLE**
Real ROM `gentest` (SGDK) boots, CPU runs its main loop (not halted), display is enabled (mode2=0x74), yet all 600 captured frames are fully black. VDP snapshot shows tile pixel data uploaded (`vram_nz=1914` at 0x0000 and 0xA000–0xAFFF) and palette uploaded (`cram_nz=60`), plane A @0xE000 / plane B @0xC000 configured — but **both nametables are empty** (`A_map_nz=0, B_map_nz=0`). Ruled out plane render, base address, off-origin cells, H32, and DMA-from-ROM/RAM (all pass in the synthetic suite). Symptom points at the VBlank-interrupt-driven tilemap/DMA-queue flush path SGDK uses for `VDP_setTileMapXY`/`VDP_drawText`. *Suggested fix:* trace SGDK's exact control/data/DMA sequence and confirm the level-6 VInt handler actually runs each frame and flushes queued nametable writes/DMAs. **This is the highest-value next investigation — it is a total-failure symptom on real software.**

**F2 · Shadow/Highlight mode absent · High · Fix L · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
No handling of reg 0x0C bit 3 (S/H enable) anywhere in `vdp.rs`; the only `shadow`/`highlight` grep hits are Z80 shadow registers. `color_to_rgba` (`vdp.rs:384`) has a single full-bright ramp, no dim/bright variants; `render_scanline` has no shadow pass and no operator-sprite (palette-3 indices 14/15) handling. Synthetic test: reg 0x0C bit 3 set vs clear on a low-priority line → **byte-identical** output. Empirically confirmed: `syn-sh-off.png` vs `syn-sh-on.png` differ in 0/71680 pixels. Affects water, dim rooms, spotlight, sprite-shadow, and priority-dimming effects. *Fix:* per-pixel S/H from plane priority + operator sprites, plus dim/bright ramps in `color_to_rgba`.

**F3 · H32 (256-px) mode renders left-aligned with a black right bar · High · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`screen_width()` (`vdp.rs:539`) correctly returns 256, and compositing loops respect it, but the writeback loop (`vdp.rs:899-902`) iterates all 320 columns and writes leftover `[0,0,0,0]` into columns 256–319. No centering/pillarbox. Synthetic + real: H32 content is jammed left with a 64-px black bar on the right instead of a centered 256-px image with border color. *Fix:* center the 256-wide region in the 320 buffer (32-px pillarbox each side using reg 7 backdrop) and restrict writeback to `0..width`.

**F4 · Sprite overflow/collision status bits never set; per-line limit hardcoded to H40 · Medium · Fix M · STATUS (re-audit 2026-07-09): PARTIAL**
`read_status` (`vdp.rs:338`) never sets 0x20 (overflow) or 0x40 (collision). `render_sprites_on_line` uses compile-time `MAX_SPRITES_PER_LINE=20` / `MAX_SPRITES_TOTAL=80` regardless of mode — H32 hardware limits are 16/64. No per-line pixel budget (dot overflow). Synthetic: 30 sprites on a line → status `0x3600`, overflow bit clear. *Fix:* set 0x20 on per-line sprite/pixel-budget exceed; gate 16/64 vs 20/80 on H32/H40; add a per-line pixel accumulator.

**F5 · X=0 sprite masking not implemented · Medium · Fix M · STATUS (re-audit 2026-07-09): OPEN**
`render_sprites_on_line` (`vdp.rs:908-1019`) renders every sprite unconditionally; there is no handling of the hardware feature where a sprite at raw X=0 masks lower-priority sprites on that line (with the "one non-masking sprite must be seen first" exception). The existing per-pixel first-sprite-wins logic is a different mechanism. *Fix:* in the SAT walk, if a sprite's raw X==0 and it is not the first drawn on the line, stop processing further sprites that scanline.

**F6 · Window horizontal position uses 8 px/unit; hardware uses 16 px (2 cells) · Medium · Fix S · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`window_h_range` (`vdp.rs:602-613`): `let pixels = cells * 8;`. Reg 0x11 (WHP) horizontal window position is in units of 2 cells = 16 px on hardware. Vertical (`window_v_range`, reg 0x12) correctly uses `cells*8` (1 cell = 8 lines). Result: a left/right window split lands at half the intended X. The existing test `window_plane_renders_over_scroll_a` (expects boundary at 20×8=160) codifies the bug. *Fix:* `pixels = cells * 16`; update the two window tests. **(New — not in prior recon.)**

**F7 · No true interlace; interlace mode 2 (double-res) unsupported · Medium · Fix L · STATUS (re-audit 2026-07-09): OPEN**
Reg 0x0C bits 1–2 (LSM0/LSM1) are never read; the only interlace state is an `odd_frame` toggle used solely for status bit 4. No doubled vertical resolution, no field selection, height hardwired to 224. Affects interlace-2 modes (Sonic 2 two-player versus split, some hi-res intros/title screens). *Fix:* read LSM bits; for mode 2 render both fields at double vertical resolution.

**F8 · DMA VRAM→VRAM copy is a silent no-op · Medium · Fix S · STATUS (re-audit 2026-07-09): OPEN**
`write_control` DMA dispatch (`vdp.rs:231-245`) leaves the mode-0b11 (copy) arm empty with `// VRAM copy — not yet implemented`. `run_dma` handles only 68K→VRAM/CRAM/VSRAM; `execute_dma_fill` only fill. Games issuing copy DMA get stale/missing graphics. *Fix:* implement byte-wise VRAM→VRAM copy using length reg 0x13/0x14 and source reg 0x15/0x16 (byte address, honoring auto-increment).

**F9 · Status register incomplete (V-int-pending 0x80, sprite bits, FIFO-full, PAL) · Medium · Fix S · STATUS (re-audit 2026-07-09): PARTIAL**
`read_status` (`vdp.rs:338-359`) hardcodes FIFO-empty set and never sets: PAL 0x01, sprite-overflow 0x20, collision 0x40, V-int-pending 0x80, FIFO-full 0x100. DMA-busy (0x02) is cleared synchronously so it is never observed. Games polling status for pending-VBlank or region read stale values. *Fix:* add a `vint_pending` flag (set at VBlank, cleared on status read/ack) as 0x80; set 0x01 from region; wire 0x20 from the sprite fix. (Overlaps F4 and the timing V-int finding.)

**F10 · CRAM 9-bit color expansion is correct (informational) · Low · Fix S · STATUS (re-audit 2026-07-09): VERIFIED (informational — correct)**
`color_to_rgba` (`vdp.rs:384-390`) correctly extracts 3-3-3 and scales to {0,36,73,…,255}; CRAM writes mask `& 0x0EEE`; color-0 transparency works. The only color deficiency is the missing S/H dim/bright ramps (tracked under F2).

### Timing

**F11 · 68000 instruction cycle counts have no EA/size timing · High · Fix L · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`exec_move` (`execute.rs:730`) returns a flat `4` for any mode/size; `read_ea`/`write_ea` add no EA cycles. Hardware MOVE.L (a0),(a1)=20 → returns 4 (−80%); MOVE.L (xxx).L,(xxx).L=36 → 4. `exec_movea`/`exec_moveq`/`exec_lea` flat 4 (LEA (d16,An)=8, (d8,An,Xn)=12 hw); `exec_clr` flat 8 for all memory (CLR.L (An)=20 hw); `exec_movem` = 8+4n vs hw 12+8n. Mul/div use fixed worst-case (mulu=70, divu=140, divs=158, operand-independent). Flow control *is* correct (Bcc/BSR/DBcc). The m68k suite discards the returned cycle count (`m68k_tests.rs:427`), so this is entirely unvalidated. Foundational: every cycle-timed effect (raster splits, DMA choreography, timed loops) is wrong. *Fix:* add a real cycle table (base + src-EA + dst-EA + size) threaded through `read_ea`/`write_ea`; re-enable the discarded cycle assertion.

**F12 · DMA is instantaneous — zero bus cycles, DMA-busy never set · High · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`execute_vdp_dma` (`api.rs:2624-2647`) advances no scheduler/CPU cycles; `run_dma` (`vdp.rs:421`) clears `dma_pending` immediately then transfers the whole length with no cycle accounting. The 68000 is never stalled during DMA and the DMA-busy status bit is never observable. Games that assume the CPU is frozen during DMA, poll DMA-busy, or spread a large transfer at ~205 bytes/line mistime (tearing, too-early VRAM writes). *Fix:* charge DMA to the scheduler at the hardware slot rate and hold DMA-busy until those cycles elapse.

**F13 · H-interrupt is whole-line granular — no mid-line raster splits · High · Fix L · STATUS (re-audit 2026-07-09): PARTIAL**
`begin_scanline` (`vdp.rs:507-525`) decrements reg 0x0A once per line; the level-4 IRQ is delivered after the whole line's cycle budget and before `render_scanline`, so each line renders atomically (`api.rs:2232/2264`). `in_hblank` is a whole-line boolean. Sub-line register changes (mid-line window/scroll splits, some water lines, heat-haze, Direct-Color DMA) are impossible; HINT effectively lands one line late. The HINT counter is also only reloaded at line 0, not during VBlank. *Fix:* drive sub-line render/splits from master-tick position; reload the HINT counter during VBlank.

**F14 · Fixed 488-cycle line budget; frame runs ~0.29% long · Medium · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`StepScanline` uses `target = cpu.cycles + 488` (`api.rs:2136/2150`); 488×7=3416 mclk vs hardware 3420 (exact = 488.571, non-integer, so a fixed integer budget can't be right). **Measured:** `StepFrame` advances **898,660 mclk/frame** vs hardware **896,040** → **59.748 Hz** vs 59.92 Hz. `FRAME_PERIOD_NS` (`api.rs:26`, ~59.92 Hz) and `MASTER_TICKS_PER_SCANLINE=3420` (used only for the audio interval) disagree with the actual CPU stepping, so audio and CPU line lengths diverge. Causes slow audio pitch/tempo and long-run A/V desync. *Fix:* drive the CPU by master ticks (run until line_start+3420) so lines average exactly 3420.

**F15 · No VDP FIFO — FIFO-empty hardcoded, no depth/back-pressure · Medium · Fix M · STATUS (re-audit 2026-07-09): OPEN**
`vdp.rs:340-341` sets FIFO-empty unconditionally ("no FIFO emulation"); there is no 4-entry depth, no FIFO-full (0x100), and data-port writes apply instantly with no stall. VDP data-port writes during active display never back-pressure the CPU (hardware stalls when the 4-word FIFO fills). Breaks VDPFIFOTesting and active-display update timing. *Fix:* model a 4-entry FIFO draining at the per-mode slot rate; stall the 68000 when full.

**F16 · HV counter is a stub · Medium · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`read_hv_counter` (`vdp.rs:365-380`): H is 2-valued (`0xE4` in hblank else `0x08`; real H40 counts 0x00–0xEA then jumps 0xE9–0xFF); V is approximate (`scanline` or `scanline-6`) and not tied to the real V-jump line; no intra-line beam position. Breaks HV-based RNG, fine timing, and light-gun (Menacer/Justifier) titles. *Fix:* derive H from intra-line master-tick position with the H40/H32 jump table; V from the exact V-jump line.

**F17 · V-int-pending status bit (0x80) absent; VBlank timing coarse · Medium · Fix S · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
(See also F9.) `read_status` never sets bit 7; the VINT is delivered at scanline==224 and the vblank flag is set there and cleared at frame start. Games/BIOS polling status bit 7 to detect/ack a pending VBlank IRQ never see it. Z80 /INT is asserted ~1 line, which is functional. *Fix:* set 0x80 on VINT, clear on status read/ack.

**F18 · Z80/68000 bus arbitration not cycle-modeled · Low · Fix M · STATUS (re-audit 2026-07-09): OPEN**
BUSREQ (0xA11100) just sets a boolean with no BUSACK latency (`api.rs:2996-3002`); the Z80 gets a fixed +228 T-states/scanline gated by a per-scanline boolean; bank-register writes cost nothing. A scanline-granular `z80_bus_released_this_scanline` workaround exists to avoid SMPS handshake deadlock. Tight handshake loops mistime. *Fix:* model BUSREQ/BUSACK acquisition latency and stall the 68000 on Z80-bus access while the Z80 runs; interleave finer than per-scanline.

### I/O · Mappers · System

**F19 · 6-button controller extras (X/Y/Z/Mode) never decoded · High · Fix S · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`read_data` (`io.rs:90-132`) branches only on `th_state` (two states) and returns only 3-button data; `th_count` is incremented on the TH rising edge (`io.rs:137`) but never consulted, and the X/Y/Z/Mode masks (`io.rs:179-182`) never appear in any read path. Synthetic test: pressing X+Y+Z+Mode and driving four full TH cycles never changes the low nibble. A 6-button pad reads identically to a 3-button pad. *Fix:* use the `th_count` phase — 3rd TH=0 returns the ID nibble (0x0), 4th TH=0 returns X/Y/Z/Mode in bits 0–3.

**F20 · No cartridge SRAM / battery save · High · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
No SRAM array exists anywhere in the core. SRAM at 0x200000–0x20FFFF is classified as `CartridgeRom` by `map_region` (`bus.rs:44`); reads return ROM/0 and writes hit the `_ => {}` arm (`api.rs:2955`) — silently dropped. The 0xA130F1 enable register maps to `Unmapped`. `rom.rs:118-119` parses the header RAM range but nothing consumes it. Battery saves are impossible; SRAM does not even hold values within a session. *Fix:* add a cart-SRAM byte array over the header RAM range (default 0x200000–0x20FFFF), gate on the 0xA130F1 latch, expose for host persistence. (Correction to recon: SRAM lands in the `CartridgeRom` region, not `CartridgeExtended`.)

**F21 · No serial EEPROM support · Medium · Fix M · STATUS (re-audit 2026-07-09): OPEN**
No EEPROM/I²C state machine (grep → none); same silent-drop write path as SRAM. Affects Wonder Boy in Monster World, Mega Man: The Wily Wars, NBA Jam / T.E., NFL Quarterback Club, Greatest Heavyweights, Evander Holyfield Boxing. *Fix:* add a 24Cxx serial-EEPROM machine at the cart's EEPROM addresses with a per-title mapping table.

**F22 · No SSF2 / >4MB bank mapper (0xA130xx) · Medium · Fix M · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
`bus.rs:44` caps `CartridgeRom` at 0x3FFFFF and `read_byte` masks `addr & 0x3FFFFF` (`api.rs:2826`), so a >4MB ROM's upper banks are unreachable; the SSF2 mapper registers 0xA130F3–0xA130FF map to `Unmapped`. Affects Super Street Fighter II (5 MB) and >4MB homebrew/repro. *Fix:* add eight 512 KB bank slots indexed by 0xA130F3/F5/…/FF, windowing 68000 0x080000–0x3FFFFF into a >4MB ROM.

**F23 · No PAL / 50 Hz support · Medium · Fix L · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
Only `MASTER_CLOCK_NTSC` exists (`scheduler.rs:18`); no PAL master clock; scanline/active-line counts hardwired NTSC (262/224); reg 1 bit 3 (V30/240-line) never consulted; HV V-wrap is NTSC-specific. European releases run at the wrong speed; V30 games render the wrong height. *Fix:* add a PAL region mode (PAL master clock, 313 lines, 240 active via reg 1 bit 3, version bit 6 set).

**F24 · Version/region register (0xA10001) hardwired to 0xA0, not configurable · Medium · Fix S · STATUS (re-audit 2026-07-09): VERIFIED FIXED**
Returned as a constant `0xA0` in three places (`api.rs:2677/2836/2900`). 0xA0 = overseas + NTSC + no expansion. Not derived from ROM header region or any config; region-locked titles always resolve to US NTSC and Japan/PAL region branches never trigger. *Fix:* make the version byte a field derived from a region config (Japan/US/Europe), defaulting to 0xA0.

**F25 · Reset leaves 68000 interrupt mask at 0 instead of 7 · Low · Fix S · STATUS (re-audit 2026-07-09): OPEN (deferred)**
`Cpu::new()` and `reset()` set SR to 0x2000 (S=1, mask 0); real 68000 reset sets 0x2700 (mask 7). A game (or test ROM) enabling the display/VInt before raising the mask could take a spurious early interrupt. Mitigated in practice because VDP registers reset to 0 (VInt disabled) and games set SR early. *Fix:* initialize reset SR to 0x2700. **(Safe, tiny fix — deferred to keep this audit diagnosis-only.)**

**F26 · Unmapped reads return 0 instead of open-bus · Low · Fix M · STATUS (re-audit 2026-07-09): OPEN**
Every bus read path ends `_ => 0` (`api.rs:2706/2879/2951`), contradicting the `bus.rs:23` doc comment ("Reads return open bus"); undefined Z80-area sub-addresses return a fixed 0xFF. A few copy-protection/edge-case titles rely on the floating (last-prefetch) value. *Fix:* track the last word on the 68000 bus and return it for unmapped reads.

**F27 · 68000→Z80 window drops bank-register and PSG writes · Low · Fix S · STATUS (re-audit 2026-07-09): OPEN (quick-win)**
`CoreBus` Z80Area write arms (`api.rs:2971-2990`) handle only Z80 RAM and YM2612 (0x4000-0x4003); the bank register (0x6000) and PSG (0x7F11) cases present in the dedicated `Z80Bus` are absent. A 68000 that programs the sound-ROM bank or PSG through its Z80 window (legal on hardware) has no effect. *Fix:* add 0x6000-0x60FF and 0x7F00-0x7FFF arms to the CoreBus Z80Area write paths.

**F28 · TMSS security register ignored (benign) · Low · Fix S · STATUS (re-audit 2026-07-09): OPEN (benign — no action)**
0xA14000/0xA14101 fall outside all defined regions → `Unmapped`; the SEGA write and VDP-lock latch are neither stored nor enforced. Ignoring TMSS is the safe choice (TMSS games boot); listed only for completeness.

**F29 · BUSREQ grant is instantaneous · Low · Fix M · STATUS (re-audit 2026-07-09): OPEN**
The Z80 bus request is granted with zero latency and the grant bit is derived purely from the request flag (`api.rs:2856-2940`); there is no BUSREQ-during-Z80-instruction delay. Most games tolerate instant grant. *Fix:* model a short grant latency, reflecting bit 0 = 1 until the Z80 reaches an instruction boundary.

## Capability gaps & breadth (feature-absent, not bugs)

| Capability | Titles affected (class) |
|---|---|
| Cartridge SRAM (F20) | Hundreds — most RPGs and sports with battery saves |
| EEPROM (F21) | ~6–12 known titles (Wonder Boy MW, Mega Man Wily Wars, NBA Jam, etc.) |
| SSF2 / >4MB mapper (F22) | Super Street Fighter II + large homebrew/repro |
| PAL / 50 Hz (F23) | All European releases (wrong speed); V30 games (wrong height) |
| Shadow/Highlight (F2) | Dozens of high-profile NTSC/PAL titles |
| H32 centering (F8) | Large class of H32 RPGs/menus |
| True interlace (F7) | Small named set (Sonic 2 2P split, some hi-res intros) |

## Recommended next-phase sequencing

1. **Root-cause F1** (SGDK black screen) first — it is a real observed total failure and likely uncovers a systemic VInt/DMA-queue defect that other findings touch (F12, F13, F17).
2. **Build a video golden-frame harness** and vendor/fetch a small set of freely-licensed test ROMs (VDPFIFOTesting, 240p Test Suite, sik's tests) so subsequent fixes are regression-guarded. This audit's standalone headless runner (PNG + inline 68k assembler) is a starting point.
3. **68000 cycle table (F11)** and **DMA cycle cost (F12)** together unlock accurate raster/DMA timing (F13, F14) — do them as a unit and re-enable the discarded m68k cycle assertion.
4. **Shadow/Highlight (F2)** and **H32 centering (F8)** are the highest-visibility pure-render fixes.
5. **SRAM (F20)** unblocks the largest title count for a moderate fix; **6-button (F19)** is a small fix with high breadth.

*All findings verified against commit `f8c9edd` by code inspection plus synthetic and real-ROM execution. Cycle numbers and hardware behaviors are cross-checked against the references listed in Method.*

---

## 2026-07-09 re-audit

**Re-audited tree:** `chore/re-audit` @ `14e9461` — after PRs #7–#13 merged on top of the original `f8c9edd` audit. Notable landings: PR #12 (PAL/50 Hz + SSF2 >4 MB mapper), PR #13 (cycle-accurate 68000 instruction timing, sub-line VDP HV/HINT position, exact NTSC frame timing), and a **new video golden-frame suite** (`tests/video_golden.rs` + `tests/goldens/*.rgba`) that did not exist at audit time.

### Methodology

- **Re-ran the full workspace suite + targeted probes on `14e9461`.** `cargo build --workspace --exclude genesoxide-desktop` builds clean (desktop alone fails on a missing `libudev` system lib via `gilrs` — frontend-only, not an emulation defect). Suite result: **no failing tests** — genesoxide-core 306, genesoxide-config 8, harness 392 pass / 106 ignored (m68k_suite 127, z80_suite 97, video_golden 7, sonic_boot 9, vgm_playback 10, audio_golden 104, lib 38).
- **Per-finding re-verification.** Each of F1–F29 was re-checked against the post-PR code paths and against the harness test / golden / probe that now exercises it (mapping in the findings map). Verdicts below are graded by whether a *real passing test or confirmed-correct code path* backs the claim — not merely by PR title.
- **Regression hunt at the churn's interaction seams.** Because PRs #7–#13 added persistent/regional/timed state (SRAM, SSF2 banks, VInt-pending, DMA-busy, region), the seams where that new state crosses older subsystems were probed directly — chiefly the rewind delta-encoder path, the framebuffer width/height latch, and the audio filter's clock derivation. Three regressions surfaced (below).

### Per-finding verdict table

Statuses mirror the STATUS tag now on each finding header above. Evidence column names the passing test / golden / code path that backs the verdict.

| # | Finding (abbrev.) | Status | Evidence / nuance |
|---|---|---|---|
| F1 | SGDK ROM boots to black — tilemaps never populate | **UNVERIFIABLE** | No in-tree SGDK/gentest probe ROM. The cited `sonic_boot::sonic_renders_hud` self-skips on a missing hard-coded Windows ROM path, so it passes vacuously — zero running/code evidence the fix holds. Needs a vendored headless probe ROM. |
| F2 | Shadow/Highlight mode absent | **VERIFIED FIXED** | `video_golden::shadow_highlight_operators` (real per-pixel S/H math against golden). |
| F3 | H32 renders left-aligned, black right bar | **VERIFIED FIXED** | `video_golden::h32_centered` + `::mode_switch_sequence` goldens lock a centred/pillarboxed 256-px image. (Reconciled from test-map evidence — see note below.) |
| F4 | Sprite overflow/collision bits; per-line limit hardcoded H40 | **PARTIAL** | Per-line limit now H32/H40 mode-aware (16/64 vs 20/80). BUT sprite-overflow (0x20) / collision (0x40) status bits still never set, and no per-line pixel-dot budget. |
| F5 | X=0 sprite masking not implemented | **OPEN** | Not implemented. |
| F6 | Window H position 8 px/unit (HW = 16 px) | **VERIFIED FIXED** | `video_golden::window_plane_positioning` + core `vdp::tests` unit tests confirm 16 px/unit. |
| F7 | No true interlace / LSM double-res | **OPEN** | Reg 0x0C LSM bits still never read. |
| F8 | DMA VRAM→VRAM copy is a silent no-op | **OPEN** | No evidence of a fix; no golden/test. Copy arm remains unaddressed. (Not in the primary verdict set — see note.) |
| F9 | Status register incomplete (0x80/sprite/FIFO/PAL) | **PARTIAL** | V-int-pending 0x80 (F17) and PAL 0x01 (F23) bits now set; but sprite-overflow 0x20 (F4) and FIFO-full 0x100 (F15) bits remain unset. Composite finding — partially closed. |
| F10 | CRAM 9-bit color expansion correct (informational) | **VERIFIED (info)** | Confirmed correct; informational only. |
| F11 | 68000 has no EA/size cycle timing | **VERIFIED FIXED** | 26 core `cpu/execute.rs` cycle unit tests vs MC68000UM values. **CAVEAT:** the `m68k_suite` SingleStepTests corpus is ABSENT (`tests/m68000-tests/v1/` empty), so its 127 suite tests self-skip and do NOT validate the cycle field — cycle correctness rests solely on the in-crate unit tests. |
| F12 | DMA instantaneous — no bus cycles, DMA-busy never set | **VERIFIED FIXED** | DMA-busy status bit modeled + CPU genuinely stalled for the cycle-costed duration (`dma_busy_*`, `dma_cost_*` tests). Nuance: data still lands in one shot; only the busy/stall window is cycle-costed. |
| F13 | H-interrupt whole-line only — no mid-line raster splits | **PARTIAL** | Counter-reload bug fixed + per-line raster bands validated (`video_golden::hint_raster_bands`). BUT true mid-line / sub-line raster splits remain UNIMPLEMENTED (explicitly deferred). PR #13's "sub-line HINT" added sub-line HV counter *position*, not sub-line HINT *delivery*. |
| F14 | Fixed 488-cycle line; frame ~0.29% long | **VERIFIED FIXED** | 896040 master ticks/frame, 59.9227 Hz (`ntsc_frame_advances_master_clock_by_896040`). Resolves the old 898,660 mclk / 59.748 Hz measurement. |
| F15 | No VDP FIFO — FIFO-empty hardcoded, no back-pressure | **OPEN** | FIFO-empty still hardcoded; no depth / back-pressure / FIFO-full bit. |
| F16 | HV counter is a stub | **VERIFIED FIXED** | Full intra-line H counter with mid-line jump + region-correct V wrap (was a 2-value stub); per-68000-instruction granularity (documented). |
| F17 | V-int-pending 0x80 absent; coarse VBlank | **VERIFIED FIXED** | `vdp::tests::vint_pending_latch_shows_in_status_bit_7` + API integration tests. Nuance: VIP clears on interrupt ack, not on control-port read (minor HW deviation). |
| F18 | Z80/68000 bus arbitration not cycle-modeled | **OPEN** | BUSREQ is still a bool; scanline-granular workaround present. |
| F19 | 6-button pad extras (X/Y/Z/Mode) never decoded | **VERIFIED FIXED** | Real decode + byte-exact core unit tests. |
| F20 | No cartridge SRAM / battery save | **VERIFIED FIXED** | Full `CartSram`, 11 passing round-trip / banking / snapshot tests. |
| F21 | No serial EEPROM support | **OPEN** | No 24Cxx/I²C state machine. |
| F22 | No SSF2 / >4 MB bank mapper | **VERIFIED FIXED** | Bank-switch remaps reads across >4 MB, upper banks reachable (`mapper_ssf2_*` tests). |
| F23 | No PAL / 50 Hz support | **VERIFIED FIXED** | Region constants (313 lines, 53.2 MHz master, ~49.7 Hz), 320x240 geometry, V30 gated to PAL (`video_golden::pal_v30_240_lines`). |
| F24 | Version/region reg (0xA10001) hardwired 0xA0 | **VERIFIED FIXED** | Region-aware (`io_version_register_returns_region`). |
| F25 | Reset leaves 68000 mask 0 not 7 | **OPEN (deferred)** | SR still resets to 0 (HW = 7 / 0x2700). Deferred. |
| F26 | Unmapped reads return 0 not open-bus | **OPEN** | Still `_ => 0`; no open-bus / last-bus. |
| F27 | 68000→Z80 window drops bank-reg & PSG writes | **OPEN (quick-win)** | PSG (0x7F11) and Z80 bank (0x6000) writes through the 68000→Z80 window still land on a no-op (the Z80's own bus path handles 0x6000). Small, high-value quick-win. |
| F28 | TMSS security register ignored (benign) | **OPEN (benign — no action)** | Intentional/benign; no action. |
| F29 | BUSREQ grant instantaneous | **OPEN** | Grant still instantaneous; no BUSACK acquisition latency. |

**Reconciliation notes.** The coordinator verdict set explicitly graded 26 findings (13 fixed, 2 partial, 1 unverifiable, 10 backlog). Three findings not named in that set were reconciled here from doc + test-map evidence: **F3** (H32 centring) is marked VERIFIED FIXED on the strength of the `h32_centered` + `mode_switch_sequence` goldens; **F8** (VRAM→VRAM copy) is marked OPEN as no test or code change addresses it; **F9** (composite status-register bits) is marked PARTIAL because its 0x80/0x01 sub-bits closed via F17/F23 while its 0x20/0x100 sub-bits stay open under F4/F15. Tally: 14 VERIFIED (incl. F10 informational), 3 PARTIAL (F4, F9, F13), 1 UNVERIFIABLE (F1), 11 OPEN.

### Regressions found

- **REG-1 (HIGH — FIXED IN THIS PR):** the rewind delta encoder (`rewind.rs`) was never updated for PRs #7–#13, so time-travel rewind silently dropped SRAM, SSF2 banks, VInt-pending, DMA-busy, and region across the ~59/60 delta frames between keyframes (proven by repro). serde snapshot/restore save-states were unaffected. Fixed in this PR by mirroring the new state into the delta path and adding regression test `delta_carries_new_persistent_state` (core suite 306→307, all green).
- **REG-2 (LOW):** framebuffer width is latched at render line 0 but active height is read live, so a mid-frame V30 toggle under PAL could momentarily mismatch. Harmless (buffer is max-sized, frontends re-query). Cleanup: latch active height at frame start too.
- **REG-3 (LOW / info):** the YM output filter cutoff is derived from the NTSC master clock even under PAL (~0.9% cutoff error) — negligible, not a cadence/drift bug.

### Test-infrastructure caveats

Several "passing" suites self-skip and validate nothing on this host — the green board is partly hollow:

- `m68k_suite` (127) — SingleStepTests corpus absent (`tests/m68000-tests/v1/` empty); does NOT validate the F11 cycle field.
- `z80_suite` — vectors absent.
- `sonic_boot` (all, including the F1 verifier) — ROM at a hard-coded Windows path, absent.
- ~200 `diagnose_ghz_*` audio diagnostics self-skip by design.

Coverage that DID run: core 306 (307 after the REG-1 rewind test), config 8, `video_golden` 7, plus audio/vgm goldens and the core cpu/dma/sram/mapper unit tests. Cross-checked timing values — HV counter, V-counter wrap (NTSC/PAL), VBlank flag window, DMA active-vs-blank rates, and the 896040-tick frame — all MATCH published Genesis hardware docs.

### Ranked next-phase backlog

1. **Restore audit confidence / make F1 testable** — vendor headless probe ROMs into the repo (an SGDK/gentest black-screen probe + a headless Sonic boot ROM) and fetch the m68k + z80 SingleStepTests corpora in CI, so the green suite stops being partly hollow and F1 becomes verifiable.
2. **F27 quick-win** — route PSG (0x7F11) and Z80 bank (0x6000) writes through the 68000→Z80 window (small; the Z80's own bus already handles 0x6000).
3. **True mid-line / sub-instruction raster splits** — HINT beam-crossing delivery (completes F13).
4. **Z80/68000 bus arbitration** — sub-scanline BUSREQ/BUSACK granularity + acquisition latency (F18/F29).
5. **Sprite overflow (0x20) + collision (0x40) status bits** and a per-line pixel-dot budget (completes F4/F9).
6. **X=0 sprite masking** (F5).
7. **VDP FIFO** depth, back-pressure, FIFO-full bit with write stalls (F15).
8. **Serial EEPROM** (24Cxx/I²C) battery saves (F21).
9. **Open-bus / last-bus** for unmapped reads (F26); reset SR mask = 7 (F25); TAS behavior.
10. **Interlace / LSM** double-resolution modes (F7).
11. **Low cleanups** — latch active-height at frame start (REG-2); PAL-correct YM filter cutoff (REG-3).

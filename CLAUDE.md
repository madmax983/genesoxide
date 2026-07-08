# Genesoxide

Sega Genesis / Mega Drive emulator in Rust. Part of the oxide emulator family.

## Architecture

- **genesoxide-core**: Pure emulation library. No I/O, no windowing. Owns all hardware state.
  - Frontends drive via `Command` enum, poll via `CoreQuery`
  - Extract framebuffer: `core.framebuffer_rgba()` (320x224 RGBA)
  - All state serializable for snapshots
- **genesoxide-config**: TOML config, `GenesisConfig::load_or_default()`
- **genesoxide-desktop**: CLI frontend. Winit + Pixels + cpal + ringbuf + Gilrs.
- **genesoxide-test-harness**: ROM-based integration tests, golden frame comparison.

## Hardware Emulated (v0.1)

- Motorola 68000 CPU @ 7.67 MHz (24-bit address space)
- VDP (YM7101): tiles, sprites, scroll planes, DMA
- Controller I/O (3-button and 6-button pads)
- 64KB work RAM, 64KB VRAM, 128B CRAM, 80B VSRAM

## Not Yet Implemented

- Mappers beyond standard ≤4MB

The Z80 CPU (audio driver), YM2612 FM synth, and SN76489 PSG are all implemented;
audio is generated, not silent.
## Implemented Audio / Z80

- Z80 CPU: full instruction set, validated per-opcode against the
  SingleStepTests/z80 suite (see `genesoxide-test-harness --test z80_suite`).
  VBlank /INT is asserted to the Z80 for one scanline per frame.
- YM2612 FM synth and SN76489 PSG are present and wired (owned/tuned by a
  separate audio workstream — avoid editing `ym2612.rs` / `psg.rs` blindly).

## Patterns

- Same Command/CoreQuery API as NES emulator
- Concrete types, no trait objects. Enum dispatch for mappers.
- Snapshot-based save states via serde Serialize/Deserialize
- Verus specs on bus mapping, 68000 decoder, VDP command parser
- Proptest on CPU instruction semantics (aspirational — proptest is a dev-dependency
  but no property tests are written yet)

## Key Constants

- Frame: 320x224 RGBA (NTSC), 59.92 Hz
- CPU: ~7.67 MHz, 1 CPU cycle ≈ 2 VDP cycles
- Memory: 24-bit address space (16MB), 64KB work RAM mirrored at 0xFF0000

## Commands

```
cargo run -p genesoxide-desktop -- run <rom> --scale 3
cargo test -p genesoxide-core
cargo test -p genesoxide-test-harness
```

## Test Tiers

1. CPU instruction tests (68kTT, Flamewing) — gate before VDP
2. VDP register/rendering tests — golden frame comparison
3. Full game: Sonic the Hedgehog boots and plays

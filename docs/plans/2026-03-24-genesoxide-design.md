# Genesoxide Design Document

**Date**: 2026-03-24
**Status**: Accepted

## Overview

Genesoxide is a Sega Genesis / Mega Drive emulator written in Rust, following the architecture established by the NES emulator (`C:\Users\markm\nes`). CLI-first, no GUI chrome, Verus-verified core invariants.

Part of the oxide emulator family — eventually unified under `retroxide`.

## Goals

- Boot Sonic the Hedgehog to playable Green Hill Zone
- CLI-first desktop frontend: `genesoxide run sonic.bin --scale 3`
- Same Command/CoreQuery API pattern as NES
- Snapshot-based save states with serde
- Verus specs on 68000 decoder and bus mapping
- Proptest on CPU instruction semantics

## Non-Goals (for v0.1)

- Z80 CPU (audio driver — stub with silence)
- YM2612 FM synthesis / PSG audio
- Netplay, rewind, MCP, TUI, web, AI, DSL
- Mappers beyond standard ≤4MB cartridges

## Project Structure

```
genesoxide/
├── Cargo.toml                   # workspace root
├── CLAUDE.md
├── genesoxide.toml              # default config
├── docs/
│   ├── adr/
│   └── plans/
└── crates/
    ├── genesoxide-core/         # pure emulation, no I/O
    ├── genesoxide-config/       # TOML config with serde
    ├── genesoxide-desktop/      # Winit + Pixels + Rodio
    └── genesoxide-test-harness/ # ROM-based integration tests
```

## Hardware Scope (v0.1)

| Component | Status | Notes |
|-----------|--------|-------|
| Motorola 68000 CPU | Included | 7.67 MHz, 24-bit address space |
| VDP (YM7101) | Included | Tile rendering, sprites, DMA, scrolling |
| Controller I/O | Included | 3-button and 6-button pads |
| ROM parsing | Included | Header at 0x100, region detection |
| DMA | Included | 68K→VRAM, fill, copy |
| Z80 CPU | Deferred | Audio driver, stub with silence |
| YM2612 | Deferred | FM synthesis |
| PSG (SN76489) | Deferred | Tone/noise generator |

## Core Architecture

### Memory Bus (24-bit, 16MB address space)

```rust
pub enum BusRegion {
    CartridgeRom,       // 0x000000..=0x3FFFFF (up to 4MB)
    CartridgeExtended,  // 0x400000..=0x7FFFFF (mapper space)
    Z80Area,            // 0xA00000..=0xA0FFFF
    IoRegisters,        // 0xA10000..=0xA1001F
    ControlRegisters,   // 0xA11100..=0xA11201
    Vdp,                // 0xC00000..=0xC00008
    Psg,                // 0xC00011
    WorkRam,            // 0xFF0000..=0xFFFFFF (64KB mirrored)
}
```

### 68000 CPU

- 8 data registers (D0-D7), 8 address registers (A0-A7)
- A7 = stack pointer (USP/SSP), 24-bit PC, status register
- Variable-length instructions (2-10 bytes)
- Concrete struct, Serialize/Deserialize for snapshots

### VDP

- 64KB VRAM, 128B CRAM (4 palettes × 16 colors × 9-bit RGB), 80B VSRAM
- Two scroll planes (A, B) + window plane + 80 sprites
- 320×224 NTSC output → RGBA framebuffer
- Control port command word state machine (two-word sequence)
- DMA engine: fill, copy, 68K→VRAM transfer

### Command/Query API

```rust
pub enum Command {
    LoadRom(Vec<u8>),
    Reset, PowerCycle,
    StepCpu, StepScanline, StepFrame,
    SetControllerState { port: u8, buttons: u16 },
    PressButton { port: u8, button: Button },
    ReleaseButton { port: u8, button: Button },
    SetSpeed(u16),
    Pause, Resume,
}
```

### Scheduler

- 68000: ~7.67 MHz
- VDP: ~13.4 MHz (~2 VDP cycles per CPU cycle)
- Wrapping u64 counters, same pattern as NES

## Desktop Frontend

CLI binary using Winit + Pixels + Rodio (+ Gilrs for gamepads).

```
genesoxide run <rom> [--scale N] [--fullscreen] [--step frame|cpu|scanline]
genesoxide info <rom>
genesoxide verify <rom>
```

Main loop: poll input → Command::StepFrame → extract framebuffer → Pixels → vsync at 59.92 Hz.

## Test Strategy

### Tier 1: CPU instruction tests
68000 test ROMs (68kTT, Flamewing suite). Gate before VDP work.

### Tier 2: VDP register/rendering tests
Homebrew test ROMs + golden frame comparison.

### Tier 3: Full game boot
Sonic the Hedgehog: SEGA logo → title screen → Green Hill Zone playable.

## Milestones

1. 68000 passes instruction tests
2. VDP renders static tiles
3. Sonic SEGA splash screen
4. Sonic title screen with input
5. Green Hill Zone playable at 60fps

## Verification (Verus)

- Bus region mapping: exhaustive address → region correctness
- 68000 instruction decoder: finite state machine properties
- VDP command word parser: state machine invariants
- DMA transfer bounds: no out-of-range VRAM access

# Genesis Audio Implementation Design

**Date**: 2026-03-26
**Status**: Accepted

See also: [`docs/genesis-audio-fidelity-log.md`](../genesis-audio-fidelity-log.md) for the chronological record of what actually happened during audio-fidelity bring-up, including bugs, false leads, harness fixes, oracle work, output-chain tuning, and carry-forward lessons.

## Overview

Full audio implementation for genesoxide: Z80 CPU, YM2612 FM synthesis, SN76489 PSG, and threaded audio output via cpal. Audio generation runs on the main emulation thread; output is on a separate cpal callback thread connected by a lock-free SPSC ring buffer (same pattern as doom-rs).

## Goals

- Full Z80 CPU (all instructions including CB/DD/ED/FD prefixes)
- Full YM2612 operator-level FM synthesis (6 channels, 4 operators, 8 algorithms, DAC mode)
- Full SN76489 PSG (3 square + 1 noise)
- Threaded audio output via cpal with graceful fallback to silence
- Sonic the Hedgehog produces audible music and SFX

## Crate & Module Structure

```
genesoxide-core/src/
├── z80/
│   ├── mod.rs          # Z80 CPU state, snapshot, re-exports
│   ├── decode.rs       # Opcode decoder (base + CB/DD/ED/FD prefixes)
│   └── execute.rs      # Instruction executor, Bus trait
├── ym2612.rs           # YM2612 FM synthesis
├── psg.rs              # SN76489 PSG
└── ... (existing files)

genesoxide-desktop/src/
├── main.rs             # adds audio init
└── audio.rs            # cpal output thread, ring buffer consumer
```

No new crate. Z80/YM2612/PSG are part of genesoxide-core (pure emulation, no I/O). Audio output lives in genesoxide-desktop.

## Z80 CPU

Runs at master clock / 15 = ~3.58 MHz. Own 64KB address space:

| Address Range | Target |
|---------------|--------|
| 0x0000-0x1FFF | Z80 RAM (8KB) |
| 0x2000-0x3FFF | Z80 RAM mirror |
| 0x4000-0x4003 | YM2612 registers |
| 0x6000 | Bank register (68K ROM window) |
| 0x7F11 | PSG write port |
| 0x8000-0xFFFF | 68K bus window (32KB, bank-selected) |

### CPU State

```rust
pub struct Z80 {
    pub a: u8, pub f: u8,
    pub b: u8, pub c: u8,
    pub d: u8, pub e: u8,
    pub h: u8, pub l: u8,
    pub a_prime: u8, pub f_prime: u8,
    pub b_prime: u8, pub c_prime: u8,
    pub d_prime: u8, pub e_prime: u8,
    pub h_prime: u8, pub l_prime: u8,
    pub ix: u16, pub iy: u16,
    pub sp: u16, pub pc: u16,
    pub i: u8, pub r: u8,
    pub iff1: bool, pub iff2: bool,
    pub im: u8,
    pub halted: bool,
    pub cycles: u64,
}
```

Bus trait: `read_byte`, `write_byte`, `read_port`, `write_port`.

Decoder handles: base (256), CB (bit ops), DD (IX-indexed), ED (extended), FD (IY-indexed), DD CB/FD CB (indexed bit ops).

## YM2612 FM Synthesis

6 FM channels, each with 4 sine-wave operators in one of 8 algorithm topologies. Channel 6 can switch to 8-bit DAC mode.

### Per-Operator State

| Field | Type | Description |
|-------|------|-------------|
| phase | u32 | 20-bit phase accumulator |
| envelope | u16 | 10-bit attenuation (0=loud, 1023=silent) |
| env_state | enum | Attack / Decay / Sustain / Release |
| rate | u8 | Envelope rate |
| total_level | u8 | TL volume |
| multiply | u8 | Frequency multiplier |
| detune | u8 | Fine pitch offset |
| key_on | bool | Key state |

### Per-Channel State

Frequency (fnum + block), algorithm (0-7), feedback level, L/R panning.

### Global State

LFO frequency/enable, timers A/B (fire IRQs to Z80), DAC enable + data byte.

### Synthesis Pipeline (per sample)

1. Advance phase: `(fnum << block) * multiply`
2. Sine lookup: 10-bit input -> 12-bit log-sin output
3. Apply envelope attenuation (add in log domain)
4. Log-to-linear conversion (power-of-2 table)
5. Route through algorithm topology
6. Sum 6 channels with L/R panning
7. DAC mode: channel 6 output replaced with raw 8-bit value

Clocks at master / 144 = ~53kHz. Output resampled to 44.1kHz.

Two register banks selected via ports 0x4000-0x4003.

## SN76489 PSG

3 square wave channels + 1 noise channel.

```rust
pub struct Psg {
    tone_period: [u16; 3],
    tone_counter: [u16; 3],
    tone_polarity: [bool; 3],
    volume: [u8; 4],           // 4-bit attenuation (0=loud, 15=silent)
    noise_register: u16,       // 16-bit LFSR
    noise_mode: NoiseMode,     // Periodic or White
    noise_rate: u8,
    noise_counter: u16,
    noise_polarity: bool,
    latch_channel: u8,
    latch_is_volume: bool,
}
```

Clocks at master / 16 = ~3.35 MHz. Noise LFSR: bit 0 XOR bit 3 for white, bit 0 only for periodic.

Written by 68000 via 0xC00011 or Z80 via 0x7F11. Latch/data byte protocol.

Output: +/-1 scaled by 15-level volume table (~2dB steps), summed mono, mixed into YM2612 stereo.

## Audio Output Thread

### Threading Model

```
Emulation thread (GenesisCore):
  step_frame():
    for each scanline:
      step_68000 (~488 cycles)
      step_z80 (~228 cycles)
      deliver interrupts
      render_scanline
      collect_audio_samples()   // advance YM2612 + PSG
    // ~735 stereo f32 samples at 44.1kHz per frame
    push samples to ring buffer

cpal callback thread:
  audio_callback(data: &mut [f32]):
    pull from ring buffer
    if empty -> silence
```

### Ring Buffer

Lock-free SPSC ring buffer, shared via `Arc`. Fixed size (~4 frames worth = ~3000 sample pairs). Using `ringbuf` crate or hand-rolled with `AtomicUsize` head/tail.

### Sample Format

Stereo interleaved f32, 44.1kHz. YM2612 ~53kHz downsampled with linear interpolation (upgradeable to Blep). PSG mono mixed into both L/R.

### Core API

```rust
impl GenesisCore {
    pub fn audio_samples(&self) -> &[f32];
    pub fn take_audio_samples(&mut self) -> Vec<f32>;
}
```

### Desktop Integration

cpal stream created in `cmd_run()`. Ring buffer shared via `Arc`. After each `StepFrame`, push samples. Graceful fallback: if no audio device, run silent.

Uses `cpal` directly (not `rodio`) — we generate raw PCM, no mixing layer needed.

## Integration: step_frame() Changes

```
Current:                          New:
for scanline in 0..262 {          for scanline in 0..262 {
  begin_scanline()                  begin_scanline()
  step_scanline()  // 68000         step_scanline()      // 68000
                                    step_z80_scanline()   // Z80 (~228 cycles)
  deliver H-int                     deliver H-int
  render_scanline()                 render_scanline()
  deliver V-int @ 224               deliver V-int @ 224
                                    collect_audio_samples()
}                                 }
```

### Timing

- 68000: master/7 = ~7.67 MHz, ~488 cycles/scanline
- Z80: master/15 = ~3.58 MHz, ~228 cycles/scanline
- YM2612: master/144 = ~53kHz, ~2.8 samples/scanline
- PSG: master/16 = ~3.35 MHz, ~13 clocks/scanline
- Audio output: 44.1kHz = ~735 samples/frame

Fractional sample accumulation with phase counter to avoid drift.

### Z80 Bus Request

The existing 0xA11100 stub becomes functional: writing 0x0100 halts the Z80, releasing resumes it. `step_z80_scanline()` skips when Z80 is bus-halted.

## GenesisCore New Fields

```rust
pub struct GenesisCore {
    // ... existing fields ...
    z80: Z80,
    z80_ram: Box<[u8; 0x2000]>,    // 8KB Z80 RAM
    z80_bank: u32,                  // 68K ROM bank register
    z80_bus_requested: bool,        // 68K has requested Z80 bus
    z80_reset: bool,                // Z80 in reset state
    ym2612: Ym2612,
    psg: Psg,
    audio_buffer: Vec<f32>,         // frame's audio samples
    audio_sample_counter: f64,      // fractional sample accumulator
}
```

## Test Strategy

### Tier 1: Z80 Instruction Tests

jsmoo Z80 test vectors (JSON format). Same harness pattern as m68000-tests: load initial state, execute one instruction, compare final state + cycles. Per-instruction files, all prefix groups.

### Tier 2: Sound Chip Unit Tests

- YM2612: register decode, envelope state machine, algorithm routing, timer overflow, DAC mode
- PSG: tone period -> frequency, volume attenuation, LFSR polynomial, latch/data protocol

### Tier 3: Integration

- Load Sonic, run 300+ frames, verify audio buffer is non-silent
- Check YM2612 key-on events occurred
- Check PSG has active channels
- Not golden-audio (too fragile) — just "audio is happening"

### Verus Specs

- Z80 decoder: opcode -> instruction exhaustiveness
- YM2612 algorithm routing: 8 topologies -> correct operator connections
- PSG LFSR: polynomial feedback correctness

## Milestones

1. Z80 passes jsmoo instruction tests
2. PSG produces square waves (ring collect SFX)
3. YM2612 produces FM tones (basic channel test)
4. Sonic produces audible music and SFX
5. Audio output at correct tempo with no buffer underruns

//! YM2612 (OPN2) FM synthesis emulation.
//!
//! The YM2612 is a 6-channel FM synthesizer with 4 operators per channel,
//! 8 algorithm topologies, per-operator ADSR envelopes, an LFO, timers,
//! and DAC mode on channel 6. In the Genesis it is written by the Z80 (and
//! occasionally the 68000) via ports `0x4000`-`0x4003` and produces stereo
//! audio output mixed with the PSG.
//!
//! Register writes use a two-step address/data protocol. Banks 0 and 1
//! control channels 1-3 and 4-6 respectively.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

// ── Lookup Tables ────────────────────────────────────────────────────────

/// Quarter-wave log-sin table. Maps a 10-bit phase index to a 12-bit
/// logarithmic magnitude: `sin_table[i] = -log2(sin(i * pi / 1024)) * 256`.
/// The full sine wave is reconstructed via symmetry in the synthesis loop.
static SIN_TABLE: LazyLock<[u16; 1024]> = LazyLock::new(|| {
    let mut table = [0u16; 1024];
    for (i, entry) in table.iter_mut().enumerate() {
        let phase = (i as f64 + 0.5) * std::f64::consts::PI / 1024.0;
        let sin_val = phase.sin();
        if sin_val > 0.0 {
            let log_sin = -sin_val.log2() * 256.0;
            *entry = (log_sin.round() as u16).min(0xFFF);
        } else {
            *entry = 0xFFF;
        }
    }
    table
});

/// Exponential table. Maps an 8-bit mantissa to a linear power-of-2 value:
/// `exp_table[i] = 2^(1 - i/256) * 1024`.
static EXP_TABLE: LazyLock<[u16; 256]> = LazyLock::new(|| {
    let mut table = [0u16; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let val = 2.0_f64.powf(1.0 - (i as f64) / 256.0) * 1024.0;
        *entry = (val.round() as u16) & 0x7FF;
    }
    table
});

/// Frequency multiplier table. MUL=0 means 0.5x (halved frequency).
/// Values are doubled so we can use integer arithmetic and divide by 2.
const MULTIPLY_TABLE: [u8; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30];

/// Simplified detune offsets indexed by `[detune & 3][key_code >> 2]`.
/// Detune values 0-3 map to positive offsets, 4-7 negate them.
/// These are coarse approximations; exact hardware tables can be refined later.
const DETUNE_TABLE: [[i32; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 0], // DT = 0: no detune
    [0, 0, 0, 1, 1, 2, 2, 3], // DT = 1: small
    [0, 1, 1, 2, 2, 3, 4, 5], // DT = 2: medium
    [0, 1, 2, 3, 4, 5, 6, 7], // DT = 3: large
];

// ── Operator register mapping ────────────────────────────────────────────

/// The YM2612's operator index in registers is *not* sequential.
/// Register slot 0→op1, 1→op3, 2→op2, 3→op4.
const SLOT_TO_OPERATOR: [usize; 4] = [0, 2, 1, 3];

// ── Envelope state ──────────────────────────────────────────────────────

/// ADSR envelope state machine. Each operator transitions through these
/// states based on key-on/off events and the current attenuation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvState {
    /// Attenuation decreasing from 1023 toward 0.
    Attack,
    /// Attenuation increasing toward the sustain level.
    Decay,
    /// Attenuation increasing at the sustain rate until key-off.
    Sustain,
    /// Attenuation increasing toward maximum (1023) after key-off.
    Release,
}

// ── Operator ─────────────────────────────────────────────────────────────

/// A single FM operator with phase generator and envelope generator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operator {
    /// 20-bit phase accumulator.
    phase: u32,
    /// Current envelope attenuation (0 = maximum volume, 1023 = silent).
    envelope: u16,
    /// Current ADSR state.
    env_state: EnvState,
    /// Total level (0-127). Higher values = quieter.
    total_level: u8,
    /// Sustain level (0-15). Mapped to attenuation as `sl * 32`.
    sustain_level: u8,
    /// Attack rate (0-31).
    attack_rate: u8,
    /// Decay rate (0-31).
    decay_rate: u8,
    /// Sustain rate (secondary decay rate, 0-31).
    sustain_rate: u8,
    /// Release rate (0-15).
    release_rate: u8,
    /// Frequency multiplier (0-15). 0 = 0.5x.
    multiply: u8,
    /// Detune (0-7). Bits 0-2: magnitude, bit 2 inverts sign.
    detune: u8,
    /// Key scale (0-3). Scales envelope rates by note pitch.
    key_scale: u8,
    /// Whether this operator's key is currently on.
    key_on: bool,
    /// SSG-EG mode (0-15). Not fully emulated in this initial impl.
    ssg_eg: u8,
    /// Last computed output sample (used for feedback on op1).
    output: i32,
    /// Previous output sample (averaged with `output` for feedback).
    prev_output: i32,
}

impl Operator {
    fn new() -> Self {
        Self {
            phase: 0,
            envelope: 1023,
            env_state: EnvState::Release,
            total_level: 127,
            sustain_level: 0,
            attack_rate: 0,
            decay_rate: 0,
            sustain_rate: 0,
            release_rate: 0,
            multiply: 0,
            detune: 0,
            key_scale: 0,
            key_on: false,
            ssg_eg: 0,
            output: 0,
            prev_output: 0,
        }
    }

    /// Trigger key-on: reset phase, start the attack phase from max attenuation.
    fn key_on(&mut self) {
        if self.key_on {
            return;
        }
        self.key_on = true;
        self.phase = 0;
        self.envelope = 1023;
        self.env_state = EnvState::Attack;
    }

    /// Trigger key-off: transition to the release phase.
    fn key_off(&mut self) {
        if !self.key_on {
            return;
        }
        self.key_on = false;
        self.env_state = EnvState::Release;
    }

    /// Compute the key code from the channel's fnum and block, used to scale
    /// envelope rates.
    fn key_code(fnum: u16, block: u8) -> u8 {
        // Simplified: use the block and top 2 bits of fnum
        let note = (fnum >> 9) as u8; // top 2 bits of 11-bit fnum
        (block << 2) | (note & 3)
    }

    /// Calculate effective envelope rate with key-scaling applied.
    fn effective_rate(&self, base_rate: u8, fnum: u16, block: u8) -> u8 {
        if base_rate == 0 {
            return 0;
        }
        let kc = Self::key_code(fnum, block);
        let ks_shift = 3u8.saturating_sub(self.key_scale);
        let scaled = (base_rate * 2) + (kc >> ks_shift);
        scaled.min(63)
    }

    /// Determine the attenuation change for this envelope update tick.
    fn envelope_increment(rate: u8, is_attack: bool) -> u16 {
        if rate == 0 {
            return 0;
        }
        if is_attack && rate >= 62 {
            return 1023; // Instant attack for very high rates
        }
        let shift = 11u8.saturating_sub(rate / 4);
        1u16 << shift.min(10)
    }

    /// Advance the envelope generator by one sample tick.
    fn update_envelope(&mut self, fnum: u16, block: u8) {
        match self.env_state {
            EnvState::Attack => {
                let rate = self.effective_rate(self.attack_rate, fnum, block);
                if rate >= 62 {
                    // Instant attack
                    self.envelope = 0;
                } else {
                    let step = Self::envelope_increment(rate, true);
                    // Attack uses exponential curve: decrement is proportional
                    // to current attenuation.
                    let decrement = ((self.envelope as u32 * step as u32) >> 12) as u16;
                    self.envelope = self.envelope.saturating_sub(decrement.max(1));
                }
                if self.envelope == 0 {
                    self.envelope = 0;
                    self.env_state = EnvState::Decay;
                }
            }
            EnvState::Decay => {
                let rate = self.effective_rate(self.decay_rate, fnum, block);
                let step = Self::envelope_increment(rate, false);
                self.envelope = (self.envelope + step).min(1023);
                let target = (self.sustain_level as u16) * 32;
                if self.envelope >= target {
                    self.envelope = target.min(1023);
                    self.env_state = EnvState::Sustain;
                }
            }
            EnvState::Sustain => {
                let rate = self.effective_rate(self.sustain_rate, fnum, block);
                let step = Self::envelope_increment(rate, false);
                self.envelope = (self.envelope + step).min(1023);
            }
            EnvState::Release => {
                // Release rate is 4-bit, doubled to get effective 5-bit rate range
                let rate = self.effective_rate(self.release_rate * 2 + 1, fnum, block);
                let step = Self::envelope_increment(rate, false);
                self.envelope = (self.envelope + step).min(1023);
            }
        }
    }

    /// Compute phase increment for this operator given the channel frequency.
    fn phase_increment(&self, fnum: u16, block: u8) -> u32 {
        let base_freq = (fnum as u32) << (block as u32);
        let mult = MULTIPLY_TABLE[self.multiply as usize] as u32;
        let phase_inc = (base_freq * mult) >> 1;

        // Apply detune
        let dt_mag = (self.detune & 3) as usize;
        let kc = Self::key_code(fnum, block) as usize;
        let dt_index = (kc >> 2).min(7);
        let dt_offset = DETUNE_TABLE[dt_mag][dt_index] as u32;

        if self.detune & 4 != 0 {
            phase_inc.saturating_sub(dt_offset)
        } else {
            phase_inc + dt_offset
        }
    }

    /// Produce the output sample for this operator, given optional modulation
    /// input from another operator (in the phase domain).
    fn compute(&mut self, phase_inc: u32, modulation: i32) -> i32 {
        // Advance phase
        self.phase = self.phase.wrapping_add(phase_inc);

        // Apply modulation to phase for sine lookup
        let modulated_phase = self.phase.wrapping_add((modulation << 1) as u32);

        // Extract components from the 20-bit phase
        let phase_10 = (modulated_phase >> 10) & 0x3FF;
        let sign = (modulated_phase >> 20) & 1;
        let half = (modulated_phase >> 19) & 1;

        // Quarter-wave symmetry
        let index = if half != 0 {
            1023 - (phase_10 & 0x3FF)
        } else {
            phase_10
        } as usize;

        // Sine lookup (log domain)
        let log_sin = SIN_TABLE[index];

        // Add envelope attenuation (and total level) in log domain
        let atten = log_sin as u32 + (self.envelope as u32) * 4 + (self.total_level as u32) * 8;

        // Clamp to prevent out-of-range values
        if atten >= 4096 {
            self.prev_output = self.output;
            self.output = 0;
            return 0;
        }

        // Log-to-linear conversion via exp table
        let mantissa = (atten & 0xFF) as usize;
        let exponent = atten >> 8;
        let linear = (EXP_TABLE[mantissa] as i32) >> exponent;

        let output = if sign != 0 { -linear } else { linear };
        self.prev_output = self.output;
        self.output = output;
        output
    }
}

// ── Channel ──────────────────────────────────────────────────────────────

/// One of six FM channels, containing four operators connected by an algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    /// The four FM operators.
    operators: [Operator; 4],
    /// 11-bit frequency number.
    fnum: u16,
    /// 3-bit octave block.
    block: u8,
    /// Algorithm topology (0-7).
    algorithm: u8,
    /// Feedback level for operator 1 (0-7, where 0 = no feedback).
    feedback: u8,
    /// Left speaker enabled.
    panning_left: bool,
    /// Right speaker enabled.
    panning_right: bool,
}

impl Channel {
    fn new() -> Self {
        Self {
            operators: [
                Operator::new(),
                Operator::new(),
                Operator::new(),
                Operator::new(),
            ],
            fnum: 0,
            block: 0,
            algorithm: 0,
            feedback: 0,
            panning_left: true,
            panning_right: true,
        }
    }

    /// Synthesize one sample from this channel using the configured algorithm.
    /// Returns (left, right) in raw integer form.
    fn output_sample(&mut self) -> (i32, i32) {
        let ops = &mut self.operators;

        // Precompute phase increments for all 4 operators
        let inc0 = ops[0].phase_increment(self.fnum, self.block);
        let inc1 = ops[1].phase_increment(self.fnum, self.block);
        let inc2 = ops[2].phase_increment(self.fnum, self.block);
        let inc3 = ops[3].phase_increment(self.fnum, self.block);

        // Feedback for operator 1
        let fb_mod = if self.feedback > 0 {
            (ops[0].prev_output + ops[0].output) >> (9 - self.feedback as i32)
        } else {
            0
        };

        // Run operators through the selected algorithm
        let out = match self.algorithm {
            0 => {
                // op1 → op2 → op3 → op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, o1);
                let o3 = ops[2].compute(inc2, o2);
                ops[3].compute(inc3, o3)
            }
            1 => {
                // (op1 + op2) → op3 → op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, 0);
                let o3 = ops[2].compute(inc2, o1 + o2);
                ops[3].compute(inc3, o3)
            }
            2 => {
                // (op2 + op1→op3) → op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, 0);
                let o3 = ops[2].compute(inc2, o1);
                ops[3].compute(inc3, o2 + o3)
            }
            3 => {
                // (op1→op2 + op3) → op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, o1);
                let o3 = ops[2].compute(inc2, 0);
                ops[3].compute(inc3, o2 + o3)
            }
            4 => {
                // (op1→op2) + (op3→op4) → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, o1);
                let o3 = ops[2].compute(inc2, 0);
                let o4 = ops[3].compute(inc3, o3);
                o2 + o4
            }
            5 => {
                // op1 → (op2 + op3 + op4) → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, o1);
                let o3 = ops[2].compute(inc2, o1);
                let o4 = ops[3].compute(inc3, o1);
                o2 + o3 + o4
            }
            6 => {
                // (op1→op2) + op3 + op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, o1);
                let o3 = ops[2].compute(inc2, 0);
                let o4 = ops[3].compute(inc3, 0);
                o2 + o3 + o4
            }
            7 => {
                // op1 + op2 + op3 + op4 → out
                let o1 = ops[0].compute(inc0, fb_mod);
                let o2 = ops[1].compute(inc1, 0);
                let o3 = ops[2].compute(inc2, 0);
                let o4 = ops[3].compute(inc3, 0);
                o1 + o2 + o3 + o4
            }
            _ => 0,
        };

        // Update envelopes for all operators
        for op in &mut ops.iter_mut() {
            op.update_envelope(self.fnum, self.block);
        }

        // Apply panning
        let left = if self.panning_left { out } else { 0 };
        let right = if self.panning_right { out } else { 0 };
        (left, right)
    }
}

// ── YM2612 ───────────────────────────────────────────────────────────────

/// Yamaha YM2612 (OPN2) FM synthesizer.
///
/// Provides 6 channels of 4-operator FM synthesis with 8 algorithm topologies,
/// per-operator ADSR envelopes, stereo panning, LFO, timers, and DAC mode
/// on channel 6.
///
/// # Usage
///
/// Write registers via the two-step address/data protocol:
/// ```ignore
/// ym.write_address(0, 0x28);   // latch key-on register
/// ym.write_data(0, 0xF0);      // key-on all ops for channel 0
/// ```
///
/// Produce audio samples:
/// ```ignore
/// let (left, right) = ym.output_sample();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ym2612 {
    /// The six FM channels.
    channels: [Channel; 6],
    /// Whether DAC mode is enabled (replaces channel 6 FM output).
    dac_enabled: bool,
    /// DAC output value (signed, from the 8-bit register).
    dac_value: i16,
    /// Latched address register for each port/bank (0 and 1).
    address_latch: [u8; 2],
    /// Timer overflow status flags (bits 0-1).
    status: u8,
    /// 10-bit Timer A period register.
    timer_a_period: u16,
    /// Timer A internal counter.
    timer_a_counter: u16,
    /// 8-bit Timer B period register.
    timer_b_period: u8,
    /// Timer B internal counter (counts up, overflows at 256 * 16).
    timer_b_counter: u16,
    /// Timer control register (0x27).
    timer_control: u8,
    /// LFO enabled flag.
    lfo_enabled: bool,
    /// LFO frequency selector (0-7).
    lfo_frequency: u8,
    /// LFO internal counter.
    lfo_counter: u32,
    /// LFO phase accumulator.
    lfo_phase: u8,
    /// Global envelope generator timer.
    eg_timer: u32,
    /// EG cycle counter.
    eg_counter: u8,
}

impl Ym2612 {
    /// Create a new YM2612 in its power-on state.
    ///
    /// All channels are silenced (operators at max attenuation), DAC is off,
    /// timers are stopped, and all registers are zeroed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: [
                Channel::new(),
                Channel::new(),
                Channel::new(),
                Channel::new(),
                Channel::new(),
                Channel::new(),
            ],
            dac_enabled: false,
            dac_value: 0,
            address_latch: [0; 2],
            status: 0,
            timer_a_period: 0,
            timer_a_counter: 0,
            timer_b_period: 0,
            timer_b_counter: 0,
            timer_control: 0,
            lfo_enabled: false,
            lfo_frequency: 0,
            lfo_counter: 0,
            lfo_phase: 0,
            eg_timer: 0,
            eg_counter: 0,
        }
    }

    /// Latch the register address for a subsequent data write.
    ///
    /// `port` selects the register bank: 0 for channels 1-3, 1 for channels 4-6.
    pub fn write_address(&mut self, port: u8, val: u8) {
        let bank = (port & 1) as usize;
        self.address_latch[bank] = val;
    }

    /// Write data to the previously latched register address.
    ///
    /// `port` selects the register bank: 0 for channels 1-3, 1 for channels 4-6.
    pub fn write_data(&mut self, port: u8, val: u8) {
        let bank = (port & 1) as usize;
        let addr = self.address_latch[bank];
        self.write_register(bank, addr, val);
    }

    /// Read the status register. Returns timer overflow flags.
    /// Reading clears the overflow bits.
    #[must_use]
    pub fn read_status(&mut self) -> u8 {
        let s = self.status;
        // Reading status clears the timer flags (bits 0 and 1)
        self.status &= !0x03;
        s
    }

    /// Internal register write dispatch.
    fn write_register(&mut self, bank: usize, addr: u8, val: u8) {
        match addr {
            // ── Global registers (bank 0 only, but writes to bank 1 are ignored) ──
            0x22 if bank == 0 => {
                // LFO control
                self.lfo_enabled = val & 0x08 != 0;
                self.lfo_frequency = val & 0x07;
            }
            0x24 if bank == 0 => {
                // Timer A high 8 bits
                self.timer_a_period = (self.timer_a_period & 0x003) | (u16::from(val) << 2);
            }
            0x25 if bank == 0 => {
                // Timer A low 2 bits
                self.timer_a_period = (self.timer_a_period & 0x3FC) | u16::from(val & 0x03);
            }
            0x26 if bank == 0 => {
                // Timer B
                self.timer_b_period = val;
            }
            0x27 if bank == 0 => {
                // Timer control / Ch3 special mode
                self.timer_control = val;
                // Reset timer flags if requested
                if val & 0x10 != 0 {
                    self.status &= !0x01; // Reset Timer A flag
                }
                if val & 0x20 != 0 {
                    self.status &= !0x02; // Reset Timer B flag
                }
            }
            0x28 if bank == 0 => {
                // Key on/off
                let ch_sel = val & 0x07;
                let ch_idx = match ch_sel {
                    0..=2 => ch_sel as usize,
                    4..=6 => (ch_sel - 4 + 3) as usize,
                    _ => return, // Invalid channel
                };
                let ch = &mut self.channels[ch_idx];
                // Bits 4-7 select which operators get key-on
                for op_bit in 0..4u8 {
                    if val & (0x10 << op_bit) != 0 {
                        ch.operators[op_bit as usize].key_on();
                    } else {
                        ch.operators[op_bit as usize].key_off();
                    }
                }
            }
            0x2A if bank == 0 => {
                // DAC data
                self.dac_value = i16::from(val) - 128;
            }
            0x2B if bank == 0 => {
                // DAC enable
                self.dac_enabled = val & 0x80 != 0;
            }

            // ── Per-operator registers ───────────────────────────────────
            0x30..=0x9F => {
                let ch_raw = addr & 0x03;
                if ch_raw == 3 {
                    return; // Invalid channel slot
                }
                let ch_idx = ch_raw as usize + bank * 3;
                if ch_idx >= 6 {
                    return;
                }
                let slot = ((addr >> 2) & 0x03) as usize;
                let op_idx = SLOT_TO_OPERATOR[slot];
                let op = &mut self.channels[ch_idx].operators[op_idx];

                match addr & 0xF0 {
                    0x30 => {
                        // DT1/MUL
                        op.multiply = val & 0x0F;
                        op.detune = (val >> 4) & 0x07;
                    }
                    0x40 => {
                        // TL
                        op.total_level = val & 0x7F;
                    }
                    0x50 => {
                        // RS/AR
                        op.attack_rate = val & 0x1F;
                        op.key_scale = (val >> 6) & 0x03;
                    }
                    0x60 => {
                        // AM/DR (AM flag not yet used)
                        op.decay_rate = val & 0x1F;
                    }
                    0x70 => {
                        // SR
                        op.sustain_rate = val & 0x1F;
                    }
                    0x80 => {
                        // SL/RR
                        op.release_rate = val & 0x0F;
                        op.sustain_level = (val >> 4) & 0x0F;
                    }
                    0x90 => {
                        // SSG-EG
                        op.ssg_eg = val & 0x0F;
                    }
                    _ => {} // Should not happen given the 0x30..=0x9F range
                }
            }

            // ── Per-channel registers ────────────────────────────────────
            0xA0..=0xA2 => {
                // Frequency number low byte
                let ch_idx = (addr & 0x03) as usize + bank * 3;
                if ch_idx < 6 {
                    self.channels[ch_idx].fnum =
                        (self.channels[ch_idx].fnum & 0x700) | u16::from(val);
                }
            }
            0xA4..=0xA6 => {
                // Block / frequency number high bits
                // Note: on real hardware, writing to 0xA4-0xA6 latches the high
                // bits and writing to 0xA0-0xA2 commits both. We simplify by
                // committing immediately.
                let ch_idx = (addr & 0x03) as usize + bank * 3;
                if ch_idx < 6 {
                    self.channels[ch_idx].fnum =
                        (self.channels[ch_idx].fnum & 0x0FF) | (u16::from(val & 0x07) << 8);
                    self.channels[ch_idx].block = (val >> 3) & 0x07;
                }
            }
            0xB0..=0xB2 => {
                // Algorithm / Feedback
                let ch_idx = (addr & 0x03) as usize + bank * 3;
                if ch_idx < 6 {
                    self.channels[ch_idx].algorithm = val & 0x07;
                    self.channels[ch_idx].feedback = (val >> 3) & 0x07;
                }
            }
            0xB4..=0xB6 => {
                // Panning / AMS / FMS
                let ch_idx = (addr & 0x03) as usize + bank * 3;
                if ch_idx < 6 {
                    self.channels[ch_idx].panning_left = val & 0x80 != 0;
                    self.channels[ch_idx].panning_right = val & 0x40 != 0;
                    // AMS and FMS not yet used in this initial implementation
                }
            }
            _ => {
                // Unhandled register — ignore silently
            }
        }
    }

    /// Advance the timers by one sample tick.
    ///
    /// Timer A uses the 10-bit period directly; Timer B uses the 8-bit period
    /// scaled by 16.
    fn advance_timers(&mut self) {
        // Timer A
        if self.timer_control & 0x01 != 0 {
            self.timer_a_counter += 1;
            if self.timer_a_counter >= (1024 - self.timer_a_period) {
                self.timer_a_counter = 0;
                if self.timer_control & 0x04 != 0 {
                    self.status |= 0x01;
                }
            }
        }

        // Timer B (period * 16)
        if self.timer_control & 0x02 != 0 {
            self.timer_b_counter += 1;
            let period = (256 - u16::from(self.timer_b_period)) * 16;
            if self.timer_b_counter >= period {
                self.timer_b_counter = 0;
                if self.timer_control & 0x08 != 0 {
                    self.status |= 0x02;
                }
            }
        }
    }

    /// Produce one stereo audio sample.
    ///
    /// Called at the native YM2612 sample rate (~53.267 kHz). Returns `(left, right)`
    /// normalized to the range `[-1.0, 1.0]`.
    pub fn output_sample(&mut self) -> (f32, f32) {
        let mut left_sum: i32 = 0;
        let mut right_sum: i32 = 0;

        for ch_idx in 0..6 {
            // DAC replaces channel 6 (index 5) when enabled
            if ch_idx == 5 && self.dac_enabled {
                let dac_out = i32::from(self.dac_value) << 6;
                if self.channels[5].panning_left {
                    left_sum += dac_out;
                }
                if self.channels[5].panning_right {
                    right_sum += dac_out;
                }
                // Still need to advance envelopes for ch6 operators
                for op in &mut self.channels[5].operators {
                    op.update_envelope(self.channels[5].fnum, self.channels[5].block);
                }
                continue;
            }

            let (l, r) = self.channels[ch_idx].output_sample();
            left_sum += l;
            right_sum += r;
        }

        // Advance timers
        self.advance_timers();

        // Scale to float. 6 channels, each operator can output up to ~2047.
        // Algorithm 7 sums 4 ops, so max per channel is ~8188.
        // 6 channels -> max ~49128. Divide by a suitable constant.
        const SCALE: f32 = 1.0 / 32768.0;
        (left_sum as f32 * SCALE, right_sum as f32 * SCALE)
    }
}

impl Default for Ym2612 {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a register via bank 0 address/data protocol.
    fn write_reg(ym: &mut Ym2612, addr: u8, val: u8) {
        ym.write_address(0, addr);
        ym.write_data(0, val);
    }

    /// Helper: write a register via bank 1 address/data protocol.
    fn write_reg_bank1(ym: &mut Ym2612, addr: u8, val: u8) {
        ym.write_address(1, addr);
        ym.write_data(1, val);
    }

    #[test]
    fn register_write_and_read() {
        let mut ym = Ym2612::new();

        // Set algorithm 4 + feedback 3 on channel 0 (register 0xB0)
        write_reg(&mut ym, 0xB0, (3 << 3) | 4);
        assert_eq!(ym.channels[0].algorithm, 4);
        assert_eq!(ym.channels[0].feedback, 3);

        // Set frequency on channel 0: fnum=0x1A0, block=4
        // High byte first: block=4 (bits 5-3), fnum high 3 bits = 1 (bit A8)
        write_reg(&mut ym, 0xA4, (4 << 3) | 0x01); // block=4, fnum_hi=1
        write_reg(&mut ym, 0xA0, 0xA0); // fnum_lo=0xA0
        assert_eq!(ym.channels[0].fnum, 0x1A0);
        assert_eq!(ym.channels[0].block, 4);

        // Set TL=50 for ch0 op1 (slot 0 in register space, op index 0)
        // Register 0x40 = TL for slot 0, ch0
        write_reg(&mut ym, 0x40, 50);
        assert_eq!(ym.channels[0].operators[0].total_level, 50);

        // Set DT1=3/MUL=5 for ch0 op1
        write_reg(&mut ym, 0x30, (3 << 4) | 5);
        assert_eq!(ym.channels[0].operators[0].multiply, 5);
        assert_eq!(ym.channels[0].operators[0].detune, 3);

        // Bank 1: set algorithm 7 on channel 3 (first channel of bank 1)
        write_reg_bank1(&mut ym, 0xB0, 7);
        assert_eq!(ym.channels[3].algorithm, 7);
        assert_eq!(ym.channels[3].feedback, 0);

        // Verify DAC enable
        write_reg(&mut ym, 0x2B, 0x80);
        assert!(ym.dac_enabled);
        write_reg(&mut ym, 0x2B, 0x00);
        assert!(!ym.dac_enabled);
    }

    #[test]
    fn key_on_starts_attack() {
        let mut ym = Ym2612::new();

        // Configure channel 0 op1 with a non-zero attack rate
        write_reg(&mut ym, 0x50, 31); // AR=31 for slot 0 ch0 (op1)
        write_reg(&mut ym, 0x40, 0); // TL=0 so it's audible

        // Verify initial state is Release
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Release);

        // Key-on channel 0, op1 (bit 4 = op1)
        write_reg(&mut ym, 0x28, 0x10); // Key-on op1 for ch0
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Attack);
        assert!(ym.channels[0].operators[0].key_on);

        // Phase should have been reset
        assert_eq!(ym.channels[0].operators[0].phase, 0);
        // Envelope should start at max attenuation
        assert_eq!(ym.channels[0].operators[0].envelope, 1023);
    }

    #[test]
    fn key_off_starts_release() {
        let mut ym = Ym2612::new();

        // Set up and key-on
        write_reg(&mut ym, 0x50, 31); // AR=31
        write_reg(&mut ym, 0x28, 0xF0); // Key-on all ops for ch0

        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Attack);

        // Advance a few samples so envelope progresses
        for _ in 0..100 {
            ym.output_sample();
        }

        // Key-off (write 0 to key-on bits for ch0)
        write_reg(&mut ym, 0x28, 0x00);

        // All operators should now be in Release
        for op_idx in 0..4 {
            assert_eq!(
                ym.channels[0].operators[op_idx].env_state,
                EnvState::Release,
                "operator {op_idx} should be in Release after key-off"
            );
            assert!(!ym.channels[0].operators[op_idx].key_on);
        }
    }

    #[test]
    fn dac_mode_replaces_channel_6() {
        let mut ym = Ym2612::new();

        // Enable DAC
        write_reg(&mut ym, 0x2B, 0x80);
        assert!(ym.dac_enabled);

        // Write DAC value (0x80 = center/zero after offset, 0xFF = max positive)
        write_reg(&mut ym, 0x2A, 0xFF);
        assert_eq!(ym.dac_value, 127); // 0xFF - 128 = 127

        // Ensure channel 6 panning is on
        write_reg_bank1(&mut ym, 0xB6, 0xC0); // Both L+R

        // Generate a sample — channel 6 should use DAC output
        let (left, right) = ym.output_sample();

        // DAC output should be 127 << 6 = 8128, scaled by 1/32768 ≈ 0.248
        let expected = 127.0 * 64.0 / 32768.0;
        assert!(
            (left - expected).abs() < 0.01,
            "DAC left output {left} should be near {expected}"
        );
        assert!(
            (right - expected).abs() < 0.01,
            "DAC right output {right} should be near {expected}"
        );

        // Write a different DAC value
        write_reg(&mut ym, 0x2A, 0x00);
        assert_eq!(ym.dac_value, -128); // 0 - 128 = -128

        let (left2, _) = ym.output_sample();
        let expected2 = -128.0 * 64.0 / 32768.0;
        assert!(
            (left2 - expected2).abs() < 0.01,
            "DAC left output {left2} should be near {expected2}"
        );
    }

    #[test]
    fn algorithm_routing() {
        let mut ym = Ym2612::new();

        // Set channel 0 to algorithm 7 (all 4 ops output independently)
        write_reg(&mut ym, 0xB0, 0x07); // algo=7, feedback=0
        write_reg(&mut ym, 0xB4, 0xC0); // Both L+R panning

        // Set a frequency so operators produce non-zero phase increments
        write_reg(&mut ym, 0xA4, (3 << 3) | 0x03); // block=3, fnum_hi=3
        write_reg(&mut ym, 0xA0, 0x00); // fnum_lo=0 -> fnum=0x300

        // Set all 4 operators: TL=0, AR=31, MUL=1
        // Operator slot mapping: slot 0=op1(0x30), slot 1=op3(0x34), slot 2=op2(0x38), slot 3=op4(0x3C)
        for slot_offset in [0x00u8, 0x04, 0x08, 0x0C] {
            write_reg(&mut ym, 0x30 + slot_offset, 0x01); // MUL=1
            write_reg(&mut ym, 0x40 + slot_offset, 0); // TL=0
            write_reg(&mut ym, 0x50 + slot_offset, 31); // AR=31
            write_reg(&mut ym, 0x80 + slot_offset, 0x0F); // SL=0, RR=15
        }

        // Key-on all 4 operators for channel 0
        write_reg(&mut ym, 0x28, 0xF0);

        // Advance enough for attack to complete (envelope → 0)
        for _ in 0..500 {
            ym.output_sample();
        }

        // At this point all 4 operators should have envelopes near 0 (loud).
        // With algorithm 7, all 4 contribute to output.
        // Generate a sample and verify it's non-zero.
        let (left, right) = ym.output_sample();
        assert!(
            left.abs() > 0.001 || right.abs() > 0.001,
            "algorithm 7 should produce audible output, got L={left}, R={right}"
        );

        // Now test algorithm 0 (serial chain): only op4 goes to output.
        // Disable all ops first.
        write_reg(&mut ym, 0x28, 0x00);
        let mut ym2 = Ym2612::new();

        write_reg(&mut ym2, 0xB0, 0x00); // algo=0, feedback=0
        write_reg(&mut ym2, 0xB4, 0xC0);
        write_reg(&mut ym2, 0xA4, (3 << 3) | 0x03);
        write_reg(&mut ym2, 0xA0, 0x00);

        // Only set TL=0 for op4 (slot 3 = register offset 0x0C), others silent
        for slot_offset in [0x00u8, 0x04, 0x08, 0x0C] {
            write_reg(&mut ym2, 0x30 + slot_offset, 0x01);
            write_reg(&mut ym2, 0x50 + slot_offset, 31);
            write_reg(&mut ym2, 0x80 + slot_offset, 0x0F);
            // TL: ops 1-3 at max attenuation, op4 audible
            if slot_offset == 0x0C {
                write_reg(&mut ym2, 0x40 + slot_offset, 0); // op4 loud
            } else {
                write_reg(&mut ym2, 0x40 + slot_offset, 127); // ops 1-3 silent
            }
        }

        write_reg(&mut ym2, 0x28, 0xF0);

        // Advance for envelope to progress
        for _ in 0..500 {
            ym2.output_sample();
        }

        // Even with algo 0, op4 receives modulation from the chain.
        // Since modulators are silent (TL=127), op4 is essentially unmodulated
        // and should still produce some output.
        let (l, r) = ym2.output_sample();
        // The carrier (op4) should produce sound
        assert!(
            l.abs() > 0.0001 || r.abs() > 0.0001,
            "algorithm 0 carrier (op4) should produce output, got L={l}, R={r}"
        );
    }

    #[test]
    fn silent_on_init() {
        let mut ym = Ym2612::new();

        // All operators at max attenuation (TL=127, envelope=1023)
        let (left, right) = ym.output_sample();
        assert!(
            left.abs() < f32::EPSILON && right.abs() < f32::EPSILON,
            "new YM2612 should output silence, got L={left}, R={right}"
        );
    }

    #[test]
    fn timer_a_overflow() {
        let mut ym = Ym2612::new();

        // Set a very short Timer A period: period = 1023, so counter overflows at
        // 1024 - 1023 = 1 tick
        write_reg(&mut ym, 0x24, 0xFF); // High 8 bits = 0xFF -> period bits 9-2
        write_reg(&mut ym, 0x25, 0x03); // Low 2 bits = 3 -> total period = 0x3FF = 1023

        assert_eq!(ym.timer_a_period, 1023);

        // Enable Timer A run + Timer A overflow flag
        // Bit 0 = Timer A run, Bit 2 = Timer A enable (flag generation)
        write_reg(&mut ym, 0x27, 0x05);

        // Status should start clean
        assert_eq!(
            ym.status & 0x01,
            0,
            "timer A flag should be clear initially"
        );

        // Advance samples until the timer overflows (period 1023 means
        // overflow at 1024 - 1023 = 1 tick)
        ym.output_sample();

        // Timer A flag should now be set
        assert_ne!(
            ym.status & 0x01,
            0,
            "timer A overflow flag should be set after {} ticks",
            1
        );

        // Reading status should return the flag and then clear it
        let status = ym.read_status();
        assert_ne!(
            status & 0x01,
            0,
            "read_status should return the overflow flag"
        );
        assert_eq!(
            ym.status & 0x01,
            0,
            "timer A flag should be cleared after reading"
        );
    }
}

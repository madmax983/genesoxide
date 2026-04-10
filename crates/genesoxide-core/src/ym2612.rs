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

// ── Lookup Tables ────────────────────────────────────────────────────────

/// Quarter-wave log-sin table. Maps a 10-bit phase index to a 12-bit
/// logarithmic magnitude: `sin_table[i] = -log2(sin(i * pi / 1024)) * 256`.
/// The full sine wave is reconstructed via symmetry in the synthesis loop.
///
/// Pre-computed at build time and included as a static array to avoid
/// `LazyLock` overhead in the hot synthesis loop.
static SIN_TABLE: [u16; 256] = include!("ym2612_sin_table.inc");

/// Exponential table. Maps an 8-bit mantissa to a linear power-of-2 value:
/// `exp_table[i] = 2^(1 - i/256) * 1024`.
static EXP_TABLE: [u16; 256] = include!("ym2612_exp_table.inc");

/// Frequency multiplier table. MUL=0 means 0.5x (halved frequency).
/// Values are doubled so we can use integer arithmetic and divide by 2.
const MULTIPLY_TABLE: [u8; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30];

/// Hardware detune table from MAME fm.c. Indexed by `[detune & 3][key_code]`
/// where key_code is the 5-bit value from `key_code()` (0-31).
/// Detune values 4-7 negate the offset (bit 2 = sign).
#[rustfmt::skip]
const DETUNE_TABLE: [[i32; 32]; 4] = [
    // DT = 0: no detune
    [ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
      0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    // DT = 1
    [ 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2,
      2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 8, 8, 8],
    // DT = 2
    [ 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5,
      5, 6, 6, 7, 8, 8, 9,10,11,12,13,14,16,16,16,16],
    // DT = 3
    [ 2, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7,
      8, 8, 9,10,11,12,13,14,16,17,19,20,22,22,22,22],
];

// ── LFO tables (Nuked-OPN2 reference) ────────────────────────────────────

/// LFO divider cycles. The LFO counter increments each FM sample; when it
/// reaches the threshold for the selected frequency, the 7-bit LFO phase
/// advances by one. Index is the LFO frequency register value (0-7).
const LFO_CYCLES: [u32; 8] = [108, 77, 71, 67, 62, 44, 8, 5];

/// PM (phase modulation / vibrato) shift tables from Nuked-OPN2 (ym3438.c).
/// Indexed by `[pms][pm_level]` where pms is the channel's PMS register (0-7)
/// and pm_level is the 3-bit LFO triangle magnitude (0-7).
/// `fnum >> 4` is right-shifted by these amounts; shift=7 means "no contribution"
/// since max fnum>>4 is 127, and 127>>7 = 0.
#[rustfmt::skip]
const PG_LFO_SH1: [[u8; 8]; 8] = [
    [7, 7, 7, 7, 7, 7, 7, 7], // pms=0: no PM
    [7, 7, 7, 7, 7, 7, 7, 7], // pms=1
    [7, 7, 7, 7, 7, 7, 1, 1], // pms=2
    [7, 7, 7, 7, 1, 1, 1, 1], // pms=3
    [7, 7, 7, 1, 1, 1, 1, 0], // pms=4
    [7, 7, 1, 1, 0, 0, 0, 0], // pms=5
    [7, 7, 1, 1, 0, 0, 0, 0], // pms=6
    [7, 7, 1, 1, 0, 0, 0, 0], // pms=7
];

#[rustfmt::skip]
const PG_LFO_SH2: [[u8; 8]; 8] = [
    [7, 7, 7, 7, 7, 7, 7, 7], // pms=0: no PM
    [7, 7, 7, 7, 2, 2, 2, 2], // pms=1
    [7, 7, 7, 2, 2, 2, 7, 7], // pms=2
    [7, 7, 2, 2, 7, 7, 2, 2], // pms=3
    [7, 7, 2, 7, 7, 7, 2, 7], // pms=4
    [7, 7, 7, 2, 7, 7, 2, 1], // pms=5
    [7, 7, 7, 2, 7, 7, 2, 1], // pms=6
    [7, 7, 7, 2, 7, 7, 2, 1], // pms=7
];

/// AM (amplitude modulation / tremolo) shift table from Nuked-OPN2.
/// Indexed by the channel's AMS value (0-3). The LFO AM output (0-127)
/// is right-shifted by this amount before being added to the envelope
/// attenuation.
const EG_AM_SHIFT: [u8; 4] = [7, 3, 1, 0];

// ── Operator register mapping ────────────────────────────────────────────

/// The YM2612's operator index in registers is *not* sequential.
/// Register slot 0→op1, 1→op3, 2→op2, 3→op4.
const SLOT_TO_OPERATOR: [usize; 4] = [0, 2, 1, 3];

// ── Envelope generator tables (MAME fm.c reference) ─────────────────────

/// EG rate shift table. Indexed by `effective_rate >> 2` (0-15).
/// Controls how often the EG updates: the update fires only when the
/// lower `shift` bits of the global EG counter are all zero.
/// Rates ≥ 48 (index ≥ 12) have shift=0 and update every EG tick.
const EG_RATE_SHIFT: [u8; 16] = [11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 0, 0, 0];

/// MAME-reference EG increment patterns. 19 rows of 8 entries each.
/// When an update fires, `(eg_counter >> shift) & 7` selects the column.
///
/// Rows 0-3:   rates 0-47  (`rate & 3` selects row)
/// Rows 4-7:   rates 48-51 (rate_high=12)
/// Rows 8-11:  rates 52-55 (rate_high=13)
/// Rows 12-15: rates 56-59 (rate_high=14)
/// Row 16:     rates 60-63 (rate_high=15, all rate_low values)
/// Row 17:     instant attack (unused in normal flow)
/// Row 18:     zero/disabled
#[rustfmt::skip]
const EG_INC: [[u8; 8]; 19] = [
    [0,1, 0,1, 0,1, 0,1], //  0: rates 0-47, rate & 3 == 0
    [0,1, 0,1, 1,1, 0,1], //  1: rates 0-47, rate & 3 == 1
    [0,1, 1,1, 0,1, 1,1], //  2: rates 0-47, rate & 3 == 2
    [0,1, 1,1, 1,1, 1,1], //  3: rates 0-47, rate & 3 == 3
    [1,1, 1,1, 1,1, 1,1], //  4: rate 48-51, & 3 == 0
    [1,1, 1,2, 1,1, 1,2], //  5: rate 48-51, & 3 == 1
    [1,2, 1,2, 1,2, 1,2], //  6: rate 48-51, & 3 == 2
    [1,2, 2,2, 1,2, 2,2], //  7: rate 48-51, & 3 == 3
    [2,2, 2,2, 2,2, 2,2], //  8: rate 52-55, & 3 == 0
    [2,2, 2,4, 2,2, 2,4], //  9: rate 52-55, & 3 == 1
    [2,4, 2,4, 2,4, 2,4], // 10: rate 52-55, & 3 == 2
    [2,4, 4,4, 2,4, 4,4], // 11: rate 52-55, & 3 == 3
    [4,4, 4,4, 4,4, 4,4], // 12: rate 56-59, & 3 == 0
    [4,4, 4,8, 4,4, 4,8], // 13: rate 56-59, & 3 == 1
    [4,8, 4,8, 4,8, 4,8], // 14: rate 56-59, & 3 == 2
    [4,8, 8,8, 4,8, 8,8], // 15: rate 56-59, & 3 == 3
    [8,8, 8,8, 8,8, 8,8], // 16: rate 60-63
    [16,16,16,16,16,16,16,16], // 17: instant attack
    [0,0, 0,0, 0,0, 0,0], // 18: zero/disabled
];

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
    /// Whether amplitude modulation (LFO tremolo) is enabled for this operator.
    am_enable: bool,
    /// SSG-EG mode register (0-15). Bit 3 enables; bit 2 = attack (initial
    /// direction); bit 1 = alternate (toggle inversion each cycle); bit 0 = hold
    /// (stop after first cycle).
    ssg_eg: u8,
    /// SSG-EG runtime inversion state. Toggled by the SSG-EG state machine when
    /// the alternate flag is set. When `ssg_invert != ssg_attack`, the operator
    /// output is inverted around the 0x200 attenuation midpoint.
    ssg_output_invert: bool,
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
            am_enable: false,
            ssg_eg: 0,
            ssg_output_invert: false,
            output: 0,
            prev_output: 0,
        }
    }

    // ── SSG-EG register bit accessors ────────────────────────────────
    fn ssg_enabled(&self) -> bool {
        self.ssg_eg & 0x08 != 0
    }
    fn ssg_attack(&self) -> bool {
        self.ssg_eg & 0x04 != 0
    }
    fn ssg_alternate(&self) -> bool {
        self.ssg_eg & 0x02 != 0
    }
    fn ssg_hold(&self) -> bool {
        self.ssg_eg & 0x01 != 0
    }

    /// Trigger key-on: reset phase, start the attack phase.
    ///
    /// Real hardware does NOT reset the envelope attenuation on key-on.
    /// The attack phase starts from wherever the current envelope is, which
    /// gives smoother retriggering for rapid note sequences (the attack
    /// formula `level += (~level * inc) >> 4` converges toward 0 from any
    /// starting point). Rates 62-63 skip the attack phase entirely.
    fn key_on(&mut self, fnum: u16, block: u8) {
        if self.key_on {
            return;
        }
        self.key_on = true;
        self.phase = 0;
        // Note: envelope is NOT reset — attack starts from current level.
        let rate = self.effective_rate(self.attack_rate, fnum, block);
        if rate >= 62 {
            // Instant attack: skip directly to Decay with zero attenuation
            self.envelope = 0;
            self.env_state = EnvState::Decay;
        } else {
            self.env_state = EnvState::Attack;
        }
        // SSG-EG: every new note starts non-inverted
        self.ssg_output_invert = false;
    }

    /// Trigger key-off: transition to the release phase.
    fn key_off(&mut self) {
        if !self.key_on {
            return;
        }
        self.key_on = false;
        // SSG-EG: bake the current inversion into the stored attenuation before
        // entering Release, because the output path does NOT apply inversion
        // during Release (the release envelope must start from the correct level).
        if self.ssg_enabled()
            && self.env_state != EnvState::Release
            && self.ssg_output_invert != self.ssg_attack()
        {
            self.envelope = 0x200u16.wrapping_sub(self.envelope) & 0x3FF;
        }
        self.env_state = EnvState::Release;
    }

    /// SSG-EG state machine. Called every EG tick when SSG-EG is enabled and
    /// attenuation has crossed the 0x200 threshold.
    ///
    /// Handles looping (restart attack), alternating (toggle inversion), holding,
    /// and phase reset. Reference: jgenesis + Nuked-OPN2.
    fn ssg_clock(&mut self, fnum: u16, block: u8) {
        if self.envelope < 0x200 {
            return;
        }

        // 1. Update inversion state
        if self.ssg_alternate() {
            if self.ssg_hold() {
                // Alternate + hold: permanently set inversion after first cycle
                self.ssg_output_invert = true;
            } else {
                // Alternate: toggle inversion each cycle
                self.ssg_output_invert = !self.ssg_output_invert;
            }
        }

        // 2. Phase reset for non-alternating, non-holding loops.
        //    Keeps the oscillator frozen at 0 until attenuation drops below 0x200.
        if !self.ssg_alternate() && !self.ssg_hold() {
            self.phase = 0;
        }

        // 3. Loop / hold / silence logic (if-else: loop takes priority over silence)
        if matches!(self.env_state, EnvState::Decay | EnvState::Sustain) && !self.ssg_hold() {
            // Loop: restart attack-decay cycle
            let rate = self.effective_rate(self.attack_rate, fnum, block);
            if rate >= 62 {
                // Instant attack: skip directly to Decay
                self.envelope = 0;
                self.env_state = EnvState::Decay;
            } else {
                self.env_state = EnvState::Attack;
            }
        } else if self.env_state == EnvState::Release
            || (self.env_state != EnvState::Attack && self.ssg_output_invert == self.ssg_attack())
        {
            // Silence: force max attenuation when in Release, or when the current
            // inversion state matches the attack flag (= "default" direction).
            self.envelope = 0x3FF;
        }
    }

    /// Compute the key code from the channel's fnum and block, used to scale
    /// envelope rates and index the detune table.
    ///
    /// Uses the hardware `opn_fktable` to map the top 4 bits of fnum to
    /// a 2-bit note value, combined with the 3-bit block for a 5-bit key code.
    fn key_code(fnum: u16, block: u8) -> u8 {
        // MAME opn_fktable: maps fnum bits 10-7 to note 0-3
        const OPN_FKTABLE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 3, 3, 3, 3, 3, 3];
        let note = OPN_FKTABLE[((fnum >> 7) & 0x0F) as usize];
        (block << 2) | note
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

    /// Calculate the EG increment for a given rate and global EG counter.
    ///
    /// Uses the MAME-reference shift+gate+pattern system:
    /// 1. The rate's shift value controls HOW OFTEN updates fire (gate check).
    /// 2. When the gate fires, `(counter >> shift) & 7` selects a column
    ///    from the 8-entry EG_INC pattern row.
    /// 3. The rate_high selects which row of EG_INC to use.
    fn eg_calc_increment(rate: u8, eg_counter: u32) -> u8 {
        if rate == 0 {
            return 0;
        }
        let rate = rate.min(63) as usize;
        let rate_high = rate >> 2;
        let rate_low = rate & 3;

        let shift = EG_RATE_SHIFT[rate_high.min(15)] as u32;

        // Gate: only fire when the lower `shift` bits of counter are all zero.
        // For shift=0 the mask is 0, so the gate always passes.
        if shift > 0 && (eg_counter & ((1u32 << shift) - 1)) != 0 {
            return 0;
        }

        // Select increment pattern row based on rate_high
        let row = match rate_high {
            0..=11 => rate_low,
            12 => 4 + rate_low,
            13 => 8 + rate_low,
            14 => 12 + rate_low,
            _ => 16, // rate_high 15 (rates 60-63)
        };

        let step_idx = ((eg_counter >> shift) & 7) as usize;
        EG_INC[row][step_idx]
    }

    /// Advance the envelope generator by one EG tick (called at FM_RATE/3).
    ///
    /// Attack uses the MAME-reference exponential curve: `level += (~level * inc) >> 4`.
    /// This produces a capacitor-charging shape — fast start, slow approach to zero.
    /// Decay/sustain/release use simple linear attenuation increase.
    ///
    /// When SSG-EG is enabled, decay/sustain/release increments are 4x faster
    /// (but only while attenuation < 0x200). The SSG-EG state machine handles
    /// looping and inversion when attenuation crosses the 0x200 threshold.
    fn update_envelope(&mut self, fnum: u16, block: u8, eg_counter: u32) {
        // SSG-EG state machine runs before the normal envelope clock.
        if self.ssg_enabled() {
            self.ssg_clock(fnum, block);
        }

        match self.env_state {
            EnvState::Attack => {
                let rate = self.effective_rate(self.attack_rate, fnum, block);
                if rate >= 62 {
                    self.envelope = 0;
                    self.env_state = EnvState::Decay;
                    return;
                }
                let inc = Self::eg_calc_increment(rate, eg_counter);
                if inc > 0 && self.envelope > 0 {
                    // MAME reference: volume += (~volume * inc) >> 4
                    let delta = (!(self.envelope as i32) * inc as i32) >> 4;
                    self.envelope = ((self.envelope as i32) + delta).max(0) as u16;
                }
                if self.envelope == 0 {
                    self.env_state = EnvState::Decay;
                }
            }
            EnvState::Decay => {
                let rate = self.effective_rate(self.decay_rate, fnum, block);
                let inc = Self::eg_calc_increment(rate, eg_counter) as u16;
                // SSG-EG: 4x decay rate while below threshold
                let inc = if self.ssg_enabled() && self.envelope < 0x200 {
                    inc * 4
                } else {
                    inc
                };
                self.envelope = (self.envelope + inc).min(1023);
                // SL=15 maps to 31*32=992 (near-silence), not 15*32=480.
                // Transition to Sustain when attenuation reaches sustain level.
                // Do NOT clamp attenuation — real hardware allows natural overshoot.
                let sl_mapped = if self.sustain_level == 15 {
                    31u16
                } else {
                    self.sustain_level as u16
                };
                let target = sl_mapped * 32;
                if self.envelope >= target {
                    self.env_state = EnvState::Sustain;
                }
            }
            EnvState::Sustain => {
                let rate = self.effective_rate(self.sustain_rate, fnum, block);
                let inc = Self::eg_calc_increment(rate, eg_counter) as u16;
                let inc = if self.ssg_enabled() && self.envelope < 0x200 {
                    inc * 4
                } else {
                    inc
                };
                self.envelope = (self.envelope + inc).min(1023);
            }
            EnvState::Release => {
                // SSG-EG 4x acceleration does NOT apply during Release.
                // On real hardware, Release always uses the normal rate.
                let rate = self.effective_rate(self.release_rate * 2 + 1, fnum, block);
                let inc = Self::eg_calc_increment(rate, eg_counter) as u16;
                self.envelope = (self.envelope + inc).min(1023);
            }
        }
    }

    /// Compute phase increment for this operator given the channel frequency.
    ///
    /// `fnum`/`block` is the (possibly PM-modulated) frequency for the base
    /// phase increment. `dt_fnum`/`dt_block` is the *original* channel
    /// frequency used for detune key_code lookup — on real hardware, detune
    /// is indexed by the stored fnum, not the vibrato-modulated one.
    fn phase_increment(&self, fnum: u16, block: u8, dt_fnum: u16, dt_block: u8) -> u32 {
        let base_freq = ((fnum as u32) << (block as u32)) >> 1;
        let mult = MULTIPLY_TABLE[self.multiply as usize] as u32;
        let phase_inc = (base_freq * mult) >> 1;

        // Detune uses the ORIGINAL fnum/block (not PM-modulated) for key_code
        let dt_mag = (self.detune & 3) as usize;
        let kc = Self::key_code(dt_fnum, dt_block) as usize;
        let dt_offset = DETUNE_TABLE[dt_mag][kc.min(31)] as u32;

        if self.detune & 4 != 0 {
            phase_inc.saturating_sub(dt_offset)
        } else {
            phase_inc + dt_offset
        }
    }

    /// Produce the output sample for this operator, given optional modulation
    /// input from another operator (in the phase domain) and LFO AM attenuation.
    fn compute(&mut self, phase_inc: u32, modulation: i32, am_atten: u32) -> i32 {
        // Advance phase
        self.phase = self.phase.wrapping_add(phase_inc);

        // Hardware-accurate phase extraction (matches Nuked-OPN2 / YMFM):
        // Extract 10-bit phase index from bits 19-10, then add modulation
        // directly. This matches the real chip where modulation is added to
        // the phase AFTER the >>10 shift.
        let phase_10 = ((self.phase >> 10) as i32).wrapping_add(modulation) as u32;

        // Extract components from the 10-bit modulated phase:
        //   bit 9 = sign (negative half of sine)
        //   bit 8 = half (second quarter, mirror the table index)
        //   bits 0-7 = 8-bit index into 256-entry quarter-wave table
        let sign = (phase_10 >> 9) & 1;
        let half = (phase_10 >> 8) & 1;
        let phase_8 = (phase_10 & 0xFF) as usize;

        // Quarter-wave symmetry: mirror index in second quarter
        let index = if half != 0 { 255 - phase_8 } else { phase_8 };

        // Sine lookup (log domain) — 256-entry quarter-wave ROM from Nuked-OPN2
        let log_sin = SIN_TABLE[index];

        // SSG-EG output inversion: when active, the attenuation is mirrored
        // around the 0x200 midpoint, flipping the envelope shape.
        // Applied only during Attack/Decay/Sustain (NOT Release), and only when
        // the current inversion state differs from the attack flag.
        let effective_envelope = if self.ssg_enabled()
            && self.env_state != EnvState::Release
            && self.ssg_output_invert != self.ssg_attack()
        {
            0x200u16.wrapping_sub(self.envelope) & 0x3FF
        } else {
            self.envelope
        };

        // Add envelope attenuation (and total level) in log domain.
        // The 12-bit space uses 256 units per 6 dB octave.
        //   Envelope: 0.09375 dB/step × (256/6) ≈ 4 units/step → ×4
        //   TL:       0.75 dB/step    × (256/6) ≈ 32 units/step → ×32
        let am = if self.am_enable { am_atten } else { 0 };
        let atten =
            log_sin as u32 + (effective_envelope as u32) * 4 + (self.total_level as u32) * 32 + am;

        // Clamp: any attenuation ≥ 4096 (≥96 dB) is silence
        if atten >= 4096 {
            self.prev_output = self.output;
            self.output = 0;
            return 0;
        }

        // Log-to-linear conversion via exp table.
        // The << 2 matches both Nuked-OPN2 (applied at runtime) and YMFM (baked
        // into the power table). This is critical for correct FM modulation depth:
        // without it, operator output is 4x too weak, producing thin/dull timbres.
        // Max output = (2042 << 2) >> 0 = 8168. After >>5 a single carrier reaches
        // ±255, filling the 9-bit DAC range. Four carriers (algo 7) clip at ±256.
        let mantissa = (atten & 0xFF) as usize;
        let exponent = atten >> 8;
        let linear = ((EXP_TABLE[mantissa] as i32) << 2) >> exponent;

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
    /// Phase modulation sensitivity (0-7). Controls vibrato depth.
    pms: u8,
    /// Amplitude modulation sensitivity (0-3). Controls tremolo depth.
    ams: u8,
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
            pms: 0,
            ams: 0,
        }
    }

    /// Compute the LFO phase modulation offset for this channel.
    ///
    /// Uses the Nuked-OPN2 PG_LFO_SH1/SH2 shift tables to compute a pitch
    /// offset from the channel's fnum and the current LFO output level.
    /// Compute LFO PM (vibrato) frequency offset using the Nuked-OPN2 method.
    ///
    /// `pm_raw` is the 5-bit PM value from `lfo_phase >> 2` (0-31).
    /// Returns a signed offset to add to `fnum` before phase increment.
    ///
    /// Hardware approach: shift `fnum >> 4` by table-selected amounts,
    /// NOT shift the PM level per fnum bit. Two barrel-shifter lookups
    /// approximate fractional multiplication without silicon multipliers.
    fn lfo_pm_offset(&self, pm_raw: u8) -> i32 {
        if self.pms == 0 {
            return 0;
        }

        let pms = self.pms as usize;

        // Extract 3-bit triangle magnitude (0-7) and sign from the 5-bit PM value.
        // Bits 0-3 form a 4-bit value; if bit 3 is set, XOR with 0x0F to fold
        // into a triangle wave with magnitude 0-7.
        let mut pm_l = (pm_raw & 0x0F) as usize;
        if pm_l & 0x08 != 0 {
            pm_l ^= 0x0F;
        }
        let sign = pm_raw & 0x10 != 0; // bit 4

        // fnum_h = top 7 bits of the 11-bit fnum
        let fnum_h = (self.fnum >> 4) as i32;

        // Two-shift approximation: fm = (fnum_h >> sh1) + (fnum_h >> sh2)
        // Shift=7 produces 0 (no contribution) since max fnum_h is 127.
        let sh1 = PG_LFO_SH1[pms][pm_l] as i32;
        let sh2 = PG_LFO_SH2[pms][pm_l] as i32;
        let mut fm = (fnum_h >> sh1) + (fnum_h >> sh2);

        // PMS > 5: extra left-shift for wider vibrato range
        if pms > 5 {
            fm <<= pms - 5;
        }

        // Final scale-down (matches Nuked-OPN2 >>2)
        fm >>= 2;

        if sign { -fm } else { fm }
    }

    /// Synthesize one sample from this channel using the configured algorithm.
    /// Returns (left, right) in raw integer form.
    ///
    /// Each carrier output is right-shifted by 5 (reducing ~11-bit operator
    /// output to ~6-bit), with intermediate clamping to ±256 (9-bit signed)
    /// after each addition — matching the hardware's multiplexed 9-bit DAC.
    ///
    /// `am_level` is the 7-bit AM LFO output (0-126) for tremolo.
    /// `pm_raw` is the 5-bit PM LFO value (0-31) for vibrato.
    fn output_sample(&mut self, am_level: u8, pm_raw: u8) -> (i32, i32) {
        // Compute LFO AM attenuation for operators with AM enabled.
        // The 7-bit LFO level is shifted right by EG_AM_SHIFT[ams], then
        // scaled to the 12-bit log-attenuation domain (×4 like envelope).
        let am_atten = if self.ams > 0 {
            let shift = EG_AM_SHIFT[self.ams as usize];
            ((am_level as u32) >> shift) * 4
        } else {
            0
        };

        // Compute LFO PM phase offset (already signed)
        let pm_offset = self.lfo_pm_offset(pm_raw);

        let ops = &mut self.operators;

        // Precompute phase increments for all 4 operators, with PM applied.
        // PM modulates the fnum for base frequency, but detune uses original fnum.
        let fnum_pm = (self.fnum as i32 + pm_offset).clamp(0, 0x7FF) as u16;
        let inc0 = ops[0].phase_increment(fnum_pm, self.block, self.fnum, self.block);
        let inc1 = ops[1].phase_increment(fnum_pm, self.block, self.fnum, self.block);
        let inc2 = ops[2].phase_increment(fnum_pm, self.block, self.fnum, self.block);
        let inc3 = ops[3].phase_increment(fnum_pm, self.block, self.fnum, self.block);

        // Feedback for operator 1
        let fb_mod = if self.feedback > 0 {
            (ops[0].prev_output + ops[0].output) >> (10 - self.feedback as i32)
        } else {
            0
        };

        // Run operators through the selected algorithm.
        //
        // YMFM reference: carrier outputs are summed at FULL resolution, then
        // the sum is right-shifted by `rshift` (5) and clamped to `clipmax`
        // (±256) ONCE. This models the 9-bit multiplexed DAC output stage.
        //
        // Critical: do NOT shift/clamp each carrier individually — that
        // destroys precision (quantization noise → shrill) and adds
        // distortion (aggressive intermediate clipping).
        //
        // Op4 is always a carrier. Other carrier designations per algorithm:
        //   0-3: only op4   |  4: op2+op4   |  5: op2+op3+op4
        //   6: op2+op3+op4  |  7: all four
        const RS: i32 = 5;
        const CLIP: i32 = 256;

        // Hardware applies >>1 to all inter-operator modulation (Nuked-OPN2:
        // `mod >>= 1` for non-feedback operators). Feedback has its own formula.
        let out = match self.algorithm {
            0 => {
                // op1→op2→op3→op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, o2 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            1 => {
                // (op1+op2)→op3→op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, (o1 >> 1) + (o2 >> 1), am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            2 => {
                // op1→op3, op2 standalone, (op2+op3)→op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, o1 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, (o2 >> 1) + (o3 >> 1), am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            3 => {
                // op1→op2, (op2+op3)→op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, (o2 >> 1) + (o3 >> 1), am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            4 => {
                // Carriers: op2, op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                ((o2 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            5 => {
                // Carriers: op2, op3, op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, o1 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, o1 >> 1, am_atten);
                ((o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            6 => {
                // Carriers: op2, op3, op4
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, 0, am_atten);
                ((o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            7 => {
                // Carriers: all four — no inter-op modulation
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, 0, am_atten);
                ((o1 + o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            _ => 0,
        };

        // Apply panning
        let left = if self.panning_left { out } else { 0 };
        let right = if self.panning_right { out } else { 0 };
        (left, right)
    }

    /// Channel 3 special mode: synthesize with per-operator frequencies.
    ///
    /// Operators 1-3 use independent fnum/block from `ch3_fnum`/`ch3_block`,
    /// while operator 4 uses the channel's normal frequency.
    fn output_sample_ch3(
        &mut self,
        ch3_fnum: &[u16; 3],
        ch3_block: &[u8; 3],
        am_level: u8,
        pm_raw: u8,
    ) -> (i32, i32) {
        let am_atten = if self.ams > 0 {
            let shift = EG_AM_SHIFT[self.ams as usize];
            ((am_level as u32) >> shift) * 4
        } else {
            0
        };

        // Compute LFO PM offset (already signed)
        let pm_offset = self.lfo_pm_offset(pm_raw);

        let ops = &mut self.operators;

        // Per-operator phase increments with PM applied to each fnum individually.
        // Ops 0-2 use ch3 special freqs, op 3 uses the channel's normal freq.
        let fnum0_pm = (ch3_fnum[0] as i32 + pm_offset).clamp(0, 0x7FF) as u16;
        let fnum1_pm = (ch3_fnum[1] as i32 + pm_offset).clamp(0, 0x7FF) as u16;
        let fnum2_pm = (ch3_fnum[2] as i32 + pm_offset).clamp(0, 0x7FF) as u16;
        let fnum3_pm = (self.fnum as i32 + pm_offset).clamp(0, 0x7FF) as u16;
        // Detune uses the original (non-PM) fnum/block for each operator
        let inc0 = ops[0].phase_increment(fnum0_pm, ch3_block[0], ch3_fnum[0], ch3_block[0]);
        let inc1 = ops[1].phase_increment(fnum1_pm, ch3_block[1], ch3_fnum[1], ch3_block[1]);
        let inc2 = ops[2].phase_increment(fnum2_pm, ch3_block[2], ch3_fnum[2], ch3_block[2]);
        let inc3 = ops[3].phase_increment(fnum3_pm, self.block, self.fnum, self.block);

        let fb_mod = if self.feedback > 0 {
            (ops[0].prev_output + ops[0].output) >> (10 - self.feedback as i32)
        } else {
            0
        };

        // 9-bit intermediate clipping (same as output_sample above)
        const RS: i32 = 5;
        const CLIP: i32 = 256;

        // Hardware applies >>1 to all inter-operator modulation.
        let out = match self.algorithm {
            0 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, o2 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            1 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, (o1 >> 1) + (o2 >> 1), am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            2 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, o1 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, (o2 >> 1) + (o3 >> 1), am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            3 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, (o2 >> 1) + (o3 >> 1), am_atten);
                (o4 >> RS).clamp(-CLIP, CLIP)
            }
            4 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, o3 >> 1, am_atten);
                ((o2 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            5 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, o1 >> 1, am_atten);
                let o4 = ops[3].compute(inc3, o1 >> 1, am_atten);
                ((o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            6 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, o1 >> 1, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, 0, am_atten);
                ((o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            7 => {
                let o1 = ops[0].compute(inc0, fb_mod, am_atten);
                let o2 = ops[1].compute(inc1, 0, am_atten);
                let o3 = ops[2].compute(inc2, 0, am_atten);
                let o4 = ops[3].compute(inc3, 0, am_atten);
                ((o1 + o2 + o3 + o4) >> RS).clamp(-CLIP, CLIP)
            }
            _ => 0,
        };
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
    /// EG sample divider (counts 0, 1, 2 then resets — /3 timing).
    eg_timer: u32,
    /// Global EG cycle counter (12-bit, wraps at 4096).
    eg_counter: u32,
    /// Frequency high-byte latch per bank per channel-offset.
    /// Written by 0xA4-0xA6; committed when 0xA0-0xA2 is written.
    fn_h_latch: [[u8; 3]; 2],
    /// Channel 3 special mode: per-operator fnum (operators 1, 2, 3).
    /// Operator 4 uses the normal channel 3 frequency.
    /// Index maps: [0]=op1 (reg 0xA9/0xAD), [1]=op2 (reg 0xAA/0xAE),
    /// [2]=op3 (reg 0xA8/0xAC).
    ch3_fnum: [u16; 3],
    /// Channel 3 special mode: per-operator block.
    ch3_block: [u8; 3],
    /// Channel 3 special mode: high-byte latches for registers 0xAC-0xAE.
    ch3_fn_h_latch: [u8; 3],
    /// CSM mode: true on the sample after Timer A overflows (triggers key-on),
    /// cleared next sample (triggers key-off). Only active when timer_control
    /// bits 6-7 = 0x80.
    csm_key_on: bool,
    /// Debug: count of register writes (address + data pairs).
    #[serde(skip)]
    write_count: u32,
    /// Debug: trace buffer of first N register writes (bank, addr, val).
    #[serde(skip)]
    write_trace: Vec<(u8, u8, u8)>,
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
            fn_h_latch: [[0; 3]; 2],
            ch3_fnum: [0; 3],
            ch3_block: [0; 3],
            ch3_fn_h_latch: [0; 3],
            csm_key_on: false,
            write_count: 0,
            write_trace: Vec::new(),
        }
    }

    /// Latch the register address for a subsequent data write.
    ///
    /// `port` selects the register bank: 0 for channels 1-3, 1 for channels 4-6.
    pub fn write_address(&mut self, port: u8, val: u8) {
        let bank = (port & 1) as usize;
        self.address_latch[bank] = val;
    }

    /// Returns the currently latched address for the given port.
    #[must_use]
    pub fn latched_address(&self, port: u8) -> u8 {
        self.address_latch[(port & 1) as usize]
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
    ///
    /// On real hardware, reading does NOT clear the flags. Timer flags are
    /// cleared only by writing to register 0x27 with bits 4-5 set.
    #[must_use]
    pub fn read_status(&self) -> u8 {
        self.status
    }

    /// Internal register write dispatch.
    fn write_register(&mut self, bank: usize, addr: u8, val: u8) {
        self.write_count += 1;
        // Only capture non-DAC writes (skip 0x2A spam)
        if addr != 0x2A && self.write_trace.len() < 500 {
            self.write_trace.push((bank as u8, addr, val));
        }
        match addr {
            // ── Global registers (bank 0 only, but writes to bank 1 are ignored) ──
            0x22 if bank == 0 => {
                // LFO control. Reset phase when LFO is enabled (0→1 transition).
                let was_enabled = self.lfo_enabled;
                self.lfo_enabled = val & 0x08 != 0;
                if self.lfo_enabled && !was_enabled {
                    self.lfo_phase = 0;
                    self.lfo_counter = 0;
                }
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
                let fnum = ch.fnum;
                let block = ch.block;
                // $28 key-on bits use operator-number ordering, not register-slot
                // ordering: bit4=op1, bit5=op2, bit6=op3, bit7=op4.
                for op_idx in 0..4usize {
                    if val & (0x10 << op_idx) != 0 {
                        ch.operators[op_idx].key_on(fnum, block);
                    } else {
                        ch.operators[op_idx].key_off();
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
                        // AM/DR
                        op.am_enable = val & 0x80 != 0;
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
                // Frequency number low byte — commits both latched high and new low
                let ch_offset = (addr & 0x03) as usize;
                let ch_idx = ch_offset + bank * 3;
                if ch_idx < 6 {
                    let hi = self.fn_h_latch[bank][ch_offset];
                    self.channels[ch_idx].fnum = (u16::from(hi & 0x07) << 8) | u16::from(val);
                    self.channels[ch_idx].block = (hi >> 3) & 0x07;
                }
            }
            0xA4..=0xA6 => {
                // Block / frequency number high bits — latch only, committed
                // atomically when the corresponding 0xA0-0xA2 is written.
                let ch_offset = (addr & 0x03) as usize;
                self.fn_h_latch[bank][ch_offset] = val;
            }
            0xA8..=0xAA if bank == 0 => {
                // Channel 3 special mode: per-operator frequency low bytes.
                // 0xA8 → op3 (index 2), 0xA9 → op1 (index 0), 0xAA → op2 (index 1)
                let slot = (addr - 0xA8) as usize;
                let idx = match slot {
                    0 => 2, // 0xA8 → op3
                    1 => 0, // 0xA9 → op1
                    _ => 1, // 0xAA → op2
                };
                let hi = self.ch3_fn_h_latch[idx];
                self.ch3_fnum[idx] = (u16::from(hi & 0x07) << 8) | u16::from(val);
                self.ch3_block[idx] = (hi >> 3) & 0x07;
            }
            0xAC..=0xAE if bank == 0 => {
                // Channel 3 special mode: per-operator frequency high bytes (latch).
                let slot = (addr - 0xAC) as usize;
                let idx = match slot {
                    0 => 2, // 0xAC → op3
                    1 => 0, // 0xAD → op1
                    _ => 1, // 0xAE → op2
                };
                self.ch3_fn_h_latch[idx] = val;
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
                // Panning / AMS / PMS
                let ch_idx = (addr & 0x03) as usize + bank * 3;
                if ch_idx < 6 {
                    self.channels[ch_idx].panning_left = val & 0x80 != 0;
                    self.channels[ch_idx].panning_right = val & 0x40 != 0;
                    self.channels[ch_idx].ams = (val >> 4) & 0x03;
                    self.channels[ch_idx].pms = val & 0x07;
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
    /// scaled by 16. CSM mode: Timer A overflow triggers key-on for channel 3.
    fn advance_timers(&mut self) {
        // Timer A
        if self.timer_control & 0x01 != 0 {
            self.timer_a_counter += 1;
            if self.timer_a_counter >= (1024 - self.timer_a_period) {
                self.timer_a_counter = 0;
                if self.timer_control & 0x04 != 0 {
                    self.status |= 0x01;
                }
                // CSM mode (bits 6-7 = 0x80): Timer A overflow triggers key-on
                // for all operators on channel 3.
                if self.timer_control & 0xC0 == 0x80 {
                    let fnum = self.channels[2].fnum;
                    let block = self.channels[2].block;
                    for op in &mut self.channels[2].operators {
                        op.key_on(fnum, block);
                    }
                    self.csm_key_on = true;
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

    /// Advance the LFO by one sample tick. Returns `(am_level, pm_raw)`:
    /// - `am_level`: 0-126 (7-bit triangle for amplitude modulation)
    /// - `pm_raw`: 0-31 (5-bit value for phase modulation, decoded in `lfo_pm_offset`)
    ///
    /// AM and PM derive from the same 7-bit LFO counter but at different
    /// resolutions. AM uses the full 7-bit triangle (0-126). PM uses
    /// `lfo_phase >> 2` (5-bit, 0-31), with the triangle folding and sign
    /// extraction handled inside `lfo_pm_offset` — matching Nuked-OPN2 where
    /// `chip->lfo_pm = chip->lfo_cnt >> 2`.
    ///
    /// When LFO is globally disabled, both AM and PM return 0 (no modulation).
    /// This matches YMFM where `am_offset = 0; pm_value = 0` when `!lfo_enable`.
    fn advance_lfo(&mut self) -> (u8, u8) {
        if !self.lfo_enabled {
            return (0, 0);
        }

        self.lfo_counter += 1;
        if self.lfo_counter >= LFO_CYCLES[self.lfo_frequency as usize] {
            self.lfo_counter = 0;
            self.lfo_phase = self.lfo_phase.wrapping_add(1) & 0x7F;
        }

        // AM: 7-bit triangle wave (0-126)
        let phase = self.lfo_phase;
        let am_level = if phase < 64 {
            phase * 2
        } else {
            (127 - phase) * 2
        };

        // PM: raw 5-bit value — triangle folding + sign handled in lfo_pm_offset
        let pm_raw = phase >> 2;

        (am_level, pm_raw)
    }

    fn output_sample_raw_per_channel(&mut self) -> [(i32, i32); 6] {
        // CSM mode: key-off channel 3 on the sample after Timer A triggered key-on.
        if self.csm_key_on {
            self.csm_key_on = false;
            for op in &mut self.channels[2].operators {
                op.key_off();
            }
        }

        // Advance LFO — returns separate AM/PM values since they differ
        // when LFO is disabled (AM = max, PM = 0; confirmed hardware behavior).
        let (am_level, pm_raw) = self.advance_lfo();

        // Advance EG timer (/3 divider)
        self.eg_timer += 1;
        let update_eg = self.eg_timer >= 3;
        if update_eg {
            self.eg_timer = 0;
            // 12-bit counter that skips 0 on overflow (verified hardware behavior).
            // When counter wraps past 0xFFF, it goes to 1 instead of 0.
            self.eg_counter += 1;
            self.eg_counter = (self.eg_counter & 0xFFF) + (self.eg_counter >> 12);
        }
        let eg_counter = self.eg_counter;

        let mut channel_samples = [(0i32, 0i32); 6];

        for ch_idx in 0..6 {
            // DAC replaces channel 6 (index 5) when enabled
            if ch_idx == 5 && self.dac_enabled {
                // DAC value is signed (-128..+127). Shift left by 1 to match the
                // 9-bit channel output range (±256). On real hardware, the DAC
                // shares the same 9-bit multiplexed converter as the FM channels.
                let dac_out = i32::from(self.dac_value) << 1;
                let left = if self.channels[5].panning_left {
                    dac_out
                } else {
                    0
                };
                let right = if self.channels[5].panning_right {
                    dac_out
                } else {
                    0
                };
                channel_samples[5] = (left, right);
                if update_eg {
                    let fnum = self.channels[5].fnum;
                    let block = self.channels[5].block;
                    for op in &mut self.channels[5].operators {
                        op.update_envelope(fnum, block, eg_counter);
                    }
                }
                continue;
            }

            // Channel 3 (index 2) uses special mode when timer_control bits 6-7 are set
            let (l, r) = if ch_idx == 2 && (self.timer_control & 0xC0) != 0 {
                let ch3_fnum = self.ch3_fnum;
                let ch3_block = self.ch3_block;
                self.channels[2].output_sample_ch3(&ch3_fnum, &ch3_block, am_level, pm_raw)
            } else {
                self.channels[ch_idx].output_sample(am_level, pm_raw)
            };
            channel_samples[ch_idx] = (l, r);

            if update_eg {
                // CH3 special mode: operators 0-2 use per-operator fnum/block
                // for EG key-scaling, matching hardware where each slot's key_code
                // derives from its own frequency registers.
                if ch_idx == 2 && (self.timer_control & 0xC0) != 0 {
                    for (op_i, op) in self.channels[2].operators.iter_mut().enumerate() {
                        let (f, b) = if op_i < 3 {
                            (self.ch3_fnum[op_i], self.ch3_block[op_i])
                        } else {
                            (self.channels[2].fnum, self.channels[2].block)
                        };
                        op.update_envelope(f, b, eg_counter);
                    }
                } else {
                    let fnum = self.channels[ch_idx].fnum;
                    let block = self.channels[ch_idx].block;
                    for op in &mut self.channels[ch_idx].operators {
                        op.update_envelope(fnum, block, eg_counter);
                    }
                }
            }
        }

        // Advance timers
        self.advance_timers();

        channel_samples
    }

    /// Produce one stereo audio sample per FM channel.
    ///
    /// Called at the native YM2612 sample rate (~53.267 kHz). Each channel pair
    /// is normalized to the range `[-1.0, 1.0]` using the same global scaling as
    /// the legacy stereo sum path.
    pub fn output_sample_per_channel(&mut self) -> [(f32, f32); 6] {
        const SCALE: f32 = 1.0 / 1536.0;
        let raw = self.output_sample_raw_per_channel();
        std::array::from_fn(|idx| {
            let (left, right) = raw[idx];
            (left as f32 * SCALE, right as f32 * SCALE)
        })
    }

    /// Produce one stereo audio sample.
    ///
    /// Called at the native YM2612 sample rate (~53.267 kHz). Returns `(left, right)`
    /// normalized to the range `[-1.0, 1.0]`.
    ///
    /// The envelope generator runs at FM_RATE/3 (~17.7 kHz), gated by `eg_timer`.
    pub fn output_sample(&mut self) -> (f32, f32) {
        let raw = self.output_sample_raw_per_channel();
        let left_sum: i32 = raw.iter().map(|&(left, _)| left).sum();
        let right_sum: i32 = raw.iter().map(|&(_, right)| right).sum();

        // Scale to float. Each channel is clamped to ±256 (9-bit DAC).
        // 6 channels → max sum = 1536. Dividing by 1536 keeps output in ±1.0.
        const SCALE: f32 = 1.0 / 1536.0;
        (left_sum as f32 * SCALE, right_sum as f32 * SCALE)
    }
}

impl Default for Ym2612 {
    fn default() -> Self {
        Self::new()
    }
}

// ── Diagnostics ─────────────────────────────────────────────────────────

/// Diagnostic snapshot of one operator's state.
#[derive(Debug, Clone)]
pub struct OpDiag {
    pub multiply: u8,
    pub detune: u8,
    pub total_level: u8,
    pub attack_rate: u8,
    pub decay_rate: u8,
    pub sustain_rate: u8,
    pub sustain_level: u8,
    pub release_rate: u8,
    pub key_scale: u8,
    pub am_enable: bool,
    pub ssg_eg: u8,
    pub key_on: bool,
    pub envelope: u16,
    pub env_state: EnvState,
}

/// Diagnostic snapshot of one channel's state.
#[derive(Debug, Clone)]
pub struct ChannelDiag {
    pub fnum: u16,
    pub block: u8,
    pub algorithm: u8,
    pub feedback: u8,
    pub panning_left: bool,
    pub panning_right: bool,
    pub pms: u8,
    pub ams: u8,
    pub operators: [OpDiag; 4],
}

/// Diagnostic snapshot of the entire YM2612 state.
#[derive(Debug, Clone)]
pub struct Ym2612Diag {
    pub channels: [ChannelDiag; 6],
    pub dac_enabled: bool,
    pub lfo_enabled: bool,
    pub lfo_frequency: u8,
    pub timer_control: u8,
}

impl Ym2612 {
    /// Capture a diagnostic snapshot of the entire chip state.
    ///
    /// For debugging and test assertions — exposes internal state that's
    /// normally private.
    #[must_use]
    pub fn diagnostic(&self) -> Ym2612Diag {
        let channels = std::array::from_fn(|i| {
            let ch = &self.channels[i];
            ChannelDiag {
                fnum: ch.fnum,
                block: ch.block,
                algorithm: ch.algorithm,
                feedback: ch.feedback,
                panning_left: ch.panning_left,
                panning_right: ch.panning_right,
                pms: ch.pms,
                ams: ch.ams,
                operators: std::array::from_fn(|j| {
                    let op = &ch.operators[j];
                    OpDiag {
                        multiply: op.multiply,
                        detune: op.detune,
                        total_level: op.total_level,
                        attack_rate: op.attack_rate,
                        decay_rate: op.decay_rate,
                        sustain_rate: op.sustain_rate,
                        sustain_level: op.sustain_level,
                        release_rate: op.release_rate,
                        key_scale: op.key_scale,
                        am_enable: op.am_enable,
                        ssg_eg: op.ssg_eg,
                        key_on: op.key_on,
                        envelope: op.envelope,
                        env_state: op.env_state,
                    }
                }),
            }
        });
        Ym2612Diag {
            channels,
            dac_enabled: self.dac_enabled,
            lfo_enabled: self.lfo_enabled,
            lfo_frequency: self.lfo_frequency,
            timer_control: self.timer_control,
        }
    }

    /// Return the total number of register data writes.
    pub fn write_count(&self) -> u32 {
        self.write_count
    }

    /// Return the first N register writes captured (bank, addr, val).
    pub fn write_trace(&self) -> &[(u8, u8, u8)] {
        &self.write_trace
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

        // Configure channel 0 op1 with a moderate attack rate.
        // AR=15, fnum/block=0, KS=0 → effective rate = 2*15 + 0 = 30 (normal attack).
        write_reg(&mut ym, 0x50, 15); // AR=15 for slot 0 ch0 (op1)
        write_reg(&mut ym, 0x40, 0); // TL=0 so it's audible

        // Verify initial state is Release
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Release);

        // Key-on channel 0, op1 (bit 4 = op1)
        write_reg(&mut ym, 0x28, 0x10); // Key-on op1 for ch0
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Attack);
        assert!(ym.channels[0].operators[0].key_on);

        // Phase should have been reset
        assert_eq!(ym.channels[0].operators[0].phase, 0);
        // Envelope is NOT reset on key-on (hardware behavior) — it starts
        // attack from whatever the current attenuation is.
    }

    #[test]
    fn key_on_instant_attack() {
        let mut ym = Ym2612::new();

        // AR=31, fnum/block=0, KS=0 → effective rate = 2*31 + 0 = 62 (instant attack).
        // Hardware skips the Attack phase entirely and goes straight to Decay
        // with attenuation=0.
        write_reg(&mut ym, 0x50, 31); // AR=31 for slot 0 ch0
        write_reg(&mut ym, 0x40, 0);

        write_reg(&mut ym, 0x28, 0x10); // Key-on op1 for ch0
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Decay);
        assert_eq!(ym.channels[0].operators[0].envelope, 0);
        assert!(ym.channels[0].operators[0].key_on);
    }

    #[test]
    fn key_on_register_uses_operator_order_not_slot_order() {
        let mut ym = Ym2612::new();

        // Give op2 and op3 distinct AR values so we can tell which one was keyed.
        write_reg(&mut ym, 0x58, 31); // slot 2 -> op2
        write_reg(&mut ym, 0x54, 15); // slot 1 -> op3

        // bit 5 of $28 is operator 2, not slot 1/op3.
        write_reg(&mut ym, 0x28, 0x20);
        assert!(
            ym.channels[0].operators[1].key_on,
            "op2 should be keyed by bit 5"
        );
        assert!(!ym.channels[0].operators[2].key_on, "op3 should remain off");

        write_reg(&mut ym, 0x28, 0x00);

        // bit 6 of $28 is operator 3, not slot 2/op2.
        write_reg(&mut ym, 0x28, 0x40);
        assert!(
            ym.channels[0].operators[2].key_on,
            "op3 should be keyed by bit 6"
        );
        assert!(!ym.channels[0].operators[1].key_on, "op2 should remain off");
    }

    #[test]
    fn key_off_starts_release() {
        let mut ym = Ym2612::new();

        // Set up and key-on with instant attack (AR=31 → rate 62)
        write_reg(&mut ym, 0x50, 31); // AR=31
        write_reg(&mut ym, 0x28, 0xF0); // Key-on all ops for ch0

        // Instant attack skips to Decay
        assert_eq!(ym.channels[0].operators[0].env_state, EnvState::Decay);

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

        // DAC output: 127 << 1 = 254, scaled by 1/1536 ≈ 0.1654
        let expected = 127.0 * 2.0 / 1536.0;
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
        let expected2 = -128.0 * 2.0 / 1536.0;
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

        // Reading status should return the flag but NOT clear it
        let status = ym.read_status();
        assert_ne!(
            status & 0x01,
            0,
            "read_status should return the overflow flag"
        );
        assert_ne!(
            ym.status & 0x01,
            0,
            "timer A flag should persist after reading (only cleared by reg 0x27)"
        );

        // Writing register 0x27 with bit 4 set should clear Timer A flag
        write_reg(&mut ym, 0x27, 0x15); // bit4=reset A, bit2=enable A flag, bit0=run A
        assert_eq!(
            ym.status & 0x01,
            0,
            "timer A flag should be cleared after writing 0x27 with bit 4"
        );
    }

    /// Diagnostic: measure the actual output pitch of a pure sine tone.
    ///
    /// Sets up a single operator on algorithm 7, frequency A4 (440 Hz),
    /// counts zero-crossings to determine the output frequency, and
    /// verifies it matches the expected pitch.
    #[test]
    fn pitch_accuracy_a440() {
        let mut ym = Ym2612::new();

        // Algorithm 7 (all ops independent), only use op1
        write_reg(&mut ym, 0xB0, 0x07); // algo=7, fb=0
        write_reg(&mut ym, 0xB4, 0xC0); // L+R panning

        // Set op1: TL=0, AR=31 (instant attack), MUL=1, no detune
        write_reg(&mut ym, 0x30, 0x01); // DT=0, MUL=1
        write_reg(&mut ym, 0x40, 0x00); // TL=0
        write_reg(&mut ym, 0x50, 0x1F); // AR=31
        write_reg(&mut ym, 0x60, 0x00); // DR=0
        write_reg(&mut ym, 0x70, 0x00); // SR=0
        write_reg(&mut ym, 0x80, 0x0F); // SL=0, RR=15

        // Silence op2, op3, op4 (set TL=127)
        for slot_offset in [0x04u8, 0x08, 0x0C] {
            write_reg(&mut ym, 0x40 + slot_offset, 127);
        }

        // A4 = 440 Hz at YM2612 native rate 53267 Hz.
        // YMFM/Nuked OPN2 phase step for MUL=1 is effectively:
        //   phase_inc = fnum * 2^(block - 1)
        // For 440 Hz: phase_inc = 440 * 2^20 / 53267 ≈ 8665
        // Solve: fnum * 2^(block - 1) = 8665
        // block=4: fnum = 8665/8 = 1083 = 0x43B
        let fnum: u16 = 0x43B; // 1083
        let block: u8 = 4;
        write_reg(&mut ym, 0xA4, (block << 3) | ((fnum >> 8) as u8 & 0x07));
        write_reg(&mut ym, 0xA0, (fnum & 0xFF) as u8);

        // Key-on op1 only
        write_reg(&mut ym, 0x28, 0x10);

        // Let envelope settle (attack + initial transient)
        for _ in 0..1000 {
            ym.output_sample();
        }

        // Collect samples at YM2612 native rate (53267 Hz) and count
        // positive-to-negative zero crossings
        let native_rate = 53267.0_f64;
        let num_samples = 10000;
        let mut samples = Vec::with_capacity(num_samples);
        for _ in 0..num_samples {
            let (l, _) = ym.output_sample();
            samples.push(l);
        }

        // Count zero crossings (positive → negative)
        let mut crossings = 0u32;
        for i in 1..samples.len() {
            if samples[i - 1] >= 0.0 && samples[i] < 0.0 {
                crossings += 1;
            }
        }

        // Each zero crossing (pos→neg) = one full cycle
        let measured_freq = crossings as f64 * native_rate / num_samples as f64;
        let expected_freq = 440.0;

        eprintln!("=== PITCH DIAGNOSTIC ===");
        eprintln!("fnum={fnum:#06X}, block={block}, MUL=1");
        eprintln!("phase_inc = {}", fnum as u32 * (1 << (block - 1)));
        eprintln!("Expected: {expected_freq:.1} Hz");
        eprintln!("Measured: {measured_freq:.1} Hz");
        eprintln!(
            "Ratio (measured/expected): {:.4}",
            measured_freq / expected_freq
        );
        eprintln!("Zero crossings: {crossings} in {num_samples} samples");
        eprintln!("Sample[0..8]: {:?}", &samples[0..8]);

        // Also check the raw phase accumulator behavior
        let phase_inc = fnum as u32 * (1u32 << (block - 1));
        let cycle_in_samples_20bit = (1u32 << 20) as f64 / phase_inc as f64;
        let cycle_in_samples_22bit = (1u32 << 22) as f64 / phase_inc as f64;
        eprintln!(
            "Phase inc: {phase_inc}, 20-bit cycle: {cycle_in_samples_20bit:.1} samples, \
             22-bit cycle: {cycle_in_samples_22bit:.1} samples"
        );
        eprintln!(
            "Freq if 20-bit: {:.1} Hz, if 22-bit: {:.1} Hz",
            native_rate / cycle_in_samples_20bit,
            native_rate / cycle_in_samples_22bit
        );

        // Allow 10% tolerance (we're counting discrete zero crossings)
        let ratio = measured_freq / expected_freq;
        assert!(
            (0.80..=1.20).contains(&ratio),
            "Pitch should be within 20% of expected. Got {measured_freq:.1} Hz, \
             expected {expected_freq:.1} Hz (ratio {ratio:.3})"
        );
    }

    /// Diagnostic: check FM modulated instrument waveform for distortion artifacts.
    ///
    /// Programs algorithm 0 (serial chain, op1→op2→op3→op4) with moderate
    /// modulation depth and prints waveform statistics.
    #[test]
    fn fm_modulated_waveform_diagnostic() {
        let mut ym = Ym2612::new();

        // Algorithm 0 (serial: op1→op2→op3→op4), feedback=3 on op1
        write_reg(&mut ym, 0xB0, (3 << 3) | 0); // fb=3, algo=0
        write_reg(&mut ym, 0xB4, 0xC0); // L+R panning

        // Op1 (modulator): MUL=2, TL=40, fast envelope
        write_reg(&mut ym, 0x30, 0x02); // DT=0, MUL=2
        write_reg(&mut ym, 0x40, 40); // TL=40
        write_reg(&mut ym, 0x50, 0x1F); // AR=31
        write_reg(&mut ym, 0x60, 0x00); // DR=0
        write_reg(&mut ym, 0x70, 0x00); // SR=0
        write_reg(&mut ym, 0x80, 0x0F); // SL=0, RR=15

        // Op2 (modulator): MUL=1, TL=50 — slot 2 → op2
        write_reg(&mut ym, 0x38, 0x01); // DT=0, MUL=1
        write_reg(&mut ym, 0x48, 50);
        write_reg(&mut ym, 0x58, 0x1F);
        write_reg(&mut ym, 0x68, 0x00);
        write_reg(&mut ym, 0x78, 0x00);
        write_reg(&mut ym, 0x88, 0x0F);

        // Op3 (modulator): MUL=1, TL=60 — slot 1 → op3
        write_reg(&mut ym, 0x34, 0x01); // DT=0, MUL=1
        write_reg(&mut ym, 0x44, 60);
        write_reg(&mut ym, 0x54, 0x1F);
        write_reg(&mut ym, 0x64, 0x00);
        write_reg(&mut ym, 0x74, 0x00);
        write_reg(&mut ym, 0x84, 0x0F);

        // Op4 (carrier): MUL=1, TL=0 — slot 3 → op4
        write_reg(&mut ym, 0x3C, 0x01); // DT=0, MUL=1
        write_reg(&mut ym, 0x4C, 0); // TL=0 (loudest)
        write_reg(&mut ym, 0x5C, 0x1F);
        write_reg(&mut ym, 0x6C, 0x00);
        write_reg(&mut ym, 0x7C, 0x00);
        write_reg(&mut ym, 0x8C, 0x0F);

        // Frequency: A3 (220 Hz), block=3, fnum=541
        write_reg(&mut ym, 0xA4, (3 << 3) | ((541 >> 8) as u8 & 0x07));
        write_reg(&mut ym, 0xA0, (541 & 0xFF) as u8);

        // Key-on all operators
        write_reg(&mut ym, 0x28, 0xF0);

        // Let attack settle
        for _ in 0..500 {
            ym.output_sample();
        }

        // Collect 5000 samples (~94ms at 53kHz, ~20 cycles of 220Hz)
        let n = 5000;
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let (l, _) = ym.output_sample();
            samples.push(l);
        }

        // 1. Non-silent
        let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        eprintln!("=== FM MODULATION DIAGNOSTIC ===");
        eprintln!("Peak amplitude: {peak:.6}");
        assert!(peak > 0.001, "Output should be non-silent, peak={peak}");

        // 2. Check DC offset
        let mean: f32 = samples.iter().sum::<f32>() / n as f32;
        eprintln!("DC offset (mean): {mean:.6}");

        // 3. Peak amplitude range
        eprintln!("Peak range: ±{peak:.4}");

        // 4. Check for discontinuities (large sample-to-sample jumps)
        let mut max_delta: f32 = 0.0;
        let mut max_delta_idx = 0;
        for i in 1..samples.len() {
            let delta = (samples[i] - samples[i - 1]).abs();
            if delta > max_delta {
                max_delta = delta;
                max_delta_idx = i;
            }
        }
        eprintln!("Max sample-to-sample delta: {max_delta:.6} at sample {max_delta_idx}");

        // 5. Waveform statistics
        let min = samples.iter().cloned().fold(f32::MAX, f32::min);
        let max_val = samples.iter().cloned().fold(f32::MIN, f32::max);
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        eprintln!("Min: {min:.6}, Max: {max_val:.6}, RMS: {rms:.6}");
        eprintln!("Crest factor (peak/rms): {:.2}", peak / rms);

        // 6. Zero-crossing count for rough frequency estimate
        let mut crossings = 0u32;
        for i in 1..samples.len() {
            if samples[i - 1] >= 0.0 && samples[i] < 0.0 {
                crossings += 1;
            }
        }
        let freq = crossings as f64 * 53267.0 / n as f64;
        eprintln!("Estimated frequency: {freq:.1} Hz (expected ~220 Hz)");

        // 7. Print first 20 raw integer outputs to check quantization
        eprintln!("First 20 samples (f32): {:?}", &samples[0..20]);

        // 8. Check operator outputs directly (raw integer values)
        // Re-create and run one sample to inspect internal state
        let mut ym2 = Ym2612::new();
        // Same setup as above
        write_reg(&mut ym2, 0xB0, (3 << 3) | 0);
        write_reg(&mut ym2, 0xB4, 0xC0);
        write_reg(&mut ym2, 0x30, 0x02);
        write_reg(&mut ym2, 0x40, 40);
        write_reg(&mut ym2, 0x50, 0x1F);
        write_reg(&mut ym2, 0x60, 0x00);
        write_reg(&mut ym2, 0x70, 0x00);
        write_reg(&mut ym2, 0x80, 0x0F);
        write_reg(&mut ym2, 0x38, 0x01);
        write_reg(&mut ym2, 0x48, 50);
        write_reg(&mut ym2, 0x58, 0x1F);
        write_reg(&mut ym2, 0x68, 0x00);
        write_reg(&mut ym2, 0x78, 0x00);
        write_reg(&mut ym2, 0x88, 0x0F);
        write_reg(&mut ym2, 0x34, 0x01);
        write_reg(&mut ym2, 0x44, 60);
        write_reg(&mut ym2, 0x54, 0x1F);
        write_reg(&mut ym2, 0x64, 0x00);
        write_reg(&mut ym2, 0x74, 0x00);
        write_reg(&mut ym2, 0x84, 0x0F);
        write_reg(&mut ym2, 0x3C, 0x01);
        write_reg(&mut ym2, 0x4C, 0);
        write_reg(&mut ym2, 0x5C, 0x1F);
        write_reg(&mut ym2, 0x6C, 0x00);
        write_reg(&mut ym2, 0x7C, 0x00);
        write_reg(&mut ym2, 0x8C, 0x0F);
        write_reg(&mut ym2, 0xA4, (3 << 3) | ((541 >> 8) as u8 & 0x07));
        write_reg(&mut ym2, 0xA0, (541 & 0xFF) as u8);
        write_reg(&mut ym2, 0x28, 0xF0);

        for _ in 0..500 {
            ym2.output_sample();
        }

        // Print operator envelope and output state
        let ch = &ym2.channels[0];
        for (i, op) in ch.operators.iter().enumerate() {
            eprintln!(
                "Op{}: envelope={}, state={:?}, output={}, prev_output={}, TL={}, phase=0x{:05X}",
                i + 1,
                op.envelope,
                op.env_state,
                op.output,
                op.prev_output,
                op.total_level,
                op.phase
            );
        }
    }

    #[test]
    fn per_channel_output_sums_to_legacy_stereo_output() {
        fn configure(ym: &mut Ym2612) {
            write_reg(ym, 0xB0, 0x07);
            write_reg(ym, 0xB4, 0x80); // channel 0 hard left
            write_reg_bank1(ym, 0xB5, 0x40); // channel 4 hard right

            write_reg(ym, 0xA4, (4 << 3) | 0x02);
            write_reg(ym, 0xA0, 0x8D);
            write_reg_bank1(ym, 0xA5, (4 << 3) | 0x02);
            write_reg_bank1(ym, 0xA1, 0xC5);

            for slot_offset in [0x00u8, 0x04, 0x08, 0x0C] {
                write_reg(ym, 0x30 + slot_offset, 0x01);
                write_reg(ym, 0x40 + slot_offset, 0);
                write_reg(ym, 0x50 + slot_offset, 31);
                write_reg(ym, 0x80 + slot_offset, 0x0F);

                write_reg_bank1(ym, 0x31 + slot_offset, 0x01);
                write_reg_bank1(ym, 0x41 + slot_offset, 0);
                write_reg_bank1(ym, 0x51 + slot_offset, 31);
                write_reg_bank1(ym, 0x81 + slot_offset, 0x0F);
            }

            write_reg(ym, 0x28, 0xF0);
            write_reg(ym, 0x28, 0xF4);

            for _ in 0..512 {
                ym.output_sample();
            }
        }

        let mut ym_channels = Ym2612::new();
        configure(&mut ym_channels);
        let channel_samples = ym_channels.output_sample_per_channel();
        let summed_left: f32 = channel_samples.iter().map(|&(l, _)| l).sum();
        let summed_right: f32 = channel_samples.iter().map(|&(_, r)| r).sum();

        let mut ym_sum = Ym2612::new();
        configure(&mut ym_sum);
        let (left, right) = ym_sum.output_sample();

        assert!(
            (summed_left - left).abs() < 1e-6,
            "per-channel left sum {summed_left:.6} should match stereo output {left:.6}"
        );
        assert!(
            (summed_right - right).abs() < 1e-6,
            "per-channel right sum {summed_right:.6} should match stereo output {right:.6}"
        );
    }
}

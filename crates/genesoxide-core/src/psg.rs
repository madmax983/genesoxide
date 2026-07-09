//! SN76489 PSG (Programmable Sound Generator) emulation.
//!
//! The SN76489 provides 3 square-wave tone channels and 1 LFSR noise channel.
//! Each channel has a 4-bit attenuation register (0 = loudest, 15 = silent).
//! Tone channels use a 10-bit period divider, and the noise channel uses a
//! 16-bit linear-feedback shift register (LFSR) in either periodic or white
//! noise mode.
//!
//! In the Genesis, the PSG is written by the 68000 at 0xC00011 and by the Z80
//! at 0x7F11. Both route to [`Psg::write`].

use serde::{Deserialize, Serialize};

// ── Volume table ──────────────────────────────────────────────────────────

/// 2 dB attenuation steps. Index 0 is maximum volume, index 15 is silent.
const VOLUME_TABLE: [f32; 16] = [
    1.0,   // 0:  0 dB (max)
    0.794, // 1: -2 dB
    0.631, // 2: -4 dB
    0.501, // 3: -6 dB
    0.398, // 4: -8 dB
    0.316, // 5: -10 dB
    0.251, // 6: -12 dB
    0.200, // 7: -14 dB
    0.158, // 8: -16 dB
    0.126, // 9: -18 dB
    0.100, // 10: -20 dB
    0.079, // 11: -22 dB
    0.063, // 12: -24 dB
    0.050, // 13: -26 dB
    0.040, // 14: -28 dB
    0.0,   // 15: silent
];

/// Fixed reload values for noise rates 0, 1, 2.
const NOISE_PERIOD_TABLE: [u16; 3] = [0x10, 0x20, 0x40];

// ── PSG state ─────────────────────────────────────────────────────────────

/// SN76489 Programmable Sound Generator.
///
/// Contains all registers and internal counters needed to produce audio.
/// Call [`Psg::write`] to send a register byte and [`Psg::clock_tick`] each
/// PSG clock cycle (master / 16, ~223 kHz). Read the current output with
/// [`Psg::sample`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Psg {
    /// 10-bit period registers for tone channels 0-2.
    tone_period: [u16; 3],
    /// Current countdown counters for tone channels.
    tone_counter: [u16; 3],
    /// Current output polarity for tone channels (true = +1, false = -1).
    tone_polarity: [bool; 3],
    /// 4-bit attenuation per channel (0 = loud, 15 = silent). Index 3 is noise.
    volume: [u8; 4],
    /// 16-bit LFSR (noise shift register).
    noise_shift: u16,
    /// Noise mode: `false` = periodic, `true` = white noise.
    noise_mode: bool,
    /// Noise rate selector (0-3). Rate 3 uses tone channel 2's period.
    noise_rate: u8,
    /// Current countdown counter for the noise channel.
    noise_counter: u16,
    /// Current output polarity for the noise channel.
    noise_polarity: bool,
    /// Last latched channel (0-3, where 3 = noise).
    latch_channel: u8,
    /// Whether the latched register is volume (`true`) or tone/noise (`false`).
    latch_is_volume: bool,
    /// PSG divides its input clock by 16.
    clock_divider: u8,
}

impl Psg {
    /// Create a new PSG in its power-on state.
    ///
    /// All channels are silenced (volume = 15), the LFSR is seeded to 0x8000,
    /// and all counters/periods are zeroed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tone_period: [0; 3],
            tone_counter: [0; 3],
            tone_polarity: [false; 3],
            volume: [15; 4],
            noise_shift: 0x8000,
            noise_mode: false,
            noise_rate: 0,
            noise_counter: 0,
            noise_polarity: false,
            latch_channel: 0,
            latch_is_volume: false,
            clock_divider: 0,
        }
    }

    /// Write a register byte to the PSG.
    ///
    /// The SN76489 uses a latch/data protocol:
    ///
    /// - **Latch byte** (bit 7 set): selects a channel and register, writes
    ///   low-order data.
    /// - **Data byte** (bit 7 clear): writes high-order data to the previously
    ///   latched register.
    pub fn write(&mut self, val: u8) {
        if val & 0x80 != 0 {
            // Latch byte
            self.latch_channel = (val >> 5) & 0x03;
            self.latch_is_volume = val & 0x10 != 0;
            let data = val & 0x0F;

            if self.latch_is_volume {
                self.volume[self.latch_channel as usize] = data;
            } else if self.latch_channel == 3 {
                // Noise control register
                self.noise_rate = data & 0x03;
                self.noise_mode = data & 0x04 != 0;
                self.noise_shift = 0x8000;
            } else {
                // Tone: write low 4 bits of period
                let ch = self.latch_channel as usize;
                self.tone_period[ch] = (self.tone_period[ch] & 0x3F0) | u16::from(data);
            }
        } else {
            // Data byte
            let data = val & 0x3F;

            if self.latch_is_volume {
                self.volume[self.latch_channel as usize] = data & 0x0F;
            } else if self.latch_channel == 3 {
                // Noise control (same as latch)
                self.noise_rate = data & 0x03;
                self.noise_mode = data & 0x04 != 0;
                self.noise_shift = 0x8000;
            } else {
                // Tone: write high 6 bits of period (bits 9-4)
                let ch = self.latch_channel as usize;
                self.tone_period[ch] = (self.tone_period[ch] & 0x00F) | (u16::from(data) << 4);
            }
        }
    }

    /// Advance the PSG by one clock cycle.
    ///
    /// Should be called at the PSG clock rate (master / 16, ~223 kHz).
    /// Each tick decrements internal counters; when they expire the output
    /// polarity toggles (tone channels) or the LFSR shifts (noise channel).
    pub fn clock_tick(&mut self) {
        // ── Tone channels ────────────────────────────────────────────
        for ch in 0..3 {
            if self.tone_period[ch] == 0 {
                // Period 0: output is always +1, no toggling.
                self.tone_polarity[ch] = true;
                continue;
            }

            if self.tone_counter[ch] == 0 {
                self.tone_counter[ch] = self.tone_period[ch];
                self.tone_polarity[ch] = !self.tone_polarity[ch];
            } else {
                self.tone_counter[ch] -= 1;
            }
        }

        // ── Noise channel ────────────────────────────────────────────
        let noise_period = if self.noise_rate < 3 {
            NOISE_PERIOD_TABLE[self.noise_rate as usize]
        } else {
            self.tone_period[2]
        };

        if self.noise_counter == 0 {
            self.noise_counter = if noise_period == 0 { 1 } else { noise_period };

            // Track the internal toggle for state parity, but clock the LFSR on
            // EVERY counter expiry — on real SN76489 hardware the shift register
            // advances each time the noise counter reloads (once per half-period
            // of the driving tone), not only on the high->low toggle transition.
            self.noise_polarity = !self.noise_polarity;

            let feedback = if self.noise_mode {
                // White noise: input bit = bit 0 XOR bit 3
                (self.noise_shift & 1) ^ ((self.noise_shift >> 3) & 1)
            } else {
                // Periodic noise: input bit = bit 0
                self.noise_shift & 1
            };
            self.noise_shift = (self.noise_shift >> 1) | (feedback << 15);
        } else {
            self.noise_counter -= 1;
        }
    }

    /// Return the current mixed audio sample as a normalized float.
    ///
    /// Sums all 4 channels (3 tone + 1 noise), applying per-channel volume
    /// attenuation, then divides by 4 to keep the result in roughly [-1, +1].
    #[must_use]
    pub fn sample(&self) -> f32 {
        let mut output = 0.0_f32;

        for ch in 0..3 {
            if self.volume[ch] < 15 {
                let level = VOLUME_TABLE[self.volume[ch] as usize];
                output += if self.tone_polarity[ch] {
                    level
                } else {
                    -level
                };
            }
        }

        // Noise channel: output based on bit 0 of LFSR.
        if self.volume[3] < 15 {
            let level = VOLUME_TABLE[self.volume[3] as usize];
            let noise_bit = self.noise_shift & 1 != 0;
            output += if noise_bit { level } else { -level };
        }

        output / 4.0
    }
}

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_table_endpoints() {
        assert!((VOLUME_TABLE[0] - 1.0).abs() < f32::EPSILON);
        assert!((VOLUME_TABLE[15] - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn latch_byte_selects_channel() {
        let mut psg = Psg::new();

        // 0x80 = latch ch0 tone (channel bits = 00, type bit = 0)
        psg.write(0x80);
        assert_eq!(psg.latch_channel, 0);
        assert!(!psg.latch_is_volume);

        // 0xA0 = latch ch1 tone (channel bits = 01, type bit = 0)
        psg.write(0xA0);
        assert_eq!(psg.latch_channel, 1);
        assert!(!psg.latch_is_volume);

        // 0xC0 = latch ch2 tone
        psg.write(0xC0);
        assert_eq!(psg.latch_channel, 2);

        // 0xE0 = latch noise channel tone/noise register
        psg.write(0xE0);
        assert_eq!(psg.latch_channel, 3);
    }

    #[test]
    fn tone_period_write() {
        let mut psg = Psg::new();

        // Latch ch0 tone with low nibble = 0x05 -> bits 3-0 = 5
        psg.write(0x85); // 1_00_0_0101
        assert_eq!(psg.tone_period[0] & 0x00F, 5);

        // Data byte with high bits = 0x1A (binary 011010) -> bits 9-4
        psg.write(0x1A); // 0_011010
        // Period = (0x1A << 4) | 5 = (26 << 4) | 5 = 416 | 5 = 421
        assert_eq!(psg.tone_period[0], (0x1A << 4) | 5);
        assert_eq!(psg.tone_period[0], 421);
    }

    #[test]
    fn volume_write() {
        let mut psg = Psg::new();

        // Latch ch0 volume with value 7: 1_00_1_0111 = 0x97
        psg.write(0x97);
        assert_eq!(psg.volume[0], 7);

        // Latch ch1 volume with value 0 (max): 1_01_1_0000 = 0xB0
        psg.write(0xB0);
        assert_eq!(psg.volume[1], 0);
    }

    #[test]
    fn noise_lfsr_white() {
        let mut psg = Psg::new();

        // Set white noise, rate 0 (period = 0x10).
        // 0xE4 = 1_11_0_0100 => ch3, type=tone/noise, data = 0b0100 => rate=0, mode=white
        psg.write(0xE4);
        assert!(psg.noise_mode);
        assert_eq!(psg.noise_rate, 0);
        assert_eq!(psg.noise_shift, 0x8000);

        // Set noise volume to 0 (audible) so we can verify
        psg.write(0xF0); // 1_11_1_0000 => ch3 volume = 0

        // Collect LFSR state snapshots after falling edges.
        // We need to clock until counter expires and we get a falling edge.
        // Rate 0 -> period 0x10 = 16 ticks per half-cycle.
        // Clock enough to capture several LFSR shifts.
        // Each shift happens on a falling edge (polarity: true -> false),
        // which means we need 2 full half-cycles = 2 * 16 = 32 ticks for
        // the first falling edge, then 32 more for the second, etc.
        for _ in 0..256 {
            psg.clock_tick();
        }

        // The LFSR should have shifted from its initial 0x8000 state.
        assert_ne!(psg.noise_shift, 0x8000, "LFSR should have shifted");

        // White noise uses feedback = bit0 XOR bit3, so the sequence is
        // deterministic. Verify the LFSR didn't degenerate to 0.
        assert_ne!(psg.noise_shift, 0, "LFSR should not be zero");
    }

    #[test]
    fn noise_lfsr_shifts_every_counter_reload() {
        // Regression for the half-rate noise bug: the LFSR must advance on EVERY
        // noise-counter expiry, not only on the high->low toggle transition.
        //
        // Rate 0 -> reload period 0x10 (16). One expiry occurs every 17 ticks
        // (1 reload tick + 16 decrement ticks), so 4352 ticks yields exactly
        // 256 counter expiries -> 256 LFSR shifts.
        //
        //   Pre-fix (buggy, half-rate): 128 shifts.
        //   Post-fix (correct):         256 shifts.
        //
        // Use periodic mode seeded at 0x8000: the single set bit walks the
        // register and the value strictly changes on every shift, so counting
        // register transitions counts shifts.
        let mut psg = Psg::new();

        // 0xE0 = ch3 tone/noise, data 0b0000 => rate 0, periodic mode.
        psg.write(0xE0);
        assert!(!psg.noise_mode);
        assert_eq!(psg.noise_rate, 0);
        assert_eq!(psg.noise_shift, 0x8000);

        let mut shifts = 0u32;
        let mut prev = psg.noise_shift;
        for _ in 0..4352 {
            psg.clock_tick();
            if psg.noise_shift != prev {
                shifts += 1;
                prev = psg.noise_shift;
            }
        }

        assert_eq!(
            shifts, 256,
            "LFSR must shift once per counter reload (256), not the half-rate 128"
        );
    }

    #[test]
    fn silent_on_init() {
        let psg = Psg::new();
        assert!(
            psg.sample().abs() < f32::EPSILON,
            "new PSG should output silence, got {}",
            psg.sample()
        );
    }
}

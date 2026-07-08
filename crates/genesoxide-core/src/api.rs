//! The core emulator API surface.
//!
//! Provides the primary interface for controlling the Genesis emulator.
//! The [`GenesisCore`] struct is the main entry point for host applications.
//! Frontends drive it via [`Command`] and poll state via [`CoreQuery`].

use crate::bus;
use crate::cpu::execute::Bus;
use crate::cpu::{self, Cpu};
use crate::io::ControllerPort;
use crate::psg;
use crate::rewind;
use crate::rom::{self, RomHeader};
use crate::scheduler::{MASTER_CLOCK_NTSC, MASTER_PER_CPU, MASTER_PER_Z80, Scheduler};
use crate::vdp::Vdp;
use crate::ym2612;
use crate::z80;
use serde::{Deserialize, Serialize};

/// Genesis visible frame width in pixels (H40 mode).
pub const FRAME_WIDTH: usize = 320;
/// Genesis visible frame height in pixels (NTSC).
pub const FRAME_HEIGHT: usize = 224;
/// Framebuffer byte count for RGBA8 format.
pub const FRAME_RGBA_BYTES: usize = FRAME_WIDTH * FRAME_HEIGHT * 4;
/// NTSC frame rate in millihertz (59.92 Hz * 1000).
pub const FPS_MILLI: u32 = 59_920;
/// NTSC frame period in nanoseconds.
/// Derived from master clock: 53_693_175 Hz / (3420 dots × 262 lines) = 59.9227 Hz.
pub const FRAME_PERIOD_NS: u64 = 16_688_155;

/// Scanlines per frame (NTSC): 224 active + 38 blanking = 262 total.
pub const SCANLINES_PER_FRAME: u16 = 262;
/// Active (visible) scanlines.
pub const ACTIVE_SCANLINES: u16 = 224;
/// NTSC Genesis master-clock ticks per scanline in H40 timing.
const MASTER_TICKS_PER_SCANLINE: u64 = 3420;
/// YM2612 audio clock period in master-clock ticks.
const YM_AUDIO_TICKS: u64 = 1008;
/// PSG audio clock period in master-clock ticks.
const PSG_AUDIO_TICKS: u64 = 240;
/// Model 1 VA0-VA2 style 3.39 kHz first-order low-pass, applied at YM native rate.
const YM_LPF_B0: f32 = 0.168_498_34;
const YM_LPF_B1: f32 = 0.168_498_34;
const YM_LPF_A1: f32 = -0.663_003_3;
/// Same 3.39 kHz first-order low-pass, but designed for PSG native rate.
const PSG_LPF_B0: f32 = 0.045_473_456;
const PSG_LPF_B1: f32 = 0.045_473_456;
const PSG_LPF_A1: f32 = -0.909_053_1;
const LEGACY_YM_CUTOFF_HZ: f32 = 14_000.0;
const DEFAULT_PSG_MIX: f32 = 0.22;
const MAX_POST_DELAY_SAMPLES: usize = 4;

/// Genesis controller button (re-exported from io module).
pub use crate::io::Button;

/// Post-mix analog/capture profile applied after raw YM2612/PSG synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputProfile {
    /// Legacy genesoxide path: 14 kHz YM Butterworth, PSG unfiltered.
    Legacy,
    /// Model 1 VA0-VA2 style low-pass on both YM and PSG paths.
    Model1Va2,
}

/// Kind of post-mix EQ stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEqKind {
    /// Low-shelf biquad.
    LowShelf,
    /// Peaking / bell biquad.
    Peaking,
    /// High-shelf biquad.
    HighShelf,
}

/// A single post-mix EQ stage shared by the live core and replay renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioEqStage {
    /// Filter topology.
    pub kind: AudioEqKind,
    /// Center/corner frequency in Hz.
    pub frequency_hz: f32,
    /// Positive boosts, negative cuts.
    pub gain_db: f32,
    /// Q for peaking filters, slope for shelf filters.
    pub q: f32,
}

impl AudioEqStage {
    /// Create a low-shelf EQ stage.
    #[must_use]
    pub const fn low_shelf(frequency_hz: f32, gain_db: f32) -> Self {
        Self {
            kind: AudioEqKind::LowShelf,
            frequency_hz,
            gain_db,
            q: 1.0,
        }
    }

    /// Create a peaking EQ stage.
    #[must_use]
    pub const fn peaking(frequency_hz: f32, q: f32, gain_db: f32) -> Self {
        Self {
            kind: AudioEqKind::Peaking,
            frequency_hz,
            gain_db,
            q,
        }
    }

    /// Create a high-shelf EQ stage.
    #[must_use]
    pub const fn high_shelf(frequency_hz: f32, gain_db: f32) -> Self {
        Self {
            kind: AudioEqKind::HighShelf,
            frequency_hz,
            gain_db,
            q: 1.0,
        }
    }
}

/// Shared audio output settings for the live core and timed replay renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioOutputConfig {
    /// Analog/output profile selection.
    pub profile: AudioOutputProfile,
    /// Post-mix master gain applied before final clamp.
    pub master_gain: f32,
    /// YM path gain applied after YM filtering, before final mix.
    pub ym_gain: f32,
    /// PSG path gain applied after PSG filtering and baseline PSG mix.
    pub psg_gain: f32,
    /// Per-YM-channel delayed side-memory amount applied before the YM sum.
    pub ym_channel_side_memory_amounts: [f32; 6],
    /// Per-YM-channel blend between raw side feed and side-change feed.
    /// `0.0` keeps the old sustained-side feed; `1.0` only feeds side changes.
    pub ym_channel_side_transient_mixes: [f32; 6],
    /// Per-YM-channel blend between raw delayed polarity and current-polarity side lift.
    /// `0.0` replays the delayed sample as-is; `1.0` keeps only its magnitude and follows
    /// the current side polarity.
    pub ym_channel_side_sign_align_mixes: [f32; 6],
    /// Per-YM-channel side-memory decay time in milliseconds at YM native rate.
    pub ym_channel_side_decay_ms: [f32; 6],
    /// Per-YM-channel delay applied to YM key-on/key-off writes.
    pub ym_channel_key_delay_ms: [f32; 6],
    /// Per-YM-channel pan-edge persistence impulse applied on YM pan changes.
    pub ym_channel_pan_edge_amounts: [f32; 6],
    /// Per-YM-channel pan-edge persistence decay time in milliseconds.
    pub ym_channel_pan_edge_decay_ms: [f32; 6],
    /// Stereo crossfeed amount after post-mix shaping, before master gain.
    pub stereo_crossfeed: f32,
    /// Mid channel gain after left/right shaping and crossfeed.
    pub mid_gain: f32,
    /// Side channel gain after left/right shaping and crossfeed.
    pub side_gain: f32,
    /// Optional post-mix high-pass stage in Hz.
    pub post_high_pass_hz: Option<f32>,
    /// Optional post-mix low-pass stage in Hz.
    pub post_low_pass_hz: Option<f32>,
    /// Optional first post-mix EQ stage.
    pub post_eq_1: Option<AudioEqStage>,
    /// Optional second post-mix EQ stage.
    pub post_eq_2: Option<AudioEqStage>,
    /// Optional third post-mix EQ stage.
    pub post_eq_3: Option<AudioEqStage>,
    /// Optional fourth post-mix EQ stage.
    pub post_eq_4: Option<AudioEqStage>,
    /// Optional fifth post-mix EQ stage.
    pub post_eq_5: Option<AudioEqStage>,
    /// Optional first post-mix EQ stage applied only to the side channel.
    pub post_side_eq_1: Option<AudioEqStage>,
    /// Optional second post-mix EQ stage applied only to the side channel.
    pub post_side_eq_2: Option<AudioEqStage>,
    /// Optional short post-mix FIR stage shared by both channels.
    pub post_fir_taps: Option<[f32; 5]>,
    /// Optional post-mix sample delay applied to the left channel.
    pub post_left_delay_samples: u8,
    /// Optional post-mix sample delay applied to the right channel.
    pub post_right_delay_samples: u8,
}

impl AudioOutputConfig {
    /// Creates an audio output config.
    #[must_use]
    pub const fn new(profile: AudioOutputProfile, master_gain: f32) -> Self {
        Self {
            profile,
            master_gain,
            ym_gain: 1.0,
            psg_gain: 1.0,
            ym_channel_side_memory_amounts: [0.0; 6],
            ym_channel_side_transient_mixes: [0.0; 6],
            ym_channel_side_sign_align_mixes: [0.0; 6],
            ym_channel_side_decay_ms: [0.0; 6],
            ym_channel_key_delay_ms: [0.0; 6],
            ym_channel_pan_edge_amounts: [0.0; 6],
            ym_channel_pan_edge_decay_ms: [0.0; 6],
            stereo_crossfeed: 0.0,
            mid_gain: 1.0,
            side_gain: 1.0,
            post_high_pass_hz: None,
            post_low_pass_hz: None,
            post_eq_1: None,
            post_eq_2: None,
            post_eq_3: None,
            post_eq_4: None,
            post_eq_5: None,
            post_side_eq_1: None,
            post_side_eq_2: None,
            post_fir_taps: None,
            post_left_delay_samples: 0,
            post_right_delay_samples: 0,
        }
    }

    /// Legacy YM-filtered / PSG-flat output profile.
    #[must_use]
    pub const fn legacy() -> Self {
        Self::new(AudioOutputProfile::Legacy, 1.0)
    }

    /// Model 1 VA0-VA2 style output profile.
    #[must_use]
    pub const fn model1_va2() -> Self {
        Self::new(AudioOutputProfile::Model1Va2, 1.0)
    }

    /// Returns this profile with a different post-mix gain.
    #[must_use]
    pub const fn with_gain(self, master_gain: f32) -> Self {
        Self {
            master_gain,
            ..self
        }
    }

    /// Returns this config with a different YM path gain.
    #[must_use]
    pub const fn with_ym_gain(self, ym_gain: f32) -> Self {
        Self { ym_gain, ..self }
    }

    /// Returns this config with a different PSG path gain.
    #[must_use]
    pub const fn with_psg_gain(self, psg_gain: f32) -> Self {
        Self { psg_gain, ..self }
    }

    /// Returns this config with per-channel YM delayed side-memory amounts.
    #[must_use]
    pub const fn with_ym_channel_side_memory_amounts(
        self,
        ym_channel_side_memory_amounts: [f32; 6],
    ) -> Self {
        Self {
            ym_channel_side_memory_amounts,
            ..self
        }
    }

    /// Returns this config with per-channel YM side-memory transient blends.
    #[must_use]
    pub const fn with_ym_channel_side_transient_mixes(
        self,
        ym_channel_side_transient_mixes: [f32; 6],
    ) -> Self {
        Self {
            ym_channel_side_transient_mixes,
            ..self
        }
    }

    /// Returns this config with per-channel YM side-memory sign-alignment blends.
    #[must_use]
    pub const fn with_ym_channel_side_sign_align_mixes(
        self,
        ym_channel_side_sign_align_mixes: [f32; 6],
    ) -> Self {
        Self {
            ym_channel_side_sign_align_mixes,
            ..self
        }
    }

    /// Returns this config with per-channel YM side-memory decay times.
    #[must_use]
    pub const fn with_ym_channel_side_decay_ms(self, ym_channel_side_decay_ms: [f32; 6]) -> Self {
        Self {
            ym_channel_side_decay_ms,
            ..self
        }
    }

    /// Returns this config with per-channel YM key-write delays in milliseconds.
    #[must_use]
    pub const fn with_ym_channel_key_delay_ms(self, ym_channel_key_delay_ms: [f32; 6]) -> Self {
        Self {
            ym_channel_key_delay_ms,
            ..self
        }
    }

    /// Returns this config with per-channel YM pan-edge persistence impulse amounts.
    #[must_use]
    pub const fn with_ym_channel_pan_edge_amounts(
        self,
        ym_channel_pan_edge_amounts: [f32; 6],
    ) -> Self {
        Self {
            ym_channel_pan_edge_amounts,
            ..self
        }
    }

    /// Returns this config with per-channel YM pan-edge persistence decay times.
    #[must_use]
    pub const fn with_ym_channel_pan_edge_decay_ms(
        self,
        ym_channel_pan_edge_decay_ms: [f32; 6],
    ) -> Self {
        Self {
            ym_channel_pan_edge_decay_ms,
            ..self
        }
    }

    /// Returns this config with a different stereo crossfeed amount.
    #[must_use]
    pub const fn with_stereo_crossfeed(self, stereo_crossfeed: f32) -> Self {
        Self {
            stereo_crossfeed,
            ..self
        }
    }

    /// Returns this config with a different mid gain.
    #[must_use]
    pub const fn with_mid_gain(self, mid_gain: f32) -> Self {
        Self { mid_gain, ..self }
    }

    /// Returns this config with a different side gain.
    #[must_use]
    pub const fn with_side_gain(self, side_gain: f32) -> Self {
        Self { side_gain, ..self }
    }

    /// Returns this config with an optional post-mix high-pass stage.
    #[must_use]
    pub const fn with_post_high_pass_hz(self, post_high_pass_hz: f32) -> Self {
        Self {
            post_high_pass_hz: Some(post_high_pass_hz),
            ..self
        }
    }

    /// Returns this config with an optional post-mix low-pass stage.
    #[must_use]
    pub const fn with_post_low_pass_hz(self, post_low_pass_hz: f32) -> Self {
        Self {
            post_low_pass_hz: Some(post_low_pass_hz),
            ..self
        }
    }

    /// Returns this config with a first post-mix EQ stage.
    #[must_use]
    pub const fn with_post_eq_1(self, post_eq_1: AudioEqStage) -> Self {
        Self {
            post_eq_1: Some(post_eq_1),
            ..self
        }
    }

    /// Returns this config with a second post-mix EQ stage.
    #[must_use]
    pub const fn with_post_eq_2(self, post_eq_2: AudioEqStage) -> Self {
        Self {
            post_eq_2: Some(post_eq_2),
            ..self
        }
    }

    /// Returns this config with a third post-mix EQ stage.
    #[must_use]
    pub const fn with_post_eq_3(self, post_eq_3: AudioEqStage) -> Self {
        Self {
            post_eq_3: Some(post_eq_3),
            ..self
        }
    }

    /// Returns this config with a fourth post-mix EQ stage.
    #[must_use]
    pub const fn with_post_eq_4(self, post_eq_4: AudioEqStage) -> Self {
        Self {
            post_eq_4: Some(post_eq_4),
            ..self
        }
    }

    /// Returns this config with a fifth post-mix EQ stage.
    #[must_use]
    pub const fn with_post_eq_5(self, post_eq_5: AudioEqStage) -> Self {
        Self {
            post_eq_5: Some(post_eq_5),
            ..self
        }
    }

    /// Returns this config with a first side-channel EQ stage.
    #[must_use]
    pub const fn with_post_side_eq_1(self, post_side_eq_1: AudioEqStage) -> Self {
        Self {
            post_side_eq_1: Some(post_side_eq_1),
            ..self
        }
    }

    /// Returns this config with a second side-channel EQ stage.
    #[must_use]
    pub const fn with_post_side_eq_2(self, post_side_eq_2: AudioEqStage) -> Self {
        Self {
            post_side_eq_2: Some(post_side_eq_2),
            ..self
        }
    }

    /// Returns this config with an optional short post-mix FIR stage.
    #[must_use]
    pub const fn with_post_fir_taps(self, post_fir_taps: [f32; 5]) -> Self {
        Self {
            post_fir_taps: Some(post_fir_taps),
            ..self
        }
    }

    /// Returns this config with a post-mix sample delay on the left channel.
    #[must_use]
    pub const fn with_post_left_delay_samples(self, post_left_delay_samples: u8) -> Self {
        Self {
            post_left_delay_samples,
            ..self
        }
    }

    /// Returns this config with a post-mix sample delay on the right channel.
    #[must_use]
    pub const fn with_post_right_delay_samples(self, post_right_delay_samples: u8) -> Self {
        Self {
            post_right_delay_samples,
            ..self
        }
    }

    /// Describes the concrete filter/mix path for this config.
    #[must_use]
    pub fn spec_for_rates(self, ym_native_rate_hz: f32, output_rate_hz: f32) -> AudioOutputSpec {
        let mut spec = match self.profile {
            AudioOutputProfile::Legacy => AudioOutputSpec {
                ym_filter: biquad_low_pass_filter_spec(LEGACY_YM_CUTOFF_HZ, ym_native_rate_hz),
                psg_filter: AudioFilterSpec::Flat,
                psg_mix: DEFAULT_PSG_MIX,
                master_gain: self.master_gain,
                ym_gain: self.ym_gain,
                psg_gain: self.psg_gain,
                ym_channel_side_memory_amounts: self.ym_channel_side_memory_amounts,
                ym_channel_side_transient_mixes: self.ym_channel_side_transient_mixes,
                ym_channel_side_sign_align_mixes: self.ym_channel_side_sign_align_mixes,
                ym_channel_side_decay_factors: std::array::from_fn(|idx| {
                    side_memory_decay_factor(self.ym_channel_side_decay_ms[idx], ym_native_rate_hz)
                }),
                ym_channel_key_delay_ticks: std::array::from_fn(|idx| {
                    key_delay_ticks(self.ym_channel_key_delay_ms[idx])
                }),
                ym_channel_pan_edge_amounts: self.ym_channel_pan_edge_amounts,
                ym_channel_pan_edge_decay_factors: std::array::from_fn(|idx| {
                    pan_edge_decay_factor(self.ym_channel_pan_edge_decay_ms[idx], ym_native_rate_hz)
                }),
                stereo_crossfeed: self.stereo_crossfeed,
                mid_gain: self.mid_gain,
                side_gain: self.side_gain,
                post_high_pass: AudioFilterSpec::Flat,
                post_low_pass: AudioFilterSpec::Flat,
                post_eq_1: AudioFilterSpec::Flat,
                post_eq_2: AudioFilterSpec::Flat,
                post_eq_3: AudioFilterSpec::Flat,
                post_eq_4: AudioFilterSpec::Flat,
                post_eq_5: AudioFilterSpec::Flat,
                post_side_eq_1: AudioFilterSpec::Flat,
                post_side_eq_2: AudioFilterSpec::Flat,
                post_fir: AudioFilterSpec::Flat,
                post_left_delay_samples: self
                    .post_left_delay_samples
                    .min(MAX_POST_DELAY_SAMPLES as u8),
                post_right_delay_samples: self
                    .post_right_delay_samples
                    .min(MAX_POST_DELAY_SAMPLES as u8),
            },
            AudioOutputProfile::Model1Va2 => AudioOutputSpec {
                ym_filter: AudioFilterSpec::FirstOrder {
                    b0: YM_LPF_B0,
                    b1: YM_LPF_B1,
                    a1: YM_LPF_A1,
                },
                psg_filter: AudioFilterSpec::FirstOrder {
                    b0: PSG_LPF_B0,
                    b1: PSG_LPF_B1,
                    a1: PSG_LPF_A1,
                },
                psg_mix: DEFAULT_PSG_MIX,
                master_gain: self.master_gain,
                ym_gain: self.ym_gain,
                psg_gain: self.psg_gain,
                ym_channel_side_memory_amounts: self.ym_channel_side_memory_amounts,
                ym_channel_side_transient_mixes: self.ym_channel_side_transient_mixes,
                ym_channel_side_sign_align_mixes: self.ym_channel_side_sign_align_mixes,
                ym_channel_side_decay_factors: std::array::from_fn(|idx| {
                    side_memory_decay_factor(self.ym_channel_side_decay_ms[idx], ym_native_rate_hz)
                }),
                ym_channel_key_delay_ticks: std::array::from_fn(|idx| {
                    key_delay_ticks(self.ym_channel_key_delay_ms[idx])
                }),
                ym_channel_pan_edge_amounts: self.ym_channel_pan_edge_amounts,
                ym_channel_pan_edge_decay_factors: std::array::from_fn(|idx| {
                    pan_edge_decay_factor(self.ym_channel_pan_edge_decay_ms[idx], ym_native_rate_hz)
                }),
                stereo_crossfeed: self.stereo_crossfeed,
                mid_gain: self.mid_gain,
                side_gain: self.side_gain,
                post_high_pass: AudioFilterSpec::Flat,
                post_low_pass: AudioFilterSpec::Flat,
                post_eq_1: AudioFilterSpec::Flat,
                post_eq_2: AudioFilterSpec::Flat,
                post_eq_3: AudioFilterSpec::Flat,
                post_eq_4: AudioFilterSpec::Flat,
                post_eq_5: AudioFilterSpec::Flat,
                post_side_eq_1: AudioFilterSpec::Flat,
                post_side_eq_2: AudioFilterSpec::Flat,
                post_fir: AudioFilterSpec::Flat,
                post_left_delay_samples: self
                    .post_left_delay_samples
                    .min(MAX_POST_DELAY_SAMPLES as u8),
                post_right_delay_samples: self
                    .post_right_delay_samples
                    .min(MAX_POST_DELAY_SAMPLES as u8),
            },
        };
        if let Some(cutoff_hz) = self.post_high_pass_hz {
            spec.post_high_pass = first_order_high_pass_filter_spec(cutoff_hz, output_rate_hz);
        }
        if let Some(cutoff_hz) = self.post_low_pass_hz {
            spec.post_low_pass = first_order_low_pass_filter_spec(cutoff_hz, output_rate_hz);
        }
        if let Some(stage) = self.post_eq_1 {
            spec.post_eq_1 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_eq_2 {
            spec.post_eq_2 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_eq_3 {
            spec.post_eq_3 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_eq_4 {
            spec.post_eq_4 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_eq_5 {
            spec.post_eq_5 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_side_eq_1 {
            spec.post_side_eq_1 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(stage) = self.post_side_eq_2 {
            spec.post_side_eq_2 = biquad_eq_filter_spec(stage, output_rate_hz);
        }
        if let Some(taps) = self.post_fir_taps {
            spec.post_fir = AudioFilterSpec::Fir { taps };
        }
        spec
    }
}

impl AudioOutputConfig {
    /// GHZ-tuned colored output profile (opt-in, **not** the shipped default).
    ///
    /// This 5-stage EQ + crossfeed + side-EQ chain was historically the shipped
    /// default. It was fit against a single Green Hill Zone hardware FLAC that the
    /// fidelity log repeatedly shows is an unreliable oracle (loop-inconsistent,
    /// phase-incoherent), so it scoops sub-bass and darkens the top in a way the
    /// reference cannot justify. It is retained here as an explicitly selectable
    /// profile for anyone who wants that particular coloration.
    #[must_use]
    pub fn ghz_colored() -> Self {
        Self::legacy()
            .with_gain(2.2)
            .with_ym_gain(1.1)
            .with_psg_gain(0.65)
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.60, 0.0, 0.0])
            .with_ym_channel_side_decay_ms([0.01, 0.0, 0.0, 0.02, 0.02, 0.0])
            .with_stereo_crossfeed(0.25)
            .with_side_gain(1.05)
            .with_post_low_pass_hz(12_000.0)
            .with_post_eq_1(AudioEqStage::low_shelf(110.0, -6.0))
            .with_post_eq_2(AudioEqStage::peaking(380.0, 0.65, 4.5))
            .with_post_eq_3(AudioEqStage::high_shelf(2_600.0, -2.8))
            .with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 3.2))
            .with_post_eq_5(AudioEqStage::peaking(560.0, 1.20, -2.4))
            .with_post_side_eq_1(AudioEqStage::peaking(450.0, 1.50, -4.0))
            .with_post_side_eq_2(AudioEqStage::peaking(2_600.0, 0.90, 0.0))
    }
}

impl Default for AudioOutputConfig {
    fn default() -> Self {
        // Flat, uncolored default: only the Legacy YM anti-alias low-pass, unity
        // EQ, no crossfeed, no side coloration. `master_gain` is set so a loud
        // multi-channel patch peaks near (but under) full scale ahead of the
        // final soft limiter, giving healthy loudness without brickwall clipping.
        // The former GHZ-fit coloration is available via `ghz_colored()`.
        Self::legacy().with_gain(1.35)
    }
}

/// Public filter description shared with the timed replay harness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioFilterSpec {
    /// No filtering beyond sample hold / averaging.
    Flat,
    /// First-order IIR low-pass: `y[n] = b0*x[n] + b1*x[n-1] - a1*y[n-1]`.
    FirstOrder { b0: f32, b1: f32, a1: f32 },
    /// Biquad IIR filter in normalized direct form I.
    Biquad {
        b0: f32,
        b1: f32,
        b2: f32,
        a1: f32,
        a2: f32,
    },
    /// Five-tap FIR for short capture/phase shaping.
    Fir { taps: [f32; 5] },
}

/// Concrete output path generated from [`AudioOutputConfig`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioOutputSpec {
    pub ym_filter: AudioFilterSpec,
    pub psg_filter: AudioFilterSpec,
    pub psg_mix: f32,
    pub master_gain: f32,
    pub ym_gain: f32,
    pub psg_gain: f32,
    pub ym_channel_side_memory_amounts: [f32; 6],
    pub ym_channel_side_transient_mixes: [f32; 6],
    pub ym_channel_side_sign_align_mixes: [f32; 6],
    pub ym_channel_side_decay_factors: [f32; 6],
    pub ym_channel_key_delay_ticks: [u64; 6],
    pub ym_channel_pan_edge_amounts: [f32; 6],
    pub ym_channel_pan_edge_decay_factors: [f32; 6],
    pub stereo_crossfeed: f32,
    pub mid_gain: f32,
    pub side_gain: f32,
    pub post_high_pass: AudioFilterSpec,
    pub post_low_pass: AudioFilterSpec,
    pub post_eq_1: AudioFilterSpec,
    pub post_eq_2: AudioFilterSpec,
    pub post_eq_3: AudioFilterSpec,
    pub post_eq_4: AudioFilterSpec,
    pub post_eq_5: AudioFilterSpec,
    pub post_side_eq_1: AudioFilterSpec,
    pub post_side_eq_2: AudioFilterSpec,
    pub post_fir: AudioFilterSpec,
    pub post_left_delay_samples: u8,
    pub post_right_delay_samples: u8,
}

fn first_order_low_pass_filter_spec(cutoff_hz: f32, sample_rate_hz: f32) -> AudioFilterSpec {
    let k = (std::f32::consts::PI * cutoff_hz / sample_rate_hz).tan();
    let b0 = k / (1.0 + k);
    let b1 = b0;
    let a1 = (k - 1.0) / (k + 1.0);
    AudioFilterSpec::FirstOrder { b0, b1, a1 }
}

fn first_order_high_pass_filter_spec(cutoff_hz: f32, sample_rate_hz: f32) -> AudioFilterSpec {
    let k = (std::f32::consts::PI * cutoff_hz / sample_rate_hz).tan();
    let b0 = 1.0 / (1.0 + k);
    let b1 = -b0;
    let a1 = (k - 1.0) / (k + 1.0);
    AudioFilterSpec::FirstOrder { b0, b1, a1 }
}

/// Smooth soft-clip / saturator applied as the final output-stage limiter.
///
/// The signal passes through unchanged (linear) below `KNEE`; above the knee the
/// excess is compressed with a `tanh` curve that is asymptotically bounded to
/// `(-1.0, 1.0)`. The slope is continuous at the knee (`tanh'(0) == 1`), so
/// transients round off gracefully instead of squaring off into the odd-harmonic
/// buzz a brickwall `clamp(-1.0, 1.0)` produces. Stays `f32` throughout.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    const RANGE: f32 = 1.0 - KNEE;
    let magnitude = x.abs();
    if magnitude <= KNEE {
        x
    } else {
        let over = magnitude - KNEE;
        let compressed = KNEE + RANGE * (over / RANGE).tanh();
        x.signum() * compressed
    }
}

fn apply_stereo_crossfeed(left: f32, right: f32, amount: f32) -> (f32, f32) {
    let amount = amount.clamp(0.0, 0.5);
    let keep = 1.0 - amount;
    (left * keep + right * amount, right * keep + left * amount)
}

fn encode_mid_side(left: f32, right: f32) -> (f32, f32) {
    ((left + right) * 0.5, (left - right) * 0.5)
}

fn decode_mid_side(mid: f32, side: f32) -> (f32, f32) {
    (mid + side, mid - side)
}

fn pan_edge_decay_factor(decay_ms: f32, ym_native_rate_hz: f32) -> f32 {
    if decay_ms <= 0.0 {
        return 0.0;
    }

    let decay_samples = (ym_native_rate_hz * decay_ms / 1000.0).max(1.0);
    (-1.0 / decay_samples).exp()
}

fn side_memory_decay_factor(decay_ms: f32, ym_native_rate_hz: f32) -> f32 {
    if decay_ms <= 0.0 {
        return 0.0;
    }

    let decay_samples = (ym_native_rate_hz * decay_ms / 1000.0).max(1.0);
    (-1.0 / decay_samples).exp()
}

fn side_memory_feed(side: f32, previous_side: f32, transient_mix: f32) -> f32 {
    let transient_mix = transient_mix.clamp(0.0, 1.0);
    let transient_side = side - previous_side;
    side * (1.0 - transient_mix) + transient_side * transient_mix
}

fn align_side_polarity(side: f32, delayed_side: f32, sign_align_mix: f32) -> f32 {
    let sign_align_mix = sign_align_mix.clamp(0.0, 1.0);
    let aligned_delayed_side = if side == 0.0 {
        0.0
    } else {
        side.signum() * delayed_side.abs()
    };
    delayed_side * (1.0 - sign_align_mix) + aligned_delayed_side * sign_align_mix
}

fn key_delay_ticks(delay_ms: f32) -> u64 {
    if delay_ms <= 0.0 {
        return 0;
    }

    ((delay_ms / 1000.0) * MASTER_CLOCK_NTSC as f32).round() as u64
}

fn ym_pan_channel_from_addr(port: u8, addr: u8) -> Option<usize> {
    if !(0xB4..=0xB6).contains(&addr) || port > 1 {
        return None;
    }

    let channel = usize::from(addr & 0x03) + usize::from(port) * 3;
    (channel < 6).then_some(channel)
}

fn ym_key_channel_from_write(port: u8, addr: u8, value: u8) -> Option<usize> {
    if port != 0 || addr != 0x28 {
        return None;
    }

    match value & 0x07 {
        0..=2 => Some((value & 0x07) as usize),
        4..=6 => Some(((value & 0x07) - 4 + 3) as usize),
        _ => None,
    }
}

fn insert_timed_write_sorted(queue: &mut Vec<TimedYm2612Write>, write: TimedYm2612Write) {
    let insert_at = queue.partition_point(|pending| pending.master_tick <= write.master_tick);
    queue.insert(insert_at, write);
}

fn maybe_delay_ym_key_write(
    queue: &mut Vec<TimedYm2612Write>,
    key_delay_ticks: [u64; 6],
    write: TimedYm2612Write,
) -> bool {
    let Some(channel) = ym_key_channel_from_write(write.port, write.addr, write.value) else {
        return false;
    };
    let delay_ticks = key_delay_ticks[channel];
    if delay_ticks == 0 {
        return false;
    }

    insert_timed_write_sorted(
        queue,
        TimedYm2612Write {
            master_tick: write.master_tick.saturating_add(delay_ticks),
            ..write
        },
    );
    true
}

fn trigger_ym_channel_pan_edge_persistence(
    side_memory: &[f32; 6],
    pan_edge_carry: &mut [f32; 6],
    pan_masks: &mut [u8; 6],
    amounts: [f32; 6],
    port: u8,
    addr: u8,
    value: u8,
) {
    let Some(channel) = ym_pan_channel_from_addr(port, addr) else {
        return;
    };

    let next_mask = value & 0xC0;
    if next_mask != 0xC0 {
        pan_edge_carry[channel] += side_memory[channel] * amounts[channel];
    }
    pan_masks[channel] = next_mask;
}

fn mix_ym_channel_outputs_with_side_memory(
    channel_samples: [(f32, f32); 6],
    side_memory: &mut [f32; 6],
    amounts: [f32; 6],
    transient_mixes: [f32; 6],
    sign_align_mixes: [f32; 6],
    side_decay_factors: [f32; 6],
    previous_side: &mut [f32; 6],
    pan_edge_carry: &mut [f32; 6],
    pan_edge_decay_factors: [f32; 6],
) -> (f32, f32) {
    let mut left_sum = 0.0f32;
    let mut right_sum = 0.0f32;

    for (idx, &(left, right)) in channel_samples.iter().enumerate() {
        let (mid, side) = encode_mid_side(left, right);
        let persisted_side = side_memory[idx];
        let shaped_memory = align_side_polarity(side, persisted_side, sign_align_mixes[idx]);
        let shaped_side = side + shaped_memory * amounts[idx] + pan_edge_carry[idx];
        pan_edge_carry[idx] *= pan_edge_decay_factors[idx];
        side_memory[idx] = side_memory_feed(side, previous_side[idx], transient_mixes[idx])
            + persisted_side * side_decay_factors[idx];
        previous_side[idx] = side;
        let (shaped_left, shaped_right) = decode_mid_side(mid, shaped_side);
        left_sum += shaped_left;
        right_sum += shaped_right;
    }

    (left_sum, right_sum)
}

fn biquad_eq_filter_spec(stage: AudioEqStage, sample_rate_hz: f32) -> AudioFilterSpec {
    match stage.kind {
        AudioEqKind::LowShelf => {
            biquad_low_shelf_filter_spec(stage.frequency_hz, stage.gain_db, stage.q, sample_rate_hz)
        }
        AudioEqKind::Peaking => {
            biquad_peaking_filter_spec(stage.frequency_hz, stage.gain_db, stage.q, sample_rate_hz)
        }
        AudioEqKind::HighShelf => biquad_high_shelf_filter_spec(
            stage.frequency_hz,
            stage.gain_db,
            stage.q,
            sample_rate_hz,
        ),
    }
}

fn biquad_low_pass_filter_spec(cutoff_hz: f32, sample_rate_hz: f32) -> AudioFilterSpec {
    let omega = 2.0 * std::f32::consts::PI * cutoff_hz / sample_rate_hz;
    let alpha = omega.sin() * std::f32::consts::FRAC_1_SQRT_2;
    let cos_omega = omega.cos();
    let b0 = (1.0 - cos_omega) * 0.5;
    let b1 = 1.0 - cos_omega;
    let b2 = b0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_omega;
    let a2 = 1.0 - alpha;

    AudioFilterSpec::Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn biquad_peaking_filter_spec(
    frequency_hz: f32,
    gain_db: f32,
    q: f32,
    sample_rate_hz: f32,
) -> AudioFilterSpec {
    let frequency_hz = frequency_hz.clamp(10.0, sample_rate_hz * 0.5 - 10.0);
    let q = q.max(0.05);
    let a = 10.0f32.powf(gain_db / 40.0);
    let omega = 2.0 * std::f32::consts::PI * frequency_hz / sample_rate_hz;
    let alpha = omega.sin() / (2.0 * q);
    let cos_omega = omega.cos();
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cos_omega;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cos_omega;
    let a2 = 1.0 - alpha / a;
    AudioFilterSpec::Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn biquad_low_shelf_filter_spec(
    frequency_hz: f32,
    gain_db: f32,
    slope: f32,
    sample_rate_hz: f32,
) -> AudioFilterSpec {
    let frequency_hz = frequency_hz.clamp(10.0, sample_rate_hz * 0.5 - 10.0);
    let slope = slope.max(0.05);
    let a = 10.0f32.powf(gain_db / 40.0);
    let omega = 2.0 * std::f32::consts::PI * frequency_hz / sample_rate_hz;
    let sin_omega = omega.sin();
    let cos_omega = omega.cos();
    let alpha = sin_omega * (((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt()) * 0.5;
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) - (a - 1.0) * cos_omega + two_sqrt_a_alpha);
    let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_omega);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cos_omega - two_sqrt_a_alpha);
    let a0 = (a + 1.0) + (a - 1.0) * cos_omega + two_sqrt_a_alpha;
    let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_omega);
    let a2 = (a + 1.0) + (a - 1.0) * cos_omega - two_sqrt_a_alpha;
    AudioFilterSpec::Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn biquad_high_shelf_filter_spec(
    frequency_hz: f32,
    gain_db: f32,
    slope: f32,
    sample_rate_hz: f32,
) -> AudioFilterSpec {
    let frequency_hz = frequency_hz.clamp(10.0, sample_rate_hz * 0.5 - 10.0);
    let slope = slope.max(0.05);
    let a = 10.0f32.powf(gain_db / 40.0);
    let omega = 2.0 * std::f32::consts::PI * frequency_hz / sample_rate_hz;
    let sin_omega = omega.sin();
    let cos_omega = omega.cos();
    let alpha = sin_omega * (((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt()) * 0.5;
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cos_omega + two_sqrt_a_alpha);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_omega);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cos_omega - two_sqrt_a_alpha);
    let a0 = (a + 1.0) - (a - 1.0) * cos_omega + two_sqrt_a_alpha;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_omega);
    let a2 = (a + 1.0) - (a - 1.0) * cos_omega - two_sqrt_a_alpha;
    AudioFilterSpec::Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// Commands that frontends send to drive the emulator.
#[derive(Debug, Clone)]
pub enum Command {
    /// Load a ROM from raw bytes.
    LoadRom(Vec<u8>),
    /// Hard reset (as if power cycled).
    PowerCycle,
    /// Soft reset (reset button).
    Reset,
    /// Execute one CPU instruction.
    StepCpu,
    /// Execute until end of current scanline.
    StepScanline,
    /// Execute one full frame.
    StepFrame,
    /// Set controller state from raw bitmask.
    SetControllerState { port: u8, buttons: u16 },
    /// Press a single button.
    PressButton { port: u8, button: Button },
    /// Release a single button.
    ReleaseButton { port: u8, button: Button },
    /// Set emulation speed in permille (1000 = normal).
    SetSpeed(u16),
    /// Set audio output sample rate in Hz (e.g. 44100, 48000).
    SetAudioSampleRate(u32),
    /// Set post-mix analog/output profile and master gain.
    SetAudioOutputConfig(AudioOutputConfig),
    /// Pause emulation.
    Pause,
    /// Resume emulation.
    Resume,
    /// Rewind emulation by the given number of frames (time-travel).
    Rewind { frames: u32 },
    /// Step exactly one frame backward (equivalent to `Rewind { frames: 1 }`).
    StepBack,
    /// Reconfigure the rewind timeline (history window, keyframe policy, enable).
    SetRewindConfig(rewind::RewindConfig),
}

/// Queries for reading emulator state without mutation.
#[derive(Debug, Clone)]
pub enum CoreQuery {
    /// Overall emulator state.
    EmulatorState,
    /// CPU register values.
    Registers,
    /// Read memory at address for given length.
    Memory { addr: u32, len: u16 },
    /// VDP register values.
    VdpRegisters,
    /// Current FPS in millihertz.
    FpsMilli,
    /// Frame counter.
    FrameCounter,
    /// Rewind buffer status (frames available, memory used, window bounds).
    RewindStatus,
}

/// A complete, serializable snapshot of the deterministic emulation state.
///
/// This captures everything required to reproduce bit-identical subsequent
/// emulation: the CPU/VDP/scheduler/Z80 register state, the large RAM/VRAM
/// buffers, the sound-chip state, and the controller ports. It intentionally
/// EXCLUDES:
///
/// * `rom` / `rom_header` — immutable for the lifetime of a loaded cartridge,
///   so it lives only in the running core and is never touched by `restore`.
/// * the RGBA framebuffer — re-derived by rendering subsequent scanlines.
/// * the ~60 audio-DSP resampler/filter fields and debug trace buffers — these
///   do not affect CPU/VDP/frame determinism and are rebuilt on `restore`.
///
/// Used both for save states and for the time-travel rewind timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenesisCoreSnapshot {
    /// 68000 CPU register state.
    pub cpu: crate::cpu::CpuSnapshot,
    /// Full VDP state (minus framebuffer).
    pub vdp: crate::vdp::VdpSnapshot,
    /// Cycle scheduler counters.
    pub scheduler: crate::scheduler::SchedulerSnapshot,
    /// Z80 CPU register state.
    pub z80: z80::Z80Snapshot,
    /// 64KB work RAM (stored as bytes for native serde support).
    pub work_ram: Vec<u8>,
    /// 8KB Z80 RAM.
    pub z80_ram: Vec<u8>,
    /// 68K ROM bank register.
    pub z80_bank: u32,
    /// 68K has requested the Z80 bus.
    pub z80_bus_requested: bool,
    /// Z80 in reset state.
    pub z80_reset: bool,
    /// Z80 reset transition pending.
    pub z80_reset_pending: bool,
    /// Z80 bus released at some point during the current scanline.
    pub z80_bus_released_this_scanline: bool,
    /// SN76489 PSG state.
    pub psg: psg::Psg,
    /// YM2612 FM synthesis state.
    pub ym2612: ym2612::Ym2612,
    /// Controller port 1.
    pub port1: ControllerPort,
    /// Controller port 2.
    pub port2: ControllerPort,
    /// Frame counter.
    pub frame_count: u64,
    /// Emulation speed in permille.
    pub speed_permille: u16,
    /// Whether emulation is paused.
    pub paused: bool,
}

/// A YM2612 register write observed on the live machine timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedYm2612Write {
    /// Absolute NTSC master-clock tick when the write reached the YM bus.
    pub master_tick: u64,
    /// Frame index when the write occurred.
    pub frame: u64,
    /// Scanline within the frame when the write occurred.
    pub scanline: u16,
    /// YM2612 port number: 0 for channels 1-3/global, 1 for channels 4-6.
    pub port: u8,
    /// YM2612 register address latched for the write.
    pub addr: u8,
    /// Register value written on the data port.
    pub value: u8,
}

/// A PSG register write observed on the live machine timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedPsgWrite {
    /// Absolute NTSC master-clock tick when the write reached the PSG bus.
    pub master_tick: u64,
    /// Frame index when the write occurred.
    pub frame: u64,
    /// Scanline within the frame when the write occurred.
    pub scanline: u16,
    /// PSG data byte written.
    pub value: u8,
}

#[derive(Debug, Clone, Copy)]
struct FirstOrderLowPassFilter {
    b0: f32,
    b1: f32,
    a1: f32,
    prev_sample: f32,
    prev_output: f32,
}

impl FirstOrderLowPassFilter {
    const fn new(b0: f32, b1: f32, a1: f32) -> Self {
        Self {
            b0,
            b1,
            a1,
            prev_sample: 0.0,
            prev_output: 0.0,
        }
    }

    fn reset(&mut self) {
        self.prev_sample = 0.0;
        self.prev_output = 0.0;
    }

    fn filter(&mut self, sample: f32) -> f32 {
        let output = self.b0 * sample + self.b1 * self.prev_sample - self.a1 * self.prev_output;
        self.prev_sample = sample;
        self.prev_output = output;
        output
    }

    const fn last_output(&self) -> f32 {
        self.prev_output
    }
}

#[derive(Debug, Clone, Copy)]
struct BiquadFilter {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    prev_sample_1: f32,
    prev_sample_2: f32,
    prev_output_1: f32,
    prev_output_2: f32,
}

impl BiquadFilter {
    const fn new(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            prev_sample_1: 0.0,
            prev_sample_2: 0.0,
            prev_output_1: 0.0,
            prev_output_2: 0.0,
        }
    }

    fn reset(&mut self) {
        self.prev_sample_1 = 0.0;
        self.prev_sample_2 = 0.0;
        self.prev_output_1 = 0.0;
        self.prev_output_2 = 0.0;
    }

    fn filter(&mut self, sample: f32) -> f32 {
        let output = self.b0 * sample + self.b1 * self.prev_sample_1 + self.b2 * self.prev_sample_2
            - self.a1 * self.prev_output_1
            - self.a2 * self.prev_output_2;
        self.prev_sample_2 = self.prev_sample_1;
        self.prev_sample_1 = sample;
        self.prev_output_2 = self.prev_output_1;
        self.prev_output_1 = output;
        output
    }

    const fn last_output(&self) -> f32 {
        self.prev_output_1
    }
}

#[derive(Debug, Clone, Copy)]
struct FirFilter {
    taps: [f32; 5],
    history: [f32; 5],
    last_output: f32,
}

impl FirFilter {
    const fn new(taps: [f32; 5]) -> Self {
        Self {
            taps,
            history: [0.0; 5],
            last_output: 0.0,
        }
    }

    fn reset(&mut self) {
        self.history = [0.0; 5];
        self.last_output = 0.0;
    }

    fn filter(&mut self, sample: f32) -> f32 {
        self.history.copy_within(0..4, 1);
        self.history[0] = sample;
        let output = self
            .taps
            .iter()
            .zip(self.history.iter())
            .map(|(tap, sample)| tap * sample)
            .sum();
        self.last_output = output;
        output
    }

    const fn last_output(&self) -> f32 {
        self.last_output
    }
}

#[derive(Debug, Clone, Copy)]
struct SampleDelay {
    delay_samples: usize,
    history: [f32; MAX_POST_DELAY_SAMPLES],
    last_output: f32,
}

impl SampleDelay {
    const fn new(delay_samples: u8) -> Self {
        let delay_samples = if (delay_samples as usize) > MAX_POST_DELAY_SAMPLES {
            MAX_POST_DELAY_SAMPLES
        } else {
            delay_samples as usize
        };
        Self {
            delay_samples,
            history: [0.0; MAX_POST_DELAY_SAMPLES],
            last_output: 0.0,
        }
    }

    fn reset(&mut self) {
        self.history = [0.0; MAX_POST_DELAY_SAMPLES];
        self.last_output = 0.0;
    }

    fn filter(&mut self, sample: f32) -> f32 {
        if self.delay_samples == 0 {
            self.last_output = sample;
            return sample;
        }

        let output = self.history[self.delay_samples - 1];
        if self.delay_samples > 1 {
            self.history.copy_within(0..self.delay_samples - 1, 1);
        }
        self.history[0] = sample;
        self.last_output = output;
        output
    }
}

#[derive(Debug, Clone, Copy)]
enum AudioFilterState {
    Flat { last_output: f32 },
    FirstOrder(FirstOrderLowPassFilter),
    Biquad(BiquadFilter),
    Fir(FirFilter),
}

impl AudioFilterState {
    const fn flat() -> Self {
        Self::Flat { last_output: 0.0 }
    }

    fn from_spec(spec: AudioFilterSpec) -> Self {
        match spec {
            AudioFilterSpec::Flat => Self::flat(),
            AudioFilterSpec::FirstOrder { b0, b1, a1 } => {
                Self::FirstOrder(FirstOrderLowPassFilter::new(b0, b1, a1))
            }
            AudioFilterSpec::Biquad { b0, b1, b2, a1, a2 } => {
                Self::Biquad(BiquadFilter::new(b0, b1, b2, a1, a2))
            }
            AudioFilterSpec::Fir { taps } => Self::Fir(FirFilter::new(taps)),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Flat { last_output } => *last_output = 0.0,
            Self::FirstOrder(filter) => filter.reset(),
            Self::Biquad(filter) => filter.reset(),
            Self::Fir(filter) => filter.reset(),
        }
    }

    fn filter(&mut self, sample: f32) -> f32 {
        match self {
            Self::Flat { last_output } => {
                *last_output = sample;
                sample
            }
            Self::FirstOrder(filter) => filter.filter(sample),
            Self::Biquad(filter) => filter.filter(sample),
            Self::Fir(filter) => filter.filter(sample),
        }
    }

    const fn last_output(&self) -> f32 {
        match self {
            Self::Flat { last_output } => *last_output,
            Self::FirstOrder(filter) => filter.last_output(),
            Self::Biquad(filter) => filter.last_output(),
            Self::Fir(filter) => filter.last_output(),
        }
    }
}

/// The Genesis emulator core.
pub struct GenesisCore {
    /// Motorola 68000 CPU.
    cpu: Cpu,
    /// Video Display Processor.
    vdp: Vdp,
    /// Cycle scheduler.
    scheduler: Scheduler,
    /// Controller port 1.
    port1: ControllerPort,
    /// Controller port 2.
    port2: ControllerPort,
    /// Cartridge ROM data.
    rom: Vec<u8>,
    /// Parsed ROM header (if loaded).
    rom_header: Option<RomHeader>,
    /// 64KB work RAM.
    work_ram: Box<[u8; 0x10000]>,
    /// Z80 CPU.
    z80: z80::Z80,
    /// 8KB Z80 RAM.
    z80_ram: Box<[u8; 0x2000]>,
    /// 68K ROM bank register (9 bits, shifted left 15).
    z80_bank: u32,
    /// 68K has requested Z80 bus.
    z80_bus_requested: bool,
    /// Z80 in reset state.
    z80_reset: bool,
    /// Set when Z80 reset transitions true→false, cleared after Z80.reset() is called.
    z80_reset_pending: bool,
    /// True if the 68K released the Z80 bus at any point during this scanline.
    /// Used to give the Z80 cycles even when the bus is re-requested by scanline end
    /// (common during tight bus polling loops in the SMPS sound driver handshake).
    z80_bus_released_this_scanline: bool,
    /// SN76489 PSG sound chip.
    psg: psg::Psg,
    /// YM2612 FM synthesis chip.
    ym2612: ym2612::Ym2612,
    /// Audio-path PSG state used for timed replay across scanlines.
    audio_psg: psg::Psg,
    /// Audio-path YM2612 state used for timed replay across scanlines.
    audio_ym2612: ym2612::Ym2612,
    /// Accumulated audio samples for the current frame (stereo interleaved f32).
    audio_buffer: Vec<f32>,
    /// Fractional audio sample accumulator for sub-scanline sample timing.
    audio_sample_phase: f64,
    /// Output audio sample rate in Hz (default 44100).
    audio_sample_rate: f64,
    /// YM2612 native sample phase accumulator for proper FM clocking.
    ym_sample_phase: f64,
    /// PSG native sample phase accumulator for proper PSG clocking.
    psg_sample_phase: f64,
    /// Post-mix analog/output settings shared with the timed replay harness.
    audio_output_config: AudioOutputConfig,
    /// YM output filters at the YM native rate.
    ym_filter_left: AudioFilterState,
    ym_filter_right: AudioFilterState,
    /// PSG output filter at the PSG native rate.
    psg_filter: AudioFilterState,
    /// Optional post-mix high-pass stage for left channel.
    post_high_pass_left: AudioFilterState,
    /// Optional post-mix high-pass stage for right channel.
    post_high_pass_right: AudioFilterState,
    /// Optional post-mix low-pass stage for left channel.
    post_low_pass_left: AudioFilterState,
    /// Optional post-mix low-pass stage for right channel.
    post_low_pass_right: AudioFilterState,
    /// Optional first post-mix EQ stage for left channel.
    post_eq_1_left: AudioFilterState,
    /// Optional first post-mix EQ stage for right channel.
    post_eq_1_right: AudioFilterState,
    /// Optional second post-mix EQ stage for left channel.
    post_eq_2_left: AudioFilterState,
    /// Optional second post-mix EQ stage for right channel.
    post_eq_2_right: AudioFilterState,
    /// Optional third post-mix EQ stage for left channel.
    post_eq_3_left: AudioFilterState,
    /// Optional third post-mix EQ stage for right channel.
    post_eq_3_right: AudioFilterState,
    /// Optional fourth post-mix EQ stage for left channel.
    post_eq_4_left: AudioFilterState,
    /// Optional fourth post-mix EQ stage for right channel.
    post_eq_4_right: AudioFilterState,
    /// Optional fifth post-mix EQ stage for left channel.
    post_eq_5_left: AudioFilterState,
    /// Optional fifth post-mix EQ stage for right channel.
    post_eq_5_right: AudioFilterState,
    /// Optional first side-only EQ stage in mid/side space.
    post_side_eq_1: AudioFilterState,
    /// Optional second side-only EQ stage in mid/side space.
    post_side_eq_2: AudioFilterState,
    /// Optional short post-mix FIR stage for left channel.
    post_fir_left: AudioFilterState,
    /// Optional short post-mix FIR stage for right channel.
    post_fir_right: AudioFilterState,
    /// Optional post-mix sample delay for the left channel.
    post_delay_left: SampleDelay,
    /// Optional post-mix sample delay for the right channel.
    post_delay_right: SampleDelay,
    /// PSG mono mix contribution relative to YM.
    audio_psg_mix: f32,
    /// YM path gain applied after YM filtering, before final mix.
    audio_ym_gain: f32,
    /// PSG path gain applied after PSG filtering and baseline PSG mix.
    audio_psg_gain: f32,
    /// Per-YM-channel delayed side-memory amount applied before YM summing.
    audio_ym_channel_side_memory_amounts: [f32; 6],
    /// Per-YM-channel blend between sustained-side and transient-side memory feed.
    audio_ym_channel_side_transient_mixes: [f32; 6],
    /// Per-YM-channel blend between delayed polarity replay and current-polarity side lift.
    audio_ym_channel_side_sign_align_mixes: [f32; 6],
    /// Per-YM-channel side-memory decay coefficient at YM native rate.
    audio_ym_channel_side_decay_factors: [f32; 6],
    /// Per-YM-channel delay applied to YM key writes on the audio path.
    audio_ym_channel_key_delay_ticks: [u64; 6],
    /// Per-YM-channel pan-edge persistence impulse.
    audio_ym_channel_pan_edge_amounts: [f32; 6],
    /// Per-YM-channel pan-edge persistence decay coefficient at YM native rate.
    audio_ym_channel_pan_edge_decay_factors: [f32; 6],
    /// Stereo crossfeed amount after post-mix shaping, before master gain.
    audio_stereo_crossfeed: f32,
    /// Mid gain after crossfeed.
    audio_mid_gain: f32,
    /// Side gain after crossfeed.
    audio_side_gain: f32,
    /// Post-mix master gain before clamping.
    audio_master_gain: f32,
    /// Global wall-clock tick that audio has been synthesized through.
    audio_master_tick: u64,
    /// Total output samples emitted since the audio pipeline was reset.
    audio_output_sample_count: u64,
    /// Next output-sample boundary in master-clock ticks.
    next_audio_output_tick: u64,
    /// Next YM2612 native sample tick in master-clock ticks.
    next_ym_tick: u64,
    /// Next PSG native sample tick in master-clock ticks.
    next_psg_tick: u64,
    /// Partial YM accumulation for the current output-sample window.
    ym_window_left_acc: f64,
    ym_window_right_acc: f64,
    ym_window_count: u32,
    ym_channel_side_memory: [f32; 6],
    ym_channel_previous_side: [f32; 6],
    ym_channel_pan_edge_carry: [f32; 6],
    ym_channel_pan_masks: [u8; 6],
    audio_pending_ym_key_writes: Vec<TimedYm2612Write>,
    /// Partial PSG accumulation for the current output-sample window.
    psg_window_acc: f64,
    psg_window_count: u32,
    /// Debug: trace 68K writes to Z80 sound command byte (RAM[$1FFF]).
    z80_cmd_trace: Vec<(u64, u8)>,
    /// Debug: count 68K writes to Z80 driver area (RAM[0x0000-0x00FF]).
    z80_driver_write_count: u32,
    /// Debug: last frame a 68K write touched the Z80 driver area.
    z80_driver_last_write_frame: u64,
    /// Debug: YM2612 writes with absolute master-clock timestamps.
    ym2612_timed_write_trace: Vec<TimedYm2612Write>,
    /// Debug: PSG writes with absolute master-clock timestamps.
    psg_timed_write_trace: Vec<TimedPsgWrite>,
    /// Frame counter.
    frame_count: u64,
    /// Emulation speed in permille.
    speed_permille: u16,
    /// Whether emulation is paused.
    paused: bool,
    /// Time-travel rewind buffer (anchor + delta compressed timeline).
    rewind: rewind::RewindBuffer,
}

impl GenesisCore {
    fn output_tick_for_sample(&self, sample_index: u64) -> u64 {
        let rate = self.audio_sample_rate.round().max(1.0) as u64;
        ((u128::from(sample_index) * u128::from(MASTER_CLOCK_NTSC) + u128::from(rate / 2))
            / u128::from(rate)) as u64
    }

    fn rebuild_audio_output_pipeline(&mut self) {
        let ym_native_rate_hz = MASTER_CLOCK_NTSC as f32 / YM_AUDIO_TICKS as f32;
        let spec = self
            .audio_output_config
            .spec_for_rates(ym_native_rate_hz, self.audio_sample_rate as f32);
        self.ym_filter_left = AudioFilterState::from_spec(spec.ym_filter);
        self.ym_filter_right = AudioFilterState::from_spec(spec.ym_filter);
        self.psg_filter = AudioFilterState::from_spec(spec.psg_filter);
        self.post_high_pass_left = AudioFilterState::from_spec(spec.post_high_pass);
        self.post_high_pass_right = AudioFilterState::from_spec(spec.post_high_pass);
        self.post_low_pass_left = AudioFilterState::from_spec(spec.post_low_pass);
        self.post_low_pass_right = AudioFilterState::from_spec(spec.post_low_pass);
        self.post_eq_1_left = AudioFilterState::from_spec(spec.post_eq_1);
        self.post_eq_1_right = AudioFilterState::from_spec(spec.post_eq_1);
        self.post_eq_2_left = AudioFilterState::from_spec(spec.post_eq_2);
        self.post_eq_2_right = AudioFilterState::from_spec(spec.post_eq_2);
        self.post_eq_3_left = AudioFilterState::from_spec(spec.post_eq_3);
        self.post_eq_3_right = AudioFilterState::from_spec(spec.post_eq_3);
        self.post_eq_4_left = AudioFilterState::from_spec(spec.post_eq_4);
        self.post_eq_4_right = AudioFilterState::from_spec(spec.post_eq_4);
        self.post_eq_5_left = AudioFilterState::from_spec(spec.post_eq_5);
        self.post_eq_5_right = AudioFilterState::from_spec(spec.post_eq_5);
        self.post_side_eq_1 = AudioFilterState::from_spec(spec.post_side_eq_1);
        self.post_side_eq_2 = AudioFilterState::from_spec(spec.post_side_eq_2);
        self.post_fir_left = AudioFilterState::from_spec(spec.post_fir);
        self.post_fir_right = AudioFilterState::from_spec(spec.post_fir);
        self.post_delay_left = SampleDelay::new(spec.post_left_delay_samples);
        self.post_delay_right = SampleDelay::new(spec.post_right_delay_samples);
        self.audio_psg_mix = spec.psg_mix;
        self.audio_ym_gain = spec.ym_gain;
        self.audio_psg_gain = spec.psg_gain;
        self.audio_ym_channel_side_memory_amounts = spec.ym_channel_side_memory_amounts;
        self.audio_ym_channel_side_transient_mixes = spec.ym_channel_side_transient_mixes;
        self.audio_ym_channel_side_sign_align_mixes = spec.ym_channel_side_sign_align_mixes;
        self.audio_ym_channel_side_decay_factors = spec.ym_channel_side_decay_factors;
        self.audio_ym_channel_key_delay_ticks = spec.ym_channel_key_delay_ticks;
        self.audio_ym_channel_pan_edge_amounts = spec.ym_channel_pan_edge_amounts;
        self.audio_ym_channel_pan_edge_decay_factors = spec.ym_channel_pan_edge_decay_factors;
        self.audio_stereo_crossfeed = spec.stereo_crossfeed;
        self.audio_mid_gain = spec.mid_gain;
        self.audio_side_gain = spec.side_gain;
        self.audio_master_gain = spec.master_gain;
    }

    fn reset_audio_resampler_state(&mut self) {
        self.audio_buffer.clear();
        self.audio_sample_phase = 0.0;
        self.ym_sample_phase = 0.0;
        self.psg_sample_phase = 0.0;
        self.rebuild_audio_output_pipeline();
        self.ym_filter_left.reset();
        self.ym_filter_right.reset();
        self.psg_filter.reset();
        self.post_high_pass_left.reset();
        self.post_high_pass_right.reset();
        self.post_low_pass_left.reset();
        self.post_low_pass_right.reset();
        self.post_eq_1_left.reset();
        self.post_eq_1_right.reset();
        self.post_eq_2_left.reset();
        self.post_eq_2_right.reset();
        self.post_eq_3_left.reset();
        self.post_eq_3_right.reset();
        self.post_eq_4_left.reset();
        self.post_eq_4_right.reset();
        self.post_eq_5_left.reset();
        self.post_eq_5_right.reset();
        self.post_side_eq_1.reset();
        self.post_side_eq_2.reset();
        self.post_fir_left.reset();
        self.post_fir_right.reset();
        self.post_delay_left.reset();
        self.post_delay_right.reset();
        self.audio_master_tick = 0;
        self.audio_output_sample_count = 0;
        self.next_audio_output_tick = self.output_tick_for_sample(1);
        self.next_ym_tick = YM_AUDIO_TICKS;
        self.next_psg_tick = PSG_AUDIO_TICKS;
        self.audio_ym2612 = self.ym2612.clone();
        self.audio_psg = self.psg.clone();
        self.ym_window_left_acc = 0.0;
        self.ym_window_right_acc = 0.0;
        self.ym_window_count = 0;
        self.ym_channel_side_memory = [0.0; 6];
        self.ym_channel_previous_side = [0.0; 6];
        self.ym_channel_pan_edge_carry = [0.0; 6];
        self.ym_channel_pan_masks = [0xC0; 6];
        self.audio_pending_ym_key_writes.clear();
        self.psg_window_acc = 0.0;
        self.psg_window_count = 0;
    }

    /// Creates a new Genesis core with no ROM loaded.
    #[must_use]
    pub fn new() -> Self {
        let mut core = Self {
            cpu: Cpu::new(),
            vdp: Vdp::new(),
            scheduler: Scheduler::new(),
            port1: ControllerPort::new(),
            port2: ControllerPort::new(),
            rom: Vec::new(),
            rom_header: None,
            work_ram: Box::new([0; 0x10000]),
            z80: z80::Z80::new(),
            z80_ram: Box::new([0; 0x2000]),
            z80_bank: 0,
            z80_bus_requested: false,
            z80_reset: true, // Z80 starts in reset
            z80_reset_pending: false,
            z80_bus_released_this_scanline: false,
            z80_cmd_trace: Vec::new(),
            z80_driver_write_count: 0,
            z80_driver_last_write_frame: 0,
            ym2612_timed_write_trace: Vec::new(),
            psg_timed_write_trace: Vec::new(),
            psg: psg::Psg::new(),
            ym2612: ym2612::Ym2612::new(),
            audio_psg: psg::Psg::new(),
            audio_ym2612: ym2612::Ym2612::new(),
            audio_buffer: Vec::with_capacity(1600),
            audio_sample_phase: 0.0,
            audio_sample_rate: 44100.0,
            ym_sample_phase: 0.0,
            psg_sample_phase: 0.0,
            audio_output_config: AudioOutputConfig::default(),
            ym_filter_left: AudioFilterState::flat(),
            ym_filter_right: AudioFilterState::flat(),
            psg_filter: AudioFilterState::flat(),
            post_high_pass_left: AudioFilterState::flat(),
            post_high_pass_right: AudioFilterState::flat(),
            post_low_pass_left: AudioFilterState::flat(),
            post_low_pass_right: AudioFilterState::flat(),
            post_eq_1_left: AudioFilterState::flat(),
            post_eq_1_right: AudioFilterState::flat(),
            post_eq_2_left: AudioFilterState::flat(),
            post_eq_2_right: AudioFilterState::flat(),
            post_eq_3_left: AudioFilterState::flat(),
            post_eq_3_right: AudioFilterState::flat(),
            post_eq_4_left: AudioFilterState::flat(),
            post_eq_4_right: AudioFilterState::flat(),
            post_eq_5_left: AudioFilterState::flat(),
            post_eq_5_right: AudioFilterState::flat(),
            post_side_eq_1: AudioFilterState::flat(),
            post_side_eq_2: AudioFilterState::flat(),
            post_fir_left: AudioFilterState::flat(),
            post_fir_right: AudioFilterState::flat(),
            post_delay_left: SampleDelay::new(0),
            post_delay_right: SampleDelay::new(0),
            audio_psg_mix: DEFAULT_PSG_MIX,
            audio_ym_gain: 1.0,
            audio_psg_gain: 1.0,
            audio_ym_channel_side_memory_amounts: [0.0; 6],
            audio_ym_channel_side_transient_mixes: [0.0; 6],
            audio_ym_channel_side_sign_align_mixes: [0.0; 6],
            audio_ym_channel_side_decay_factors: [0.0; 6],
            audio_ym_channel_key_delay_ticks: [0; 6],
            audio_ym_channel_pan_edge_amounts: [0.0; 6],
            audio_ym_channel_pan_edge_decay_factors: [0.0; 6],
            audio_stereo_crossfeed: 0.0,
            audio_mid_gain: 1.0,
            audio_side_gain: 1.0,
            audio_master_gain: 1.0,
            audio_master_tick: 0,
            audio_output_sample_count: 0,
            next_audio_output_tick: 0,
            next_ym_tick: YM_AUDIO_TICKS,
            next_psg_tick: PSG_AUDIO_TICKS,
            ym_window_left_acc: 0.0,
            ym_window_right_acc: 0.0,
            ym_window_count: 0,
            ym_channel_side_memory: [0.0; 6],
            ym_channel_previous_side: [0.0; 6],
            ym_channel_pan_edge_carry: [0.0; 6],
            ym_channel_pan_masks: [0xC0; 6],
            audio_pending_ym_key_writes: Vec::new(),
            psg_window_acc: 0.0,
            psg_window_count: 0,
            frame_count: 0,
            speed_permille: 1000,
            paused: false,
            rewind: rewind::RewindBuffer::new(rewind::RewindConfig::default()),
        };
        core.reset_audio_resampler_state();
        core
    }

    /// Executes a command.
    pub fn execute(&mut self, cmd: Command) {
        match cmd {
            Command::LoadRom(data) => self.load_rom(data),
            Command::PowerCycle => self.power_cycle(),
            Command::Reset => self.reset(),
            Command::StepCpu => self.step_cpu(),
            Command::StepScanline => self.step_scanline(),
            Command::StepFrame => self.step_frame(),
            Command::SetControllerState { port, buttons } => {
                self.controller_port_mut(port).set_buttons(buttons);
            }
            Command::PressButton { port, button } => {
                self.controller_port_mut(port).press(button);
            }
            Command::ReleaseButton { port, button } => {
                self.controller_port_mut(port).release(button);
            }
            Command::SetSpeed(s) => self.speed_permille = s,
            Command::SetAudioSampleRate(rate) => {
                self.audio_sample_rate = f64::from(rate);
                self.reset_audio_resampler_state();
            }
            Command::SetAudioOutputConfig(config) => {
                self.audio_output_config = config;
                self.reset_audio_resampler_state();
            }
            Command::Pause => self.paused = true,
            Command::Resume => self.paused = false,
            Command::Rewind { frames } => self.rewind_frames(frames),
            Command::StepBack => self.rewind_frames(1),
            Command::SetRewindConfig(cfg) => self.rewind.set_config(cfg),
        }
    }

    /// Rewinds by `frames`, reconstructing the older state, restoring it, and
    /// truncating the abandoned forward history so subsequent stepping records
    /// over it.
    fn rewind_frames(&mut self, frames: u32) {
        let target = self.frame_count.saturating_sub(u64::from(frames));
        if let Some(snap) = self.rewind.reconstruct(target) {
            self.restore(&snap);
            self.rewind.truncate_after(target);
        }
    }

    /// Returns a reference to the RGBA framebuffer.
    #[must_use]
    pub fn framebuffer_rgba(&self) -> &[u8] {
        self.vdp.framebuffer()
    }

    /// Returns the frame counter.
    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Returns the current NTSC master-clock tick count.
    #[must_use]
    pub fn master_ticks(&self) -> u64 {
        self.scheduler.master_ticks()
    }

    /// Returns the wall-clock tick that audio has been synthesized through.
    #[must_use]
    pub fn audio_master_ticks(&self) -> u64 {
        self.audio_master_tick
    }

    /// Returns the total number of output samples emitted by the audio pipeline.
    #[must_use]
    pub fn audio_output_sample_count(&self) -> u64 {
        self.audio_output_sample_count
    }

    /// Returns the current post-mix output profile/gain settings.
    #[must_use]
    pub fn audio_output_config(&self) -> AudioOutputConfig {
        self.audio_output_config
    }

    /// Returns the parsed ROM header, if a ROM is loaded.
    #[must_use]
    pub fn rom_header(&self) -> Option<&RomHeader> {
        self.rom_header.as_ref()
    }

    /// Returns whether emulation is paused.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Returns the CPU program counter (debug).
    #[must_use]
    pub fn cpu_pc(&self) -> u32 {
        self.cpu.pc
    }

    /// Returns the CPU supervisor stack pointer (debug).
    #[must_use]
    pub fn cpu_ssp(&self) -> u32 {
        self.cpu.ssp
    }

    /// Returns true if the 68K CPU is stopped (STOP instruction).
    #[must_use]
    pub fn cpu_stopped(&self) -> bool {
        self.cpu.stopped
    }

    /// Returns true if the 68K CPU is halted (double bus fault).
    #[must_use]
    pub fn cpu_halted(&self) -> bool {
        self.cpu.halted
    }

    /// Returns the 68K status register value (debug).
    #[must_use]
    pub fn cpu_sr(&self) -> u16 {
        self.cpu.sr.0
    }

    /// Returns the 68K interrupt priority mask (0-7, from SR bits 8-10).
    #[must_use]
    pub fn cpu_interrupt_mask(&self) -> u8 {
        self.cpu.sr.interrupt_mask()
    }

    /// Returns VDP register value (debug).
    #[must_use]
    pub fn vdp_register(&self, reg: u8) -> u8 {
        self.vdp.read_register(reg as usize)
    }

    /// Z80 debug accessors.
    #[must_use]
    pub fn z80_pc(&self) -> u16 {
        self.z80.pc
    }
    #[must_use]
    pub fn z80_cycles(&self) -> u64 {
        self.z80.cycles
    }
    #[must_use]
    pub fn z80_bus_requested(&self) -> bool {
        self.z80_bus_requested
    }
    #[must_use]
    pub fn z80_in_reset(&self) -> bool {
        self.z80_reset
    }

    /// Returns a YM2612 diagnostic snapshot for debugging.
    #[must_use]
    pub fn ym2612_diagnostic(&self) -> ym2612::Ym2612Diag {
        self.ym2612.diagnostic()
    }

    /// Returns the total number of YM2612 register writes.
    pub fn ym2612_write_count(&self) -> u32 {
        self.ym2612.write_count()
    }

    /// Returns the first N captured YM2612 register writes.
    pub fn ym2612_write_trace(&self) -> &[(u8, u8, u8)] {
        self.ym2612.write_trace()
    }

    /// Returns timed YM2612 writes captured from the live bus.
    pub fn ym2612_timed_write_trace(&self) -> &[TimedYm2612Write] {
        &self.ym2612_timed_write_trace
    }

    /// Clears the timed YM2612 write trace.
    pub fn clear_ym2612_timed_write_trace(&mut self) {
        self.ym2612_timed_write_trace.clear();
    }

    /// Returns timed PSG writes captured from the live bus.
    pub fn psg_timed_write_trace(&self) -> &[TimedPsgWrite] {
        &self.psg_timed_write_trace
    }

    /// Clears the timed PSG write trace.
    pub fn clear_psg_timed_write_trace(&mut self) {
        self.psg_timed_write_trace.clear();
    }

    /// Returns a VDP snapshot for debugging.
    #[must_use]
    pub fn vdp_snapshot(&self) -> crate::vdp::VdpSnapshot {
        self.vdp.snapshot()
    }

    /// Returns a Z80 snapshot for save states and debugging.
    #[must_use]
    pub fn z80_snapshot(&self) -> z80::Z80Snapshot {
        self.z80.snapshot()
    }

    /// Captures a complete snapshot of the deterministic emulation state.
    ///
    /// The result excludes the immutable ROM, the RGBA framebuffer, and the
    /// audio-DSP/debug scratch state (see [`GenesisCoreSnapshot`]).
    #[must_use]
    pub fn snapshot(&self) -> GenesisCoreSnapshot {
        GenesisCoreSnapshot {
            cpu: self.cpu.snapshot(),
            vdp: self.vdp.snapshot(),
            scheduler: self.scheduler.snapshot(),
            z80: self.z80.snapshot(),
            work_ram: self.work_ram.to_vec(),
            z80_ram: self.z80_ram.to_vec(),
            z80_bank: self.z80_bank,
            z80_bus_requested: self.z80_bus_requested,
            z80_reset: self.z80_reset,
            z80_reset_pending: self.z80_reset_pending,
            z80_bus_released_this_scanline: self.z80_bus_released_this_scanline,
            psg: self.psg.clone(),
            ym2612: self.ym2612.clone(),
            port1: self.port1.clone(),
            port2: self.port2.clone(),
            frame_count: self.frame_count,
            speed_permille: self.speed_permille,
            paused: self.paused,
        }
    }

    /// Restores the deterministic emulation state from a snapshot.
    ///
    /// The immutable ROM and header are left untouched. The RGBA framebuffer is
    /// re-derived by subsequent rendering. The audio-DSP resampler/filter state
    /// is rebuilt so the core is left internally consistent for audio output.
    pub fn restore(&mut self, snap: &GenesisCoreSnapshot) {
        self.cpu.restore(&snap.cpu);
        self.vdp.restore(&snap.vdp);
        self.scheduler.restore(&snap.scheduler);
        self.z80.restore(&snap.z80);
        self.work_ram.copy_from_slice(&snap.work_ram);
        self.z80_ram.copy_from_slice(&snap.z80_ram);
        self.z80_bank = snap.z80_bank;
        self.z80_bus_requested = snap.z80_bus_requested;
        self.z80_reset = snap.z80_reset;
        self.z80_reset_pending = snap.z80_reset_pending;
        self.z80_bus_released_this_scanline = snap.z80_bus_released_this_scanline;
        self.psg = snap.psg.clone();
        self.ym2612 = snap.ym2612.clone();
        self.port1 = snap.port1.clone();
        self.port2 = snap.port2.clone();
        self.frame_count = snap.frame_count;
        self.speed_permille = snap.speed_permille;
        self.paused = snap.paused;
        // Rebuild the audio resampler/filter pipeline so the (excluded) DSP
        // scratch state is consistent with the restored chip state.
        self.reset_audio_resampler_state();
    }

    /// Returns the current rewind buffer status.
    #[must_use]
    pub fn rewind_status(&self) -> rewind::RewindStatus {
        self.rewind.status()
    }

    /// Returns the number of frames currently available to rewind through.
    #[must_use]
    pub fn rewind_frames_available(&self) -> u64 {
        self.rewind.frames_available()
    }

    /// Returns the estimated memory (bytes) used by the rewind timeline.
    #[must_use]
    pub fn rewind_memory_used(&self) -> usize {
        self.rewind.memory_used()
    }

    /// Returns the Z80 RAM (8KB) for debugging.
    #[must_use]
    pub fn z80_ram(&self) -> &[u8] {
        &*self.z80_ram
    }

    /// Returns the Z80 bank register value (bits 15-23 of banked ROM offset).
    pub fn z80_bank(&self) -> u32 {
        self.z80_bank
    }

    /// Read a byte from the ROM at an absolute offset (for debugging).
    pub fn rom_byte(&self, offset: usize) -> u8 {
        self.rom.get(offset).copied().unwrap_or(0xFF)
    }

    /// Returns 68K writes to Z80 RAM[$1FFF] (sound command byte): (frame, value).
    pub fn z80_cmd_trace(&self) -> &[(u64, u8)] {
        &self.z80_cmd_trace
    }

    /// Returns debug info: (total 68K writes to Z80 driver area, last frame written).
    pub fn z80_driver_write_info(&self) -> (u32, u64) {
        (
            self.z80_driver_write_count,
            self.z80_driver_last_write_frame,
        )
    }

    // --- Internal ---

    fn controller_port_mut(&mut self, port: u8) -> &mut ControllerPort {
        if port == 0 {
            &mut self.port1
        } else {
            &mut self.port2
        }
    }

    fn load_rom(&mut self, data: Vec<u8>) {
        self.rom_header = rom::parse_header(&data).ok();
        self.rom = data;
        self.power_cycle();
    }

    fn power_cycle(&mut self) {
        self.cpu = Cpu::new();
        self.vdp = Vdp::new();
        self.scheduler = Scheduler::new();
        self.work_ram.fill(0);
        self.z80 = z80::Z80::new();
        self.z80_ram.fill(0);
        self.z80_bank = 0;
        self.z80_bus_requested = false;
        self.z80_reset = true;
        self.z80_reset_pending = false;
        self.z80_bus_released_this_scanline = false;
        self.z80_cmd_trace.clear();
        self.z80_driver_write_count = 0;
        self.z80_driver_last_write_frame = 0;
        self.ym2612_timed_write_trace.clear();
        self.psg_timed_write_trace.clear();
        self.psg = psg::Psg::new();
        self.ym2612 = ym2612::Ym2612::new();
        self.reset_audio_resampler_state();
        self.frame_count = 0;

        // A new power cycle invalidates any recorded rewind history; keep the
        // active configuration but start the timeline fresh.
        self.rewind = rewind::RewindBuffer::new(self.rewind.config.clone());

        // 68000 boot: read SSP from 0x000000, PC from 0x000004
        if self.rom.len() >= 8 {
            self.cpu.ssp = self.read_long(0x000000);
            self.cpu.pc = self.read_long(0x000004);
        }
    }

    fn reset(&mut self) {
        // Soft reset: re-read vectors, keep RAM
        if self.rom.len() >= 8 {
            self.cpu.ssp = self.read_long(0x000000);
            self.cpu.pc = self.read_long(0x000004);
        }
        self.cpu.sr = super::cpu::StatusRegister::new(super::cpu::StatusRegister::S);
        self.cpu.halted = false;
        self.cpu.stopped = false;
    }

    fn step_cpu_at(&mut self, scanline: u16, master_tick: u64) {
        if self.cpu.halted || self.cpu.stopped {
            return;
        }

        // Fetch the opcode word from the instruction stream.
        let opcode = self.read_word(self.cpu.pc);
        self.cpu.pc = self.cpu.pc.wrapping_add(2);

        // Build a bus wrapper that borrows the non-CPU fields.
        let mut bus = CoreBus {
            rom: &self.rom,
            work_ram: &mut self.work_ram,
            vdp: &mut self.vdp,
            port1: &mut self.port1,
            port2: &mut self.port2,
            z80_ram: &mut self.z80_ram,
            z80_bus_requested: &mut self.z80_bus_requested,
            z80_reset: &mut self.z80_reset,
            z80_reset_pending: &mut self.z80_reset_pending,
            z80_bus_released_this_scanline: &mut self.z80_bus_released_this_scanline,
            ym2612: &mut self.ym2612,
            psg: &mut self.psg,
            z80_cmd_trace: &mut self.z80_cmd_trace,
            z80_driver_write_count: &mut self.z80_driver_write_count,
            z80_driver_last_write_frame: &mut self.z80_driver_last_write_frame,
            ym2612_timed_write_trace: &mut self.ym2612_timed_write_trace,
            psg_timed_write_trace: &mut self.psg_timed_write_trace,
            frame_count: self.frame_count,
            scanline,
            master_tick,
        };

        let cycles = cpu::execute_instruction(&mut self.cpu, opcode, &mut bus);
        let cycles_u64 = u64::from(cycles);
        self.cpu.cycles += cycles_u64;
        self.scheduler.advance_cpu(cycles_u64);
    }

    fn step_cpu(&mut self) {
        self.step_cpu_at(self.vdp.scanline(), self.scheduler.master_ticks());
    }

    fn step_scanline(&mut self) {
        // Genesis H40: ~488 68K cycles per scanline.
        // Run instructions until we've consumed enough cycles.
        let target = self.cpu.cycles + 488;
        while self.cpu.cycles < target {
            self.step_cpu();
            // After each instruction, check if DMA is pending
            if self.vdp.dma_pending() {
                self.execute_vdp_dma();
            }
            if self.cpu.halted || self.cpu.stopped {
                break;
            }
        }
    }

    fn step_scanline_with_timing(&mut self, scanline: u16, scanline_start_tick: u64) -> u64 {
        let target = self.cpu.cycles + 488;
        let cpu_cycle_base = self.cpu.cycles;
        while self.cpu.cycles < target {
            let instruction_tick =
                scanline_start_tick + (self.cpu.cycles - cpu_cycle_base) * MASTER_PER_CPU;
            self.step_cpu_at(scanline, instruction_tick);
            if self.vdp.dma_pending() {
                self.execute_vdp_dma();
            }
            if self.cpu.halted || self.cpu.stopped {
                break;
            }
        }
        cpu_cycle_base
    }

    fn step_frame(&mut self) {
        if self.paused {
            return;
        }

        // Clear audio buffer at frame start
        self.audio_buffer.clear();

        // Clear V-blank at frame start and de-assert Z80 INT
        self.vdp.set_vblank(false);
        self.z80.int_line = false;

        for scanline in 0..SCANLINES_PER_FRAME {
            let scanline_start_tick = self.audio_master_tick;
            let ym_trace_start = self.ym2612_timed_write_trace.len();
            let psg_trace_start = self.psg_timed_write_trace.len();

            // Begin scanline timing
            self.vdp.begin_scanline(scanline);

            // Reset per-scanline bus tracking before the 68K runs.
            self.z80_bus_released_this_scanline = false;

            // Run CPU for this scanline
            let cpu_cycle_base = self.step_scanline_with_timing(scanline, scanline_start_tick);

            // Handle Z80 reset: when reset is de-asserted, restart Z80 from PC=0
            if self.z80_reset_pending {
                self.z80.reset();
                self.z80_reset_pending = false;
            }

            // Step Z80 for this scanline if it's not in reset and got bus access.
            // The Z80 runs when the bus is currently free, OR when the 68K released
            // the bus at any point during this scanline (even if re-requested by now).
            // This handles tight bus polling loops where the 68K requests/releases
            // the bus multiple times per scanline — the Z80 must get cycles in the
            // brief release windows or the SMPS sound driver handshake deadlocks.
            let z80_gets_cycles =
                !self.z80_reset && (!self.z80_bus_requested || self.z80_bus_released_this_scanline);
            if z80_gets_cycles {
                self.step_z80_scanline_with_timing(scanline, scanline_start_tick);
            }

            // Check for H-interrupt (level 4)
            if self.vdp.h_interrupt_pending() {
                self.vdp.clear_h_interrupt();
                let cpu_master_tick =
                    scanline_start_tick + (self.cpu.cycles - cpu_cycle_base) * MASTER_PER_CPU;
                let mut bus = CoreBus {
                    rom: &self.rom,
                    work_ram: &mut self.work_ram,
                    vdp: &mut self.vdp,
                    port1: &mut self.port1,
                    port2: &mut self.port2,
                    z80_ram: &mut self.z80_ram,
                    z80_bus_requested: &mut self.z80_bus_requested,
                    z80_reset: &mut self.z80_reset,
                    z80_reset_pending: &mut self.z80_reset_pending,
                    z80_bus_released_this_scanline: &mut self.z80_bus_released_this_scanline,
                    ym2612: &mut self.ym2612,
                    psg: &mut self.psg,
                    z80_cmd_trace: &mut self.z80_cmd_trace,
                    z80_driver_write_count: &mut self.z80_driver_write_count,
                    z80_driver_last_write_frame: &mut self.z80_driver_last_write_frame,
                    ym2612_timed_write_trace: &mut self.ym2612_timed_write_trace,
                    psg_timed_write_trace: &mut self.psg_timed_write_trace,
                    frame_count: self.frame_count,
                    scanline,
                    master_tick: cpu_master_tick,
                };
                let cycles = cpu::deliver_interrupt(&mut self.cpu, &mut bus, 4);
                self.cpu.cycles += u64::from(cycles);
                self.scheduler.advance_cpu(u64::from(cycles));
            }

            // Render visible scanlines
            if scanline < ACTIVE_SCANLINES {
                self.vdp.render_scanline(scanline);
            }

            // At scanline 224: enter V-blank and fire V-blank interrupt
            if scanline == ACTIVE_SCANLINES {
                self.vdp.set_vblank(true);
                // Assert Z80 INT — the Genesis directly connects this to V-blank.
                // The Z80 will service it on its next stepping opportunity when
                // IFF1 is enabled (the SMPS driver relies on this for music updates).
                self.z80.int_line = true;

                // Fire level 6 interrupt if V-interrupt is enabled (reg 1, bit 5)
                let vint_enabled = self.vdp.read_register(1) & 0x20 != 0;
                if vint_enabled {
                    let cpu_master_tick =
                        scanline_start_tick + (self.cpu.cycles - cpu_cycle_base) * MASTER_PER_CPU;
                    let mut bus = CoreBus {
                        rom: &self.rom,
                        work_ram: &mut self.work_ram,
                        vdp: &mut self.vdp,
                        port1: &mut self.port1,
                        port2: &mut self.port2,
                        z80_ram: &mut self.z80_ram,
                        z80_bus_requested: &mut self.z80_bus_requested,
                        z80_reset: &mut self.z80_reset,
                        z80_reset_pending: &mut self.z80_reset_pending,
                        z80_bus_released_this_scanline: &mut self.z80_bus_released_this_scanline,
                        ym2612: &mut self.ym2612,
                        psg: &mut self.psg,
                        z80_cmd_trace: &mut self.z80_cmd_trace,
                        z80_driver_write_count: &mut self.z80_driver_write_count,
                        z80_driver_last_write_frame: &mut self.z80_driver_last_write_frame,
                        ym2612_timed_write_trace: &mut self.ym2612_timed_write_trace,
                        psg_timed_write_trace: &mut self.psg_timed_write_trace,
                        frame_count: self.frame_count,
                        scanline,
                        master_tick: cpu_master_tick,
                    };
                    let cycles = cpu::deliver_interrupt(&mut self.cpu, &mut bus, 6);
                    self.cpu.cycles += u64::from(cycles);
                    self.scheduler.advance_cpu(u64::from(cycles));
                }
            }

            let ym_writes = self.ym2612_timed_write_trace[ym_trace_start..].to_vec();
            let psg_writes = self.psg_timed_write_trace[psg_trace_start..].to_vec();
            self.synthesize_audio_interval(
                &ym_writes,
                &psg_writes,
                scanline_start_tick,
                scanline_start_tick + MASTER_TICKS_PER_SCANLINE,
            );
        }

        self.vdp.end_frame();
        self.port1.reset_th_counter();
        self.port2.reset_th_counter();
        self.frame_count += 1;

        // Record this frame into the rewind timeline. record() is a no-op when
        // rewind is disabled; when enabled it captures a snapshot plus the input
        // that produced this frame so the state can later be rewound and the
        // input stream deterministically replayed.
        if self.rewind.config.enabled {
            let snapshot = self.snapshot();
            let input = rewind::FrameInput {
                port1_buttons: self.port1.buttons(),
                port2_buttons: self.port2.buttons(),
            };
            self.rewind.record(self.frame_count, snapshot, input);
        }
    }

    /// Returns the current frame's audio samples (stereo interleaved f32).
    #[must_use]
    pub fn audio_samples(&self) -> &[f32] {
        &self.audio_buffer
    }

    /// Clears the audio buffer (call after pushing to ring buffer).
    pub fn clear_audio_buffer(&mut self) {
        self.audio_buffer.clear();
    }

    fn step_z80_scanline_with_timing(&mut self, scanline: u16, scanline_start_tick: u64) {
        let target = self.z80.cycles + 228;
        let z80_cycle_base = self.z80.cycles;
        while self.z80.cycles < target {
            {
                let mut bus = Z80Bus {
                    z80_ram: &mut self.z80_ram,
                    rom: &self.rom,
                    z80_bank: &mut self.z80_bank,
                    ym2612: &mut self.ym2612,
                    psg: &mut self.psg,
                    ym2612_timed_write_trace: &mut self.ym2612_timed_write_trace,
                    psg_timed_write_trace: &mut self.psg_timed_write_trace,
                    frame_count: self.frame_count,
                    scanline,
                    master_tick: scanline_start_tick
                        + (self.z80.cycles - z80_cycle_base) * MASTER_PER_Z80,
                };
                let int_cycles = z80::execute::accept_interrupt(&mut self.z80, &mut bus);
                if int_cycles > 0 {
                    self.z80.cycles += u64::from(int_cycles);
                    continue;
                }
            }

            if self.z80.halted {
                self.z80.cycles = target;
                break;
            }

            let mut bus = Z80Bus {
                z80_ram: &mut self.z80_ram,
                rom: &self.rom,
                z80_bank: &mut self.z80_bank,
                ym2612: &mut self.ym2612,
                psg: &mut self.psg,
                ym2612_timed_write_trace: &mut self.ym2612_timed_write_trace,
                psg_timed_write_trace: &mut self.psg_timed_write_trace,
                frame_count: self.frame_count,
                scanline,
                master_tick: scanline_start_tick
                    + (self.z80.cycles - z80_cycle_base) * MASTER_PER_Z80,
            };
            let cycles = z80::execute_instruction(&mut self.z80, &mut bus);
            self.z80.cycles += u64::from(cycles);
        }
    }

    fn clock_ym_audio_sample(&mut self, ym: &mut ym2612::Ym2612) {
        let channel_samples = ym.output_sample_per_channel();
        let (left, right) = mix_ym_channel_outputs_with_side_memory(
            channel_samples,
            &mut self.ym_channel_side_memory,
            self.audio_ym_channel_side_memory_amounts,
            self.audio_ym_channel_side_transient_mixes,
            self.audio_ym_channel_side_sign_align_mixes,
            self.audio_ym_channel_side_decay_factors,
            &mut self.ym_channel_previous_side,
            &mut self.ym_channel_pan_edge_carry,
            self.audio_ym_channel_pan_edge_decay_factors,
        );
        let filtered_left = self.ym_filter_left.filter(left);
        let filtered_right = self.ym_filter_right.filter(right);

        self.ym_window_left_acc += f64::from(filtered_left);
        self.ym_window_right_acc += f64::from(filtered_right);
        self.ym_window_count += 1;
        self.next_ym_tick += YM_AUDIO_TICKS;
    }

    fn clock_psg_audio_sample(&mut self, psg: &mut psg::Psg) {
        psg.clock_tick();
        self.psg_window_acc += f64::from(self.psg_filter.filter(psg.sample()));
        self.psg_window_count += 1;
        self.next_psg_tick += PSG_AUDIO_TICKS;
    }

    fn push_audio_output_sample(&mut self, _psg: &psg::Psg) {
        let filtered_left = if self.ym_window_count > 0 {
            (self.ym_window_left_acc / self.ym_window_count as f64) as f32
        } else {
            self.ym_filter_left.last_output()
        };
        let filtered_right = if self.ym_window_count > 0 {
            (self.ym_window_right_acc / self.ym_window_count as f64) as f32
        } else {
            self.ym_filter_right.last_output()
        };
        let psg_out = if self.psg_window_count > 0 {
            (self.psg_window_acc / self.psg_window_count as f64) as f32
        } else {
            self.psg_filter.last_output()
        };

        let ym_left = filtered_left * self.audio_ym_gain;
        let ym_right = filtered_right * self.audio_ym_gain;
        let psg_mixed = psg_out * self.audio_psg_mix * self.audio_psg_gain;
        let mixed_left = ym_left + psg_mixed;
        let mixed_right = ym_right + psg_mixed;
        let shaped_left = self.post_eq_5_left.filter(
            self.post_eq_4_left.filter(
                self.post_eq_3_left.filter(
                    self.post_eq_2_left.filter(
                        self.post_eq_1_left.filter(
                            self.post_low_pass_left
                                .filter(self.post_high_pass_left.filter(mixed_left)),
                        ),
                    ),
                ),
            ),
        );
        let shaped_right = self.post_eq_5_right.filter(
            self.post_eq_4_right.filter(
                self.post_eq_3_right.filter(
                    self.post_eq_2_right.filter(
                        self.post_eq_1_right.filter(
                            self.post_low_pass_right
                                .filter(self.post_high_pass_right.filter(mixed_right)),
                        ),
                    ),
                ),
            ),
        );
        let (crossfed_left, crossfed_right) =
            apply_stereo_crossfeed(shaped_left, shaped_right, self.audio_stereo_crossfeed);
        let (mid, side) = encode_mid_side(crossfed_left, crossfed_right);
        let shaped_mid = mid * self.audio_mid_gain;
        let shaped_side = self
            .post_side_eq_2
            .filter(self.post_side_eq_1.filter(side * self.audio_side_gain));
        let (ms_left, ms_right) = decode_mid_side(shaped_mid, shaped_side);
        let fir_left = self.post_fir_left.filter(ms_left);
        let fir_right = self.post_fir_right.filter(ms_right);
        let delayed_left = self.post_delay_left.filter(fir_left);
        let delayed_right = self.post_delay_right.filter(fir_right);
        let left = delayed_left * self.audio_master_gain;
        let right = delayed_right * self.audio_master_gain;
        self.audio_buffer.push(soft_limit(left));
        self.audio_buffer.push(soft_limit(right));

        self.ym_window_left_acc = 0.0;
        self.ym_window_right_acc = 0.0;
        self.ym_window_count = 0;
        self.psg_window_acc = 0.0;
        self.psg_window_count = 0;
        self.audio_output_sample_count += 1;
        self.next_audio_output_tick =
            self.output_tick_for_sample(self.audio_output_sample_count + 1);
    }

    fn synthesize_audio_interval(
        &mut self,
        ym_writes: &[TimedYm2612Write],
        psg_writes: &[TimedPsgWrite],
        start_tick: u64,
        end_tick: u64,
    ) {
        debug_assert_eq!(self.audio_master_tick, start_tick);

        let mut ym = self.audio_ym2612.clone();
        let mut psg = self.audio_psg.clone();
        let mut ym_idx = 0usize;
        let mut psg_idx = 0usize;

        loop {
            let next_write_is_ym = match (ym_writes.get(ym_idx), psg_writes.get(psg_idx)) {
                (Some(ym), Some(psg)) => ym.master_tick <= psg.master_tick,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true,
            };
            let next_write_tick = if next_write_is_ym {
                ym_writes.get(ym_idx).map(|write| write.master_tick)
            } else {
                psg_writes.get(psg_idx).map(|write| write.master_tick)
            };
            let next_delayed_key_tick = self
                .audio_pending_ym_key_writes
                .first()
                .map(|write| write.master_tick);

            if self.next_audio_output_tick <= end_tick
                && next_write_tick.is_none_or(|tick| self.next_audio_output_tick <= tick)
                && next_delayed_key_tick.is_none_or(|tick| self.next_audio_output_tick <= tick)
                && self.next_audio_output_tick <= self.next_ym_tick
                && self.next_audio_output_tick <= self.next_psg_tick
            {
                self.push_audio_output_sample(&psg);
                continue;
            }

            if let Some(write_tick) = next_delayed_key_tick {
                if write_tick < end_tick
                    && write_tick < self.next_audio_output_tick
                    && write_tick <= self.next_ym_tick
                    && write_tick <= self.next_psg_tick
                    && next_write_tick.is_none_or(|tick| write_tick <= tick)
                {
                    let write = self.audio_pending_ym_key_writes.remove(0);
                    ym.write_address(write.port, write.addr);
                    ym.write_data(write.port, write.value);
                    continue;
                }
            }

            if let Some(write_tick) = next_write_tick {
                if write_tick < end_tick
                    && write_tick < self.next_audio_output_tick
                    && write_tick <= self.next_ym_tick
                    && write_tick <= self.next_psg_tick
                {
                    if next_write_is_ym {
                        let write = ym_writes[ym_idx];
                        if !maybe_delay_ym_key_write(
                            &mut self.audio_pending_ym_key_writes,
                            self.audio_ym_channel_key_delay_ticks,
                            write,
                        ) {
                            ym.write_address(write.port, write.addr);
                            trigger_ym_channel_pan_edge_persistence(
                                &self.ym_channel_side_memory,
                                &mut self.ym_channel_pan_edge_carry,
                                &mut self.ym_channel_pan_masks,
                                self.audio_ym_channel_pan_edge_amounts,
                                write.port,
                                write.addr,
                                write.value,
                            );
                            ym.write_data(write.port, write.value);
                        }
                        ym_idx += 1;
                    } else {
                        psg.write(psg_writes[psg_idx].value);
                        psg_idx += 1;
                    }
                    continue;
                }
            }

            if self.next_ym_tick < end_tick
                && self.next_ym_tick < self.next_audio_output_tick
                && next_write_tick.is_none_or(|tick| self.next_ym_tick < tick)
                && next_delayed_key_tick.is_none_or(|tick| self.next_ym_tick < tick)
                && self.next_ym_tick <= self.next_psg_tick
            {
                self.clock_ym_audio_sample(&mut ym);
                continue;
            }

            if self.next_psg_tick < end_tick
                && self.next_psg_tick < self.next_audio_output_tick
                && next_write_tick.is_none_or(|tick| self.next_psg_tick < tick)
                && next_delayed_key_tick.is_none_or(|tick| self.next_psg_tick < tick)
            {
                self.clock_psg_audio_sample(&mut psg);
                continue;
            }

            break;
        }

        self.audio_ym2612 = ym;
        self.audio_psg = psg;
        self.audio_master_tick = end_tick;
    }

    /// Collects audio samples for one scanline.
    #[cfg(test)]
    fn collect_audio_samples(&mut self) {
        let start_tick = self.audio_master_tick;
        let end_tick = start_tick + MASTER_TICKS_PER_SCANLINE;
        self.synthesize_audio_interval(&[], &[], start_tick, end_tick);
    }

    /// Executes a pending VDP DMA transfer by providing a bus read callback.
    fn execute_vdp_dma(&mut self) {
        // We need to read from the 68K bus, so we build a closure that
        // accesses ROM and work RAM.
        let rom = &self.rom;
        let work_ram = &self.work_ram;
        let mut read_word = |addr: u32| -> u16 {
            match bus::map_region(addr) {
                bus::BusRegion::CartridgeRom => {
                    let offset = (addr & 0x3FFFFF) as usize;
                    let hi = u16::from(*rom.get(offset).unwrap_or(&0));
                    let lo = u16::from(*rom.get(offset + 1).unwrap_or(&0));
                    (hi << 8) | lo
                }
                bus::BusRegion::WorkRam => {
                    let offset = (addr & 0xFFFF) as usize;
                    let hi = u16::from(work_ram[offset]);
                    let lo = u16::from(work_ram[offset | 1]);
                    (hi << 8) | lo
                }
                _ => 0,
            }
        };
        self.vdp.run_dma(&mut read_word);
    }

    /// Reads a big-endian u32 from the bus.
    fn read_long(&self, addr: u32) -> u32 {
        let hi = u32::from(self.read_word(addr));
        let lo = u32::from(self.read_word(addr.wrapping_add(2)));
        (hi << 16) | lo
    }

    /// Reads a big-endian u16 from the bus.
    fn read_word(&self, addr: u32) -> u16 {
        let hi = u16::from(self.read_byte(addr));
        let lo = u16::from(self.read_byte(addr.wrapping_add(1)));
        (hi << 8) | lo
    }

    /// Reads a byte from the bus.
    fn read_byte(&self, addr: u32) -> u8 {
        match bus::map_region(addr) {
            bus::BusRegion::CartridgeRom => {
                let offset = (addr & 0x3FFFFF) as usize;
                self.rom.get(offset).copied().unwrap_or(0)
            }
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset]
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                match reg {
                    0x00 | 0x01 => 0xA0, // Version: overseas NTSC, revision 0
                    0x02 | 0x03 => self.port1.read_data(),
                    0x04 | 0x05 => self.port2.read_data(),
                    0x08 | 0x09 => self.port1.read_ctrl(),
                    0x0A | 0x0B => self.port2.read_ctrl(),
                    _ => 0,
                }
            }
            bus::BusRegion::ControlRegisters => {
                // Z80 bus request (0xA11100): bit 0 = 0 means bus granted to 68K
                // Since Z80 is not emulated, bus is always available.
                0x00
            }
            bus::BusRegion::Vdp => {
                // VDP byte reads: return high or low byte of word read
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x04 | 0x06 => {
                        // Status register (read-only)
                        let status = self.vdp.read_status();
                        if vdp_addr & 1 == 0 {
                            (status >> 8) as u8
                        } else {
                            status as u8
                        }
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// Writes a byte to the bus.
    #[allow(dead_code)]
    fn write_byte_bus(&mut self, addr: u32, val: u8) {
        match bus::map_region(addr) {
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset] = val;
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                match reg {
                    0x02 | 0x03 => self.port1.write_data(val),
                    0x04 | 0x05 => self.port2.write_data(val),
                    0x08 | 0x09 => self.port1.write_ctrl(val),
                    0x0A | 0x0B => self.port2.write_ctrl(val),
                    _ => {}
                }
            }
            bus::BusRegion::ControlRegisters => {
                // Z80 bus request/reset — absorbed (Z80 not emulated)
            }
            _ => {}
        }
    }

    /// Writes a big-endian u16 to the bus.
    #[allow(dead_code)]
    fn write_word_bus(&mut self, addr: u32, val: u16) {
        match bus::map_region(addr) {
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset] = (val >> 8) as u8;
                self.work_ram[offset | 1] = val as u8;
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                let lo = val as u8;
                match reg {
                    0x02 | 0x03 => self.port1.write_data(lo),
                    0x04 | 0x05 => self.port2.write_data(lo),
                    0x08 | 0x09 => self.port1.write_ctrl(lo),
                    0x0A | 0x0B => self.port2.write_ctrl(lo),
                    _ => {}
                }
            }
            bus::BusRegion::ControlRegisters => {
                // Z80 bus request/reset — absorbed (Z80 not emulated)
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x00 | 0x02 => self.vdp.write_data(val),
                    0x04 | 0x06 => self.vdp.write_control(val),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Bus wrapper that borrows non-CPU fields from [`GenesisCore`],
/// allowing the CPU executor to access memory without conflicting
/// with the mutable borrow of the CPU.
struct CoreBus<'a> {
    rom: &'a [u8],
    work_ram: &'a mut Box<[u8; 0x10000]>,
    vdp: &'a mut Vdp,
    port1: &'a mut ControllerPort,
    port2: &'a mut ControllerPort,
    z80_ram: &'a mut Box<[u8; 0x2000]>,
    z80_bus_requested: &'a mut bool,
    z80_reset: &'a mut bool,
    z80_reset_pending: &'a mut bool,
    z80_bus_released_this_scanline: &'a mut bool,
    ym2612: &'a mut ym2612::Ym2612,
    psg: &'a mut psg::Psg,
    z80_cmd_trace: &'a mut Vec<(u64, u8)>,
    z80_driver_write_count: &'a mut u32,
    z80_driver_last_write_frame: &'a mut u64,
    ym2612_timed_write_trace: &'a mut Vec<TimedYm2612Write>,
    psg_timed_write_trace: &'a mut Vec<TimedPsgWrite>,
    frame_count: u64,
    scanline: u16,
    master_tick: u64,
}

impl CoreBus<'_> {
    fn write_ym2612_data(&mut self, port: u8, value: u8) {
        let addr = self.ym2612.latched_address(port);
        self.ym2612_timed_write_trace.push(TimedYm2612Write {
            master_tick: self.master_tick,
            frame: self.frame_count,
            scanline: self.scanline,
            port,
            addr,
            value,
        });
        self.ym2612.write_data(port, value);
    }

    fn write_psg(&mut self, value: u8) {
        self.psg_timed_write_trace.push(TimedPsgWrite {
            master_tick: self.master_tick,
            frame: self.frame_count,
            scanline: self.scanline,
            value,
        });
        self.psg.write(value);
    }
}

impl Bus for CoreBus<'_> {
    fn read_byte(&mut self, addr: u32) -> u8 {
        match bus::map_region(addr) {
            bus::BusRegion::CartridgeRom => {
                let offset = (addr & 0x3FFFFF) as usize;
                self.rom.get(offset).copied().unwrap_or(0)
            }
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset]
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                match reg {
                    0x00 | 0x01 => 0xA0, // Version: overseas NTSC, revision 0
                    0x02 | 0x03 => self.port1.read_data(),
                    0x04 | 0x05 => self.port2.read_data(),
                    0x08 | 0x09 => self.port1.read_ctrl(),
                    0x0A | 0x0B => self.port2.read_ctrl(),
                    _ => 0,
                }
            }
            bus::BusRegion::Z80Area => {
                let z80_addr = addr & 0xFFFF;
                match z80_addr {
                    0x0000..=0x1FFF => self.z80_ram[z80_addr as usize],
                    0x2000..=0x3FFF => self.z80_ram[(z80_addr & 0x1FFF) as usize],
                    0x4000..=0x4003 => self.ym2612.read_status(),
                    _ => 0xFF,
                }
            }
            bus::BusRegion::ControlRegisters => {
                // Z80 bus request (0xA11100): bit 0 = 0 means bus granted to 68K
                let offset = addr & 0x01FF;
                match offset {
                    0x0000..=0x0001 => {
                        // Return bus status: if bus was requested and Z80 is idle,
                        // bit 0 = 0 means bus granted
                        if *self.z80_bus_requested { 0x00 } else { 0x01 }
                    }
                    _ => 0x00,
                }
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x04 | 0x06 => {
                        let status = self.vdp.read_status();
                        if addr & 1 == 0 {
                            (status >> 8) as u8
                        } else {
                            status as u8
                        }
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn read_word(&mut self, addr: u32) -> u16 {
        match bus::map_region(addr) {
            bus::BusRegion::CartridgeRom => {
                let offset = (addr & 0x3FFFFF) as usize;
                let hi = u16::from(*self.rom.get(offset).unwrap_or(&0));
                let lo = u16::from(*self.rom.get(offset + 1).unwrap_or(&0));
                (hi << 8) | lo
            }
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                let hi = u16::from(self.work_ram[offset]);
                let lo = u16::from(self.work_ram[offset | 1]);
                (hi << 8) | lo
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                let val = match reg {
                    0x00 | 0x01 => 0xA0, // Version register
                    0x02 | 0x03 => self.port1.read_data(),
                    0x04 | 0x05 => self.port2.read_data(),
                    0x08 | 0x09 => self.port1.read_ctrl(),
                    0x0A | 0x0B => self.port2.read_ctrl(),
                    _ => 0,
                };
                u16::from(val)
            }
            bus::BusRegion::Z80Area => {
                let z80_addr = addr & 0xFFFF;
                match z80_addr {
                    0x0000..=0x1FFF => {
                        let offset = z80_addr as usize;
                        let hi = u16::from(self.z80_ram[offset]);
                        let lo = u16::from(self.z80_ram[(offset + 1) & 0x1FFF]);
                        (hi << 8) | lo
                    }
                    0x2000..=0x3FFF => {
                        let offset = (z80_addr & 0x1FFF) as usize;
                        let hi = u16::from(self.z80_ram[offset]);
                        let lo = u16::from(self.z80_ram[(offset + 1) & 0x1FFF]);
                        (hi << 8) | lo
                    }
                    0x4000..=0x4003 => u16::from(self.ym2612.read_status()),
                    _ => 0xFFFF,
                }
            }
            bus::BusRegion::ControlRegisters => {
                // Z80 bus request (0xA11100): bit 0 = 0 means bus granted
                let offset = addr & 0x01FF;
                match offset {
                    0x0000..=0x0001 => {
                        if *self.z80_bus_requested {
                            0x0000
                        } else {
                            0x0100
                        }
                    }
                    _ => 0x0000,
                }
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x00 | 0x02 => self.vdp.read_data(),
                    0x04 | 0x06 => self.vdp.read_status(),
                    0x08 | 0x0A | 0x0C | 0x0E => self.vdp.read_hv_counter(),
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write_byte(&mut self, addr: u32, val: u8) {
        match bus::map_region(addr) {
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset] = val;
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                match reg {
                    0x02 | 0x03 => self.port1.write_data(val),
                    0x04 | 0x05 => self.port2.write_data(val),
                    0x08 | 0x09 => self.port1.write_ctrl(val),
                    0x0A | 0x0B => self.port2.write_ctrl(val),
                    _ => {}
                }
            }
            bus::BusRegion::Z80Area => {
                let z80_addr = addr & 0xFFFF;
                match z80_addr {
                    0x0000..=0x1FFF => {
                        if z80_addr == 0x1FFF && self.z80_cmd_trace.len() < 100 {
                            self.z80_cmd_trace.push((self.frame_count, val));
                        }
                        if z80_addr <= 0x00FF {
                            *self.z80_driver_write_count += 1;
                            *self.z80_driver_last_write_frame = self.frame_count;
                        }
                        self.z80_ram[z80_addr as usize] = val;
                    }
                    0x2000..=0x3FFF => self.z80_ram[(z80_addr & 0x1FFF) as usize] = val,
                    0x4000 => self.ym2612.write_address(0, val),
                    0x4001 => self.write_ym2612_data(0, val),
                    0x4002 => self.ym2612.write_address(1, val),
                    0x4003 => self.write_ym2612_data(1, val),
                    _ => {}
                }
            }
            bus::BusRegion::ControlRegisters => {
                // 0xA11100 = Z80 bus request, 0xA11200 = Z80 reset
                let reg = addr & 0xFFFF;
                match reg {
                    0x1100..=0x1101 => {
                        let new_req = val & 0x01 != 0;
                        if *self.z80_bus_requested && !new_req {
                            *self.z80_bus_released_this_scanline = true;
                        }
                        *self.z80_bus_requested = new_req;
                    }
                    0x1200..=0x1201 => {
                        let new_reset = val & 0x01 == 0;
                        if *self.z80_reset && !new_reset {
                            *self.z80_reset_pending = true;
                        }
                        *self.z80_reset = new_reset;
                    }
                    _ => {}
                }
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x11 | 0x13 | 0x15 | 0x17 => {
                        self.write_psg(val);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn write_word(&mut self, addr: u32, val: u16) {
        match bus::map_region(addr) {
            bus::BusRegion::WorkRam => {
                let offset = (addr & 0xFFFF) as usize;
                self.work_ram[offset] = (val >> 8) as u8;
                self.work_ram[offset | 1] = val as u8;
            }
            bus::BusRegion::Vdp => {
                let vdp_addr = addr & 0x1F;
                match vdp_addr {
                    0x00 | 0x02 => self.vdp.write_data(val),
                    0x04 | 0x06 => self.vdp.write_control(val),
                    0x10 | 0x12 | 0x14 | 0x16 => {
                        // PSG port (write low byte)
                        self.psg.write(val as u8);
                    }
                    _ => {}
                }
            }
            bus::BusRegion::IoRegisters => {
                let reg = (addr & 0x1F) as u8;
                let lo = val as u8;
                match reg {
                    0x02 | 0x03 => self.port1.write_data(lo),
                    0x04 | 0x05 => self.port2.write_data(lo),
                    0x08 | 0x09 => self.port1.write_ctrl(lo),
                    0x0A | 0x0B => self.port2.write_ctrl(lo),
                    _ => {}
                }
            }
            bus::BusRegion::Z80Area => {
                let z80_addr = addr & 0xFFFF;
                let hi = (val >> 8) as u8;
                let lo = val as u8;
                match z80_addr {
                    0x0000..=0x1FFF => {
                        if z80_addr <= 0x00FF {
                            *self.z80_driver_write_count += 2;
                            *self.z80_driver_last_write_frame = self.frame_count;
                        }
                        self.z80_ram[z80_addr as usize] = hi;
                        self.z80_ram[((z80_addr + 1) & 0x1FFF) as usize] = lo;
                    }
                    0x2000..=0x3FFF => {
                        let off = (z80_addr & 0x1FFF) as usize;
                        self.z80_ram[off] = hi;
                        self.z80_ram[(off + 1) & 0x1FFF] = lo;
                    }
                    0x4000 => {
                        self.ym2612.write_address(0, hi);
                        self.write_ym2612_data(0, lo);
                    }
                    0x4002 => {
                        self.ym2612.write_address(1, hi);
                        self.write_ym2612_data(1, lo);
                    }
                    _ => {}
                }
            }
            bus::BusRegion::ControlRegisters => {
                // 0xA11100 = Z80 bus request, 0xA11200 = Z80 reset
                let reg = addr & 0xFFFF;
                match reg {
                    0x1100..=0x1101 => {
                        let new_req = (val >> 8) & 0x01 != 0;
                        if *self.z80_bus_requested && !new_req {
                            *self.z80_bus_released_this_scanline = true;
                        }
                        *self.z80_bus_requested = new_req;
                    }
                    0x1200..=0x1201 => {
                        let new_reset = (val >> 8) & 0x01 == 0;
                        if *self.z80_reset && !new_reset {
                            *self.z80_reset_pending = true;
                        }
                        *self.z80_reset = new_reset;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Bus wrapper for Z80 access to the Genesis sound subsystem.
///
/// Used in [`GenesisCore::step_z80_scanline`] to give the Z80 access
/// to its RAM, banked 68K ROM, YM2612, and PSG.
struct Z80Bus<'a> {
    z80_ram: &'a mut Box<[u8; 0x2000]>,
    rom: &'a [u8],
    z80_bank: &'a mut u32,
    ym2612: &'a mut ym2612::Ym2612,
    psg: &'a mut psg::Psg,
    ym2612_timed_write_trace: &'a mut Vec<TimedYm2612Write>,
    psg_timed_write_trace: &'a mut Vec<TimedPsgWrite>,
    frame_count: u64,
    scanline: u16,
    master_tick: u64,
}

impl Z80Bus<'_> {
    fn write_ym2612_data(&mut self, port: u8, value: u8) {
        let addr = self.ym2612.latched_address(port);
        self.ym2612_timed_write_trace.push(TimedYm2612Write {
            master_tick: self.master_tick,
            frame: self.frame_count,
            scanline: self.scanline,
            port,
            addr,
            value,
        });
        self.ym2612.write_data(port, value);
    }

    fn write_psg(&mut self, value: u8) {
        self.psg_timed_write_trace.push(TimedPsgWrite {
            master_tick: self.master_tick,
            frame: self.frame_count,
            scanline: self.scanline,
            value,
        });
        self.psg.write(value);
    }
}

impl z80::execute::Bus for Z80Bus<'_> {
    fn read_byte(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.z80_ram[addr as usize],
            0x2000..=0x3FFF => self.z80_ram[(addr & 0x1FFF) as usize],
            0x4000..=0x4003 => self.ym2612.read_status(),
            0x8000..=0xFFFF => {
                // Banked 68K ROM window
                let offset = *self.z80_bank + u32::from(addr & 0x7FFF);
                self.rom.get(offset as usize).copied().unwrap_or(0)
            }
            _ => 0xFF,
        }
    }

    fn write_byte(&mut self, addr: u16, val: u8) {
        match addr {
            0x0000..=0x1FFF => self.z80_ram[addr as usize] = val,
            0x2000..=0x3FFF => self.z80_ram[(addr & 0x1FFF) as usize] = val,
            0x4000 => self.ym2612.write_address(0, val),
            0x4001 => self.write_ym2612_data(0, val),
            0x4002 => self.ym2612.write_address(1, val),
            0x4003 => self.write_ym2612_data(1, val),
            0x6000..=0x60FF => {
                // Bank register: shift in one bit at a time (bit 0 of val),
                // 9 bits forming bits 15-23 of the ROM address.
                *self.z80_bank = ((*self.z80_bank >> 1) | ((u32::from(val) & 1) << 23)) & 0xFF8000;
            }
            0x7F00..=0x7FFF => self.write_psg(val),
            _ => {}
        }
    }

    fn read_port(&mut self, _port: u16) -> u8 {
        0xFF
    }

    fn write_port(&mut self, _port: u16, _val: u8) {}
}

impl Default for GenesisCore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::z80::execute::Bus as _;

    fn core_bus_with_master_tick<'a>(core: &'a mut GenesisCore, master_tick: u64) -> CoreBus<'a> {
        let scanline = core.vdp.scanline();
        CoreBus {
            rom: &core.rom,
            work_ram: &mut core.work_ram,
            vdp: &mut core.vdp,
            port1: &mut core.port1,
            port2: &mut core.port2,
            z80_ram: &mut core.z80_ram,
            z80_bus_requested: &mut core.z80_bus_requested,
            z80_reset: &mut core.z80_reset,
            z80_reset_pending: &mut core.z80_reset_pending,
            z80_bus_released_this_scanline: &mut core.z80_bus_released_this_scanline,
            ym2612: &mut core.ym2612,
            psg: &mut core.psg,
            z80_cmd_trace: &mut core.z80_cmd_trace,
            z80_driver_write_count: &mut core.z80_driver_write_count,
            z80_driver_last_write_frame: &mut core.z80_driver_last_write_frame,
            ym2612_timed_write_trace: &mut core.ym2612_timed_write_trace,
            psg_timed_write_trace: &mut core.psg_timed_write_trace,
            frame_count: core.frame_count,
            scanline,
            master_tick,
        }
    }

    fn z80_bus_with_master_tick<'a>(core: &'a mut GenesisCore, master_tick: u64) -> Z80Bus<'a> {
        Z80Bus {
            z80_ram: &mut core.z80_ram,
            rom: &core.rom,
            z80_bank: &mut core.z80_bank,
            ym2612: &mut core.ym2612,
            psg: &mut core.psg,
            ym2612_timed_write_trace: &mut core.ym2612_timed_write_trace,
            psg_timed_write_trace: &mut core.psg_timed_write_trace,
            frame_count: core.frame_count,
            scanline: core.vdp.scanline(),
            master_tick,
        }
    }

    fn write_ym_reg(ym: &mut ym2612::Ym2612, bank: u8, addr: u8, val: u8) {
        ym.write_address(bank, addr);
        ym.write_data(bank, val);
    }

    fn configure_two_op_fm_tone(ym: &mut ym2612::Ym2612) {
        // Match the test-harness two-op FM probe: algorithm 4, op1->op2 pair active.
        write_ym_reg(ym, 0, 0xB0, 0x04);
        write_ym_reg(ym, 0, 0xB4, 0xC0);

        write_ym_reg(ym, 0, 0x40, 127);
        write_ym_reg(ym, 0, 0x44, 127);
        write_ym_reg(ym, 0, 0x48, 127);
        write_ym_reg(ym, 0, 0x4C, 127);

        write_ym_reg(ym, 0, 0x30, 0x01);
        write_ym_reg(ym, 0, 0x40, 0x00);
        write_ym_reg(ym, 0, 0x50, 31);
        write_ym_reg(ym, 0, 0x60, 0x00);
        write_ym_reg(ym, 0, 0x70, 0x00);
        write_ym_reg(ym, 0, 0x80, 0x0F);

        write_ym_reg(ym, 0, 0x38, 0x01);
        write_ym_reg(ym, 0, 0x48, 0x00);
        write_ym_reg(ym, 0, 0x58, 31);
        write_ym_reg(ym, 0, 0x68, 0x00);
        write_ym_reg(ym, 0, 0x78, 0x00);
        write_ym_reg(ym, 0, 0x88, 0x0F);

        write_ym_reg(ym, 0, 0xA4, (4 << 3) | ((653 >> 8) as u8 & 0x07));
        write_ym_reg(ym, 0, 0xA0, (653 & 0xFF) as u8);
        write_ym_reg(ym, 0, 0x28, 0x30);
    }

    fn cross_correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        if n == 0 {
            return 0.0;
        }

        let mean_a: f64 = a[..n].iter().map(|&x| f64::from(x)).sum::<f64>() / n as f64;
        let mean_b: f64 = b[..n].iter().map(|&x| f64::from(x)).sum::<f64>() / n as f64;

        let mut cov = 0.0f64;
        let mut var_a = 0.0f64;
        let mut var_b = 0.0f64;

        for i in 0..n {
            let da = f64::from(a[i]) - mean_a;
            let db = f64::from(b[i]) - mean_b;
            cov += da * db;
            var_a += da * da;
            var_b += db * db;
        }

        if var_a < 1e-12 || var_b < 1e-12 {
            return 0.0;
        }

        (cov / (var_a * var_b).sqrt()) as f32
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }

        let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    fn render_reference_fm_audio(sample_count: usize) -> Vec<f32> {
        const YM_NATIVE_RATE: f64 = 7_670_454.0 / 144.0;
        const OUTPUT_RATE: f64 = 44_100.0;

        let mut ym = ym2612::Ym2612::new();
        configure_two_op_fm_tone(&mut ym);

        let ym_ratio = YM_NATIVE_RATE / OUTPUT_RATE;
        let mut ym_phase = 0.0f64;
        let mut left_lpf = FirstOrderLowPassFilter::new(YM_LPF_B0, YM_LPF_B1, YM_LPF_A1);
        let mut right_lpf = FirstOrderLowPassFilter::new(YM_LPF_B0, YM_LPF_B1, YM_LPF_A1);
        let mut output = Vec::with_capacity(sample_count * 2);

        for _ in 0..sample_count {
            ym_phase += ym_ratio;
            let mut left_acc = 0.0f64;
            let mut right_acc = 0.0f64;
            let mut ym_count = 0u32;

            while ym_phase >= 1.0 {
                ym_phase -= 1.0;
                let (l, r) = ym.output_sample();
                left_acc += f64::from(left_lpf.filter(l));
                right_acc += f64::from(right_lpf.filter(r));
                ym_count += 1;
            }

            let (left, right) = if ym_count > 0 {
                (
                    (left_acc / ym_count as f64) as f32,
                    (right_acc / ym_count as f64) as f32,
                )
            } else {
                (left_lpf.last_output(), right_lpf.last_output())
            };

            output.push(left);
            output.push(right);
        }

        output
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()))
    }

    /// Fraction of samples at or above a near-full-scale magnitude threshold.
    fn clipped_fraction(samples: &[f32], threshold: f32) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let clipped = samples.iter().filter(|&&s| s.abs() >= threshold).count();
        clipped as f32 / samples.len() as f32
    }

    /// Configures one YM channel as a loud sustained tone: algorithm 7 (all four
    /// operators are carriers), max volume (TL 0), fast attack, no decay, hard-panned
    /// to both outputs.
    fn configure_loud_channel(ym: &mut ym2612::Ym2612, bank: u8, ch: u8, fnum: u16, block: u8) {
        write_ym_reg(ym, bank, 0xB0 + ch, 0x07); // algorithm 7, feedback 0
        write_ym_reg(ym, bank, 0xB4 + ch, 0xC0); // left + right enabled
        for op in [0x00u8, 0x04, 0x08, 0x0C] {
            let reg = ch + op;
            write_ym_reg(ym, bank, 0x30 + reg, 0x01); // detune 0, multiple 1
            write_ym_reg(ym, bank, 0x40 + reg, 0x00); // total level 0 (loudest)
            write_ym_reg(ym, bank, 0x50 + reg, 0x1F); // attack rate 31
            write_ym_reg(ym, bank, 0x60 + reg, 0x00); // decay rate 0
            write_ym_reg(ym, bank, 0x70 + reg, 0x00); // sustain rate 0
            write_ym_reg(ym, bank, 0x80 + reg, 0x00); // sustain level 0, release 0
        }
        write_ym_reg(ym, bank, 0xA4 + ch, (block << 3) | ((fnum >> 8) as u8 & 0x07));
        write_ym_reg(ym, bank, 0xA0 + ch, (fnum & 0xFF) as u8);
    }

    /// Loud, dense representative patch: four simultaneous algorithm-7 channels at
    /// distinct pitches (16 carriers summed), all keyed on and hard-panned centre.
    fn configure_loud_patch(ym: &mut ym2612::Ym2612) {
        configure_loud_channel(ym, 0, 0, 617, 4);
        configure_loud_channel(ym, 0, 1, 800, 4);
        configure_loud_channel(ym, 0, 2, 1000, 4);
        configure_loud_channel(ym, 1, 0, 1083, 4);
        // Key on all four operators of channels 1, 2, 3 (bank 0) and 4 (bank 1).
        for code in [0x00u8, 0x01, 0x02, 0x04] {
            write_ym_reg(ym, 0, 0x28, 0xF0 | code);
        }
    }

    /// Renders the loud patch through the full live output chain under `config`,
    /// returning the interleaved stereo output buffer (post soft limiter).
    fn render_loud_patch(config: AudioOutputConfig, stereo_pairs: usize) -> Vec<f32> {
        let mut core = GenesisCore::new();
        core.audio_output_config = config;
        core.rebuild_audio_output_pipeline();
        configure_loud_patch(&mut core.audio_ym2612);
        while core.audio_buffer.len() < stereo_pairs * 2 {
            core.collect_audio_samples();
        }
        core.audio_buffer.clone()
    }

    #[test]
    fn default_audio_config_is_flat() {
        let cfg = AudioOutputConfig::default();
        // No baked EQ coloration.
        assert!(cfg.post_eq_1.is_none());
        assert!(cfg.post_eq_2.is_none());
        assert!(cfg.post_eq_3.is_none());
        assert!(cfg.post_eq_4.is_none());
        assert!(cfg.post_eq_5.is_none());
        assert!(cfg.post_side_eq_1.is_none());
        assert!(cfg.post_side_eq_2.is_none());
        // No crossfeed / side coloration / side memory.
        assert_eq!(cfg.stereo_crossfeed, 0.0);
        assert_eq!(cfg.mid_gain, 1.0);
        assert_eq!(cfg.side_gain, 1.0);
        assert_eq!(cfg.ym_channel_side_memory_amounts, [0.0; 6]);
        // Unity per-chip gains, only the Legacy YM anti-alias low-pass profile.
        assert_eq!(cfg.ym_gain, 1.0);
        assert_eq!(cfg.psg_gain, 1.0);
        assert_eq!(cfg.profile, AudioOutputProfile::Legacy);
        // The former colored chain is still reachable, but is not the default.
        assert_ne!(cfg, AudioOutputConfig::ghz_colored());
    }

    #[test]
    fn soft_limit_is_smooth_and_bounded() {
        // Linear (identity) below the knee.
        assert!((soft_limit(0.5) - 0.5).abs() < 1e-6);
        assert!((soft_limit(-0.79) - (-0.79)).abs() < 1e-6);
        // Never exceeds full scale, and stays strictly inside it for moderate overshoot.
        assert!(soft_limit(100.0) <= 1.0);
        assert!(soft_limit(-100.0) >= -1.0);
        assert!(soft_limit(2.0) < 1.0);
        assert!(soft_limit(-2.0) > -1.0);
        // Monotonic and continuous around the knee.
        assert!(soft_limit(0.85) > soft_limit(0.8));
        assert!(soft_limit(0.85) < 0.85);
    }

    #[test]
    fn soft_limiter_keeps_loud_patch_unclipped() {
        const PAIRS: usize = 8_000;
        // Skip the attack transient at the very start of the render.
        const SKIP: usize = 2_000;

        // AFTER: new flat default (soft limiter, gain tuned for loudness).
        let after = render_loud_patch(AudioOutputConfig::default(), PAIRS);
        let after = &after[SKIP..];
        let after_clip = clipped_fraction(after, 0.999);
        let after_peak = peak(after);
        let after_rms = rms(after);

        // BEFORE (reference numbers): the old shipped default (`ghz_colored`,
        // master_gain 2.2, colored EQ) fed a brickwall `clamp(-1.0, 1.0)`. The
        // pre-limiter chain is linear in master_gain, so render it at a tiny gain,
        // scale back up to the old 2.2 gain, and hard-clamp to reproduce the old
        // behavior on the identical patch.
        let unity = render_loud_patch(AudioOutputConfig::ghz_colored().with_gain(0.01), PAIRS);
        let before_hard: Vec<f32> = unity[SKIP..]
            .iter()
            .map(|&s| (s * 100.0 * 2.2).clamp(-1.0, 1.0))
            .collect();
        let before_clip = clipped_fraction(&before_hard, 0.999);
        let before_peak = peak(&before_hard);
        let before_rms = rms(&before_hard);

        eprintln!(
            "[soft-limiter] BEFORE (old default, hard clamp): clip%={:.2} peak={:.4} rms={:.4}",
            before_clip * 100.0,
            before_peak,
            before_rms
        );
        eprintln!(
            "[soft-limiter] AFTER  (new flat default, soft):   clip%={:.2} peak={:.4} rms={:.4}",
            after_clip * 100.0,
            after_peak,
            after_rms
        );

        // The soft limiter must virtually eliminate clipping...
        assert!(
            after_clip < 0.01,
            "clipped fraction too high: {after_clip}"
        );
        // ...while keeping a healthy, loud level well inside full scale.
        assert!(
            (0.5..=0.99).contains(&after_peak),
            "peak outside healthy loud range: {after_peak}"
        );
        assert!(after_rms > 0.1, "output too quiet: rms={after_rms}");
    }

    #[test]
    fn new_core_is_not_paused() {
        let core = GenesisCore::new();
        assert!(!core.paused());
    }

    #[test]
    fn set_audio_output_config_rebuilds_audio_pipeline() {
        let mut core = GenesisCore::new();
        core.audio_buffer.extend_from_slice(&[0.25, -0.25]);
        core.audio_output_sample_count = 99;
        core.ym_filter_left.filter(1.0);
        core.psg_filter.filter(0.5);

        let config = AudioOutputConfig::new(AudioOutputProfile::Legacy, 2.5)
            .with_ym_gain(1.75)
            .with_psg_gain(0.5)
            .with_ym_channel_side_memory_amounts([0.20, 0.0, 0.0, 0.15, 0.0, 0.0])
            .with_ym_channel_side_decay_ms([0.0, 0.0, 0.0, 12.0, 25.0, 0.0])
            .with_ym_channel_key_delay_ms([5.0, 0.0, 0.0, 5.0, 5.0, 0.0])
            .with_ym_channel_pan_edge_amounts([0.0, 0.0, 0.0, 0.08, 0.12, 0.0])
            .with_ym_channel_pan_edge_decay_ms([0.0, 0.0, 0.0, 12.0, 25.0, 0.0])
            .with_stereo_crossfeed(0.12)
            .with_mid_gain(1.02)
            .with_side_gain(0.96)
            .with_post_high_pass_hz(60.0)
            .with_post_low_pass_hz(12_000.0)
            .with_post_eq_1(AudioEqStage::low_shelf(110.0, -8.0))
            .with_post_eq_2(AudioEqStage::peaking(420.0, 0.75, 6.0))
            .with_post_eq_3(AudioEqStage::high_shelf(2_600.0, -3.5))
            .with_post_eq_4(AudioEqStage::peaking(190.0, 0.90, 3.0))
            .with_post_eq_5(AudioEqStage::peaking(760.0, 1.10, 1.5))
            .with_post_side_eq_1(AudioEqStage::peaking(420.0, 0.90, -1.5))
            .with_post_side_eq_2(AudioEqStage::peaking(900.0, 1.00, 1.2))
            .with_post_fir_taps([0.88, 0.10, 0.02, 0.0, 0.0])
            .with_post_left_delay_samples(1)
            .with_post_right_delay_samples(2);
        core.execute(Command::SetAudioOutputConfig(config));

        assert_eq!(core.audio_output_config(), config);
        assert!(core.audio_buffer.is_empty());
        assert_eq!(core.audio_output_sample_count(), 0);
        assert_eq!(core.audio_master_ticks(), 0);
        assert_eq!(core.audio_master_gain, 2.5);
        assert_eq!(core.audio_ym_gain, 1.75);
        assert_eq!(core.audio_psg_gain, 0.5);
        assert_eq!(
            core.audio_ym_channel_side_memory_amounts,
            [0.20, 0.0, 0.0, 0.15, 0.0, 0.0]
        );
        assert_eq!(
            core.audio_ym_channel_side_decay_factors,
            [
                0.0,
                0.0,
                0.0,
                pan_edge_decay_factor(12.0, MASTER_CLOCK_NTSC as f32 / YM_AUDIO_TICKS as f32),
                pan_edge_decay_factor(25.0, MASTER_CLOCK_NTSC as f32 / YM_AUDIO_TICKS as f32),
                0.0,
            ]
        );
        assert_eq!(
            core.audio_ym_channel_key_delay_ticks,
            [
                ((5.0 / 1000.0) * MASTER_CLOCK_NTSC as f32).round() as u64,
                0,
                0,
                ((5.0 / 1000.0) * MASTER_CLOCK_NTSC as f32).round() as u64,
                ((5.0 / 1000.0) * MASTER_CLOCK_NTSC as f32).round() as u64,
                0,
            ]
        );
        assert_eq!(
            core.audio_ym_channel_pan_edge_amounts,
            [0.0, 0.0, 0.0, 0.08, 0.12, 0.0]
        );
        assert_eq!(
            core.audio_ym_channel_pan_edge_decay_factors,
            [
                0.0,
                0.0,
                0.0,
                pan_edge_decay_factor(12.0, MASTER_CLOCK_NTSC as f32 / YM_AUDIO_TICKS as f32),
                pan_edge_decay_factor(25.0, MASTER_CLOCK_NTSC as f32 / YM_AUDIO_TICKS as f32),
                0.0,
            ]
        );
        assert_eq!(core.audio_stereo_crossfeed, 0.12);
        assert_eq!(core.audio_mid_gain, 1.02);
        assert_eq!(core.audio_side_gain, 0.96);
        assert_eq!(core.audio_psg_mix, DEFAULT_PSG_MIX);
        assert!(matches!(core.ym_filter_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.ym_filter_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.psg_filter, AudioFilterState::Flat { .. }));
        assert!(matches!(
            core.post_high_pass_left,
            AudioFilterState::FirstOrder(_)
        ));
        assert!(matches!(
            core.post_high_pass_right,
            AudioFilterState::FirstOrder(_)
        ));
        assert!(matches!(
            core.post_low_pass_left,
            AudioFilterState::FirstOrder(_)
        ));
        assert!(matches!(
            core.post_low_pass_right,
            AudioFilterState::FirstOrder(_)
        ));
        assert!(matches!(core.post_eq_1_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_1_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_2_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_2_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_3_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_3_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_4_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_4_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_5_left, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_eq_5_right, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_side_eq_1, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_side_eq_2, AudioFilterState::Biquad(_)));
        assert!(matches!(core.post_fir_left, AudioFilterState::Fir(_)));
        assert!(matches!(core.post_fir_right, AudioFilterState::Fir(_)));
        assert_eq!(core.post_delay_left.delay_samples, 1);
        assert_eq!(core.post_delay_right.delay_samples, 2);
    }

    #[test]
    fn stereo_crossfeed_blends_channels_symmetrically() {
        let (left, right) = apply_stereo_crossfeed(1.0, 0.0, 0.25);
        assert!((left - 0.75).abs() < 1e-6);
        assert!((right - 0.25).abs() < 1e-6);

        let (left, right) = apply_stereo_crossfeed(1.0, -1.0, 0.10);
        assert!((left - 0.8).abs() < 1e-6);
        assert!((right + 0.8).abs() < 1e-6);
    }

    #[test]
    fn ym_channel_side_memory_zero_amount_is_identity() {
        let channel_samples = [
            (0.5, -0.5),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.25, 0.25),
            (0.0, 0.0),
            (0.0, 0.0),
        ];
        let mut side_memory = [0.0f32; 6];
        let mut previous_side = [0.0f32; 6];
        let mixed = mix_ym_channel_outputs_with_side_memory(
            channel_samples,
            &mut side_memory,
            [0.0; 6],
            [0.0; 6],
            [0.0; 6],
            [0.0; 6],
            &mut previous_side,
            &mut [0.0; 6],
            [0.0; 6],
        );
        assert!((mixed.0 - 0.75).abs() < 1e-6);
        assert!((mixed.1 + 0.25).abs() < 1e-6);
        assert_eq!(side_memory, [0.5, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn ym_channel_side_memory_adds_delayed_side_without_touching_centered_channel() {
        let mut side_memory = [0.0f32; 6];
        let mut previous_side = [0.0f32; 6];
        let mut pan_edge_carry = [0.0f32; 6];
        let amounts = [0.20, 0.0, 0.0, 0.0, 0.0, 0.0];
        let transient_mixes = [0.0f32; 6];
        let sign_align_mixes = [0.0f32; 6];
        let side_decay = [0.0f32; 6];
        let decay = [0.0f32; 6];

        let first = mix_ym_channel_outputs_with_side_memory(
            [
                (0.5, -0.5),
                (0.25, 0.25),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            decay,
        );
        let second = mix_ym_channel_outputs_with_side_memory(
            [
                (0.0, 0.0),
                (0.25, 0.25),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            decay,
        );

        assert!((first.0 - 0.75).abs() < 1e-6);
        assert!((first.1 + 0.25).abs() < 1e-6);
        assert!(
            (second.0 - 0.10 - 0.25).abs() < 1e-6,
            "expected delayed side memory on left output, got {:?}",
            second
        );
        assert!(
            (second.1 + 0.10 - 0.25).abs() < 1e-6,
            "expected delayed side memory on right output, got {:?}",
            second
        );
    }

    #[test]
    fn ym_channel_side_memory_decay_extends_persistence_across_extra_sample() {
        let mut side_memory = [0.0f32; 6];
        let mut previous_side = [0.0f32; 6];
        let mut pan_edge_carry = [0.0f32; 6];
        let amounts = [0.20, 0.0, 0.0, 0.0, 0.0, 0.0];
        let transient_mixes = [0.0f32; 6];
        let sign_align_mixes = [0.0f32; 6];
        let side_decay = [0.50f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let pan_decay = [0.0f32; 6];

        let _ = mix_ym_channel_outputs_with_side_memory(
            [
                (0.5, -0.5),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );
        let second = mix_ym_channel_outputs_with_side_memory(
            [(0.0, 0.0); 6],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );
        let third = mix_ym_channel_outputs_with_side_memory(
            [(0.0, 0.0); 6],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );

        assert!((second.0 - 0.10).abs() < 1e-6);
        assert!((second.1 + 0.10).abs() < 1e-6);
        assert!(
            (third.0 - 0.05).abs() < 1e-6,
            "expected decayed side memory to persist into third sample, got {third:?}"
        );
        assert!(
            (third.1 + 0.05).abs() < 1e-6,
            "expected decayed side memory to persist into third sample, got {third:?}"
        );
    }

    #[test]
    fn ym_channel_pan_edge_persistence_triggers_on_pan_change_and_decays_after_mix() {
        let side_memory = [0.5f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut pan_edge_carry = [0.0f32; 6];
        let mut pan_masks = [0xC0u8; 6];
        trigger_ym_channel_pan_edge_persistence(
            &side_memory,
            &mut pan_edge_carry,
            &mut pan_masks,
            [0.20, 0.0, 0.0, 0.0, 0.0, 0.0],
            0,
            0xB4,
            0x80,
        );

        assert!((pan_edge_carry[0] - 0.10).abs() < 1e-6);
        assert_eq!(pan_masks[0], 0x80);

        let mixed = mix_ym_channel_outputs_with_side_memory(
            [(0.0, 0.0); 6],
            &mut [0.0; 6],
            [0.0; 6],
            [0.0; 6],
            [0.0; 6],
            [0.0; 6],
            &mut [0.0; 6],
            &mut pan_edge_carry,
            [0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
        );

        assert!((mixed.0 - 0.10).abs() < 1e-6);
        assert!((mixed.1 + 0.10).abs() < 1e-6);
        assert!((pan_edge_carry[0] - 0.05).abs() < 1e-6);
    }

    #[test]
    fn ym_channel_pan_edge_persistence_ignores_repeat_centered_pan_state() {
        let side_memory = [0.5f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut pan_edge_carry = [0.0f32; 6];
        let mut pan_masks = [0xC0u8; 6];
        trigger_ym_channel_pan_edge_persistence(
            &side_memory,
            &mut pan_edge_carry,
            &mut pan_masks,
            [0.20, 0.0, 0.0, 0.0, 0.0, 0.0],
            0,
            0xB4,
            0xC0,
        );

        assert_eq!(pan_edge_carry, [0.0; 6]);
        assert_eq!(pan_masks[0], 0xC0);
    }

    #[test]
    fn ym_channel_pan_edge_persistence_refreshes_on_repeated_hard_pan_writes() {
        let side_memory = [0.5f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut pan_edge_carry = [0.0f32; 6];
        let mut pan_masks = [0x80u8, 0xC0, 0xC0, 0xC0, 0xC0, 0xC0];
        trigger_ym_channel_pan_edge_persistence(
            &side_memory,
            &mut pan_edge_carry,
            &mut pan_masks,
            [0.20, 0.0, 0.0, 0.0, 0.0, 0.0],
            0,
            0xB4,
            0x80,
        );

        assert!((pan_edge_carry[0] - 0.10).abs() < 1e-6);
        assert_eq!(pan_masks[0], 0x80);
    }

    #[test]
    fn ym_channel_side_transient_mix_limits_delayed_side_on_steady_tone() {
        let mut side_memory = [0.0f32; 6];
        let mut previous_side = [0.0f32; 6];
        let mut pan_edge_carry = [0.0f32; 6];
        let amounts = [0.20, 0.0, 0.0, 0.0, 0.0, 0.0];
        let transient_mixes = [1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let sign_align_mixes = [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let side_decay = [0.0f32; 6];
        let pan_decay = [0.0f32; 6];

        let first = mix_ym_channel_outputs_with_side_memory(
            [
                (0.5, -0.5),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );
        let second = mix_ym_channel_outputs_with_side_memory(
            [
                (0.5, -0.5),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );
        let third = mix_ym_channel_outputs_with_side_memory(
            [
                (0.5, -0.5),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            amounts,
            transient_mixes,
            sign_align_mixes,
            side_decay,
            &mut previous_side,
            &mut pan_edge_carry,
            pan_decay,
        );

        assert!((first.0 - 0.5).abs() < 1e-6);
        assert!((first.1 + 0.5).abs() < 1e-6);
        assert!((second.0 - 0.6).abs() < 1e-6);
        assert!((second.1 + 0.6).abs() < 1e-6);
        assert!(
            (third.0 - 0.5).abs() < 1e-6 && (third.1 + 0.5).abs() < 1e-6,
            "expected transient-fed side memory to stop reinforcing a steady tone, got {third:?}"
        );
    }

    #[test]
    fn ym_channel_side_sign_align_follows_current_polarity_instead_of_replaying_old_phase() {
        let mut side_memory = [0.5f32, 0.0, 0.0, 0.0, 0.0, 0.0];
        let mut previous_side = [0.0f32; 6];
        let mut pan_edge_carry = [0.0f32; 6];
        let mixed = mix_ym_channel_outputs_with_side_memory(
            [
                (-0.5, 0.5),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
                (0.0, 0.0),
            ],
            &mut side_memory,
            [0.20, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.0; 6],
            [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [0.0; 6],
            &mut previous_side,
            &mut pan_edge_carry,
            [0.0; 6],
        );

        assert!(
            (mixed.0 + 0.6).abs() < 1e-6 && (mixed.1 - 0.6).abs() < 1e-6,
            "expected sign-aligned side memory to widen the current polarity instead of canceling it, got {mixed:?}"
        );
    }

    #[test]
    fn load_rom_reads_vectors() {
        let mut rom = vec![0u8; 1024];
        // SSP = 0x00FF_FFF0 at address 0
        rom[0..4].copy_from_slice(&0x00FF_FFF0u32.to_be_bytes());
        // PC = 0x0000_0200 at address 4
        rom[4..8].copy_from_slice(&0x0000_0200u32.to_be_bytes());
        // System type for header validation
        rom[0x100..0x110].copy_from_slice(b"SEGA GENESIS    ");

        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom));

        assert_eq!(core.cpu.ssp, 0x00FF_FFF0);
        assert_eq!(core.cpu.pc, 0x0000_0200);
    }

    #[test]
    fn step_frame_increments_counter() {
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(vec![0; 1024]));
        core.execute(Command::StepFrame);
        assert_eq!(core.frame_count(), 1);
    }

    #[test]
    fn pause_prevents_frame_execution() {
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(vec![0; 1024]));
        core.execute(Command::Pause);
        core.execute(Command::StepFrame);
        assert_eq!(core.frame_count(), 0);
    }

    #[test]
    fn work_ram_read_write() {
        let mut core = GenesisCore::new();
        core.work_ram[0x1234] = 0xAB;
        assert_eq!(core.read_byte(0xFF1234), 0xAB);
    }

    #[test]
    fn io_version_register_returns_region() {
        let core = GenesisCore::new();
        let val = core.read_byte(0xA10001);
        // Should be 0xA0 (overseas NTSC), not controller data (0x7F)
        assert_eq!(val, 0xA0);
    }

    #[test]
    fn z80_bus_request_grants_immediately() {
        let core = GenesisCore::new();
        // Z80 bus request: bit 0 = 0 means bus granted to 68K
        let val = core.read_byte(0xA11100);
        assert_eq!(val & 0x01, 0x00, "bit 0 should be 0 (bus granted)");
    }

    #[test]
    fn power_cycle_resets_z80() {
        let mut core = GenesisCore::new();
        core.z80.pc = 0x1234;
        core.z80_ram[0] = 0xFF;
        core.execute(Command::PowerCycle);
        assert_eq!(core.z80.pc, 0);
        assert_eq!(core.z80_ram[0], 0);
    }

    #[test]
    fn timed_ym2612_trace_records_68k_bus_writes() {
        let mut core = GenesisCore::new();
        {
            let mut bus = core_bus_with_master_tick(&mut core, 1_234);
            bus.write_byte(0xA04000, 0x28);
            bus.write_byte(0xA04001, 0x30);
            bus.write_byte(0xA04002, 0x2A);
            bus.write_byte(0xA04003, 0x7F);
        }

        assert_eq!(
            core.ym2612_timed_write_trace(),
            &[
                TimedYm2612Write {
                    master_tick: 1_234,
                    frame: 0,
                    scanline: 0,
                    port: 0,
                    addr: 0x28,
                    value: 0x30,
                },
                TimedYm2612Write {
                    master_tick: 1_234,
                    frame: 0,
                    scanline: 0,
                    port: 1,
                    addr: 0x2A,
                    value: 0x7F,
                },
            ]
        );
    }

    #[test]
    fn timed_ym2612_trace_records_z80_bus_writes_and_can_clear() {
        let mut core = GenesisCore::new();
        {
            let mut bus = z80_bus_with_master_tick(&mut core, 9_876);
            bus.write_byte(0x4000, 0x2B);
            bus.write_byte(0x4001, 0x80);
        }

        assert_eq!(
            core.ym2612_timed_write_trace(),
            &[TimedYm2612Write {
                master_tick: 9_876,
                frame: 0,
                scanline: 0,
                port: 0,
                addr: 0x2B,
                value: 0x80,
            }]
        );

        core.clear_ym2612_timed_write_trace();
        assert!(core.ym2612_timed_write_trace().is_empty());
    }

    #[test]
    fn timed_psg_trace_records_68k_and_z80_writes() {
        let mut core = GenesisCore::new();
        {
            let mut bus = core_bus_with_master_tick(&mut core, 2_468);
            bus.write_byte(0xC00011, 0x9F);
        }
        {
            let mut bus = z80_bus_with_master_tick(&mut core, 8_642);
            bus.write_byte(0x7F11, 0xE4);
        }

        assert_eq!(
            core.psg_timed_write_trace(),
            &[
                TimedPsgWrite {
                    master_tick: 2_468,
                    frame: 0,
                    scanline: 0,
                    value: 0x9F,
                },
                TimedPsgWrite {
                    master_tick: 8_642,
                    frame: 0,
                    scanline: 0,
                    value: 0xE4,
                },
            ]
        );

        core.clear_psg_timed_write_trace();
        assert!(core.psg_timed_write_trace().is_empty());
    }

    #[test]
    fn live_audio_resampler_matches_accumulated_reference() {
        let mut core = GenesisCore::new();
        core.audio_sample_rate = 44_100.0;
        core.execute(Command::SetAudioOutputConfig(
            AudioOutputConfig::model1_va2(),
        ));
        configure_two_op_fm_tone(&mut core.ym2612);
        core.audio_ym2612 = core.ym2612.clone();

        while core.audio_buffer.len() < 4096 * 2 {
            core.collect_audio_samples();
        }

        let actual = core.audio_buffer[..4096 * 2].to_vec();
        let expected = render_reference_fm_audio(4096);

        let corr = cross_correlation(&actual, &expected);
        let rms_ratio = rms(&actual) / rms(&expected).max(1e-9);

        assert!(
            corr > 0.995,
            "live resampler correlation too low: {corr:.6}"
        );
        assert!(
            (0.98..=1.02).contains(&rms_ratio),
            "live resampler RMS ratio out of range: {rms_ratio:.6}"
        );
    }

    #[test]
    fn first_order_low_pass_heavily_attenuates_ultrasonic_input() {
        let mut filter = FirstOrderLowPassFilter::new(PSG_LPF_B0, PSG_LPF_B1, PSG_LPF_A1);
        let mut input = Vec::with_capacity(4096);
        let mut output = Vec::with_capacity(4096);

        for idx in 0..4096 {
            let sample = if idx % 2 == 0 { 1.0 } else { -1.0 };
            input.push(sample);
            output.push(filter.filter(sample));
        }

        let input_rms = rms(&input[512..]);
        let output_rms = rms(&output[512..]);

        assert!(
            output_rms < input_rms * 0.02,
            "expected native low-pass to crush Nyquist-ish energy, got ratio {:.4}",
            output_rms / input_rms.max(1e-9)
        );
    }

    #[test]
    fn fir_filter_matches_impulse_response() {
        let mut filter = AudioFilterState::from_spec(AudioFilterSpec::Fir {
            taps: [0.84, 0.12, 0.04, 0.0, 0.0],
        });
        let impulse = [1.0, 0.0, 0.0, 0.0, 0.0];
        let output: Vec<f32> = impulse
            .iter()
            .map(|&sample| filter.filter(sample))
            .collect();

        assert!((output[0] - 0.84).abs() < 1e-6);
        assert!((output[1] - 0.12).abs() < 1e-6);
        assert!((output[2] - 0.04).abs() < 1e-6);
        assert!(output[3].abs() < 1e-6);
        assert!(output[4].abs() < 1e-6);
    }

    #[test]
    fn sample_delay_outputs_prior_samples() {
        let mut delay = SampleDelay::new(2);
        let input = [1.0, 0.5, -0.25, 0.75];
        let output: Vec<f32> = input.iter().map(|&sample| delay.filter(sample)).collect();

        assert!(output[0].abs() < 1e-6);
        assert!(output[1].abs() < 1e-6);
        assert!((output[2] - 1.0).abs() < 1e-6);
        assert!((output[3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mid_side_gain_reduces_stereo_width() {
        let (mid, side) = encode_mid_side(1.0, -1.0);
        let (left, right) = decode_mid_side(mid, side * 0.5);

        assert!((left - 0.5).abs() < 1e-6);
        assert!((right + 0.5).abs() < 1e-6);
    }

    fn filtered_sine_rms(spec: AudioFilterSpec, frequency_hz: f32, sample_rate_hz: f32) -> f32 {
        let mut filter = AudioFilterState::from_spec(spec);
        let mut output = Vec::with_capacity(8192);
        for idx in 0..8192 {
            let phase = 2.0 * std::f32::consts::PI * frequency_hz * idx as f32 / sample_rate_hz;
            output.push(filter.filter(phase.sin()));
        }
        rms(&output[2048..])
    }

    #[test]
    fn eq_stages_shape_frequency_response() {
        let sample_rate_hz = 44_100.0;

        let low_shelf = biquad_eq_filter_spec(AudioEqStage::low_shelf(110.0, -8.0), sample_rate_hz);
        let low_shelf_80 = filtered_sine_rms(low_shelf, 80.0, sample_rate_hz);
        let low_shelf_500 = filtered_sine_rms(low_shelf, 500.0, sample_rate_hz);
        assert!(
            low_shelf_80 < low_shelf_500 * 0.7,
            "expected low shelf to cut bass more than mids, got 80Hz {:.4} vs 500Hz {:.4}",
            low_shelf_80,
            low_shelf_500
        );

        let peaking =
            biquad_eq_filter_spec(AudioEqStage::peaking(420.0, 0.75, 6.0), sample_rate_hz);
        let peaking_420 = filtered_sine_rms(peaking, 420.0, sample_rate_hz);
        let peaking_1400 = filtered_sine_rms(peaking, 1_400.0, sample_rate_hz);
        assert!(
            peaking_420 > peaking_1400 * 1.5,
            "expected peaking EQ to favor center band, got 420Hz {:.4} vs 1400Hz {:.4}",
            peaking_420,
            peaking_1400
        );

        let high_shelf =
            biquad_eq_filter_spec(AudioEqStage::high_shelf(2_600.0, -3.5), sample_rate_hz);
        let high_shelf_700 = filtered_sine_rms(high_shelf, 700.0, sample_rate_hz);
        let high_shelf_6000 = filtered_sine_rms(high_shelf, 6_000.0, sample_rate_hz);
        assert!(
            high_shelf_6000 < high_shelf_700 * 0.85,
            "expected high shelf to cut treble more than mids, got 700Hz {:.4} vs 6kHz {:.4}",
            high_shelf_700,
            high_shelf_6000
        );
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let rom = vec![0u8; 0x8000];
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom));

        // Advance to a non-trivial state.
        for _ in 0..10 {
            core.execute(Command::StepFrame);
        }
        let snap = core.snapshot();

        // Run forward and capture the reference framebuffer.
        for _ in 0..10 {
            core.execute(Command::StepFrame);
        }
        let fb_forward = core.framebuffer_rgba().to_vec();
        let snap_forward = core.snapshot();

        // Restore and replay the same frames — must reproduce exactly.
        core.restore(&snap);
        assert_eq!(core.frame_count(), snap.frame_count);
        for _ in 0..10 {
            core.execute(Command::StepFrame);
        }
        let fb_replay = core.framebuffer_rgba().to_vec();

        assert_eq!(fb_forward, fb_replay, "framebuffers diverged after restore");
        assert!(
            core.snapshot() == snap_forward,
            "snapshot state diverged after restore+replay"
        );
    }

    /// Deterministic scripted controller input for frame `frame`.
    fn scripted_buttons(frame: u64) -> u16 {
        // A fixed, frame-dependent bit pattern. The exact semantics are
        // irrelevant — both runs receive the identical stream, so the emulator
        // must reach identical state.
        let mut b = 0u16;
        if frame % 2 == 0 {
            b |= 0x0001;
        }
        if frame % 3 == 0 {
            b |= 0x0010;
        }
        if frame % 5 == 0 {
            b |= 0x0020;
        }
        if frame % 7 == 0 {
            b |= 0x0008;
        }
        b
    }

    #[test]
    fn determinism_frame_to_frame() {
        let rom = vec![0u8; 0x8000];
        let mut a = GenesisCore::new();
        let mut b = GenesisCore::new();
        a.execute(Command::LoadRom(rom.clone()));
        b.execute(Command::LoadRom(rom));

        for frame in 0..120u64 {
            let buttons = scripted_buttons(frame);
            a.execute(Command::SetControllerState { port: 0, buttons });
            b.execute(Command::SetControllerState { port: 0, buttons });
            a.execute(Command::StepFrame);
            b.execute(Command::StepFrame);
        }

        let sa = a.snapshot();
        let sb = b.snapshot();
        assert!(sa == sb, "snapshot state diverged between identical runs");
        // Byte-equal serialized snapshots.
        let ja = serde_json::to_vec(&sa).unwrap();
        let jb = serde_json::to_vec(&sb).unwrap();
        assert_eq!(ja, jb, "serialized snapshots are not byte-equal");
        // Framebuffers identical.
        assert_eq!(
            a.framebuffer_rgba(),
            b.framebuffer_rgba(),
            "framebuffers diverged between identical runs"
        );
    }

    #[test]
    fn rewind_roundtrip() {
        const F: u64 = 120;
        const R: u32 = 30;
        let rom = vec![0u8; 0x8000];

        // Core under test (rewind enabled by default).
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom.clone()));
        // Reference core that never rewinds.
        let mut reference = GenesisCore::new();
        reference.execute(Command::LoadRom(rom));

        for frame in 0..F {
            let buttons = scripted_buttons(frame);
            core.execute(Command::SetControllerState { port: 0, buttons });
            reference.execute(Command::SetControllerState { port: 0, buttons });
            core.execute(Command::StepFrame);
            reference.execute(Command::StepFrame);
        }

        let ref_snap = reference.snapshot();
        let ref_fb = reference.framebuffer_rgba().to_vec();
        assert_eq!(core.frame_count(), F);

        // Rewind, then replay the same input stream for the abandoned frames.
        core.execute(Command::Rewind { frames: R });
        assert_eq!(core.frame_count(), F - u64::from(R));
        for frame in (F - u64::from(R))..F {
            let buttons = scripted_buttons(frame);
            core.execute(Command::SetControllerState { port: 0, buttons });
            core.execute(Command::StepFrame);
        }
        assert_eq!(core.frame_count(), F);

        assert!(
            core.snapshot() == ref_snap,
            "state after rewind+replay differs from the never-rewound reference"
        );
        assert_eq!(
            serde_json::to_vec(&core.snapshot()).unwrap(),
            serde_json::to_vec(&ref_snap).unwrap(),
            "serialized state after rewind+replay is not byte-equal to reference"
        );
        assert_eq!(
            core.framebuffer_rgba(),
            ref_fb.as_slice(),
            "framebuffer after rewind+replay differs from reference"
        );
    }

    /// Reports snapshot size, capture cost, and naive-vs-delta memory.
    /// Run with `cargo test -p genesoxide-core rewind_measurements -- --nocapture`.
    #[test]
    fn rewind_measurements() {
        let rom = vec![0u8; 0x8000];
        let mut core = GenesisCore::new();
        core.execute(Command::LoadRom(rom));

        // Warm up to a representative state.
        for _ in 0..120 {
            core.execute(Command::StepFrame);
        }

        let snap = core.snapshot();
        let json_bytes = serde_json::to_vec(&snap).unwrap().len();
        let bin_bytes = bincode::serialize(&snap).unwrap().len();

        // Time snapshot capture.
        let iters = 200;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let s = core.snapshot();
            std::hint::black_box(&s);
        }
        let per_capture = start.elapsed() / iters;

        // Fill a full 30s window (default config = 1800 frames) and measure the
        // compressed timeline against a naive full-snapshot ring buffer.
        const WINDOW_FRAMES: u64 = 1800;
        for _ in 0..WINDOW_FRAMES {
            core.execute(Command::StepFrame);
        }
        let delta_bytes = core.rewind_memory_used();
        let naive_bytes = (bin_bytes as u64) * WINDOW_FRAMES;

        eprintln!("=== rewind measurements (synthetic 0x8000 ROM) ===");
        eprintln!(
            "snapshot serialized size: {} bytes (bincode), {} bytes (serde_json)",
            bin_bytes, json_bytes
        );
        eprintln!("snapshot capture time: {:?} per snapshot", per_capture);
        eprintln!(
            "naive ring buffer (30s @ 60fps = {} frames x {} B): {} bytes ({:.2} MB)",
            WINDOW_FRAMES,
            bin_bytes,
            naive_bytes,
            naive_bytes as f64 / (1024.0 * 1024.0)
        );
        eprintln!(
            "anchor+delta timeline actual: {} bytes ({:.2} MB), frames_available={}",
            delta_bytes,
            delta_bytes as f64 / (1024.0 * 1024.0),
            core.rewind_frames_available()
        );
        eprintln!(
            "compression ratio: {:.1}x",
            naive_bytes as f64 / delta_bytes.max(1) as f64
        );

        assert!(delta_bytes > 0);
        assert!((delta_bytes as u64) < naive_bytes);
    }
}

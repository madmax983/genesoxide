//! Test-only YM2612 reference renderer backed by ymfm.

use crate::vgm::{
    Vgm, VgmCommand, VgmRenderer, cross_correlation, left_channel, peak, right_channel, rms,
};
use std::ffi::c_void;
use std::ptr::NonNull;

/// YM2612 reference renderer backed by ymfm.
pub struct Ymfm2612Renderer {
    ym: Ymfm2612,
    output_rate: f64,
    ym_native_rate: f64,
    ym_phase: f64,
}

pub struct YmfmComparison {
    pub samples: usize,
    pub correlation_left: f32,
    pub correlation_right: f32,
    pub rms_ratio_left: f32,
    pub peak_ratio_left: f32,
}

struct Ymfm2612 {
    raw: NonNull<c_void>,
}

unsafe extern "C" {
    fn ymfm2612_create() -> *mut c_void;
    fn ymfm2612_destroy(handle: *mut c_void);
    fn ymfm2612_reset(handle: *mut c_void);
    fn ymfm2612_sample_rate(handle: *mut c_void, input_clock: u32) -> u32;
    fn ymfm2612_write(handle: *mut c_void, port: u8, reg: u8, value: u8);
    fn ymfm2612_generate(handle: *mut c_void, left: *mut i32, right: *mut i32);
}

impl Ymfm2612Renderer {
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(7_670_453)
    }

    #[must_use]
    pub fn with_clock(ym_clock: u32) -> Self {
        let ym = Ymfm2612::new();
        let ym_native_rate = ym.sample_rate(ym_clock) as f64;

        Self {
            ym,
            output_rate: 44_100.0,
            ym_native_rate,
            ym_phase: 0.0,
        }
    }

    pub fn render(&mut self, vgm: &Vgm) -> Vec<f32> {
        self.ym.reset();
        self.ym_phase = 0.0;

        let capacity = (vgm.header.total_samples as usize + 1024) * 2;
        let mut output = Vec::with_capacity(capacity);

        for cmd in &vgm.commands {
            match *cmd {
                VgmCommand::Ym2612Port0 { reg, val } => {
                    self.ym.write(0, reg, val);
                }
                VgmCommand::Ym2612Port1 { reg, val } => {
                    self.ym.write(1, reg, val);
                }
                VgmCommand::Psg { .. } => {}
                VgmCommand::DacWrite { offset, wait } => {
                    if let Some(&byte) = vgm.dac_data.get(offset as usize) {
                        self.ym.write(0, 0x2A, byte);
                    }
                    if wait > 0 {
                        self.render_samples(wait as u32, &mut output);
                    }
                }
                VgmCommand::Wait { samples } => {
                    self.render_samples(samples as u32, &mut output);
                }
                VgmCommand::End => break,
                VgmCommand::Unknown { .. } => {}
            }
        }

        output
    }

    fn render_samples(&mut self, count: u32, output: &mut Vec<f32>) {
        let ym_ratio = self.ym_native_rate / self.output_rate;

        for _ in 0..count {
            self.ym_phase += ym_ratio;

            let mut left_acc = 0.0f64;
            let mut right_acc = 0.0f64;
            let mut ym_count = 0u32;

            while self.ym_phase >= 1.0 {
                self.ym_phase -= 1.0;
                let (left, right) = self.ym.generate_sample();
                left_acc += f64::from(left);
                right_acc += f64::from(right);
                ym_count += 1;
            }

            let (left, right) = if ym_count > 0 {
                (
                    (left_acc / ym_count as f64 / 32_768.0) as f32,
                    (right_acc / ym_count as f64 / 32_768.0) as f32,
                )
            } else {
                (0.0, 0.0)
            };

            output.push(left.clamp(-1.0, 1.0));
            output.push(right.clamp(-1.0, 1.0));
        }
    }

    pub fn compare_against_genesoxide(vgm: &Vgm) -> YmfmComparison {
        let mut genesoxide = VgmRenderer::with_clock(vgm.header.ym2612_clock);
        let mut ymfm = Ymfm2612Renderer::with_clock(vgm.header.ym2612_clock);

        let genesoxide_samples = genesoxide.render(vgm);
        let ymfm_samples = ymfm.render(vgm);

        let left_genesoxide = left_channel(&genesoxide_samples);
        let right_genesoxide = right_channel(&genesoxide_samples);
        let left_ymfm = left_channel(&ymfm_samples);
        let right_ymfm = right_channel(&ymfm_samples);

        let n = left_genesoxide.len().min(left_ymfm.len());
        let left_corr = cross_correlation(&left_genesoxide[..n], &left_ymfm[..n]);
        let right_corr = cross_correlation(&right_genesoxide[..n], &right_ymfm[..n]);
        let genesoxide_rms = rms(&left_genesoxide[..n]);
        let ymfm_rms = rms(&left_ymfm[..n]);
        let genesoxide_peak = peak(&left_genesoxide[..n]);
        let ymfm_peak = peak(&left_ymfm[..n]);

        YmfmComparison {
            samples: n,
            correlation_left: left_corr,
            correlation_right: right_corr,
            rms_ratio_left: if ymfm_rms > 1e-6 {
                genesoxide_rms / ymfm_rms
            } else {
                0.0
            },
            peak_ratio_left: if ymfm_peak > 1e-6 {
                genesoxide_peak / ymfm_peak
            } else {
                0.0
            },
        }
    }
}

impl Ymfm2612 {
    fn new() -> Self {
        let raw = unsafe { ymfm2612_create() };
        let raw = NonNull::new(raw).expect("failed to create ymfm ym2612");
        Self { raw }
    }

    fn reset(&mut self) {
        unsafe { ymfm2612_reset(self.raw.as_ptr()) };
    }

    fn sample_rate(&self, input_clock: u32) -> u32 {
        unsafe { ymfm2612_sample_rate(self.raw.as_ptr(), input_clock) }
    }

    fn write(&mut self, port: u8, reg: u8, value: u8) {
        unsafe { ymfm2612_write(self.raw.as_ptr(), port, reg, value) };
    }

    fn generate_sample(&mut self) -> (i32, i32) {
        let mut left = 0;
        let mut right = 0;
        unsafe {
            ymfm2612_generate(self.raw.as_ptr(), &mut left, &mut right);
        }
        (left, right)
    }
}

impl Drop for Ymfm2612 {
    fn drop(&mut self) {
        unsafe { ymfm2612_destroy(self.raw.as_ptr()) };
    }
}

impl Default for Ymfm2612Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::VgmBuilder;
    use crate::vgm::{estimate_frequency, save_wav, zero_crossings};
    use std::path::Path;

    fn steady_window(samples: &[f32]) -> &[f32] {
        let start = 500.min(samples.len());
        let end = samples.len().saturating_sub(4_410).max(start);
        &samples[start..end]
    }

    #[test]
    fn ymfm_renderer_matches_vgm_sample_count() {
        let vgm = VgmBuilder::new().wait(4410).build();
        let mut renderer = Ymfm2612Renderer::new();
        let samples = renderer.render(&vgm);
        assert_eq!(samples.len(), 8820);
    }

    #[test]
    fn ymfm_renderer_produces_audible_single_operator_tone() {
        let vgm = VgmBuilder::new()
            .single_op_tone(541, 4, 0, 31)
            .wait(44100)
            .ym_write(0, 0x28, 0x00)
            .wait(4410)
            .build();

        let mut renderer = Ymfm2612Renderer::new();
        let samples = renderer.render(&vgm);
        let left = left_channel(&samples);
        let rms_val = rms(&left);
        let peak_val = peak(&left);

        assert!(
            rms_val > 0.001,
            "reference tone should be audible, got RMS {rms_val:.6}"
        );
        assert!(
            peak_val > 0.01,
            "reference tone peak too low: {peak_val:.6}"
        );
    }

    #[test]
    fn compare_genesoxide_against_ymfm_reports_metrics() {
        let vgm = VgmBuilder::new()
            .two_op_fm(653, 4, 0, 0, 1, 1, 0)
            .wait(22_050)
            .build();

        let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);

        assert_eq!(metrics.samples, 22_050);
        assert!(metrics.rms_ratio_left.is_finite());
        assert!(metrics.peak_ratio_left.is_finite());
    }

    #[test]
    fn carrier_only_two_op_pair_matches_ymfm() {
        let vgm = VgmBuilder::new()
            .two_op_fm(653, 4, 127, 0, 1, 1, 0)
            .wait(22_050)
            .build();

        let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
        eprintln!(
            "carrier_only_two_op_pair: corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}",
            metrics.correlation_left,
            metrics.correlation_right,
            metrics.rms_ratio_left,
            metrics.peak_ratio_left,
        );

        assert!(metrics.correlation_left > 0.95);
        assert!((0.8..=1.2).contains(&metrics.rms_ratio_left));
    }

    #[test]
    fn op2_carrier_only_matches_ymfm() {
        let vgm = VgmBuilder::new()
            .ym_write(0, 0xB0, 0x07)
            .ym_write(0, 0xB4, 0xC0)
            .ym_write(0, 0x40, 127)
            .ym_write(0, 0x44, 127)
            .ym_write(0, 0x48, 127)
            .ym_write(0, 0x4C, 127)
            .ym_write(0, 0x38, 0x01)
            .ym_write(0, 0x48, 0x00)
            .ym_write(0, 0x58, 31)
            .ym_write(0, 0x68, 0)
            .ym_write(0, 0x78, 0)
            .ym_write(0, 0x88, 0x0F)
            .ym_write(0, 0xA4, (4 << 3) | ((1083 >> 8) as u8 & 0x07))
            .ym_write(0, 0xA0, (1083 & 0xFF) as u8)
            // Slot 2 in register space is op2, but $28 key-on uses operator order.
            .ym_write(0, 0x28, 0x20)
            .wait(22_050)
            .build();

        let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
        eprintln!(
            "op2_carrier_only: corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}",
            metrics.correlation_left,
            metrics.correlation_right,
            metrics.rms_ratio_left,
            metrics.peak_ratio_left,
        );

        assert!(metrics.correlation_left > 0.95);
        assert!((0.8..=1.2).contains(&metrics.rms_ratio_left));
    }

    #[test]
    fn dac_square_wave_matches_ymfm() {
        let mut builder = VgmBuilder::new()
            .ym_write(0, 0x2B, 0x80)
            .ym_write(1, 0xB6, 0xC0);

        for _ in 0..256 {
            builder = builder
                .ym_write(0, 0x2A, 0xFF)
                .wait(32)
                .ym_write(0, 0x2A, 0x00)
                .wait(32);
        }

        let vgm = builder.build();
        let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
        eprintln!(
            "dac_square_wave: corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}",
            metrics.correlation_left,
            metrics.correlation_right,
            metrics.rms_ratio_left,
            metrics.peak_ratio_left,
        );

        assert!(metrics.correlation_left > 0.95);
        assert!((0.8..=1.2).contains(&metrics.rms_ratio_left));
    }

    /// Build a single-carrier tone driving the SSG-EG in a looping configuration:
    /// instant attack, moderate decay toward a high sustain level, and a fast
    /// sustain climb, so the attenuation repeatedly crosses the 0x200 SSG midpoint
    /// and the SSG-EG state machine actually cycles.
    fn ssg_eg_looping_vgm(mode: u8) -> crate::vgm::Vgm {
        let (fnum, block) = (1083u16, 4u8);
        let fnum_hi = ((block & 7) << 3) | ((fnum >> 8) & 0x07) as u8;
        let fnum_lo = (fnum & 0xFF) as u8;
        VgmBuilder::new()
            .ym_write(0, 0xB0, 0x07) // algorithm 7 (all operators are carriers)
            .ym_write(0, 0xB4, 0xC0) // L + R
            .ym_write(0, 0x40, 127)
            .ym_write(0, 0x44, 127)
            .ym_write(0, 0x48, 127)
            .ym_write(0, 0x4C, 127)
            .ym_write(0, 0x30, 0x01) // op1 MUL=1
            .ym_write(0, 0x40, 0x00) // op1 TL=0
            .ym_write(0, 0x50, 0x1F) // AR=31 (instant attack)
            .ym_write(0, 0x60, 0x10) // DR=16
            .ym_write(0, 0x70, 0x1F) // SR=31 (fast sustain climb)
            .ym_write(0, 0x80, 0xF0) // SL=15, RR=0
            .ym_write(0, 0x90, mode) // SSG-EG mode
            .ym_write(0, 0xA4, fnum_hi)
            .ym_write(0, 0xA0, fnum_lo)
            .ym_write(0, 0x28, 0x10) // key on op1
            .wait(22_050)
            .build()
    }

    /// Single sustained carrier with LFO phase modulation (vibrato) at a given
    /// PM sensitivity, used to check vibrato depth/shape against ymfm.
    fn lfo_pm_vgm(pms: u8, lfo_freq: u8) -> crate::vgm::Vgm {
        let (fnum, block) = (1083u16, 4u8);
        let fnum_hi = ((block & 7) << 3) | ((fnum >> 8) & 0x07) as u8;
        let fnum_lo = (fnum & 0xFF) as u8;
        VgmBuilder::new()
            .ym_write(0, 0x22, 0x08 | (lfo_freq & 0x07)) // LFO enable + rate
            .ym_write(0, 0xB0, 0x07)
            .ym_write(0, 0xB4, 0xC0 | (pms & 0x07)) // L+R, PMS
            .ym_write(0, 0x40, 127)
            .ym_write(0, 0x44, 127)
            .ym_write(0, 0x48, 127)
            .ym_write(0, 0x4C, 127)
            .ym_write(0, 0x30, 0x01)
            .ym_write(0, 0x40, 0x00)
            .ym_write(0, 0x50, 0x1F)
            .ym_write(0, 0x60, 0x00)
            .ym_write(0, 0x70, 0x00)
            .ym_write(0, 0x80, 0x00) // sustain forever
            .ym_write(0, 0xA4, fnum_hi)
            .ym_write(0, 0xA0, fnum_lo)
            .ym_write(0, 0x28, 0x10)
            .wait(44_100)
            .build()
    }

    #[test]
    fn lfo_pm_depth_matches_ymfm() {
        // Regression for LFO phase modulation (vibrato), which was 2x too deep and
        // badly decorrelated at high PM sensitivity (PMS 7 was corr ~0.05).
        for &(pms, min_corr) in &[(3u8, 0.97f32), (5, 0.97), (6, 0.93), (7, 0.85)] {
            let vgm = lfo_pm_vgm(pms, 4);
            let m = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
            eprintln!(
                "lfo_pm pms={pms}: corr={:.4} rms_ratio={:.4}",
                m.correlation_left, m.rms_ratio_left
            );
            assert!(
                m.correlation_left > min_corr,
                "PM pms={pms} correlation {:.4} below {:.2} (vibrato depth/shape wrong)",
                m.correlation_left,
                min_corr
            );
            assert!(
                (0.8..=1.2).contains(&m.rms_ratio_left),
                "PM pms={pms} level off vs ymfm: rms_ratio={:.4}",
                m.rms_ratio_left
            );
        }
    }

    #[test]
    fn ssg_eg_modes_match_ymfm() {
        // Regression for the SSG-EG "attack"/invert modes (12-15, i.e. register
        // values 0x0C-0x0F) which were previously near-silent (rms ~0.03x ymfm).
        // All eight enabled SSG-EG modes should now track ymfm in level and shape.
        for mode in 0x08u8..=0x0F {
            let vgm = ssg_eg_looping_vgm(mode);
            let m = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
            eprintln!(
                "ssg_eg mode {mode:#04x}: corr={:.4} rms_ratio={:.4} peak_ratio={:.4}",
                m.correlation_left, m.rms_ratio_left, m.peak_ratio_left
            );
            assert!(
                m.correlation_left > 0.95,
                "SSG mode {mode:#04x} shape diverged: corr={:.4}",
                m.correlation_left
            );
            // The attack modes must no longer collapse to silence; every mode should
            // land near the ~0.93x per-channel level ymfm produces.
            assert!(
                m.rms_ratio_left > 0.5,
                "SSG mode {mode:#04x} too quiet vs ymfm: rms_ratio={:.4}",
                m.rms_ratio_left
            );
            assert!(
                (0.8..=1.2).contains(&m.peak_ratio_left),
                "SSG mode {mode:#04x} peak off vs ymfm: peak_ratio={:.4}",
                m.peak_ratio_left
            );
        }
    }

    #[test]
    #[ignore]
    fn dump_genesoxide_vs_ymfm_reference_cases() {
        let output_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("vgm_output")
            .join("ymfm_compare");
        std::fs::create_dir_all(&output_dir).expect("failed to create ymfm compare output dir");

        let cases = [
            (
                "single_operator_tone",
                VgmBuilder::new()
                    .single_op_tone(1083, 4, 0, 31)
                    .wait(44_100)
                    .ym_write(0, 0x28, 0x00)
                    .wait(4_410)
                    .build(),
            ),
            (
                "two_operator_fm",
                VgmBuilder::new()
                    .two_op_fm(653, 4, 0, 0, 1, 1, 0)
                    .wait(22_050)
                    .build(),
            ),
        ];

        for (name, vgm) in cases {
            let mut genesoxide = VgmRenderer::with_clock(vgm.header.ym2612_clock);
            let mut ymfm = Ymfm2612Renderer::with_clock(vgm.header.ym2612_clock);

            let genesoxide_samples = genesoxide.render(&vgm);
            let ymfm_samples = ymfm.render(&vgm);
            let metrics = Ymfm2612Renderer::compare_against_genesoxide(&vgm);
            let genesoxide_left = left_channel(&genesoxide_samples);
            let ymfm_left = left_channel(&ymfm_samples);
            let genesoxide_steady = steady_window(&genesoxide_left);
            let ymfm_steady = steady_window(&ymfm_left);
            let genesoxide_freq = estimate_frequency(genesoxide_steady, 44_100);
            let ymfm_freq = estimate_frequency(ymfm_steady, 44_100);
            let genesoxide_zc = zero_crossings(genesoxide_steady);
            let ymfm_zc = zero_crossings(ymfm_steady);

            save_wav(
                &output_dir.join(format!("{name}-genesoxide.wav")),
                &genesoxide_samples,
                44_100,
            )
            .expect("failed to save genesoxide comparison wav");
            save_wav(
                &output_dir.join(format!("{name}-ymfm.wav")),
                &ymfm_samples,
                44_100,
            )
            .expect("failed to save ymfm comparison wav");

            eprintln!(
                "{name}: samples={}, corr(L/R)=({:.4}, {:.4}), rms_ratio_left={:.4}, peak_ratio_left={:.4}, freq(genesoxide/ymfm)=({:.1}, {:.1}), zc(genesoxide/ymfm)=({}, {})",
                metrics.samples,
                metrics.correlation_left,
                metrics.correlation_right,
                metrics.rms_ratio_left,
                metrics.peak_ratio_left,
                genesoxide_freq,
                ymfm_freq,
                genesoxide_zc,
                ymfm_zc,
            );
        }
    }
}

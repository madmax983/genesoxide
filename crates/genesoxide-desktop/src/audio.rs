//! Audio output via cpal with a lock-free ring buffer and light rate matching.
//!
//! The emulator core generates audio per emulated frame (~59.92 Hz), paced by
//! the wall clock, and pushes it into an SPSC ring buffer. cpal pops samples in
//! its callback at the *device* clock rate. Those two clocks are independent, so
//! without correction they drift: the ring slowly drains (underrun -> a silence
//! gap then a refill, heard as an occasional delay) or fills (overflow -> dropped
//! samples). To keep them locked we measure the ring's fill level each frame and
//! apply a small, clamped resample ratio to the produced block so the producer's
//! effective rate tracks the consumer. The correction is tiny (<=2%, ~34 cents)
//! and proportional to the fill error, so it converges without audible pitch
//! wobble. Underruns (callback) and overflow drops (push) are counted for
//! diagnostics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

/// Target amount of audio kept buffered, i.e. the output latency we aim to hold.
const TARGET_LATENCY_MS: u32 = 60;
/// Ring capacity in milliseconds of audio. Larger than the target so the fill can
/// swing above and below target without over/underflowing under normal jitter.
const CAPACITY_LATENCY_MS: u32 = 120;
/// Floor on ring capacity (stereo f32 samples) for very low device rates.
const MIN_CAPACITY_SAMPLES: usize = 2048;
/// Maximum resample correction (fraction). 2% ~= 34 cents; below audible wobble.
const MAX_CORRECTION: f32 = 0.02;
/// Proportional gain from normalized fill error to correction.
const CORRECTION_GAIN: f32 = 0.5;

/// Ring-buffer capacity (in interleaved-stereo f32 samples) for a device rate.
#[must_use]
pub fn ring_capacity_samples(sample_rate: u32) -> usize {
    let frames = sample_rate as usize * CAPACITY_LATENCY_MS as usize / 1000;
    (frames * 2).max(MIN_CAPACITY_SAMPLES)
}

/// Target ring fill (interleaved-stereo f32 samples) for a device rate.
#[must_use]
pub fn target_fill_samples(sample_rate: u32) -> usize {
    (sample_rate as usize * TARGET_LATENCY_MS as usize / 1000) * 2
}

/// Small, clamped resample ratio (output/input) that nudges the producer toward
/// the target fill. `> 1.0` stretches (emit more) when the ring is draining below
/// target; `< 1.0` shrinks (emit fewer) when it is overfull. The correction is
/// proportional to the normalized fill error and clamped to `+/- MAX_CORRECTION`,
/// so it is bounded and converges to `1.0` as fill approaches target.
#[must_use]
pub fn rate_correction(fill: usize, target: usize) -> f32 {
    if target == 0 {
        return 1.0;
    }
    // err > 0 when the buffer is below target (draining) -> stretch to refill.
    let err = (target as f32 - fill as f32) / target as f32;
    (1.0 + CORRECTION_GAIN * err).clamp(1.0 - MAX_CORRECTION, 1.0 + MAX_CORRECTION)
}

/// Stateful linear resampler for interleaved-stereo f32 with a time-varying ratio.
///
/// Maintains fractional read position and a one-frame carry across calls so
/// consecutive blocks join seamlessly (no per-block discontinuity). Output length
/// is approximately `input_frames * ratio`.
#[derive(Default)]
pub struct Resampler {
    /// Fractional read position, in input frames, relative to the current block.
    pos: f32,
    /// Last input frame of the previous block (virtual index -1 for this block).
    last: [f32; 2],
    have_last: bool,
}

impl Resampler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resamples `input` (interleaved stereo) by `ratio`, appending to `out`.
    pub fn process(&mut self, input: &[f32], ratio: f32, out: &mut Vec<f32>) {
        let n = (input.len() / 2) as i64;
        if n == 0 {
            return;
        }
        if !self.have_last {
            self.last = [input[0], input[1]];
            self.have_last = true;
        }
        let last = self.last;
        let frame = |i: i64| -> [f32; 2] {
            if i < 0 {
                last
            } else {
                // Clamp to the final input frame so the right neighbour of the last
                // sample (virtual index n) reads the last frame instead of going
                // OOB; with frac == 0 there this yields exactly `frame(n-1)`.
                let u = (i.min(n - 1) as usize) * 2;
                [input[u], input[u + 1]]
            }
        };
        // Step through input in `1/ratio` frame increments, interpolating each
        // output frame between `base` and `base + 1` (base spans -1..n-1; the
        // clamp above keeps base+1 == n reading the last frame).
        let step = 1.0 / ratio.clamp(0.5, 2.0);
        while self.pos <= (n - 1) as f32 {
            let base = self.pos.floor();
            let bi = base as i64;
            let frac = self.pos - base;
            let a = frame(bi);
            let b = frame(bi + 1);
            out.push(a[0] + (b[0] - a[0]) * frac);
            out.push(a[1] + (b[1] - a[1]) * frac);
            self.pos += step;
        }
        // Rebase position for the next block (next block's frame 0 is this block's
        // frame n) and carry this block's final frame as the next virtual -1.
        self.pos -= n as f32;
        let li = ((n - 1) as usize) * 2;
        self.last = [input[li], input[li + 1]];
    }
}

/// Snapshot of the audio path's health, for diagnostics/logging.
#[derive(Debug, Clone, Copy)]
pub struct AudioDiagnostics {
    /// Current ring fill (interleaved-stereo f32 samples).
    pub fill: usize,
    /// Ring capacity (interleaved-stereo f32 samples).
    pub capacity: usize,
    /// Target fill we rate-match toward.
    pub target_fill: usize,
    /// Total callback underruns (samples the callback could not fill from the ring).
    pub underruns: u64,
    /// Total samples dropped on push because the ring was full (overflow).
    pub dropped: u64,
    /// Most recent resample correction ratio applied.
    pub last_ratio: f32,
}

/// Manages the cpal audio stream and its producer-side ring buffer handle.
pub struct AudioOutput {
    /// Keep the stream alive; dropping it stops playback.
    _stream: cpal::Stream,
    /// Producer half of the ring buffer fed to the cpal callback.
    producer: ringbuf::HeapProd<f32>,
    /// Device sample rate in Hz.
    sample_rate: u32,
    /// Ring capacity (interleaved-stereo f32 samples).
    capacity: usize,
    /// Target ring fill we rate-match toward.
    target_fill: usize,
    /// Rate-matching resampler applied to each produced block.
    resampler: Resampler,
    /// Scratch buffer for the resampled block (reused to avoid per-frame alloc).
    scratch: Vec<f32>,
    /// Callback underrun counter, shared with the audio thread.
    underruns: Arc<AtomicU64>,
    /// Overflow (dropped-on-full) counter.
    dropped: u64,
    /// Most recent correction ratio, for diagnostics.
    last_ratio: f32,
}

impl AudioOutput {
    /// Opens the default audio device and starts a stereo stream at its native rate.
    ///
    /// Returns `None` if no output device is available or the stream fails to
    /// start (e.g. no audio hardware).
    pub fn open() -> Option<Self> {
        let host = cpal::default_host();
        eprintln!("Audio: using host {:?}", host.id());

        let device = match host.default_output_device() {
            Some(d) => {
                eprintln!("Audio: default device: {:?}", d.name().unwrap_or_default());
                d
            }
            None => {
                // Try enumerating all output devices as fallback
                eprintln!("Audio: no default output device, enumerating...");
                match host.output_devices() {
                    Ok(mut devices) => match devices.next() {
                        Some(d) => {
                            eprintln!(
                                "Audio: using fallback device: {:?}",
                                d.name().unwrap_or_default()
                            );
                            d
                        }
                        None => {
                            eprintln!("Audio: no output devices found at all");
                            return None;
                        }
                    },
                    Err(e) => {
                        eprintln!("Audio: failed to enumerate devices: {e}");
                        return None;
                    }
                }
            }
        };

        // Query device's preferred config and use its sample rate
        let default_config = match device.default_output_config() {
            Ok(c) => {
                eprintln!(
                    "Audio: device default config: {} ch, {} Hz, {:?}",
                    c.channels(),
                    c.sample_rate().0,
                    c.sample_format()
                );
                c
            }
            Err(e) => {
                eprintln!("Audio: failed to get default config: {e}");
                return None;
            }
        };

        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: default_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let sample_rate = config.sample_rate.0;
        let capacity = ring_capacity_samples(sample_rate);
        let target_fill = target_fill_samples(sample_rate);
        eprintln!(
            "Audio: opening stream at {sample_rate} Hz (ring capacity {capacity} samples \
             ~{CAPACITY_LATENCY_MS} ms, target fill {target_fill} samples ~{TARGET_LATENCY_MS} ms)"
        );

        // Ring buffer sized for the target latency window at the device rate.
        let ring = HeapRb::<f32>::new(capacity);
        let (producer, mut consumer) = ring.split();

        let underruns = Arc::new(AtomicU64::new(0));
        let cb_underruns = Arc::clone(&underruns);

        let stream = match device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut misses = 0u64;
                for sample in data.iter_mut() {
                    match consumer.try_pop() {
                        Some(s) => *sample = s,
                        None => {
                            *sample = 0.0;
                            misses += 1;
                        }
                    }
                }
                if misses > 0 {
                    cb_underruns.fetch_add(misses, Ordering::Relaxed);
                }
            },
            |err| eprintln!("Audio stream error: {err}"),
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Audio: failed to build stream: {e}");
                return None;
            }
        };

        if let Err(e) = stream.play() {
            eprintln!("Audio: failed to start playback: {e}");
            return None;
        }

        Some(Self {
            _stream: stream,
            producer,
            sample_rate,
            capacity,
            target_fill,
            resampler: Resampler::new(),
            scratch: Vec::with_capacity(capacity),
            underruns,
            dropped: 0,
            last_ratio: 1.0,
        })
    }

    /// Returns the actual sample rate of the audio device.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Current diagnostics snapshot (fill, capacity, underruns, drops, last ratio).
    #[must_use]
    pub fn diagnostics(&self) -> AudioDiagnostics {
        AudioDiagnostics {
            fill: self.producer.occupied_len(),
            capacity: self.capacity,
            target_fill: self.target_fill,
            underruns: self.underruns.load(Ordering::Relaxed),
            dropped: self.dropped,
            last_ratio: self.last_ratio,
        }
    }

    /// Pushes one emulated frame's worth of stereo interleaved f32 samples,
    /// applying light rate matching so the producer tracks the device clock.
    ///
    /// The block is resampled by a small, fill-derived ratio before being pushed.
    /// Overflow samples (ring full) are dropped and counted to bound latency.
    pub fn push_frame(&mut self, samples: &[f32]) {
        let fill = self.producer.occupied_len();
        let ratio = rate_correction(fill, self.target_fill);
        self.last_ratio = ratio;

        self.scratch.clear();
        self.resampler.process(samples, ratio, &mut self.scratch);

        for &s in &self.scratch {
            if self.producer.try_push(s).is_err() {
                self.dropped += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_capacity_and_target_scale_with_rate() {
        // 48 kHz stereo: 120 ms capacity, 60 ms target.
        assert_eq!(ring_capacity_samples(48_000), (48_000 * 120 / 1000) * 2);
        assert_eq!(target_fill_samples(48_000), (48_000 * 60 / 1000) * 2);
        // Capacity strictly exceeds target so fill can swing both ways.
        assert!(ring_capacity_samples(48_000) > target_fill_samples(48_000));
        assert!(ring_capacity_samples(44_100) > target_fill_samples(44_100));
        // Target latency is in the sane 50-80 ms band.
        let secs = target_fill_samples(48_000) as f32 / (2.0 * 48_000.0);
        assert!((0.05..=0.08).contains(&secs), "target latency {secs}s out of band");
        // Very low rates still get the floor.
        assert!(ring_capacity_samples(1) >= MIN_CAPACITY_SAMPLES);
    }

    #[test]
    fn rate_correction_is_bounded_and_directional() {
        let target = 5_760usize;
        // At target: no correction.
        assert!((rate_correction(target, target) - 1.0).abs() < 1e-6);
        // Draining below target -> stretch (> 1.0). Overfull -> shrink (< 1.0).
        assert!(rate_correction(target / 2, target) > 1.0);
        assert!(rate_correction(target * 2, target) < 1.0);
        // Always clamped to +/- MAX_CORRECTION even at the extremes.
        for &fill in &[0usize, 1, target, target * 4, target * 100] {
            let r = rate_correction(fill, target);
            assert!(
                (1.0 - MAX_CORRECTION..=1.0 + MAX_CORRECTION).contains(&r),
                "ratio {r} unbounded"
            );
        }
        // Degenerate target.
        assert_eq!(rate_correction(0, 0), 1.0);
    }

    /// Closed-loop: feeding the correction back into a simulated buffer must drive
    /// the fill toward the target and hold there (convergence, no runaway).
    #[test]
    fn rate_correction_converges_to_target() {
        let target = 5_760f32;
        // Producer emits ~800 stereo pairs/frame; consumer drains at a slightly
        // different (drifted) rate. Rate matching should still lock the fill.
        let produced_frames = 800.0f32;
        let consumed_per_frame = 815.0f32; // device runs ~1.9% fast -> tends to drain
        let mut fill = 0.0f32;
        for _ in 0..4_000 {
            let ratio = rate_correction(fill as usize, target as usize);
            fill += produced_frames * 2.0 * ratio - consumed_per_frame * 2.0;
            fill = fill.max(0.0);
        }
        // Converged near target (within 15%), not pinned at 0 or runaway.
        let err = (fill - target).abs() / target;
        assert!(
            err < 0.15,
            "fill {fill} did not converge to target {target} (err {err})"
        );
    }

    #[test]
    fn resampler_output_length_tracks_ratio() {
        // Unity ratio: output length ~= input length.
        let input: Vec<f32> = (0..2_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut rs = Resampler::new();
        let mut out = Vec::new();
        rs.process(&input, 1.0, &mut out);
        let in_frames = input.len() / 2;
        let out_frames = out.len() / 2;
        assert!((out_frames as i64 - in_frames as i64).abs() <= 1);

        // Stretch ratio 1.02 over many blocks: total output ~= total input * 1.02.
        let mut rs = Resampler::new();
        let mut total_out = 0usize;
        let blocks = 200;
        for _ in 0..blocks {
            let mut o = Vec::new();
            rs.process(&input, 1.02, &mut o);
            total_out += o.len() / 2;
        }
        let total_in = in_frames * blocks;
        let achieved = total_out as f32 / total_in as f32;
        assert!((achieved - 1.02).abs() < 0.01, "achieved ratio {achieved} != 1.02");
    }

    #[test]
    fn resampler_preserves_signal_and_is_continuous() {
        // A DC block must stay DC (no interpolation artifacts / discontinuities),
        // including across block boundaries.
        let block = vec![0.5f32; 400];
        let mut rs = Resampler::new();
        let mut out = Vec::new();
        for _ in 0..5 {
            rs.process(&block, 0.99, &mut out);
        }
        for &s in &out {
            assert!((s - 0.5).abs() < 1e-4, "DC not preserved: {s}");
        }
        assert!(!out.is_empty());
    }
}
